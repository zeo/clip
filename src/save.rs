use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use windows::core::{Result, HSTRING};
use windows::Win32::Media::MediaFoundation::*;

use crate::audio::Aac;
use crate::encode::{describe, parameter_sets, Format};
use crate::hdr::Mastering;
use crate::ring::Packet;

/// Writes already encoded packets to an MP4 without re-encoding: the video, then one AAC track per
/// audio lane cut to the video's span and titled with its name, and for hdr10 the screen it was
/// mastered on. The file appears under its final name only once it is complete.
pub fn write_mp4(
    path: &Path,
    packets: &[Arc<Packet>],
    format: Format,
    audio: &[Vec<Arc<Packet>>],
    names: &[&str],
    mastering: Option<Mastering>,
) -> Result<()> {
    let first = packets
        .first()
        .filter(|p| p.key)
        .ok_or(windows::core::Error::new(MF_E_INVALIDREQUEST, "a clip has to start on a keyframe"))?;
    let end = packets.iter().map(|p| p.pts + p.duration).max().unwrap_or(first.pts);
    let audio: Vec<Vec<Arc<Packet>>> =
        audio.iter().map(|lane| lane.iter().filter(|p| p.pts >= first.pts && p.pts < end).cloned().collect()).collect();
    let writer = Writer::open(path, first, format, audio.len())?;
    writer.write(packets, &audio)?;
    writer.finish(names, mastering)
}

const E_RENAME: windows::core::HRESULT = windows::core::HRESULT(0x8007_0050_u32 as i32);
const E_LABEL: windows::core::HRESULT = windows::core::HRESULT(0x8007_000D_u32 as i32);

/// An MP4 written a stretch at a time under a .part name, which a writer dropped unfinished deletes.
pub struct Writer {
    sink: Option<IMFSinkWriter>,
    video: u32,
    lanes: Vec<u32>,
    base: i64,
    path: PathBuf,
    partial: PathBuf,
}

impl Writer {
    /// Opens a file for video starting on the keyframe `first`, with `lanes` audio tracks.
    pub fn open(path: &Path, first: &Packet, format: Format, lanes: usize) -> Result<Self> {
        let partial = path.with_extension("mp4.part");
        unsafe {
            let mut attributes = None;
            MFCreateAttributes(&mut attributes, 2)?;
            let attributes = attributes.expect("created");
            // the container comes from this attribute, so the .part extension is fine
            attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
            attributes.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)?;
            let sink = MFCreateSinkWriterFromURL(&HSTRING::from(partial.as_os_str()), None, &attributes)?;
            let mut writer =
                Self { sink: Some(sink), video: 0, lanes: Vec::new(), base: first.pts, path: path.into(), partial };
            let sink = writer.sink.as_ref().expect("just opened");

            let video = MFCreateMediaType()?;
            video.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            video.SetGUID(&MF_MT_SUBTYPE, &format.codec.subtype())?;
            video.SetUINT32(&MF_MT_AVG_BITRATE, format.bitrate)?;
            describe(&video, format)?;
            let headers = parameter_sets(format.codec, &first.data);
            if !headers.is_empty() {
                video.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &headers)?;
            }
            writer.video = sink.AddStream(&video)?;
            sink.SetInputMediaType(writer.video, &video, None)?;
            if lanes > 0 {
                let aac = Aac::new()?.output_type()?;
                for _ in 0..lanes {
                    let lane = sink.AddStream(&aac)?;
                    sink.SetInputMediaType(lane, &aac, None)?;
                    writer.lanes.push(lane);
                }
            }
            sink.BeginWriting()?;
            Ok(writer)
        }
    }

    /// Writes video packets in decode order with each lane's sound, interleaved by time so players
    /// never wait on one track to reach another.
    pub fn write(&self, packets: &[Arc<Packet>], audio: &[Vec<Arc<Packet>>]) -> Result<()> {
        let sink = self.sink.as_ref().expect("open until finished");
        let mut sound: Vec<(u32, &Packet)> = Vec::new();
        for (&lane, packets) in self.lanes.iter().zip(audio) {
            sound.extend(packets.iter().filter(|p| p.pts >= self.base).map(|p| (lane, &**p)));
        }
        sound.sort_by_key(|(_, p)| p.pts);
        let mut sound = sound.into_iter().peekable();
        for packet in packets {
            while let Some((lane, p)) = sound.next_if(|(_, p)| p.pts <= packet.pts) {
                unsafe { write_sample(sink, lane, p, self.base)? };
            }
            unsafe { write_sample(sink, self.video, packet, self.base)? };
        }
        sound.try_for_each(|(lane, p)| unsafe { write_sample(sink, lane, p, self.base) })
    }

    /// Closes the file, names its tracks, and moves it to its final name.
    pub fn finish(mut self, names: &[&str], mastering: Option<Mastering>) -> Result<()> {
        let sink = self.sink.take().expect("open until finished");
        let finalized = unsafe { sink.Finalize() };
        drop(sink);
        let done = finalized
            .and_then(|()| {
                label(&self.partial, names, mastering).map_err(|e| windows::core::Error::new(E_LABEL, e.to_string()))
            })
            .and_then(|()| {
                std::fs::rename(&self.partial, &self.path)
                    .map_err(|e| windows::core::Error::new(E_RENAME, e.to_string()))
            });
        if done.is_err() {
            let _ = std::fs::remove_file(&self.partial);
        }
        done
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.sink.take().is_some() {
            let _ = std::fs::remove_file(&self.partial);
        }
    }
}

unsafe fn write_sample(writer: &IMFSinkWriter, stream: u32, packet: &Packet, base: i64) -> Result<()> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(packet.data.len() as u32)?;
        let mut target = std::ptr::null_mut();
        buffer.Lock(&mut target, None, None)?;
        std::ptr::copy_nonoverlapping(packet.data.as_ptr(), target, packet.data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(packet.data.len() as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime(packet.pts - base)?;
        sample.SetSampleDuration(packet.duration)?;
        if packet.key {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        }
        writer.WriteSample(stream, &sample)
    }
}

/// Media foundation's sink writes neither hdr10 mastering boxes nor track names, so they go into the
/// moov afterwards. It sits after the media, so growing it moves no chunk offset.
fn label(path: &Path, names: &[&str], mastering: Option<Mastering>) -> std::io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    let (at, size) = moov(&mut file, len)?;
    let mut old = vec![0; size as usize];
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(&mut old)?;
    let (_, body) = boxes(&old).and_then(|b| b.into_iter().next()).ok_or_else(invalid)?;
    let new = boxed(b"moov", &label_moov(body, names, mastering).ok_or_else(invalid)?);
    if at + size == len {
        file.seek(SeekFrom::Start(at))?;
        file.write_all(&new)?;
        file.set_len(at + new.len() as u64)
    } else {
        file.seek(SeekFrom::Start(at + 4))?;
        file.write_all(b"free")?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(&new)
    }
}

fn invalid() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "the mp4 boxes don't add up")
}

/// Where the moov box starts in `file` and how long it is.
fn moov(file: &mut std::fs::File, len: u64) -> std::io::Result<(u64, u64)> {
    let (mut at, mut moov) = (0, None);
    while at + 8 <= len {
        let mut head = [0; 8];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut head)?;
        let size = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
            0 => len - at,
            1 => {
                let mut large = [0; 8];
                file.read_exact(&mut large)?;
                u64::from_be_bytes(large)
            }
            n => n as u64,
        };
        if size < 8 || at + size > len {
            return Err(invalid());
        }
        if &head[4..] == b"moov" {
            moov = Some((at, size));
        }
        at += size;
    }
    moov.ok_or_else(invalid)
}

/// What a clip's moov says about it.
pub struct Probe {
    pub seconds: f64,
    /// frames per second of the video, 0 when the file doesn't say
    pub fps: f64,
    /// the audio tracks' names in order, empty where a track has none
    pub tracks: Vec<String>,
    pub mastering: Option<Mastering>,
}

pub fn probe(path: &Path) -> std::io::Result<Probe> {
    read_moov(&moov_of(path)?).ok_or_else(invalid)
}

/// Every audio track's frame sizes in track order, which tell its stream apart in a demuxer that
/// lists the tracks in another order.
pub fn frame_sizes(path: &Path) -> std::io::Result<Vec<Vec<u32>>> {
    read_frame_sizes(&moov_of(path)?).ok_or_else(invalid)
}

fn moov_of(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let (at, size) = moov(&mut file, len)?;
    let mut data = vec![0; size as usize];
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(&mut data)?;
    Ok(data)
}

fn read_frame_sizes(data: &[u8]) -> Option<Vec<Vec<u32>>> {
    let u32_at = |data: &[u8], at: usize| Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?));
    let mut tracks = Vec::new();
    for (kind, trak) in boxes(child(data, &[b"moov"])?)? {
        if &kind != b"trak" || child(trak, &[b"mdia", b"hdlr"])?.get(8..12)? != b"soun" {
            continue;
        }
        let stsz = child(trak, &[b"mdia", b"minf", b"stbl", b"stsz"])?;
        let (size, count) = (u32_at(stsz, 4)?, u32_at(stsz, 8)? as usize);
        tracks.push(match size {
            0 => (0..count).map(|i| u32_at(stsz, 12 + 4 * i)).collect::<Option<_>>()?,
            size => vec![size; count],
        });
    }
    Some(tracks)
}

fn read_moov(data: &[u8]) -> Option<Probe> {
    let moov = child(data, &[b"moov"])?;
    let mvhd = child(moov, &[b"mvhd"])?;
    let number = |at: usize, wide: bool| -> Option<u64> {
        Some(match wide {
            true => u64::from_be_bytes(mvhd.get(at..at + 8)?.try_into().ok()?),
            false => u32::from_be_bytes(mvhd.get(at..at + 4)?.try_into().ok()?) as u64,
        })
    };
    let (scale, duration) = match mvhd.first()? {
        1 => (number(20, false)?, number(24, true)?),
        _ => (number(12, false)?, number(16, false)?),
    };
    let mut tracks = Vec::new();
    let mut mastering = None;
    let mut fps = 0.0;
    for (kind, trak) in boxes(moov)? {
        if &kind != b"trak" {
            continue;
        }
        let hdlr = child(trak, &[b"mdia", b"hdlr"])?;
        match hdlr.get(8..12)? {
            b"soun" => {
                let name = hdlr.get(24..).unwrap_or_default();
                let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
                tracks.push(String::from_utf8_lossy(name).into_owned());
            }
            b"vide" => {
                let entry = |last: &[u8; 4]| {
                    [b"hvc1", b"hev1"]
                        .into_iter()
                        .find_map(|e| child(trak, &[b"mdia", b"minf", b"stbl", b"stsd", e, last]))
                };
                if let (Some(mdcv), Some(clli)) = (entry(b"mdcv"), entry(b"clli")) {
                    mastering = read_mastering(mdcv, clli);
                }
                fps = frame_rate(trak).unwrap_or(0.0);
            }
            _ => {}
        }
    }
    Some(Probe { seconds: duration as f64 / scale.max(1) as f64, fps, tracks, mastering })
}

/// The track's timescale over its first sample's duration, which is every frame's in a clip.
fn frame_rate(trak: &[u8]) -> Option<f64> {
    let u32_at = |data: &[u8], at: usize| Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?) as f64);
    let mdhd = child(trak, &[b"mdia", b"mdhd"])?;
    let scale = u32_at(mdhd, if mdhd.first()? == &1 { 20 } else { 12 })?;
    let delta = u32_at(child(trak, &[b"mdia", b"minf", b"stbl", b"stts"])?, 12)?;
    (delta > 0.0).then(|| scale / delta)
}

/// The inverse of `mdcv` and `clli`.
fn read_mastering(mdcv: &[u8], clli: &[u8]) -> Option<Mastering> {
    let u16_at = |data: &[u8], at: usize| Some(u16::from_be_bytes(data.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_be_bytes(mdcv.get(at..at + 4)?.try_into().ok()?));
    let xy = |at: usize| Some([u16_at(mdcv, at)? as f32 * 0.00002, u16_at(mdcv, at + 2)? as f32 * 0.00002]);
    Some(Mastering {
        primaries: [xy(8)?, xy(0)?, xy(4)?, xy(12)?],
        max: u32_at(16)? as f32 / 10_000.0,
        min: u32_at(20)? as f32 / 10_000.0,
        average: u16_at(clli, 2)? as f32,
    })
}

/// The body of the box at `path`, stepping over the fields an stsd or sample entry has first.
fn child<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
    let (kind, body) = boxes(data)?.into_iter().find(|(kind, _)| kind == path[0])?;
    match path.len() {
        1 => Some(body),
        _ => child(body.get(header(&kind)..)?, &path[1..]),
    }
}

/// The first trak is the video, the rest are the audio lanes in order.
fn label_moov(body: &[u8], names: &[&str], mastering: Option<Mastering>) -> Option<Vec<u8>> {
    let mut track = 0;
    let mut out = Vec::new();
    for (kind, body) in boxes(body)? {
        let body = match (&kind, track) {
            (b"trak", 0) => match mastering {
                Some(m) => [b"hvc1", b"hev1"].into_iter().try_fold(body.to_vec(), |trak, entry| {
                    let path = [b"mdia", b"minf", b"stbl", b"stsd", entry];
                    edit(&trak, &path, &mut |e| [e, &mdcv(m), &clli(m)].concat())
                })?,
                None => body.to_vec(),
            },
            (b"trak", n) => match names.get(n - 1) {
                Some(name) => named(body, name)?,
                None => body.to_vec(),
            },
            _ => body.to_vec(),
        };
        track += usize::from(&kind == b"trak");
        out.extend(boxed(&kind, &body));
    }
    Some(out)
}

/// The handler name is what ffmpeg and the players built on it list as the track's name.
fn named(trak: &[u8], name: &str) -> Option<Vec<u8>> {
    edit(trak, &[b"mdia", b"hdlr"], &mut |hdlr| {
        // version and flags, pre_defined, handler type and three reserved words come before the name
        if hdlr.len() < 24 {
            hdlr.to_vec()
        } else {
            [&hdlr[..24], name.as_bytes(), &[0]].concat()
        }
    })
}

/// Rewrites the body at the end of `path` through `f`. An stsd keeps its version and entry count,
/// and a visual sample entry its 78 bytes of fields, ahead of the boxes inside.
fn edit(data: &[u8], path: &[&[u8; 4]], f: &mut dyn FnMut(&[u8]) -> Vec<u8>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for (kind, body) in boxes(data)? {
        let body = match path {
            [only] if **only == kind => f(body),
            [first, rest @ ..] if **first == kind => {
                let skip = header(&kind);
                [body.get(..skip)?, &edit(&body[skip..], rest, f)?].concat()
            }
            _ => body.to_vec(),
        };
        out.extend(boxed(&kind, &body));
    }
    Some(out)
}

fn header(kind: &[u8; 4]) -> usize {
    match kind {
        b"stsd" => 8,
        b"hvc1" | b"hev1" | b"avc1" => 78,
        _ => 0,
    }
}

fn boxes(mut data: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    let mut found = Vec::new();
    while !data.is_empty() {
        let size = u32::from_be_bytes(data.get(..4)?.try_into().ok()?) as usize;
        let kind: [u8; 4] = data.get(4..8)?.try_into().ok()?;
        let (start, size) = match size {
            0 => (8, data.len()),
            1 => (16, u64::from_be_bytes(data.get(8..16)?.try_into().ok()?) as usize),
            n => (8, n),
        };
        found.push((kind, data.get(start..size)?));
        data = &data[size..];
    }
    Some(found)
}

fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    [&(body.len() as u32 + 8).to_be_bytes(), kind, body].concat()
}

/// Mastering display colour volume: primaries in green, blue, red order as in the hevc sei, in
/// 0.00002 steps of xy, then max and min luminance in 0.0001 nits.
fn mdcv(m: Mastering) -> Vec<u8> {
    let xy = |p: [f32; 2]| p.map(|v| ((v / 0.00002).round() as u16).to_be_bytes()).concat();
    let nits = |v: f32| ((v * 10_000.0).round() as u32).to_be_bytes().to_vec();
    let [red, green, blue, white] = m.primaries;
    boxed(b"mdcv", &[xy(green), xy(blue), xy(red), xy(white), nits(m.max), nits(m.min)].concat())
}

/// Content light level: MaxCLL then MaxFALL in whole nits.
fn clli(m: Mastering) -> Vec<u8> {
    boxed(b"clli", &[(m.max.round() as u16).to_be_bytes(), (m.average.round() as u16).to_be_bytes()].concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    const PANEL: Mastering = Mastering {
        primaries: [[0.68, 0.32], [0.265, 0.69], [0.15, 0.06], [0.3127, 0.329]],
        min: 0.05,
        max: 1000.0,
        average: 400.0,
    };

    #[test]
    fn labels_land_in_the_right_boxes() {
        let hdlr = |kind: &[u8; 4]| boxed(b"hdlr", &[&[0; 8][..], kind, &[0; 12], b"Handler\0"].concat());
        let entry = boxed(b"hvc1", &[&[0; 78][..], &boxed(b"hvcC", &[1, 2, 3])].concat());
        let stsd = boxed(b"stsd", &[&[0, 0, 0, 0, 0, 0, 0, 1][..], &entry].concat());
        let video = boxed(b"mdia", &[hdlr(b"vide"), boxed(b"minf", &boxed(b"stbl", &stsd))].concat());
        let sound = boxed(b"mdia", &hdlr(b"soun"));
        let moov = [boxed(b"mvhd", &[0; 4]), boxed(b"trak", &video), boxed(b"trak", &sound)].concat();

        let out = label_moov(&moov, &["game"], Some(PANEL)).unwrap();
        let traks: Vec<_> = boxes(&out).unwrap().into_iter().filter(|(kind, _)| kind == b"trak").collect();
        let entry =
            |last: &'static [u8; 4]| child(traks[0].1, &[b"mdia", b"minf", b"stbl", b"stsd", b"hvc1", last]).unwrap();
        assert_eq!(entry(b"hvcC"), [1, 2, 3]);
        let mdcv = entry(b"mdcv");
        // green x comes first, max luminance follows the four points
        assert_eq!(u16::from_be_bytes([mdcv[0], mdcv[1]]), 13250);
        assert_eq!(u32::from_be_bytes([mdcv[16], mdcv[17], mdcv[18], mdcv[19]]), 10_000_000);
        assert_eq!(entry(b"clli"), [0x03, 0xE8, 0x01, 0x90]);
        assert_eq!(&child(traks[1].1, &[b"mdia", b"hdlr"]).unwrap()[24..], b"game\0");
        assert_eq!(&child(traks[0].1, &[b"mdia", b"hdlr"]).unwrap()[24..], b"Handler\0");
    }

    #[test]
    fn probe_reads_back_what_labels_wrote() {
        let be = |v: u32| v.to_be_bytes();
        let hdlr = |kind: &[u8; 4]| boxed(b"hdlr", &[&[0; 8][..], kind, &[0; 12], b"Handler\0"].concat());
        let entry = boxed(b"hvc1", &[0; 78]);
        let stsd = boxed(b"stsd", &[&[0, 0, 0, 0, 0, 0, 0, 1][..], &entry].concat());
        let stts = boxed(b"stts", &[&[0; 4][..], &be(1), &be(300), &be(1500)].concat());
        let mdhd = boxed(b"mdhd", &[&[0; 12][..], &be(90_000), &be(450_000)].concat());
        let minf = boxed(b"minf", &boxed(b"stbl", &[stsd, stts].concat()));
        let video = boxed(b"mdia", &[mdhd, hdlr(b"vide"), minf].concat());
        let sound = || boxed(b"trak", &boxed(b"mdia", &hdlr(b"soun")));
        let mvhd = boxed(b"mvhd", &[&[0; 12][..], &be(1000), &be(5500)].concat());
        let moov = [mvhd, boxed(b"trak", &video), sound(), sound()].concat();

        let labelled = boxed(b"moov", &label_moov(&moov, &["game", "voice"], Some(PANEL)).unwrap());
        let probe = read_moov(&labelled).unwrap();
        assert_eq!(probe.seconds, 5.5);
        assert_eq!(probe.fps, 60.0);
        assert_eq!(probe.tracks, ["game", "voice"]);
        let mastering = probe.mastering.unwrap();
        assert!((mastering.max - PANEL.max).abs() < 0.01 && (mastering.average - PANEL.average).abs() < 0.5);
        assert!((mastering.primaries[1][0] - PANEL.primaries[1][0]).abs() < 0.0001);
    }
}

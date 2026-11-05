use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
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
    let partial = path.with_extension("mp4.part");
    let written = unsafe { write(&partial, packets, first, format, audio) }.and_then(|()| {
        label(&partial, names, mastering).map_err(|e| windows::core::Error::new(E_LABEL, e.to_string()))
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    std::fs::rename(&partial, path).map_err(|e| windows::core::Error::new(E_RENAME, e.to_string()))
}

const E_RENAME: windows::core::HRESULT = windows::core::HRESULT(0x8007_0050_u32 as i32);
const E_LABEL: windows::core::HRESULT = windows::core::HRESULT(0x8007_000D_u32 as i32);

unsafe fn write(
    path: &Path,
    packets: &[Arc<Packet>],
    first: &Packet,
    format: Format,
    audio: &[Vec<Arc<Packet>>],
) -> Result<()> {
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 2)?;
        let attributes = attributes.expect("created");
        // the container comes from this attribute, so the .part extension is fine
        attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
        attributes.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)?;
        let writer = MFCreateSinkWriterFromURL(&HSTRING::from(path.as_os_str()), None, &attributes)?;

        let video = MFCreateMediaType()?;
        video.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        video.SetGUID(&MF_MT_SUBTYPE, &format.codec.subtype())?;
        video.SetUINT32(&MF_MT_AVG_BITRATE, format.bitrate)?;
        describe(&video, format)?;
        let headers = parameter_sets(format.codec, &first.data);
        if !headers.is_empty() {
            video.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &headers)?;
        }
        let stream = writer.AddStream(&video)?;
        writer.SetInputMediaType(stream, &video, None)?;
        let mut lanes = Vec::new();
        if !audio.is_empty() {
            let aac = Aac::new()?.output_type()?;
            for _ in audio {
                let lane = writer.AddStream(&aac)?;
                writer.SetInputMediaType(lane, &aac, None)?;
                lanes.push(lane);
            }
        }
        writer.BeginWriting()?;

        let base = first.pts;
        let end = packets.iter().map(|p| p.pts + p.duration).max().unwrap_or(base);
        let mut sound: Vec<(u32, &Packet)> = Vec::new();
        for (&lane, packets) in lanes.iter().zip(audio) {
            sound.extend(packets.iter().filter(|p| p.pts >= base && p.pts < end).map(|p| (lane, &**p)));
        }
        sound.sort_by_key(|(_, p)| p.pts);
        // interleaved by time so players never wait on one track to reach another, video kept in decode order
        let mut sound = sound.into_iter().peekable();
        for packet in packets {
            while let Some((lane, p)) = sound.next_if(|(_, p)| p.pts <= packet.pts) {
                write_sample(&writer, lane, p, base)?;
            }
            write_sample(&writer, stream, packet, base)?;
        }
        for (lane, p) in sound {
            write_sample(&writer, lane, p, base)?;
        }
        writer.Finalize()
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
    let invalid = || std::io::Error::new(std::io::ErrorKind::InvalidData, "the mp4 boxes don't add up");
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
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
    let (at, size) = moov.ok_or_else(invalid)?;
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

    fn child<'a>(data: &'a [u8], path: &[&[u8; 4]]) -> &'a [u8] {
        let (kind, body) = boxes(data).unwrap().into_iter().find(|(kind, _)| kind == path[0]).unwrap();
        if path.len() == 1 {
            body
        } else {
            child(&body[header(&kind)..], &path[1..])
        }
    }

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
        let entry = |last: &'static [u8; 4]| child(traks[0].1, &[b"mdia", b"minf", b"stbl", b"stsd", b"hvc1", last]);
        assert_eq!(entry(b"hvcC"), [1, 2, 3]);
        let mdcv = entry(b"mdcv");
        // green x comes first, max luminance follows the four points
        assert_eq!(u16::from_be_bytes([mdcv[0], mdcv[1]]), 13250);
        assert_eq!(u32::from_be_bytes([mdcv[16], mdcv[17], mdcv[18], mdcv[19]]), 10_000_000);
        assert_eq!(entry(b"clli"), [0x03, 0xE8, 0x01, 0x90]);
        assert_eq!(&child(traks[1].1, &[b"mdia", b"hdlr"])[24..], b"game\0");
        assert_eq!(&child(traks[0].1, &[b"mdia", b"hdlr"])[24..], b"Handler ");
    }
}

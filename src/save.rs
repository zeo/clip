use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use windows::core::{Error, Result, GUID, HSTRING};
use windows::Win32::Media::MediaFoundation::*;

use crate::audio::Aac;
use crate::encode::{describe, is_hevc_parameter_set, nal_units, parameter_sets, Codec, Format};
use crate::hdr::Mastering;
use crate::ring::{load, Held, Packet};

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
    let held = |packets: &[Arc<Packet>]| packets.iter().cloned().map(Held::from).collect::<Vec<_>>();
    let audio: Vec<_> = audio.iter().map(|lane| held(lane)).collect();
    write_held(path, &held(packets), format, &audio, names, mastering)
}

/// [`write_mp4`] for packets a ring may hold on disk, read back a stretch at a time so a buffer of
/// hours never has to fit in memory.
pub fn write_held(
    path: &Path,
    packets: &[Held],
    format: Format,
    audio: &[Vec<Held>],
    names: &[&str],
    mastering: Option<Mastering>,
) -> Result<()> {
    let io = |e: std::io::Error| windows::core::Error::new(E_READ, e.to_string());
    let read = |held: &[Held]| load(held).map_err(io);
    let first = packets
        .first()
        .filter(|p| p.key)
        .ok_or(windows::core::Error::new(MF_E_INVALIDREQUEST, "a clip has to start on a keyframe"))?;
    let end = packets.iter().map(|p| p.pts + p.duration).max().unwrap_or(first.pts);
    let mut lanes: Vec<&[Held]> = audio
        .iter()
        .map(|lane| &lane[lane.partition_point(|p| p.pts < first.pts)..lane.partition_point(|p| p.pts < end)])
        .collect();
    let mut writer = Writer::open(path, &*first.load().map_err(io)?, format, lanes.len(), false)?;
    let mut stretches = packets.chunks(STRETCH).peekable();
    while let Some(stretch) = stretches.next() {
        // sound only up to the stretch's newest frame, the rest goes with the next one
        let upto = match stretches.peek() {
            Some(_) => stretch.iter().map(|p| p.pts).max().unwrap_or(i64::MAX),
            None => i64::MAX,
        };
        let sound = lanes
            .iter_mut()
            .map(|lane| {
                let (now, rest) = lane.split_at(lane.partition_point(|p| p.pts <= upto));
                *lane = rest;
                read(now)
            })
            .collect::<Result<Vec<_>>>()?;
        writer.write(&read(stretch)?, &sound)?;
    }
    writer.finish(names, mastering)
}

/// video packets read back at a time, about 10 s at 60 fps
const STRETCH: usize = 600;

const E_RENAME: windows::core::HRESULT = windows::core::HRESULT(0x8007_0050_u32 as i32);
const E_LABEL: windows::core::HRESULT = windows::core::HRESULT(0x8007_000D_u32 as i32);
const E_READ: windows::core::HRESULT = windows::core::HRESULT(0x8007_001E_u32 as i32);
const E_WRITE: windows::core::HRESULT = windows::core::HRESULT(0x8007_001D_u32 as i32);

/// An MP4 written a stretch at a time under a .part name, which a writer dropped unfinished deletes.
pub struct Writer {
    sink: Option<Sink>,
    fragments: bool,
    base: i64,
    path: PathBuf,
    partial: PathBuf,
}

enum Sink {
    Media { sink: IMFSinkWriter, video: u32, lanes: Vec<u32> },
    Own(Fragments),
}

impl Writer {
    /// Opens a file for video starting on the keyframe `first`, with `lanes` audio tracks. With
    /// `fragments` it goes down in fragments that a crash leaves playable, see [`recover`]. Media
    /// foundation's fragmented sink refuses hevc, so clip writes those fragments itself.
    pub fn open(path: &Path, first: &Packet, format: Format, lanes: usize, fragments: bool) -> Result<Self> {
        let named = |end: &str| {
            let mut name = path.as_os_str().to_owned();
            name.push(end);
            PathBuf::from(name)
        };
        let partial = named(crate::library::PART);
        let container = match fragments {
            true => MFTranscodeContainerType_FMPEG4,
            false => MFTranscodeContainerType_MPEG4,
        };
        let sink = match (fragments, format.codec) {
            (true, Codec::Hdr10) => {
                let head = named(&format!(".head{}", crate::library::PART));
                Sink::Own(Fragments::open(&partial, &head, first, format, lanes)?)
            }
            _ => {
                let (sink, video, lanes) = media(&partial, first, format, lanes, container)?;
                Sink::Media { sink, video, lanes }
            }
        };
        Ok(Self { sink: Some(sink), fragments, base: first.pts, path: path.into(), partial })
    }

    /// Writes video packets in decode order with each lane's sound, interleaved by time so players
    /// never wait on one track to reach another.
    pub fn write(&mut self, packets: &[Arc<Packet>], audio: &[Vec<Arc<Packet>>]) -> Result<()> {
        let base = self.base;
        let (sink, video, lanes) = match self.sink.as_mut().expect("open until finished") {
            Sink::Media { sink, video, lanes } => (sink, *video, lanes),
            Sink::Own(own) => return own.write(packets, audio, base).map_err(|e| Error::new(E_WRITE, e.to_string())),
        };
        let mut sound: Vec<(u32, &Packet)> = Vec::new();
        for (&lane, packets) in lanes.iter().zip(audio) {
            sound.extend(packets.iter().filter(|p| p.pts >= base).map(|p| (lane, &**p)));
        }
        sound.sort_by_key(|(_, p)| p.pts);
        let mut sound = sound.into_iter().peekable();
        for packet in packets {
            while let Some((lane, p)) = sound.next_if(|(_, p)| p.pts <= packet.pts) {
                unsafe { write_sample(sink, lane, p, base)? };
            }
            unsafe { write_sample(sink, video, packet, base)? };
        }
        sound.try_for_each(|(lane, p)| unsafe { write_sample(sink, lane, p, base) })
    }

    /// Leaves the track names and the hdr10 screen beside a fragmented file, where [`recover`]
    /// finds them after a crash.
    pub fn keep(&self, names: &[String], mastering: Option<Mastering>) {
        if self.fragments {
            // drives without alternate streams, like exfat ones, recover recordings without them
            let _ = std::fs::write(beside(&self.partial, NAMES), names.join("\n"));
            if let Some(m) = mastering {
                let _ = std::fs::write(beside(&self.partial, SCREEN), [mdcv(m), clli(m)].concat());
            }
        }
    }

    /// Closes the file, names its tracks, and moves it to its final name.
    pub fn finish(mut self, names: &[&str], mastering: Option<Mastering>) -> Result<()> {
        let closed = match self.sink.take().expect("open until finished") {
            Sink::Media { sink, .. } => unsafe { sink.Finalize() },
            Sink::Own(mut own) => own.flush().map_err(|e| Error::new(E_WRITE, e.to_string())),
        };
        let done = closed
            .and_then(|()| match self.fragments {
                true => defragment(&self.partial).map_err(|e| Error::new(E_LABEL, e.to_string())),
                false => Ok(()),
            })
            .and_then(|()| label(&self.partial, names, mastering).map_err(|e| Error::new(E_LABEL, e.to_string())))
            .and_then(|()| {
                for kept in [NAMES, SCREEN] {
                    let _ = std::fs::remove_file(beside(&self.partial, kept));
                }
                std::fs::rename(&self.partial, &self.path).map_err(|e| Error::new(E_RENAME, e.to_string()))
            });
        if done.is_err() {
            let _ = std::fs::remove_file(&self.partial);
        }
        done
    }
}

/// Media foundation's sink writer on `partial`, with the video and `lanes` aac streams set up.
fn media(
    partial: &Path,
    first: &Packet,
    format: Format,
    lanes: usize,
    container: GUID,
) -> Result<(IMFSinkWriter, u32, Vec<u32>)> {
    unsafe {
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 2)?;
        let attributes = attributes.expect("created");
        // the container comes from this attribute, so the part extension is fine
        attributes.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &container)?;
        attributes.SetUINT32(&MF_READWRITE_DISABLE_CONVERTERS, 1)?;
        let sink = MFCreateSinkWriterFromURL(&HSTRING::from(partial.as_os_str()), None, &attributes)?;
        let video = MFCreateMediaType()?;
        video.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        video.SetGUID(&MF_MT_SUBTYPE, &format.codec.subtype())?;
        video.SetUINT32(&MF_MT_AVG_BITRATE, format.bitrate)?;
        describe(&video, format)?;
        let headers = parameter_sets(format.codec, &first.data);
        if !headers.is_empty() {
            video.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &headers)?;
        }
        let stream = sink.AddStream(&video)?;
        sink.SetInputMediaType(stream, &video, None)?;
        let mut streams = Vec::new();
        if lanes > 0 {
            let aac = Aac::new()?.output_type()?;
            for _ in 0..lanes {
                let lane = sink.AddStream(&aac)?;
                sink.SetInputMediaType(lane, &aac, None)?;
                streams.push(lane);
            }
        }
        sink.BeginWriting()?;
        Ok((sink, stream, streams))
    }
}

/// A fragmented mp4 clip writes itself: a moov with no samples, then a moof and mdat per write
/// holding every track's new samples, laid out the way [`plain`] reads them back.
struct Fragments {
    file: std::fs::File,
    sequence: u32,
    /// the video, then the lanes
    tracks: Vec<Track>,
}

struct Track {
    id: u32,
    /// media time units per second, from the track's mdhd
    scale: u64,
    /// media time of the next sample
    decoded: u64,
    /// the newest sample, written once the next one tells its duration
    held: Option<Pending>,
}

struct Pending {
    pts: i64,
    duration: i64,
    key: bool,
    data: Vec<u8>,
}

impl Fragments {
    /// Media foundation writes a plain mp4 of just `first` to `head`, and its moov, emptied of
    /// samples, starts the fragmented file. That way its sample descriptions, hvcC and esds
    /// included, are the ones every clip it saves whole carries.
    fn open(partial: &Path, head: &Path, first: &Packet, format: Format, lanes: usize) -> Result<Self> {
        let skeleton = (|| {
            let (sink, video, _) = media(head, first, format, lanes, MFTranscodeContainerType_MPEG4)?;
            unsafe {
                write_sample(&sink, video, first, first.pts)?;
                sink.Finalize()?;
            }
            drop(sink);
            std::fs::read(head).map_err(|e| Error::new(E_READ, e.to_string()))
        })();
        let _ = std::fs::remove_file(head);
        let skeleton = skeleton?;
        let bad = || Error::new(E_LABEL, "media foundation's mp4 has no moov to start from");
        let top = boxes(&skeleton).ok_or_else(bad)?;
        let ftyp = top.iter().find(|(kind, _)| kind == b"ftyp").ok_or_else(bad)?.1;
        let moov = top.iter().find(|(kind, _)| kind == b"moov").ok_or_else(bad)?.1;
        let (moov, tracks) = emptied(moov).ok_or_else(bad)?;
        if tracks.len() != lanes + 1 {
            return Err(bad());
        }
        let start = [boxed(b"ftyp", ftyp), boxed(b"moov", &moov)].concat();
        let file = std::fs::File::create(partial).and_then(|mut file| file.write_all(&start).map(|()| file));
        let file = file.map_err(|e| {
            let _ = std::fs::remove_file(partial);
            Error::new(E_WRITE, e.to_string())
        })?;
        Ok(Self { file, sequence: 0, tracks })
    }

    fn write(&mut self, packets: &[Arc<Packet>], audio: &[Vec<Arc<Packet>>], base: i64) -> std::io::Result<()> {
        let fresh = std::iter::once(packets).chain(audio.iter().map(Vec::as_slice));
        let mut runs = Vec::new();
        for (n, (track, packets)) in self.tracks.iter_mut().zip(fresh).enumerate() {
            let mut run = Vec::new();
            for p in packets.iter().filter(|p| p.pts >= base) {
                let data = match n {
                    0 => length_prefixed(&p.data),
                    _ => p.data.clone(),
                };
                let next = Pending { pts: p.pts - base, duration: p.duration, key: p.key, data };
                if let Some(done) = track.held.replace(next) {
                    let duration = track.at(p.pts - base).saturating_sub(track.at(done.pts));
                    run.push((done, duration as u32));
                }
            }
            runs.push(run);
        }
        self.fragment(runs)
    }

    /// Writes the samples still held, each lasting as long as its packet said.
    fn flush(&mut self) -> std::io::Result<()> {
        let runs: Vec<Vec<_>> = (self.tracks.iter_mut())
            .map(|track| {
                let done = track.held.take()?;
                let duration = track.at(done.pts + done.duration) - track.at(done.pts);
                Some((done, duration as u32))
            })
            .map(|done| done.into_iter().collect())
            .collect();
        self.fragment(runs)?;
        self.file.sync_data()
    }

    fn fragment(&mut self, runs: Vec<Vec<(Pending, u32)>>) -> std::io::Result<()> {
        if runs.iter().all(Vec::is_empty) {
            return Ok(());
        }
        self.sequence += 1;
        let full = |kind: &[u8; 4], head: [u8; 4], body: &[u8]| boxed(kind, &[&head[..], body].concat());
        let moof = |data: u64| {
            let mut at = data;
            let mut trafs = Vec::new();
            for (track, run) in self.tracks.iter().zip(&runs).filter(|(_, run)| !run.is_empty()) {
                // an absolute base offset and every sample's duration, size and flags
                let tfhd = full(b"tfhd", [0, 0, 0, 1], &[&track.id.to_be_bytes()[..], &at.to_be_bytes()].concat());
                let tfdt = full(b"tfdt", [1, 0, 0, 0], &track.decoded.to_be_bytes());
                let mut trun = (run.len() as u32).to_be_bytes().to_vec();
                for (sample, duration) in run {
                    // a keyframe depends on nothing, any other frame is no sync sample
                    let flags: u32 = if sample.key { 0x0200_0000 } else { 0x0101_0000 };
                    trun.extend([*duration, sample.data.len() as u32, flags].iter().flat_map(|v| v.to_be_bytes()));
                    at += sample.data.len() as u64;
                }
                trafs.push(boxed(b"traf", &[tfhd, tfdt, full(b"trun", [0, 0, 7, 0], &trun)].concat()));
            }
            boxed(b"moof", &[full(b"mfhd", [0; 4], &self.sequence.to_be_bytes()), trafs.concat()].concat())
        };
        let start = self.file.seek(SeekFrom::End(0))?;
        let size = moof(0).len() as u64;
        let media: Vec<u8> = runs.iter().flatten().flat_map(|(sample, _)| sample.data.iter().copied()).collect();
        // one write, so a crash leaves at most one cut off fragment, which a rebuild leaves out
        self.file.write_all(&[moof(start + size + 8), boxed(b"mdat", &media)].concat())?;
        for (track, run) in self.tracks.iter_mut().zip(&runs) {
            track.decoded += run.iter().map(|&(_, duration)| duration as u64).sum::<u64>();
        }
        Ok(())
    }
}

impl Track {
    /// Media time at `pts` in 100 ns units since the first frame. Every sample's time is rounded on
    /// its own, so the durations add up to the recording's length and no frame drifts off its pts.
    fn at(&self, pts: i64) -> u64 {
        ((pts.max(0) as u128 * self.scale as u128 + 5_000_000) / 10_000_000) as u64
    }
}

/// An Annex B access unit as an mp4 sample: each NAL unit after its length. The parameter sets stay
/// in the sample entry's hvcC alone, as media foundation's writer leaves them.
fn length_prefixed(access_unit: &[u8]) -> Vec<u8> {
    let units = nal_units(access_unit).into_iter().filter(|unit| !is_hevc_parameter_set(unit));
    units.flat_map(|unit| [&(unit.len() as u32).to_be_bytes()[..], unit].concat()).collect()
}

/// The moov body of a plain mp4 with its samples taken out and an mvex added, ready for fragments,
/// with its tracks.
fn emptied(moov: &[u8]) -> Option<(Vec<u8>, Vec<Track>)> {
    let u32_at = |data: &[u8], at: usize| Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?));
    let (mut out, mut tracks) = (Vec::new(), Vec::new());
    for (kind, body) in boxes(moov)? {
        if &kind != b"trak" {
            out.extend(boxed(&kind, body));
            continue;
        }
        let tkhd = child(body, &[b"tkhd"])?;
        let mdhd = child(body, &[b"mdia", b"mdhd"])?;
        let long = |header: &[u8]| header.first() == Some(&1);
        tracks.push(Track {
            id: u32_at(tkhd, if long(tkhd) { 20 } else { 12 })?,
            scale: u32_at(mdhd, if long(mdhd) { 20 } else { 12 })?.max(1) as u64,
            decoded: 0,
            held: None,
        });
        let stsd = child(body, &[b"mdia", b"minf", b"stbl", b"stsd"])?.to_vec();
        // the first sample's edit no longer applies, the fragments hold every sample from time 0
        let trak: Vec<u8> = boxes(body)?
            .into_iter()
            .filter(|(kind, _)| kind != b"edts")
            .flat_map(|(kind, body)| boxed(&kind, body))
            .collect();
        let empty = |kind: &[u8; 4], words: usize| boxed(kind, &vec![0; 4 * words]);
        let stbl = [boxed(b"stsd", &stsd), empty(b"stts", 2), empty(b"stsc", 2), empty(b"stsz", 3), empty(b"stco", 2)];
        out.extend(boxed(b"trak", &edit(&trak, &[b"mdia", b"minf", b"stbl"], &mut |_| stbl.concat())?));
    }
    // trex: version and flags, track id, sample description 1, then no default duration, size or flags
    let trex = |track: &Track| {
        boxed(b"trex", &[[0; 4], track.id.to_be_bytes(), 1u32.to_be_bytes(), [0; 4], [0; 4], [0; 4]].concat())
    };
    out.extend(boxed(b"mvex", &tracks.iter().flat_map(trex).collect::<Vec<u8>>()));
    Some((out, tracks))
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
        let (kind, size) = top(file, at, len)?.ok_or_else(invalid)?;
        if &kind == b"moov" {
            moov = Some((at, size));
        }
        at += size;
    }
    moov.ok_or_else(invalid)
}

/// The kind and size of the top level box at `at`, none when it runs past `len`.
fn top(file: &mut std::fs::File, at: u64, len: u64) -> std::io::Result<Option<([u8; 4], u64)>> {
    if at + 8 > len {
        return Ok(None);
    }
    let mut head = [0; 16];
    file.seek(SeekFrom::Start(at))?;
    file.read_exact(&mut head[..8])?;
    let size = match u32::from_be_bytes([head[0], head[1], head[2], head[3]]) {
        0 => len - at,
        1 if at + 16 <= len => {
            file.read_exact(&mut head[8..])?;
            u64::from_be_bytes(head[8..].try_into().expect("eight bytes"))
        }
        1 => return Ok(None),
        n => n as u64,
    };
    let kind = [head[4], head[5], head[6], head[7]];
    Ok((size >= 8 && at + size <= len).then_some((kind, size)))
}

/// Rebuilds a fragmented recording a crash cut off, see [`defragment`], and gives its tracks the
/// names and hdr10 screen [`Writer::keep`] left beside it.
pub fn recover(path: &Path) -> std::io::Result<()> {
    defragment(path)?;
    let screen = std::fs::read(beside(path, SCREEN)).ok();
    let mastering = screen.as_deref().and_then(|m| read_mastering(child(m, &[b"mdcv"])?, child(m, &[b"clli"])?));
    if let Ok(names) = std::fs::read_to_string(beside(path, NAMES)) {
        let names: Vec<&str> = names.split('\n').collect();
        // a recording that plays without names beats none
        let _ = label(path, &names, mastering);
    }
    for kept in [NAMES, SCREEN] {
        let _ = std::fs::remove_file(beside(path, kept));
    }
    Ok(())
}

/// the track names, one a line
const NAMES: &str = "names";
/// an hdr10 recording's mdcv and clli boxes
const SCREEN: &str = "mastering";

/// An alternate data stream, which moves and goes away with its file.
fn beside(path: &Path, kept: &str) -> PathBuf {
    let mut stream = path.as_os_str().to_owned();
    stream.push(format!(":clip-{kept}"));
    stream.into()
}

/// Turns a fragmented mp4 into a plain one where it lies: the fragments' indexes become one moov
/// at the end, where mfra was, and the first moov and every moof become free boxes, so no sample
/// moves. A file cut off by a crash keeps every fragment that was written whole. A whole plain mp4
/// is left as it is.
pub fn defragment(path: &Path) -> std::io::Result<()> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let len = file.metadata()?.len();
    let read = |file: &mut std::fs::File, at: u64, size: u64| -> std::io::Result<Vec<u8>> {
        let mut data = vec![0; size as usize];
        file.seek(SeekFrom::Start(at))?;
        file.read_exact(&mut data)?;
        Ok(data)
    };
    let (mut at, mut end, mut first, mut moof, mut moofs) = (0, 0, None, None, Vec::new());
    let mut rebuilt = false;
    while let Some((kind, size)) = top(&mut file, at, len)? {
        match &kind {
            b"moov" if first.is_none() => {
                first = Some((at, read(&mut file, at, size)?));
                end = at + size;
            }
            // an earlier pass got as far as appending its moov, only some indexes are left to free
            b"moov" => {
                rebuilt = true;
                break;
            }
            b"moof" => moof = Some((at, read(&mut file, at, size)?)),
            b"mdat" => {
                if let Some(whole) = moof.take() {
                    moofs.push(whole);
                    end = at + size;
                }
            }
            // what follows the fragments, mfra or a moov an earlier pass appended, is rebuilt
            _ if moof.is_some() || !moofs.is_empty() => break,
            _ => {}
        }
        at += size;
    }
    let (moov_at, moov) = first.ok_or_else(invalid)?;
    let fragmented = child(&moov, &[b"moov", b"mvex"]).is_some();
    if !fragmented && moofs.is_empty() && at == len {
        return Ok(());
    }
    if !fragmented || (moofs.is_empty() && !rebuilt) {
        return Err(invalid());
    }
    if !rebuilt {
        let plain = plain(&moov, &moofs).ok_or_else(invalid)?;
        file.set_len(end)?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(&plain)?;
        file.sync_data()?;
    }
    // the old indexes go last, so a crash before here leaves a file this can still rebuild
    for at in moofs.iter().map(|(at, _)| *at).chain([moov_at]) {
        file.seek(SeekFrom::Start(at + 4))?;
        file.write_all(b"free")?;
    }
    Ok(())
}

struct Sample {
    offset: u64,
    size: u32,
    duration: u32,
    composition: u32,
    sync: bool,
}

/// The moov a plain mp4 of `moofs`, each with where it starts, needs in place of the fragmented
/// `moov`.
fn plain(moov: &[u8], moofs: &[(u64, Vec<u8>)]) -> Option<Vec<u8>> {
    let u32_at = |data: &[u8], at: usize| Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?));
    let moov = child(moov, &[b"moov"])?;
    // trex: track id, then the default description index, duration, size and flags
    let defaults: HashMap<u32, [u32; 3]> = boxes(child(moov, &[b"mvex"])?)?
        .into_iter()
        .filter(|(kind, _)| kind == b"trex")
        .map(|(_, trex)| Some((u32_at(trex, 4)?, [u32_at(trex, 12)?, u32_at(trex, 16)?, u32_at(trex, 20)?])))
        .collect::<Option<_>>()?;
    let mut tracks: HashMap<u32, Vec<Sample>> = HashMap::new();
    for (start, moof) in moofs {
        let mut next = *start;
        for (kind, traf) in boxes(child(moof, &[b"moof"])?)? {
            if &kind != b"traf" {
                continue;
            }
            let tfhd = child(traf, &[b"tfhd"])?;
            let flags = u32_at(tfhd, 0)? & 0xff_ffff;
            let id = u32_at(tfhd, 4)?;
            let [mut duration, mut size, mut sample_flags] = *defaults.get(&id)?;
            let mut at = 8;
            let base = match flags {
                f if f & 0x1 != 0 => {
                    at += 8;
                    u64::from_be_bytes(tfhd.get(8..16)?.try_into().ok()?)
                }
                f if f & 0x2_0000 != 0 => *start,
                _ => next,
            };
            at += if flags & 0x2 != 0 { 4 } else { 0 };
            for (bit, field) in [(0x8, &mut duration), (0x10, &mut size), (0x20, &mut sample_flags)] {
                if flags & bit != 0 {
                    *field = u32_at(tfhd, at)?;
                    at += 4;
                }
            }
            let samples = tracks.entry(id).or_default();
            let mut cursor = base;
            for (kind, trun) in boxes(traf)? {
                if &kind != b"trun" {
                    continue;
                }
                let flags = u32_at(trun, 0)? & 0xff_ffff;
                let mut at = 8;
                let mut field = |bit: u32| -> Option<Option<u32>> {
                    if flags & bit == 0 {
                        return Some(None);
                    }
                    at += 4;
                    u32_at(trun, at - 4).map(Some)
                };
                if let Some(offset) = field(0x1)? {
                    cursor = base.checked_add_signed(offset as i32 as i64)?;
                }
                let first_flags = field(0x4)?;
                for i in 0..u32_at(trun, 4)? {
                    let duration = field(0x100)?.unwrap_or(duration);
                    let size = field(0x200)?.unwrap_or(size);
                    let flags = field(0x400)?.or(first_flags.filter(|_| i == 0)).unwrap_or(sample_flags);
                    let composition = field(0x800)?.unwrap_or(0);
                    // sample_is_non_sync_sample
                    let sync = flags & 0x1_0000 == 0;
                    samples.push(Sample { offset: cursor, size, duration, composition, sync });
                    cursor += size as u64;
                }
            }
            next = cursor;
        }
    }
    let scale_of = |header: &[u8]| Some(u32_at(header, if header.first()? == &1 { 20 } else { 12 })?.max(1) as u128);
    let mvhd = child(moov, &[b"mvhd"])?;
    let scale = scale_of(mvhd)?;
    let mut longest = 0;
    let mut traks = Vec::new();
    for (kind, trak) in boxes(moov)? {
        if &kind != b"trak" {
            continue;
        }
        let tkhd = child(trak, &[b"tkhd"])?;
        // a lane that never got a packet before a crash plays as a silent track
        let id = u32_at(tkhd, if tkhd.first()? == &1 { 20 } else { 12 })?;
        let samples = tracks.get(&id).map_or(&[][..], Vec::as_slice);
        let mdhd = child(trak, &[b"mdia", b"mdhd"])?;
        let media: u64 = samples.iter().map(|s| s.duration as u64).sum();
        let shown = (media as u128 * scale / scale_of(mdhd)?) as u64;
        longest = longest.max(shown);
        let stbl = table(child(trak, &[b"mdia", b"minf", b"stbl", b"stsd"])?, samples);
        let (tkhd, mdhd) = (widened(tkhd, 8, shown)?, widened(mdhd, 4, media)?);
        let trak = edit(trak, &[b"mdia", b"minf", b"stbl"], &mut |_| stbl.clone())?;
        let trak = edit(&trak, &[b"mdia", b"mdhd"], &mut |_| mdhd.clone())?;
        traks.push(boxed(b"trak", &edit(&trak, &[b"tkhd"], &mut |_| tkhd.clone())?));
    }
    let mut out = boxed(b"mvhd", &widened(mvhd, 4, longest)?);
    for (kind, body) in boxes(moov)? {
        if !matches!(&kind, b"mvhd" | b"trak" | b"mvex") {
            out.extend(boxed(&kind, body));
        }
    }
    out.extend(traks.concat());
    Some(boxed(b"moov", &out))
}

/// An stbl body for `samples` under the sample descriptions `stsd`, one chunk per run of samples
/// that sit back to back in the file.
fn table(stsd: &[u8], samples: &[Sample]) -> Vec<u8> {
    let full = |kind: &[u8; 4], version: u8, rows: &[Vec<u32>]| {
        let count = (rows.len() as u32).to_be_bytes();
        let rows: Vec<u8> = rows.iter().flatten().flat_map(|v| v.to_be_bytes()).collect();
        boxed(kind, &[&[version, 0, 0, 0][..], &count, &rows].concat())
    };
    let runs = |values: &mut dyn Iterator<Item = u32>| {
        let mut rows: Vec<Vec<u32>> = Vec::new();
        for value in values {
            match rows.last_mut() {
                Some(row) if row[1] == value => row[0] += 1,
                _ => rows.push(vec![1, value]),
            }
        }
        rows
    };
    let mut chunks: Vec<(u64, u64, u32)> = Vec::new();
    for s in samples {
        match chunks.last_mut() {
            Some((_, end, count)) if *end == s.offset => {
                *end += s.size as u64;
                *count += 1;
            }
            _ => chunks.push((s.offset, s.offset + s.size as u64, 1)),
        }
    }
    let mut stsc: Vec<Vec<u32>> = Vec::new();
    for (i, &(_, _, count)) in chunks.iter().enumerate() {
        if stsc.last().is_none_or(|row| row[1] != count) {
            stsc.push(vec![i as u32 + 1, count, 1]);
        }
    }
    let count = (samples.len() as u32).to_be_bytes();
    let same = samples.first().map(|s| s.size).filter(|&size| samples.iter().all(|s| s.size == size));
    let stsz: Vec<u8> = match same {
        Some(size) => [[0; 4], size.to_be_bytes(), count].concat(),
        None => [[0; 4], [0; 4], count]
            .into_iter()
            .flatten()
            .chain(samples.iter().flat_map(|s| s.size.to_be_bytes()))
            .collect(),
    };
    let wide = chunks.last().is_some_and(|&(at, _, _)| at > u32::MAX as u64);
    let offsets: Vec<u8> = match wide {
        true => chunks.iter().flat_map(|&(at, _, _)| at.to_be_bytes()).collect(),
        false => chunks.iter().flat_map(|&(at, _, _)| (at as u32).to_be_bytes()).collect(),
    };
    let mut stbl = [boxed(b"stsd", stsd), full(b"stts", 0, &runs(&mut samples.iter().map(|s| s.duration)))].concat();
    if samples.iter().any(|s| s.composition != 0) {
        // version 1 reads the offsets as signed, as a fragment's version 1 trun holds them
        stbl.extend(full(b"ctts", 1, &runs(&mut samples.iter().map(|s| s.composition))));
    }
    if samples.iter().any(|s| !s.sync) {
        let synced: Vec<Vec<u32>> =
            (samples.iter().enumerate()).filter(|(_, s)| s.sync).map(|(i, _)| vec![i as u32 + 1]).collect();
        stbl.extend(full(b"stss", 0, &synced));
    }
    stbl.extend(boxed(b"stsz", &stsz));
    stbl.extend(full(b"stsc", 0, &stsc));
    let chunk_count = (chunks.len() as u32).to_be_bytes();
    stbl.extend(boxed(if wide { b"co64" } else { b"stco" }, &[&[0; 4][..], &chunk_count, &offsets].concat()));
    stbl
}

/// An mvhd, tkhd or mdhd body as version 1 with `duration`, the 64 bit form, so hours at a fine
/// timescale fit. `middle` is how many bytes sit between the times and the duration.
fn widened(body: &[u8], middle: usize, duration: u64) -> Option<Vec<u8>> {
    let long = body.first()? == &1;
    let (times, at) = match long {
        true => (body.get(4..20)?.to_vec(), 20),
        false => {
            let time =
                |at: usize| Some((u32::from_be_bytes(body.get(at..at + 4)?.try_into().ok()?) as u64).to_be_bytes());
            ([time(4)?, time(8)?].concat(), 12)
        }
    };
    let rest = body.get(at + middle + if long { 8 } else { 4 }..)?;
    Some([&[1], body.get(1..4)?, &times, body.get(at..at + middle)?, &duration.to_be_bytes(), rest].concat())
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

    #[test]
    fn defragment_turns_whole_fragments_into_a_plain_mp4() {
        let be = |v: u32| v.to_be_bytes();
        let full = |kind: &[u8; 4], flags: u32, body: &[u8]| boxed(kind, &[&be(flags)[..], body].concat());
        let hdlr = |kind: &[u8; 4]| boxed(b"hdlr", &[&[0; 8][..], kind, &[0; 12], b"Handler\0"].concat());
        let trak = |id: u32, kind: &[u8; 4], entry: &[u8; 4], scale: u32| {
            let tkhd = full(b"tkhd", 0, &[&[0; 8][..], &be(id), &[0; 68]].concat());
            let mdhd = full(b"mdhd", 0, &[&[0; 8][..], &be(scale), &[0; 8]].concat());
            let stsd = full(b"stsd", 0, &[&be(1)[..], &boxed(entry, &[0; 78])].concat());
            let minf = boxed(b"minf", &boxed(b"stbl", &stsd));
            boxed(b"trak", &[tkhd, boxed(b"mdia", &[mdhd, hdlr(kind), minf].concat())].concat())
        };
        let trex = |id: u32, duration: u32, size: u32, flags: u32| {
            full(b"trex", 0, &[be(id), be(1), be(duration), be(size), be(flags)].concat())
        };
        let (sync, later) = (0x0200_0000, 0x1_0000);
        let mvex = boxed(b"mvex", &[trex(1, 1000, 0, later), trex(2, 1024, 9, sync)].concat());
        let mvhd = full(b"mvhd", 0, &[&[0; 8][..], &be(1000), &[0; 84]].concat());
        let moov = boxed(
            b"moov",
            &[mvhd, trak(1, b"vide", b"avc1", 30_000), trak(2, b"soun", b"mp4a", 48_000), mvex].concat(),
        );
        let mut file = [boxed(b"ftyp", b"isom"), moov].concat();
        let fill = |sizes: &[u32], first: u8| -> Vec<u8> {
            sizes.iter().zip(first..).flat_map(|(&size, byte)| vec![byte; size as usize]).collect()
        };
        // as media foundation writes them: absolute offsets and every field in the trun
        let moof = |base: u64| {
            let video = [
                full(b"tfhd", 0x1, &[&be(1)[..], &base.to_be_bytes()].concat()),
                full(
                    b"trun",
                    0xf01,
                    &[be(2), be(0), be(1000), be(3), be(sync), be(0), be(1000), be(4), be(later), be(500)].concat(),
                ),
            ];
            let sound = [
                full(b"tfhd", 0x1, &[&be(2)[..], &(base + 7).to_be_bytes()].concat()),
                full(b"trun", 0x301, &[be(3), be(0), be(1024), be(5), be(1024), be(6), be(1024), be(7)].concat()),
            ];
            let trafs = [boxed(b"traf", &video.concat()), boxed(b"traf", &sound.concat())];
            boxed(b"moof", &[full(b"mfhd", 0, &be(1)), trafs.concat()].concat())
        };
        let start = file.len() as u64;
        let base = start + moof(0).len() as u64 + 8;
        let media = [fill(&[3, 4], 0x10), fill(&[5, 6, 7], 0x40)].concat();
        file.extend([moof(base), boxed(b"mdat", &media)].concat());
        // the other way: offsets from the moof, sizes and flags from the trex defaults
        let moof = |into: u32| {
            let video = [
                full(b"tfhd", 0x2_0000, &be(1)),
                full(b"trun", 0x205, &[be(2), be(into), be(sync), be(2), be(3)].concat()),
            ];
            let sound = [full(b"tfhd", 0x2_0000, &be(2)), full(b"trun", 0x1, &[be(2), be(into + 5)].concat())];
            let trafs = [boxed(b"traf", &video.concat()), boxed(b"traf", &sound.concat())];
            boxed(b"moof", &[full(b"mfhd", 0, &be(2)), trafs.concat()].concat())
        };
        let into = moof(0).len() as u32 + 8;
        let media = [fill(&[2, 3], 0x12), fill(&[9, 9], 0x43)].concat();
        file.extend([moof(into), boxed(b"mdat", &media)].concat());
        // cut off mid fragment, as a crash leaves it
        file.extend(&moof(into)[..]);
        file.extend(&boxed(b"mdat", &media)[..12]);

        let path = std::env::temp_dir().join(format!("clip-defragment-{}.mp4", std::process::id()));
        std::fs::write(&path, &file[..start as usize + 20]).unwrap();
        assert!(defragment(&path).is_err(), "no whole fragment to keep");
        std::fs::write(&path, &file).unwrap();
        defragment(&path).unwrap();
        let data = std::fs::read(&path).unwrap();
        let kinds: Vec<[u8; 4]> = boxes(&data).unwrap().into_iter().map(|(kind, _)| kind).collect();
        assert_eq!(kinds, [*b"ftyp", *b"free", *b"free", *b"mdat", *b"free", *b"mdat", *b"moov"]);
        let probe = probe(&path).unwrap();
        assert_eq!((probe.seconds, probe.fps), (0.133, 30.0));
        assert_eq!(frame_sizes(&path).unwrap(), [[5, 6, 7, 9, 9]]);

        let moov = moov_of(&path).unwrap();
        let moov = child(&moov, &[b"moov"]).unwrap();
        assert!(child(moov, &[b"mvex"]).is_none());
        let words = |stbl: &[u8], kind: &[u8; 4]| -> Vec<u32> {
            child(stbl, &[kind]).unwrap()[4..].chunks(4).map(|w| u32::from_be_bytes(w.try_into().unwrap())).collect()
        };
        let traks: Vec<_> = boxes(moov).unwrap().into_iter().filter(|(kind, _)| kind == b"trak").collect();
        let mut firsts = Vec::new();
        for (_, trak) in &traks {
            let stbl = child(trak, &[b"mdia", b"minf", b"stbl"]).unwrap();
            let (stco, stsc, stsz) = (words(stbl, b"stco"), words(stbl, b"stsc"), words(stbl, b"stsz"));
            let sizes = match stsz[0] {
                0 => stsz[2..].to_vec(),
                size => vec![size; stsz[1] as usize],
            };
            let mut sizes = sizes.into_iter();
            for (chunk, &offset) in stco[1..].iter().enumerate() {
                let row = stsc[1..].chunks(3).rfind(|row| row[0] as usize <= chunk + 1).unwrap();
                let mut at = offset as usize;
                for size in sizes.by_ref().take(row[1] as usize) {
                    assert!(data[at..at + size as usize].iter().all(|&b| b == data[at]));
                    firsts.push(data[at]);
                    at += size as usize;
                }
            }
        }
        assert_eq!(firsts, [0x10, 0x11, 0x12, 0x13, 0x40, 0x41, 0x42, 0x43, 0x44]);
        let video = child(traks[0].1, &[b"mdia", b"minf", b"stbl"]).unwrap();
        assert_eq!(words(video, b"stss"), [2, 1, 3]);
        assert_eq!(words(video, b"ctts"), [3, 1, 0, 1, 500, 2, 0]);

        defragment(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data, "a plain mp4 is left alone");
        // a crash after the new moov landed but before the old indexes were freed
        let mut cut = data.clone();
        let mut at = 0;
        for (i, (_, body)) in boxes(&data).unwrap().into_iter().enumerate() {
            match i {
                1 => cut[at + 4..at + 8].copy_from_slice(b"moov"),
                2 | 4 => cut[at + 4..at + 8].copy_from_slice(b"moof"),
                _ => {}
            }
            at += body.len() + 8;
        }
        std::fs::write(&path, &cut).unwrap();
        defragment(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), data, "the next start finishes freeing them");

        std::fs::write(&path, &file).unwrap();
        std::fs::write(beside(&path, NAMES), "game").unwrap();
        recover(&path).unwrap();
        assert_eq!(super::probe(&path).unwrap().tracks, ["game"]);
        assert!(!beside(&path, NAMES).exists(), "the names go once they're in the moov");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn own_fragments_rebuild_into_the_samples_written() {
        let be = |v: u32| v.to_be_bytes();
        let full = |kind: &[u8; 4], body: &[u8]| boxed(kind, &[&[0; 4][..], body].concat());
        let hdlr = |kind: &[u8; 4]| boxed(b"hdlr", &[&[0; 8][..], kind, &[0; 12], b"Handler\0"].concat());
        // as media foundation leaves a plain mp4 of one frame
        let trak = |id: u32, kind: &[u8; 4], entry: &[u8; 4], scale: u32| {
            let tkhd = full(b"tkhd", &[&[0; 8][..], &be(id), &[0; 68]].concat());
            let edts = boxed(b"edts", &full(b"elst", &[0; 16]));
            let mdhd = full(b"mdhd", &[&[0; 8][..], &be(scale), &[0; 8]].concat());
            let stsd = full(b"stsd", &[&be(1)[..], &boxed(entry, &[0; 78])].concat());
            let stts = full(b"stts", &[be(1), be(1), be(1000)].concat());
            let minf = boxed(b"minf", &boxed(b"stbl", &[stsd, stts].concat()));
            boxed(b"trak", &[tkhd, edts, boxed(b"mdia", &[mdhd, hdlr(kind), minf].concat())].concat())
        };
        let mvhd = full(b"mvhd", &[&[0; 8][..], &be(1000), &[0; 84]].concat());
        let moov = [mvhd, trak(1, b"vide", b"hvc1", 30_000), trak(2, b"soun", b"mp4a", 48_000)].concat();
        let moov = [moov, trak(3, b"soun", b"mp4a", 48_000)].concat();
        let (moov, tracks) = emptied(&moov).unwrap();
        let found: Vec<(u32, u64)> = tracks.iter().map(|t| (t.id, t.scale)).collect();
        assert_eq!(found, [(1, 30_000), (2, 48_000), (3, 48_000)]);
        assert!(child(&moov, &[b"trak", b"edts"]).is_none());
        assert_eq!(child(&moov, &[b"mvex", b"trex"]).unwrap()[4..8], be(1));

        let path = std::env::temp_dir().join(format!("clip-fragments-{}.mp4", std::process::id()));
        std::fs::write(&path, [boxed(b"ftyp", b"isom"), boxed(b"moov", &moov)].concat()).unwrap();
        let file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let mut own = Fragments { file, sequence: 0, tracks };
        let (frame, tick) = (333_334, 213_333);
        let packet = |pts: i64, duration: i64, key: bool, data: &[u8]| {
            Arc::new(Packet { pts: 1_000 + pts, duration, key, data: data.to_vec() })
        };
        // the vps leaves the keyframe, the access unit delimiter and slices stay
        let video = [
            packet(0, frame, true, &[0, 0, 0, 1, 0x40, 1, 7, 0, 0, 0, 1, 0x46, 1, 0x50, 0, 0, 1, 0x26, 1, 5, 5]),
            packet(frame, frame, false, &[0, 0, 0, 1, 0x02, 1, 9]),
            packet(2 * frame, frame, false, &[0, 0, 1, 0x02, 1, 8, 8]),
        ];
        let sound: Vec<_> = (0..5).map(|i| packet(i * tick, tick, true, &vec![i as u8; 6 + i as usize])).collect();
        own.write(&video[..2], &[sound[..2].to_vec(), Vec::new()], 1_000).unwrap();
        own.write(&video[2..], &[sound[2..4].to_vec(), Vec::new()], 1_000).unwrap();

        let crashed = path.with_extension("crashed.mp4");
        std::fs::copy(&path, &crashed).unwrap();
        std::fs::write(beside(&crashed, NAMES), "game\nvoice").unwrap();
        std::fs::write(beside(&crashed, SCREEN), [mdcv(PANEL), clli(PANEL)].concat()).unwrap();
        recover(&crashed).unwrap();
        let probe = super::probe(&crashed).unwrap();
        assert_eq!((probe.tracks, probe.fps), (vec!["game".to_string(), "voice".to_string()], 30.0));
        assert!((probe.mastering.unwrap().max - PANEL.max).abs() < 0.01);
        assert_eq!(frame_sizes(&crashed).unwrap(), [vec![6, 7, 8], vec![]], "the newest sample waits for the next");
        std::fs::remove_file(&crashed).unwrap();

        own.write(&[], &[sound[4..].to_vec(), Vec::new()], 1_000).unwrap();
        own.flush().unwrap();
        drop(own);
        defragment(&path).unwrap();
        assert_eq!(frame_sizes(&path).unwrap(), [vec![6, 7, 8, 9, 10], vec![]]);
        let moov = moov_of(&path).unwrap();
        let stbl = child(&moov, &[b"moov", b"trak", b"mdia", b"minf", b"stbl"]).unwrap();
        let words = |kind: &[u8; 4]| -> Vec<u32> {
            child(stbl, &[kind]).unwrap()[4..].chunks(4).map(|w| u32::from_be_bytes(w.try_into().unwrap())).collect()
        };
        assert_eq!(words(b"stts"), [1, 3, 1000], "a frame is 1000 units at 30000 a second");
        assert_eq!(words(b"stss"), [1, 1]);
        assert_eq!(words(b"stsz"), [0, 3, 15, 7, 8]);
        let data = std::fs::read(&path).unwrap();
        let at = words(b"stco")[1] as usize;
        assert_eq!(data[at..at + 15], [0, 0, 0, 3, 0x46, 1, 0x50, 0, 0, 0, 4, 0x26, 1, 5, 5], "length prefixed");
        std::fs::remove_file(&path).unwrap();
    }
}

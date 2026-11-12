use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::{Interface, Result, GUID, HSTRING};

use windows::Win32::Foundation::{E_ABORT, E_FAIL, E_INVALIDARG};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709, DXGI_FORMAT_NV12,
    DXGI_FORMAT_P010, DXGI_SAMPLE_DESC,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

use crate::audio::{audio_type, Aac, AAC_BYTES_PER_SECOND, BLOCK, CHANNELS, RATE};
use crate::capture::Converter;
use crate::encode::{Codec, Encoder, Format, Purpose};
use crate::ring::{Packet, SECOND};
use crate::sync::each_sample;

/// Every audio track of a clip as 16 bit stereo pcm at `RATE` from the clip's start, with its name.
pub fn tracks(path: &Path) -> std::result::Result<Vec<(String, Vec<u8>)>, String> {
    let mut all = Vec::new();
    each_track(path, |name, pcm| {
        all.push((name, pcm));
        Ok(())
    })?;
    Ok(all)
}

/// Hands `each` the audio tracks of a clip one at a time, like `tracks`, so only one is held at once.
pub fn each_track(
    path: &Path,
    mut each: impl FnMut(String, Vec<u8>) -> std::result::Result<(), String>,
) -> std::result::Result<(), String> {
    let names = crate::save::probe(path).map(|probe| probe.tracks).unwrap_or_default();
    let unreadable = |e: windows::core::Error| format!("can't read the clip's audio: {}", e.message());
    let reader = unsafe { MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None) }.map_err(unreadable)?;
    for (i, stream) in unsafe { audio_streams(&reader, path) }.map_err(unreadable)?.into_iter().enumerate() {
        let pcm = decode(&reader, stream).map_err(unreadable)?;
        let name = names.get(i).filter(|name| !name.is_empty()).cloned();
        each(name.unwrap_or_else(|| format!("track {}", i + 1)), pcm)?;
    }
    Ok(())
}

fn decode(reader: &IMFSourceReader, stream: u32) -> Result<Vec<u8>> {
    unsafe { reader.SetCurrentMediaType(stream, None, &audio_type(MFAudioFormat_PCM)?)? };
    let mut pcm = Vec::new();
    each_sample(reader, stream, |time, data| {
        let at = (time.max(0) * RATE as i64 / SECOND) as usize * BLOCK;
        if pcm.len() < at {
            pcm.resize(at, 0);
        }
        pcm.extend_from_slice(data);
    })?;
    Ok(pcm)
}

/// The reader's audio streams in the file's track order. The mpeg-4 source numbers its streams in
/// another order than the file's tracks (reversed on windows 11), so each stream is put back in
/// its track's place by the frame sizes it carries.
pub unsafe fn audio_streams(reader: &IMFSourceReader, path: &Path) -> Result<Vec<u32>> {
    let found: Vec<u32> = (0..)
        .map_while(|i| reader.GetCurrentMediaType(i).ok().map(|media| (i, media)))
        .filter(|(_, media)| media.GetGUID(&MF_MT_MAJOR_TYPE).is_ok_and(|major| major == MFMediaType_Audio))
        .map(|(i, _)| i)
        .collect();
    let tracks = crate::save::frame_sizes(path).unwrap_or_default();
    Ok(in_track_order(&found, &stream_frame_sizes(reader, &found)?, &tracks))
}

/// The compressed size of every frame of each stream, read in one pass without decoding.
unsafe fn stream_frame_sizes(reader: &IMFSourceReader, streams: &[u32]) -> Result<Vec<Vec<u32>>> {
    reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
    for &stream in streams {
        reader.SetStreamSelection(stream, true)?;
    }
    let mut sizes = vec![Vec::new(); streams.len()];
    let mut ended = 0;
    while ended < streams.len() {
        let (mut index, mut flags, mut sample) = (0, 0, None);
        reader.ReadSample(
            MF_SOURCE_READER_ANY_STREAM.0 as u32,
            0,
            Some(&mut index),
            Some(&mut flags),
            None,
            Some(&mut sample),
        )?;
        if flags & MF_SOURCE_READERF_ERROR.0 as u32 != 0 {
            return Err(E_FAIL.into());
        }
        let Some(lane) = streams.iter().position(|&stream| stream == index) else { continue };
        if let Some(sample) = sample {
            sizes[lane].push(sample.GetTotalLength()?);
        }
        if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
            ended += 1;
        }
    }
    Ok(sizes)
}

/// The streams reordered so the nth is the one whose frames match the file's nth audio track.
/// Streams that match no track fill the places left, and tracks with the same frames are
/// interchangeable.
fn in_track_order(streams: &[u32], seen: &[Vec<u32>], tracks: &[Vec<u32>]) -> Vec<u32> {
    let mut taken = vec![false; tracks.len().max(streams.len())];
    let matched: Vec<Option<usize>> = seen
        .iter()
        .map(|frames| {
            let track = (0..tracks.len()).find(|&track| !taken[track] && tracks[track] == *frames)?;
            taken[track] = true;
            Some(track)
        })
        .collect();
    let mut placed: Vec<(usize, u32)> = matched
        .into_iter()
        .zip(streams)
        .map(|(track, &stream)| {
            let track = track.unwrap_or_else(|| {
                let free = taken.iter().position(|taken| !taken).unwrap_or(0);
                taken[free] = true;
                free
            });
            (track, stream)
        })
        .collect();
    placed.sort_unstable();
    placed.into_iter().map(|(_, stream)| stream).collect()
}

pub fn write_wav(path: &Path, pcm: &[u8]) -> std::io::Result<()> {
    let size = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + size).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&CHANNELS.to_le_bytes());
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * BLOCK as u32).to_le_bytes());
    wav.extend_from_slice(&(BLOCK as u16).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&size.to_le_bytes());
    let mut file = std::fs::File::create(path)?;
    file.write_all(&wav)?;
    file.write_all(pcm)
}

/// The loudest sample in each of `count` equal stretches of 16 bit stereo pcm, from 0 to 1, padded with
/// silence when there are fewer frames than stretches.
pub fn peaks(pcm: &[u8], count: usize) -> Vec<f32> {
    let per = (pcm.len() / BLOCK / count).max(1) * BLOCK;
    let loudest = |part: &[u8]| part.as_chunks::<2>().0.iter().map(|&s| i16::from_le_bytes(s).unsigned_abs()).max();
    pcm.chunks(per)
        .map(|part| f32::from(loudest(part).unwrap_or(0)) / 32768.0)
        .chain(std::iter::repeat(0.0))
        .take(count)
        .collect()
}

/// How an export is shaped beyond the cut: a size to fit under, a 9:16 window, and a way to hear
/// how far it got and to stop it.
#[derive(Default)]
pub struct Job<'a> {
    pub fit: Option<u64>,
    /// where the middle of the 9:16 window sits across the picture, from 0 to 1
    pub portrait: Option<f64>,
    pub progress: Option<&'a dyn Fn(f64)>,
    pub cancel: Option<&'a AtomicBool>,
}

/// Writes the `ranges` of `source`, in seconds and in order, one after another to `out`, re-encoded
/// on the gpu so each cut lands on the exact frames asked for. Only the audio `tracks` listed are
/// kept, summed into one when `mix`.
/// With a size to fit, the video gets what the audio leaves, the frame rate and then the picture
/// come down when that is too little for them, and a copy that comes out over is encoded again lower.
pub fn trim(
    source: &Path,
    out: &Path,
    ranges: &[(f64, f64)],
    tracks: &[usize],
    mix: bool,
    job: &Job,
) -> std::result::Result<(), String> {
    let probe = crate::save::probe(source).map_err(|e| format!("can't read the clip: {e}"))?;
    let whole = matches!(ranges, [(start, end)] if *start <= 0.05 && *end >= probe.seconds - 0.05)
        && tracks.len() == probe.tracks.len();
    let bytes = std::fs::metadata(source).map_or(u64::MAX, |meta| meta.len());
    if whole && probe.tracks.len() <= 1 && job.portrait.is_none() && job.fit.is_some_and(|budget| bytes <= budget) {
        // it already fits, and encoding it again would only lose detail
        std::fs::copy(source, out).map_err(|e| format!("can't copy the clip: {e}"))?;
        return Ok(());
    }
    let streams = if mix { tracks.len().min(1) } else { tracks.len() } as u64;
    // aiming a little under covers encoder overshoot and sites that count a megabyte as 2^20
    let goal = job.fit.map(|bytes| bytes / 100 * 97);
    // small budgets keep more for the picture, 96 kbps aac still sounds clean for game audio
    let aac_rate = match job.fit {
        Some(..=50_000_000) => 12_000,
        _ => AAC_BYTES_PER_SECOND,
    };
    let target = match goal {
        Some(bytes) => Some(video_bitrate(
            bytes,
            ranges.iter().map(|(start, end)| end - start).sum(),
            streams * aac_rate as u64 * 8,
        )?),
        None => None,
    };
    let ticks: Vec<(i64, i64)> =
        ranges.iter().map(|(start, end)| ((start * SECOND as f64) as i64, (end * SECOND as f64) as i64)).collect();
    let video_of = |target| {
        trim_video(&[(source, &ticks)], target, job).map_err(|e| {
            if e.code() == E_ABORT {
                CANCELLED.to_string()
            } else {
                format!("can't re-encode the video: {}", e.message())
            }
        })
    };
    let (mut video, mut format, spans) = video_of(target)?;
    if video.is_empty() {
        return Err("there are no frames between the start and the end".into());
    }
    let all = self::tracks(source)?;
    let chosen: Vec<&(String, Vec<u8>)> = tracks
        .iter()
        .map(|&i| all.get(i).ok_or(format!("the clip has no track {i}")))
        .collect::<std::result::Result<_, _>>()?;
    let span = |pcm: &[u8]| heard(pcm, &spans);
    let lanes: Vec<(String, Vec<u8>)> = match (mix, chosen.as_slice()) {
        (_, []) => Vec::new(),
        (true, [_, _, ..]) => {
            let parts: Vec<Vec<u8>> = chosen.iter().map(|(_, pcm)| span(pcm)).collect();
            let name = chosen.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(" + ");
            vec![(name, sum(&parts))]
        }
        _ => chosen.iter().map(|(name, pcm)| (name.clone(), span(pcm))).collect(),
    };
    let audio = lanes
        .iter()
        .map(|(_, pcm)| aac(pcm, aac_rate))
        .collect::<Result<Vec<_>>>()
        .map_err(|e| format!("can't encode the audio: {}", e.message()))?;
    let names: Vec<&str> = lanes.iter().map(|(name, _)| name.as_str()).collect();
    let mastering = probe.mastering.filter(|_| format.codec == Codec::Hdr10);
    let write = |video: &[Arc<Packet>], format| {
        crate::save::write_mp4(out, video, format, &audio, &names, mastering)
            .map_err(|e| format!("can't write {}: {}", out.display(), e.message()))
    };
    write(&video, format)?;
    if let (Some(budget), Some(goal)) = (job.fit, goal) {
        // a busy stretch can still push the rate over, each retry aims as far under as the last went over
        for _ in 0..2 {
            let size = std::fs::metadata(out).map_or(u64::MAX, |meta| meta.len());
            if size <= budget {
                break;
            }
            let bitrate = (format.bitrate as f64 * goal as f64 / size as f64 * 0.97) as u32;
            (video, format, _) = video_of(Some(bitrate))?;
            write(&video, format)?;
        }
        let size = std::fs::metadata(out).map_or(u64::MAX, |meta| meta.len());
        if size > budget {
            let _ = std::fs::remove_file(out);
            return Err(format!("couldn't get it under {} MB, trim it shorter", budget / 1_000_000));
        }
    }
    Ok(())
}

/// The sound under each of the `spans` of frames kept, one after another, each as long as its frames
/// so the parts after it stay in sync.
fn heard(pcm: &[u8], spans: &[(i64, i64)]) -> Vec<u8> {
    let mut kept = Vec::new();
    for &(first, length) in spans {
        let from = (first * RATE as i64 / SECOND) as usize * BLOCK;
        let samples = (length * RATE as i64 / SECOND) as usize * BLOCK;
        let part = pcm.get(from.min(pcm.len())..(from + samples).min(pcm.len())).unwrap_or_default();
        kept.extend_from_slice(part);
        kept.resize(kept.len() + samples - part.len(), 0);
    }
    kept
}

/// Writes the `clips` whole, one after another, to `out` in the first one's format, re-encoded on the
/// gpu. Tracks go together by name, silent where a clip has none of that name. Returns where each
/// clip starts in the joined one, in seconds.
pub fn join(clips: &[std::path::PathBuf], out: &Path, job: &Job) -> std::result::Result<Vec<f64>, String> {
    let ends: Vec<[(i64, i64); 1]> = clips
        .iter()
        .map(|clip| match crate::save::probe(clip) {
            // past the end the reader stops at the last frame
            Ok(probe) => Ok([(0, (probe.seconds * SECOND as f64) as i64 + SECOND)]),
            Err(e) => Err(format!("can't read {}: {e}", clip.display())),
        })
        .collect::<std::result::Result<_, _>>()?;
    let cuts: Vec<(&Path, &[(i64, i64)])> =
        clips.iter().zip(&ends).map(|(clip, end)| (clip.as_path(), &end[..])).collect();
    let (video, format, spans) = trim_video(&cuts, None, job).map_err(|e| match e.code() {
        E_ABORT => CANCELLED.to_string(),
        E_INVALIDARG => format!("{}, join clips recorded alike", e.message()),
        _ => format!("can't re-encode the video: {}", e.message()),
    })?;
    let mut lanes: Vec<(String, Vec<u8>)> = Vec::new();
    let mut length = 0;
    for (clip, span) in clips.iter().zip(&spans) {
        let mut taken = Vec::new();
        for (name, pcm) in tracks(clip)? {
            let lane = lanes.iter().enumerate().position(|(i, (named, _))| *named == name && !taken.contains(&i));
            let lane = lane.unwrap_or_else(|| {
                lanes.push((name, vec![0; length]));
                lanes.len() - 1
            });
            lanes[lane].1.extend(heard(&pcm, std::slice::from_ref(span)));
            taken.push(lane);
        }
        length += heard(&[], std::slice::from_ref(span)).len();
        for (_, pcm) in &mut lanes {
            pcm.resize(length, 0);
        }
    }
    let audio = lanes
        .iter()
        .map(|(_, pcm)| aac(pcm, AAC_BYTES_PER_SECOND))
        .collect::<Result<Vec<_>>>()
        .map_err(|e| format!("can't encode the audio: {}", e.message()))?;
    let names: Vec<&str> = lanes.iter().map(|(name, _)| name.as_str()).collect();
    let mastering =
        crate::save::probe(&clips[0]).ok().and_then(|probe| probe.mastering).filter(|_| format.codec == Codec::Hdr10);
    crate::save::write_mp4(out, &video, format, &audio, &names, mastering)
        .map_err(|e| format!("can't write {}: {}", out.display(), e.message()))?;
    let starts = spans.iter().scan(0, |at, &(_, kept)| Some(std::mem::replace(at, *at + kept) as f64 / SECOND as f64));
    Ok(starts.collect())
}

pub const CANCELLED: &str = "the export was cancelled";

// room for the mp4's boxes and sample tables, a few tens of kB for a minute at 60 fps
const OVERHEAD: u64 = 200_000;

/// The video bitrate that leaves room for `audio` bits per second when `seconds` of clip has to
/// fit in `bytes`.
fn video_bitrate(bytes: u64, seconds: f64, audio: u64) -> std::result::Result<u32, String> {
    let video = (bytes.saturating_sub(OVERHEAD) as f64 * 8.0 * 0.95 / seconds) - audio as f64;
    if video < 300_000.0 {
        return Err(format!("{seconds:.0} s is too long for that size, trim it shorter or pick a bigger one"));
    }
    Ok(video as u32)
}

// bits for each pixel of each frame: under the first a smooth half rate beats a smeared full one,
// under the second a smaller sharp picture beats a big blocky one
const FLUID: f64 = 0.06;
const SHARP: f64 = 0.04;

/// The tallest picture and how many source frames make one kept frame for `bitrate` to cover a
/// `shown` picture at `fps`.
fn ladder(bitrate: u32, shown: (u32, u32), fps: u32) -> (u32, u32) {
    let density = |(width, height): (u32, u32), fps: u32| bitrate as f64 / (width as f64 * height as f64 * fps as f64);
    let step = if fps >= 50 && density(shown, fps) < FLUID { (fps / 30).max(2) } else { 1 };
    let rate = fps / step;
    let tallest = [shown.1, 1080, 720, 540, 360]
        .into_iter()
        .filter(|&height| height <= shown.1)
        .find(|&height| density(crate::capture::output_size(shown, height), rate) >= SHARP)
        .unwrap_or(shown.1.min(360));
    (tallest, step)
}

/// The left edge and width of a 9:16 window centred at `middle` across a `shown` picture, both
/// even as nv12 and p010 need.
fn window(shown: (u32, u32), middle: f64) -> (u32, u32) {
    let width = ((shown.1 as f64 * 9.0 / 16.0).round() as u32).min(shown.0) & !1;
    let left =
        (middle.clamp(0.0, 1.0) * shown.0 as f64 - width as f64 / 2.0).round().clamp(0.0, (shown.0 - width) as f64);
    (left as u32 & !1, width)
}

/// The size of the picture shown, smaller than the coded frame when the encoder padded it, as
/// hevc pads 720 rows to 736.
pub fn picture(media: &IMFMediaType) -> Result<(u32, u32)> {
    let mut area = MFVideoArea::default();
    let blob = unsafe { std::slice::from_raw_parts_mut((&raw mut area).cast(), size_of::<MFVideoArea>()) };
    match unsafe { media.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, blob, None) } {
        Ok(()) if area.Area.cx > 0 && area.Area.cy > 0 => Ok((area.Area.cx as u32, area.Area.cy as u32)),
        _ => {
            let size = unsafe { media.GetUINT64(&MF_MT_FRAME_SIZE)? };
            Ok(((size >> 32) as u32, size as u32))
        }
    }
}

fn sum(parts: &[Vec<u8>]) -> Vec<u8> {
    let length = parts.iter().map(Vec::len).max().unwrap_or(0);
    let mut total = vec![0i32; length / 2];
    for part in parts {
        for (total, sample) in total.iter_mut().zip(part.as_chunks::<2>().0) {
            *total += i16::from_le_bytes(*sample) as i32;
        }
    }
    total.iter().flat_map(|&t| (t.clamp(i16::MIN as i32, i16::MAX as i32) as i16).to_le_bytes()).collect()
}

fn aac(pcm: &[u8], bytes_per_second: u32) -> Result<Vec<Arc<Packet>>> {
    let mut encoder = Aac::at(bytes_per_second)?;
    let chunk = RATE as usize / 10 * BLOCK;
    let mut packets = Vec::new();
    for (n, part) in pcm.chunks(chunk).enumerate() {
        let pts = (n * chunk / BLOCK) as i64 * SECOND / RATE as i64;
        packets.extend(encoder.encode(pts, part)?);
    }
    packets.extend(encoder.finish()?);
    Ok(packets.into_iter().map(Arc::new).collect())
}

/// For each range, where its first kept frame was in the source and how long its kept frames last.
type Spans = Vec<(i64, i64)>;

/// The codec, picture and frame rate of a clip's video.
fn shape(native: &IMFMediaType) -> Result<(Codec, (u32, u32), u32)> {
    let codec = match unsafe { native.GetGUID(&MF_MT_SUBTYPE)? } {
        subtype if subtype == MFVideoFormat_HEVC => Codec::Hdr10,
        _ => Codec::H264,
    };
    let rate = unsafe { native.GetUINT64(&MF_MT_FRAME_RATE)? };
    let fps = ((rate >> 32) as f64 / (rate & 0xffff_ffff).max(1) as f64).round().max(1.0) as u32;
    Ok((codec, picture(native)?, fps))
}

/// Decodes the frames of each clip's ranges on the gpu and encodes them again one after another from
/// time 0, at the first clip's bitrate and size or at most the `target` bitrate, cut to the job's
/// 9:16 window. Returns the packets, their format and what each range kept.
fn trim_video(
    cuts: &[(&Path, &[(i64, i64)])],
    target: Option<u32>,
    job: &Job,
) -> Result<(Vec<Arc<Packet>>, Format, Spans)> {
    let gpu = crate::capture::gpu()?;
    unsafe {
        let mut token = 0;
        let mut manager = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
        let manager = manager.ok_or(windows::core::Error::from(E_FAIL))?;
        manager.ResetDevice(&gpu.device, token)?;
        let mut attributes = None;
        MFCreateAttributes(&mut attributes, 2)?;
        let attributes = attributes.ok_or(windows::core::Error::from(E_FAIL))?;
        attributes.SetUnknown(&MF_SOURCE_READER_D3D_MANAGER, &manager)?;
        attributes.SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, 1)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let open = |source: &Path| -> Result<(IMFSourceReader, IMFMediaType)> {
            let reader = MFCreateSourceReaderFromURL(&HSTRING::from(source.as_os_str()), &attributes)?;
            reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
            reader.SetStreamSelection(stream, true)?;
            let native = reader.GetNativeMediaType(stream, 0)?;
            Ok((reader, native))
        };
        let [(source, _), ..] = cuts else { return Err(windows::core::Error::from(E_INVALIDARG)) };
        let (reader, native) = open(source)?;
        let (codec, full, fps) = shape(&native)?;
        let (left, width) = job.portrait.map_or((0, full.0), |middle| window(full, middle));
        let shown = (width, full.1);
        let bytes = std::fs::metadata(source).map_or(0, |meta| meta.len());
        let seconds = crate::save::probe(source).map_or(0.0, |probe| probe.seconds).max(1.0);
        let bitrate = native.GetUINT32(&MF_MT_AVG_BITRATE).unwrap_or((bytes as f64 * 8.0 / seconds) as u32);
        // a narrower window needs fewer bits for the same detail
        let bitrate = (bitrate as u64 * shown.0 as u64 / full.0.max(1) as u64) as u32;
        let (bitrate, (width, height), step) = match target {
            Some(most) => {
                let (tallest, step) = ladder(most, shown, fps);
                (bitrate.min(most), crate::capture::output_size(shown, tallest), step)
            }
            None => (bitrate, shown, 1),
        };
        let format = Format { width, height, fps: fps / step, bitrate, codec };

        let (subtype, texture_format, space) = match codec {
            Codec::H264 => (MFVideoFormat_NV12, DXGI_FORMAT_NV12, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709),
            Codec::Hdr10 => (MFVideoFormat_P010, DXGI_FORMAT_P010, DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020),
        };
        let decoded = MFCreateMediaType()?;
        decoded.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        decoded.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        // the source reader's own video processor lifts pq black from 64 to 93, so the picture is
        // shrunk by one told the same color space on both sides
        let mut shrink = ((width, height) != shown)
            .then(|| Converter::new(&gpu, shown, (width, height), fps, texture_format, (space, space)))
            .transpose()?;

        let choices = crate::encode::encoders(codec, Some(gpu.adapter))?;
        let mut encoder = choices
            .iter()
            .find_map(|choice| Encoder::open(choice, &gpu.device, format, Purpose::Export).ok())
            .ok_or(windows::core::Error::new(MF_E_TOPO_CODEC_NOT_FOUND, "no encoder opened"))?;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: shown.0,
            Height: shown.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: texture_format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: 0,
        };
        // the encoder may still be reading a frame when the next is decoded
        let mut targets = Vec::new();
        for _ in 0..4 {
            let mut texture = None;
            gpu.device.CreateTexture2D(&desc, None, Some(&mut texture))?;
            targets.push(texture.ok_or(windows::core::Error::from(E_FAIL))?);
        }
        // decoders pad the coded height to a multiple of 16, so only the picture is copied
        let picture = D3D11_BOX { left, top: 0, front: 0, right: left + shown.0, bottom: shown.1, back: 1 };

        let total = cuts.iter().flat_map(|(_, ranges)| *ranges).map(|(start, end)| end - start).sum::<i64>().max(1);
        let (mut packets, mut spans, mut kept, mut seen, mut done) = (Vec::new(), Vec::new(), 0i64, 0u32, 0);
        let mut opened = Some(reader);
        for &(source, ranges) in cuts {
            let reader = match opened.take() {
                Some(reader) => reader,
                None => {
                    let (reader, native) = open(source)?;
                    if shape(&native)? != (codec, full, fps) {
                        return Err(windows::core::Error::new(
                            E_INVALIDARG,
                            "the clips differ in size, frame rate or hdr",
                        ));
                    }
                    reader
                }
            };
            reader.SetCurrentMediaType(stream, None, &decoded)?;
            for &(start, end) in ranges {
                reader.SetCurrentPosition(&GUID::zeroed(), &PROPVARIANT::from(start))?;
                let (mut first, began) = (None, kept);
                loop {
                    if job.cancel.is_some_and(|cancel| cancel.load(Ordering::Relaxed)) {
                        return Err(windows::core::Error::from(E_ABORT));
                    }
                    let (mut flags, mut time, mut sample) = (0, 0, None);
                    reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))?;
                    if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 || time >= end {
                        break;
                    }
                    let Some(sample) = sample.filter(|_| time >= start) else { continue };
                    seen += 1;
                    if (seen - 1) % step != 0 {
                        continue;
                    }
                    if let Some(progress) = job.progress {
                        progress((done + time - start) as f64 / total as f64);
                    }
                    let buffer = sample.GetBufferByIndex(0)?.cast::<IMFDXGIBuffer>().map_err(|_| {
                        windows::core::Error::new(MF_E_UNSUPPORTED_D3D_TYPE, "the gpu can't decode this clip")
                    })?;
                    let mut decoded = std::ptr::null_mut();
                    buffer.GetResource(&ID3D11Texture2D::IID, &mut decoded)?;
                    let decoded = ID3D11Texture2D::from_raw(decoded);
                    let target = &targets[kept as usize % targets.len()];
                    gpu.context.CopySubresourceRegion(
                        target,
                        0,
                        0,
                        0,
                        0,
                        &decoded,
                        buffer.GetSubresourceIndex()?,
                        Some(&picture),
                    );
                    let frame = match &mut shrink {
                        Some(shrink) => shrink.convert(target, shown, fps)?,
                        None => target.clone(),
                    };
                    packets.extend(encoder.encode(&frame, format.time(kept))?);
                    first.get_or_insert(time);
                    kept += 1;
                }
                spans.push((first.unwrap_or(start), format.time(kept) - format.time(began)));
                done += end - start;
            }
        }
        packets.extend(encoder.finish()?);
        Ok((packets.into_iter().map(Arc::new).collect(), format, spans))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streams_go_back_to_their_tracks_by_frame_sizes() {
        let tracks = [vec![9, 8, 7], vec![6, 6, 6], vec![5, 4, 3]];
        let seen = [tracks[2].clone(), tracks[1].clone(), tracks[0].clone()];
        assert_eq!(in_track_order(&[1, 2, 3], &seen, &tracks), [3, 2, 1]);
        assert_eq!(in_track_order(&[1, 2], &[vec![1], vec![2]], &[]), [1, 2]);
        assert_eq!(in_track_order(&[4, 5], &[vec![1], tracks[0].clone()], &tracks), [5, 4]);
    }

    #[test]
    fn a_tighter_budget_drops_the_frame_rate_and_then_the_picture() {
        let shape = |megabytes: u64, seconds: f64| {
            let bitrate = video_bitrate(megabytes * 970_000, seconds, 96_000).unwrap();
            ladder(bitrate, (1920, 1080), 60)
        };
        assert_eq!(shape(1000, 120.0), (1080, 1));
        assert_eq!(shape(100, 30.0), (1080, 1));
        assert_eq!(shape(50, 30.0), (1080, 1));
        assert_eq!(shape(20, 30.0), (1080, 2));
        assert_eq!(shape(20, 60.0), (720, 2));
        assert_eq!(shape(20, 120.0), (720, 2));
        assert_eq!(shape(20, 300.0), (360, 2));
        assert!(video_bitrate(20 * 970_000, 900.0, 96_000).is_err());
        assert_eq!(ladder(2_000_000, (1280, 720), 30), (720, 1));
        assert_eq!(ladder(1_000_000, (2560, 1440), 144), (540, 4));
    }

    #[test]
    fn kept_sound_follows_the_spans_and_pads_past_the_end_with_silence() {
        let second = RATE as usize * BLOCK;
        let pcm: Vec<u8> = (0..2 * second).map(|i| (i / second + 1) as u8).collect();
        let kept = heard(&pcm, &[(SECOND, SECOND / 2), (SECOND + SECOND / 2, SECOND)]);
        assert_eq!(kept.len(), second * 3 / 2);
        assert!(kept[..second].iter().all(|&b| b == 2));
        assert!(kept[second..].iter().all(|&b| b == 0));
    }

    #[test]
    fn peaks_take_the_loudest_sample_of_each_stretch_and_pad_a_short_track() {
        let pcm = |samples: &[i16]| samples.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>();
        let frames = pcm(&[0, 16384, -100, 50, i16::MIN, 0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(peaks(&frames, 3), [0.5, 1.0, 6.0 / 32768.0]);
        assert_eq!(peaks(&frames[..8], 3), [0.5, 100.0 / 32768.0, 0.0]);
    }

    #[test]
    fn a_mix_adds_the_tracks_and_clamps() {
        let pcm = |samples: &[i16]| samples.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<u8>>();
        assert_eq!(sum(&[pcm(&[100, 30000, -30000]), pcm(&[-50, 30000])]), pcm(&[50, i16::MAX, -30000]));
    }

    #[test]
    fn the_portrait_window_is_even_and_stays_on_the_picture() {
        assert_eq!(window((1920, 1080), 0.5), (656, 608));
        assert_eq!(window((1920, 1080), 0.0), (0, 608));
        assert_eq!(window((1920, 1080), 1.0), (1312, 608));
        assert_eq!(window((1920, 1080), 0.71), (1058, 608));
        assert_eq!(window((2560, 1440), 0.5), (874, 810));
        assert_eq!(window((600, 1080), 0.5), (0, 600));
    }
}

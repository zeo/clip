use std::path::Path;
use std::sync::Arc;

use windows::core::{Interface, Result, GUID, HSTRING};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_FORMAT_P010, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;

use crate::audio::{audio_type, Aac, BLOCK, CHANNELS, RATE};
use crate::encode::{Codec, Encoder, Format};
use crate::ring::{Packet, SECOND};
use crate::sync::each_sample;

/// Every audio track of a clip as 16 bit stereo pcm at `RATE` from the clip's start, with its name.
pub fn tracks(path: &Path) -> std::result::Result<Vec<(String, Vec<u8>)>, String> {
    let names = crate::save::probe(path).map(|probe| probe.tracks).unwrap_or_default();
    decode_tracks(path)
        .map(|tracks| {
            tracks
                .into_iter()
                .enumerate()
                .map(|(i, pcm)| {
                    let name = names.get(i).filter(|name| !name.is_empty()).cloned();
                    (name.unwrap_or_else(|| format!("track {}", i + 1)), pcm)
                })
                .collect()
        })
        .map_err(|e| format!("can't read the clip's audio: {}", e.message()))
}

fn decode_tracks(path: &Path) -> Result<Vec<Vec<u8>>> {
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        let streams: Vec<u32> = (0..)
            .map_while(|i| reader.GetCurrentMediaType(i).ok().map(|media| (i, media)))
            .filter(|(_, media)| media.GetGUID(&MF_MT_MAJOR_TYPE).is_ok_and(|major| major == MFMediaType_Audio))
            .map(|(i, _)| i)
            .collect();
        streams
            .iter()
            .map(|&stream| {
                reader.SetCurrentMediaType(stream, None, &audio_type(MFAudioFormat_PCM)?)?;
                let mut pcm = Vec::new();
                each_sample(&reader, stream, |time, data| {
                    let at = (time.max(0) * RATE as i64 / SECOND) as usize * BLOCK;
                    if pcm.len() < at {
                        pcm.resize(at, 0);
                    }
                    pcm.extend_from_slice(data);
                })?;
                Ok(pcm)
            })
            .collect()
    }
}

pub fn write_wav(path: &Path, pcm: &[u8]) -> std::io::Result<()> {
    let size = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm.len());
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
    wav.extend_from_slice(pcm);
    std::fs::write(path, wav)
}

/// Writes `start` to `end` seconds of `source` to `out`, re-encoded on the gpu so the cut lands on
/// the exact frames asked for. Only the audio `tracks` listed are kept, summed into one when `mix`.
pub fn trim(
    source: &Path,
    out: &Path,
    start: f64,
    end: f64,
    tracks: &[usize],
    mix: bool,
) -> std::result::Result<(), String> {
    let probe = crate::save::probe(source).map_err(|e| format!("can't read the clip: {e}"))?;
    let (video, format, first, length) =
        trim_video(source, (start * SECOND as f64) as i64, (end * SECOND as f64) as i64)
            .map_err(|e| format!("can't re-encode the video: {}", e.message()))?;
    if video.is_empty() {
        return Err("there are no frames between the start and the end".into());
    }
    let all = self::tracks(source)?;
    let chosen: Vec<&(String, Vec<u8>)> = tracks
        .iter()
        .map(|&i| all.get(i).ok_or(format!("the clip has no track {i}")))
        .collect::<std::result::Result<_, _>>()?;
    let span = |pcm: &[u8]| {
        let from = (first * RATE as i64 / SECOND) as usize * BLOCK;
        let to = from + (length * RATE as i64 / SECOND) as usize * BLOCK;
        pcm.get(from.min(pcm.len())..to.min(pcm.len())).unwrap_or_default().to_vec()
    };
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
        .map(|(_, pcm)| aac(pcm))
        .collect::<Result<Vec<_>>>()
        .map_err(|e| format!("can't encode the audio: {}", e.message()))?;
    let names: Vec<&str> = lanes.iter().map(|(name, _)| name.as_str()).collect();
    let mastering = probe.mastering.filter(|_| format.codec == Codec::Hdr10);
    crate::save::write_mp4(out, &video, format, &audio, &names, mastering)
        .map_err(|e| format!("can't write {}: {}", out.display(), e.message()))
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

fn aac(pcm: &[u8]) -> Result<Vec<Arc<Packet>>> {
    let mut encoder = Aac::new()?;
    let chunk = RATE as usize / 10 * BLOCK;
    let mut packets = Vec::new();
    for (n, part) in pcm.chunks(chunk).enumerate() {
        let pts = (n * chunk / BLOCK) as i64 * SECOND / RATE as i64;
        packets.extend(encoder.encode(pts, part)?);
    }
    packets.extend(encoder.finish()?);
    Ok(packets.into_iter().map(Arc::new).collect())
}

/// Decodes the frames from `start` to `end` on the gpu and encodes them again from time 0. Returns
/// the packets, their format, the source time of the first frame kept and how many were kept.
fn trim_video(source: &Path, start: i64, end: i64) -> Result<(Vec<Arc<Packet>>, Format, i64, i64)> {
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
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(source.as_os_str()), &attributes)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(stream, true)?;

        let native = reader.GetNativeMediaType(stream, 0)?;
        let codec = match native.GetGUID(&MF_MT_SUBTYPE)? {
            subtype if subtype == MFVideoFormat_HEVC => Codec::Hdr10,
            _ => Codec::H264,
        };
        let size = native.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let rate = native.GetUINT64(&MF_MT_FRAME_RATE)?;
        let fps = ((rate >> 32) as f64 / (rate & 0xffff_ffff).max(1) as f64).round().max(1.0) as u32;
        let bytes = std::fs::metadata(source).map_or(0, |meta| meta.len());
        let seconds = crate::save::probe(source).map_or(0.0, |probe| probe.seconds).max(1.0);
        let bitrate = native.GetUINT32(&MF_MT_AVG_BITRATE).unwrap_or((bytes as f64 * 8.0 / seconds) as u32);
        let format = Format { width: (size >> 32) as u32, height: size as u32, fps, bitrate, codec };

        let (subtype, texture_format) = match codec {
            Codec::H264 => (MFVideoFormat_NV12, DXGI_FORMAT_NV12),
            Codec::Hdr10 => (MFVideoFormat_P010, DXGI_FORMAT_P010),
        };
        let decoded = MFCreateMediaType()?;
        decoded.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        decoded.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        reader.SetCurrentMediaType(stream, None, &decoded)?;

        let choices = crate::encode::encoders(codec, Some(gpu.adapter))?;
        let mut encoder = choices
            .iter()
            .find_map(|choice| Encoder::open(choice, &gpu.device, format).ok())
            .ok_or(windows::core::Error::new(MF_E_TOPO_CODEC_NOT_FOUND, "no encoder opened"))?;

        let desc = D3D11_TEXTURE2D_DESC {
            Width: format.width,
            Height: format.height,
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
        let picture = D3D11_BOX { left: 0, top: 0, front: 0, right: format.width, bottom: format.height, back: 1 };

        reader.SetCurrentPosition(&GUID::zeroed(), &PROPVARIANT::from(start))?;
        let (mut packets, mut first, mut kept) = (Vec::new(), None, 0i64);
        loop {
            let (mut flags, mut time, mut sample) = (0, 0, None);
            reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 || time >= end {
                break;
            }
            let Some(sample) = sample.filter(|_| time >= start) else { continue };
            let buffer = sample
                .GetBufferByIndex(0)?
                .cast::<IMFDXGIBuffer>()
                .map_err(|_| windows::core::Error::new(MF_E_UNSUPPORTED_D3D_TYPE, "the gpu can't decode this clip"))?;
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
            packets.extend(encoder.encode(target, format.time(kept))?);
            first.get_or_insert(time);
            kept += 1;
        }
        packets.extend(encoder.finish()?);
        let length = format.time(kept);
        Ok((packets.into_iter().map(Arc::new).collect(), format, first.unwrap_or(start), length))
    }
}

use std::path::Path;
use std::sync::Arc;

use windows::core::{Result, HSTRING};
use windows::Win32::Media::MediaFoundation::*;

use crate::audio::Aac;
use crate::encode::{describe, parameter_sets, Format};
use crate::ring::Packet;

/// Writes already encoded packets to an MP4 without re-encoding: the video, then one AAC track per
/// audio lane cut to the video's span. The file appears under its final name only once it is complete.
pub fn write_mp4(path: &Path, packets: &[Arc<Packet>], format: Format, audio: &[Vec<Arc<Packet>>]) -> Result<()> {
    let first = packets
        .first()
        .filter(|p| p.key)
        .ok_or(windows::core::Error::new(MF_E_INVALIDREQUEST, "a clip has to start on a keyframe"))?;
    let partial = path.with_extension("mp4.part");
    let written = unsafe { write(&partial, packets, first, format, audio) };
    if let Err(e) = written {
        let _ = std::fs::remove_file(&partial);
        return Err(e);
    }
    std::fs::rename(&partial, path).map_err(|e| windows::core::Error::new(E_RENAME, e.to_string()))
}

const E_RENAME: windows::core::HRESULT = windows::core::HRESULT(0x8007_0050_u32 as i32);

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

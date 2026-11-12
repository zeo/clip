use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};

use windows::core::{Interface, Result, GUID};
use windows::Win32::Foundation::{E_FAIL, LUID};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::Variant::VARIANT;

use crate::ring::{Packet, SECOND};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    /// 8 bit bt.709, what every player and discord embed shows correctly
    H264,
    /// hevc main10 with the pq transfer and bt.2020 primaries
    Hdr10,
}

impl Codec {
    fn input(self) -> GUID {
        match self {
            Codec::H264 => MFVideoFormat_NV12,
            Codec::Hdr10 => MFVideoFormat_P010,
        }
    }

    pub fn subtype(self) -> GUID {
        match self {
            Codec::H264 => MFVideoFormat_H264,
            Codec::Hdr10 => MFVideoFormat_HEVC,
        }
    }

    fn profile(self) -> u32 {
        match self {
            Codec::H264 => eAVEncH264VProfile_High.0 as u32,
            Codec::Hdr10 => eAVEncH265VProfile_Main_420_10.0 as u32,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Format {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub codec: Codec,
}

impl Format {
    pub fn frame(&self) -> i64 {
        SECOND / self.fps as i64
    }

    /// Exact timestamp of frame `tick`, so long recordings don't drift from rounding `frame()`
    pub fn time(&self, tick: i64) -> i64 {
        tick * SECOND / self.fps as i64
    }
}

/// What the stream is for: capture wants a steady rate and a keyframe every second so a clip can
/// start anywhere, an export wants the best picture for its size.
#[derive(Clone, Copy, PartialEq)]
pub enum Purpose {
    Capture,
    Export,
}

pub struct Choice {
    activate: IMFActivate,
    pub name: String,
    pub hardware: bool,
}

/// Encoders for `codec` on this machine, hardware ones on `adapter` first, then software ones.
pub fn encoders(codec: Codec, adapter: Option<LUID>) -> Result<Vec<Choice>> {
    let mut found = Vec::new();
    for hardware in [true, false] {
        let flags = if hardware { MFT_ENUM_FLAG_HARDWARE } else { MFT_ENUM_FLAG_SYNCMFT };
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: codec.input() };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: codec.subtype() };
        let mut list = std::ptr::null_mut();
        let mut count = 0;
        unsafe {
            let mut filter = None;
            MFCreateAttributes(&mut filter, 1)?;
            let filter = filter.expect("created");
            if let (true, Some(luid)) = (hardware, adapter) {
                let bytes = std::slice::from_raw_parts(&luid as *const LUID as *const u8, size_of::<LUID>());
                filter.SetBlob(&MFT_ENUM_ADAPTER_LUID, bytes)?;
            }
            MFTEnum2(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags | MFT_ENUM_FLAG_SORTANDFILTER,
                Some(&input),
                Some(&output),
                &filter,
                &mut list,
                &mut count,
            )?;
            if list.is_null() {
                continue;
            }
            for slot in std::slice::from_raw_parts_mut(list, count as usize) {
                let Some(activate) = slot.take() else {
                    continue;
                };
                found.push(Choice { name: friendly_name(&activate), activate, hardware });
            }
            CoTaskMemFree(Some(list as _));
        }
    }
    Ok(found)
}

/// The error an async encoder reported, which ends its stream.
fn failed(event: &IMFMediaEvent) -> windows::core::Error {
    let code = unsafe { event.GetStatus() }.unwrap_or(E_FAIL);
    windows::core::Error::new(if code.is_ok() { E_FAIL } else { code }, "the encoder failed")
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut text = windows::core::PWSTR::null();
    let mut len = 0;
    unsafe {
        if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut len).is_err() {
            return "unknown encoder".into();
        }
        let name = text.to_string().unwrap_or_default();
        CoTaskMemFree(Some(text.0 as _));
        name
    }
}

pub struct Encoder {
    activate: IMFActivate,
    transform: IMFTransform,
    events: Option<IMFMediaEventGenerator>,
    credit: u32,
    provides_samples: bool,
    output_size: u32,
    gpu_input: bool,
    staging: Option<ID3D11Texture2D>,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    format: Format,
    /// for capture, the knob that forces a keyframe and the pts of the last one
    keys: Option<(ICodecAPI, Option<i64>)>,
}

// hardware encoders answer within a frame or two, this only trips on a wedged driver
const STALL: Duration = Duration::from_secs(2);

#[allow(non_upper_case_globals)] // the event names are the sdk's own constants
impl Encoder {
    pub fn open(choice: &Choice, device: &ID3D11Device, format: Format, purpose: Purpose) -> Result<Self> {
        let transform: IMFTransform = unsafe { choice.activate.ActivateObject()? };
        Self::start(choice, transform.clone(), device, format, purpose)
            .inspect_err(|_| close(&transform, &choice.activate))
    }

    fn start(
        choice: &Choice,
        transform: IMFTransform,
        device: &ID3D11Device,
        format: Format,
        purpose: Purpose,
    ) -> Result<Self> {
        unsafe {
            let attributes = transform.GetAttributes()?;
            let events = if choice.hardware {
                attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                Some(transform.cast::<IMFMediaEventGenerator>()?)
            } else {
                None
            };
            let gpu_input = attributes.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0) != 0;
            if gpu_input {
                let mut token = 0;
                let mut manager = None;
                MFCreateDXGIDeviceManager(&mut token, &mut manager)?;
                let manager = manager.ok_or(windows::core::Error::from(E_FAIL))?;
                manager.ResetDevice(device, token)?;
                transform.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, manager.as_raw() as usize)?;
            }
            let capture = purpose == Purpose::Capture;
            let _ = attributes.SetUINT32(&MF_LOW_LATENCY, capture as u32);

            // vendors support different subsets, so each knob is best effort
            let api = transform.cast::<ICodecAPI>().ok();
            if let Some(codec) = &api {
                let set = |api: GUID, value: VARIANT| {
                    let _ = codec.SetValue(&api, &value);
                };
                let mode = if capture {
                    eAVEncCommonRateControlMode_CBR
                } else {
                    // spends bits where the picture moves, the peak keeps a busy stretch from blowing the size
                    set(CODECAPI_AVEncCommonMaxBitRate, VARIANT::from(format.bitrate / 2 * 3));
                    set(CODECAPI_AVEncCommonQualityVsSpeed, VARIANT::from(100u32));
                    eAVEncCommonRateControlMode_PeakConstrainedVBR
                };
                set(CODECAPI_AVEncCommonRateControlMode, VARIANT::from(mode.0 as u32));
                set(CODECAPI_AVEncCommonMeanBitRate, VARIANT::from(format.bitrate));
                set(CODECAPI_AVEncMPVGOPSize, VARIANT::from(format.fps * if capture { 1 } else { 4 }));
                // b frames would need decode times the muxer doesn't write
                set(CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
                set(CODECAPI_AVLowLatencyMode, VARIANT::from(capture));
            }

            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &format.codec.subtype())?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, format.bitrate)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, format.codec.profile())?;
            describe(&output, format)?;
            transform.SetOutputType(0, &output, 0)?;

            let input = (0..)
                .map_while(|i| transform.GetInputAvailableType(0, i).ok())
                .find(|t| t.GetGUID(&MF_MT_SUBTYPE).is_ok_and(|s| s == format.codec.input()))
                .ok_or(windows::core::Error::from(MF_E_INVALIDMEDIATYPE))?;
            describe(&input, format)?;
            transform.SetInputType(0, &input, 0)?;

            let info = transform.GetOutputStreamInfo(0)?;
            let provides = (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            Ok(Self {
                activate: choice.activate.clone(),
                transform,
                events,
                credit: 0,
                provides_samples: info.dwFlags & provides != 0,
                output_size: info.cbSize.max(format.width * format.height * 3 / 2),
                gpu_input,
                staging: None,
                device: device.clone(),
                context: device.GetImmediateContext()?,
                format,
                keys: api.filter(|_| capture).map(|api| (api, None)),
            })
        }
    }

    pub fn encode(&mut self, texture: &ID3D11Texture2D, pts: i64) -> Result<Vec<Packet>> {
        let sample = self.sample(texture)?;
        unsafe {
            sample.SetSampleTime(pts)?;
            sample.SetSampleDuration(self.format.frame())?;
        }
        // a clip can only start on a keyframe, so one that's two seconds late is asked for
        if let Some((api, keyed)) = &mut self.keys {
            if pts - *keyed.get_or_insert(pts) >= 2 * SECOND {
                let _ = unsafe { api.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
                *keyed = Some(pts);
            }
        }
        let mut out = Vec::new();
        if self.events.is_some() {
            let started = Instant::now();
            while self.credit == 0 {
                if !self.pump(&mut out)? {
                    if started.elapsed() > STALL {
                        return Err(windows::core::Error::new(
                            MF_E_HW_MFT_FAILED_START_STREAMING,
                            "encoder stopped asking for frames",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
            unsafe { self.transform.ProcessInput(0, &sample, 0)? };
            self.credit -= 1;
            while self.pump(&mut out)? {}
        } else {
            unsafe { self.transform.ProcessInput(0, &sample, 0)? };
            while let Some(packet) = self.pull()? {
                out.push(packet);
            }
        }
        if let (Some((_, Some(keyed))), Some(key)) = (&mut self.keys, out.iter().rfind(|p| p.key)) {
            *keyed = (*keyed).max(key.pts);
        }
        Ok(out)
    }

    pub fn finish(&mut self) -> Result<Vec<Packet>> {
        let mut out = Vec::new();
        unsafe {
            self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)?;
            self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        let Some(events) = self.events.clone() else {
            while let Some(packet) = self.pull()? {
                out.push(packet);
            }
            return Ok(out);
        };
        let started = Instant::now();
        while started.elapsed() < STALL {
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => match MF_EVENT_TYPE(unsafe { event.GetType()? } as i32) {
                    METransformHaveOutput => out.extend(self.pull()?),
                    METransformDrainComplete => break,
                    MEError => return Err(failed(&event)),
                    _ => {}
                },
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => std::thread::sleep(Duration::from_millis(1)),
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Handles one pending async event. False when none was waiting.
    fn pump(&mut self, out: &mut Vec<Packet>) -> Result<bool> {
        let events = self.events.as_ref().expect("async encoder");
        let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
            Ok(event) => event,
            Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(false),
            Err(e) => return Err(e),
        };
        match MF_EVENT_TYPE(unsafe { event.GetType()? } as i32) {
            METransformNeedInput => self.credit += 1,
            METransformHaveOutput => out.extend(self.pull()?),
            MEError => return Err(failed(&event)),
            _ => {}
        }
        Ok(true)
    }

    fn pull(&mut self) -> Result<Option<Packet>> {
        unsafe {
            let provided = if self.provides_samples {
                None
            } else {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                Some(sample)
            };
            let mut buffer = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(provided),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let result = self.transform.ProcessOutput(0, &mut buffer, &mut status);
            let sample = ManuallyDrop::take(&mut buffer[0].pSample);
            drop(ManuallyDrop::take(&mut buffer[0].pEvents));
            match result {
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let renegotiated = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &renegotiated, 0)?;
                    self.pull()
                }
                Err(e) => Err(e),
                Ok(()) => sample.map(|s| packet(&s, self.format.frame())).transpose(),
            }
        }
    }

    fn sample(&mut self, texture: &ID3D11Texture2D) -> Result<IMFSample> {
        unsafe {
            let buffer = if self.gpu_input {
                let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
                let length = buffer.cast::<IMF2DBuffer>()?.GetContiguousLength()?;
                buffer.SetCurrentLength(length)?;
                buffer
            } else {
                self.read_back(texture)?
            };
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            Ok(sample)
        }
    }

    /// Copies an NV12 or P010 texture into a packed system memory buffer for software encoders.
    fn read_back(&mut self, texture: &ID3D11Texture2D) -> Result<IMFMediaBuffer> {
        unsafe {
            let staging = match &self.staging {
                Some(staging) => staging.clone(),
                None => {
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    texture.GetDesc(&mut desc);
                    desc.Usage = D3D11_USAGE_STAGING;
                    desc.BindFlags = 0;
                    desc.MiscFlags = 0;
                    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                    let mut staging = None;
                    self.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
                    let staging = staging.ok_or(windows::core::Error::from(E_FAIL))?;
                    self.staging = Some(staging.clone());
                    staging
                }
            };
            self.context.CopyResource(&staging, texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let sample_bytes = if self.format.codec == Codec::Hdr10 { 2 } else { 1 };
            let (width, height) = (self.format.width as usize * sample_bytes, self.format.height as usize);
            let size = width * height * 3 / 2;
            let buffer = MFCreateMemoryBuffer(size as u32)?;
            let mut target = std::ptr::null_mut();
            buffer.Lock(&mut target, None, None)?;
            let source = mapped.pData as *const u8;
            let pitch = mapped.RowPitch as usize;
            // a full height luma plane, then a half height interleaved chroma plane
            for row in 0..height + height / 2 {
                std::ptr::copy_nonoverlapping(source.add(row * pitch), target.add(row * width), width);
            }
            buffer.Unlock()?;
            self.context.Unmap(&staging, 0);
            buffer.SetCurrentLength(size as u32)?;
            Ok(buffer)
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        close(&self.transform, &self.activate);
        // nvidia's async mfts can still be running a queued work item that takes the encoder's lock after
        // shutdown, and freeing the encoder under it crashes clip, so the last reference goes a little later
        let kept = Kept { _transform: self.transform.clone(), _activate: self.activate.clone() };
        let _ = std::thread::Builder::new().name("encoder release".into()).spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            drop(kept);
        });
    }
}

struct Kept {
    _transform: IMFTransform,
    _activate: IMFActivate,
}

// a media foundation transform and its activate can be released from any thread
unsafe impl Send for Kept {}

/// Lets go of an encoder's worker threads, event queue and gpu session, which a hardware one keeps
/// until it's shut down, whatever references are left.
fn close(transform: &IMFTransform, activate: &IMFActivate) {
    unsafe {
        let _ = transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        if let Ok(shutdown) = transform.cast::<IMFShutdown>() {
            let _ = shutdown.Shutdown();
        }
        let _ = activate.ShutdownObject();
    }
}

/// Size, rate and color signalling shared by the encoder types and the saved file.
pub fn describe(media: &IMFMediaType, format: Format) -> Result<()> {
    let (primaries, transfer, matrix) = match format.codec {
        Codec::H264 => (MFVideoPrimaries_BT709.0, MFVideoTransFunc_709.0, MFVideoTransferMatrix_BT709.0),
        Codec::Hdr10 => (MFVideoPrimaries_BT2020.0, MFVideoTransFunc_2084.0, MFVideoTransferMatrix_BT2020_10.0),
    };
    unsafe {
        media.SetUINT64(&MF_MT_FRAME_SIZE, ((format.width as u64) << 32) | format.height as u64)?;
        media.SetUINT64(&MF_MT_FRAME_RATE, ((format.fps as u64) << 32) | 1)?;
        media.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
        media.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        media.SetUINT32(&MF_MT_VIDEO_PRIMARIES, primaries as u32)?;
        media.SetUINT32(&MF_MT_TRANSFER_FUNCTION, transfer as u32)?;
        media.SetUINT32(&MF_MT_YUV_MATRIX, matrix as u32)?;
        media.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    }
    Ok(())
}

pub fn packet(sample: &IMFSample, frame: i64) -> Result<Packet> {
    unsafe {
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut data = std::ptr::null_mut();
        let mut length = 0;
        buffer.Lock(&mut data, None, Some(&mut length))?;
        let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
        buffer.Unlock()?;
        Ok(Packet {
            pts: sample.GetSampleTime()?,
            duration: sample.GetSampleDuration().unwrap_or(frame),
            key: sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0,
            data: bytes,
        })
    }
}

/// Parameter set NAL units (h.264 sps and pps, hevc vps sps and pps), start codes included,
/// from an Annex B access unit.
pub fn parameter_sets(codec: Codec, access_unit: &[u8]) -> Vec<u8> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= access_unit.len() {
        if access_unit[i..i + 3] == [0, 0, 1] {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(access_unit.len(), |&next| next - 3);
        while n + 1 < starts.len() && end > start && access_unit[end - 1] == 0 {
            end -= 1;
        }
        let header = access_unit.get(start).copied().unwrap_or(0);
        let wanted = match codec {
            Codec::H264 => matches!(header & 0x1f, 7 | 8),
            Codec::Hdr10 => matches!((header >> 1) & 0x3f, 32..=34),
        };
        if start < end && wanted {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(&access_unit[start..end]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{parameter_sets, Codec};

    #[test]
    fn keeps_only_sps_and_pps() {
        let unit = [
            0, 0, 0, 1, 0x09, 0xf0, // access unit delimiter
            0, 0, 0, 1, 0x67, 1, 2, 3, // sps
            0, 0, 1, 0x68, 4, 5, // pps, three byte start code
            0, 0, 0, 1, 0x65, 9, 9, 9, // idr slice
        ];
        assert_eq!(parameter_sets(Codec::H264, &unit), [0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 0, 1, 0x68, 4, 5]);
    }

    #[test]
    fn keeps_hevc_vps_sps_and_pps() {
        let unit = [
            0, 0, 0, 1, 0x46, 0x01, 0x50, // access unit delimiter
            0, 0, 0, 1, 0x40, 0x01, 7, // vps
            0, 0, 0, 1, 0x42, 0x01, 8, // sps
            0, 0, 0, 1, 0x44, 0x01, 9, // pps
            0, 0, 0, 1, 0x26, 0x01, 5, // idr slice
        ];
        let expected = [0, 0, 0, 1, 0x40, 0x01, 7, 0, 0, 0, 1, 0x42, 0x01, 8, 0, 0, 0, 1, 0x44, 0x01, 9];
        assert_eq!(parameter_sets(Codec::Hdr10, &unit), expected);
    }

    #[test]
    fn empty_without_parameter_sets() {
        assert!(parameter_sets(Codec::H264, &[0, 0, 1, 0x41, 7, 7]).is_empty());
    }

    // nvidia's hevc encoder used to crash the process a moment after a drop, once every few hundred drops
    #[test]
    #[ignore]
    fn a_hardware_encoder_survives_being_opened_and_dropped_over_and_over() {
        use super::{encoders, Encoder, Format, MFStartup, Purpose, MFSTARTUP_FULL, MF_VERSION};
        use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.unwrap();
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.unwrap();
        let gpu = crate::capture::gpu().unwrap();
        let format = Format { width: 1280, height: 720, fps: 30, bitrate: 4_000_000, codec: Codec::Hdr10 };
        let choice = encoders(Codec::Hdr10, Some(gpu.adapter)).unwrap().into_iter().find(|c| c.hardware).unwrap();
        let mut pipeline =
            crate::capture::Pipeline::new(&gpu, crate::TEST_DISPLAY, format, crate::hdr::Filter::Area).unwrap();
        let source = crate::pattern(&gpu.device, 800, 600, 0, true).unwrap();
        for round in 0..300 {
            let mut encoder = Encoder::open(&choice, &gpu.device, format, Purpose::Capture)
                .unwrap_or_else(|e| panic!("open in round {round}: {e}"));
            for i in 0..10 {
                let frame = pipeline.convert(&source, (800, 600), format.fps).unwrap();
                encoder.encode(&frame, format.time(i)).unwrap();
            }
            encoder.finish().unwrap();
        }
        // the last encoders are let go a second after they're dropped, and a crash then still has to fail the test
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

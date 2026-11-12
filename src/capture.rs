use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Interface, Result};
use windows::Foundation::TimeSpan;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{E_FAIL, HMODULE, HWND, LUID, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTONEAREST};
use windows::Win32::System::WinRT::Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::encode::{Codec, Encoder, Format};
use crate::hdr::{self, Display, Filter, Pq, Scaler, Tonemap};
use crate::ring::{Clock, Keep, Ring, SECOND};

#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: LUID,
}

/// Relative to normal (0), down to idle (-7).
pub const GPU_PRIORITY: i32 = -3;

pub fn gpu() -> Result<Gpu> {
    unsafe {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.ok_or(windows::core::Error::from(E_FAIL))?;
        let context = context.ok_or(windows::core::Error::from(E_FAIL))?;
        // media foundation drives the encoder from its own threads on this device
        let _ = device.cast::<ID3D11Multithread>()?.SetMultithreadProtected(true);
        let dxgi = device.cast::<IDXGIDevice>()?;
        // when the gpu is saturated the game's frames go first and ours fit in around them
        let _ = dxgi.SetGPUThreadPriority(GPU_PRIORITY);
        let adapter = dxgi.GetAdapter()?.GetDesc()?.AdapterLuid;
        Ok(Gpu { device, context, adapter })
    }
}

#[derive(Clone, Copy)]
pub enum Target {
    Monitor(HMONITOR),
    Window(HWND),
}

// window and monitor handles are plain ids that any thread may use
unsafe impl Send for Target {}

impl Target {
    pub fn monitor(self) -> HMONITOR {
        match self {
            Target::Monitor(monitor) => monitor,
            Target::Window(window) => unsafe { MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST) },
        }
    }
}

pub struct Source {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    device: IDirect3DDevice,
    size: SizeInt32,
    pixels: DirectXPixelFormat,
    target: Target,
}

/// scRGB FP16 on an hdr display, otherwise windows hands over 8 bit sRGB already clipped
fn pixels(hdr: bool) -> DirectXPixelFormat {
    if hdr {
        DirectXPixelFormat::R16G16B16A16Float
    } else {
        DirectXPixelFormat::B8G8R8A8UIntNormalized
    }
}

impl Source {
    pub fn start(gpu: &Gpu, target: Target, fps: u32, hdr: bool) -> Result<Self> {
        let pixels = pixels(hdr);
        unsafe {
            let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
            let item: GraphicsCaptureItem = match target {
                Target::Monitor(monitor) => interop.CreateForMonitor(monitor)?,
                Target::Window(window) => interop.CreateForWindow(window)?,
            };
            let device: IDirect3DDevice =
                CreateDirect3D11DeviceFromDXGIDevice(&gpu.device.cast::<IDXGIDevice>()?)?.cast()?;
            let size = item.Size()?;
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(&device, pixels, 2, size)?;
            let session = pool.CreateCaptureSession(&item)?;
            let _ = session.SetIsBorderRequired(false);
            let _ = session.SetIsCursorCaptureEnabled(true);
            // windows 11 24h2 and later stop compositing frames faster than we encode them
            let _ = session.SetMinUpdateInterval(TimeSpan { Duration: SECOND / fps as i64 });
            session.StartCapture()?;
            Ok(Self { pool, session, device, size, pixels, target })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.size.Width as u32, self.size.Height as u32)
    }

    /// The oldest queued frame, and the pool follows the content size when it changes.
    pub fn next(&mut self) -> Option<Direct3D11CaptureFrame> {
        let frame = self.pool.TryGetNextFrame().ok()?;
        if let Ok(content) = frame.ContentSize() {
            if content != self.size
                && content.Width > 0
                && content.Height > 0
                && self.pool.Recreate(&self.device, self.pixels, 2, content).is_ok()
            {
                self.size = content;
            }
        }
        Some(frame)
    }

    /// Switches the frame format when the display goes into or out of hdr. Frames already queued
    /// keep the old format, and a failed switch keeps it too, so the pipeline takes either.
    pub fn follow(&mut self, hdr: bool) {
        let pixels = pixels(hdr);
        if pixels != self.pixels && self.pool.Recreate(&self.device, pixels, 2, self.size).is_ok() {
            self.pixels = pixels;
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

pub fn texture(frame: &Direct3D11CaptureFrame) -> Result<ID3D11Texture2D> {
    unsafe { frame.Surface()?.cast::<IDirect3DDxgiInterfaceAccess>()?.GetInterface() }
}

/// Scales frames and converts them into `format` from one color space to another with the GPU's
/// video processor. Output textures rotate so the encoder can still be reading one while the next
/// is written.
pub struct Converter {
    device: ID3D11Device,
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: (u32, u32),
    output: (u32, u32),
    format: DXGI_FORMAT,
    spaces: (DXGI_COLOR_SPACE_TYPE, DXGI_COLOR_SPACE_TYPE),
    targets: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

const IN_FLIGHT: usize = 6;

impl Converter {
    pub fn new(
        gpu: &Gpu,
        input: (u32, u32),
        output: (u32, u32),
        fps: u32,
        format: DXGI_FORMAT,
        spaces: (DXGI_COLOR_SPACE_TYPE, DXGI_COLOR_SPACE_TYPE),
    ) -> Result<Self> {
        unsafe {
            let video: ID3D11VideoDevice = gpu.device.cast()?;
            let context: ID3D11VideoContext1 = gpu.context.cast()?;
            let rate = DXGI_RATIONAL { Numerator: fps, Denominator: 1 };
            let enumerator = video.CreateVideoProcessorEnumerator(&D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: rate,
                InputWidth: input.0,
                InputHeight: input.1,
                OutputFrameRate: rate,
                OutputWidth: output.0,
                OutputHeight: output.1,
                Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
            })?;
            let processor = video.CreateVideoProcessor(&enumerator, 0)?;
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            context.VideoProcessorSetStreamColorSpace1(&processor, 0, spaces.0);
            context.VideoProcessorSetOutputColorSpace1(&processor, spaces.1);
            let full = RECT { left: 0, top: 0, right: output.0 as i32, bottom: output.1 as i32 };
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full));
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&full));

            let desc = D3D11_TEXTURE2D_DESC {
                Width: output.0,
                Height: output.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut targets = Vec::with_capacity(IN_FLIGHT);
            for _ in 0..IN_FLIGHT {
                let mut texture = None;
                gpu.device.CreateTexture2D(&desc, None, Some(&mut texture))?;
                let texture = texture.ok_or(windows::core::Error::from(E_FAIL))?;
                let mut view = None;
                video.CreateVideoProcessorOutputView(&texture, &enumerator, &view_desc, Some(&mut view))?;
                targets.push((texture, view.ok_or(windows::core::Error::from(E_FAIL))?));
            }
            Ok(Self {
                device: gpu.device.clone(),
                video,
                context,
                enumerator,
                processor,
                input,
                output,
                format,
                spaces,
                targets,
                next: 0,
            })
        }
    }

    /// Scales the `content` region of `source` over the whole of the next target and returns it.
    pub fn convert(&mut self, source: &ID3D11Texture2D, content: (u32, u32), fps: u32) -> Result<ID3D11Texture2D> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            source.GetDesc(&mut desc);
            if (desc.Width, desc.Height) != self.input {
                let context = self.device.GetImmediateContext()?;
                let gpu = Gpu { device: self.device.clone(), context, adapter: LUID::default() };
                *self = Self::new(&gpu, (desc.Width, desc.Height), self.output, fps, self.format, self.spaces)?;
            }
            let content = (content.0.min(desc.Width).max(1), content.1.min(desc.Height).max(1));
            let mut view = None;
            self.video.CreateVideoProcessorInputView(
                source,
                &self.enumerator,
                &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                    },
                },
                Some(&mut view),
            )?;
            let source_rect = RECT { left: 0, top: 0, right: content.0 as i32, bottom: content.1 as i32 };
            self.context.VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&source_rect));
            let (texture, target) = &self.targets[self.next];
            self.next = (self.next + 1) % self.targets.len();
            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(view),
                ..Default::default()
            };
            let mut streams = [stream];
            let result = self.context.VideoProcessorBlt(&self.processor, target, 0, &streams);
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            result?;
            Ok(texture.clone())
        }
    }
}

/// Everything between a captured frame and the encoder: our own resampling to the encode size, then
/// for h.264 FP16 frames are tonemapped and the video processor turns BGRA into NV12 one to one, and
/// for hdr10 both formats go to P010, sdr frames with their white at the display's sdr white level.
/// The display can switch hdr on or off mid recording, so each frame is converted by its own format.
pub struct Pipeline {
    scaler: Scaler,
    output: (u32, u32),
    encode: Encode,
    pub display: Display,
}

enum Encode {
    Sdr(Tonemap, Converter),
    /// nits for sdr white, the last level an hdr display reported
    Hdr10(Pq, f32),
}

/// sdr reference white from ITU-R BT.2408, for when no hdr display has said otherwise
const REFERENCE_WHITE: f32 = 203.0;

impl Pipeline {
    pub fn new(gpu: &Gpu, display: Display, format: Format, filter: Filter) -> Result<Self> {
        let output = (format.width, format.height);
        let (encode, middle) = match format.codec {
            Codec::Hdr10 => {
                let white = if display.hdr { display.sdr_white } else { REFERENCE_WHITE };
                (Encode::Hdr10(Pq::new(&gpu.device, output, IN_FLIGHT)?, white), DXGI_FORMAT_R16G16B16A16_FLOAT)
            }
            Codec::H264 => (
                Encode::Sdr(
                    Tonemap::new(&gpu.device, display)?,
                    Converter::new(
                        gpu,
                        output,
                        output,
                        format.fps,
                        DXGI_FORMAT_NV12,
                        (DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709),
                    )?,
                ),
                DXGI_FORMAT_B8G8R8A8_UNORM,
            ),
        };
        Ok(Self { scaler: Scaler::new(&gpu.device, filter, output, middle)?, output, encode, display })
    }

    /// Takes on a new display state: the tonemap follows it, and hdr10 keeps the last hdr sdr white.
    pub fn follow(&mut self, display: Display) {
        match &mut self.encode {
            Encode::Sdr(tonemap, _) => tonemap.follow(display),
            Encode::Hdr10(_, white) if display.hdr => *white = display.sdr_white,
            Encode::Hdr10(..) => {}
        }
        self.display = display;
    }

    /// Letterboxes the `content` region of `frame` into the next encoder surface and returns it.
    pub fn convert(&mut self, frame: &ID3D11Texture2D, content: (u32, u32), fps: u32) -> Result<ID3D11Texture2D> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { frame.GetDesc(&mut desc) };
        let hdr = desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
        let output = self.output;
        let dest = letterbox(content, output);
        match &mut self.encode {
            Encode::Hdr10(pq, white) => {
                let scaled = self.scaler.apply(frame, content, dest)?;
                let full = RECT { left: 0, top: 0, right: output.0 as i32, bottom: output.1 as i32 };
                pq.convert(&scaled, output, full, (!hdr).then_some(*white))
            }
            Encode::Sdr(tonemap, converter) => {
                let frame = if hdr { tonemap.apply(frame)? } else { frame.clone() };
                converter.convert(&self.scaler.apply(&frame, content, dest)?, output, fps)
            }
        }
    }
}

/// The largest centered rect with the content's aspect that fits the output, on even pixels for chroma.
pub fn letterbox(content: (u32, u32), output: (u32, u32)) -> RECT {
    let scale = f64::min(output.0 as f64 / content.0 as f64, output.1 as f64 / content.1 as f64);
    let width = ((content.0 as f64 * scale) as i32 & !1).min(output.0 as i32);
    let height = ((content.1 as f64 * scale) as i32 & !1).min(output.1 as i32);
    let left = ((output.0 as i32 - width) / 2) & !1;
    let top = ((output.1 as i32 - height) / 2) & !1;
    RECT { left, top, right: left + width, bottom: top + height }
}

/// Encode size for a capture: the source scaled down to at most `max_height`, kept even.
pub fn output_size(source: (u32, u32), max_height: u32) -> (u32, u32) {
    let height = source.1.min(max_height).max(2);
    let width = (source.0 as u64 * height as u64 / source.1.max(1) as u64) as u32;
    (width.max(2) & !1, height & !1)
}

/// A frame as it was captured, rows packed tight: 8 bit BGRA, or on an hdr display FP16 scRGB.
#[derive(Clone)]
pub struct Still {
    pub width: u32,
    pub height: u32,
    hdr: bool,
    /// nits sdr white sat at, for bringing scRGB down to 8 bit
    white: f32,
    data: Vec<u8>,
}

/// Whoever waits for the next captured frame.
pub type Shots = Arc<Mutex<Vec<mpsc::Sender<std::result::Result<Still, String>>>>>;

impl Still {
    /// Opaque 8 bit BGRA. Hdr frames keep sdr white as white, and anything brighter clips.
    pub fn bgra(&self) -> Vec<u8> {
        if !self.hdr {
            let mut out = self.data.clone();
            out.iter_mut().skip(3).step_by(4).for_each(|alpha| *alpha = 255);
            return out;
        }
        // scRGB 1.0 is 80 nits
        let scale = 80.0 / self.white.max(1.0);
        let encode: Vec<u8> = (0..=4096).map(|i| (srgb(i as f32 / 4096.0) * 255.0 + 0.5) as u8).collect();
        let level = |bytes: &[u8]| {
            encode[((half(u16::from_le_bytes([bytes[0], bytes[1]])) * scale).clamp(0.0, 1.0) * 4096.0) as usize]
        };
        self.data
            .as_chunks::<8>()
            .0
            .iter()
            .flat_map(|px| [level(&px[4..6]), level(&px[2..4]), level(&px[0..2]), 255])
            .collect()
    }
}

fn srgb(linear: f32) -> f32 {
    if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    }
}

fn half(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let (exponent, fraction) = ((bits >> 10) & 0x1f, (bits & 0x3ff) as f32 / 1024.0);
    sign * match exponent {
        0 => fraction * 2f32.powi(-14),
        31 => f32::INFINITY,
        e => (1.0 + fraction) * 2f32.powi(e as i32 - 15),
    }
}

/// Copies the `content` corner of a captured frame back to the cpu.
fn still(frame: &ID3D11Texture2D, content: (u32, u32), white: f32) -> Result<Still> {
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        frame.GetDesc(&mut desc);
        let (width, height) = (content.0.min(desc.Width), content.1.min(desc.Height));
        let device = frame.GetDevice()?;
        let context = device.GetImmediateContext()?;
        let staging = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut copy = None;
        device.CreateTexture2D(&staging, None, Some(&mut copy))?;
        let copy = copy.ok_or(windows::core::Error::from(E_FAIL))?;
        let region = D3D11_BOX { left: 0, top: 0, front: 0, right: width, bottom: height, back: 1 };
        context.CopySubresourceRegion(&copy, 0, 0, 0, 0, frame, 0, Some(&region));
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&copy, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let hdr = desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT;
        let row = width as usize * if hdr { 8 } else { 4 };
        let mut data = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let at = mapped.pData.cast::<u8>().add(y * mapped.RowPitch as usize);
            data.extend_from_slice(std::slice::from_raw_parts(at, row));
        }
        context.Unmap(&copy, 0);
        Ok(Still { width, height, hdr, white, data })
    }
}

/// Captures `target` into `ring` at a constant frame rate until `stop` is set, handing a copy of
/// the next frame to whoever waits in `shots`.
#[allow(clippy::too_many_arguments)] // the capture thread owns the first four and `keep`, the rest are shared with the session
/// Each frame shows from the slot nearest the moment the compositor drew it, and when the source
/// sends nothing new the last frame repeats, so every clip is constant frame rate.
pub fn record(
    mut source: Source,
    mut pipeline: Pipeline,
    mut encoder: Encoder,
    format: Format,
    clock: Clock,
    ring: &Arc<Mutex<Ring>>,
    mut keep: Keep,
    stop: &AtomicBool,
    shots: &Shots,
) -> Result<()> {
    let period = Duration::from_nanos(1_000_000_000 / format.fps as u64);
    let started = clock.started;
    let slot = |frame: &Direct3D11CaptureFrame| -> Result<i64> {
        let drawn = clock.at(frame.SystemRelativeTime()?.Duration);
        Ok((drawn * format.fps as i64 + SECOND / 2).div_euclid(SECOND))
    };
    // setup took a moment, so the first frame lands where the clock already is
    let mut tick = (started.elapsed().as_nanos() / period.as_nanos()) as u64;
    let mut held = None;
    let mut last = None;
    let mut stalled: Option<Instant> = None;
    while !stop.load(Ordering::Relaxed) {
        // wake half a slot late so every frame drawn nearer this slot than the next has arrived
        let due = started + period * tick as u32 + period / 2;
        match due.checked_duration_since(Instant::now()) {
            Some(wait) => std::thread::sleep(wait),
            None if Instant::now() > due + period * 2 => stalled = Some(Instant::now()),
            None => {}
        }
        // once a second, catch hdr being switched, the sdr brightness slider, or a window moving screens
        // switching hdr stalls capture and the frames change a moment later, so for a second after a stall
        // watch every tick
        let after_stall = stalled.is_some_and(|at| at.elapsed() < Duration::from_secs(1));
        if after_stall || tick.is_multiple_of(format.fps as u64) {
            if let Ok(now) = hdr::display(source.target.monitor()) {
                if now != pipeline.display {
                    source.follow(now.hdr);
                    pipeline.follow(now);
                }
            }
        }
        let mut newest = None;
        while let Some(frame) = held.take().or_else(|| source.next()) {
            // frames left in the old format were composed around an hdr switch, either side of it
            if frame.Surface()?.Description()?.Format != source.pixels {
                let _ = frame.Close();
                continue;
            }
            if slot(&frame)? > tick as i64 {
                held = Some(frame);
                break;
            }
            if let Some(older) = newest.replace(frame) {
                let _ = older.Close();
            }
        }
        if let Some(frame) = newest {
            let content = frame.ContentSize()?;
            let content = (content.Width as u32, content.Height as u32);
            let waiting = std::mem::take(&mut *shots.lock().expect("shots poisoned"));
            if !waiting.is_empty() {
                let white = if pipeline.display.hdr { pipeline.display.sdr_white } else { 80.0 };
                let taken = still(&texture(&frame)?, content, white).map_err(|e| e.to_string());
                for waiter in waiting {
                    let _ = waiter.send(taken.clone());
                }
            }
            last = Some(pipeline.convert(&texture(&frame)?, content, format.fps)?);
            let _ = frame.Close();
        }
        if let Some(texture) = &last {
            keep.fill(ring, encoder.encode(texture, format.time(tick as i64))?);
        }
        // after a stall, repeat the last frame through the missed ticks, but never fall more than a second behind
        let reached = (started.elapsed().as_nanos() / period.as_nanos()) as u64;
        tick = (tick + 1).max(reached.saturating_sub(format.fps as u64));
    }
    keep.fill(ring, encoder.finish()?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stills_come_out_opaque_bgra() {
        let sdr = Still { width: 1, height: 1, hdr: false, white: 80.0, data: vec![10, 20, 30, 0] };
        assert_eq!(sdr.bgra(), [10, 20, 30, 255]);
        // scRGB red at sdr white, green at half of it, blue past it, as r, g, b, a halves
        let halves = [0x3c00u16, 0x3800, 0x4400, 0x3c00];
        let data = halves.iter().flat_map(|h| h.to_le_bytes()).collect();
        let hdr = Still { width: 1, height: 1, hdr: true, white: 80.0, data };
        let [b, g, r, a] = hdr.bgra()[..] else { panic!("one pixel") };
        assert_eq!((b, r, a), (255, 255, 255));
        assert!(g.abs_diff(188) <= 1, "half of white is sRGB 188, got {g}");
        assert_eq!(half(0xc000), -2.0);
        assert_eq!(half(0x0001), 2f32.powi(-24));
    }

    #[test]
    fn letterbox_centers_a_narrow_window() {
        let rect = letterbox((800, 600), (1920, 1080));
        assert_eq!((rect.left, rect.top, rect.right, rect.bottom), (240, 0, 1680, 1080));
    }

    #[test]
    fn letterbox_fills_a_matching_aspect() {
        let rect = letterbox((2560, 1440), (1920, 1080));
        assert_eq!((rect.left, rect.top, rect.right, rect.bottom), (0, 0, 1920, 1080));
    }

    #[test]
    fn output_size_scales_down_and_stays_even() {
        assert_eq!(output_size((3440, 1440), 1080), (2580, 1080));
        assert_eq!(output_size((1280, 721), 1080), (1280, 720));
        assert_eq!(output_size((1919, 1079), 1080), (1918, 1078));
    }
}

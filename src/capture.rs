use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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

use crate::encode::{Choice, Codec, Encoder, Format};
use crate::hdr::{Display, Pq, Tonemap};
use crate::ring::Ring;

#[derive(Clone)]
pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub adapter: LUID,
}

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
        let adapter = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetDesc()?.AdapterLuid;
        Ok(Gpu { device, context, adapter })
    }
}

#[derive(Clone, Copy)]
pub enum Target {
    Monitor(HMONITOR),
    Window(HWND),
}

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
}

impl Source {
    /// `hdr` asks for scRGB FP16 frames, otherwise windows hands over 8 bit sRGB already clipped.
    pub fn start(gpu: &Gpu, target: Target, fps: u32, hdr: bool) -> Result<Self> {
        let pixels =
            if hdr { DirectXPixelFormat::R16G16B16A16Float } else { DirectXPixelFormat::B8G8R8A8UIntNormalized };
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
            let _ = session.SetMinUpdateInterval(TimeSpan { Duration: crate::ring::SECOND / fps as i64 });
            session.StartCapture()?;
            Ok(Self { pool, session, device, size, pixels })
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.size.Width as u32, self.size.Height as u32)
    }

    /// The newest frame since the last call, older queued frames go straight back to the pool.
    pub fn next(&mut self) -> Option<Direct3D11CaptureFrame> {
        let mut latest: Option<Direct3D11CaptureFrame> = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            if let Some(older) = latest.replace(frame) {
                let _ = older.Close();
            }
        }
        if let Some(content) = latest.as_ref().and_then(|f| f.ContentSize().ok()) {
            if content != self.size
                && content.Width > 0
                && content.Height > 0
                && self.pool.Recreate(&self.device, self.pixels, 2, content).is_ok()
            {
                self.size = content;
            }
        }
        latest
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

/// Scales and converts BGRA frames to NV12 with the GPU's video processor.
/// Output textures rotate so the encoder can still be reading one while the next is written.
pub struct Converter {
    device: ID3D11Device,
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    input: (u32, u32),
    output: (u32, u32),
    targets: Vec<(ID3D11Texture2D, ID3D11VideoProcessorOutputView)>,
    next: usize,
}

const IN_FLIGHT: usize = 6;

impl Converter {
    pub fn new(gpu: &Gpu, input: (u32, u32), output: (u32, u32), fps: u32) -> Result<Self> {
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
            context.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            context.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709);
            let full = RECT { left: 0, top: 0, right: output.0 as i32, bottom: output.1 as i32 };
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&full));

            let desc = D3D11_TEXTURE2D_DESC {
                Width: output.0,
                Height: output.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
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
                targets,
                next: 0,
            })
        }
    }

    /// Letterboxes the `content` region of `source` into the next NV12 target and returns it.
    pub fn convert(&mut self, source: &ID3D11Texture2D, content: (u32, u32), fps: u32) -> Result<ID3D11Texture2D> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            source.GetDesc(&mut desc);
            if (desc.Width, desc.Height) != self.input {
                let context = self.device.GetImmediateContext()?;
                let gpu = Gpu { device: self.device.clone(), context, adapter: LUID::default() };
                *self = Self::new(&gpu, (desc.Width, desc.Height), self.output, fps)?;
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
            let dest_rect = letterbox(content, self.output);
            self.context.VideoProcessorSetStreamSourceRect(&self.processor, 0, true, Some(&source_rect));
            self.context.VideoProcessorSetStreamDestRect(&self.processor, 0, true, Some(&dest_rect));
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

/// Everything between a captured frame and the encoder. On an HDR display FP16 frames are
/// tonemapped for h.264 or written straight to P010 for hdr10; otherwise the video processor
/// turns BGRA into NV12.
pub enum Pipeline {
    Sdr(Option<Tonemap>, Converter),
    Hdr10(Pq, (u32, u32)),
}

impl Pipeline {
    pub fn new(gpu: &Gpu, display: Display, input: (u32, u32), format: Format) -> Result<Self> {
        let output = (format.width, format.height);
        Ok(match format.codec {
            Codec::Hdr10 => Pipeline::Hdr10(Pq::new(&gpu.device, output, IN_FLIGHT)?, output),
            Codec::H264 => Pipeline::Sdr(
                display.hdr.then(|| Tonemap::new(&gpu.device, display)).transpose()?,
                Converter::new(gpu, input, output, format.fps)?,
            ),
        })
    }

    /// Letterboxes the `content` region of `frame` into the next encoder surface and returns it.
    pub fn convert(&mut self, frame: &ID3D11Texture2D, content: (u32, u32), fps: u32) -> Result<ID3D11Texture2D> {
        match self {
            Pipeline::Hdr10(pq, output) => pq.convert(frame, content, letterbox(content, *output)),
            Pipeline::Sdr(Some(tonemap), converter) => converter.convert(&tonemap.apply(frame)?, content, fps),
            Pipeline::Sdr(None, converter) => converter.convert(frame, content, fps),
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

/// Captures `target` into `ring` at a constant frame rate until `stop` is set.
/// When the source sends nothing new the last frame repeats, so every clip is constant frame rate.
pub fn record(
    gpu: &Gpu,
    target: Target,
    display: Display,
    choice: &Choice,
    format: Format,
    ring: &Arc<Mutex<Ring>>,
    stop: &AtomicBool,
) -> Result<()> {
    let mut source = Source::start(gpu, target, format.fps, display.hdr)?;
    let mut pipeline = Pipeline::new(gpu, display, source.size(), format)?;
    let mut encoder = Encoder::open(choice, &gpu.device, format)?;
    let period = Duration::from_nanos(1_000_000_000 / format.fps as u64);
    let started = Instant::now();
    let mut tick: u64 = 0;
    let mut last = None;
    while !stop.load(Ordering::Relaxed) {
        let due = started + period * tick as u32;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            std::thread::sleep(wait);
        }
        if let Some(frame) = source.next() {
            let content = frame.ContentSize()?;
            let content = (content.Width as u32, content.Height as u32);
            last = Some(pipeline.convert(&texture(&frame)?, content, format.fps)?);
            let _ = frame.Close();
        }
        if let Some(texture) = &last {
            let packets = encoder.encode(texture, format.time(tick as i64))?;
            let mut ring = ring.lock().expect("ring poisoned");
            packets.into_iter().for_each(|p| ring.push(p));
        }
        // after a stall, skip the missed ticks instead of bursting to catch up
        tick = (tick + 1).max((started.elapsed().as_nanos() / period.as_nanos()) as u64);
    }
    let packets = encoder.finish()?;
    let mut ring = ring.lock().expect("ring poisoned");
    packets.into_iter().for_each(|p| ring.push(p));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

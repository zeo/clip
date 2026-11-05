mod capture;
mod encode;
mod ring;
mod save;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{Result, BOOL, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, MonitorFromPoint, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, IsWindowVisible};

use capture::Target;
use encode::Format;
use ring::{Ring, SECOND};

const USAGE: &str = "\
memory capture [--monitor N | --window TEXT] [--buffer SECONDS] [--for SECONDS]
               [--fps N] [--mbps N] [--height N] [--software] [--out FILE]
memory encoders
memory self-test";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
        .and_then(|()| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) })
        .map_err(|e| e.to_string())
        .and_then(|()| match args.first().map(String::as_str) {
            Some("capture") => capture_command(&args[1..]),
            Some("encoders") => list_encoders().map_err(|e| e.to_string()),
            Some("self-test") => self_test().map_err(|e| e.to_string()),
            _ => Err(USAGE.into()),
        });
    if let Err(message) = result {
        eprintln!("{message}");
        std::process::exit(1);
    }
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

fn number(args: &[String], name: &str, default: u32) -> std::result::Result<u32, String> {
    flag(args, name).map_or(Ok(default), |v| v.parse().map_err(|_| format!("{name} needs a whole number, got {v}")))
}

fn capture_command(args: &[String]) -> std::result::Result<(), String> {
    let buffer = number(args, "--buffer", 30)?;
    let duration = number(args, "--for", 10)?;
    let fps = number(args, "--fps", 60)?.clamp(10, 240);
    let mbps = number(args, "--mbps", 20)?.clamp(1, 200);
    let max_height = number(args, "--height", 1080)?;
    let out = PathBuf::from(flag(args, "--out").unwrap_or("clip.mp4"));
    let target = match (flag(args, "--window"), flag(args, "--monitor")) {
        (Some(text), _) => Target::Window(find_window(text).ok_or(format!("no visible window titled like \"{text}\""))?),
        (None, Some(n)) => {
            let n: usize = n.parse().map_err(|_| "--monitor needs a number".to_string())?;
            let monitors = monitors();
            Target::Monitor(*monitors.get(n).ok_or(format!("monitor {n} not found, {} connected", monitors.len()))?)
        }
        (None, None) => Target::Monitor(unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }),
    };

    let gpu = capture::gpu().map_err(|e| format!("no usable gpu: {e}"))?;
    let software = args.iter().any(|a| a == "--software");
    let choice = encode::encoders(Some(gpu.adapter))
        .map_err(|e| e.to_string())?
        .into_iter()
        .find(|c| c.hardware != software)
        .ok_or("no h.264 encoder found")?;
    let size = capture::Source::start(&gpu, target, fps).map_err(|e| format!("capture failed: {e}"))?.size();
    let (width, height) = capture::output_size(size, max_height);
    let format = Format { width, height, fps, bitrate: mbps * 1_000_000 };
    println!("{} -> {width}x{height} at {fps} fps, {mbps} Mbps, {}", describe(size), choice.name);

    let ring = Arc::new(Mutex::new(Ring::new(buffer as i64 * SECOND)));
    let stop = Arc::new(AtomicBool::new(false));
    let timer = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(duration as u64));
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        })
    };
    let recorded = capture::record(&gpu, target, &choice, format, &ring, &stop);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = timer.join();
    recorded.map_err(|e| format!("recording failed: {e}"))?;

    let ring = ring.lock().expect("ring poisoned");
    let packets = ring.since(i64::MIN);
    let seconds = (ring.end().unwrap_or(0) - packets.first().map_or(0, |p| p.pts)) as f64 / SECOND as f64;
    save::write_mp4(&out, &packets, format).map_err(|e| format!("saving failed: {e}"))?;
    println!("saved {seconds:.1} s, {} frames, {:.1} MB to {}", packets.len(), ring.bytes() as f64 / 1e6, out.display());
    Ok(())
}

fn describe(size: (u32, u32)) -> String {
    format!("{}x{}", size.0, size.1)
}

fn monitors() -> Vec<HMONITOR> {
    unsafe extern "system" fn collect(monitor: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe { (*(data.0 as *mut Vec<HMONITOR>)).push(monitor) };
        true.into()
    }
    let mut found: Vec<HMONITOR> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(collect), LPARAM(&mut found as *mut _ as isize));
    }
    found
}

fn find_window(text: &str) -> Option<HWND> {
    struct Search {
        needle: String,
        found: Option<HWND>,
    }
    unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> BOOL {
        let search = unsafe { &mut *(data.0 as *mut Search) };
        let mut title = [0u16; 512];
        let len = unsafe { GetWindowTextW(window, &mut title) } as usize;
        let title = String::from_utf16_lossy(&title[..len]).to_lowercase();
        if unsafe { IsWindowVisible(window) }.as_bool() && title.contains(&search.needle) {
            search.found = Some(window);
            return false.into();
        }
        true.into()
    }
    let mut search = Search { needle: text.to_lowercase(), found: None };
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(&mut search as *mut _ as isize));
    }
    search.found
}

fn list_encoders() -> Result<()> {
    let gpu = capture::gpu()?;
    for choice in encode::encoders(Some(gpu.adapter))? {
        println!("{} {}", if choice.hardware { "hardware" } else { "software" }, choice.name);
    }
    Ok(())
}

/// Runs synthetic frames through every stage after capture, on each encoder this machine has,
/// and reads the saved file back.
fn self_test() -> Result<()> {
    let gpu = capture::gpu()?;
    let format = Format { width: 640, height: 360, fps: 30, bitrate: 4_000_000 };
    let frames = 2 * format.fps as i64;
    let dir = std::env::temp_dir().join(format!("memory-self-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| windows::core::Error::new(E_SELF_TEST, e.to_string()))?;
    let mut failures = 0;
    for choice in encode::encoders(Some(gpu.adapter))? {
        let kind = if choice.hardware { "hardware" } else { "software" };
        let path = dir.join(format!("{kind}.mp4"));
        let outcome = (|| -> Result<(u32, bool, i64)> {
            let mut converter = capture::Converter::new(&gpu, (800, 600), (format.width, format.height), format.fps)
                .map_err(at("converter"))?;
            let mut encoder = encode::Encoder::open(&choice, &gpu.device, format).map_err(at("open"))?;
            let mut ring = Ring::new(10 * SECOND);
            for i in 0..frames {
                let source = pattern(&gpu.device, 800, 600, i as u32).map_err(at("pattern"))?;
                let nv12 = converter.convert(&source, (800, 600), format.fps).map_err(at("convert"))?;
                encoder.encode(&nv12, format.time(i)).map_err(at("encode"))?.into_iter().for_each(|p| ring.push(p));
            }
            encoder.finish().map_err(at("drain"))?.into_iter().for_each(|p| ring.push(p));
            save::write_mp4(&path, &ring.since(i64::MIN), format).map_err(at("save"))?;
            read_back(&path).map_err(at("read back"))
        })();
        match outcome {
            Ok((count, key, last)) if count as i64 == frames && key && last == format.time(frames - 1) => {
                println!("ok   {kind} {}: {count} frames", choice.name)
            }
            Ok((count, key, last)) => {
                failures += 1;
                println!("FAIL {kind} {}: {count} of {frames} frames, keyframe first {key}, last at {last}", choice.name)
            }
            Err(e) => {
                failures += 1;
                println!("FAIL {kind} {}: {e}", choice.name)
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if failures > 0 {
        return Err(windows::core::Error::new(E_SELF_TEST, format!("{failures} encoder(s) failed")));
    }
    Ok(())
}

fn at(stage: &'static str) -> impl Fn(windows::core::Error) -> windows::core::Error {
    move |e| windows::core::Error::new(e.code(), format!("{stage}: {}", e.message()))
}

const E_SELF_TEST: windows::core::HRESULT = windows::core::HRESULT(0x8000_4005_u32 as i32);

/// A moving bar over a gradient, so consecutive frames differ.
fn pattern(device: &ID3D11Device, width: u32, height: u32, frame: u32) -> Result<ID3D11Texture2D> {
    let bar = (frame * 13) % width;
    let pixels: Vec<u8> = (0..height)
        .flat_map(|y| (0..width).map(move |x| (x, y)))
        .flat_map(|(x, y)| {
            if x.abs_diff(bar) < 24 {
                [255, 255, 255, 255]
            } else {
                [(x * 255 / width) as u8, (y * 255 / height) as u8, 96, 255]
            }
        })
        .collect();
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let data = D3D11_SUBRESOURCE_DATA { pSysMem: pixels.as_ptr() as _, SysMemPitch: width * 4, SysMemSlicePitch: 0 };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture))? };
    Ok(texture.expect("created"))
}

/// Frame count, whether the first frame is a keyframe, and the last frame's timestamp.
fn read_back(path: &std::path::Path) -> Result<(u32, bool, i64)> {
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let (mut count, mut first_key, mut last) = (0, false, 0);
        loop {
            let mut flags = 0;
            let mut time = 0;
            let mut sample = None;
            reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                break;
            }
            if let Some(sample) = sample {
                if count == 0 {
                    first_key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                }
                count += 1;
                last = time;
            }
        }
        Ok((count, first_key, last))
    }
}

mod audio;
mod capture;
mod encode;
mod hdr;
mod ring;
mod save;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{Result, BOOL, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_P010, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, MonitorFromPoint, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};

use capture::Target;
use encode::{Codec, Format};
use hdr::Display;
use ring::{Clock, Ring, SECOND};

const USAGE: &str = "\
memory capture [--monitor N | --window TEXT] [--buffer SECONDS] [--for SECONDS]
               [--fps N] [--mbps N] [--height N] [--software] [--hdr10] [--mic] [--app EXE]... [--out FILE]
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
        (Some(text), _) => {
            Target::Window(find_window(text).ok_or(format!("no visible window titled like \"{text}\""))?)
        }
        (None, Some(n)) => {
            let n: usize = n.parse().map_err(|_| "--monitor needs a number".to_string())?;
            let monitors = monitors();
            Target::Monitor(*monitors.get(n).ok_or(format!("monitor {n} not found, {} connected", monitors.len()))?)
        }
        (None, None) => Target::Monitor(unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) }),
    };

    let gpu = capture::gpu().map_err(|e| format!("no usable gpu: {e}"))?;
    let software = args.iter().any(|a| a == "--software");
    let display = hdr::display(target.monitor()).map_err(|e| format!("can't read the display: {e}"))?;
    let pick = |codec| -> std::result::Result<Option<encode::Choice>, String> {
        let found = encode::encoders(codec, Some(gpu.adapter)).map_err(|e| e.to_string())?;
        Ok(found.into_iter().find(|c| c.hardware != software))
    };
    let hdr10 = args.iter().any(|a| a == "--hdr10") && display.hdr;
    let (codec, choice) = match hdr10.then(|| pick(Codec::Hdr10)).transpose()?.flatten() {
        Some(choice) => (Codec::Hdr10, choice),
        None => (Codec::H264, pick(Codec::H264)?.ok_or("no h.264 encoder found")?),
    };
    let source = capture::Source::start(&gpu, target, fps, display.hdr).map_err(|e| format!("capture failed: {e}"))?;
    let size = source.size();
    let (width, height) = capture::output_size(size, max_height);
    let format = Format { width, height, fps, bitrate: mbps * 1_000_000, codec };
    println!("{}, {}", describe(size), describe_display(display, codec));
    println!("-> {width}x{height} at {fps} fps, {mbps} Mbps, {}", choice.name);

    let pipeline =
        capture::Pipeline::new(&gpu, display, size, format).map_err(|e| format!("can't convert frames: {e}"))?;
    let encoder =
        encode::Encoder::open(&choice, &gpu.device, format).map_err(|e| format!("can't open the encoder: {e}"))?;
    let game = match target {
        Target::Window(window) => {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
            Some(pid).filter(|&pid| pid != 0)
        }
        Target::Monitor(_) => None,
    };
    let apps: Vec<String> = args.windows(2).filter(|pair| pair[0] == "--app").map(|pair| pair[1].clone()).collect();
    let lanes = audio::lanes(game, &apps, args.iter().any(|a| a == "--mic"));
    println!("audio: {}", lanes.iter().map(|lane| lane.name.as_str()).collect::<Vec<_>>().join(", "));

    let span = buffer as i64 * SECOND;
    let ring = Arc::new(Mutex::new(Ring::new(span)));
    let stop = Arc::new(AtomicBool::new(false));
    let clock = Clock::start();
    let sound = audio::Recording::start(&lanes, span, clock, stop.clone()).map_err(|e| format!("audio failed: {e}"))?;
    let timer = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(duration as u64));
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
        })
    };
    let recorded = capture::record(source, pipeline, encoder, format, clock, &ring, &stop);
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let _ = timer.join();
    let rings = sound.rings.clone();
    let heard = sound.finish();
    recorded.map_err(|e| format!("recording failed: {e}"))?;
    heard.map_err(|e| format!("audio failed: {e}"))?;
    let audio: Vec<_> = rings.iter().map(|ring| ring.lock().expect("ring poisoned").since(i64::MIN)).collect();

    let ring = ring.lock().expect("ring poisoned");
    let packets = ring.since(i64::MIN);
    let seconds = (ring.end().unwrap_or(0) - packets.first().map_or(0, |p| p.pts)) as f64 / SECOND as f64;
    save::write_mp4(&out, &packets, format, &audio).map_err(|e| format!("saving failed: {e}"))?;
    println!(
        "saved {seconds:.1} s, {} frames, {:.1} MB to {}",
        packets.len(),
        ring.bytes() as f64 / 1e6,
        out.display()
    );
    Ok(())
}

fn describe(size: (u32, u32)) -> String {
    format!("{}x{}", size.0, size.1)
}

fn describe_display(display: Display, codec: Codec) -> String {
    match (display.hdr, codec) {
        (false, _) => "sdr display".into(),
        (true, Codec::H264) => format!(
            "hdr display (sdr white {:.0} nits, peak {:.0} nits), tonemapped to sdr",
            display.sdr_white, display.peak
        ),
        (true, Codec::Hdr10) => {
            format!("hdr display (peak {:.0} nits), kept as hdr10", display.peak)
        }
    }
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
    for codec in [Codec::H264, Codec::Hdr10] {
        for choice in encode::encoders(codec, Some(gpu.adapter))? {
            let kind = if choice.hardware { "hardware" } else { "software" };
            println!("{codec:?} {kind} {}", choice.name);
        }
    }
    Ok(())
}

/// Runs synthetic frames through every stage after capture, on each encoder this machine has,
/// with two synthetic audio lanes, and reads the saved file back. Covers sdr, hdr tonemapped to sdr, and hdr10.
fn self_test() -> Result<()> {
    let gpu = capture::gpu()?;
    let dir = std::env::temp_dir().join(format!("memory-self-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| windows::core::Error::new(E_SELF_TEST, e.to_string()))?;
    let mut failures = 0;
    match check_transfer(&gpu) {
        Ok(report) => println!("ok   transfer: {report}"),
        Err(e) => {
            failures += 1;
            println!("FAIL transfer: {}", e.message())
        }
    }
    let lanes = [tone(440.0).map_err(at("audio lane"))?, tone(660.0).map_err(at("audio lane"))?];
    for (case, codec, hdr) in
        [("sdr", Codec::H264, false), ("tonemap", Codec::H264, true), ("hdr10", Codec::Hdr10, true)]
    {
        let format = Format { width: 640, height: 360, fps: 30, bitrate: 4_000_000, codec };
        let frames = 2 * format.fps as i64;
        let choices = encode::encoders(codec, Some(gpu.adapter))?;
        if choices.is_empty() {
            println!("skip {case}: no {codec:?} encoder");
        }
        for choice in choices {
            let kind = if choice.hardware { "hardware" } else { "software" };
            let path = dir.join(format!("{case}-{kind}.mp4"));
            let outcome = (|| -> Result<Vec<(u32, bool, i64)>> {
                let display = if hdr { TEST_DISPLAY } else { SDR_DISPLAY };
                let mut pipeline = capture::Pipeline::new(&gpu, display, (800, 600), format).map_err(at("pipeline"))?;
                let mut encoder = encode::Encoder::open(&choice, &gpu.device, format).map_err(at("open"))?;
                let mut ring = Ring::new(10 * SECOND);
                for i in 0..frames {
                    let source = pattern(&gpu.device, 800, 600, i as u32, hdr).map_err(at("pattern"))?;
                    let frame = pipeline.convert(&source, (800, 600), format.fps).map_err(at("convert"))?;
                    encoder
                        .encode(&frame, format.time(i))
                        .map_err(at("encode"))?
                        .into_iter()
                        .for_each(|p| ring.push(p));
                }
                encoder.finish().map_err(at("drain"))?.into_iter().for_each(|p| ring.push(p));
                save::write_mp4(&path, &ring.since(i64::MIN), format, &lanes).map_err(at("save"))?;
                read_back(&path).map_err(at("read back"))
            })();
            let end = format.time(frames - 1);
            // a clip's audio may stop up to two aac frames short of the last video frame
            let heard = |(count, _, last): &(u32, bool, i64)| *count >= 90 && end - last <= SECOND / 20;
            match outcome.as_deref() {
                Ok([(count, true, last), sound @ ..])
                    if *count as i64 == frames
                        && *last == end
                        && sound.len() == lanes.len()
                        && sound.iter().all(heard) =>
                {
                    let packets: Vec<u32> = sound.iter().map(|s| s.0).collect();
                    println!("ok   {case} {kind} {}: {count} frames, aac packets {packets:?}", choice.name)
                }
                Ok(streams) => {
                    failures += 1;
                    println!(
                        "FAIL {case} {kind} {}: want {frames} frames and {} audio tracks, got {streams:?}",
                        choice.name,
                        lanes.len()
                    )
                }
                Err(e) => {
                    failures += 1;
                    println!("FAIL {case} {kind} {}: {e}", choice.name)
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if failures > 0 {
        return Err(windows::core::Error::new(E_SELF_TEST, format!("{failures} check(s) failed")));
    }
    Ok(())
}

fn at(stage: &'static str) -> impl Fn(windows::core::Error) -> windows::core::Error {
    move |e| windows::core::Error::new(e.code(), format!("{stage}: {}", e.message()))
}

const E_SELF_TEST: windows::core::HRESULT = windows::core::HRESULT(0x8000_4005_u32 as i32);

const TEST_DISPLAY: Display = Display { hdr: true, sdr_white: 200.0, peak: 1000.0 };
const SDR_DISPLAY: Display = Display { hdr: false, sdr_white: hdr::SCRGB_WHITE, peak: hdr::SCRGB_WHITE };
const MID_GREY: u8 = 128;

/// How the compositor stores an sRGB value on an HDR desktop: linear, scaled up to the sdr white level.
fn composed(srgb: u8, display: Display) -> f32 {
    let v = srgb as f32 / 255.0;
    let linear = if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    linear * display.sdr_white / hdr::SCRGB_WHITE
}

/// Runs the hdr pattern through the tonemap and the PQ pass and reads back the values that matter:
/// sdr grey keeps its value, a 250 nit highlight lands below clipping, the panel peak reaches white,
/// and in hdr10 grey and peak sit at their PQ codes with neutral chroma.
fn check_transfer(gpu: &capture::Gpu) -> Result<String> {
    let source = pattern(&gpu.device, 64, 64, 0, true)?;
    let sdr = read_texture(gpu, &hdr::Tonemap::new(&gpu.device, TEST_DISPLAY)?.apply(&source)?)?;
    let full = windows::Win32::Foundation::RECT { left: 0, top: 0, right: 64, bottom: 64 };
    let p010 = read_texture(gpu, &hdr::Pq::new(&gpu.device, (64, 64), 1)?.convert(&source, (64, 64), full)?)?;
    let byte = |x: usize, y: usize, channel: usize| sdr.0[y * sdr.1 + x * 4 + channel] as u32;
    let sample = |offset: usize| u16::from_le_bytes([p010.0[offset], p010.0[offset + 1]]) as u32 >> 6;
    let luma = |x: usize, y: usize| sample(y * p010.1 + x * 2);
    let chroma = |x: usize, y: usize, channel: usize| sample((64 + y / 2) * p010.1 + (x / 2) * 4 + channel * 2);
    // grey from the flat left half, highlight from the red ramp on the right, peak from the bar below it
    let (grey, highlight, white) = (byte(16, 32, 1), byte(40, 10, 2), byte(40, 48, 2));
    let (pq_grey, pq_peak) = (luma(16, 32), luma(40, 48));
    let neutral = (chroma(16, 32, 0), chroma(16, 32, 1));
    let code = |nits: f32| (64.0 + 876.0 * pq(nits)).round() as u32;
    let want_grey = code(composed(MID_GREY, TEST_DISPLAY) * hdr::SCRGB_WHITE);
    let want_peak = code(TEST_DISPLAY.peak);
    let report = format!(
        "sdr grey {MID_GREY} -> {grey}, 250 nits -> {highlight}, peak -> {white}; \
         hdr10 grey y {pq_grey} (want {want_grey}) cb cr {neutral:?}, peak y {pq_peak} (want {want_peak})"
    );
    let good = grey.abs_diff(MID_GREY as u32) <= 2
        && (236..255).contains(&highlight)
        && white >= 254
        && pq_grey.abs_diff(want_grey) <= 1
        && pq_peak.abs_diff(want_peak) <= 1
        && neutral.0.abs_diff(512) <= 1
        && neutral.1.abs_diff(512) <= 1;
    if good {
        Ok(report)
    } else {
        Err(windows::core::Error::new(E_SELF_TEST, report))
    }
}

/// SMPTE ST 2084 encode, written out separately from the shader so the check is independent
fn pq(nits: f32) -> f32 {
    let y = (nits / 10_000.0).powf(0.159_301_76);
    ((0.835_937_5 + 18.851_563 * y) / (1.0 + 18.6875 * y)).powf(78.84375)
}

/// Copies a texture to the CPU and returns its rows (both planes for P010) with the row pitch.
fn read_texture(gpu: &capture::Gpu, texture: &ID3D11Texture2D) -> Result<(Vec<u8>, usize)> {
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);
        let rows = if desc.Format == DXGI_FORMAT_P010 { desc.Height * 3 / 2 } else { desc.Height } as usize;
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        let mut staging = None;
        gpu.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.expect("created");
        gpu.context.CopyResource(&staging, texture);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let pitch = mapped.RowPitch as usize;
        let bytes = std::slice::from_raw_parts(mapped.pData as *const u8, pitch * rows).to_vec();
        gpu.context.Unmap(&staging, 0);
        Ok((bytes, pitch))
    }
}

/// A moving bar over a gradient, so consecutive frames differ. The hdr version is scRGB FP16
/// with sdr grey on the left half, highlights up to the panel peak on the right, and a bar at peak.
fn pattern(device: &ID3D11Device, width: u32, height: u32, frame: u32, hdr: bool) -> Result<ID3D11Texture2D> {
    let bar = (frame * 13 + 40) % width;
    let (format, bytes): (DXGI_FORMAT, Vec<u8>) = if hdr {
        let grey = composed(MID_GREY, TEST_DISPLAY);
        let peak = TEST_DISPLAY.peak / hdr::SCRGB_WHITE;
        let pixels = (0..height).flat_map(|y| (0..width).map(move |x| (x, y))).flat_map(|(x, y)| {
            let rgb = if x.abs_diff(bar) < 24 && y > height / 2 {
                [peak; 3]
            } else if x < width / 2 {
                [grey; 3]
            } else {
                [peak * (x - width / 2) as f32 / (width / 2) as f32, 0.5, (y as f32 / height as f32).max(0.01)]
            };
            [rgb[0], rgb[1], rgb[2], 1.0].map(half)
        });
        (DXGI_FORMAT_R16G16B16A16_FLOAT, pixels.flat_map(u16::to_le_bytes).collect())
    } else {
        let pixels = (0..height).flat_map(|y| (0..width).map(move |x| (x, y))).flat_map(|(x, y)| {
            if x.abs_diff(bar) < 24 {
                [255, 255, 255, 255]
            } else {
                [(x * 255 / width) as u8, (y * 255 / height) as u8, 96, 255]
            }
        });
        (DXGI_FORMAT_B8G8R8A8_UNORM, pixels.collect())
    };
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        // capture frames carry render target binding and the video processor depends on it
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let pitch = bytes.len() as u32 / height;
    let data = D3D11_SUBRESOURCE_DATA { pSysMem: bytes.as_ptr() as _, SysMemPitch: pitch, SysMemSlicePitch: 0 };
    let mut texture = None;
    unsafe { device.CreateTexture2D(&desc, Some(&data), Some(&mut texture))? };
    Ok(texture.expect("created"))
}

/// f32 to IEEE half for the positive normal values the test pattern uses.
fn half(value: f32) -> u16 {
    if value <= 0.0 {
        return 0;
    }
    let bits = value.to_bits();
    let exponent = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    ((exponent.clamp(1, 30) as u16) << 10) | ((bits >> 13) & 0x3ff) as u16
}

/// Frame count, whether the first frame is a keyframe, and the last frame's timestamp.
/// Every stream's sample count, whether it starts on a keyframe, and its last timestamp: the video
/// stream first, then the audio streams in file order.
fn read_back(path: &std::path::Path) -> Result<Vec<(u32, bool, i64)>> {
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, true)?;
        let (mut video, mut audio) = (Vec::new(), Vec::new());
        for stream in 0.. {
            let Ok(media) = reader.GetNativeMediaType(stream, 0) else {
                break;
            };
            let streams = if media.GetMajorType()? == MFMediaType_Video { &mut video } else { &mut audio };
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
            streams.push((count, first_key, last));
        }
        video.extend(audio);
        Ok(video)
    }
}

/// Two seconds of a sine tone through the aac encoder in 10 ms chunks, as a capture lane delivers them.
fn tone(hz: f32) -> Result<Vec<Arc<ring::Packet>>> {
    let mut aac = audio::Aac::new()?;
    let mut ring = Ring::new(10 * SECOND);
    let chunk = audio::RATE / 100;
    for start in (0..2 * audio::RATE).step_by(chunk as usize) {
        let pcm: Vec<u8> = (start..start + chunk)
            .map(|n| ((n as f32 * hz * std::f32::consts::TAU / audio::RATE as f32).sin() * 8000.0) as i16)
            .flat_map(|v| [v, v])
            .flat_map(i16::to_le_bytes)
            .collect();
        aac.encode(start as i64 * SECOND / audio::RATE as i64, &pcm)?.into_iter().for_each(|p| ring.push(p));
    }
    aac.finish()?.into_iter().for_each(|p| ring.push(p));
    Ok(ring.since(i64::MIN))
}

#[cfg(test)]
mod tests {
    use super::half;

    #[test]
    fn half_matches_ieee_bit_patterns() {
        assert_eq!(half(1.0), 0x3c00);
        assert_eq!(half(2.5), 0x4100);
        assert_eq!(half(0.5), 0x3800);
        assert_eq!(half(0.0), 0);
    }
}

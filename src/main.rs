#![windows_subsystem = "windows"]

mod app;
mod art;
mod audio;
mod capture;
mod clipboard;
mod encode;
mod export;
mod focus;
mod library;
mod notice;
use hdrkit as hdr;
mod ring;
mod save;
mod sync;
mod toast;

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use windows::core::{Interface, Result, BOOL, HSTRING};
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_P010, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, MonitorFromPoint, HDC, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Console::{AttachConsole, ATTACH_PARENT_PROCESS};
use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible};

use capture::Target;
use encode::{Codec, Format};
use hdr::Display;
use ring::{Clock, Ring, SECOND};

const USAGE: &str = "\
clip capture [--monitor N | --window TEXT] [--buffer SECONDS] [--for SECONDS]
               [--fps N] [--mbps N] [--height N] [--scale FILTER] [--software] [--hdr10] [--mic] [--app EXE]... [--out FILE]
clip sync
clip encoders
clip self-test [--live]";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        return app::run();
    }
    // the exe is a windows app so the tray never opens a console, so the command line borrows the caller's
    let _ = unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
        .and_then(|()| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) })
        .map_err(|e| e.to_string())
        .and_then(|()| match args.first().map(String::as_str) {
            Some("capture") => capture_command(&args[1..]),
            Some("sync") => sync::run(&args[1..]),
            Some("encoders") => list_encoders().map_err(|e| e.to_string()),
            Some("self-test") => self_test().map_err(|e| e.to_string()).and_then(|()| match args.get(1) {
                Some(live) if live == "--live" => sync::run(&args[2..]),
                _ => Ok(()),
            }),
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
    let duration = number(args, "--for", 10)?;
    let target = match (flag(args, "--window"), flag(args, "--monitor")) {
        (Some(text), _) => {
            Target::Window(find_window(text).ok_or(format!("no visible window titled like \"{text}\""))?)
        }
        (None, Some(n)) => {
            let n: usize = n.parse().map_err(|_| "--monitor needs a number".to_string())?;
            let monitors = monitors();
            Target::Monitor(*monitors.get(n).ok_or(format!("monitor {n} not found, {} connected", monitors.len()))?)
        }
        (None, None) => primary(),
    };
    let game = match target {
        Target::Window(window) => {
            let mut pid = 0;
            unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
            Some(pid).filter(|&pid| pid != 0)
        }
        Target::Monitor(_) => None,
    };
    let apps: Vec<String> = args.windows(2).filter(|pair| pair[0] == "--app").map(|pair| pair[1].clone()).collect();
    let mic = args.iter().any(|a| a == "--mic").then(|| (String::new(), 0));
    let lanes = audio::lanes(game, &apps, mic, &Default::default());
    let out = PathBuf::from(flag(args, "--out").unwrap_or("clip.mp4"));
    record_clip(args, target, &lanes, &out, move |_| std::thread::sleep(Duration::from_secs(duration as u64)))?;
    Ok(())
}

fn primary() -> Target {
    Target::Monitor(unsafe { MonitorFromPoint(POINT::default(), MONITOR_DEFAULTTOPRIMARY) })
}

/// How a session records, from the command line or the app's settings.
#[derive(Clone)]
pub struct Options {
    pub buffer: u32,
    pub fps: u32,
    pub mbps: u32,
    pub height: u32,
    pub filter: hdr::Filter,
    pub software: bool,
    pub hdr10: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> std::result::Result<Self, String> {
        Ok(Self {
            buffer: number(args, "--buffer", 30)?,
            fps: number(args, "--fps", 60)?,
            mbps: number(args, "--mbps", 20)?,
            height: number(args, "--height", 1080)?,
            filter: match flag(args, "--scale") {
                Some(name) => hdr::Filter::named(name)
                    .ok_or(format!("--scale takes bilinear, area, bicubic or lanczos, got {name}"))?,
                None => hdr::Filter::Area,
            },
            software: args.iter().any(|a| a == "--software"),
            hdr10: args.iter().any(|a| a == "--hdr10"),
        })
    }
}

pub struct Saved {
    pub seconds: f64,
    pub frames: usize,
    pub bytes: usize,
    /// clock time of the first frame
    pub start: i64,
}

type Prepared = (capture::Source, capture::Pipeline, encode::Encoder, Format, Option<hdr::Mastering>, String);

/// Picks the encoder and opens the capture, the converter and the encoder for `target`.
fn prepare(options: &Options, target: Target) -> std::result::Result<Prepared, String> {
    let fps = options.fps.clamp(10, 240);
    let mbps = options.mbps.clamp(1, 200);
    let gpu = capture::gpu().map_err(|e| format!("no usable gpu: {e}"))?;
    let display = hdr::display(target.monitor()).map_err(|e| format!("can't read the display: {e}"))?;
    let pick = |codec| -> std::result::Result<Option<encode::Choice>, String> {
        let found = encode::encoders(codec, Some(gpu.adapter)).map_err(|e| e.to_string())?;
        Ok(found.into_iter().find(|c| c.hardware != options.software))
    };
    let hdr10 = options.hdr10 && display.hdr;
    let (codec, choice) = match hdr10.then(|| pick(Codec::Hdr10)).transpose()?.flatten() {
        Some(choice) => (Codec::Hdr10, choice),
        None => (Codec::H264, pick(Codec::H264)?.ok_or("no h.264 encoder found")?),
    };
    let source = capture::Source::start(&gpu, target, fps, display.hdr).map_err(|e| format!("capture failed: {e}"))?;
    let size = source.size();
    let (width, height) = capture::output_size(size, options.height);
    let format = Format { width, height, fps, bitrate: mbps * 1_000_000, codec };
    let mastering = matches!(codec, Codec::Hdr10)
        .then(|| hdr::mastering(target.monitor()))
        .transpose()
        .map_err(|e| format!("can't read the display: {e}"))?;
    let summary = format!(
        "{}, {}\n-> {width}x{height} at {fps} fps, {mbps} Mbps, {:?} scaling, {}",
        describe(size),
        describe_display(display, codec),
        options.filter,
        choice.name
    );
    let pipeline = capture::Pipeline::new(&gpu, display, format, options.filter)
        .map_err(|e| format!("can't convert frames: {e}"))?;
    let encoder = encode::Encoder::open(&choice, &gpu.device, format, encode::Purpose::Capture)
        .map_err(|e| format!("can't open the encoder: {e}"))?;
    Ok((source, pipeline, encoder, format, mastering, summary))
}

/// A recording held in rings that can be saved from at any moment, until it stops.
pub struct Session {
    pub clock: Clock,
    pub summary: String,
    format: Format,
    mastering: Option<hdr::Mastering>,
    names: Vec<String>,
    ring: Arc<Mutex<Ring>>,
    rings: Vec<Arc<Mutex<Ring>>>,
    stop: Arc<AtomicBool>,
    shots: capture::Shots,
    video: Option<std::thread::JoinHandle<Result<()>>>,
    sound: Option<audio::Recording>,
}

impl Session {
    pub fn start(options: &Options, target: Target, lanes: &[audio::Lane]) -> std::result::Result<Self, String> {
        let span = options.buffer.max(1) as i64 * SECOND;
        let ring = Arc::new(Mutex::new(Ring::new(span)));
        let stop = Arc::new(AtomicBool::new(false));
        let shots = capture::Shots::default();
        let (opened, ready) = std::sync::mpsc::sync_channel(1);
        let video = {
            let (options, ring, stop, shots) = (options.clone(), ring.clone(), stop.clone(), shots.clone());
            std::thread::spawn(move || -> Result<()> {
                unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
                match prepare(&options, target) {
                    Ok((source, pipeline, encoder, format, mastering, summary)) => {
                        let clock = Clock::start();
                        let _ = opened.send(Ok((format, mastering, summary, clock)));
                        capture::record(source, pipeline, encoder, format, clock, &ring, &stop, &shots)
                    }
                    Err(message) => {
                        let _ = opened.send(Err(message));
                        Ok(())
                    }
                }
            })
        };
        let (format, mastering, summary, clock) = match ready.recv() {
            Ok(prepared) => prepared?,
            Err(_) => {
                let reason = video.join().ok().and_then(|r| r.err()).map_or("it stopped".into(), |e| e.to_string());
                return Err(format!("capture failed: {reason}"));
            }
        };
        let sound = match audio::Recording::start(lanes, span, clock, stop.clone()) {
            Ok(sound) => sound,
            Err(e) => {
                stop.store(true, std::sync::atomic::Ordering::Relaxed);
                let _ = video.join();
                return Err(format!("audio failed: {e}"));
            }
        };
        Ok(Self {
            clock,
            summary,
            format,
            mastering,
            names: lanes.iter().map(|lane| lane.name.clone()).collect(),
            ring,
            rings: sound.rings.clone(),
            stop,
            shots,
            video: Some(video),
            sound: Some(sound),
        })
    }

    /// Writes what the rings hold right now to `out`, or only about the last `seconds` of it, from
    /// the keyframe before.
    pub fn save(&self, out: &std::path::Path, seconds: Option<u32>) -> std::result::Result<Saved, String> {
        let (packets, end) = {
            let ring = self.ring.lock().expect("ring poisoned");
            let end = ring.end().unwrap_or(0);
            (ring.since(seconds.map_or(i64::MIN, |s| end - s as i64 * SECOND)), end)
        };
        let start = packets.first().map_or(0, |p| p.pts);
        let from = if seconds.is_some() { start } else { i64::MIN };
        let audio: Vec<_> = self.rings.iter().map(|ring| ring.lock().expect("ring poisoned").since(from)).collect();
        let bytes = packets.iter().map(|p| p.data.len()).sum();
        let names: Vec<&str> = self.names.iter().map(String::as_str).collect();
        save::write_mp4(out, &packets, self.format, &audio, &names, self.mastering)
            .map_err(|e| format!("saving failed: {e}"))?;
        Ok(Saved { seconds: (end - start) as f64 / SECOND as f64, frames: packets.len(), bytes, start })
    }

    /// The next frame the game draws, as captured.
    pub fn still(&self) -> std::result::Result<capture::Still, String> {
        let (taken, took) = std::sync::mpsc::channel();
        self.shots.lock().expect("shots poisoned").push(taken);
        match took.recv_timeout(Duration::from_millis(1500)) {
            Ok(still) => still.map_err(|e| format!("couldn't take the screenshot: {e}")),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err("the game drew nothing new to take".into()),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => Err("the recording stopped".into()),
        }
    }

    /// Starts writing everything from the newest keyframe on to `out`, for as long as it's pulled.
    pub fn tape(&self, out: &std::path::Path) -> std::result::Result<Tape, String> {
        let first = {
            let ring = self.ring.lock().expect("ring poisoned");
            ring.since(ring.end().ok_or("nothing is recorded yet")?)
                .into_iter()
                .next()
                .ok_or("nothing is recorded yet")?
        };
        let writer = save::Writer::open(out, &first, self.format, self.rings.len())
            .map_err(|e| format!("can't start the recording: {e}"))?;
        Ok(Tape {
            writer,
            ring: self.ring.clone(),
            rings: self.rings.clone(),
            names: self.names.clone(),
            mastering: self.mastering,
            video: first.pts - 1,
            audio: vec![first.pts - 1; self.rings.len()],
            start: first.pts,
            frames: 0,
            bytes: 0,
        })
    }

    /// Stops recording and flushes the encoders into the rings, which can still be saved after.
    pub fn stop(&mut self) -> std::result::Result<(), String> {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        let video = self.video.take().map(|thread| thread.join());
        let heard = self.sound.take().map(audio::Recording::finish).transpose();
        if let Some(recorded) = video {
            recorded
                .map_err(|_| "the capture thread panicked".to_string())?
                .map_err(|e| format!("recording failed: {e}"))?;
        }
        heard.map_err(|e| format!("audio failed: {e}"))?;
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// A recording written out as it goes, from the rings of a session that may have ended since.
pub struct Tape {
    writer: save::Writer,
    ring: Arc<Mutex<Ring>>,
    rings: Vec<Arc<Mutex<Ring>>>,
    names: Vec<String>,
    mastering: Option<hdr::Mastering>,
    /// pts of the newest packet written, per stream
    video: i64,
    audio: Vec<i64>,
    start: i64,
    frames: usize,
    bytes: usize,
}

impl Tape {
    /// Writes what the rings gained since the last pull.
    pub fn pull(&mut self) -> std::result::Result<(), String> {
        let fresh = |ring: &Mutex<Ring>, after: i64| -> Vec<Arc<ring::Packet>> {
            let ring = ring.lock().expect("ring poisoned");
            ring.since(after).into_iter().filter(|p| p.pts > after).collect()
        };
        let packets = fresh(&self.ring, self.video);
        let Some(newest) = packets.last().map(|p| p.pts) else { return Ok(()) };
        // sound only up to the newest frame, so the next pull's frames never land before it
        let audio: Vec<_> = (self.rings.iter().zip(&self.audio))
            .map(|(ring, &after)| fresh(ring, after).into_iter().filter(|p| p.pts <= newest).collect::<Vec<_>>())
            .collect();
        self.writer.write(&packets, &audio).map_err(|e| format!("recording failed: {e}"))?;
        for (at, lane) in self.audio.iter_mut().zip(&audio) {
            *at = lane.last().map_or(*at, |p| p.pts);
        }
        self.video = newest;
        self.frames += packets.len();
        self.bytes += packets.iter().map(|p| p.data.len()).sum::<usize>();
        Ok(())
    }

    /// Writes the rest and closes the file.
    pub fn finish(mut self) -> std::result::Result<Saved, String> {
        self.pull()?;
        let names: Vec<&str> = self.names.iter().map(String::as_str).collect();
        let Self { writer, video, start, frames, bytes, mastering, .. } = self;
        writer.finish(&names, mastering).map_err(|e| format!("saving the recording failed: {e}"))?;
        Ok(Saved { seconds: (video - start) as f64 / SECOND as f64, frames, bytes, start })
    }
}

/// Records `target` and `lanes` while `work` runs, saves the clip to `out`, and returns what `work`
/// returned with the clock time the clip starts at.
fn record_clip<T>(
    args: &[String],
    target: Target,
    lanes: &[audio::Lane],
    out: &std::path::Path,
    work: impl FnOnce(Clock) -> T,
) -> std::result::Result<(T, i64), String> {
    let mut session = Session::start(&Options::parse(args)?, target, lanes)?;
    println!("{}", session.summary);
    println!("audio: {}", lanes.iter().map(|lane| lane.name.as_str()).collect::<Vec<_>>().join(", "));
    let result = work(session.clock);
    session.stop()?;
    let saved = session.save(out, None)?;
    println!(
        "saved {:.1} s, {} frames, {:.1} MB to {}",
        saved.seconds,
        saved.frames,
        saved.bytes as f64 / 1e6,
        out.display()
    );
    Ok((result, saved.start))
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
/// with two synthetic audio lanes, and reads the saved file back. Covers sdr, hdr tonemapped to sdr, and hdr10,
/// each switching hdr on or off halfway.
fn self_test() -> Result<()> {
    let gpu = capture::gpu()?;
    let dir = std::env::temp_dir().join(format!("clip-self-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| windows::core::Error::new(E_SELF_TEST, e.to_string()))?;
    let mut failures = 0;
    match unsafe {
        windows::core::Interface::cast::<windows::Win32::Graphics::Dxgi::IDXGIDevice>(&gpu.device)?
            .GetGPUThreadPriority()?
    } {
        capture::GPU_PRIORITY => println!("ok   gpu priority: {}", capture::GPU_PRIORITY),
        other => {
            failures += 1;
            println!("FAIL gpu priority: {other}, asked for {}", capture::GPU_PRIORITY)
        }
    }
    match check_transfer(&gpu) {
        Ok(report) => println!("ok   transfer: {report}"),
        Err(e) => {
            failures += 1;
            println!("FAIL transfer: {}", e.message())
        }
    }
    match check_scale(&gpu) {
        Ok(report) => println!("ok   scale: {report}"),
        Err(e) => {
            failures += 1;
            println!("FAIL scale: {}", e.message())
        }
    }
    let lanes = [tone(440.0).map_err(at("audio lane"))?, tone(660.0).map_err(at("audio lane"))?];
    for (case, codec, hdr) in
        [("sdr", Codec::H264, false), ("tonemap", Codec::H264, true), ("hdr10", Codec::Hdr10, true)]
    {
        let format = Format { width: 1280, height: 720, fps: 30, bitrate: 4_000_000, codec };
        let frames = 2 * format.fps as i64;
        let choices = encode::encoders(codec, Some(gpu.adapter))?;
        if choices.is_empty() {
            println!("skip {case}: no {codec:?} encoder");
        }
        for choice in choices {
            let kind = if choice.hardware { "hardware" } else { "software" };
            let path = dir.join(format!("{case}-{kind}.mp4"));
            let outcome = (|| -> Result<(Streams, Option<i64>)> {
                let display = if hdr { TEST_DISPLAY } else { SDR_DISPLAY };
                let mut pipeline =
                    capture::Pipeline::new(&gpu, display, format, hdr::Filter::Area).map_err(at("pipeline"))?;
                let mut encoder = encode::Encoder::open(&choice, &gpu.device, format, encode::Purpose::Capture)
                    .map_err(at("open"))?;
                let mut ring = Ring::new(10 * SECOND);
                // halfway through, the display switches hdr on or off
                let switched = if hdr { SDR_DISPLAY } else { TEST_DISPLAY };
                for i in 0..frames {
                    if i == frames / 2 {
                        pipeline.follow(switched);
                    }
                    let hdr = hdr != (i >= frames / 2);
                    let source = pattern(&gpu.device, 800, 600, i as u32, hdr).map_err(at("pattern"))?;
                    let frame = pipeline.convert(&source, (800, 600), format.fps).map_err(at("convert"))?;
                    encoder
                        .encode(&frame, format.time(i))
                        .map_err(at("encode"))?
                        .into_iter()
                        .for_each(|p| ring.push(p));
                }
                encoder.finish().map_err(at("drain"))?.into_iter().for_each(|p| ring.push(p));
                let mastering = matches!(codec, Codec::Hdr10).then_some(TEST_PANEL);
                save::write_mp4(&path, &ring.since(i64::MIN), format, &lanes, &["tone 440", "tone 660"], mastering)
                    .map_err(at("save"))?;
                let streams = read_back(&path).map_err(at("read back"))?;
                Ok((streams, sync::onsets(&path).map_err(at("read sound back"))?.1.first().copied()))
            })();
            let end = format.time(frames - 1);
            // a clip's audio may stop up to two aac frames short of the last video frame
            let heard = |(count, _, last): &(u32, bool, i64)| *count >= 90 && end - last <= SECOND / 20;
            // the tone starts one second in, and the clip has to play it right there
            let on_time = |onset: Option<i64>| onset.is_some_and(|t| (t - SECOND).abs() < SECOND / 1000);
            match outcome.as_ref().map(|(streams, onset)| (streams.as_slice(), *onset)) {
                Ok(([(count, true, last), sound @ ..], onset))
                    if *count as i64 == frames
                        && *last == end
                        && sound.len() == lanes.len()
                        && sound.iter().all(heard)
                        && on_time(onset) =>
                {
                    let packets: Vec<u32> = sound.iter().map(|s| s.0).collect();
                    println!("ok   {case} {kind} {}: {count} frames, aac packets {packets:?}", choice.name)
                }
                Ok((streams, onset)) => {
                    failures += 1;
                    println!(
                        "FAIL {case} {kind} {}: want {frames} frames, {} audio tracks and the tone at 1 s, got {streams:?}, tone at {onset:?}",
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
    for case in ["sdr-hardware", "hdr10-hardware"] {
        let source = dir.join(format!("{case}.mp4"));
        if !source.exists() {
            continue;
        }
        let (mixed, kept) = (dir.join(format!("{case} mixed.mp4")), dir.join(format!("{case} kept.mp4")));
        let fitted = dir.join(format!("{case} fitted.mp4"));
        // one second in this many bytes leaves under 1 Mbps for the video, which drops it to 540p
        let budget = 340_000;
        let outcome = (|| -> std::result::Result<_, String> {
            let tracks = export::tracks(&source)?.into_iter().map(|(name, pcm)| (name, pcm.len() / audio::BLOCK));
            let tracks: Vec<(String, usize)> = tracks.collect();
            export::trim(&source, &mixed, 0.5, 1.5, &[0, 1], true, &Default::default())?;
            export::trim(&source, &kept, 0.5, 1.5, &[1], false, &Default::default())?;
            export::trim(
                &source,
                &fitted,
                0.5,
                1.5,
                &[0, 1],
                true,
                &export::Job { fit: Some(budget), ..Default::default() },
            )?;
            let back = |path| read_back(path).map_err(|e| e.message());
            let onset = sync::onsets(&mixed).map_err(|e| e.message())?.1.first().copied();
            let heard = sync::onsets(&source).map_err(|e| e.message())?.1.first().copied();
            let size = std::fs::metadata(&fitted).map_err(|e| e.to_string())?.len();
            let fit = (back(&fitted)?, frame_size(&fitted).map_err(|e| e.message())?, size);
            let level = |path, from| luma(path, from).map_err(|e| e.message());
            let levels = [level(&source, 0.5)?, level(&mixed, 0.0)?, level(&fitted, 0.0)?];
            Ok((tracks, back(&mixed)?, back(&kept)?, onset.zip(heard), fit, levels))
        })();
        let two_seconds = |frames: usize| frames.abs_diff(2 * audio::RATE as usize) <= 2048;
        // a cut from 0.5 s hears the tone half a second sooner than the source does
        let on_time = |onsets: Option<(i64, i64)>| {
            onsets.is_some_and(|(cut, source)| (source - cut - SECOND / 2).abs() < SECOND / 1000)
        };
        match &outcome {
            Ok((tracks, mixed, kept, onset, (fitted, frame, size), [level, rest @ ..]))
                if tracks.iter().map(|(name, _)| name.as_str()).eq(["tone 440", "tone 660"])
                    && tracks.iter().all(|&(_, frames)| two_seconds(frames))
                    && matches!(mixed.as_slice(), [(30, true, _), (40.., _, _)])
                    && matches!(kept.as_slice(), [(30, true, _), (40.., _, _)])
                    && on_time(*onset)
                    && matches!(fitted.as_slice(), [(30, true, _), (40.., _, _)])
                    && *frame == (960, 540)
                    && *size <= budget
                    && rest.iter().all(|other| (other - level).abs() < 2.0 / 256.0) =>
            {
                println!(
                    "ok   trim {case}: 30 frames from 0.5 s, mixed and kept audio, {size} bytes at 960x540, levels kept"
                )
            }
            other => {
                failures += 1;
                println!("FAIL trim {case}: {other:?}")
            }
        }
    }
    match check_tape(&gpu, &lanes, &dir) {
        Ok(report) => println!("ok   tape: {report}"),
        Err(e) => {
            failures += 1;
            println!("FAIL tape: {}", e.message())
        }
    }
    match check_png(&dir) {
        Ok(report) => println!("ok   png: {report}"),
        Err(e) => {
            failures += 1;
            println!("FAIL png: {}", e.message())
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    if failures > 0 {
        return Err(windows::core::Error::new(E_SELF_TEST, format!("{failures} check(s) failed")));
    }
    Ok(())
}

/// Feeds rings a few frames at a time while a tape pulls from them, the way a long recording runs,
/// and checks the file holds every packet once.
fn check_tape(gpu: &capture::Gpu, lanes: &[Vec<Arc<ring::Packet>>], dir: &std::path::Path) -> Result<String> {
    let fail = |what: String| windows::core::Error::new(E_SELF_TEST, what);
    let format = Format { width: 1280, height: 720, fps: 30, bitrate: 4_000_000, codec: Codec::H264 };
    let choice = encode::encoders(Codec::H264, Some(gpu.adapter))?
        .into_iter()
        .next()
        .ok_or_else(|| fail("no h.264 encoder".into()))?;
    let mut pipeline = capture::Pipeline::new(gpu, SDR_DISPLAY, format, hdr::Filter::Area).map_err(at("pipeline"))?;
    let mut encoder =
        encode::Encoder::open(&choice, &gpu.device, format, encode::Purpose::Capture).map_err(at("open"))?;
    let mut video = Vec::new();
    let frames = 3 * format.fps as i64;
    for i in 0..frames {
        let source = pattern(&gpu.device, 800, 600, i as u32, false).map_err(at("pattern"))?;
        let frame = pipeline.convert(&source, (800, 600), format.fps).map_err(at("convert"))?;
        video.extend(encoder.encode(&frame, format.time(i)).map_err(at("encode"))?);
    }
    video.extend(encoder.finish().map_err(at("drain"))?);
    // a short ring, so the tape has to keep up for nothing to fall out of it
    let ring = Arc::new(Mutex::new(Ring::new(SECOND)));
    let rings: Vec<_> = lanes.iter().map(|_| Arc::new(Mutex::new(Ring::new(SECOND)))).collect();
    let path = dir.join("tape.mp4");
    let mut sent = vec![0; lanes.len()];
    let mut feed = |upto: usize| {
        let mut ring = ring.lock().expect("ring poisoned");
        let mut newest = 0;
        for p in video.drain(..upto.min(video.len())) {
            newest = p.pts;
            ring.push(p);
        }
        for ((lane, sent), out) in lanes.iter().zip(&mut sent).zip(&rings) {
            let mut out = out.lock().expect("ring poisoned");
            while let Some(p) = lane.get(*sent).filter(|p| p.pts <= newest) {
                out.push(ring::Packet { pts: p.pts, duration: p.duration, key: p.key, data: p.data.clone() });
                *sent += 1;
            }
        }
        !video.is_empty()
    };
    feed(5);
    let first = ring.lock().expect("ring poisoned").since(i64::MIN)[0].clone();
    let mut tape = Tape {
        writer: save::Writer::open(&path, &first, format, rings.len()).map_err(at("open tape"))?,
        ring: ring.clone(),
        rings: rings.clone(),
        names: vec!["tone 440".into(), "tone 660".into()],
        mastering: None,
        video: first.pts - 1,
        audio: vec![first.pts - 1; rings.len()],
        start: first.pts,
        frames: 0,
        bytes: 0,
    };
    for chunk in [1, 7, 3, 12, 30, 2] {
        tape.pull().map_err(fail)?;
        feed(chunk);
    }
    while feed(8) {
        tape.pull().map_err(fail)?;
    }
    let saved = tape.finish().map_err(fail)?;
    let streams = read_back(&path).map_err(at("read back"))?;
    let end = format.time(frames - 1);
    let sound: Vec<usize> = lanes.iter().map(|lane| lane.iter().filter(|p| p.pts <= end).count()).collect();
    match streams.as_slice() {
        [(count, true, last), heard @ ..]
            if *count as i64 == frames
                && saved.frames as i64 == frames
                && *last == end
                && heard.iter().map(|s| s.0 as usize).eq(sound.iter().copied()) =>
        {
            Ok(format!("{count} frames and aac packets {sound:?} written in pulls, none twice or missing"))
        }
        other => Err(fail(format!("want {frames} frames to {end} and aac packets {sound:?}, got {other:?}"))),
    }
}

/// Writes a screenshot and checks the png's header.
fn check_png(dir: &std::path::Path) -> Result<String> {
    let path = dir.join("shot.png");
    let (width, height) = (33, 7);
    let pixels: Vec<u8> = (0..width * height * 4).map(|i| (i * 7) as u8).collect();
    focus::png(width, height, &pixels, &path).map_err(at("write"))?;
    let bytes = std::fs::read(&path).map_err(|e| windows::core::Error::new(E_SELF_TEST, e.to_string()))?;
    let be = |at: usize| u32::from_be_bytes(bytes[at..at + 4].try_into().expect("four bytes"));
    // ihdr: width, height, then 8 bits a channel in color type 2, rgb with no alpha
    match bytes.get(..8) == Some(b"\x89PNG\r\n\x1a\n")
        && (be(16), be(20), bytes[24], bytes[25]) == (width, height, 8, 2)
    {
        true => Ok(format!("{width}x{height} rgb, {} bytes", bytes.len())),
        false => Err(windows::core::Error::new(E_SELF_TEST, format!("bad header {:?}", &bytes[..26.min(bytes.len())]))),
    }
}

fn at(stage: &'static str) -> impl Fn(windows::core::Error) -> windows::core::Error {
    move |e| windows::core::Error::new(e.code(), format!("{stage}: {}", e.message()))
}

const E_SELF_TEST: windows::core::HRESULT = windows::core::HRESULT(0x8000_4005_u32 as i32);

const TEST_DISPLAY: Display = Display { hdr: true, sdr_white: 200.0, peak: 1000.0 };
const SDR_DISPLAY: Display = Display { hdr: false, sdr_white: hdr::SCRGB_WHITE, peak: hdr::SCRGB_WHITE };
const MID_GREY: u8 = 128;
const TEST_PANEL: hdr::Mastering = hdr::Mastering {
    primaries: [[0.68, 0.32], [0.265, 0.69], [0.15, 0.06], [0.3127, 0.329]],
    min: 0.05,
    max: TEST_DISPLAY.peak,
    average: 400.0,
};

/// How the compositor stores an sRGB value on an HDR desktop: linear, scaled up to the sdr white level.
fn composed(srgb: u8, display: Display) -> f32 {
    let v = srgb as f32 / 255.0;
    let linear = if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    linear * display.sdr_white / hdr::SCRGB_WHITE
}

/// Every filter must hand a frame back unchanged at 1:1, and area must average a 4:1 downscale of
/// one white column in four to a flat quarter grey with no aliasing.
fn check_scale(gpu: &capture::Gpu) -> Result<String> {
    let source = pattern(&gpu.device, 64, 64, 0, false)?;
    let (original, _) = read_texture(gpu, &source)?;
    let full = RECT { left: 0, top: 0, right: 64, bottom: 64 };
    let mut report = Vec::new();
    let mut good = true;
    for filter in [hdr::Filter::Bilinear, hdr::Filter::Area, hdr::Filter::Bicubic, hdr::Filter::Lanczos] {
        let mut scaler = hdr::Scaler::new(&gpu.device, filter, (64, 64), DXGI_FORMAT_B8G8R8A8_UNORM)?;
        let (copy, _) = read_texture(gpu, &scaler.apply(&source, (64, 64), full)?)?;
        let off = original.iter().zip(&copy).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0);
        good &= off <= 1;
        report.push(format!("{filter:?} 1:1 off by {off}"));
    }
    let stripes: Vec<u8> = (0..64 * 256).flat_map(|i| if i % 4 == 0 { [255; 4] } else { [0, 0, 0, 255] }).collect();
    let stripes = upload(&gpu.device, 256, 64, DXGI_FORMAT_B8G8R8A8_UNORM, &stripes)?;
    let mut scaler = hdr::Scaler::new(&gpu.device, hdr::Filter::Area, (64, 16), DXGI_FORMAT_B8G8R8A8_UNORM)?;
    let quarter = RECT { left: 0, top: 0, right: 64, bottom: 16 };
    let (bytes, pitch) = read_texture(gpu, &scaler.apply(&stripes, (256, 64), quarter)?)?;
    let blue: Vec<u8> = (0..16).flat_map(|y| (0..64).map(move |x| y * pitch + x * 4)).map(|i| bytes[i]).collect();
    let (low, high) = (blue.iter().min().copied().unwrap_or(0), blue.iter().max().copied().unwrap_or(255));
    good &= low >= 63 && high <= 65;
    report.push(format!("area 4:1 stripes -> {low}..{high} (want 64)"));
    let report = report.join(", ");
    if good {
        Ok(report)
    } else {
        Err(windows::core::Error::new(E_SELF_TEST, report))
    }
}

/// Runs the hdr pattern through the tonemap and the PQ pass and reads back the values that matter:
/// sdr grey keeps its value, a 250 nit highlight lands below clipping, the panel peak reaches white,
/// and in hdr10 grey and peak sit at their PQ codes with neutral chroma. Plain sRGB grey, as frames
/// come once hdr is switched off, must land on the same hdr10 code as the composed grey.
fn check_transfer(gpu: &capture::Gpu) -> Result<String> {
    let source = pattern(&gpu.device, 64, 64, 0, true)?;
    let sdr = read_texture(gpu, &hdr::Tonemap::new(&gpu.device, TEST_DISPLAY)?.apply(&source)?)?;
    let full = RECT { left: 0, top: 0, right: 64, bottom: 64 };
    let mut pq_pass = hdr::Pq::new(&gpu.device, (64, 64), 1)?;
    let p010 = read_texture(gpu, &pq_pass.convert(&source, (64, 64), full, None)?)?;
    let flat =
        upload(&gpu.device, 64, 64, DXGI_FORMAT_B8G8R8A8_UNORM, &[MID_GREY, MID_GREY, MID_GREY, 255].repeat(64 * 64))?;
    let plain = read_texture(gpu, &pq_pass.convert(&flat, (64, 64), full, Some(TEST_DISPLAY.sdr_white))?)?;
    let plain_grey = u16::from_le_bytes([plain.0[32 * plain.1 + 32], plain.0[32 * plain.1 + 33]]) as u32 >> 6;
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
         hdr10 grey y {pq_grey} (want {want_grey}) cb cr {neutral:?}, peak y {pq_peak} (want {want_peak}), \
         sdr grey into hdr10 y {plain_grey}"
    );
    let good = grey.abs_diff(MID_GREY as u32) <= 2
        && (236..255).contains(&highlight)
        && white >= 254
        && pq_grey.abs_diff(want_grey) <= 1
        && plain_grey.abs_diff(want_grey) <= 1
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
    upload(device, width, height, format, &bytes)
}

fn upload(
    device: &ID3D11Device,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    bytes: &[u8],
) -> Result<ID3D11Texture2D> {
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

type Streams = Vec<(u32, bool, i64)>;

fn frame_size(path: &std::path::Path) -> Result<(u32, u32)> {
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        export::picture(&reader.GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)?)
    }
}

/// The mean luma of the first frame at or after `from` seconds, from 0 to 1 of the code range.
fn luma(path: &std::path::Path, from: f64) -> Result<f64> {
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        let stream = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let native = reader.GetNativeMediaType(stream, 0)?;
        let (width, height) = export::picture(&native)?;
        let ten = native.GetGUID(&MF_MT_SUBTYPE)? == MFVideoFormat_HEVC;
        let decoded = MFCreateMediaType()?;
        decoded.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        decoded.SetGUID(&MF_MT_SUBTYPE, if ten { &MFVideoFormat_P010 } else { &MFVideoFormat_NV12 })?;
        reader.SetCurrentMediaType(stream, None, &decoded)?;
        let from = (from * SECOND as f64) as i64;
        loop {
            let (mut flags, mut time, mut sample) = (0, 0, None);
            reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Err(windows::core::Error::new(E_SELF_TEST, "no frame that late"));
            }
            let Some(sample) = sample.filter(|_| time >= from) else { continue };
            let buffer: IMF2DBuffer = sample.ConvertToContiguousBuffer()?.cast()?;
            let (mut first, mut pitch) = (std::ptr::null_mut(), 0);
            buffer.Lock2D(&mut first, &mut pitch)?;
            let row = |y: u32| first.offset(y as isize * pitch as isize);
            let sum: u64 = (0..height)
                .map(|y| match ten {
                    // p010 keeps its 10 bits at the top of each 16
                    true => std::slice::from_raw_parts(row(y) as *const u16, width as usize)
                        .iter()
                        .map(|&v| (v >> 6) as u64)
                        .sum::<u64>(),
                    false => std::slice::from_raw_parts(row(y), width as usize).iter().map(|&v| v as u64).sum(),
                })
                .sum();
            buffer.Unlock2D()?;
            let top = if ten { 1023.0 } else { 255.0 };
            return Ok(sum as f64 / (width as u64 * height as u64) as f64 / top);
        }
    }
}

/// Every stream's sample count, whether it starts on a keyframe, and its last timestamp: the video
/// stream first, then the audio streams in file order.
fn read_back(path: &std::path::Path) -> Result<Streams> {
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

/// A second of silence then a second of sine tone through the aac encoder in 10 ms chunks, as a capture lane delivers them.
fn tone(hz: f32) -> Result<Vec<Arc<ring::Packet>>> {
    let mut aac = audio::Aac::new()?;
    let mut ring = Ring::new(10 * SECOND);
    let chunk = audio::RATE / 100;
    for start in (0..2 * audio::RATE).step_by(chunk as usize) {
        let pcm: Vec<u8> = (start..start + chunk)
            .map(|n| {
                if n < audio::RATE {
                    0
                } else {
                    ((n as f32 * hz * std::f32::consts::TAU / audio::RATE as f32).sin() * 8000.0) as i16
                }
            })
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

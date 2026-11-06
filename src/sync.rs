use std::sync::mpsc;
use std::time::{Duration, Instant};

use windows::core::{w, Result, HSTRING};
use windows::Win32::Foundation::{COLORREF, E_FAIL, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmFlush, DwmGetCompositionTimingInfo, DWM_TIMING_INFO};
use windows::Win32::Graphics::Gdi::{
    GetStockObject, InvalidateRect, UpdateWindow, BLACK_BRUSH, GET_STOCK_OBJECT_FLAGS, HBRUSH, WHITE_BRUSH,
};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::audio::{self, Lane, Source, BLOCK, CHANNELS, RATE};
use crate::capture::Target;
use crate::ring::{Clock, SECOND};

const EVENTS: usize = 6;
const FIRST: Duration = Duration::from_millis(1500);
const SPACING: Duration = Duration::from_secs(1);
const LIT: Duration = Duration::from_millis(250);
const SIDE: i32 = 400;
// -72 dbfs, inaudible at any sane volume yet plain against the exact silence of our own loopback
const LOUD: i16 = 8;
const HEARD: i16 = 4;
const CLICK: u64 = RATE as u64 / 50;
// a sound starts a new click only after this much quiet
const QUIET: i64 = SECOND * 3 / 10;
// a measured onset farther than this from its mark belongs to another event
const REACH: i64 = SECOND / 2;

/// Clock times each flash was handed to the compositor and each click played at the output.
struct Marks {
    flashes: Vec<i64>,
    clicks: Vec<i64>,
}

/// Records a test window and this process's own audio while the window flashes white and a faint
/// click plays, then reads the clip back and reports how far each landed from when it really happened.
/// The window is all but transparent, sits under every other window and never takes focus, and the
/// click is far below anything audible over a game, so this runs fine mid match.
pub fn run(args: &[String]) -> std::result::Result<(), String> {
    let out = std::env::temp_dir().join(format!("memory-sync-{}.mp4", std::process::id()));
    let lanes = [Lane { name: "sync".into(), source: Source::Process(std::process::id()) }];
    let (opened, window) = mpsc::channel();
    let (go, begun) = mpsc::channel();
    let stage = std::thread::spawn(move || stage(&opened, &begun));
    let window = window.recv().map_err(|_| "the test window thread quit".to_string())?;
    let window = HWND(window.map_err(|e| format!("can't open the test window: {e}"))? as *mut _);
    let work = move |clock| {
        let _ = go.send(clock);
        stage.join()
    };
    // a flash can only land on a captured frame, so capture every frame the compositor shows
    let mut args = args.to_vec();
    if let Some(rate) = refresh_rate().filter(|_| !args.iter().any(|a| a == "--fps")) {
        args.extend(["--fps".into(), rate.to_string()]);
    }
    let (marks, start) = crate::record_clip(&args, Target::Window(window), &lanes, &out, work)?;
    let read = onsets(&out);
    let _ = std::fs::remove_file(&out);
    let marks = marks.map_err(|_| "the test window thread panicked".to_string())?;
    let marks = marks.map_err(|e| format!("sync test failed: {e}"))?;
    let (whites, sounds) = read.map_err(|e| format!("can't read the clip back: {e}"))?;
    let ms = |ticks: i64| ticks as f64 / 1e4;
    let (mut video, mut sound, mut gap) = (Vec::new(), Vec::new(), Vec::new());
    println!("event  video ms  audio ms  audio behind video ms");
    for (n, (&flash, &click)) in marks.flashes.iter().zip(&marks.clicks).enumerate() {
        let v = nearest(&whites, flash - start);
        let a = nearest(&sounds, click - start);
        let show = |d: Option<i64>| d.map_or("missed".into(), |d| format!("{:+.1}", ms(d)));
        println!("{n:>5}  {:>8}  {:>8}  {:>8}", show(v), show(a), show(a.zip(v).map(|(a, v)| a - v)));
        video.extend(v);
        sound.extend(a);
        gap.extend(a.zip(v).map(|(a, v)| a - v));
    }
    if gap.is_empty() {
        return Err("no event was seen in both the picture and the sound".into());
    }
    println!(
        "median: video {:+.1} ms, audio {:+.1} ms, audio behind video {:+.1} ms",
        ms(median(&mut video)),
        ms(median(&mut sound)),
        ms(median(&mut gap))
    );
    Ok(())
}

fn refresh_rate() -> Option<u32> {
    let mut info = DWM_TIMING_INFO { cbSize: size_of::<DWM_TIMING_INFO>() as u32, ..Default::default() };
    unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut info) }.ok()?;
    let rate = info.rateRefresh;
    (rate.uiDenominator > 0).then(|| (rate.uiNumerator as f64 / rate.uiDenominator as f64).round() as u32)
}

fn nearest(onsets: &[i64], mark: i64) -> Option<i64> {
    onsets.iter().map(|&t| t - mark).filter(|d| d.abs() < REACH).min_by_key(|d| d.abs())
}

fn median(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    values.get(values.len() / 2).copied().unwrap_or(0)
}

/// Opens the test window, hands its handle over as an integer since a handle can't cross threads,
/// and flashes it once recording has begun.
fn stage(opened: &mpsc::Sender<Result<isize>>, begun: &mpsc::Receiver<Clock>) -> Result<Marks> {
    let window = match open_window() {
        Ok(window) => window,
        Err(e) => {
            let _ = opened.send(Err(e.clone()));
            return Err(e);
        }
    };
    let _ = opened.send(Ok(window.0 as isize));
    let staged = loop {
        pump();
        match begun.recv_timeout(Duration::from_millis(10)) {
            Ok(clock) => break flash_and_click(clock, window),
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break Err(windows::core::Error::from(E_FAIL)),
        }
    };
    let _ = unsafe { DestroyWindow(window) };
    staged
}

fn open_window() -> Result<HWND> {
    unsafe {
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            lpszClassName: w!("memory-sync"),
            hbrBackground: HBRUSH(GetStockObject(BLACK_BRUSH).0),
            ..Default::default()
        };
        RegisterClassW(&class);
        // nearly transparent and under every other window, yet captured at full brightness
        let window = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("memory-sync"),
            w!("memory sync"),
            WS_POPUP,
            0,
            0,
            SIDE,
            SIDE,
            None,
            None,
            None,
            None,
        )?;
        SetLayeredWindowAttributes(window, COLORREF(0), 1, LWA_ALPHA)?;
        SetWindowPos(window, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_SHOWWINDOW)?;
        Ok(window)
    }
}

fn pump() {
    unsafe {
        let mut message = MSG::default();
        while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
            DispatchMessageW(&message);
        }
    }
}

fn flash_and_click(clock: Clock, window: HWND) -> Result<Marks> {
    let mut speaker = Speaker::open()?;
    let mut marks = Marks { flashes: Vec::new(), clicks: Vec::new() };
    let started = Instant::now();
    let mut clicked = None;
    while marks.clicks.len() < EVENTS || started.elapsed() < FIRST + SPACING * EVENTS as u32 {
        pump();
        let due = FIRST + SPACING * marks.clicks.len() as u32;
        let elapsed = started.elapsed();
        match clicked {
            None if marks.clicks.len() < EVENTS && elapsed >= due => {
                clicked = Some(speaker.click());
                speaker.feed()?;
                paint(window, WHITE_BRUSH)?;
                marks.flashes.push(clock.now());
            }
            Some(frame) if elapsed >= due + LIT => {
                paint(window, BLACK_BRUSH)?;
                marks.clicks.push(clock.at(speaker.when(frame, clock)?));
                clicked = None;
            }
            _ => speaker.feed()?,
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Ok(marks)
}

fn paint(window: HWND, brush: GET_STOCK_OBJECT_FLAGS) -> Result<()> {
    unsafe {
        SetClassLongPtrW(window, GCLP_HBRBACKGROUND, GetStockObject(brush).0 as isize);
        let _ = InvalidateRect(Some(window), None, true);
        let _ = UpdateWindow(window);
        // returns after the next composition, which can land a frame before the one showing the paint
        DwmFlush()
    }
}

unsafe extern "system" fn window_proc(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

/// A shared mode stream on the default output that plays silence until asked for a click, counting
/// every frame it writes so the click's stream position is exact.
struct Speaker {
    client: IAudioClient,
    render: IAudioRenderClient,
    clock: IAudioClock,
    size: u32,
    written: u64,
    click: Option<u64>,
}

impl Speaker {
    fn open() -> Result<Self> {
        unsafe {
            let devices: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let client: IAudioClient =
                devices.GetDefaultAudioEndpoint(eRender, eConsole)?.Activate(CLSCTX_ALL, None)?;
            let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, SECOND / 10, 0, &audio::wave(), None)?;
            // windows keeps an app's mute across runs, and a muted session reaches the loopback silent
            if client.GetService::<ISimpleAudioVolume>()?.GetMute()?.as_bool() {
                return Err(windows::core::Error::new(E_FAIL, "memory is muted in the volume mixer"));
            }
            let mut speaker = Self {
                size: client.GetBufferSize()?,
                render: client.GetService()?,
                clock: client.GetService()?,
                client,
                written: 0,
                click: None,
            };
            speaker.feed()?;
            speaker.client.Start()?;
            Ok(speaker)
        }
    }

    /// The click starts at the next frame written, and this is that frame.
    fn click(&mut self) -> u64 {
        self.click = Some(self.written);
        self.written
    }

    fn feed(&mut self) -> Result<()> {
        unsafe {
            let free = self.size - self.client.GetCurrentPadding()?;
            if free == 0 {
                return Ok(());
            }
            let data = self.render.GetBuffer(free)?;
            let samples = std::slice::from_raw_parts_mut(data as *mut i16, free as usize * CHANNELS as usize);
            for (offset, frame) in samples.as_chunks_mut::<{ CHANNELS as usize }>().0.iter_mut().enumerate() {
                let n = self.written + offset as u64;
                let value = match self.click {
                    Some(start) if (start..start + CLICK).contains(&n) => {
                        if ((n - start) * 2000 / RATE as u64).is_multiple_of(2) {
                            LOUD
                        } else {
                            -LOUD
                        }
                    }
                    _ => 0,
                };
                frame.fill(value);
            }
            self.render.ReleaseBuffer(free, 0)?;
            self.written += free as u64;
            Ok(())
        }
    }

    /// The performance counter time, in 100 ns units, at which stream frame `frame` plays.
    fn when(&self, frame: u64, clock: Clock) -> Result<i64> {
        unsafe {
            let (mut position, mut qpc) = (0, 0);
            self.clock.GetPosition(&mut position, Some(&mut qpc))?;
            let played = position as i128 * RATE as i128 / self.clock.GetFrequency()? as i128;
            if (clock.at(qpc as i64) - clock.now()).abs() > SECOND {
                return Err(windows::core::Error::new(E_FAIL, "audio clock is not on the performance counter"));
            }
            Ok(qpc as i64 + ((frame as i128 - played) * SECOND as i128 / RATE as i128) as i64)
        }
    }
}

impl Drop for Speaker {
    fn drop(&mut self) {
        let _ = unsafe { self.client.Stop() };
    }
}

/// When the centre of the picture turns bright and when the sound starts after a quiet stretch,
/// both in clip time.
pub fn onsets(path: &std::path::Path) -> Result<(Vec<i64>, Vec<i64>)> {
    unsafe {
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(path.as_os_str()), None)?;
        let video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;
        let sound = *crate::export::audio_streams(&reader, path)?.first().ok_or(E_FAIL)?;
        let nv12 = MFCreateMediaType()?;
        nv12.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        nv12.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        reader.SetCurrentMediaType(video, None, &nv12)?;
        reader.SetCurrentMediaType(sound, None, &audio::audio_type(MFAudioFormat_PCM)?)?;
        let current = reader.GetCurrentMediaType(video)?;
        let size = current.GetUINT64(&MF_MT_FRAME_SIZE)?;
        let (width, height) = ((size >> 32) as usize, (size & 0xffff_ffff) as usize);
        let stride = current.GetUINT32(&MF_MT_DEFAULT_STRIDE).map_or(width, |s| s as i32 as usize);

        let (mut whites, mut lit) = (Vec::new(), false);
        each_sample(&reader, video, |time, luma| {
            let rows = height / 2 - 16..height / 2 + 16;
            let total: u32 = rows.flat_map(|row| &luma[row * stride + width / 2 - 16..][..32]).map(|&y| y as u32).sum();
            let bright = total / (32 * 32) > 128;
            if bright && !lit {
                whites.push(time);
            }
            lit = bright;
        })?;

        let (mut sounds, mut heard) = (Vec::new(), None);
        each_sample(&reader, sound, |time, pcm| {
            for (n, frame) in pcm.as_chunks::<BLOCK>().0.iter().enumerate() {
                let at = time + n as i64 * SECOND / RATE as i64;
                if frame.as_chunks::<2>().0.iter().any(|&s| i16::from_le_bytes(s).unsigned_abs() > HEARD as u16) {
                    if heard.is_none_or(|heard| at - heard > QUIET) {
                        sounds.push(at);
                    }
                    heard = Some(at);
                }
            }
        })?;
        Ok((whites, sounds))
    }
}

pub fn each_sample(reader: &IMFSourceReader, stream: u32, mut visit: impl FnMut(i64, &[u8])) -> Result<()> {
    unsafe {
        reader.SetStreamSelection(MF_SOURCE_READER_ALL_STREAMS.0 as u32, false)?;
        reader.SetStreamSelection(stream, true)?;
        reader.SetCurrentPosition(
            &windows::core::GUID::zeroed(),
            &windows::Win32::System::Com::StructuredStorage::PROPVARIANT::from(0i64),
        )?;
        loop {
            let (mut flags, mut time, mut sample) = (0, 0, None);
            reader.ReadSample(stream, 0, None, Some(&mut flags), Some(&mut time), Some(&mut sample))?;
            if flags & MF_SOURCE_READERF_ENDOFSTREAM.0 as u32 != 0 {
                return Ok(());
            }
            let Some(sample) = sample else { continue };
            let buffer = sample.ConvertToContiguousBuffer()?;
            let (mut data, mut length) = (std::ptr::null_mut(), 0);
            buffer.Lock(&mut data, None, Some(&mut length))?;
            visit(time, std::slice::from_raw_parts(data, length as usize));
            buffer.Unlock()?;
        }
    }
}

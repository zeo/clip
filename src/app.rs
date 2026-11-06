use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewWindowBuilder};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HWND, LPARAM, WPARAM};
use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_NOREPEAT, VK_F9};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, IsWindow, MessageBoxW, PeekMessageW, PostThreadMessageW, SetTimer, MB_ICONERROR, MB_ICONHAND, MSG,
    PM_NOREMOVE, WM_APP, WM_HOTKEY, WM_TIMER,
};

use crate::capture::Target;
use crate::focus::{self, Follow};
use crate::library::{self, Clip, Marks};
use crate::{hdr, Options, Session};

// capscr's shutter played backwards, so the two apps are told apart by ear
static SAVED: &[u8] = include_bytes!("../assets/saved.wav");
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const WM_RESTART: u32 = WM_APP;
// an app left a moment ago keeps recording this long, so alt+tabbing back finds its buffer whole
const GRACE: Duration = Duration::from_secs(60);

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    buffer: u32,
    fps: u32,
    height: u32,
    mbps: u32,
    hdr10: bool,
    mic: bool,
    /// exe names that get a track each
    apps: Vec<String>,
    folder: PathBuf,
    start_with_windows: bool,
    /// whole gigabytes the clips folder may hold, 0 for no limit
    cap: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            buffer: 30,
            fps: 60,
            height: 1080,
            mbps: 20,
            hdr10: false,
            mic: false,
            apps: Vec::new(),
            folder: PathBuf::new(),
            start_with_windows: false,
            cap: 0,
        }
    }
}

impl Settings {
    fn options(&self) -> Options {
        Options {
            buffer: self.buffer.clamp(5, 600),
            fps: self.fps,
            mbps: self.mbps,
            height: self.height,
            filter: hdr::Filter::Area,
            software: false,
            hdr10: self.hdr10,
        }
    }

    /// Whether moving from `self` to `next` needs a new session.
    fn records_differently(&self, next: &Self) -> bool {
        (self.buffer, self.fps, self.height, self.mbps, self.hdr10, self.mic, &self.apps)
            != (next.buffer, next.fps, next.height, next.mbps, next.hdr10, next.mic, &next.apps)
    }
}

#[derive(Serialize, Clone, Default)]
struct Status {
    recording: bool,
    /// the app being recorded, or desktop
    app: String,
    message: String,
}

struct Shared {
    settings: Arc<Mutex<Settings>>,
    status: Arc<Mutex<Status>>,
    file: PathBuf,
    marks: Arc<Mutex<Marks>>,
    marks_file: PathBuf,
    /// stills and decoded audio for the window, safe to lose
    cache: PathBuf,
    /// the run value, and with a profile the settings folder too
    name: String,
    first: AtomicBool,
    recorder: u32,
}

/// Runs memory from the tray, keeping the last stretch of the focused app ready to save.
pub fn run() {
    if let Err(e) = serve() {
        unsafe { MessageBoxW(None, &HSTRING::from(e), w!("memory"), MB_ICONERROR) };
    }
}

fn serve() -> Result<(), String> {
    // below normal, not background mode, which would also let windows page out the buffer
    let _ = unsafe { SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) };
    // a profile runs a separate memory beside the usual one, with its own settings and single instance
    let name = match std::env::var("MEMORY_PROFILE") {
        Ok(p) if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') => format!("memory-{p}"),
        _ => "memory".to_string(),
    };
    let mut context = tauri::generate_context!();
    context.config_mut().identifier = format!("lt.rot.{name}");
    tauri::Builder::default()
        // a second copy would record everything twice, so launching again opens the window instead
        .plugin(tauri_plugin_single_instance::init(|app, _, _| show(app)))
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            settings,
            status,
            first_run,
            apply,
            clips,
            save_clip,
            open,
            reveal,
            audio_apps,
            star,
            rename,
            delete,
            save_thumb,
            track_audio,
            export
        ])
        .setup(move |app| {
            let dir = app.path().config_dir()?.join(&name);
            let file = dir.join("settings.json");
            let marks_file = dir.join("library.json");
            let marks = Arc::new(Mutex::new(Marks::load(&marks_file)));
            let cache = app.path().cache_dir()?.join(&name);
            app.asset_protocol_scope().allow_directory(&cache, true)?;
            let stored = std::fs::read_to_string(&file).ok();
            let mut settings: Settings =
                stored.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
            if settings.folder.as_os_str().is_empty() {
                settings.folder = app.path().video_dir()?.join("memory");
            }
            app.asset_protocol_scope().allow_directory(&settings.folder, true)?;
            let menu = Menu::with_items(
                app,
                &[
                    &MenuItem::with_id(app, "save", "save clip   alt+f9", true, None::<&str>)?,
                    &MenuItem::with_id(app, "show", "open memory", true, None::<&str>)?,
                    &MenuItem::with_id(app, "folder", "open clips folder", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "quit", "quit", true, None::<&str>)?,
                ],
            )?;
            let mut tray = TrayIconBuilder::with_id("memory")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("memory: starting")
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left, button_state: MouseButtonState::Up, ..
                    } = event
                    {
                        show(tray.app_handle());
                    }
                });
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            let tray = tray.build(app)?;
            let settings = Arc::new(Mutex::new(settings));
            let status = Arc::new(Mutex::new(Status::default()));
            let (ready, recorder) = mpsc::channel();
            {
                let (app, settings, status) = (app.handle().clone(), settings.clone(), status.clone());
                let library = (marks.clone(), cache.join("thumbs"));
                std::thread::spawn(move || record(&app, &tray, &settings, &status, &library, ready));
            }
            let first = stored.is_none();
            let recorder = recorder.recv()?;
            app.manage(Shared {
                settings,
                status,
                file,
                marks,
                marks_file,
                cache,
                name: name.clone(),
                first: AtomicBool::new(first),
                recorder,
            });
            app.on_menu_event(|app, event| {
                let shared = app.state::<Shared>();
                match event.id.as_ref() {
                    "save" => post(shared.recorder, WM_HOTKEY),
                    "show" => show(app),
                    "folder" => {
                        let folder = shared.settings.lock().expect("settings poisoned").folder.clone();
                        let _ = std::fs::create_dir_all(&folder);
                        let _ = Command::new("explorer").arg(folder).spawn();
                    }
                    "quit" => app.exit(0),
                    _ => {}
                }
            });
            if first {
                show(app.handle());
            }
            Ok(())
        })
        .build(context)
        .map_err(|e| e.to_string())?
        .run(|_, event| {
            // closing the window only hides memory in the tray, quitting from the tray passes a code
            if let RunEvent::ExitRequested { code: None, api, .. } = event {
                api.prevent_exit();
            }
        });
    Ok(())
}

fn post(thread: u32, message: u32) {
    let _ = unsafe { PostThreadMessageW(thread, message, WPARAM(0), LPARAM(0)) };
}

/// Brings the window up, creating it if it was closed.
fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
        return;
    }
    let app = app.clone();
    // building a webview inside an event handler deadlocks on windows
    std::thread::spawn(move || {
        if let Some(config) = app.config().app.windows.first() {
            let _ = WebviewWindowBuilder::from_config(&app, config).and_then(|window| window.build());
        }
    });
}

/// A session and the app it records, or the primary monitor when `window` is none.
struct Recording {
    session: Session,
    window: Option<HWND>,
    app: String,
}

/// Records on this thread's message loop, following the focused app, saving a clip on every
/// WM_HOTKEY and starting over on WM_RESTART.
fn record(
    app: &AppHandle,
    tray: &TrayIcon,
    settings: &Mutex<Settings>,
    status: &Mutex<Status>,
    (marks, thumbs): &(Arc<Mutex<Marks>>, PathBuf),
    ready: mpsc::Sender<u32>,
) {
    let mut msg = MSG::default();
    // the queue has to exist before anything is posted to it
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    let _ = ready.send(unsafe { GetCurrentThreadId() });
    let hotkey = unsafe { RegisterHotKey(None, 1, MOD_ALT | MOD_NOREPEAT, VK_F9.0 as u32) }.is_ok();
    unsafe { SetTimer(None, 0, 250, None) };
    let media = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
        .and_then(|()| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) })
        .map_err(|e| e.to_string());
    let begin = |window: Option<HWND>| -> Result<Recording, String> {
        media.clone()?;
        let current = settings.lock().expect("settings poisoned").clone();
        let (target, game, name) = match window {
            Some(window) => {
                let pid = focus::pid(window);
                (Target::Window(window), Some(pid), focus::app_name(pid))
            }
            None => (crate::primary(), None, "desktop".to_string()),
        };
        let lanes = crate::audio::lanes(game, &current.apps, current.mic);
        Ok(Recording { session: Session::start(&current.options(), target, &lanes)?, window, app: name })
    };
    let report = |recording: &Result<Recording, String>| {
        let buffer = settings.lock().expect("settings poisoned").options().buffer;
        let next = match recording {
            Err(e) => Status { recording: false, app: String::new(), message: e.clone() },
            Ok(recording) => Status {
                recording: true,
                app: recording.app.clone(),
                message: match hotkey {
                    true => format!("keeping the last {buffer} s, alt+f9 saves"),
                    false => format!("keeping the last {buffer} s, alt+f9 is taken by another app"),
                },
            },
        };
        let _ = tray.set_tooltip(Some(match next.recording {
            true => format!("memory: recording {}, {}", next.app, next.message),
            false => format!("memory: {}", next.message),
        }));
        let _ = app.emit("status", &next);
        *status.lock().expect("status poisoned") = next;
    };
    let mut current: Result<Recording, String> = Err("starting".into());
    let mut previous: Option<(Recording, Instant)> = None;
    let mut follow = Follow::default();
    let mut refused = None;
    let mut restart = true;
    loop {
        if restart {
            restart = false;
            // the old sessions let go of the encoder before the new one opens its own
            previous = None;
            drop(std::mem::replace(&mut current, Err("starting".into())));
            let window = focus::focused();
            current = begin(window).or_else(|e| if window.is_some() { begin(None) } else { Err(e) });
            report(&current);
        }
        if unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 <= 0 {
            break;
        }
        let recorded = current.as_ref().ok().and_then(|r| r.window);
        let back = previous.as_ref().and_then(|(r, _)| r.window);
        let next = match msg.message {
            WM_RESTART => {
                restart = true;
                None
            }
            WM_TIMER => {
                if previous.as_ref().is_some_and(|(_, left)| left.elapsed() > GRACE) {
                    previous = None;
                }
                if recorded.is_some_and(|w| !unsafe { IsWindow(Some(w)) }.as_bool()) {
                    Some(None)
                } else {
                    let focused = focus::focused().filter(|&w| Some(w) != refused);
                    follow.tick(focused, recorded, back, Instant::now()).map(Some)
                }
            }
            WM_HOTKEY => {
                // pressed straight after coming back to an app, the clip is of that app
                if back.is_some()
                    && focus::focused() == back
                    && switch(back, &mut current, &mut previous, &begin).is_ok()
                {
                    report(&current);
                }
                let (folder, cap) = {
                    let settings = settings.lock().expect("settings poisoned");
                    (settings.folder.clone(), settings.cap)
                };
                let saved = current.as_ref().map_err(Clone::clone).and_then(|recording| {
                    let folder = folder.join(&recording.app);
                    std::fs::create_dir_all(&folder).map_err(|e| format!("can't make {}: {e}", folder.display()))?;
                    recording.session.save(&folder.join(stamp()))
                });
                match &saved {
                    Ok(_) => unsafe {
                        let _ = PlaySoundW(PCWSTR(SAVED.as_ptr().cast()), None, SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
                    },
                    Err(_) => {
                        let _ = unsafe { MessageBeep(MB_ICONHAND) };
                    }
                }
                match saved {
                    Ok(_) => {
                        if cap > 0 {
                            let marks = marks.lock().expect("marks poisoned");
                            library::trim_to(&library::list(&folder, &marks, thumbs), cap as u64 * 1_000_000_000);
                        }
                        let _ = app.emit("clips", ());
                    }
                    Err(e) => {
                        let _ = tray.set_tooltip(Some(format!("memory: {e}")));
                    }
                }
                None
            }
            _ => None,
        };
        if let Some(window) = next {
            match switch(window, &mut current, &mut previous, &begin) {
                Ok(()) => refused = None,
                Err(_) if window.is_some() => refused = window,
                // with the app gone and the monitor failing too there is nothing left to record
                Err(e) => current = Err(e),
            }
            report(&current);
        }
    }
}

/// Moves recording to `window`, leaving the app it was on recording for a while in case focus
/// comes straight back.
fn switch(
    window: Option<HWND>,
    current: &mut Result<Recording, String>,
    previous: &mut Option<(Recording, Instant)>,
    begin: &impl Fn(Option<HWND>) -> Result<Recording, String>,
) -> Result<(), String> {
    let next = match previous.take() {
        Some((recording, _)) if window.is_some() && recording.window == window => recording,
        // two sessions at most, so the older one goes first
        _ => begin(window)?,
    };
    let left = std::mem::replace(current, Ok(next));
    *previous = left
        .ok()
        .filter(|left| left.window.is_some_and(|w| unsafe { IsWindow(Some(w)) }.as_bool()))
        .map(|left| (left, Instant::now()));
    Ok(())
}

fn stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{}-{:02}-{:02} {:02}-{:02}-{:02}.mp4", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn run_at_login(name: &str, on: bool) -> windows::core::Result<()> {
    let name = HSTRING::from(name);
    let result = if on {
        let exe = std::env::current_exe()?;
        let value = HSTRING::from(format!("\"{}\"", exe.display()));
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                &name,
                REG_SZ.0,
                Some(value.as_ptr().cast()),
                ((value.len() + 1) * 2) as u32,
            )
        }
    } else {
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, &name) } {
            ERROR_FILE_NOT_FOUND => return Ok(()),
            other => other,
        }
    };
    result.ok()
}

#[tauri::command]
fn settings(shared: State<Shared>) -> Settings {
    shared.settings.lock().expect("settings poisoned").clone()
}

#[tauri::command]
fn status(shared: State<Shared>) -> Status {
    shared.status.lock().expect("status poisoned").clone()
}

#[tauri::command]
fn first_run(shared: State<Shared>) -> bool {
    shared.first.load(Ordering::Relaxed)
}

#[tauri::command]
fn apply(app: AppHandle, shared: State<Shared>, settings: Settings) -> Result<(), String> {
    if settings.folder.as_os_str().is_empty() || !settings.folder.is_absolute() {
        return Err("pick a folder for clips".into());
    }
    std::fs::create_dir_all(&settings.folder).map_err(|e| format!("can't make {}: {e}", settings.folder.display()))?;
    let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    if let Some(dir) = shared.file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&shared.file, json).map_err(|e| format!("can't save settings: {e}"))?;
    run_at_login(&shared.name, settings.start_with_windows)
        .map_err(|e| format!("can't change start with windows: {e}"))?;
    app.asset_protocol_scope().allow_directory(&settings.folder, true).map_err(|e| e.to_string())?;
    shared.first.store(false, Ordering::Relaxed);
    let restart = {
        let mut current = shared.settings.lock().expect("settings poisoned");
        let restart = current.records_differently(&settings);
        *current = settings;
        restart
    };
    if restart {
        post(shared.recorder, WM_RESTART);
    }
    let _ = app.emit("clips", ());
    Ok(())
}

#[tauri::command]
fn clips(shared: State<Shared>) -> Vec<Clip> {
    let folder = shared.settings.lock().expect("settings poisoned").folder.clone();
    let thumbs = shared.cache.join("thumbs");
    let clips = library::list(&folder, &shared.marks.lock().expect("marks poisoned"), &thumbs);
    library::prune(&thumbs, &clips);
    clips
}

fn marked(shared: &Shared, change: impl FnOnce(&mut Marks, &Path)) -> Result<(), String> {
    let folder = shared.settings.lock().expect("settings poisoned").folder.clone();
    let mut marks = shared.marks.lock().expect("marks poisoned");
    change(&mut marks, &folder);
    marks.store(&shared.marks_file)
}

#[tauri::command]
fn star(app: AppHandle, shared: State<Shared>, path: PathBuf, on: bool) -> Result<(), String> {
    clip_in_folder(&shared, &path)?;
    marked(&shared, |marks, folder| marks.star(folder, &path, on))?;
    let _ = app.emit("clips", ());
    Ok(())
}

#[tauri::command]
fn rename(app: AppHandle, shared: State<Shared>, path: PathBuf, name: String) -> Result<PathBuf, String> {
    clip_in_folder(&shared, &path)?;
    let to = library::renamed(&path, &name)?;
    std::fs::rename(&path, &to).map_err(|e| format!("couldn't rename: {e}"))?;
    marked(&shared, |marks, folder| marks.moved(folder, &path, &to))?;
    let _ = app.emit("clips", ());
    Ok(to)
}

#[tauri::command]
fn delete(app: AppHandle, shared: State<Shared>, path: PathBuf) -> Result<(), String> {
    clip_in_folder(&shared, &path)?;
    library::recycle(&path)?;
    marked(&shared, |marks, folder| marks.star(folder, &path, false))?;
    let _ = app.emit("clips", ());
    Ok(())
}

#[tauri::command]
fn save_thumb(shared: State<Shared>, path: PathBuf, jpeg: Vec<u8>) -> Result<PathBuf, String> {
    clip_in_folder(&shared, &path)?;
    if !jpeg.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Err("a still has to be a jpeg".into());
    }
    let meta = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    let modified = meta.modified().map_err(|e| e.to_string())?;
    let modified = modified.duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?.as_millis() as u64;
    let thumbs = shared.cache.join("thumbs");
    let thumb = library::thumb(&thumbs, &path, modified, meta.len());
    std::fs::create_dir_all(&thumbs).map_err(|e| e.to_string())?;
    std::fs::write(&thumb, jpeg).map_err(|e| e.to_string())?;
    Ok(thumb)
}

#[derive(Serialize)]
struct Track {
    name: String,
    /// a wav file in the cache
    path: PathBuf,
}

/// Decodes every audio track of a clip to its own wav, since the webview only plays the first.
#[tauri::command]
async fn track_audio(shared: State<'_, Shared>, path: PathBuf) -> Result<Vec<Track>, String> {
    clip_in_folder(&shared, &path)?;
    let audio = shared.cache.join("audio");
    blocking(move || {
        let tracks = crate::export::tracks(&path)?;
        // only the clip open now keeps its decoded audio
        let _ = std::fs::remove_dir_all(&audio);
        std::fs::create_dir_all(&audio).map_err(|e| e.to_string())?;
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        tracks
            .into_iter()
            .enumerate()
            .map(|(i, (name, pcm))| {
                let wav = audio.join(format!("{stem} {i}.wav"));
                crate::export::write_wav(&wav, &pcm).map_err(|e| e.to_string())?;
                Ok(Track { name, path: wav })
            })
            .collect()
    })
    .await
}

/// Saves `start` to `end` seconds of a clip as a new clip beside it, with only the `tracks` asked
/// for, either mixed into one or kept apart.
#[tauri::command]
async fn export(
    app: AppHandle,
    shared: State<'_, Shared>,
    path: PathBuf,
    start: f64,
    end: f64,
    tracks: Vec<usize>,
    mix: bool,
) -> Result<PathBuf, String> {
    clip_in_folder(&shared, &path)?;
    if !(start >= 0.0 && end > start) {
        return Err("the end has to come after the start".into());
    }
    let out = blocking(move || {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let out = (1..)
            .map(|n| match n {
                1 => path.with_file_name(format!("{stem} trimmed.mp4")),
                n => path.with_file_name(format!("{stem} trimmed {n}.mp4")),
            })
            .find(|out| !out.exists())
            .expect("some name is free");
        crate::export::trim(&path, &out, start, end, &tracks, mix)?;
        Ok(out)
    })
    .await?;
    let _ = app.emit("clips", ());
    Ok(out)
}

/// Runs media foundation work off the webview's threads.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, String> + Send + 'static) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }.map_err(|e| e.to_string())?;
        work()
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
fn save_clip(shared: State<Shared>) {
    post(shared.recorder, WM_HOTKEY);
}

/// Only clips in the clips folder get handed to explorer, which would run anything else.
fn clip_in_folder(shared: &Shared, path: &Path) -> Result<(), String> {
    let folder = shared.settings.lock().expect("settings poisoned").folder.clone();
    let parent = path.parent();
    let inside = !path.components().any(|part| part == Component::ParentDir)
        && (parent == Some(folder.as_path()) || parent.and_then(Path::parent) == Some(folder.as_path()))
        && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4"))
        && path.is_file();
    if inside {
        Ok(())
    } else {
        Err(format!("{} is not a clip", path.display()))
    }
}

#[tauri::command]
fn open(shared: State<Shared>, path: PathBuf) -> Result<(), String> {
    clip_in_folder(&shared, &path)?;
    Command::new("explorer").arg(&path).spawn().map(drop).map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal(shared: State<Shared>, path: PathBuf) -> Result<(), String> {
    clip_in_folder(&shared, &path)?;
    // explorer reads /select, with the path quoted after the comma, which std's quoting can't produce
    Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", path.display()))
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn audio_apps() -> Result<Vec<String>, String> {
    crate::audio::playing().map_err(|e| e.to_string())
}

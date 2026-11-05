use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewWindowBuilder};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, ERROR_FILE_NOT_FOUND, LPARAM, WPARAM};
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_NOREPEAT, VK_F9};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, MessageBoxW, PeekMessageW, PostThreadMessageW, MB_ICONERROR, MB_ICONHAND, MB_OK, MSG, PM_NOREMOVE,
    WM_APP, WM_HOTKEY,
};

use crate::{hdr, Options, Session};

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const WM_RESTART: u32 = WM_APP;

#[derive(Serialize, Deserialize, Clone, PartialEq)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    buffer: u32,
    fps: u32,
    height: u32,
    mbps: u32,
    hdr10: bool,
    mic: bool,
    folder: PathBuf,
    start_with_windows: bool,
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
            folder: PathBuf::new(),
            start_with_windows: false,
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
        (self.buffer, self.fps, self.height, self.mbps, self.hdr10, self.mic)
            != (next.buffer, next.fps, next.height, next.mbps, next.hdr10, next.mic)
    }
}

#[derive(Serialize, Clone, Default)]
struct Status {
    recording: bool,
    message: String,
}

#[derive(Serialize)]
struct Clip {
    path: PathBuf,
    name: String,
    bytes: u64,
    /// milliseconds since the unix epoch
    modified: u64,
}

struct Shared {
    settings: Arc<Mutex<Settings>>,
    status: Arc<Mutex<Status>>,
    file: PathBuf,
    first: AtomicBool,
    recorder: u32,
}

/// Runs memory from the tray, keeping the last stretch of the primary monitor ready to save.
pub fn run() {
    if let Err(e) = serve() {
        unsafe { MessageBoxW(None, &HSTRING::from(e), w!("memory"), MB_ICONERROR) };
    }
}

fn serve() -> Result<(), String> {
    // a second copy would record everything twice
    let _single = unsafe { CreateMutexW(None, true, w!(r"Local\memory")) }.map_err(|e| e.to_string())?;
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(());
    }
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![settings, status, first_run, apply, clips, save_clip, open, reveal])
        .setup(|app| {
            let file = app.path().config_dir()?.join("memory").join("settings.json");
            let stored = std::fs::read_to_string(&file).ok();
            let mut settings: Settings =
                stored.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
            if settings.folder.as_os_str().is_empty() {
                settings.folder = app.path().video_dir()?.join("memory");
            }
            app.asset_protocol_scope().allow_directory(&settings.folder, false)?;
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
                std::thread::spawn(move || record(&app, &tray, &settings, &status, ready));
            }
            let first = stored.is_none();
            app.manage(Shared { settings, status, file, first: AtomicBool::new(first), recorder: recorder.recv()? });
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
        .build(tauri::generate_context!())
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

/// Records on this thread's message loop, saving a clip on every WM_HOTKEY and starting over on WM_RESTART.
fn record(
    app: &AppHandle,
    tray: &TrayIcon,
    settings: &Mutex<Settings>,
    status: &Mutex<Status>,
    ready: mpsc::Sender<u32>,
) {
    let mut msg = MSG::default();
    // the queue has to exist before anything is posted to it
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    let _ = ready.send(unsafe { GetCurrentThreadId() });
    let hotkey = unsafe { RegisterHotKey(None, 1, MOD_ALT | MOD_NOREPEAT, VK_F9.0 as u32) }.is_ok();
    let media = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
        .and_then(|()| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) })
        .map_err(|e| e.to_string());
    let report = |next: Status| {
        let _ = tray.set_tooltip(Some(format!("memory: {}", next.message)));
        let _ = app.emit("status", &next);
        *status.lock().expect("status poisoned") = next;
    };
    let mut session: Option<Result<Session, String>> = None;
    loop {
        if session.is_none() {
            let current = settings.lock().expect("settings poisoned").clone();
            let started = media.clone().and_then(|()| {
                Session::start(&current.options(), crate::primary(), &crate::audio::lanes(None, &[], current.mic))
            });
            report(match &started {
                Err(e) => Status { recording: false, message: e.clone() },
                Ok(_) if hotkey => Status {
                    recording: true,
                    message: format!("keeping the last {} s, alt+f9 saves", current.options().buffer),
                },
                Ok(_) => Status {
                    recording: true,
                    message: format!("keeping the last {} s, alt+f9 is taken by another app", current.options().buffer),
                },
            });
            session = Some(started);
        }
        if unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 <= 0 {
            break;
        }
        match msg.message {
            WM_RESTART => {
                // the old session lets go of the encoder before the new one opens its own
                session = None;
            }
            WM_HOTKEY => {
                let folder = settings.lock().expect("settings poisoned").folder.clone();
                let saved = session.as_ref().expect("session started above").as_ref().map_err(Clone::clone).and_then(
                    |session| {
                        std::fs::create_dir_all(&folder)
                            .map_err(|e| format!("can't make {}: {e}", folder.display()))?;
                        session.save(&folder.join(stamp()))
                    },
                );
                unsafe { MessageBeep(if saved.is_ok() { MB_OK } else { MB_ICONHAND }) }.ok();
                match saved {
                    Ok(_) => {
                        let _ = app.emit("clips", ());
                    }
                    Err(e) => {
                        let _ = tray.set_tooltip(Some(format!("memory: {e}")));
                    }
                }
            }
            _ => {}
        }
    }
}

fn stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{}-{:02}-{:02} {:02}-{:02}-{:02}.mp4", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

fn run_at_login(on: bool) -> windows::core::Result<()> {
    let result = if on {
        let exe = std::env::current_exe()?;
        let value = HSTRING::from(format!("\"{}\"", exe.display()));
        unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                w!("memory"),
                REG_SZ.0,
                Some(value.as_ptr().cast()),
                ((value.len() + 1) * 2) as u32,
            )
        }
    } else {
        match unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, w!("memory")) } {
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
    run_at_login(settings.start_with_windows).map_err(|e| format!("can't change start with windows: {e}"))?;
    app.asset_protocol_scope().allow_directory(&settings.folder, false).map_err(|e| e.to_string())?;
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
    let mut clips: Vec<Clip> = std::fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")))
        .filter_map(|entry| {
            let meta = entry.metadata().ok()?;
            let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
            let path = entry.path();
            let name = path.file_stem()?.to_string_lossy().into_owned();
            Some(Clip { path, name, bytes: meta.len(), modified })
        })
        .collect();
    clips.sort_by_key(|clip| std::cmp::Reverse(clip.modified));
    clips
}

#[tauri::command]
fn save_clip(shared: State<Shared>) {
    post(shared.recorder, WM_HOTKEY);
}

/// Only clips in the clips folder get handed to explorer, which would run anything else.
fn clip_in_folder(shared: &Shared, path: &Path) -> Result<(), String> {
    let folder = shared.settings.lock().expect("settings poisoned").folder.clone();
    let inside = path.parent() == Some(folder.as_path())
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

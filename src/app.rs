use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State, WebviewWindow, WebviewWindowBuilder, WindowEvent, Wry};
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, HWND, LPARAM, WPARAM};
use windows::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::Registry::{RegDeleteKeyValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentThreadId, SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_NOREPEAT, VK_F10, VK_F9,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetForegroundWindow, GetMessageW, IsWindow, MessageBoxW, PeekMessageW, PostThreadMessageW,
    SetTimer, MB_ICONERROR, MB_ICONHAND, MSG, PM_NOREMOVE, WM_APP, WM_HOTKEY, WM_TIMER,
};

use crate::art::{self, Art};
use crate::capture::Target;
use crate::focus::{self, Follow};
use crate::library::{self, Clip, Marks};
use crate::ring::SECOND;
use crate::{hdr, notice, toast, Options, Session, Tape};

// capscr's capture sound
static SAVED: &[u8] = include_bytes!("../assets/saved.wav");
const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const WM_RESTART: u32 = WM_APP;
const WM_HOTKEYS: u32 = WM_APP + 1;
// wparam 1 lets go of every hotkey so the keys of one being bound reach the window, 0 takes them back
const WM_HOLD: u32 = WM_APP + 2;
// wparam 1 pauses, 0 resumes
const WM_PAUSE: u32 = WM_APP + 3;
const WAITING: &str = "waiting for a game";
const PAUSED: &str = "paused";
const SAVE: usize = 1;
const MARK: usize = 2;
const SHOT: usize = 3;
const MIC: usize = 4;
const RECORD: usize = 5;
// hotkey ids from here on are indexes into the settings' hotkeys
const HOTKEY: usize = 100;
// a marked clip saves itself this long after the newest mark, so the moment sits mid clip
const LEAD: Duration = Duration::from_secs(15);
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
    /// exe names recorded in a window too, not only covering a monitor
    record: Vec<String>,
    folder: PathBuf,
    /// other apps' clip folders the library lists and never changes
    sources: Vec<PathBuf>,
    start_with_windows: bool,
    /// whole gigabytes the clips folder may hold, 0 for no limit
    cap: u32,
    /// a steamgriddb api key, for art of games steam doesn't have
    steamgriddb: String,
    hotkeys: Vec<Hotkey>,
    /// track name to volume in percent, 100 when missing
    volumes: BTreeMap<String, u32>,
    /// a capture endpoint id, empty for the default microphone
    mic_device: String,
    /// a virtual key the microphone is only heard while held, 0 for always
    push_to_talk: u32,
    sound: bool,
    /// percent of the saved sound's own level
    sound_volume: u32,
    notify: bool,
    /// days an unstarred clip is kept, 0 for ever
    keep_days: u32,
    open_on_start: bool,
    close_to_tray: bool,
    /// lowercase exe names never recorded
    ignore: Vec<String>,
    /// a note in a corner of the game's screen on saves and key presses
    notice: bool,
    notice_corner: notice::Corner,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
struct Hotkey {
    action: Action,
    /// MOD_ALT, MOD_CONTROL, MOD_SHIFT and MOD_WIN bits
    mods: u32,
    /// a virtual key code, 0 for none
    key: u32,
    /// how much of the buffer a save keeps, 0 for all of it
    seconds: u32,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Save,
    Mark,
    Shot,
    Mic,
    Record,
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
            record: Vec::new(),
            folder: PathBuf::new(),
            sources: Vec::new(),
            start_with_windows: false,
            cap: 0,
            steamgriddb: String::new(),
            hotkeys: vec![
                Hotkey { action: Action::Save, mods: MOD_ALT.0, key: VK_F9.0 as u32, seconds: 0 },
                Hotkey { action: Action::Mark, mods: MOD_ALT.0, key: VK_F10.0 as u32, seconds: 0 },
            ],
            volumes: BTreeMap::new(),
            mic_device: String::new(),
            push_to_talk: 0,
            sound: true,
            sound_volume: 100,
            notify: false,
            keep_days: 0,
            open_on_start: false,
            close_to_tray: true,
            ignore: Vec::new(),
            notice: true,
            notice_corner: notice::Corner::TopRight,
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
        let audio = |s: &Self| (s.mic, s.apps.clone(), s.volumes.clone(), s.mic_device.clone(), s.push_to_talk);
        (self.buffer, self.fps, self.height, self.mbps, self.hdr10, &self.ignore, audio(self))
            != (next.buffer, next.fps, next.height, next.mbps, next.hdr10, &next.ignore, audio(next))
    }
}

#[derive(Serialize, Clone, Default)]
struct Status {
    recording: bool,
    app: String,
    /// the recorded app's lowercase exe name
    exe: String,
    message: String,
    paused: bool,
    hotkeys: Vec<Bound>,
    mic_muted: bool,
    /// ms since 1970 a long recording started at, 0 when none runs
    taping: u64,
}

#[derive(Serialize, Clone)]
struct Bound {
    #[serde(flatten)]
    hotkey: Hotkey,
    /// another app holds the key
    taken: bool,
}

struct Shared {
    settings: Arc<Mutex<Settings>>,
    status: Arc<Mutex<Status>>,
    file: PathBuf,
    marks: Arc<Mutex<Marks>>,
    marks_file: PathBuf,
    /// stills and decoded audio for the window, safe to lose
    cache: PathBuf,
    /// the run value that starts clip with windows
    login: String,
    first: AtomicBool,
    recorder: u32,
}

/// Runs clip from the tray, keeping the last stretch of the focused app ready to save.
pub fn run() {
    if let Err(e) = serve() {
        unsafe { MessageBoxW(None, &HSTRING::from(e), w!("clip"), MB_ICONERROR) };
    }
}

fn serve() -> Result<(), String> {
    // below normal, not background mode, which would also let windows page out the buffer
    let _ = unsafe { SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) };
    // a profile runs a separate clip beside the usual one, with its own settings and single instance
    let suffix = match std::env::var("CLIP_PROFILE") {
        Ok(p) if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') => format!("-{p}"),
        _ => String::new(),
    };
    // clip was called memory, and its folders and start entry carry that name until moved
    let old = format!("memory{suffix}");
    // sfmg, once called clip, holds Clip for its install and library folders, run value and lt.rot.clip, and windows ignores case
    let name = format!("clip-recorder{suffix}");
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
            mark_clip,
            take_shot,
            toggle_mic,
            toggle_recording,
            open,
            reveal,
            audio_apps,
            windowed_apps,
            star,
            rename,
            delete,
            set_marks,
            save_frame,
            copy_files,
            save_thumb,
            track_audio,
            export,
            cancel_export,
            restore_original,
            drop_original,
            game_art,
            pause,
            hold_hotkeys,
            preview_sound,
            mics,
            mic_level,
            test_notification,
            disk,
            other_folders,
            import_folder
        ])
        .setup(move |app| {
            let paths = app.path();
            let moves = [
                (paths.config_dir()?, old.clone(), name.clone()),
                (paths.cache_dir()?, old.clone(), name.clone()),
                (paths.local_data_dir()?, format!("lt.rot.{old}"), format!("lt.rot.{name}")),
            ];
            if let Err(e) = migrate(&moves.map(|(root, from, to)| (root.join(from), root.join(to)))) {
                // the old folders stay where they are, so nothing is lost by starting without them
                std::thread::spawn(move || unsafe {
                    MessageBoxW(
                        None,
                        &HSTRING::from(format!("clip starts without its old settings: {e}")),
                        w!("clip"),
                        MB_ICONERROR,
                    )
                });
            }
            let dir = app.path().config_dir()?.join(&name);
            let file = dir.join("settings.json");
            let marks_file = dir.join("library.json");
            let marks = Arc::new(Mutex::new(Marks::load(&marks_file)));
            let cache = app.path().cache_dir()?.join(&name);
            app.asset_protocol_scope().allow_directory(&cache, true)?;
            let icon = cache.join("icon.png");
            let _ = std::fs::create_dir_all(&cache)
                .and_then(|()| std::fs::write(&icon, include_bytes!("../icons/icon.png")));
            let _ = toast::register(&app.config().identifier, "clip", &icon);
            let stored = std::fs::read_to_string(&file).ok();
            let mut settings: Settings =
                stored.as_deref().and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default();
            if settings.folder.as_os_str().is_empty() {
                settings.folder = app.path().video_dir()?.join("clip-recorder");
            }
            app.asset_protocol_scope().allow_directory(&settings.folder, true)?;
            for source in &settings.sources {
                app.asset_protocol_scope().allow_directory(source, true)?;
            }
            let _ = run_at_login(&old, false);
            if settings.start_with_windows {
                let _ = run_at_login(&name, true);
            }
            let pause = CheckMenuItem::with_id(app, "pause", "pause recording", true, false, None::<&str>)?;
            let taping = MenuItem::with_id(app, "record", "start recording", true, None::<&str>)?;
            let mute = CheckMenuItem::with_id(app, "mic", "mute microphone", true, false, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &MenuItem::with_id(app, "save", "save clip", true, None::<&str>)?,
                    &MenuItem::with_id(app, "shot", "take screenshot", true, None::<&str>)?,
                    &taping,
                    &PredefinedMenuItem::separator(app)?,
                    &mute,
                    &pause,
                    &MenuItem::with_id(app, "show", "open clip", true, None::<&str>)?,
                    &MenuItem::with_id(app, "folder", "open clips folder", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "quit", "quit", true, None::<&str>)?,
                ],
            )?;
            let mut tray = TrayIconBuilder::with_id("clip")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("clip: starting")
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
                let library = (marks.clone(), marks_file.clone(), cache.join("thumbs"));
                let tray = Tray { icon: tray, pause, taping, mute };
                std::thread::spawn(move || record(&app, &tray, &settings, &status, &library, ready));
            }
            let first = stored.is_none();
            let open_on_start = settings.lock().expect("settings poisoned").open_on_start;
            let recorder = recorder.recv()?;
            app.manage(Shared {
                settings,
                status,
                file,
                marks,
                marks_file,
                cache,
                login: name.clone(),
                first: AtomicBool::new(first),
                recorder,
            });
            app.on_menu_event(|app, event| {
                let shared = app.state::<Shared>();
                match event.id.as_ref() {
                    "save" => post(shared.recorder, WM_HOTKEY, SAVE),
                    "shot" => post(shared.recorder, WM_HOTKEY, SHOT),
                    "record" => post(shared.recorder, WM_HOTKEY, RECORD),
                    "mic" => post(shared.recorder, WM_HOTKEY, MIC),
                    "pause" => {
                        let paused = shared.status.lock().expect("status poisoned").paused;
                        post(shared.recorder, WM_PAUSE, !paused as usize);
                    }
                    "show" => show(app),
                    "folder" => {
                        let folder = shared.settings.lock().expect("settings poisoned").folder.clone();
                        let _ = std::fs::create_dir_all(&folder);
                        let _ = Command::new("explorer").arg(folder).spawn();
                    }
                    "quit" => {
                        if let Some(window) = app.get_webview_window("main") {
                            remember(app, &window);
                        }
                        app.exit(0)
                    }
                    _ => {}
                }
            });
            if first || open_on_start {
                show(app.handle());
            }
            Ok(())
        })
        .build(context)
        .map_err(|e| e.to_string())?
        .run(|app, event| match event {
            RunEvent::WindowEvent { label, event: WindowEvent::CloseRequested { .. }, .. } => {
                if let Some(window) = app.get_webview_window(&label) {
                    remember(app, &window);
                }
                if !app.state::<Shared>().settings.lock().expect("settings poisoned").close_to_tray {
                    app.exit(0);
                }
            }
            // closing the window only hides clip in the tray, quitting from the tray passes a code
            RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
            // a long recording only plays once it's closed, so quitting closes it first
            RunEvent::Exit => {
                let shared = app.state::<Shared>();
                let taping = || shared.status.lock().expect("status poisoned").taping != 0;
                if taping() {
                    post(shared.recorder, WM_HOTKEY, RECORD);
                    let until = Instant::now() + Duration::from_secs(10);
                    while taping() && Instant::now() < until {
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
            _ => {}
        });
    Ok(())
}

fn post(thread: u32, message: u32, key: usize) {
    let _ = unsafe { PostThreadMessageW(thread, message, WPARAM(key), LPARAM(0)) };
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
        let Some(mut config) = app.config().app.windows.first().cloned() else { return };
        match placement(&app) {
            Some(placed) => {
                (config.width, config.height, config.maximized) = (placed.width, placed.height, placed.maximized);
                if let Some((x, y)) = placed.x.zip(placed.y).filter(|&(x, y)| on_screen(&app, x, y)) {
                    (config.x, config.y, config.center) = (Some(x), Some(y), false);
                }
            }
            None => config.maximized = true,
        }
        let _ = WebviewWindowBuilder::from_config(&app, &config).and_then(|window| window.build());
    });
}

/// Where the window was when it last closed, in logical pixels.
#[derive(Serialize, Deserialize, Clone, Copy)]
struct Placement {
    x: Option<f64>,
    y: Option<f64>,
    width: f64,
    height: f64,
    maximized: bool,
}

fn placement_file(app: &AppHandle) -> PathBuf {
    app.state::<Shared>().file.with_file_name("window.json")
}

fn placement(app: &AppHandle) -> Option<Placement> {
    serde_json::from_str(&std::fs::read_to_string(placement_file(app)).ok()?).ok()
}

fn remember(app: &AppHandle, window: &WebviewWindow) {
    if window.is_minimized().unwrap_or(true) {
        return;
    }
    let maximized = window.is_maximized().unwrap_or(false);
    let placed = if maximized {
        // a maximized window's bounds are the monitor's, so the last normal ones are kept for restoring
        let fallback = app.config().app.windows.first().map_or((1080.0, 700.0), |config| (config.width, config.height));
        let last =
            placement(app).unwrap_or(Placement { x: None, y: None, width: fallback.0, height: fallback.1, maximized });
        Placement { maximized, ..last }
    } else {
        let (Ok(scale), Ok(at), Ok(size)) = (window.scale_factor(), window.outer_position(), window.inner_size())
        else {
            return;
        };
        let (at, size) = (at.to_logical::<f64>(scale), size.to_logical::<f64>(scale));
        Placement { x: Some(at.x), y: Some(at.y), width: size.width, height: size.height, maximized }
    };
    if let Ok(json) = serde_json::to_string(&placed) {
        let _ = std::fs::write(placement_file(app), json);
    }
}

/// Whether a window placed at `x`, `y` would have its title bar on a monitor that is still there.
fn on_screen(app: &AppHandle, x: f64, y: f64) -> bool {
    app.available_monitors().unwrap_or_default().iter().any(|monitor| {
        let (at, size) = (
            monitor.position().to_logical::<f64>(monitor.scale_factor()),
            monitor.size().to_logical::<f64>(monitor.scale_factor()),
        );
        (at.x..at.x + size.width).contains(&(x + 60.0)) && (at.y..at.y + size.height).contains(&(y + 10.0))
    })
}

/// The tray menu items the recorder keeps up to date.
struct Tray {
    icon: TrayIcon,
    pause: CheckMenuItem<Wry>,
    taping: MenuItem<Wry>,
    mute: CheckMenuItem<Wry>,
}

/// A long recording, written as it goes for as long as the session it follows lives.
struct Taping {
    tape: Tape,
    recording: std::rc::Weak<Recording>,
    app: String,
    window: HWND,
    /// where it's written, for the drive's free space
    out: PathBuf,
    checked: Instant,
}

// a long recording stops before the drive fills up
const ROOM: u64 = 2_000_000_000;

/// A session and the app window it records.
struct Recording {
    session: Session,
    window: HWND,
    app: String,
    exe: String,
}

/// A clip that saves itself once `due`, holding on to its recording until then.
struct Pending {
    recording: Rc<Recording>,
    /// clock times of the marks
    marks: Vec<i64>,
    first: Instant,
    due: Instant,
}

impl Pending {
    fn mark(&mut self, at: i64, now: Instant, buffer: Duration) {
        self.marks.push(at);
        self.due = due(self.first, now, buffer);
    }
}

/// When a clip marked first at `first` and last at `now` saves: [`LEAD`] after the newest mark,
/// but never so late that the first would have left a buffer `buffer` long.
fn due(first: Instant, now: Instant, buffer: Duration) -> Instant {
    let lead = LEAD.min(buffer / 2);
    let latest = first + buffer - lead.min(Duration::from_secs(2));
    (now + lead).min(latest).max(now)
}

/// Library paths and stills, shared with the window.
type Library = (Arc<Mutex<Marks>>, PathBuf, PathBuf);

/// Records on this thread's message loop, following the focused game, saving or marking a clip on its
/// hotkeys, and starting over on WM_RESTART.
fn record(
    app: &AppHandle,
    menu: &Tray,
    settings: &Mutex<Settings>,
    status: &Mutex<Status>,
    (marks, marks_file, thumbs): &Library,
    ready: mpsc::Sender<u32>,
) {
    let mut msg = MSG::default();
    // the queue has to exist before anything is posted to it
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    let _ = ready.send(unsafe { GetCurrentThreadId() });
    let (bound, held, paused) = (RefCell::new(Vec::<Bound>::new()), Cell::new(false), Cell::new(false));
    let (tray, pause) = (&menu.icon, &menu.pause);
    let taping_since = Cell::new(0u64);
    let rebind = || {
        let mut bound = bound.borrow_mut();
        for id in HOTKEY..HOTKEY + bound.len() {
            let _ = unsafe { UnregisterHotKey(None, id as i32) };
        }
        let hotkeys = settings.lock().expect("settings poisoned").hotkeys.clone();
        *bound = (hotkeys.into_iter().enumerate())
            .map(|(i, hotkey)| {
                let mods = HOT_KEY_MODIFIERS(hotkey.mods & 0xF) | MOD_NOREPEAT;
                let id = (HOTKEY + i) as i32;
                let taken =
                    !held.get() && hotkey.key != 0 && unsafe { RegisterHotKey(None, id, mods, hotkey.key) }.is_err();
                Bound { hotkey, taken }
            })
            .collect();
    };
    rebind();
    let pressed = |msg: &MSG| match msg.wParam.0 {
        SAVE => Some((Action::Save, 0)),
        MARK => Some((Action::Mark, 0)),
        SHOT => Some((Action::Shot, 0)),
        MIC => Some((Action::Mic, 0)),
        RECORD => Some((Action::Record, 0)),
        id => bound.borrow().get(id.wrapping_sub(HOTKEY)).map(|bound| (bound.hotkey.action, bound.hotkey.seconds)),
    };
    unsafe { SetTimer(None, 0, 250, None) };
    let media = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
        .and_then(|()| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) })
        .map_err(|e| e.to_string());
    let begin = |window: HWND| -> Result<Rc<Recording>, String> {
        media.clone()?;
        let current = settings.lock().expect("settings poisoned").clone();
        let pid = focus::pid(window);
        let mic = current.mic.then(|| (current.mic_device.clone(), current.push_to_talk));
        let lanes = crate::audio::lanes(Some(pid), &current.apps, mic, &current.volumes);
        let session = Session::start(&current.options(), Target::Window(window), &lanes)?;
        let app = focus::app_name(pid);
        // the icon only dresses up the library, a game without one still records
        let _ = focus::save_icon(pid, &library::icon(thumbs, &app));
        if let Some(exe) = focus::exe(pid) {
            let _ = art::remember(&exe, &art::dir(thumbs, &app));
        }
        let exe = focus::exe_name(pid).unwrap_or_default();
        Ok(Rc::new(Recording { session, window, app, exe }))
    };
    let focused = || {
        let settings = settings.lock().expect("settings poisoned");
        focus::focused(&settings.record, &settings.ignore)
    };
    let report = |recording: &Result<Rc<Recording>, String>| {
        let buffer = settings.lock().expect("settings poisoned").options().buffer;
        let hotkeys = bound.borrow().clone();
        let taken = if hotkeys.iter().any(|bound| bound.taken) { ", a hotkey is taken by another app" } else { "" };
        let (mic_muted, taping) = (crate::audio::MIC_MUTED.load(Ordering::Relaxed), taping_since.get());
        let next = match recording {
            Err(e) => Status {
                message: format!("{e}{taken}"),
                paused: paused.get(),
                hotkeys,
                mic_muted,
                taping,
                ..Status::default()
            },
            Ok(recording) => Status {
                recording: true,
                app: recording.app.clone(),
                exe: recording.exe.clone(),
                message: format!("keeping the last {buffer} s{taken}"),
                paused: false,
                hotkeys,
                mic_muted,
                taping,
            },
        };
        let _ = pause.set_checked(next.paused);
        let _ = menu.mute.set_checked(next.mic_muted);
        let _ = menu.taping.set_text(if next.taping == 0 { "start recording" } else { "stop recording" });
        let _ = tray.set_tooltip(Some(match next.recording {
            true => format!("clip: recording {}, {}", next.app, next.message),
            false => format!("clip: {}", next.message),
        }));
        let _ = app.emit("status", &next);
        *status.lock().expect("status poisoned") = next;
    };
    // nothing shows over clip's own window, where the window says it
    let note = |text: &str, bad: bool, game: HWND| {
        let corner = {
            let settings = settings.lock().expect("settings poisoned");
            settings.notice.then_some(settings.notice_corner)
        };
        if let Some(corner) = corner.filter(|_| !focus::ours(unsafe { GetForegroundWindow() })) {
            let text: String = match text.chars().count() {
                ..=90 => text.into(),
                _ => text.chars().take(89).chain(['\u{2026}']).collect(),
            };
            notice::show(&text, bad, corner, game);
        }
    };
    let feedback = |text: String, bad: bool, game: HWND| {
        let sound = {
            let settings = settings.lock().expect("settings poisoned");
            settings.sound.then_some(settings.sound_volume)
        };
        match (bad, sound) {
            (true, _) => {
                let _ = unsafe { MessageBeep(MB_ICONHAND) };
            }
            (false, Some(volume)) => play_saved(volume),
            (false, None) => {}
        }
        note(&text, bad, game);
        let _ = app.emit("saved", (text, bad));
    };
    let save = |recording: &Recording, marked: &[i64], seconds: Option<u32>| {
        let (folder, cap, keep_days, sound, notify) = {
            let settings = settings.lock().expect("settings poisoned");
            let sound = settings.sound.then_some(settings.sound_volume);
            (settings.folder.clone(), settings.cap, settings.keep_days, sound, settings.notify)
        };
        let dir = folder.join(&recording.app);
        let saved = std::fs::create_dir_all(&dir)
            .map_err(|e| format!("can't make {}: {e}", dir.display()))
            .map(|()| free(&dir, "", "mp4"))
            .and_then(|out| Ok((recording.session.save(&out, seconds)?, out)));
        match &saved {
            Ok(_) => {
                if let Some(volume) = sound {
                    play_saved(volume);
                }
            }
            Err(_) => {
                let _ = unsafe { MessageBeep(MB_ICONHAND) };
            }
        }
        if notify {
            let id = &app.config().identifier;
            let _ = match &saved {
                Ok((saved, out)) => {
                    let body = format!("{}, {:.0} s, {}", recording.app, saved.seconds, size(saved.bytes as u64));
                    toast::show(id, "Clip saved", &body, Some(out), true)
                }
                Err(e) => toast::show(id, "Clip not saved", e, None, true),
            };
        }
        // the window says it too, where a screen reader hears it
        let _ = app.emit(
            "saved",
            match &saved {
                Ok((saved, _)) => {
                    (format!("Saved {:.0} s of {}, {}", saved.seconds, recording.app, size(saved.bytes as u64)), false)
                }
                Err(e) => (format!("Clip not saved: {e}"), true),
            },
        );
        match &saved {
            Ok((saved, _)) => {
                note(&format!("Saved {:.0} s of {}", saved.seconds, recording.app), false, recording.window)
            }
            Err(e) => note(&format!("Clip not saved: {e}"), true, recording.window),
        }
        match saved {
            Ok((saved, out)) => {
                let mut marks = marks.lock().expect("marks poisoned");
                let at: Vec<f64> = marked
                    .iter()
                    .map(|&at| (at - saved.start) as f64 / SECOND as f64)
                    .filter(|at| (0.0..=saved.seconds).contains(at))
                    .collect();
                if !at.is_empty() {
                    marks.mark(&folder, &out, at);
                    if let Err(e) = marks.store(marks_file) {
                        let _ = tray.set_tooltip(Some(format!("clip: {e}")));
                    }
                }
                trim(&folder, &marks, thumbs, cap, keep_days);
                let _ = app.emit("clips", ());
            }
            Err(e) => {
                let _ = tray.set_tooltip(Some(format!("clip: {e}")));
            }
        }
    };
    {
        let (folder, cap, keep_days) = {
            let settings = settings.lock().expect("settings poisoned");
            (settings.folder.clone(), settings.cap, settings.keep_days)
        };
        library::settle(&folder);
        trim(&folder, &marks.lock().expect("marks poisoned"), thumbs, cap, keep_days);
    }
    let shoot = |recording: &Recording| {
        let folder = settings.lock().expect("settings poisoned").folder.clone();
        let dir = folder.join(&recording.app);
        let taken = recording.session.still().and_then(|still| {
            std::fs::create_dir_all(&dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
            let out = free(&dir, "", "png");
            focus::png(still.width, still.height, &still.bgra(), &out).map_err(|e| e.to_string())?;
            Ok(still)
        });
        match taken {
            Ok(still) => {
                feedback(
                    format!("Screenshot of {} saved, {}x{}", recording.app, still.width, still.height),
                    false,
                    recording.window,
                );
                let _ = app.emit("clips", ());
            }
            Err(e) => feedback(format!("Screenshot not saved: {e}"), true, recording.window),
        }
    };
    let mut taping: Option<Taping> = None;
    let stop_tape = |taping: Taping, why: Option<&str>| {
        let Taping { tape, app: game, window, .. } = taping;
        taping_since.set(0);
        match tape.finish() {
            Ok(saved) => {
                let length = saved.seconds as u64;
                let length = format!("{}:{:02}", length / 60, length % 60);
                let text = match why {
                    Some(why) => format!("Recording saved, {length} of {game}, {why}"),
                    None => format!("Recording saved, {length} of {game}"),
                };
                feedback(text, false, window);
                let (folder, cap, keep_days) = {
                    let settings = settings.lock().expect("settings poisoned");
                    (settings.folder.clone(), settings.cap, settings.keep_days)
                };
                trim(&folder, &marks.lock().expect("marks poisoned"), thumbs, cap, keep_days);
                let _ = app.emit("clips", ());
            }
            Err(e) => feedback(format!("Recording not saved: {e}"), true, window),
        }
    };
    let mut current: Result<Rc<Recording>, String> = Err(WAITING.into());
    let mut previous: Option<(Rc<Recording>, Instant)> = None;
    let mut pending: Option<Pending> = None;
    let mut follow = Follow::default();
    let mut refused = None;
    let mut restart = true;
    loop {
        if restart {
            restart = false;
            if let Some(early) = pending.take() {
                save(&early.recording, &early.marks, None);
            }
            // the old sessions let go of the encoder before the new one opens its own
            previous = None;
            drop(std::mem::replace(&mut current, Err(if paused.get() { PAUSED } else { WAITING }.into())));
            refused = None;
            if let Some(window) = focused().filter(|_| !paused.get()) {
                current = begin(window);
                refused = current.is_err().then_some(window);
            }
            report(&current);
        }
        if unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 <= 0 {
            break;
        }
        // the notice's own paints and timers
        if !msg.hwnd.is_invalid() {
            unsafe { DispatchMessageW(&msg) };
            continue;
        }
        if msg.message == WM_TIMER {
            if let Some(mut running) = taping.take() {
                let gone = running.recording.upgrade().is_none();
                let left = (running.checked.elapsed() > Duration::from_secs(5)).then(|| {
                    running.checked = Instant::now();
                    room(&running.out)
                });
                let why = match running.tape.pull() {
                    Err(e) => Some(e),
                    Ok(()) if left.is_some_and(|left| left < ROOM) => Some("the drive is nearly full".into()),
                    Ok(()) if gone => Some("its recording ended".into()),
                    Ok(()) => None,
                };
                match why {
                    Some(why) => {
                        stop_tape(running, Some(&why));
                        report(&current);
                    }
                    None => taping = Some(running),
                }
            }
        }
        let gone = |w: HWND| !unsafe { IsWindow(Some(w)) }.as_bool();
        let recorded = current.as_ref().ok().map(|r| r.window);
        let back = previous.as_ref().map(|(r, _)| r.window);
        let next = match msg.message {
            WM_RESTART => {
                restart = true;
                None
            }
            WM_HOTKEYS | WM_HOLD => {
                if msg.message == WM_HOLD {
                    held.set(msg.wParam.0 == 1);
                }
                rebind();
                report(&current);
                None
            }
            WM_PAUSE => {
                let on = msg.wParam.0 == 1;
                restart = paused.replace(on) != on;
                None
            }
            WM_TIMER if paused.get() => None,
            WM_TIMER => {
                if previous.as_ref().is_some_and(|(r, left)| left.elapsed() > GRACE || gone(r.window)) {
                    previous = None;
                }
                let recorded = recorded.filter(|&w| !gone(w));
                if recorded.is_none() && current.is_ok() {
                    current = Err(WAITING.into());
                    report(&current);
                }
                if let Some(due) = pending.take_if(|due| due.due <= Instant::now()) {
                    save(&due.recording, &due.marks, None);
                }
                let candidate = focused().filter(|&w| Some(w) != refused);
                follow.tick(candidate, recorded, back, Instant::now())
            }
            WM_HOTKEY if pressed(&msg).is_some_and(|(action, _)| action == Action::Shot) => {
                match &current {
                    Ok(recording) => shoot(recording),
                    Err(e) => feedback(format!("Screenshot not taken: {e}"), true, HWND::default()),
                }
                None
            }
            WM_HOTKEY if pressed(&msg).is_some_and(|(action, _)| action == Action::Mic) => {
                let window = current.as_ref().map_or(HWND::default(), |r| r.window);
                if settings.lock().expect("settings poisoned").mic {
                    let muted = !crate::audio::MIC_MUTED.fetch_xor(true, Ordering::Relaxed);
                    note(if muted { "Mic muted" } else { "Mic on" }, false, window);
                    let _ = app.emit("saved", (if muted { "Mic muted" } else { "Mic on" }, false));
                    report(&current);
                } else {
                    feedback("The mic isn't recorded, turn it on in Audio".into(), true, window);
                }
                None
            }
            WM_HOTKEY if pressed(&msg).is_some_and(|(action, _)| action == Action::Record) => {
                match (taping.take(), &current) {
                    (Some(running), _) => stop_tape(running, None),
                    (None, Ok(recording)) => {
                        let folder = settings.lock().expect("settings poisoned").folder.clone();
                        let dir = folder.join(&recording.app);
                        let started = std::fs::create_dir_all(&dir)
                            .map_err(|e| format!("can't make {}: {e}", dir.display()))
                            .map(|()| free(&dir, " recording", "mp4"))
                            .and_then(|out| match room(&out) {
                                room if room < ROOM => Err("the drive is nearly full".to_string()),
                                _ => Ok((recording.session.tape(&out)?, out)),
                            });
                        match started {
                            Ok((tape, out)) => {
                                let now = std::time::SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .map_or(1, |d| d.as_millis() as u64);
                                taping_since.set(now);
                                taping = Some(Taping {
                                    tape,
                                    recording: Rc::downgrade(recording),
                                    app: recording.app.clone(),
                                    window: recording.window,
                                    out,
                                    checked: Instant::now(),
                                });
                                note(&format!("Recording {}", recording.app), false, recording.window);
                                let _ = app.emit("saved", (format!("Recording {}", recording.app), false));
                            }
                            Err(e) => feedback(format!("Recording not started: {e}"), true, recording.window),
                        }
                    }
                    (None, Err(e)) => feedback(format!("Recording not started: {e}"), true, HWND::default()),
                }
                report(&current);
                None
            }
            WM_HOTKEY if pressed(&msg).is_some_and(|(action, _)| action == Action::Mark) => {
                match &current {
                    Ok(recording) => {
                        let (now, at) = (Instant::now(), recording.session.clock.now());
                        let buffer =
                            Duration::from_secs(settings.lock().expect("settings poisoned").options().buffer as u64);
                        // marks in another app's recording make their clip now
                        if let Some(other) = pending.take_if(|p| !Rc::ptr_eq(&p.recording, recording)) {
                            save(&other.recording, &other.marks, None);
                        }
                        pending
                            .get_or_insert_with(|| Pending {
                                recording: recording.clone(),
                                marks: Vec::new(),
                                first: now,
                                due: now,
                            })
                            .mark(at, now, buffer);
                    }
                    Err(_) => {
                        let _ = unsafe { MessageBeep(MB_ICONHAND) };
                    }
                }
                None
            }
            WM_HOTKEY => {
                let seconds = pressed(&msg).map(|(_, seconds)| seconds).filter(|&seconds| seconds > 0);
                // pressed straight after coming back to an app, the clip is of that app
                if let Some(back) = back.filter(|&back| focused() == Some(back)) {
                    if switch(back, &mut current, &mut previous, &begin).is_ok() {
                        report(&current);
                    }
                }
                match &current {
                    // a save while a marked clip waits keeps its marks, and the marked clip still comes
                    Ok(recording) => {
                        let marked = pending.as_ref().filter(|p| Rc::ptr_eq(&p.recording, recording));
                        save(recording, marked.map_or(&[], |p| &p.marks), seconds);
                    }
                    Err(e) => {
                        let _ = unsafe { MessageBeep(MB_ICONHAND) };
                        let _ = tray.set_tooltip(Some(format!("clip: {e}")));
                    }
                }
                None
            }
            _ => None,
        };
        if let Some(window) = next {
            match switch(window, &mut current, &mut previous, &begin) {
                Ok(()) => refused = None,
                Err(e) => {
                    refused = Some(window);
                    // a running recording carries on, only the waiting note gives way to the error
                    if current.is_err() {
                        current = Err(e);
                    }
                }
            }
            report(&current);
        }
    }
}

/// Moves recording to `window`, leaving the app it was on recording for a while in case focus
/// comes straight back.
fn switch(
    window: HWND,
    current: &mut Result<Rc<Recording>, String>,
    previous: &mut Option<(Rc<Recording>, Instant)>,
    begin: &impl Fn(HWND) -> Result<Rc<Recording>, String>,
) -> Result<(), String> {
    let next = match previous.take() {
        Some((recording, _)) if recording.window == window => recording,
        // two sessions at most, so the older one goes first
        _ => begin(window)?,
    };
    let left = std::mem::replace(current, Ok(next));
    *previous =
        left.ok().filter(|left| unsafe { IsWindow(Some(left.window)) }.as_bool()).map(|left| (left, Instant::now()));
    Ok(())
}

/// A path in `dir` named after the time and `tail`, numbered when one from the same second is there.
fn free(dir: &Path, tail: &str, ext: &str) -> PathBuf {
    let t = unsafe { GetLocalTime() };
    let stamp = format!("{}-{:02}-{:02} {:02}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond);
    (1..)
        .map(|n| match n {
            1 => dir.join(format!("{stamp}{tail}.{ext}")),
            n => dir.join(format!("{stamp}{tail} {n}.{ext}")),
        })
        .find(|out| !out.exists())
        .expect("some name is free")
}

/// Renames each folder to its new name, unless it's gone or the new one is already there.
fn migrate(moves: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    moves
        .iter()
        .filter(|(from, to)| from.is_dir() && !to.exists())
        .try_for_each(|(from, to)| std::fs::rename(from, to))
        .map_err(|e| e.to_string())
}

/// Deletes the oldest unstarred clips past `cap` GB or older than `keep_days`, 0 being no limit.
fn trim(folder: &Path, marks: &Marks, thumbs: &Path, cap: u32, keep_days: u32) {
    if cap == 0 && keep_days == 0 {
        return;
    }
    let now = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |now| now.as_millis() as u64);
    let before = match keep_days {
        0 => 0,
        days => now.saturating_sub(days as u64 * 86_400_000),
    };
    let cap = if cap == 0 { u64::MAX } else { cap as u64 * 1_000_000_000 };
    library::trim(&library::list(folder, &[], marks, thumbs), cap, before);
}

/// Plays the saved sound at `percent` of its level, on a thread of its own that keeps the scaled copy.
fn play_saved(percent: u32) {
    let wav = louder(SAVED, percent.min(150));
    std::thread::spawn(move || unsafe {
        let _ = PlaySoundW(PCWSTR(wav.as_ptr().cast()), None, SND_MEMORY | SND_NODEFAULT);
    });
}

/// A 16 bit pcm wav with its samples scaled to `percent`, clipping instead of wrapping.
fn louder(wav: &[u8], percent: u32) -> Vec<u8> {
    let mut out = wav.to_vec();
    let mut at = 12;
    while at + 8 <= out.len() {
        let size = u32::from_le_bytes(out[at + 4..at + 8].try_into().expect("four bytes")) as usize;
        let body = at + 8..(at + 8 + size).min(out.len());
        if &out[at..at + 4] == b"data" {
            for sample in out[body].as_chunks_mut::<2>().0 {
                let scaled = i16::from_le_bytes(*sample) as i32 * percent as i32 / 100;
                sample.copy_from_slice(&(scaled.clamp(i16::MIN.into(), i16::MAX.into()) as i16).to_le_bytes());
            }
            break;
        }
        // chunks are padded to an even length
        at = body.end + (size & 1);
    }
    out
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
    if let Some(bad) = settings.sources.iter().find(|dir| !dir.is_absolute()) {
        return Err(format!("{} isn't a whole path", bad.display()));
    }
    // clip's own files can be deleted by cleanup, so a folder inside it can't be read only
    let low = |dir: &Path| format!("{}\\", dir.to_string_lossy().trim_end_matches(['\\', '/']).to_lowercase());
    if let Some(inside) = settings.sources.iter().find(|dir| low(dir).starts_with(&low(&settings.folder))) {
        return Err(format!("{} is in the clips folder, so it's listed already", inside.display()));
    }
    let json = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string())?;
    if let Some(dir) = shared.file.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(&shared.file, json).map_err(|e| format!("can't save settings: {e}"))?;
    run_at_login(&shared.login, settings.start_with_windows)
        .map_err(|e| format!("can't change start with windows: {e}"))?;
    for dir in std::iter::once(&settings.folder).chain(&settings.sources) {
        app.asset_protocol_scope().allow_directory(dir, true).map_err(|e| e.to_string())?;
    }
    shared.first.store(false, Ordering::Relaxed);
    let (restart, rebind) = {
        let mut current = shared.settings.lock().expect("settings poisoned");
        let changed = (current.records_differently(&settings), current.hotkeys != settings.hotkeys);
        *current = settings;
        changed
    };
    if rebind {
        post(shared.recorder, WM_HOTKEYS, 0);
    }
    if restart {
        post(shared.recorder, WM_RESTART, 0);
    }
    let _ = app.emit("clips", ());
    Ok(())
}

#[tauri::command]
fn clips(shared: State<Shared>) -> Vec<Clip> {
    let (folder, sources) = {
        let settings = shared.settings.lock().expect("settings poisoned");
        (settings.folder.clone(), settings.sources.clone())
    };
    let thumbs = shared.cache.join("thumbs");
    let clips = library::list(&folder, &sources, &shared.marks.lock().expect("marks poisoned"), &thumbs);
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
fn star(app: AppHandle, shared: State<Shared>, paths: Vec<PathBuf>, on: bool) -> Result<(), String> {
    paths.iter().try_for_each(|path| listed(&shared, path))?;
    marked(&shared, |marks, folder| paths.iter().for_each(|path| marks.star(folder, path, on)))?;
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
fn delete(app: AppHandle, shared: State<Shared>, paths: Vec<PathBuf>) -> Result<(), String> {
    paths.iter().try_for_each(|path| saved_in_folder(&shared, path))?;
    let recycled = library::recycle(&paths);
    // a cancelled operation can still have moved some of them
    let gone = paths.iter().filter(|path| !path.exists());
    marked(&shared, |marks, folder| gone.for_each(|path| marks.forget(folder, path)))?;
    let _ = app.emit("clips", ());
    recycled
}

#[tauri::command]
fn set_marks(app: AppHandle, shared: State<Shared>, path: PathBuf, mut at: Vec<f64>) -> Result<(), String> {
    listed(&shared, &path)?;
    at.retain(|t| t.is_finite() && *t >= 0.0);
    at.sort_by(f64::total_cmp);
    at.dedup_by(|a, b| (*a - *b).abs() < 0.05);
    marked(&shared, |marks, folder| marks.mark(folder, &path, at))?;
    let _ = app.emit("clips", ());
    Ok(())
}

/// Saves the png the webview drew of a paused frame, or the gif it made of a cut, beside its clip.
/// The body is the image, so a 4k frame isn't sent as a json list of numbers, and the headers say
/// which clip and from when.
#[tauri::command]
fn save_frame(shared: State<Shared>, request: tauri::ipc::Request) -> Result<PathBuf, String> {
    let header = |name: &str| {
        let value = request.headers().get(name).and_then(|v| v.to_str().ok());
        value.and_then(unescape).ok_or_else(|| format!("the frame came without its {name}"))
    };
    let path = PathBuf::from(header("clip")?);
    let at: f64 = header("at")?.parse().map_err(|_| "the frame's time isn't a number")?;
    listed(&shared, &path)?;
    let dir = home(&shared, &path)?;
    let tauri::ipc::InvokeBody::Raw(image) = request.body() else {
        return Err("the frame has to come as bytes".into());
    };
    let kind = match image.as_slice() {
        [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n', ..] => "png",
        [b'G', b'I', b'F', b'8', b'9', b'a', ..] => "gif",
        _ => return Err("a frame has to be a png or a gif".into()),
    };
    let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
    let (minutes, seconds) = ((at / 60.0).floor(), at % 60.0);
    let out = (1..)
        .map(|n| match n {
            1 => dir.join(format!("{stem} {minutes}m{seconds:05.2}s.{kind}")),
            n => dir.join(format!("{stem} {minutes}m{seconds:05.2}s {n}.{kind}")),
        })
        .find(|out| !out.exists())
        .expect("some name is free");
    std::fs::write(&out, image).map_err(|e| format!("couldn't save the {kind}: {e}"))?;
    Ok(out)
}

/// Undoes the webview's encodeURIComponent, which is how a path fits in a header.
fn unescape(text: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(text.len());
    let mut rest = text.as_bytes();
    while let [first, tail @ ..] = rest {
        if *first == b'%' {
            let hex = std::str::from_utf8(tail.get(..2)?).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            rest = &tail[2..];
        } else {
            bytes.push(*first);
            rest = tail;
        }
    }
    String::from_utf8(bytes).ok()
}

#[tauri::command]
fn copy_files(window: WebviewWindow, shared: State<Shared>, paths: Vec<PathBuf>) -> Result<(), String> {
    paths.iter().try_for_each(|path| listed(&shared, path))?;
    crate::clipboard::copy_files(window.hwnd().map_err(|e| e.to_string())?, &paths)
}

#[tauri::command]
fn save_thumb(shared: State<Shared>, path: PathBuf, jpeg: Vec<u8>) -> Result<PathBuf, String> {
    listed(&shared, &path)?;
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

/// A game's cover, hero and logo as far as they're on disk. Missing ones are fetched in the background
/// and arrive in an `art` event.
#[tauri::command]
fn game_art(handle: AppHandle, shared: State<Shared>, app: String) -> Result<Art, String> {
    let mut parts = Path::new(&app).components();
    if !matches!((parts.next(), parts.next()), (Some(Component::Normal(_)), None)) {
        return Err(format!("{app} is not an app"));
    }
    let dir = art::dir(&shared.cache.join("thumbs"), &app);
    let have = art::cached(&dir, &app);
    let key = shared.settings.lock().expect("settings poisoned").steamgriddb.trim().to_string();
    if art::due(&dir, &have, &key) {
        std::thread::spawn(move || {
            if let Some(art) = art::fetch(&dir, &app, &key) {
                let _ = handle.emit("art", &art);
            }
        });
    }
    Ok(have)
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
    listed(&shared, &path)?;
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

// one export runs at a time, the window waits on it before starting another
static CANCEL: AtomicBool = AtomicBool::new(false);

/// Saves `start` to `end` seconds of a clip with only the `tracks` asked for, mixed into one or
/// kept apart. With `fit` it comes in under that many bytes, with `portrait` it is cut to a 9:16
/// window centred that far across, and with `replace` it takes the clip's place while the clip
/// waits beside it as `.orig` for an undo. Sends `export-progress` in percent as it goes.
#[allow(clippy::too_many_arguments)] // each is a field of the request the webview sends
#[tauri::command]
async fn export(
    app: AppHandle,
    shared: State<'_, Shared>,
    path: PathBuf,
    start: f64,
    end: f64,
    tracks: Vec<usize>,
    mix: bool,
    fit: Option<u64>,
    portrait: Option<f64>,
    replace: bool,
) -> Result<PathBuf, String> {
    match replace {
        true => clip_in_folder(&shared, &path)?,
        false => listed(&shared, &path)?,
    }
    let dir = home(&shared, &path)?;
    if !(start >= 0.0 && end > start) {
        return Err("the end has to come after the start".into());
    }
    if fit.is_some_and(|bytes| bytes < 1_000_000) {
        return Err("the size has to be at least 1 MB".into());
    }
    CANCEL.store(false, Ordering::Relaxed);
    let progress = app.clone();
    let out = blocking(move || {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let kind = match (fit, portrait) {
            (Some(bytes), Some(_)) => format!("{} MB 9x16", bytes / 1_000_000),
            (Some(bytes), None) => format!("{} MB", bytes / 1_000_000),
            (None, Some(_)) => "9x16".into(),
            (None, None) => "trimmed".into(),
        };
        let out = (1..)
            .map(|n| match n {
                1 => dir.join(format!("{stem} {kind}.mp4")),
                n => dir.join(format!("{stem} {kind} {n}.mp4")),
            })
            .find(|out| !out.exists())
            .expect("some name is free");
        let sent = Cell::new(u32::MAX);
        let report = |done: f64| {
            let percent = (done.clamp(0.0, 1.0) * 100.0) as u32;
            if sent.replace(percent) != percent {
                let _ = progress.emit("export-progress", percent);
            }
        };
        let job = crate::export::Job { fit, portrait, progress: Some(&report), cancel: Some(&CANCEL) };
        if let Err(e) = crate::export::trim(&path, &out, start, end, &tracks, mix || fit.is_some(), &job) {
            let _ = std::fs::remove_file(&out);
            return Err(e);
        }
        if replace {
            take_place(&path, &out)?;
            return Ok((path.clone(), path));
        }
        Ok((path, out))
    })
    .await
    .and_then(|(path, out)| {
        marked(&shared, |marks, folder| {
            let kept = marks.marks(folder, &path).into_iter().filter(|at| (start..=end).contains(at));
            marks.mark(folder, &out, kept.map(|at| at - start).collect());
        })?;
        Ok(out)
    });
    let _ = app.emit("clips", ());
    out
}

/// Puts `trimmed` where `clip` was, with its date, and keeps the clip beside it as `.orig`.
fn take_place(clip: &Path, trimmed: &Path) -> Result<(), String> {
    let original = clip.with_extension("mp4.orig");
    // a second replace inside the undo keeps only the newest original
    if original.exists() {
        library::recycle(std::slice::from_ref(&original))?;
    }
    let modified = std::fs::metadata(clip).and_then(|meta| meta.modified());
    let failed = |e: std::io::Error| {
        let _ = std::fs::remove_file(trimmed);
        format!("can't replace {}: {e}", clip.display())
    };
    std::fs::rename(clip, &original).map_err(failed)?;
    if let Err(e) = std::fs::rename(trimmed, clip) {
        let _ = std::fs::rename(&original, clip);
        return Err(failed(e));
    }
    // the library keeps a replaced clip in its place by date
    if let Ok(modified) = modified {
        let _ = std::fs::File::options().write(true).open(clip).and_then(|file| file.set_modified(modified));
    }
    Ok(())
}

#[tauri::command]
fn cancel_export() {
    CANCEL.store(true, Ordering::Relaxed);
}

/// Undoes a replace: the trimmed clip goes to the recycle bin and the original takes its place again.
#[tauri::command]
fn restore_original(app: AppHandle, shared: State<Shared>, path: PathBuf) -> Result<(), String> {
    clip_in_folder(&shared, &path)?;
    let original = path.with_extension("mp4.orig");
    if !original.is_file() {
        return Err("the original is gone already".into());
    }
    library::recycle(std::slice::from_ref(&path))?;
    std::fs::rename(&original, &path).map_err(|e| format!("can't put the original back: {e}"))?;
    let _ = app.emit("clips", ());
    Ok(())
}

/// Sends a replaced clip's original to the recycle bin once its undo has run out.
#[tauri::command]
fn drop_original(shared: State<Shared>, path: PathBuf) -> Result<(), String> {
    clip_in_folder(&shared, &path)?;
    let original = path.with_extension("mp4.orig");
    match original.is_file() {
        true => library::recycle(&[original]),
        false => Ok(()),
    }
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
fn pause(shared: State<Shared>, on: bool) {
    post(shared.recorder, WM_PAUSE, on as usize);
}

#[tauri::command]
fn hold_hotkeys(shared: State<Shared>, on: bool) {
    post(shared.recorder, WM_HOLD, on as usize);
}

#[tauri::command]
fn preview_sound(percent: u32) {
    play_saved(percent);
}

#[tauri::command]
fn test_notification(app: AppHandle) -> Result<(), String> {
    let body = "Saved clips show up like this";
    toast::show(&app.config().identifier, "Clip saved", body, None, true)
}

#[derive(Serialize)]
struct Disk {
    free: u64,
    total: u64,
}

/// Free bytes on the drive `path` is on, or as many as it takes when the drive can't say.
fn room(path: &Path) -> u64 {
    let found = path.ancestors().find(|dir| dir.is_dir());
    let mut free = u64::MAX;
    if let Some(dir) = found {
        let _ = unsafe { GetDiskFreeSpaceExW(&HSTRING::from(dir), Some(&mut free), None, None) };
    }
    free
}

/// Space on the drive a folder is on, which may not exist yet.
#[tauri::command]
fn disk(folder: PathBuf) -> Result<Disk, String> {
    let found =
        folder.ancestors().find(|dir| dir.is_dir()).ok_or_else(|| format!("{} is on no drive", folder.display()))?;
    let (mut free, mut total) = (0, 0);
    unsafe { GetDiskFreeSpaceExW(&HSTRING::from(found), Some(&mut free), Some(&mut total), None) }
        .map_err(|e| e.to_string())?;
    Ok(Disk { free, total })
}

fn size(bytes: u64) -> String {
    match bytes {
        1_000_000_000.. => format!("{:.2} GB", bytes as f64 / 1e9),
        _ => format!("{:.1} MB", bytes as f64 / 1e6),
    }
}

#[tauri::command]
fn save_clip(shared: State<Shared>) {
    post(shared.recorder, WM_HOTKEY, SAVE);
}

#[tauri::command]
fn mark_clip(shared: State<Shared>) {
    post(shared.recorder, WM_HOTKEY, MARK);
}

#[tauri::command]
fn take_shot(shared: State<Shared>) {
    post(shared.recorder, WM_HOTKEY, SHOT);
}

#[tauri::command]
fn toggle_mic(shared: State<Shared>) {
    post(shared.recorder, WM_HOTKEY, MIC);
}

#[tauri::command]
fn toggle_recording(shared: State<Shared>) {
    post(shared.recorder, WM_HOTKEY, RECORD);
}

/// A clip in the clips folder, which clip may change.
fn clip_in_folder(shared: &Shared, path: &Path) -> Result<(), String> {
    in_folder(shared, path, &["mp4"], false)
}

/// A clip, or an image saved from a game or a clip, in the clips folder.
fn saved_in_folder(shared: &Shared, path: &Path) -> Result<(), String> {
    in_folder(shared, path, &["mp4", "png", "gif"], false)
}

/// Anything the library lists, also from other apps' folders, which clip only reads. Only these get
/// handed to explorer, which would run anything else.
fn listed(shared: &Shared, path: &Path) -> Result<(), String> {
    in_folder(shared, path, &["mp4", "png", "gif"], true)
}

fn in_folder(shared: &Shared, path: &Path, kinds: &[&str], sources: bool) -> Result<(), String> {
    let settings = shared.settings.lock().expect("settings poisoned");
    let parent = path.parent();
    let under =
        |root: &PathBuf| parent == Some(root.as_path()) || parent.and_then(Path::parent) == Some(root.as_path());
    let shown = !path.components().any(|part| part == Component::ParentDir)
        && path.extension().is_some_and(|e| kinds.iter().any(|kind| e.eq_ignore_ascii_case(kind)))
        && path.is_file();
    let theirs = settings.sources.iter().any(under);
    if shown && (under(&settings.folder) || sources && theirs) {
        Ok(())
    } else if shown && theirs {
        Err(format!("{} is in another app's folder, clip only reads it", path.display()))
    } else {
        Err(format!("{} is not a clip", path.display()))
    }
}

/// The folder files made from `path` go in, made if it's new.
fn home(shared: &Shared, path: &Path) -> Result<PathBuf, String> {
    let folder = shared.settings.lock().expect("settings poisoned").folder.clone();
    let dir = library::home(&folder, path);
    std::fs::create_dir_all(&dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
    Ok(dir)
}

#[derive(Serialize)]
struct Found {
    name: &'static str,
    path: PathBuf,
}

/// The folders other clipping apps save to by default that this pc has.
#[tauri::command]
fn other_folders(app: AppHandle) -> Vec<Found> {
    let paths = app.path();
    let (videos, documents) = (paths.video_dir().unwrap_or_default(), paths.document_dir().unwrap_or_default());
    let medal =
        [PathBuf::from(r"C:\Medal\Clips"), PathBuf::from(r"C:\Medal"), videos.join("Medal"), documents.join("Medal")];
    let real = |dir: &PathBuf| dir.is_absolute() && dir.is_dir();
    [
        ("Medal", medal.into_iter().find(real)),
        ("Outplayed", Some(videos.join("Overwolf").join("Outplayed"))),
        ("Xbox Game Bar", Some(videos.join("Captures"))),
        ("NVIDIA App", Some(videos)),
    ]
    .into_iter()
    .filter_map(|(name, path)| Some(Found { name, path: path.filter(real)? }))
    .collect()
}

/// Copies the clips and images of one of the other folders into the clips folder, sending
/// `import-progress` in percent. Returns how many were copied.
#[tauri::command]
async fn import_folder(app: AppHandle, shared: State<'_, Shared>, source: PathBuf) -> Result<usize, String> {
    let folder = {
        let settings = shared.settings.lock().expect("settings poisoned");
        if !settings.sources.contains(&source) {
            return Err(format!("{} isn't one of the other folders", source.display()));
        }
        settings.folder.clone()
    };
    let progress = app.clone();
    let copied = blocking(move || {
        library::import(&source, &folder, |done, total| {
            let _ = progress.emit("import-progress", done * 100 / total.max(1));
        })
    })
    .await;
    let _ = app.emit("clips", ());
    copied
}

#[tauri::command]
fn open(shared: State<Shared>, path: PathBuf) -> Result<(), String> {
    listed(&shared, &path)?;
    Command::new("explorer").arg(&path).spawn().map(drop).map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal(shared: State<Shared>, path: PathBuf) -> Result<(), String> {
    listed(&shared, &path)?;
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

#[derive(Serialize)]
struct Mic {
    id: String,
    name: String,
}

#[tauri::command]
async fn mics() -> Result<Vec<Mic>, String> {
    let mics = crate::audio::mics().map_err(|e| e.to_string())?;
    Ok(mics.into_iter().map(|(id, name)| Mic { id, name }).collect())
}

/// How loud a microphone is right now, 0 to 1, from an empty id's default one.
#[tauri::command]
async fn mic_level(device: String) -> Result<f32, String> {
    blocking(move || crate::audio::mic_peak(&device).map_err(|e| e.to_string())).await
}

#[tauri::command]
async fn windowed_apps() -> Vec<String> {
    focus::windowed()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescape_reads_what_encode_uri_component_writes() {
        assert_eq!(unescape("D%3A%5Cclips%5Cpython%20%C3%A9.mp4").as_deref(), Some(r"D:\clips\python é.mp4"));
        assert_eq!(unescape("50%"), None);
        assert_eq!(unescape("%zz"), None);
        assert_eq!(unescape("%C3"), None);
    }

    #[test]
    fn a_marked_clip_saves_after_the_newest_mark_while_it_still_holds_the_first() {
        let (first, s) = (Instant::now(), Duration::from_secs(1));
        let buffer = 30 * s;
        assert_eq!(due(first, first, buffer), first + 15 * s);
        assert_eq!(due(first, first + 10 * s, buffer), first + 25 * s);
        assert_eq!(due(first, first + 20 * s, buffer), first + 28 * s);
        assert_eq!(due(first, first + 29 * s, buffer), first + 29 * s);
        assert_eq!(due(first, first, 10 * s), first + 5 * s);
    }

    #[test]
    fn the_saved_sound_scales_and_clips() {
        let wav = |samples: &[i16]| {
            let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
            let mut wav = b"RIFF\0\0\0\0WAVEfmt \x02\0\0\0abLIST\x03\0\0\0xyz\0data".to_vec();
            wav.extend((data.len() as u32).to_le_bytes());
            wav.extend(data);
            wav
        };
        let input = wav(&[1000, -1000, 30000, -30000]);
        assert_eq!(louder(&input, 100), input);
        assert_eq!(louder(&input, 50), wav(&[500, -500, 15000, -15000]));
        assert_eq!(louder(&input, 150), wav(&[1500, -1500, i16::MAX, i16::MIN]));
    }

    #[test]
    fn old_folders_move_once_and_never_over_new_ones() {
        let root = std::env::temp_dir().join(format!("clip-migrate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["memory", "memory-test", "lt.rot.clip-recorder"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
            std::fs::write(root.join(dir).join("settings.json"), dir).unwrap();
        }
        std::fs::create_dir_all(root.join("lt.rot.memory")).unwrap();
        let moves = [
            ("memory", "clip-recorder"),
            ("memory-test", "clip-recorder-test"),
            ("lt.rot.memory", "lt.rot.clip-recorder"),
            ("gone", "new"),
        ]
        .map(|(from, to)| (root.join(from), root.join(to)));
        migrate(&moves).unwrap();
        let read = |dir: &str| std::fs::read_to_string(root.join(dir).join("settings.json")).unwrap();
        assert_eq!(
            (read("clip-recorder"), read("clip-recorder-test"), read("lt.rot.clip-recorder")),
            ("memory".into(), "memory-test".into(), "lt.rot.clip-recorder".into())
        );
        assert!(!root.join("memory").exists() && root.join("lt.rot.memory").exists() && !root.join("new").exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}

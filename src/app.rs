use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
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
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS, HWND, LPARAM, WPARAM};
use windows::Win32::Media::Audio::{PlaySoundW, SND_MEMORY, SND_NODEFAULT};
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegDeleteTreeW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ,
};
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
use crate::ring::{room, Spool, IN_MEMORY, ROOM, SECOND};
use crate::update::{self, Update};
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
// a game reported a big play, waiting in the plays channel
const WM_PLAY: u32 = WM_APP + 4;
const WAITING: &str = "waiting for a game";
const PAUSED: &str = "paused";
const SAVE: usize = 1;
const MARK: usize = 2;
const SHOT: usize = 3;
const MIC: usize = 4;
const RECORD: usize = 5;
const REWIND: usize = 6;
// the tail of a whole buffer saved to be cut down, which its cuts drop from their names
const REWOUND: &str = " rewind";
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
    #[serde(deserialize_with = "known")]
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
    #[serde(deserialize_with = "or_default")]
    notice_corner: notice::Corner,
    /// ids of the games whose big plays save a clip
    auto: Vec<String>,
    /// seconds an auto clip keeps before the first play and after the last
    auto_before: u32,
    auto_after: u32,
    /// what the valve games' cfg proves itself with
    gsi_token: String,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
struct Hotkey {
    action: Action,
    /// MOD_ALT, MOD_CONTROL, MOD_SHIFT and MOD_WIN bits
    #[serde(default)]
    mods: u32,
    /// a virtual key code, 0 for none
    #[serde(default)]
    key: u32,
    /// how much of the buffer a save keeps, 0 for all of it
    #[serde(default)]
    seconds: u32,
}

/// Keeps the entries that parse, so a hotkey of an action clip doesn't know drops alone.
fn known<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned>(from: D) -> Result<Vec<T>, D::Error> {
    let all = Vec::<serde_json::Value>::deserialize(from)?;
    Ok(all.into_iter().filter_map(|one| serde_json::from_value(one).ok()).collect())
}

fn or_default<'de, D: serde::Deserializer<'de>, T: serde::de::DeserializeOwned + Default>(
    from: D,
) -> Result<T, D::Error> {
    Ok(serde_json::from_value(serde_json::Value::deserialize(from)?).unwrap_or_default())
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Action {
    Save,
    Mark,
    Shot,
    Mic,
    Record,
    Rewind,
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
            auto: Vec::new(),
            auto_before: 15,
            auto_after: 5,
            gsi_token: String::new(),
        }
    }
}

impl Settings {
    fn options(&self) -> Options {
        Options {
            buffer: self.buffer.clamp(5, 7200),
            fps: self.fps,
            mbps: self.mbps,
            height: self.height,
            filter: hdr::Filter::Area,
            software: false,
            hdr10: self.hdr10,
            spool: Some(spool(&self.folder)),
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
    /// no hardware encoder was found, so the cpu encodes at 720p30
    on_cpu: bool,
    /// the encoder, size, rate and colour being recorded
    summary: String,
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
    /// made when the quick panel first asks
    meter: Mutex<Option<crate::cost::Meter>>,
    /// why the valve games can't be heard, if they can't
    deaf: Option<String>,
    /// files that couldn't be read at start, for the window to say once
    damaged: Mutex<Vec<String>>,
    /// the last update check and the installer to update with
    update: Arc<Mutex<(Update, Option<update::Owner>)>>,
}

/// Runs clip from the tray, keeping the last stretch of the focused app ready to save.
pub fn run(restarted: bool) {
    if let Err(e) = serve(restarted) {
        unsafe { MessageBoxW(None, &HSTRING::from(e), w!("clip"), MB_ICONERROR) };
    }
}

/// `restarted` after a crash, when the window stays hidden so a game in front keeps its screen
fn serve(restarted: bool) -> Result<(), String> {
    // below normal, not background mode, which would also let windows page out the buffer
    let _ = unsafe { SetPriorityClass(GetCurrentProcess(), BELOW_NORMAL_PRIORITY_CLASS) };
    // a profile runs a separate clip beside the usual one, with its own settings and single instance
    let suffix = match std::env::var("CLIP_PROFILE") {
        Ok(p) if !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') => format!("-{p}"),
        _ => String::new(),
    };
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
            damaged,
            log_folder,
            system_info,
            update_state,
            install_update,
            apply,
            clips,
            save_clip,
            mark_clip,
            take_shot,
            toggle_mic,
            toggle_recording,
            rewind,
            cost,
            auto_games,
            open,
            reveal,
            audio_apps,
            app_icons,
            windowed_apps,
            star,
            rename,
            delete,
            set_marks,
            save_frame,
            copy_files,
            drag_out,
            save_thumb,
            track_audio,
            export,
            join,
            cancel_export,
            restore_original,
            drop_original,
            game_art,
            pause,
            hold_hotkeys,
            key_names,
            preview_sound,
            mics,
            mic_level,
            test_notification,
            disk,
            other_folders,
            import_folder
        ])
        .setup(move |app| {
            let dir = app.path().config_dir()?.join(&name);
            let file = dir.join("settings.json");
            let marks_file = dir.join("library.json");
            let cache = app.path().cache_dir()?.join(&name);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::create_dir_all(&cache);
            crate::log::open(&cache.join("clip.log"));
            crate::crash::guard(&cache);
            // not the whole cache, which holds crash dumps and the log
            for part in ["thumbs", "art", "icons", "app-icons", "audio"] {
                app.asset_protocol_scope().allow_directory(cache.join(part), true)?;
            }
            let icon = cache.join("icon.png");
            if let Err(e) = std::fs::write(&icon, include_bytes!("../icons/icon.png"))
                .map_err(|e| e.to_string())
                .and_then(|()| toast::register(&app.config().identifier, "clip", &icon).map_err(|e| e.to_string()))
            {
                crate::log::write(format_args!("notifications can't show clip's name and icon: {e}"));
            }
            let mut damaged = Vec::new();
            let stored = library::read_json::<Settings>(&file).unwrap_or_else(|e| {
                damaged.push(format!("Settings were reset: {e}"));
                None
            });
            let first = stored.is_none() && damaged.is_empty();
            let had = stored.is_some();
            let mut settings = stored.unwrap_or_default();
            let default_folder = app.path().video_dir()?.join("clip-recorder");
            if settings.folder.as_os_str().is_empty() {
                settings.folder = default_folder.clone();
            }
            let (mut library, problem) = Marks::load(&marks_file);
            damaged.extend(problem.map(|e| format!("Stars and marks were reset: {e}")));
            library.seed(&default_folder);
            if let Err(e) = library.store(&marks_file) {
                crate::log::write(e);
            }
            damaged.iter().for_each(crate::log::write);
            let marks = Arc::new(Mutex::new(library));
            // a cfg carries the token, so a fresh one each start would leave the games unheard
            if settings.gsi_token.is_empty() {
                settings.gsi_token = crate::events::token();
                // before the first apply there's no file, and no game to hear
                let saved = serde_json::to_string_pretty(&settings).map_err(|e| e.to_string());
                if let Err(e) = saved.and_then(|json| match had {
                    true => library::write_json(&file, &json).map_err(|e| e.to_string()),
                    false => Ok(()),
                }) {
                    crate::log::write(format_args!("can't save the game token: {e}"));
                }
            }
            // the games read their cfg at launch, and a steam library that moved leaves an old one behind
            for game in crate::events::GAMES.iter().filter(|game| settings.auto.iter().any(|id| id == game.id)) {
                if let Err(e) = game.connect(true, &settings.gsi_token) {
                    crate::log::write(e);
                }
            }
            app.asset_protocol_scope().allow_directory(&settings.folder, true)?;
            for source in &settings.sources {
                app.asset_protocol_scope().allow_directory(source, true)?;
            }
            if settings.start_with_windows {
                if let Err(e) = run_at_login(&name, true) {
                    crate::log::write(format_args!("can't start with windows: {e}"));
                }
            }
            let pause = CheckMenuItem::with_id(app, "pause", "pause recording", true, false, None::<&str>)?;
            let taping = MenuItem::with_id(app, "record", "start recording", true, None::<&str>)?;
            let mute = CheckMenuItem::with_id(app, "mic", "mute microphone", true, false, None::<&str>)?;
            let menu = Menu::with_items(
                app,
                &[
                    &MenuItem::with_id(app, "save", "save clip", true, None::<&str>)?,
                    &MenuItem::with_id(app, "rewind", "rewind the buffer", true, None::<&str>)?,
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
            let (heard, plays) = mpsc::channel();
            {
                let (app, settings, status) = (app.handle().clone(), settings.clone(), status.clone());
                let library = (marks.clone(), marks_file.clone(), cache.join("thumbs"));
                let tray = Tray { icon: tray, pause, taping, mute };
                std::thread::spawn(move || record(&app, &tray, &settings, &status, &library, ready, plays));
            }
            let open_on_start = settings.lock().expect("settings poisoned").open_on_start;
            let recorder = recorder.recv()?;
            let send = move |game: &'static crate::events::Game, play: String| {
                if heard.send((game, play)).is_ok() {
                    post(recorder, WM_PLAY, 0);
                }
            };
            let deaf = match crate::events::bind() {
                Ok(listener) => {
                    let settings = settings.clone();
                    let token = move || settings.lock().expect("settings poisoned").gsi_token.clone();
                    crate::events::listen(listener, token, send.clone());
                    None
                }
                Err(e) => Some(format!("port {} is taken: {e}", crate::events::PORT)),
            };
            {
                let (settings, status) = (settings.clone(), status.clone());
                let league = crate::events::game("league").expect("league is listed");
                crate::events::watch_league(
                    move || {
                        status.lock().expect("status poisoned").exe == league.exe
                            && settings.lock().expect("settings poisoned").auto.iter().any(|id| id == league.id)
                    },
                    send,
                );
            }
            let update = Arc::new(Mutex::new((Update::default(), None)));
            match cfg!(debug_assertions) {
                true => update.lock().expect("update poisoned").0.error = "a dev build doesn't update".into(),
                false => {
                    let (update, handle) = (update.clone(), app.handle().clone());
                    std::thread::spawn(move || loop {
                        let (found, owner) = match update::check() {
                            Ok((found, owner)) => (found, Some(owner)),
                            Err(error) => {
                                crate::log::write(format_args!("update check failed: {error}"));
                                (Update { error, ..Update::default() }, None)
                            }
                        };
                        let changed = {
                            let mut held = update.lock().expect("update poisoned");
                            std::mem::replace(&mut *held, (found.clone(), owner)).0 != found
                        };
                        if changed {
                            let _ = handle.emit("update", &found);
                        }
                        std::thread::sleep(update::EVERY);
                    });
                }
            }
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
                meter: Mutex::default(),
                deaf,
                damaged: Mutex::new(damaged),
                update,
            });
            app.on_menu_event(|app, event| {
                let shared = app.state::<Shared>();
                match event.id.as_ref() {
                    "save" => post(shared.recorder, WM_HOTKEY, SAVE),
                    "shot" => post(shared.recorder, WM_HOTKEY, SHOT),
                    "record" => post(shared.recorder, WM_HOTKEY, RECORD),
                    "rewind" => post(shared.recorder, WM_HOTKEY, REWIND),
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
            if !restarted && (first || open_on_start) {
                show(app.handle());
            }
            Ok(())
        })
        .build(context)
        .map_err(|e| e.to_string())?
        .run(|app, event| match event {
            // a key box focused as the window went holds the hotkeys off, so they come back with it
            RunEvent::WindowEvent { event: WindowEvent::Destroyed, .. } => {
                post(app.state::<Shared>().recorder, WM_HOLD, 0);
            }
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
        if let Err(e) = WebviewWindowBuilder::from_config(&app, &config).and_then(|window| window.build()) {
            crate::log::write(format_args!("the window didn't open: {e}"));
        }
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
    let json = serde_json::to_string(&placed).expect("a placement is json");
    if let Err(e) = library::write_json(&placement_file(app), &json) {
        crate::log::write(format_args!("can't keep where the window was: {e}"));
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

/// A session and the app window it records.
struct Recording {
    session: Session,
    window: HWND,
    app: String,
    exe: String,
}

/// A clip of big plays a game reported, saving itself once `due`.
struct Auto {
    recording: Rc<Recording>,
    marks: Vec<i64>,
    first: Instant,
    due: Instant,
    /// the latest play, which names the clip
    play: String,
    before: u32,
}

impl Auto {
    fn seconds(&self) -> Option<u32> {
        Some(self.before + self.first.elapsed().as_secs_f64().ceil() as u32)
    }

    fn tail(&self) -> String {
        format!(" {}", self.play)
    }
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
    fn mark(&mut self, at: i64, now: Instant) {
        self.marks.push(at);
        self.due = due(self.first, now, Duration::from_secs(self.recording.session.buffer() as u64));
    }

    /// How much the clip keeps: all of a buffer in memory, or of one on disk from a minute before
    /// the first mark, which spares writing out hours for one moment.
    fn seconds(&self) -> Option<u32> {
        (self.recording.session.buffer() > IN_MEMORY).then(|| self.first.elapsed().as_secs() as u32 + 60)
    }
}

/// The hidden folder in the clips folder that a buffer too long for memory goes to.
fn spool(folder: &Path) -> PathBuf {
    folder.join(".buffer")
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
    plays: mpsc::Receiver<(&'static crate::events::Game, String)>,
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
        REWIND => Some((Action::Rewind, 0)),
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
    // the reason the buffer is short that the status last showed, which a failing drive changes
    let told = RefCell::new(None);
    let report = |recording: &Result<Rc<Recording>, String>| {
        *told.borrow_mut() = recording.as_ref().ok().and_then(|recording| recording.session.short());
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
                message: format!(
                    "keeping the last {}{}{taken}",
                    length(recording.session.buffer()),
                    recording
                        .session
                        .short()
                        .map_or(String::new(), |why| format!(", a longer buffer won't fit: {why}"))
                ),
                paused: false,
                hotkeys,
                mic_muted,
                taping,
                on_cpu: recording.session.on_cpu,
                summary: recording.session.summary.clone(),
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
    // writes the buffer into the game's folder with its marks, and lets the library know
    let keep = |recording: &Recording, marked: &[i64], seconds: Option<u32>, tail: &str| {
        let (folder, cap, keep_days) = {
            let settings = settings.lock().expect("settings poisoned");
            (settings.folder.clone(), settings.cap, settings.keep_days)
        };
        let dir = folder.join(&recording.app);
        let (saved, out) = std::fs::create_dir_all(&dir)
            .map_err(|e| format!("can't make {}: {e}", dir.display()))
            .map(|()| free(&dir, tail, "mp4"))
            .and_then(|out| Ok((recording.session.save(&out, seconds)?, out)))?;
        let mut marks = marks.lock().expect("marks poisoned");
        let at: Vec<f64> = marked
            .iter()
            .map(|&at| (at - saved.start) as f64 / SECOND as f64)
            .filter(|at| (0.0..=saved.seconds).contains(at))
            .collect();
        marks.mark(&folder, &out, at);
        marks.wrote(&out);
        trim(&folder, &mut marks, marks_file, thumbs, cap, keep_days);
        let _ = app.emit("clips", ());
        Ok::<_, String>((saved, out))
    };
    let save = |recording: &Recording, marked: &[i64], seconds: Option<u32>, tail: &str| {
        let (sound, notify) = {
            let settings = settings.lock().expect("settings poisoned");
            (settings.sound.then_some(settings.sound_volume), settings.notify)
        };
        let saved = keep(recording, marked, seconds, tail);
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
        if let Err(e) = saved {
            let _ = tray.set_tooltip(Some(format!("clip: {e}")));
        }
    };
    {
        let (folder, cap, keep_days) = {
            let settings = settings.lock().expect("settings poisoned");
            (settings.folder.clone(), settings.cap, settings.keep_days)
        };
        library::settle(&folder);
        Spool::clear(&spool(&folder));
        trim(&folder, &mut marks.lock().expect("marks poisoned"), marks_file, thumbs, cap, keep_days);
    }
    let shoot = |recording: &Recording| {
        let folder = settings.lock().expect("settings poisoned").folder.clone();
        let dir = folder.join(&recording.app);
        let taken = recording.session.still().and_then(|still| {
            std::fs::create_dir_all(&dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
            let out = free(&dir, "", "png");
            focus::png(still.width, still.height, &still.bgra(), &out).map_err(|e| e.to_string())?;
            wrote(marks, marks_file, &out);
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
        let Taping { tape, app: game, window, out, .. } = taping;
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
                let mut marks = marks.lock().expect("marks poisoned");
                marks.wrote(&out);
                trim(&folder, &mut marks, marks_file, thumbs, cap, keep_days);
                let _ = app.emit("clips", ());
            }
            Err(e) => feedback(format!("Recording not saved: {e}"), true, window),
        }
    };
    let mut current: Result<Rc<Recording>, String> = Err(WAITING.into());
    let mut previous: Option<(Rc<Recording>, Instant)> = None;
    let mut pending: Option<Pending> = None;
    let mut auto: Option<Auto> = None;
    let mut follow = Follow::default();
    let mut refused = None;
    let mut restart = true;
    // when sessions that ended on their own were started again, at most three a minute
    let mut revived: Vec<Instant> = Vec::new();
    loop {
        if restart {
            restart = false;
            if let Some(early) = pending.take() {
                save(&early.recording, &early.marks, early.seconds(), "");
            }
            if let Some(early) = auto.take() {
                save(&early.recording, &early.marks, early.seconds(), &early.tail());
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
            if let Some((why, window)) = current.as_ref().ok().and_then(|r| Some((r.session.dead()?, r.window))) {
                crate::log::write(format_args!("recording ended on its own: {why}"));
                if revive(&mut revived, Instant::now()) {
                    restart = true;
                } else {
                    current = Err(format!("{why}, recording stopped after starting again 3 times in a minute"));
                    refused = Some(window);
                    report(&current);
                }
            }
            if current.as_ref().ok().and_then(|recording| recording.session.short()) != *told.borrow() {
                report(&current);
            }
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
            WM_PLAY => {
                let (on, before, after) = {
                    let settings = settings.lock().expect("settings poisoned");
                    (settings.auto.clone(), settings.auto_before, settings.auto_after)
                };
                for (game, play) in plays.try_iter() {
                    // a play only counts in its own game's recording, while that game is switched on
                    let Some(recording) =
                        current.as_ref().ok().filter(|r| r.exe == game.exe && on.iter().any(|id| id == game.id))
                    else {
                        continue;
                    };
                    let (now, at) = (Instant::now(), recording.session.clock.now());
                    if let Some(other) = auto.take_if(|a| !Rc::ptr_eq(&a.recording, recording)) {
                        save(&other.recording, &other.marks, other.seconds(), &other.tail());
                    }
                    // plays close together make one clip, named after the last
                    let clip = auto.get_or_insert_with(|| Auto {
                        recording: recording.clone(),
                        marks: Vec::new(),
                        first: now,
                        due: now,
                        play: String::new(),
                        before,
                    });
                    clip.marks.push(at);
                    clip.due = now + Duration::from_secs(after as u64);
                    clip.play = play;
                }
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
                    save(&due.recording, &due.marks, due.seconds(), "");
                }
                if let Some(due) = auto.take_if(|due| due.due <= Instant::now()) {
                    save(&due.recording, &due.marks, due.seconds(), &due.tail());
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
            WM_HOTKEY if pressed(&msg).is_some_and(|(action, _)| action == Action::Rewind) => {
                match &current {
                    Ok(recording) => {
                        let marked = pending.as_ref().filter(|p| Rc::ptr_eq(&p.recording, recording));
                        // a buffer on disk rewinds its last part, the whole one can still be saved
                        let seconds = (recording.session.buffer() > IN_MEMORY).then_some(IN_MEMORY);
                        match keep(recording, marked.map_or(&[], |p| &p.marks), seconds, REWOUND) {
                            Ok((_, out)) => {
                                show(app);
                                let _ = app.emit("rewind", out);
                            }
                            Err(e) => feedback(format!("Couldn't rewind: {e}"), true, recording.window),
                        }
                    }
                    Err(e) => feedback(format!("Nothing to rewind: {e}"), true, HWND::default()),
                }
                None
            }
            WM_HOTKEY if pressed(&msg).is_some_and(|(action, _)| action == Action::Mark) => {
                match &current {
                    Ok(recording) => {
                        let (now, at) = (Instant::now(), recording.session.clock.now());
                        // marks in another app's recording make their clip now
                        if let Some(other) = pending.take_if(|p| !Rc::ptr_eq(&p.recording, recording)) {
                            save(&other.recording, &other.marks, other.seconds(), "");
                        }
                        pending
                            .get_or_insert_with(|| Pending {
                                recording: recording.clone(),
                                marks: Vec::new(),
                                first: now,
                                due: now,
                            })
                            .mark(at, now);
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
                        save(recording, marked.map_or(&[], |p| &p.marks), seconds, "");
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

/// Whether a session that ended on its own starts again, which it does three times a minute at most.
fn revive(revived: &mut Vec<Instant>, now: Instant) -> bool {
    revived.retain(|&at| now.duration_since(at) < Duration::from_secs(60));
    let again = revived.len() < 3;
    if again {
        revived.push(now);
    }
    again
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

/// Records `out` as a file clip wrote, which cleanup may delete later.
fn wrote(marks: &Mutex<Marks>, marks_file: &Path, out: &Path) {
    let mut marks = marks.lock().expect("marks poisoned");
    marks.wrote(out);
    if let Err(e) = marks.store(marks_file) {
        crate::log::write(e);
    }
}

/// Deletes the oldest unstarred clips past `cap` GB or older than `keep_days`, 0 being no limit, then
/// stores the library without them.
fn trim(folder: &Path, marks: &mut Marks, marks_file: &Path, thumbs: &Path, cap: u32, keep_days: u32) {
    if cap != 0 || keep_days != 0 {
        let now = std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |now| now.as_millis() as u64);
        let before = match keep_days {
            0 => 0,
            days => now.saturating_sub(days as u64 * 86_400_000),
        };
        // gigabytes as explorer and the window count them
        let cap = if cap == 0 { u64::MAX } else { (cap as u64) << 30 };
        for gone in library::trim(&library::list(folder, &[], marks, thumbs), marks, cap, before) {
            marks.forget(folder, &gone);
        }
    }
    if let Err(e) = marks.store(marks_file) {
        crate::log::write(e);
    }
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

/// What the rot installer runs before it removes clip: the run value, the notification id and the games' cfgs.
pub fn uninstall() -> Result<(), String> {
    let name = "clip-recorder";
    let mut failed = Vec::new();
    if let Err(e) = run_at_login(name, false) {
        failed.push(format!("can't remove the start with windows value: {e}"));
    }
    let id = HSTRING::from(toast::key(&format!("lt.rot.{name}")));
    match unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, &id) } {
        ERROR_SUCCESS | ERROR_FILE_NOT_FOUND => {}
        other => failed.push(format!("can't remove the notification id: {}", other.to_hresult().message())),
    }
    failed.extend(crate::events::GAMES.iter().filter_map(|game| game.connect(false, "").err()));
    match failed.is_empty() {
        true => Ok(()),
        false => Err(failed.join("\n")),
    }
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

/// What couldn't be read at start, said once.
#[tauri::command]
fn damaged(shared: State<Shared>) -> Vec<String> {
    std::mem::take(&mut *shared.damaged.lock().expect("damaged poisoned"))
}

#[tauri::command]
fn log_folder(shared: State<Shared>) -> Result<(), String> {
    Command::new("explorer").arg(&shared.cache).spawn().map(drop).map_err(|e| e.to_string())
}

/// clip's version, the windows build, the gpu and its encoders, and what's being recorded, for a bug report.
#[tauri::command]
async fn system_info(shared: State<'_, Shared>) -> Result<String, String> {
    let summary = shared.status.lock().expect("status poisoned").summary.clone();
    let machine = blocking(|| crate::machine().map_err(|e| e.message())).await;
    let mut lines = vec![format!("clip {}", env!("CARGO_PKG_VERSION")), windows_build()];
    lines.extend(machine.unwrap_or_else(|e| vec![format!("no gpu: {e}")]));
    lines.push(match summary.is_empty() {
        true => "not recording".into(),
        false => format!("recording {summary}"),
    });
    Ok(lines.join("\n"))
}

#[tauri::command]
fn update_state(shared: State<Shared>) -> Update {
    shared.update.lock().expect("update poisoned").0.clone()
}

/// Quits clip for the installer, which opens it again once it's updated.
#[tauri::command]
fn install_update(app: AppHandle, shared: State<Shared>) -> Result<(), String> {
    let owner = {
        let held = shared.update.lock().expect("update poisoned");
        held.1.clone().filter(|_| held.0.available)
    };
    update::install(&owner.ok_or("there's no update to install")?)?;
    app.exit(0);
    Ok(())
}

fn windows_build() -> String {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ};
    let key = w!("SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion");
    let text = |name| {
        let mut value = [0u16; 128];
        let mut size = (value.len() * 2) as u32;
        let read = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key,
                name,
                RRF_RT_REG_SZ,
                None,
                Some(value.as_mut_ptr().cast()),
                Some(&mut size),
            )
        };
        match read.is_ok() {
            true => String::from_utf16_lossy(&value[..(size as usize / 2).saturating_sub(1)]),
            false => String::new(),
        }
    };
    let mut ubr = 0u32;
    let mut size = 4u32;
    let _ = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            key,
            w!("UBR"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut ubr).cast()),
            Some(&mut size),
        )
    };
    format!("windows {} build {}.{ubr}", text(w!("DisplayVersion")), text(w!("CurrentBuild")))
}

#[tauri::command]
fn apply(app: AppHandle, shared: State<Shared>, mut settings: Settings) -> Result<(), String> {
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
    if !(5..=60).contains(&settings.auto_before) || settings.auto_after > 30 {
        return Err("auto clips keep 5 to 60 seconds before a play and up to 30 after".into());
    }
    let (was, token) = {
        let current = shared.settings.lock().expect("settings poisoned");
        (current.auto.clone(), current.gsi_token.clone())
    };
    // the token is clip's own, the window only carries it back
    settings.gsi_token = token;
    let save = |settings: &Settings| {
        let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
        library::write_json(&shared.file, &json).map_err(|e| format!("can't save settings: {e}"))
    };
    save(&settings)?;
    let mut failed = Vec::new();
    for game in &crate::events::GAMES {
        let on = settings.auto.iter().any(|id| id == game.id);
        if on != was.iter().any(|id| id == game.id) {
            if let Err(e) = game.connect(on, &settings.gsi_token) {
                failed.push(e);
                // a game whose cfg can't change keeps its old switch, so the setting says what's on disk
                match on {
                    true => settings.auto.retain(|id| id != game.id),
                    false => settings.auto.push(game.id.into()),
                }
            }
        }
    }
    if !failed.is_empty() {
        save(&settings)?;
    }
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
    match failed.is_empty() {
        true => Ok(()),
        false => Err(failed.join("; ")),
    }
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
    // a folder that's away for a moment lists nothing, which mustn't drop every game's art
    if folder.is_dir() {
        let live = shared.status.lock().expect("status poisoned").app.clone();
        library::prune_apps(&thumbs, clips.iter().map(|clip| clip.app.as_str()).chain([live.as_str()]));
    }
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
    wrote(&shared.marks, &shared.marks_file, &out);
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

/// Hands the files to windows to drag out, from a dragstart in the library while the button is still down.
#[tauri::command]
async fn drag_out(window: WebviewWindow, shared: State<'_, Shared>, paths: Vec<PathBuf>) -> Result<(), String> {
    paths.iter().try_for_each(|path| listed(&shared, path))?;
    // a window handle isn't Send, its value is
    let owner = window.hwnd().map_err(|e| e.to_string())?.0 as isize;
    let (done, dropped) = mpsc::channel();
    // ole drags run on the thread that owns the window
    window
        .run_on_main_thread(move || {
            let _ = done.send(crate::clipboard::drag_files(HWND(owner as _), &paths));
        })
        .map_err(|e| e.to_string())?;
    dropped.recv().map_err(|e| e.to_string())?
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
    /// the loudest sample of each of `PEAKS` equal stretches, from 0 to 1
    peaks: Vec<f32>,
}

/// How many bars the editor draws a track's sound in.
const PEAKS: usize = 800;

/// Decodes every audio track of a clip to its own wav, since the webview only plays the first.
#[tauri::command]
async fn track_audio(shared: State<'_, Shared>, path: PathBuf) -> Result<Vec<Track>, String> {
    listed(&shared, &path)?;
    let audio = shared.cache.join("audio");
    blocking(move || {
        // only the clip open now keeps its decoded audio
        let _ = std::fs::remove_dir_all(&audio);
        std::fs::create_dir_all(&audio).map_err(|e| e.to_string())?;
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let mut tracks = Vec::new();
        crate::export::each_track(&path, |name, pcm| {
            let wav = audio.join(format!("{stem} {}.wav", tracks.len()));
            crate::export::write_wav(&wav, &pcm).map_err(|e| e.to_string())?;
            tracks.push(Track { name, path: wav, peaks: crate::export::peaks(&pcm, PEAKS) });
            Ok(())
        })?;
        Ok(tracks)
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
    ranges: Vec<(f64, f64)>,
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
    if ranges.is_empty() || ranges.iter().any(|&(start, end)| !(start >= 0.0 && end > start)) {
        return Err("each part has to end after it starts".into());
    }
    if ranges.windows(2).any(|pair| pair[1].0 < pair[0].1) {
        return Err("the parts have to be in order and apart".into());
    }
    if fit.is_some_and(|bytes| bytes < 1_000_000) {
        return Err("the size has to be at least 1 MB".into());
    }
    CANCEL.store(false, Ordering::Relaxed);
    let progress = app.clone();
    let parts = ranges.clone();
    let out = blocking(move || {
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let kind = match (fit, portrait) {
            (Some(bytes), Some(_)) => format!("{} MB 9x16", bytes / 1_000_000),
            (Some(bytes), None) => format!("{} MB", bytes / 1_000_000),
            (None, Some(_)) => "9x16".into(),
            (None, None) => "trimmed".into(),
        };
        let base = match stem.strip_suffix(REWOUND) {
            Some(base) if kind == "trimmed" => base.to_string(),
            Some(base) => format!("{base} {kind}"),
            None => format!("{stem} {kind}"),
        };
        let out = (1..)
            .map(|n| match n {
                1 => dir.join(format!("{base}.mp4")),
                n => dir.join(format!("{base} {n}.mp4")),
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
        if let Err(e) = crate::export::trim(&path, &out, &parts, &tracks, mix || fit.is_some(), &job) {
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
            marks.mark(folder, &out, carried(&marks.marks(folder, &path), &ranges));
            // a replace keeps the clip's place and whether clip wrote it
            if out != path {
                marks.wrote(&out);
            }
        })?;
        Ok(out)
    });
    let _ = app.emit("clips", ());
    out
}

/// Joins `paths` in order into one new clip beside the first, with every clip's marks moved along.
#[tauri::command]
async fn join(app: AppHandle, shared: State<'_, Shared>, paths: Vec<PathBuf>) -> Result<PathBuf, String> {
    if paths.len() < 2 {
        return Err("pick two clips or more to join".into());
    }
    paths.iter().try_for_each(|path| in_folder(&shared, path, &["mp4"], true))?;
    let dir = home(&shared, &paths[0])?;
    CANCEL.store(false, Ordering::Relaxed);
    let progress = app.clone();
    let clips = paths.clone();
    let joined = blocking(move || {
        let out = free(&dir, " joined", "mp4");
        let sent = Cell::new(u32::MAX);
        let report = |done: f64| {
            let percent = (done.clamp(0.0, 1.0) * 100.0) as u32;
            if sent.replace(percent) != percent {
                let _ = progress.emit("export-progress", percent);
            }
        };
        let job = crate::export::Job { progress: Some(&report), cancel: Some(&CANCEL), ..Default::default() };
        match crate::export::join(&clips, &out, &job) {
            Ok(starts) => Ok((out, starts)),
            Err(e) => {
                let _ = std::fs::remove_file(&out);
                Err(e)
            }
        }
    })
    .await
    .and_then(|(out, starts)| {
        marked(&shared, |marks, folder| {
            let moved = paths
                .iter()
                .zip(starts)
                .flat_map(|(path, start)| marks.marks(folder, path).into_iter().map(move |at| start + at));
            let moved = moved.collect();
            marks.mark(folder, &out, moved);
            marks.wrote(&out);
        })?;
        Ok(out)
    });
    let _ = app.emit("clips", ());
    joined
}

/// Where `marks` land once only the `ranges` are kept, one after another.
fn carried(marks: &[f64], ranges: &[(f64, f64)]) -> Vec<f64> {
    let mut before = 0.0;
    let mut kept = Vec::new();
    for &(start, end) in ranges {
        kept.extend(marks.iter().filter(|at| (start..=end).contains(*at)).map(|at| before + at - start));
        before += end - start;
    }
    kept
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

/// What the punctuation keys print on this keyboard layout, which differs from the us names.
#[tauri::command]
fn key_names() -> BTreeMap<u32, String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_CHAR};
    (0xba..=0xc0)
        .chain(0xdb..=0xdf)
        .chain([0xe2])
        .filter_map(|vk| {
            // a dead key sets the top bit, and its low bits still hold the accent
            let printed = unsafe { MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR) } & 0xffff;
            char::from_u32(printed).filter(|c| !c.is_control()).map(|c| (vk, c.to_uppercase().collect()))
        })
        .collect()
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

/// Seconds the way the settings say them: 30 s, 10 min, 2 h.
fn length(seconds: u32) -> String {
    match seconds {
        0..60 => format!("{seconds} s"),
        60..3600 => format!("{} min", seconds as f64 / 60.0),
        _ => format!("{} h", seconds as f64 / 3600.0),
    }
}

fn size(bytes: u64) -> String {
    match bytes {
        0x4000_0000.. => format!("{:.2} GB", bytes as f64 / (1u64 << 30) as f64),
        _ => format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64),
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

#[tauri::command]
fn auto_games(shared: State<Shared>) -> Vec<crate::events::Found> {
    crate::events::GAMES.iter().map(|game| game.found(shared.deaf.as_deref())).collect()
}

#[tauri::command]
fn cost(shared: State<Shared>) -> crate::cost::Cost {
    shared.meter.lock().expect("meter poisoned").get_or_insert_with(crate::cost::Meter::new).read()
}

#[tauri::command]
fn rewind(shared: State<Shared>) {
    post(shared.recorder, WM_HOTKEY, REWIND);
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

/// Each exe's icon, saved from a running copy the first time and kept for when it isn't running.
#[tauri::command]
async fn app_icons(shared: State<'_, Shared>, exes: Vec<String>) -> Result<BTreeMap<String, PathBuf>, String> {
    let dir = shared.cache.join("app-icons");
    let kept: Vec<String> = {
        let settings = shared.settings.lock().expect("settings poisoned");
        settings.apps.iter().chain(&settings.record).chain(&settings.ignore).map(|exe| exe.to_lowercase()).collect()
    };
    blocking(move || {
        // the names become file names, so only bare ones are taken
        let exes: Vec<String> = (exes.into_iter().map(|exe| exe.to_lowercase()))
            .filter(|exe| Path::new(exe).file_name().is_some_and(|name| name == exe.as_str()))
            .collect();
        // icons nothing asks for and no setting names are only made again once that app runs
        let keep: HashSet<String> = exes.iter().chain(&kept).map(|exe| format!("{exe}.png")).collect();
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            if !keep.contains(&entry.file_name().to_string_lossy().to_lowercase()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        for (exe, pid) in crate::audio::running(&exes) {
            // an exe without an icon just shows the plain one
            let _ = focus::save_icon(pid, &dir.join(format!("{exe}.png")));
        }
        let icon =
            |exe: String| Some(dir.join(format!("{exe}.png"))).filter(|icon| icon.is_file()).map(|icon| (exe, icon));
        Ok(exes.into_iter().filter_map(icon).collect())
    })
    .await
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
    fn marks_follow_their_part_and_drop_with_the_cut() {
        assert_eq!(carried(&[0.5, 2.5, 4.5], &[(0.0, 1.0), (4.0, 5.0)]), [0.5, 1.5]);
        assert_eq!(carried(&[3.0], &[(1.0, 4.0)]), [2.0]);
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
    fn a_recording_that_keeps_dying_starts_again_three_times_a_minute() {
        let (start, s) = (Instant::now(), Duration::from_secs(1));
        let mut revived = Vec::new();
        assert!([0, 10, 20].iter().all(|&at| revive(&mut revived, start + at * s)));
        assert!(!revive(&mut revived, start + 30 * s));
        assert!(revive(&mut revived, start + 61 * s), "the first one is a minute old");
        assert!(!revive(&mut revived, start + 62 * s));
    }

    #[test]
    fn older_settings_keep_the_defaults_for_what_they_miss_and_drop_what_clip_no_longer_knows() {
        let old = r#"{"buffer": 120, "noticeCorner": "middle", "gone": 1,
            "hotkeys": [{"action": "save", "mods": 1, "key": 120}, {"action": "teleport", "key": 1}]}"#;
        let settings: Settings = serde_json::from_str(old).unwrap();
        let default = Settings::default();
        assert_eq!(settings.buffer, 120);
        assert_eq!(settings.fps, default.fps);
        assert!(settings.notice_corner == notice::Corner::TopRight);
        assert!(settings.hotkeys == [Hotkey { action: Action::Save, mods: 1, key: 120, seconds: 0 }]);
        assert!(serde_json::from_str::<Settings>("{}").unwrap() == default);
    }

    #[test]
    fn a_replace_keeps_the_original_beside_it_with_the_clip_s_date() {
        let dir = std::env::temp_dir().join(format!("clip-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (clip, trimmed) = (dir.join("a.mp4"), dir.join("a trimmed.mp4"));
        std::fs::write(&clip, "whole").unwrap();
        let date = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        std::fs::File::options().write(true).open(&clip).unwrap().set_modified(date).unwrap();
        std::fs::write(&trimmed, "cut").unwrap();
        take_place(&clip, &trimmed).unwrap();
        assert_eq!(std::fs::read_to_string(&clip).unwrap(), "cut");
        assert_eq!(std::fs::read_to_string(clip.with_extension("mp4.orig")).unwrap(), "whole");
        assert_eq!(std::fs::metadata(&clip).unwrap().modified().unwrap(), date);
        assert!(!trimmed.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_replace_that_can_t_land_puts_the_clip_back() {
        let dir = std::env::temp_dir().join(format!("clip-replace-back-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("a.mp4");
        std::fs::write(&clip, "whole").unwrap();
        assert!(take_place(&clip, &dir.join("missing.mp4")).is_err());
        assert_eq!(std::fs::read_to_string(&clip).unwrap(), "whole");
        assert!(!clip.with_extension("mp4.orig").exists());
        std::fs::remove_dir_all(&dir).unwrap();
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
}

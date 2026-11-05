use std::path::Path;
use std::sync::mpsc;

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder};
use tauri::Manager;
use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, LPARAM, WPARAM};
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Threading::{CreateMutexW, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_ALT, MOD_NOREPEAT, VK_F9};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMessageW, MessageBoxW, PeekMessageW, PostThreadMessageW, MB_ICONERROR, MB_ICONHAND, MB_OK, MSG, PM_NOREMOVE,
    WM_HOTKEY,
};

use crate::{Options, Session};

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
        .setup(|app| {
            let clips = app.path().video_dir()?.join("memory");
            let menu = Menu::with_items(
                app,
                &[
                    &MenuItem::with_id(app, "save", "save clip   alt+f9", true, None::<&str>)?,
                    &MenuItem::with_id(app, "folder", "open clips folder", true, None::<&str>)?,
                    &PredefinedMenuItem::separator(app)?,
                    &MenuItem::with_id(app, "quit", "quit", true, None::<&str>)?,
                ],
            )?;
            let mut tray = TrayIconBuilder::with_id("memory").menu(&menu).tooltip("memory: starting");
            if let Some(icon) = app.default_window_icon() {
                tray = tray.icon(icon.clone());
            }
            let tray = tray.build(app)?;
            let (ready, recorder) = mpsc::channel();
            let folder = clips.clone();
            std::thread::spawn(move || record(&folder, &tray, ready));
            let recorder = recorder.recv()?;
            app.on_menu_event(move |app, event| match event.id.as_ref() {
                "save" => unsafe {
                    let _ = PostThreadMessageW(recorder, WM_HOTKEY, WPARAM(0), LPARAM(0));
                },
                "folder" => {
                    let _ = std::fs::create_dir_all(&clips);
                    let _ = std::process::Command::new("explorer").arg(&clips).spawn();
                }
                "quit" => app.exit(0),
                _ => {}
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .map_err(|e| e.to_string())?
        .run(|_, _| {});
    Ok(())
}

/// Records on this thread's message loop, saving a clip on every WM_HOTKEY.
fn record(clips: &Path, tray: &TrayIcon, ready: mpsc::Sender<u32>) {
    let mut msg = MSG::default();
    // the queue has to exist before anything is posted to it
    let _ = unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE) };
    let _ = ready.send(unsafe { GetCurrentThreadId() });
    let hotkey = unsafe { RegisterHotKey(None, 1, MOD_ALT | MOD_NOREPEAT, VK_F9.0 as u32) }.is_ok();
    let session = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
        .and_then(|()| unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) })
        .map_err(|e| e.to_string())
        .and_then(|()| Options::parse(&[]))
        .and_then(|options| Session::start(&options, crate::primary(), &crate::audio::lanes(None, &[], false)));
    let status = match (&session, hotkey) {
        (Err(e), _) => format!("memory: {e}"),
        (Ok(_), true) => "memory: keeping the last 30 s, alt+f9 saves".into(),
        (Ok(_), false) => "memory: keeping the last 30 s, alt+f9 is taken by another app".into(),
    };
    let _ = tray.set_tooltip(Some(status));
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.0 > 0 {
        if msg.message != WM_HOTKEY {
            continue;
        }
        let saved = session.as_ref().map_err(Clone::clone).and_then(|session| {
            std::fs::create_dir_all(clips).map_err(|e| format!("can't make {}: {e}", clips.display()))?;
            session.save(&clips.join(stamp()))
        });
        unsafe { MessageBeep(if saved.is_ok() { MB_OK } else { MB_ICONHAND }) }.ok();
        if let Err(e) = saved {
            let _ = tray.set_tooltip(Some(format!("memory: {e}")));
        }
    }
}

fn stamp() -> String {
    let t = unsafe { GetLocalTime() };
    format!("{}-{:02}-{:02} {:02}-{:02}-{:02}.mp4", t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond)
}

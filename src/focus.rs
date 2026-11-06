use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::{w, HSTRING, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    GWL_EXSTYLE, WS_EX_TOOLWINDOW,
};

/// How long a newly focused app has to keep focus before recording moves to it, so passing
/// through windows on the way to another doesn't throw the buffer away.
pub const SETTLE: Duration = Duration::from_secs(1);

// the desktop, taskbars, start, search, alt+tab and the tray overflow take focus without being apps
const SHELL: [&str; 10] = [
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow",
    "XamlExplorerHostIslandWindow",
    "MultitaskingViewFrame",
    "ForegroundStaging",
    "TopLevelWindowForOverflowXamlIsland",
    "NotifyIconOverflowWindow",
];

/// The focused window when it belongs to an app that can be recorded: not the shell, a tool
/// window, something minimized, or memory itself.
pub fn focused() -> Option<HWND> {
    unsafe {
        let window = GetForegroundWindow();
        if window.is_invalid() || !IsWindowVisible(window).as_bool() || IsIconic(window).as_bool() {
            return None;
        }
        if GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        let mut class = [0u16; 64];
        let len = GetClassNameW(window, &mut class) as usize;
        if SHELL.contains(&String::from_utf16_lossy(&class[..len]).as_str()) {
            return None;
        }
        let pid = pid(window);
        (pid != 0 && pid != std::process::id()).then_some(window)
    }
}

pub fn pid(window: HWND) -> u32 {
    let mut pid = 0;
    unsafe { GetWindowThreadProcessId(window, Some(&mut pid)) };
    pid
}

/// A folder name for the app: its exe's description, like "Counter-Strike 2", or the exe's name.
pub fn app_name(pid: u32) -> String {
    let Some(exe) = exe(pid) else { return "app".into() };
    let name = description(&exe).unwrap_or_else(|| exe.file_stem().unwrap_or_default().to_string_lossy().into_owned());
    folder_safe(&name)
}

fn exe(pid: u32) -> Option<PathBuf> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut path = [0u16; 1024];
        let mut len = path.len() as u32;
        let named = QueryFullProcessImageNameW(process, PROCESS_NAME_WIN32, PWSTR(path.as_mut_ptr()), &mut len);
        let _ = CloseHandle(process);
        named.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&path[..len as usize])))
    }
}

fn description(exe: &Path) -> Option<String> {
    let path = HSTRING::from(exe);
    unsafe {
        let size = GetFileVersionInfoSizeW(&path, None);
        if size == 0 {
            return None;
        }
        let mut info = vec![0u8; size as usize];
        GetFileVersionInfoW(&path, None, size, info.as_mut_ptr().cast()).ok()?;
        let mut value = std::ptr::null_mut();
        let mut len = 0;
        if !VerQueryValueW(info.as_ptr().cast(), w!(r"\VarFileInfo\Translation"), &mut value, &mut len).as_bool()
            || len < 4
        {
            return None;
        }
        let (language, page) = (*(value as *const u16), *(value as *const u16).add(1));
        let key = HSTRING::from(format!(r"\StringFileInfo\{language:04x}{page:04x}\FileDescription"));
        if !VerQueryValueW(info.as_ptr().cast(), &key, &mut value, &mut len).as_bool() || len == 0 {
            return None;
        }
        let text = String::from_utf16_lossy(std::slice::from_raw_parts(value as *const u16, len as usize));
        Some(text.trim_end_matches('\0').trim().to_string()).filter(|text| !text.is_empty())
    }
}

pub fn folder_safe(name: &str) -> String {
    let name: String =
        name.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { ' ' } else { c }).collect();
    let name = name.trim().trim_end_matches(['.', ' ']);
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&name.to_ascii_uppercase().as_str())
        || (name.len() == 4
            && name.get(..3).is_some_and(|stem| ["COM", "LPT"].contains(&stem.to_ascii_uppercase().as_str()))
            && name.as_bytes()[3].is_ascii_digit());
    match name {
        "" => "app".into(),
        _ if reserved => format!("{name} app"),
        _ => name.into(),
    }
}

/// Decides when recording moves to the focused app.
pub struct Follow<W> {
    pending: Option<(W, Instant)>,
}

impl<W> Default for Follow<W> {
    fn default() -> Self {
        Self { pending: None }
    }
}

impl<W: PartialEq + Copy> Follow<W> {
    /// The window to record now, if it should change. An app recorded a moment ago gets
    /// recording back at once, any other once it has kept focus for [`SETTLE`].
    pub fn tick(&mut self, focused: Option<W>, current: Option<W>, previous: Option<W>, now: Instant) -> Option<W> {
        let Some(window) = focused.filter(|&w| Some(w) != current) else {
            self.pending = None;
            return None;
        };
        match self.pending {
            _ if Some(window) == previous => {}
            Some((pending, since)) if pending == window && now.duration_since(since) >= SETTLE => {}
            Some((pending, _)) if pending == window => return None,
            _ => {
                self.pending = Some((window, now));
                return None;
            }
        }
        self.pending = None;
        Some(window)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_app_takes_over_once_it_keeps_focus() {
        let mut follow = Follow::default();
        let start = Instant::now();
        assert_eq!(follow.tick(Some(2), Some(1), None, start), None);
        assert_eq!(follow.tick(Some(2), Some(1), None, start + SETTLE / 2), None);
        assert_eq!(follow.tick(Some(2), Some(1), None, start + SETTLE), Some(2));
    }

    #[test]
    fn passing_through_an_app_starts_its_wait_over() {
        let mut follow = Follow::default();
        let start = Instant::now();
        follow.tick(Some(2), Some(1), None, start);
        follow.tick(Some(3), Some(1), None, start + SETTLE / 2);
        assert_eq!(follow.tick(Some(2), Some(1), None, start + SETTLE), None);
        follow.tick(None, Some(1), None, start + SETTLE * 2);
        assert_eq!(follow.tick(Some(2), Some(1), None, start + SETTLE * 2), None);
    }

    #[test]
    fn the_previous_app_comes_back_at_once() {
        let mut follow = Follow::default();
        assert_eq!(follow.tick(Some(1), Some(2), Some(1), Instant::now()), Some(1));
    }

    #[test]
    fn the_recorded_app_or_no_app_changes_nothing() {
        let mut follow = Follow::default();
        let start = Instant::now();
        assert_eq!(follow.tick(Some(1), Some(1), None, start), None);
        assert_eq!(follow.tick(None, Some(1), None, start + SETTLE * 3), None);
    }

    #[test]
    fn folder_names_drop_what_windows_refuses() {
        assert_eq!(folder_safe("Counter-Strike 2"), "Counter-Strike 2");
        assert_eq!(folder_safe("a/b: c?"), "a b  c");
        assert_eq!(folder_safe("game..."), "game");
        assert_eq!(folder_safe("  "), "app");
        assert_eq!(folder_safe("nul"), "nul app");
        assert_eq!(folder_safe("COM3"), "COM3 app");
        assert_eq!(folder_safe("Comet"), "Comet");
        assert_eq!(folder_safe("a€"), "a€");
    }
}

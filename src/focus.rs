use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::{w, BOOL, HSTRING, PWSTR};
use windows::Win32::Foundation::GENERIC_WRITE;
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Gdi::{GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONULL};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_ContainerFormatPng, IWICImagingFactory, WICBitmapEncoderNoCache,
};
use windows::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Shell::SHDefExtractIconW;
use windows::Win32::UI::WindowsAndMessaging::{DestroyIcon, HICON};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, IsZoomed, GWL_EXSTYLE, WS_EX_TOOLWINDOW,
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

/// The focused window when it is worth recording: an app's window covering its whole monitor,
/// fullscreen or borderless, or one of `also`, lowercase exe names recorded in a window too.
pub fn focused(also: &[String]) -> Option<HWND> {
    let window = unsafe { GetForegroundWindow() };
    if unsafe { IsIconic(window) }.as_bool() {
        return None;
    }
    let pid = app_pid(window)?;
    (fills_monitor(window) || (!also.is_empty() && exe_name(pid).is_some_and(|name| also.contains(&name))))
        .then_some(window)
}

/// The lowercase exe names of apps with a window open, for picking which to record in a window.
pub fn windowed() -> Vec<String> {
    unsafe extern "system" fn visit(window: HWND, data: LPARAM) -> BOOL {
        let pids = unsafe { &mut *(data.0 as *mut Vec<u32>) };
        if unsafe { GetWindowTextLengthW(window) } > 0 {
            pids.extend(app_pid(window));
        }
        true.into()
    }
    let mut pids = Vec::new();
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&mut pids as *mut _ as isize)) };
    pids.sort_unstable();
    pids.dedup();
    let mut names: Vec<String> = pids.into_iter().filter_map(exe_name).collect();
    names.sort_unstable();
    names.dedup();
    names
}

/// The process behind a window that belongs to an app: not the shell, a tool window, something
/// hidden or cloaked, or clip itself.
fn app_pid(window: HWND) -> Option<u32> {
    unsafe {
        if window.is_invalid() || !IsWindowVisible(window).as_bool() {
            return None;
        }
        if GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return None;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(window, DWMWA_CLOAKED, (&mut cloaked as *mut u32).cast(), 4);
        if cloaked != 0 {
            return None;
        }
        let mut class = [0u16; 64];
        let len = GetClassNameW(window, &mut class) as usize;
        if SHELL.contains(&String::from_utf16_lossy(&class[..len]).as_str()) {
            return None;
        }
    }
    let pid = pid(window);
    (pid != 0 && pid != std::process::id()).then_some(pid)
}

// a maximized window can cover a monitor without a taskbar, and that is still a window
fn fills_monitor(window: HWND) -> bool {
    unsafe {
        let monitor = MonitorFromWindow(window, MONITOR_DEFAULTTONULL);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        let mut rect = RECT::default();
        if monitor.is_invalid()
            || IsZoomed(window).as_bool()
            || !GetMonitorInfoW(monitor, &mut info).as_bool()
            || GetWindowRect(window, &mut rect).is_err()
        {
            return false;
        }
        covers(rect, info.rcMonitor)
    }
}

fn covers(window: RECT, monitor: RECT) -> bool {
    window.left <= monitor.left
        && window.top <= monitor.top
        && window.right >= monitor.right
        && window.bottom >= monitor.bottom
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

/// Writes the icon of `pid`'s exe to `to` as a 256 px png, unless one is there already.
pub fn save_icon(pid: u32, to: &Path) -> Result<(), String> {
    if to.exists() {
        return Ok(());
    }
    let exe = exe(pid).ok_or("no exe")?;
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let part = to.with_extension("part");
    unsafe {
        let mut icon = HICON::default();
        SHDefExtractIconW(&HSTRING::from(exe.as_path()), 0, 0, Some(&mut icon), None, 256)
            .ok()
            .map_err(|e| e.to_string())?;
        if icon.is_invalid() {
            return Err(format!("{} has no icon", exe.display()));
        }
        let written = (|| -> windows::core::Result<()> {
            let factory: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let bitmap = factory.CreateBitmapFromHICON(icon)?;
            let stream = factory.CreateStream()?;
            stream.InitializeFromFilename(&HSTRING::from(part.as_path()), GENERIC_WRITE.0)?;
            let encoder = factory.CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())?;
            encoder.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame = None;
            encoder.CreateNewFrame(&mut frame, std::ptr::null_mut())?;
            let frame = frame.ok_or_else(windows::core::Error::empty)?;
            frame.Initialize(None)?;
            frame.WriteSource(&bitmap, std::ptr::null())?;
            frame.Commit()?;
            encoder.Commit()
        })();
        let _ = DestroyIcon(icon);
        written.map_err(|e| e.to_string())?;
    }
    std::fs::rename(&part, to).map_err(|e| e.to_string())
}

fn exe_name(pid: u32) -> Option<String> {
    Some(exe(pid)?.file_name()?.to_string_lossy().to_lowercase())
}

pub fn exe(pid: u32) -> Option<PathBuf> {
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
    fn only_a_window_over_the_whole_monitor_fills_it() {
        let rect = |left, top, right, bottom| RECT { left, top, right, bottom };
        let monitor = rect(1920, 0, 4480, 1440);
        assert!(covers(monitor, monitor));
        assert!(covers(rect(1912, -8, 4488, 1448), monitor));
        assert!(!covers(rect(1912, -8, 4488, 1400), monitor));
        assert!(!covers(rect(0, 0, 1920, 1080), monitor));
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

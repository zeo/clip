//! A small note in a corner of the game's screen that never takes focus and stays out of captures.

use std::cell::{Cell, RefCell};

use serde::{Deserialize, Serialize};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Default, Debug)]
#[serde(rename_all = "kebab-case")]
pub enum Corner {
    TopLeft,
    #[default]
    TopRight,
    BottomLeft,
    BottomRight,
}

const SHOWN_MS: u32 = 2500;
const BACK: COLORREF = rgb(0x1c, 0x1c, 0x1c);
const TEXT: COLORREF = rgb(0xe8, 0xe8, 0xe8);
const GOOD: COLORREF = rgb(0x8f, 0xc4, 0x6a);
const BAD: COLORREF = rgb(0xff, 0x7a, 0x6e);

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

thread_local! {
    static WINDOW: Cell<Option<HWND>> = const { Cell::new(None) };
    static SHOWING: RefCell<(Vec<u16>, bool, u32)> = const { RefCell::new((Vec::new(), false, 96)) };
}

/// Shows `text` near `game` for a moment. Called from a thread that dispatches its window messages.
pub fn show(text: &str, bad: bool, corner: Corner, game: HWND) {
    let Some(window) = WINDOW.with(|w| w.get().or_else(|| create().inspect(|&made| w.set(Some(made))))) else {
        return;
    };
    unsafe {
        let monitor = MonitorFromWindow(game, MONITOR_DEFAULTTOPRIMARY);
        let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
        if !GetMonitorInfoW(monitor, &mut info).as_bool() {
            return;
        }
        let (mut dpi, mut unused) = (96, 0);
        let _ = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi, &mut unused);
        let px = |n: i32| n * dpi as i32 / 96;
        let wide: Vec<u16> = text.encode_utf16().collect();
        let font = font(dpi);
        let dc = GetDC(Some(window));
        let old = SelectObject(dc, font.into());
        let mut measured = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &wide, &mut measured);
        SelectObject(dc, old);
        ReleaseDC(Some(window), dc);
        let _ = DeleteObject(font.into());
        SHOWING.with(|s| *s.borrow_mut() = (wide, bad, dpi));

        let (width, height) = (px(16 + 8 + 10 + 18) + measured.cx, px(40));
        let area = info.rcWork;
        let margin = px(24);
        let x = match corner {
            Corner::TopLeft | Corner::BottomLeft => area.left + margin,
            Corner::TopRight | Corner::BottomRight => area.right - margin - width,
        };
        let y = match corner {
            Corner::TopLeft | Corner::TopRight => area.top + margin,
            Corner::BottomLeft | Corner::BottomRight => area.bottom - margin - height,
        };
        let _ = SetWindowPos(window, Some(HWND_TOPMOST), x, y, width, height, SWP_NOACTIVATE | SWP_SHOWWINDOW);
        let _ = InvalidateRect(Some(window), None, true);
        SetTimer(Some(window), 1, SHOWN_MS, None);
    }
}

fn create() -> Option<HWND> {
    unsafe {
        let instance = GetModuleHandleW(None).ok()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(procedure),
            hInstance: instance.into(),
            lpszClassName: w!("clip-recorder-notice"),
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            ..Default::default()
        };
        RegisterClassW(&class);
        let window = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TOOLWINDOW,
            w!("clip-recorder-notice"),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance.into()),
            None,
        )
        .ok()?;
        // other recorders and screen shares leave it out, so it never ends up in someone's video
        let _ = SetWindowDisplayAffinity(window, WDA_EXCLUDEFROMCAPTURE);
        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            window,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&raw const round).cast(),
            size_of_val(&round) as u32,
        );
        Some(window)
    }
}

fn font(dpi: u32) -> HFONT {
    unsafe {
        CreateFontW(
            -(14 * dpi as i32 / 96),
            0,
            0,
            0,
            FW_SEMIBOLD.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            0,
            w!("Segoe UI"),
        )
    }
}

unsafe extern "system" fn procedure(window: HWND, message: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match message {
        WM_TIMER => {
            let _ = KillTimer(Some(window), 1);
            let _ = ShowWindow(window, SW_HIDE);
            LRESULT(0)
        }
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(window, &mut paint);
            let mut area = RECT::default();
            let _ = GetClientRect(window, &mut area);
            SHOWING.with(|s| {
                let (text, bad, dpi) = &*s.borrow();
                let px = |n: i32| n * *dpi as i32 / 96;
                let back = CreateSolidBrush(BACK);
                FillRect(dc, &area, back);
                let _ = DeleteObject(back.into());
                let dot = CreateSolidBrush(if *bad { BAD } else { GOOD });
                let old = SelectObject(dc, dot.into());
                let pen = SelectObject(dc, GetStockObject(NULL_PEN));
                let middle = (area.bottom - area.top) / 2;
                let _ = Ellipse(dc, px(16), middle - px(4), px(24) + 1, middle + px(4) + 1);
                SelectObject(dc, pen);
                SelectObject(dc, old);
                let _ = DeleteObject(dot.into());
                let font = font(*dpi);
                let old = SelectObject(dc, font.into());
                SetBkMode(dc, TRANSPARENT);
                SetTextColor(dc, TEXT);
                let mut line = RECT { left: px(34), ..area };
                let mut text = text.clone();
                DrawTextW(dc, &mut text, &mut line, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
                SelectObject(dc, old);
                let _ = DeleteObject(font.into());
            });
            let _ = EndPaint(window, &paint);
            LRESULT(0)
        }
        _ => DefWindowProcW(window, message, wparam, lparam),
    }
}

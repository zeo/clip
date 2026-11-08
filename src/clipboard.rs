use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

use windows::core::{w, HSTRING};
use windows::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{IDropSource, CF_HDROP, DROPEFFECT_COPY};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    BHID_DataObject, ILFree, SHCreateShellItemArrayFromIDLists, SHDoDragDrop, SHParseDisplayName, DROPFILES,
};

/// A DROPFILES header and then the paths as wide strings, each ended by a nul and the list by
/// another, the way explorer puts files on the clipboard.
fn dropped(paths: &[PathBuf]) -> Vec<u8> {
    let header = size_of::<DROPFILES>();
    let mut bytes = vec![0; header];
    bytes[..4].copy_from_slice(&(header as u32).to_le_bytes());
    // fWide is the last field
    bytes[header - 4..].copy_from_slice(&1u32.to_le_bytes());
    for path in paths {
        path.as_os_str().encode_wide().chain([0]).for_each(|unit| bytes.extend(unit.to_le_bytes()));
    }
    bytes.extend([0, 0]);
    bytes
}

/// Puts files on the clipboard to paste into explorer, discord or a chat, as a copy and never a move.
pub fn copy_files(owner: HWND, paths: &[PathBuf]) -> Result<(), String> {
    let set = |format: u32, data: &[u8]| unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, data.len()).map_err(|e| e.to_string())?;
        let target = GlobalLock(memory);
        if target.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err("couldn't lock the clipboard memory".to_string());
        }
        std::ptr::copy_nonoverlapping(data.as_ptr(), target.cast(), data.len());
        let _ = GlobalUnlock(memory);
        // the clipboard owns the memory once this succeeds
        SetClipboardData(format, Some(HANDLE(memory.0))).map(drop).map_err(|e| {
            let _ = GlobalFree(Some(memory));
            e.to_string()
        })
    };
    unsafe {
        // another app can hold the clipboard for a moment
        let mut opened = OpenClipboard(Some(owner));
        for _ in 0..10 {
            if opened.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            opened = OpenClipboard(Some(owner));
        }
        opened.map_err(|e| format!("the clipboard is busy: {e}"))?;
        let done = EmptyClipboard().map_err(|e| e.to_string()).and_then(|()| {
            set(u32::from(CF_HDROP.0), &dropped(paths))?;
            let effect = RegisterClipboardFormatW(w!("Preferred DropEffect"));
            set(effect, &DROPEFFECT_COPY.0.to_le_bytes())
        });
        let _ = CloseClipboard();
        done
    }
}

/// The data object explorer itself hands out for these files, so every drop target reads it.
fn shell_data(paths: &[PathBuf]) -> windows::core::Result<IDataObject> {
    let mut lists = Vec::with_capacity(paths.len());
    let parsed = paths.iter().try_for_each(|path| {
        let mut list: *mut ITEMIDLIST = std::ptr::null_mut();
        unsafe { SHParseDisplayName(&HSTRING::from(path.as_path()), None, &mut list, 0, None) }?;
        lists.push(list.cast_const());
        Ok(())
    });
    let data = parsed
        .and_then(|()| unsafe { SHCreateShellItemArrayFromIDLists(&lists)?.BindToHandler(None, &BHID_DataObject) });
    for list in lists {
        unsafe { ILFree(Some(list)) };
    }
    data
}

/// Drags files out to explorer, discord or a browser while the mouse button stays down, as a copy.
/// Blocks until they're dropped, on the thread that owns `owner`.
pub fn drag_files(owner: HWND, paths: &[PathBuf]) -> Result<(), String> {
    let data = shell_data(paths).map_err(|e| e.to_string())?;
    // with no drop source of its own the shell supplies one, along with the drag image
    unsafe { SHDoDragDrop(Some(owner), &data, None::<&IDropSource>, DROPEFFECT_COPY) }
        .map(drop)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{
        CoInitializeEx, COINIT_APARTMENTTHREADED, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL,
    };

    #[test]
    fn dragged_files_offer_a_file_drop() {
        let file = std::env::temp_dir().join("clip drag test.mp4");
        std::fs::write(&file, b"x").unwrap();
        let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
        let data = shell_data(std::slice::from_ref(&file)).unwrap();
        let format = FORMATETC {
            cfFormat: CF_HDROP.0,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let offered = unsafe { data.QueryGetData(&format) };
        std::fs::remove_file(&file).unwrap();
        assert!(offered.is_ok(), "{offered:?}");
        assert!(shell_data(&[PathBuf::from(
            r"Z:
o such\clip.mp4"
        )])
        .is_err());
    }

    #[test]
    fn dropped_files_are_wide_and_double_nul_ended() {
        let bytes = dropped(&[PathBuf::from(r"C:\a.mp4"), PathBuf::from(r"D:\é b.mp4")]);
        let header = size_of::<DROPFILES>();
        assert_eq!(header, 20);
        assert_eq!(&bytes[..4], &20u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &1u32.to_le_bytes());
        let units: Vec<u16> = bytes[header..].chunks(2).map(|p| u16::from_le_bytes([p[0], p[1]])).collect();
        let text = String::from_utf16(&units).unwrap();
        assert_eq!(text, "C:\\a.mp4\0D:\\é b.mp4\0\0");
    }
}

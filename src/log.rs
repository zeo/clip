//! clip.log beside the crash dump, for what went wrong out of sight: one line each, kept to about 1 MB
//! and one older file.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use windows::Win32::System::SystemInformation::GetLocalTime;

const MAX: u64 = 1_000_000;

static FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn open(file: &Path) {
    *FILE.lock().expect("log poisoned") = Some(file.to_path_buf());
}

/// Adds a line to the log, or to stderr before the log is open.
pub fn write(text: impl std::fmt::Display) {
    let file = FILE.lock().expect("log poisoned");
    let Some(file) = file.as_deref() else {
        eprintln!("{text}");
        return;
    };
    let t = unsafe { GetLocalTime() };
    let line = format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02} {text}\r\n",
        t.wYear, t.wMonth, t.wDay, t.wHour, t.wMinute, t.wSecond
    );
    if std::fs::metadata(file).is_ok_and(|meta| meta.len() + line.len() as u64 > MAX) {
        let _ = std::fs::rename(file, file.with_extension("log.old"));
    }
    // a log that can't be written has nowhere left to say so
    if let Ok(mut out) = std::fs::File::options().create(true).append(true).open(file) {
        let _ = out.write_all(line.as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_rolls_over_into_one_old_file() {
        let dir = std::env::temp_dir().join(format!("clip-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("clip.log");
        open(&file);
        let long = "x".repeat(400_000);
        for _ in 0..5 {
            write(&long);
        }
        let size = |path: &Path| std::fs::metadata(path).map_or(0, |meta| meta.len());
        assert!(size(&file) <= MAX && size(&dir.join("clip.log.old")) <= MAX);
        assert!(size(&dir.join("clip.log.old")) > 0);
        *FILE.lock().unwrap() = None;
        let _ = std::fs::remove_dir_all(&dir);
    }
}

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::UI::Shell::{
    SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW,
};

/// Clips of one app saved less than this far apart belong to one session.
const SESSION_GAP: u64 = 30 * 60 * 1000;

#[derive(Serialize)]
pub struct Clip {
    pub path: PathBuf,
    pub app: String,
    pub name: String,
    pub bytes: u64,
    /// milliseconds since the unix epoch
    pub modified: u64,
    pub seconds: f64,
    pub fps: f64,
    pub starred: bool,
    /// seconds into the clip where alt+f10 was pressed
    pub marks: Vec<f64>,
    pub session: usize,
    pub thumb: Option<PathBuf>,
    /// the app's exe icon, once memory has recorded it
    pub icon: Option<PathBuf>,
}

/// What memory remembers about clips beside the files, kept next to the settings.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Marks {
    /// paths inside the clips folder, with forward slashes
    starred: HashSet<String>,
    marked: HashMap<String, Vec<f64>>,
}

impl Marks {
    pub fn load(file: &Path) -> Self {
        std::fs::read_to_string(file).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn store(&self, file: &Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(file, json).map_err(|e| format!("can't save the library: {e}"))
    }

    pub fn starred(&self, folder: &Path, path: &Path) -> bool {
        self.starred.contains(&key(folder, path))
    }

    pub fn star(&mut self, folder: &Path, path: &Path, on: bool) {
        let key = key(folder, path);
        if on {
            self.starred.insert(key);
        } else {
            self.starred.remove(&key);
        }
    }

    pub fn marks(&self, folder: &Path, path: &Path) -> Vec<f64> {
        self.marked.get(&key(folder, path)).cloned().unwrap_or_default()
    }

    pub fn mark(&mut self, folder: &Path, path: &Path, at: Vec<f64>) {
        if !at.is_empty() {
            self.marked.insert(key(folder, path), at);
        }
    }

    pub fn moved(&mut self, folder: &Path, from: &Path, to: &Path) {
        if self.starred.remove(&key(folder, from)) {
            self.starred.insert(key(folder, to));
        }
        if let Some(at) = self.marked.remove(&key(folder, from)) {
            self.marked.insert(key(folder, to), at);
        }
    }

    pub fn forget(&mut self, folder: &Path, path: &Path) {
        self.starred.remove(&key(folder, path));
        self.marked.remove(&key(folder, path));
    }
}

fn key(folder: &Path, path: &Path) -> String {
    path.strip_prefix(folder).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

/// Every clip in `folder` and its app folders, newest first, grouped into sessions.
pub fn list(folder: &Path, marks: &Marks, thumbs: &Path) -> Vec<Clip> {
    let entries = |dir: &Path| std::fs::read_dir(dir).into_iter().flatten().flatten();
    let mut found = Vec::new();
    for entry in entries(folder) {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let app = entry.file_name().to_string_lossy().into_owned();
            found.extend(entries(&entry.path()).map(|inner| (inner, app.clone())));
        } else {
            // clips saved before memory sorted them by app were all of the desktop
            found.push((entry, "desktop".to_string()));
        }
    }
    let mut clips: Vec<Clip> = found
        .into_iter()
        .filter(|(entry, _)| entry.path().extension().is_some_and(|e| e.eq_ignore_ascii_case("mp4")))
        .filter_map(|(entry, app)| {
            let meta = entry.metadata().ok().filter(|meta| meta.is_file())?;
            let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
            let path = entry.path();
            let name = path.file_stem()?.to_string_lossy().into_owned();
            let (seconds, fps) = crate::save::probe(&path).map_or((0.0, 0.0), |probe| (probe.seconds, probe.fps));
            let (starred, at) = (marks.starred(folder, &path), marks.marks(folder, &path));
            let thumb = Some(thumb(thumbs, &path, modified, meta.len())).filter(|thumb| thumb.is_file());
            let icon = Some(icon(thumbs, &app)).filter(|icon| icon.is_file());
            let bytes = meta.len();
            Some(Clip { path, app, name, bytes, modified, seconds, fps, starred, marks: at, session: 0, thumb, icon })
        })
        .collect();
    clips.sort_by_key(|clip| std::cmp::Reverse(clip.modified));
    let sessions = sessions(&clips.iter().map(|clip| (clip.app.as_str(), clip.modified)).collect::<Vec<_>>());
    clips.iter_mut().zip(sessions).for_each(|(clip, session)| clip.session = session);
    clips
}

/// Numbers sessions for clips sorted newest first, in the order each session's newest clip comes.
fn sessions(clips: &[(&str, u64)]) -> Vec<usize> {
    let mut open: HashMap<&str, (usize, u64)> = HashMap::new();
    let mut next = 0;
    clips
        .iter()
        .map(|&(app, modified)| {
            let session = match open.get(app) {
                Some(&(session, oldest)) if oldest.saturating_sub(modified) < SESSION_GAP => session,
                _ => {
                    next += 1;
                    next - 1
                }
            };
            open.insert(app, (session, modified));
            session
        })
        .collect()
}

/// Where a clip's still lives, named after what would change if the clip did.
pub fn thumb(thumbs: &Path, path: &Path, modified: u64, bytes: u64) -> PathBuf {
    let mut hash = std::hash::DefaultHasher::new();
    (path, modified, bytes).hash(&mut hash);
    // the size is in the name so stills from a smaller long side get made again
    thumbs.join(format!("{:016x}-640.jpg", hash.finish()))
}

/// Where an app's exe icon lives, beside the stills.
pub fn icon(thumbs: &Path, app: &str) -> PathBuf {
    thumbs.with_file_name("icons").join(format!("{app}.png"))
}

/// Drops stills whose clip has gone.
pub fn prune(thumbs: &Path, clips: &[Clip]) {
    let kept: HashSet<&Path> = clips.iter().filter_map(|clip| clip.thumb.as_deref()).collect();
    for entry in std::fs::read_dir(thumbs).into_iter().flatten().flatten() {
        if !kept.contains(entry.path().as_path()) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A new name for a clip, kept in its folder.
pub fn renamed(path: &Path, name: &str) -> Result<PathBuf, String> {
    let name = name.trim();
    if name.is_empty() || crate::focus::folder_safe(name) != name {
        return Err(format!("\"{name}\" can't be a file name"));
    }
    let to = path.with_file_name(format!("{name}.mp4"));
    if to.exists() {
        return Err(format!("{name} already exists"));
    }
    Ok(to)
}

/// Moves clips to the recycle bin in one operation, so one undo brings them all back.
pub fn recycle(paths: &[PathBuf]) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    if paths.is_empty() {
        return Ok(());
    }
    // shell file operations take a list of paths, each ending in a nul and the list in another
    let from: Vec<u16> = paths.iter().flat_map(|path| path.as_os_str().encode_wide().chain([0])).chain([0]).collect();
    let mut operation = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    match unsafe { SHFileOperationW(&mut operation) } {
        0 if !operation.fAnyOperationsAborted.as_bool() => Ok(()),
        code => {
            let what = match paths {
                [path] => path.display().to_string(),
                _ => format!("{} clips", paths.len()),
            };
            Err(format!("couldn't move {what} to the recycle bin ({code:#x})"))
        }
    }
}

/// Deletes the oldest unstarred clips, never the newest, until the folder holds at most `cap` bytes.
/// Returns the paths deleted.
pub fn trim_to(clips: &[Clip], cap: u64) -> Vec<PathBuf> {
    let mut total: u64 = clips.iter().map(|clip| clip.bytes).sum();
    let mut deleted = Vec::new();
    for clip in clips.iter().skip(1).rev().filter(|clip| !clip.starred) {
        if total <= cap {
            break;
        }
        if std::fs::remove_file(&clip.path).is_ok() {
            total -= clip.bytes;
            deleted.push(clip.path.clone());
        }
    }
    deleted
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60 * 1000;

    #[test]
    fn sessions_split_on_a_long_gap_and_per_app() {
        let clips = [
            ("Fortnite", 200 * MIN),
            ("Discord", 195 * MIN),
            ("Fortnite", 190 * MIN),
            ("Discord", 170 * MIN),
            // 31 minutes before the fortnite clip above
            ("Fortnite", 159 * MIN),
        ];
        assert_eq!(sessions(&clips), [0, 1, 0, 1, 2]);
    }

    #[test]
    fn a_session_chains_through_short_gaps() {
        let clips = [("a", 100 * MIN), ("a", 75 * MIN), ("a", 50 * MIN), ("a", 25 * MIN), ("a", 0)];
        assert_eq!(sessions(&clips), [0; 5]);
    }

    #[test]
    fn marks_follow_a_rename_and_go_with_a_delete() {
        let folder = Path::new(r"C:\clips");
        let (from, to) = (folder.join(r"a\one.mp4"), folder.join(r"a\two.mp4"));
        let mut marks = Marks::default();
        marks.mark(folder, &from, vec![12.5]);
        marks.star(folder, &from, true);
        marks.moved(folder, &from, &to);
        assert!(marks.marks(folder, &from).is_empty());
        assert_eq!(marks.marks(folder, &to), [12.5]);
        marks.forget(folder, &to);
        assert!(marks.marks(folder, &to).is_empty() && !marks.starred(folder, &to));
    }

    #[test]
    fn renaming_refuses_what_windows_would() {
        let path = Path::new(r"C:\clips\Fortnite\2026-11-06 14-02-11.mp4");
        assert!(renamed(path, "clutch 1v4").is_ok_and(|to| to.ends_with(r"Fortnite\clutch 1v4.mp4")));
        for bad in ["", "  ", "a/b", r"..\up", "what?", "CON", "trailing."] {
            assert!(renamed(path, bad).is_err(), "{bad:?} was allowed");
        }
    }

    fn clip(name: &str, bytes: u64, starred: bool) -> Clip {
        Clip {
            path: std::env::temp_dir().join(format!("memory-trim-{}-{name}.mp4", std::process::id())),
            app: "a".into(),
            name: name.into(),
            bytes,
            modified: 0,
            seconds: 0.0,
            fps: 0.0,
            starred,
            marks: Vec::new(),
            session: 0,
            thumb: None,
            icon: None,
        }
    }

    #[test]
    fn trimming_spares_stars_and_the_newest() {
        let clips =
            [clip("newest", 50, false), clip("starred", 50, true), clip("mid", 50, false), clip("old", 50, false)];
        for clip in &clips {
            std::fs::write(&clip.path, b"x").unwrap();
        }
        let deleted = trim_to(&clips, 120);
        assert_eq!(deleted, [clips[3].path.clone(), clips[2].path.clone()]);
        assert!(clips[0].path.exists() && clips[1].path.exists());
        let _ = std::fs::remove_file(&clips[0].path);
        let _ = std::fs::remove_file(&clips[1].path);
    }
}

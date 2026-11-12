use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::UI::Shell::{
    SHFileOperationW, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, FO_DELETE, SHFILEOPSTRUCTW,
};

/// Clips of one app saved less than this far apart belong to one session.
const SESSION_GAP: u64 = 30 * 60 * 1000;

#[derive(Serialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Clip,
    /// a screenshot, a saved frame or a gif
    Image,
}

#[derive(Serialize)]
pub struct Clip {
    pub path: PathBuf,
    pub kind: Kind,
    /// the folder name, for clips in another app's folder that clip only reads
    pub source: Option<String>,
    pub app: String,
    pub name: String,
    pub bytes: u64,
    /// milliseconds since the unix epoch
    pub modified: u64,
    pub seconds: f64,
    pub fps: f64,
    /// hevc with hdr10 metadata, which many players and sites can't show
    pub hdr: bool,
    pub starred: bool,
    /// seconds into the clip where alt+f10 was pressed
    pub marks: Vec<f64>,
    pub session: usize,
    pub thumb: Option<PathBuf>,
    /// the app's exe icon, once clip has recorded it
    pub icon: Option<PathBuf>,
}

/// What clip remembers about clips beside the files, kept next to the settings.
#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Marks {
    /// paths inside the clips folder, with forward slashes
    starred: HashSet<String>,
    marked: HashMap<String, Vec<f64>>,
    /// whole lowercase paths of the files clip wrote, the only ones cleanup may delete
    written: HashSet<String>,
    /// the files in clip's own default folder were taken as written once
    seeded: bool,
}

impl Marks {
    /// The library in `file`, or an empty one with what went wrong when it can't be read.
    pub fn load(file: &Path) -> (Self, Option<String>) {
        match read_json(file) {
            Ok(marks) => (marks.unwrap_or_default(), None),
            // a library that can't be read isn't seeded again, so its clips stay out of cleanup
            Err(e) => (Self { seeded: true, ..Self::default() }, Some(e)),
        }
    }

    pub fn store(&self, file: &Path) -> Result<(), String> {
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())?;
        write_json(file, &json).map_err(|e| format!("can't save the library: {e}"))
    }

    pub fn wrote(&mut self, path: &Path) {
        self.written.insert(whole(path));
    }

    pub fn ours(&self, path: &Path) -> bool {
        self.written.contains(&whole(path))
    }

    /// Takes every clip and image already in `dir` as clip's, once, for clips saved before clip kept count.
    pub fn seed(&mut self, dir: &Path) {
        if !std::mem::replace(&mut self.seeded, true) {
            self.written.extend(scan(dir, "").iter().map(|(entry, _)| whole(&entry.path())));
        }
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
        if at.is_empty() {
            self.marked.remove(&key(folder, path));
        } else {
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
        if self.written.remove(&whole(from)) {
            self.written.insert(whole(to));
        }
    }

    pub fn forget(&mut self, folder: &Path, path: &Path) {
        self.starred.remove(&key(folder, path));
        self.marked.remove(&key(folder, path));
        self.written.remove(&whole(path));
    }
}

fn key(folder: &Path, path: &Path) -> String {
    path.strip_prefix(folder).unwrap_or(path).to_string_lossy().replace('\\', "/")
}

// windows paths match in any case
fn whole(path: &Path) -> String {
    path.to_string_lossy().replace('/', "\\").to_lowercase()
}

/// What a json file holds, None when there's none yet. One that won't parse is copied beside itself
/// as `.bad` before clip goes on without it, so writing a fresh one can't lose it.
pub fn read_json<T: DeserializeOwned>(file: &Path) -> Result<Option<T>, String> {
    let text = match std::fs::read_to_string(file) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        text => text.map_err(|e| format!("couldn't read {}: {e}", file.display()))?,
    };
    serde_json::from_str(&text).map(Some).map_err(|e| {
        let bad = sibling(file, ".bad");
        match std::fs::copy(file, &bad) {
            Ok(_) => format!("{} couldn't be read ({e}), a copy is at {}", file.display(), bad.display()),
            Err(copy) => format!("{} couldn't be read ({e}) or copied aside ({copy})", file.display()),
        }
    })
}

/// Writes `text` beside `file` and moves it over, so a crash or a full drive mid write leaves the
/// old file whole.
pub fn write_json(file: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let part = sibling(file, ".new");
    let written = std::fs::File::create(&part).and_then(|mut out| {
        out.write_all(text.as_bytes())?;
        out.sync_all()
    });
    written.and_then(|()| std::fs::rename(&part, file)).inspect_err(|_| drop(std::fs::remove_file(&part)))
}

fn sibling(file: &Path, tail: &str) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(tail);
    name.into()
}

/// What a file is by its extension, if the library shows it.
pub fn kind(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    match ext.as_str() {
        "mp4" => Some(Kind::Clip),
        "png" | "gif" => Some(Kind::Image),
        _ => None,
    }
}

/// The files of `dir` and its app folders, with the app each belongs to. Files straight in `dir` go
/// to `flat`.
fn scan(dir: &Path, flat: &str) -> Vec<(std::fs::DirEntry, String)> {
    let entries = |dir: &Path| std::fs::read_dir(dir).into_iter().flatten().flatten();
    let mut found = Vec::new();
    for entry in entries(dir) {
        if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            let app = entry.file_name().to_string_lossy().into_owned();
            found.extend(entries(&entry.path()).map(|inner| (inner, app.clone())));
        } else {
            found.push((entry, flat.to_string()));
        }
    }
    found.retain(|(entry, _)| kind(&entry.path()).is_some());
    found
}

fn folder_name(dir: &Path) -> String {
    dir.file_name().map_or_else(|| dir.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// Every clip and image in `folder` and its app folders, then in each of `sources` that clip only
/// reads, newest first and grouped into sessions. A file in a source that the clips folder has a copy
/// of, by name and size, shows once.
pub fn list(folder: &Path, sources: &[PathBuf], marks: &Marks, thumbs: &Path) -> Vec<Clip> {
    // clips saved before clip sorted them by app were all of the desktop
    let mut found: Vec<_> = scan(folder, "desktop").into_iter().map(|(entry, app)| (entry, app, None)).collect();
    let mut seen: HashSet<PathBuf> = found.iter().map(|(entry, ..)| entry.path()).collect();
    let mut copies: HashSet<(String, u64)> = found
        .iter()
        .filter_map(|(entry, ..)| {
            Some((entry.file_name().to_string_lossy().to_lowercase(), entry.metadata().ok()?.len()))
        })
        .collect();
    for source in sources {
        let name = folder_name(source);
        for (entry, app) in scan(source, &name) {
            let copy =
                (entry.file_name().to_string_lossy().to_lowercase(), entry.metadata().map_or(0, |meta| meta.len()));
            if seen.insert(entry.path()) && copies.insert(copy) {
                found.push((entry, app, Some(name.clone())));
            }
        }
    }
    let mut clips: Vec<Clip> = found
        .into_iter()
        .filter_map(|(entry, app, source)| {
            let meta = entry.metadata().ok().filter(|meta| meta.is_file())?;
            let modified = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64;
            let path = entry.path();
            let kind = kind(&path)?;
            let name = path.file_stem()?.to_string_lossy().into_owned();
            let probe = (kind == Kind::Clip).then(|| crate::save::probe(&path)).and_then(Result::ok);
            let (seconds, fps) = probe.as_ref().map_or((0.0, 0.0), |probe| (probe.seconds, probe.fps));
            let hdr = probe.is_some_and(|probe| probe.mastering.is_some());
            let (starred, at) = (marks.starred(folder, &path), marks.marks(folder, &path));
            let thumb = Some(thumb(thumbs, &path, modified, meta.len())).filter(|thumb| thumb.is_file());
            let icon = Some(icon(thumbs, &app)).filter(|icon| icon.is_file());
            let bytes = meta.len();
            Some(Clip {
                path,
                kind,
                source,
                app,
                name,
                bytes,
                modified,
                seconds,
                fps,
                hdr,
                starred,
                marks: at,
                session: 0,
                thumb,
                icon,
            })
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

/// Drops the art and icons of apps no clip is from any more.
pub fn prune_apps<'a>(thumbs: &Path, apps: impl IntoIterator<Item = &'a str>) {
    let kept: HashSet<String> = apps.into_iter().map(str::to_lowercase).collect();
    let gone = |name: &str| !kept.contains(&name.to_lowercase());
    for entry in std::fs::read_dir(thumbs.with_file_name("art")).into_iter().flatten().flatten() {
        if gone(&entry.file_name().to_string_lossy()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
    for entry in std::fs::read_dir(thumbs.with_file_name("icons")).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.file_stem().is_some_and(|app| gone(&app.to_string_lossy())) {
            let _ = std::fs::remove_file(path);
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

/// What clip's own unfinished writes end in, so a start clears only those and never another app's
/// `.part` download beside them.
pub const PART: &str = ".clip-part";

/// Settles every replace that was waiting out its undo when clip last stopped: the trimmed clip
/// stays and its original goes to the recycle bin, or the original comes back if the trimmed one
/// never landed. A long recording cut off by a crash is rebuilt into a playable clip up to its last
/// fragment; those are returned.
pub fn settle(folder: &Path) -> Vec<PathBuf> {
    let dirs = std::fs::read_dir(folder).into_iter().flatten().flatten().map(|entry| entry.path());
    let dirs = std::iter::once(folder.to_path_buf()).chain(dirs.filter(|dir| dir.is_dir()));
    let originals =
        dirs.flat_map(|dir| std::fs::read_dir(dir).into_iter().flatten().flatten().map(|entry| entry.path()));
    let lower = |path: &Path| path.to_string_lossy().to_ascii_lowercase();
    let part = |path: &Path| lower(path).ends_with(PART);
    let (parts, originals): (Vec<PathBuf>, Vec<PathBuf>) =
        originals.filter(|path| part(path) || lower(path).ends_with(".mp4.orig")).partition(|path| part(path));
    // any other write cut off by a crash or power loss can't be played, its index is only written at the end
    let mut recovered = Vec::new();
    for part in parts {
        let out = part.with_extension("");
        let rebuilt = lower(&part).ends_with(&format!(".mp4{PART}"))
            && !out.exists()
            && crate::save::recover(&part).is_ok()
            && std::fs::rename(&part, &out).is_ok();
        match rebuilt {
            true => recovered.push(out),
            false => {
                let _ = std::fs::remove_file(part);
            }
        }
    }
    let (kept, lost): (Vec<PathBuf>, Vec<PathBuf>) =
        originals.into_iter().partition(|path| path.with_extension("").exists());
    for path in lost {
        let _ = std::fs::rename(&path, path.with_extension(""));
    }
    let _ = recycle(&kept);
    recovered
}

/// Deletes the oldest unstarred clips clip wrote, never the newest, until they take at most `cap`
/// bytes and none is older than `before`, in ms since 1970. Images, clips clip didn't write and
/// clips in other apps' folders are never touched. Returns the paths deleted.
pub fn trim(clips: &[Clip], marks: &Marks, cap: u64, before: u64) -> Vec<PathBuf> {
    let ours: Vec<&Clip> = (clips.iter())
        .filter(|clip| clip.kind == Kind::Clip && clip.source.is_none() && marks.ours(&clip.path))
        .collect();
    let mut total: u64 = ours.iter().map(|clip| clip.bytes).sum();
    let mut deleted = Vec::new();
    for clip in ours.into_iter().skip(1).rev().filter(|clip| !clip.starred) {
        if total <= cap && clip.modified >= before {
            break;
        }
        if std::fs::remove_file(&clip.path).is_ok() {
            total -= clip.bytes;
            deleted.push(clip.path.clone());
        }
    }
    deleted
}

/// Where files made from a clip go: beside it when it's in the clips folder, or in the clips folder
/// under its app when it's in another app's folder.
pub fn home(folder: &Path, path: &Path) -> PathBuf {
    let dir = path.parent().unwrap_or(folder);
    if dir == folder || dir.parent() == Some(folder) {
        dir.to_path_buf()
    } else {
        folder.join(folder_name(dir))
    }
}

/// Copies the clips and images of another app's folder into the clips folder under the same app
/// names, leaving the originals and skipping any already there. Returns how many were copied.
pub fn import(source: &Path, folder: &Path, progress: impl Fn(usize, usize)) -> Result<usize, String> {
    let found = scan(source, &folder_name(source));
    // windows paths match in any case, which Path::starts_with doesn't
    let low = |dir: &Path| dir.to_string_lossy().to_lowercase();
    let own = format!("{}\\", low(folder).trim_end_matches(['\\', '/']));
    let mut copied = 0;
    for (i, (entry, app)) in found.iter().enumerate() {
        progress(i, found.len());
        let dir = folder.join(crate::focus::folder_safe(app));
        let to = dir.join(entry.file_name());
        // a folder holding the clips folder, like Videos, would hand clip its own files back
        if to.exists() || low(&entry.path()).starts_with(&own) || !entry.file_type().is_ok_and(|kind| kind.is_file()) {
            continue;
        }
        let mut part = to.clone().into_os_string();
        part.push(PART);
        // a copy cut off half way is left as a part, which the next start clears
        std::fs::create_dir_all(&dir)
            .and_then(|()| std::fs::copy(entry.path(), &part))
            .and_then(|_| std::fs::rename(&part, &to))
            .map_err(|e| {
                let _ = std::fs::remove_file(&part);
                format!("couldn't copy {}: {e}", entry.path().display())
            })?;
        copied += 1;
    }
    progress(found.len(), found.len());
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60 * 1000;

    #[test]
    fn a_start_clears_clip_s_own_unfinished_writes_and_brings_back_a_lost_original() {
        let dir = std::env::temp_dir().join(format!("clip-settle-{}", std::process::id()));
        let game = dir.join("Fortnite");
        std::fs::create_dir_all(&game).unwrap();
        let file = |path: PathBuf| {
            std::fs::write(&path, "x").unwrap();
            path
        };
        let (ours, deeper) = (file(dir.join(format!("a.mp4{PART}"))), file(game.join(format!("b.png{PART}"))));
        let download = file(dir.join("movie.mp4.part"));
        let original = file(game.join("c.mp4.orig"));
        assert!(settle(&dir).is_empty(), "a cut off plain write isn't rebuilt");
        assert!(!ours.exists() && !deeper.exists());
        assert!(download.exists(), "another app's download stays");
        assert!(!original.exists() && game.join("c.mp4").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
            path: std::env::temp_dir().join(format!("clip-trim-{}-{name}.mp4", std::process::id())),
            kind: Kind::Clip,
            source: None,
            app: "a".into(),
            name: name.into(),
            bytes,
            modified: 0,
            seconds: 0.0,
            fps: 0.0,
            hdr: false,
            starred,
            marks: Vec::new(),
            session: 0,
            thumb: None,
            icon: None,
        }
    }

    fn ours(clips: &[Clip]) -> Marks {
        let mut marks = Marks::default();
        clips.iter().for_each(|clip| marks.wrote(&clip.path));
        marks
    }

    #[test]
    fn trimming_spares_stars_and_the_newest() {
        let clips =
            [clip("newest", 50, false), clip("starred", 50, true), clip("mid", 50, false), clip("old", 50, false)];
        for clip in &clips {
            std::fs::write(&clip.path, b"x").unwrap();
        }
        let deleted = trim(&clips, &ours(&clips), 120, 0);
        assert_eq!(deleted, [clips[3].path.clone(), clips[2].path.clone()]);
        assert!(clips[0].path.exists() && clips[1].path.exists());
        let kept = trim(&clips[..2], &ours(&clips), u64::MAX, u64::MAX);
        assert!(kept.is_empty(), "the newest and the starred stay at any age");
        let _ = std::fs::remove_file(&clips[0].path);
        let _ = std::fs::remove_file(&clips[1].path);
    }

    #[test]
    fn trimming_by_age_keeps_the_newer_clips() {
        let mut clips = [clip("age-new", 1, false), clip("age-mid", 1, false), clip("age-old", 1, false)];
        for (clip, at) in clips.iter_mut().zip([300, 200, 100]) {
            clip.modified = at;
            std::fs::write(&clip.path, b"x").unwrap();
        }
        assert_eq!(trim(&clips, &ours(&clips), u64::MAX, 150), [clips[2].path.clone()]);
        clips.iter().for_each(|clip| drop(std::fs::remove_file(&clip.path)));
    }

    #[test]
    fn trimming_leaves_images_and_other_apps_folders_alone() {
        let mut clips = [clip("kept-new", 1, false), clip("kept-shot", 1, false), clip("kept-medal", 1, false)];
        clips[1].kind = Kind::Image;
        clips[2].source = Some("Medal".into());
        for clip in &clips {
            std::fs::write(&clip.path, b"x").unwrap();
        }
        assert!(trim(&clips, &ours(&clips), 0, u64::MAX).is_empty());
        assert!(clips.iter().all(|clip| clip.path.exists()));
        clips.iter().for_each(|clip| drop(std::fs::remove_file(&clip.path)));
    }

    #[test]
    fn trimming_deletes_only_what_clip_wrote() {
        let clips = [clip("trim-ours-new", 1, false), clip("trim-obs", 1, false), clip("trim-ours-old", 1, false)];
        for clip in &clips {
            std::fs::write(&clip.path, b"x").unwrap();
        }
        let mut marks = Marks::default();
        marks.wrote(&clips[0].path);
        // a path cased differently is the same file to windows
        marks.wrote(&PathBuf::from(clips[2].path.to_string_lossy().to_uppercase()));
        assert_eq!(trim(&clips, &marks, 0, u64::MAX), [clips[2].path.clone()]);
        assert!(clips[1].path.exists(), "a file clip didn't write stays at any size or age");
        clips.iter().for_each(|clip| drop(std::fs::remove_file(&clip.path)));
    }

    #[test]
    fn written_files_follow_a_rename_and_seed_once() {
        let root = std::env::temp_dir().join(format!("clip-seed-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Fortnite")).unwrap();
        std::fs::write(root.join(r"Fortnite\old.mp4"), b"1").unwrap();
        let mut marks = Marks::default();
        marks.seed(&root);
        assert!(marks.ours(&root.join(r"Fortnite\old.mp4")));
        std::fs::write(root.join(r"Fortnite\later.mp4"), b"1").unwrap();
        marks.seed(&root);
        assert!(!marks.ours(&root.join(r"Fortnite\later.mp4")), "seeding happens once");
        let (old, new) = (root.join(r"Fortnite\old.mp4"), root.join(r"Fortnite\renamed.mp4"));
        marks.moved(&root, &old, &new);
        assert!(marks.ours(&new) && !marks.ours(&old));
        marks.forget(&root, &new);
        assert!(!marks.ours(&new));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_broken_file_is_kept_aside_and_a_write_replaces_whole() {
        let root = std::env::temp_dir().join(format!("clip-json-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("library.json");
        assert_eq!(read_json::<Marks>(&file).map(|m| m.is_none()), Ok(true), "no file yet is no error");
        std::fs::write(&file, b"{\"starred\": [").unwrap();
        let (marks, problem) = Marks::load(&file);
        assert!(problem.is_some_and(|e| e.contains("library.json.bad")));
        assert!(marks.seeded, "a library that couldn't be read isn't seeded over");
        assert_eq!(std::fs::read(root.join("library.json.bad")).unwrap(), b"{\"starred\": [");
        marks.store(&file).unwrap();
        assert!(Marks::load(&file).1.is_none() && !root.join("library.json.new").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn art_and_icons_go_with_the_last_clip_of_their_app() {
        let root = std::env::temp_dir().join(format!("clip-prune-{}", std::process::id()));
        for dir in ["art/Fortnite", "art/Old Game", "icons"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for file in ["art/Fortnite/cover.jpg", "icons/Fortnite.png", "icons/Old Game.png"] {
            std::fs::write(root.join(file), b"1").unwrap();
        }
        prune_apps(&root.join("thumbs"), ["fortnite"]);
        assert!(root.join("art/Fortnite/cover.jpg").exists() && root.join("icons/Fortnite.png").exists());
        assert!(!root.join("art/Old Game").exists() && !root.join("icons/Old Game.png").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn other_apps_clips_list_once_and_copy_in() {
        let root = std::env::temp_dir().join(format!("clip-sources-{}", std::process::id()));
        let (folder, medal) = (root.join("clips"), root.join("Medal"));
        let write = |path: PathBuf, bytes: &[u8]| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        write(folder.join(r"Fortnite\ours.mp4"), b"1");
        write(medal.join(r"Fortnite\theirs.mp4"), b"22");
        write(medal.join(r"Fortnite\ours.mp4"), b"1");
        write(medal.join("loose.png"), b"333");
        write(medal.join("notes.txt"), b"4");
        let thumbs = root.join("thumbs");
        let listed = |sources: &[PathBuf]| {
            let mut names: Vec<_> = list(&folder, sources, &Marks::default(), &thumbs)
                .into_iter()
                .map(|clip| (clip.name, clip.app, clip.source, clip.kind))
                .collect();
            names.sort_by(|a, b| a.0.cmp(&b.0));
            names
        };
        let medal_name = Some("Medal".to_string());
        assert_eq!(
            listed(std::slice::from_ref(&medal)),
            [
                ("loose".into(), "Medal".into(), medal_name.clone(), Kind::Image),
                ("ours".into(), "Fortnite".into(), None, Kind::Clip),
                ("theirs".into(), "Fortnite".into(), medal_name, Kind::Clip),
            ]
        );
        assert_eq!(listed(&[folder.clone(), medal.clone(), medal.clone()]).len(), 3, "overlapping folders list once");
        assert_eq!(home(&folder, &medal.join(r"Fortnite\theirs.mp4")), folder.join("Fortnite"));
        assert_eq!(home(&folder, &medal.join("loose.png")), folder.join("Medal"));
        assert_eq!(home(&folder, &folder.join(r"Fortnite\ours.mp4")), folder.join("Fortnite"));

        assert_eq!(import(&medal, &folder, |_, _| {}), Ok(2));
        assert_eq!(std::fs::read(folder.join(r"Fortnite\ours.mp4")).unwrap(), b"1", "a clip already there stays");
        assert!(folder.join(r"Fortnite\theirs.mp4").is_file() && folder.join(r"Medal\loose.png").is_file());
        assert!(medal.join(r"Fortnite\theirs.mp4").is_file(), "the originals stay");
        assert!(
            listed(std::slice::from_ref(&medal)).iter().all(|(.., source, _)| source.is_none()),
            "copies show once"
        );
        assert_eq!(import(&medal, &folder, |_, _| {}), Ok(0));
        write(folder.join("desktop.mp4"), b"55555");
        assert_eq!(
            import(&root, &folder, |_, _| {}),
            Ok(0),
            "a folder around the clips folder copies none of its files"
        );
        assert!(!folder.join("clips").exists());
        let shouted = PathBuf::from(folder.to_string_lossy().to_uppercase());
        assert_eq!(import(&root, &shouted, |_, _| {}), Ok(0), "the clips folder is found in any case");
        let _ = std::fs::remove_dir_all(&root);
    }
}

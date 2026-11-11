//! Update checks and installs through the shared rot installer, the way split and capscr do it.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

pub const EVERY: Duration = Duration::from_secs(6 * 3600);
const APP: &str = "clip";
const INSTALLER: &str = "rot-installer.exe";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;

/// What the last check found, for the window.
#[derive(Serialize, Clone, Default, PartialEq)]
pub struct Update {
    /// a newer version is out
    pub available: bool,
    pub version: String,
    pub notes: String,
    /// why there's no answer, empty when the check went through
    pub error: String,
}

/// The installer that put clip on this pc and where it did, kept for the install.
#[derive(Clone)]
pub struct Owner {
    exe: PathBuf,
    root: PathBuf,
}

fn installers() -> Vec<PathBuf> {
    let here = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf));
    let mut candidates: Vec<PathBuf> =
        here.into_iter().flat_map(|dir| [dir.join(INSTALLER), dir.join("installer").join(INSTALLER)]).collect();
    if let Some(root) = std::env::var_os("PROGRAMFILES") {
        candidates.push(Path::new(&root).join("rot").join("installer").join(INSTALLER));
    }
    if let Some(root) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(Path::new(&root).join("Programs").join("rot-installer").join(INSTALLER));
    }
    candidates.into_iter().filter(|exe| exe.is_file()).collect()
}

fn run(exe: &Path, args: &[&str]) -> Result<Output, String> {
    Command::new(exe).args(args).creation_flags(CREATE_NO_WINDOW).output().map_err(|e| e.to_string())
}

/// Asks the installer that owns clip for its newest release.
pub fn check() -> Result<(Update, Owner), String> {
    let candidates = installers();
    if candidates.is_empty() {
        return Err("the rot installer isn't on this pc".into());
    }
    // an older shared installer may not know clip while a per-user one owns the install
    let owner = candidates.into_iter().find_map(|exe| {
        let status = run(&exe, &["status", APP, "--json"]).ok().filter(|out| out.status.success())?;
        let receipts: Value = serde_json::from_slice(&status.stdout).ok()?;
        let root = PathBuf::from(receipts.get(0)?.get("install_root")?.as_str()?);
        Some(Owner { exe, root })
    });
    let owner = owner.ok_or("this copy of clip wasn't installed by the rot installer")?;
    let release = run(&owner.exe, &["check", APP, "--json"])?;
    if !release.status.success() {
        let stderr = String::from_utf8_lossy(&release.stderr);
        return Err(stderr.trim().lines().last().unwrap_or("the update check failed").to_string());
    }
    let release: Value = serde_json::from_slice(&release.stdout).map_err(|e| e.to_string())?;
    let version = release.get("version").and_then(Value::as_str).ok_or("the installer named no version")?;
    let update = Update {
        available: newer(version, env!("CARGO_PKG_VERSION")),
        version: version.to_string(),
        notes: release.get("notes").and_then(Value::as_str).unwrap_or_default().to_string(),
        error: String::new(),
    };
    Ok((update, owner))
}

fn newer(candidate: &str, current: &str) -> bool {
    let key = |version: &str| {
        let (core, pre) = version.split('+').next().unwrap_or_default().split_once('-').unwrap_or((version, ""));
        let core: Vec<u64> = core.split('.').map(|part| part.parse().unwrap_or(0)).collect();
        // a release sorts above its own pre-releases
        (core, pre.is_empty(), pre.to_string())
    };
    key(candidate) > key(current)
}

/// Hands off to the installer in a console of its own, which waits for clip to quit, updates and
/// opens it again. A failure stays on screen until a key is pressed.
pub fn install(owner: &Owner) -> Result<(), String> {
    let relaunch = owner.root.join("clip.exe");
    let script = format!(
        "title Updating clip & \"{}\" update {APP} --wait-pid {} && start \"\" \"{}\" || pause",
        owner.exe.display(),
        std::process::id(),
        relaunch.display()
    );
    Command::new("cmd.exe")
        .raw_arg(format!("/s /c \"{script}\""))
        .creation_flags(CREATE_NEW_CONSOLE)
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number_and_a_release_beats_its_previews() {
        assert!(newer("0.1.1", "0.1.0"));
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("0.2.0", "0.2.0-rc.1"));
        assert!(!newer("0.2.0-rc.1", "0.2.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0+build", "0.1.0"));
    }
}

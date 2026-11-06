//! Game art for the library: Steam's own for Steam games, SteamGridDB's for the rest when there's a key.

use std::collections::HashSet;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData,
    WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts, INTERNET_DEFAULT_HTTPS_PORT,
    WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};

// a game nothing was found for isn't asked about again for this long
const RETRY: Duration = Duration::from_secs(7 * 24 * 3600);
const MOST: usize = 20_000_000;
const SGDB: &str = "https://www.steamgriddb.com/api/v2";
// each kind: its steam cdn files, sharpest first, and its steamgriddb listing
const KINDS: [(&str, &[&str], &str); 3] = [
    ("cover", &["library_600x900_2x.jpg", "library_600x900.jpg"], "grids/game/{}?dimensions=600x900"),
    ("hero", &["library_hero_2x.jpg", "library_hero.jpg"], "heroes/game/{}"),
    ("logo", &["logo.png"], "logos/game/{}"),
];
static FETCHING: Mutex<Option<HashSet<String>>> = Mutex::new(None);

#[derive(Serialize, Clone, Default)]
pub struct Art {
    pub app: String,
    pub cover: Option<PathBuf>,
    pub hero: Option<PathBuf>,
    pub logo: Option<PathBuf>,
}

/// Where an app's art lives, beside the stills.
pub fn dir(thumbs: &Path, app: &str) -> PathBuf {
    thumbs.with_file_name("art").join(app)
}

/// Notes the Steam appid of the game at `exe`, or that it has none, so art never has to guess it by name.
pub fn remember(exe: &Path, dir: &Path) -> Result<(), String> {
    let file = dir.join("appid");
    if file.exists() {
        return Ok(());
    }
    let id = steam().and_then(|steam| appid(exe, &libraries(&steam)));
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    std::fs::write(file, id.map(|id| id.to_string()).unwrap_or_default()).map_err(|e| e.to_string())
}

/// The art already on disk.
pub fn cached(dir: &Path, app: &str) -> Art {
    let find = |kind: &str| {
        std::fs::read_dir(dir).ok()?.flatten().map(|entry| entry.path()).find(|path| {
            path.file_stem().is_some_and(|stem| stem == kind)
                && path.extension().is_some_and(|ext| ["jpg", "png", "webp"].iter().any(|known| ext.eq_ignore_ascii_case(known)))
        })
    };
    Art { app: app.to_string(), cover: find("cover"), hero: find("hero"), logo: find("logo") }
}

/// Whether some art is missing and worth asking for: never asked, asked long ago, or asked before there was a key.
pub fn due(dir: &Path, art: &Art, key: &str) -> bool {
    if art.cover.is_some() && art.hero.is_some() && art.logo.is_some() {
        return false;
    }
    let tried = dir.join("tried");
    let fresh = std::fs::metadata(&tried)
        .and_then(|meta| meta.modified())
        .is_ok_and(|at| at.elapsed().is_ok_and(|age| age < RETRY));
    !fresh || (!key.is_empty() && std::fs::read_to_string(&tried).is_ok_and(|with| with.is_empty()))
}

/// Downloads whatever art is missing, unless the same app is being fetched already.
pub fn fetch(dir: &Path, app: &str, key: &str) -> Option<Art> {
    if !FETCHING.lock().expect("fetching poisoned").get_or_insert_with(HashSet::new).insert(app.to_string()) {
        return None;
    }
    let art = download(dir, app, key);
    FETCHING.lock().expect("fetching poisoned").get_or_insert_with(HashSet::new).remove(app);
    Some(art)
}

fn download(dir: &Path, app: &str, key: &str) -> Art {
    let have = cached(dir, app);
    if std::fs::create_dir_all(dir).is_err() {
        return have;
    }
    let id: Option<u32> = std::fs::read_to_string(dir.join("appid")).ok().and_then(|id| id.trim().parse().ok());
    let mut game = None;
    for ((kind, files, listing), got) in KINDS.iter().zip([&have.cover, &have.hero, &have.logo]) {
        if got.is_some() {
            continue;
        }
        let steam = || {
            let id = id?;
            files.iter().find_map(|file| {
                let url = format!("https://shared.steamstatic.com/store_item_assets/steam/apps/{id}/{file}");
                Some((get(&url, None).ok()?, extension(file)))
            })
        };
        let grid = || {
            if key.is_empty() {
                return None;
            }
            let found = *game.get_or_insert_with(|| grid_game(app, id, key));
            let listed = json(&format!("{SGDB}/{}", listing.replace("{}", &found?.to_string())), key)?;
            let url = listed["data"][0]["url"].as_str()?.to_string();
            Some((get(&url, None).ok()?, extension(&url)))
        };
        if let Some((bytes, ext)) = steam().or_else(grid) {
            let part = dir.join(format!("{kind}.part"));
            if std::fs::write(&part, bytes).is_ok() {
                let _ = std::fs::rename(&part, dir.join(format!("{kind}.{ext}")));
            }
        }
    }
    let _ = std::fs::write(dir.join("tried"), if key.is_empty() { "" } else { "key" });
    cached(dir, app)
}

fn extension(url: &str) -> &'static str {
    match url.rsplit('.').next().map(str::to_ascii_lowercase).as_deref() {
        Some("png") => "png",
        Some("webp") => "webp",
        _ => "jpg",
    }
}

/// SteamGridDB's id for the game: by appid for a Steam game, else the best match for its name.
fn grid_game(app: &str, appid: Option<u32>, key: &str) -> Option<u64> {
    let found = match appid {
        Some(id) => json(&format!("{SGDB}/games/steam/{id}"), key)?["data"]["id"].as_u64(),
        None => None,
    };
    found.or_else(|| json(&format!("{SGDB}/search/autocomplete/{}", escape(app)), key)?["data"][0]["id"].as_u64())
}

fn json(url: &str, key: &str) -> Option<serde_json::Value> {
    serde_json::from_slice(&get(url, Some(key)).ok()?).ok()
}

fn escape(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

struct Handle(*mut c_void);

impl Handle {
    fn new(raw: *mut c_void) -> Result<Self, String> {
        if raw.is_null() {
            Err(windows::core::Error::from_thread().to_string())
        } else {
            Ok(Self(raw))
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = unsafe { WinHttpCloseHandle(self.0) };
    }
}

/// The body of an https GET, with a bearer key when given. Redirects are followed by winhttp.
fn get(url: &str, key: Option<&str>) -> Result<Vec<u8>, String> {
    let rest = url.strip_prefix("https://").ok_or("only https")?;
    let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let headers = key.map(|key| HSTRING::from(format!("Authorization: Bearer {key}")));
    unsafe {
        let session = Handle::new(WinHttpOpen(w!("memory"), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, PCWSTR::null(), PCWSTR::null(), 0))?;
        WinHttpSetTimeouts(session.0, 10_000, 10_000, 10_000, 20_000).map_err(|e| e.to_string())?;
        let connection = Handle::new(WinHttpConnect(session.0, &HSTRING::from(host), INTERNET_DEFAULT_HTTPS_PORT, 0))?;
        let request = Handle::new(WinHttpOpenRequest(
            connection.0,
            w!("GET"),
            &HSTRING::from(if path.is_empty() { "/" } else { path }),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            WINHTTP_FLAG_SECURE,
        ))?;
        WinHttpSendRequest(request.0, headers.as_deref(), None, 0, 0, 0).map_err(|e| e.to_string())?;
        WinHttpReceiveResponse(request.0, std::ptr::null_mut()).map_err(|e| e.to_string())?;
        let mut status = 0u32;
        let mut len = 4u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some((&mut status as *mut u32).cast()),
            &mut len,
            std::ptr::null_mut(),
        )
        .map_err(|e| e.to_string())?;
        if status != 200 {
            return Err(format!("{url} answered {status}"));
        }
        let mut body = Vec::new();
        let mut chunk = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            WinHttpReadData(request.0, chunk.as_mut_ptr().cast(), chunk.len() as u32, &mut read).map_err(|e| e.to_string())?;
            if read == 0 {
                return Ok(body);
            }
            body.extend_from_slice(&chunk[..read as usize]);
            if body.len() > MOST {
                return Err(format!("{url} is too big"));
            }
        }
    }
}

fn steam() -> Option<PathBuf> {
    let mut path = [0u16; 1024];
    let mut size = (path.len() * 2) as u32;
    let read = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Valve\\Steam"),
            w!("SteamPath"),
            RRF_RT_REG_SZ,
            None,
            Some(path.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    read.is_ok().then(|| PathBuf::from(String::from_utf16_lossy(&path[..(size as usize / 2).saturating_sub(1)])))
}

/// Steam's own folder and every library listed in its libraryfolders.vdf.
fn libraries(steam: &Path) -> Vec<PathBuf> {
    let mut found = vec![steam.to_path_buf()];
    if let Ok(vdf) = std::fs::read_to_string(steam.join("steamapps").join("libraryfolders.vdf")) {
        found.extend(pairs(&vdf).filter(|(key, _)| key == "path").map(|(_, path)| PathBuf::from(path)));
    }
    found
}

/// The `"key" "value"` lines of a vdf or acf file, unescaped.
fn pairs(text: &str) -> impl Iterator<Item = (String, String)> + '_ {
    text.lines().filter_map(|line| {
        let mut quoted = line.split('"').skip(1).step_by(2);
        Some((quoted.next()?.to_string(), quoted.next()?.replace("\\\\", "\\")))
    })
}

/// The app whose install folder holds `exe`, from the appmanifests in each library.
fn appid(exe: &Path, libraries: &[PathBuf]) -> Option<u32> {
    let plain = |path: &Path| path.to_string_lossy().replace('/', "\\").to_lowercase();
    let exe = plain(exe);
    for library in libraries {
        let apps = library.join("steamapps");
        for entry in std::fs::read_dir(&apps).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().to_lowercase();
            if !name.starts_with("appmanifest_") || !name.ends_with(".acf") {
                continue;
            }
            let Ok(acf) = std::fs::read_to_string(entry.path()) else { continue };
            let (mut id, mut installdir) = (None, None);
            for (key, value) in pairs(&acf) {
                match key.to_ascii_lowercase().as_str() {
                    "appid" => id = id.or(value.parse().ok()),
                    "installdir" => installdir = installdir.or(Some(value)),
                    _ => {}
                }
            }
            if let (Some(id), Some(installdir)) = (id, installdir) {
                if exe.starts_with(&format!("{}\\", plain(&apps.join("common").join(installdir)))) {
                    return Some(id);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_steam_exe_finds_its_appid_through_the_library_manifests() {
        let root = std::env::temp_dir().join(format!("memory-art-{}", std::process::id()));
        let steam = root.join("Steam");
        let library = root.join("Games");
        std::fs::create_dir_all(steam.join("steamapps")).unwrap();
        std::fs::create_dir_all(library.join("steamapps")).unwrap();
        let listed = library.to_string_lossy().replace('\\', "\\\\");
        std::fs::write(
            steam.join("steamapps").join("libraryfolders.vdf"),
            format!("\"libraryfolders\"\n{{\n\t\"1\"\n\t{{\n\t\t\"path\"\t\t\"{listed}\"\n\t}}\n}}\n"),
        )
        .unwrap();
        std::fs::write(
            library.join("steamapps").join("appmanifest_730.acf"),
            "\"AppState\"\n{\n\t\"appid\"\t\t\"730\"\n\t\"name\"\t\t\"Counter-Strike 2\"\n\t\"installdir\"\t\t\"Counter-Strike Global Offensive\"\n}\n",
        )
        .unwrap();
        let found = libraries(&steam);
        let game = library.join("steamapps/common/Counter-Strike Global Offensive/game/bin/win64/cs2.exe");
        let elsewhere = library.join("steamapps/common/Counter-Strike Global Offensive 2/cs2.exe");
        let (hit, miss) = (appid(&game, &found), appid(&elsewhere, &found));
        std::fs::remove_dir_all(&root).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(hit, Some(730));
        assert_eq!(miss, None);
        assert_eq!(escape("Counter-Strike 2"), "Counter-Strike%202");
    }
}

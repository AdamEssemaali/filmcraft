//! Settings ▸ Versions on the desktop: the releases of storytold/filmcraft on GitHub, this system's
//! download of each (checked against the release's `SHA256SUMS.txt`) unpacked into
//! `<data dir>/Versions/<version>/` with the system's `tar` (`hdiutil` + `ditto` for the macOS
//! disk image), and the chosen one started once this session has quit ([`take_launch`]).
//!
//! A version started from here gets `FILMCRAFT_HOME_EXE` / `FILMCRAFT_HOME_VERSION` naming the
//! installed app it came from, so it can switch back to it without downloading it again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use filmcraft_ui_egui::{VersionDownload, VersionHost, VersionInfo, VersionsState};
use serde_json::Value;

const RELEASES: &str = "https://api.github.com/repos/storytold/filmcraft/releases?per_page=50";
const DOWNLOADS: &str = "https://github.com/storytold/filmcraft/releases/download/";
/// No FilmCraft download comes near this; a bigger size in the release list is refused.
const MAX_DOWNLOAD: u64 = 4 << 30;
const HOME_EXE: &str = "FILMCRAFT_HOME_EXE";
const HOME_VERSION: &str = "FILMCRAFT_HOME_VERSION";

/// A release as listed on GitHub.
#[derive(Clone, Debug, Default, PartialEq)]
struct Release {
    version: String,
    date: String,
    prerelease: bool,
    /// This system's download: (url, size), and the checksum list's url.
    asset: Option<(String, u64)>,
    sums: Option<String>,
}

#[derive(Default)]
struct Shared {
    releases: Vec<Release>,
    checked: bool,
    loading: bool,
    downloading: Option<VersionDownload>,
    error: String,
}

/// What to start after quitting: the executable and the project to open.
pub type Launch = Arc<Mutex<Option<(PathBuf, Option<String>)>>>;

pub struct Host {
    dir: PathBuf,
    shared: Arc<Mutex<Shared>>,
    launch: Launch,
}

impl Host {
    pub fn new(data_dir: &Path, launch: Launch) -> Self {
        Self { dir: data_dir.join("Versions"), shared: Arc::default(), launch }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The executable of a downloaded `version`, or of the installed app this one was started from.
    fn executable(&self, version: &str) -> Option<PathBuf> {
        if valid_version(version)
            && let Some(exe) = find_executable(&self.dir.join(version), 4)
        {
            return Some(exe);
        }
        let home = std::env::var_os(HOME_EXE).map(PathBuf::from)?;
        (std::env::var(HOME_VERSION).ok()? == version && home.is_file()).then_some(home)
    }
}

impl VersionHost for Host {
    fn state(&self) -> VersionsState {
        let s = self.lock();
        let home = std::env::var(HOME_VERSION).ok();
        let releases = s
            .releases
            .iter()
            .map(|r| VersionInfo {
                version: r.version.clone(),
                date: r.date.clone(),
                prerelease: r.prerelease,
                installed: self.dir.join(&r.version).is_dir() || home.as_deref() == Some(r.version.as_str()),
                available: r.asset.is_some() && r.sums.is_some(),
            })
            .collect();
        VersionsState {
            current: env!("CARGO_PKG_VERSION").into(),
            releases,
            checked: s.checked,
            loading: s.loading,
            downloading: s.downloading.clone(),
            error: s.error.clone(),
            folder: self.dir.display().to_string(),
        }
    }

    fn refresh(&mut self) {
        {
            let mut s = self.lock();
            if s.loading {
                return;
            }
            s.loading = true;
            s.error.clear();
        }
        let shared = self.shared.clone();
        background(shared.clone(), move || {
            let json = filmcraft_speech::models::fetch_text(RELEASES).map_err(|e| e.to_string())?;
            let releases = parse_releases(&json, asset_suffix())?;
            let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
            s.releases = releases;
            Ok(())
        });
    }

    fn download(&mut self, version: &str) {
        let release = {
            let mut s = self.lock();
            if s.downloading.is_some() {
                return;
            }
            let Some(r) = s.releases.iter().find(|r| r.version == version).cloned() else {
                s.error = format!("unknown version `{version}`");
                return;
            };
            let (Some((url, size)), Some(sums)) = (r.asset.clone(), r.sums.clone()) else {
                s.error = format!("FilmCraft {version} has no download for this system");
                return;
            };
            s.error.clear();
            s.downloading = Some(VersionDownload { version: version.into(), done: 0, total: size });
            (r.version, url, size, sums)
        };
        let (dir, shared) = (self.dir.clone(), self.shared.clone());
        background(shared.clone(), move || {
            let (version, url, size, sums_url) = release;
            let name = url.rsplit('/').next().unwrap_or_default().to_string();
            let sums = filmcraft_speech::models::fetch_text(&sums_url).map_err(|e| e.to_string())?;
            let sha = checksum(&sums, &name).ok_or_else(|| format!("{name} is not in the release's SHA256SUMS.txt"))?;
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let archive = dir.join(&name);
            let progress = shared.clone();
            filmcraft_speech::models::download_verified(&url, &archive, &sha, size, &mut |done| {
                if let Some(d) = progress.lock().unwrap_or_else(PoisonError::into_inner).downloading.as_mut() {
                    d.done = done;
                }
                true
            })
            .map_err(|e| e.to_string())?;
            let unpacked = install(&archive, &dir, &version);
            let _ = std::fs::remove_file(&archive);
            unpacked
        });
    }

    fn switch_to(&mut self, version: &str, project: Option<&str>) -> Result<(), String> {
        if version == env!("CARGO_PKG_VERSION") {
            return Err(format!("FilmCraft {version} is already running"));
        }
        let exe = self.executable(version).ok_or_else(|| format!("FilmCraft {version} is not downloaded"))?;
        *self.launch.lock().unwrap_or_else(PoisonError::into_inner) = Some((exe, project.map(str::to_string)));
        Ok(())
    }

    fn cancel_switch(&mut self) {
        *self.launch.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

/// Run `job` on a thread (panics become its error), then clear the busy flags and keep the error.
fn background(shared: Arc<Mutex<Shared>>, job: impl FnOnce() -> Result<(), String> + Send + 'static) {
    let spawned = std::thread::Builder::new().name("versions".into()).spawn({
        let shared = shared.clone();
        move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).unwrap_or_else(|_| Err("the version download stopped unexpectedly".into()));
            let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
            if s.loading {
                s.checked = true;
            }
            s.loading = false;
            s.downloading = None;
            if let Err(e) = r {
                log::warn!("versions: {e}");
                s.error = e;
            }
        }
    });
    if let Err(e) = spawned {
        let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
        (s.loading, s.downloading, s.error) = (false, None, e.to_string());
    }
}

/// Start the version chosen in Settings ▸ Versions, if any, after the window has closed.
pub fn take_launch(launch: &Launch) -> Result<(), String> {
    use std::ffi::OsString;
    use std::process::{Command, Stdio};
    let Some((exe, project)) = launch.lock().unwrap_or_else(PoisonError::into_inner).take() else { return Ok(()) };
    let mut env: Vec<(&str, OsString)> = Vec::new();
    if std::env::var_os(HOME_EXE).is_none()
        && let Ok(me) = std::env::current_exe()
    {
        env = vec![(HOME_EXE, me.into_os_string()), (HOME_VERSION, env!("CARGO_PKG_VERSION").into())];
    }
    let fail = |e: String| format!("FilmCraft could not start {}: {e}", exe.display());
    let bundle = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).filter(|_| cfg!(target_os = "macos"));
    if let Some(app) = bundle {
        // through LaunchServices, as the Finder would; -n: a new instance even while another
        // FilmCraft (same bundle id) runs
        let mut cmd = Command::new("open");
        cmd.arg("-n").arg("-a").arg(app);
        for (k, v) in env {
            let mut kv = OsString::from(format!("{k}="));
            kv.push(v);
            cmd.arg("--env").arg(kv);
        }
        cmd.arg("--args").args(project);
        return run(&mut cmd).map_err(fail);
    }
    let mut cmd = Command::new(&exe);
    cmd.args(project).envs(env).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    // its own process group, so closing the terminal this one was started from doesn't take it down
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().map(|_| ()).map_err(|e| fail(e.to_string()))
}

/// The end of this system's download name: `filmcraft-<version>-<suffix>`.
fn asset_suffix() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", _) => "macos-universal.dmg",
        ("windows", "x86_64") => "windows-x64-portable.zip",
        ("windows", "aarch64") => "windows-arm64-portable.zip",
        ("windows", "x86") => "windows-x86-portable.zip",
        ("linux", "x86_64") => "linux-x86_64.tar.gz",
        ("linux", "aarch64") => "linux-aarch64.tar.gz",
        ("linux", "riscv64") => "linux-riscv64.tar.gz",
        ("freebsd", "x86_64") => "freebsd-x86_64.tar.gz",
        _ => return None,
    })
}

/// A version used as a folder name: `1.2.3` or `1.2.3-rc.1`, nothing that could leave the folder.
fn valid_version(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 64
        && v.starts_with(|c: char| c.is_ascii_digit())
        && v.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'))
}

/// GitHub's release list (newest first); drafts and releases without a usable version are left out.
fn parse_releases(json: &str, suffix: Option<&str>) -> Result<Vec<Release>, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("unexpected answer from GitHub: {e}"))?;
    let list = v.as_array().ok_or("unexpected answer from GitHub: not a release list")?;
    let mut out = Vec::new();
    for r in list.iter().take(100) {
        let tag = r.get("tag_name").and_then(Value::as_str).unwrap_or_default();
        let version = tag.strip_prefix('v').unwrap_or(tag);
        if !valid_version(version) || r.get("draft").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let assets = r.get("assets").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
        let url_of = |name: &str| {
            assets.iter().find(|a| a.get("name").and_then(Value::as_str) == Some(name)).and_then(|a| {
                let url = a.get("browser_download_url").and_then(Value::as_str)?;
                url.starts_with(DOWNLOADS).then(|| (url.to_string(), a.get("size").and_then(Value::as_u64).unwrap_or(0)))
            })
        };
        let asset = suffix.and_then(|s| url_of(&format!("filmcraft-{version}-{s}"))).filter(|(_, size)| (1..=MAX_DOWNLOAD).contains(size));
        let date = r.get("published_at").and_then(Value::as_str).and_then(|d| d.get(..10)).unwrap_or_default();
        out.push(Release {
            version: version.into(),
            date: date.into(),
            prerelease: r.get("prerelease").and_then(Value::as_bool).unwrap_or(false),
            asset,
            sums: url_of("SHA256SUMS.txt").map(|(u, _)| u),
        });
    }
    Ok(out)
}

/// `name`'s SHA-256 in a `sha256sum` listing (`<hex>  <name>`, or `<hex> *<name>`).
fn checksum(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let (hex, file) = l.trim().split_once(char::is_whitespace)?;
        let ok = hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()) && file.trim_start().trim_start_matches('*') == name;
        ok.then(|| hex.to_ascii_lowercase())
    })
}

/// Unpack `archive` into `dir/<version>`, through a scratch folder so a half-unpacked version
/// never looks downloaded.
fn install(archive: &Path, dir: &Path, version: &str) -> Result<(), String> {
    let scratch = dir.join(format!("{version}.unpacking"));
    let _ = std::fs::remove_dir_all(&scratch);
    std::fs::create_dir_all(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
    let r = unpack(archive, &scratch).and_then(|()| {
        find_executable(&scratch, 4).ok_or("the download holds no FilmCraft app")?;
        let dest = dir.join(version);
        let _ = std::fs::remove_dir_all(&dest);
        std::fs::rename(&scratch, &dest).map_err(|e| format!("{}: {e}", dest.display()))
    });
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&scratch);
    }
    r
}

fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    if archive.extension().is_some_and(|e| e == "dmg") {
        let mount = into.join(".mount");
        run(std::process::Command::new("hdiutil").args(["attach", "-nobrowse", "-readonly", "-noautoopen", "-mountpoint"]).arg(&mount).arg(archive))?;
        let copied = run(std::process::Command::new("ditto").arg(mount.join("FilmCraft.app")).arg(into.join("FilmCraft.app")));
        let _ = run(std::process::Command::new("hdiutil").args(["detach", "-force"]).arg(&mount));
        let _ = std::fs::remove_dir(&mount);
        return copied;
    }
    // Windows' own bsdtar (System32), which reads zip files, rather than another tar on the PATH.
    let tar =
        std::env::var_os("SystemRoot").filter(|_| cfg!(windows)).map_or_else(|| PathBuf::from("tar"), |r| PathBuf::from(r).join("System32").join("tar.exe"));
    run(std::process::Command::new(tar).arg("-xf").arg(archive).arg("-C").arg(into))
}

fn run(cmd: &mut std::process::Command) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let name = cmd.get_program().to_string_lossy().into_owned();
    let out = cmd.output().map_err(|e| format!("{name}: {e}"))?;
    if out.status.success() { Ok(()) } else { Err(format!("{name} failed: {}", String::from_utf8_lossy(&out.stderr).trim())) }
}

/// The FilmCraft app in an unpacked download, at most `depth` folders down (symlinks not followed).
fn find_executable(dir: &Path, depth: u32) -> Option<PathBuf> {
    let name = if cfg!(target_os = "macos") {
        "FilmCraft"
    } else if cfg!(windows) {
        "filmcraft.exe"
    } else {
        "filmcraft"
    };
    let entries: Vec<_> = std::fs::read_dir(dir).ok()?.flatten().filter_map(|e| Some((e.path(), e.file_type().ok()?))).collect();
    if let Some((p, _)) = entries.iter().find(|(p, t)| t.is_file() && p.file_name().is_some_and(|n| n == name)) {
        return Some(p.clone());
    }
    let next = depth.checked_sub(1)?;
    entries.iter().filter(|(_, t)| t.is_dir()).find_map(|(p, _)| find_executable(p, next))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12ab12";

    #[test]
    fn releases_keep_this_systems_download_and_drop_hostile_entries() {
        let json = r#"[
            {"tag_name": "v0.6.0", "published_at": "2026-10-11T00:28:54Z", "prerelease": false, "assets": [
                {"name": "filmcraft-0.6.0-linux-x86_64.tar.gz", "size": 1000, "browser_download_url": "https://github.com/storytold/filmcraft/releases/download/v0.6.0/filmcraft-0.6.0-linux-x86_64.tar.gz"},
                {"name": "SHA256SUMS.txt", "size": 10, "browser_download_url": "https://github.com/storytold/filmcraft/releases/download/v0.6.0/SHA256SUMS.txt"}]},
            {"tag_name": "v0.5.0-rc.1", "prerelease": true, "published_at": "x", "assets": [
                {"name": "filmcraft-0.5.0-rc.1-linux-x86_64.tar.gz", "size": 1000, "browser_download_url": "https://evil.example/f.tar.gz"}]},
            {"tag_name": "v../../etc", "assets": []},
            {"tag_name": "v0.4.0", "draft": true},
            {"tag_name": 7},
            "nonsense"
        ]"#;
        let r = parse_releases(json, Some("linux-x86_64.tar.gz")).unwrap();
        assert_eq!(r.iter().map(|r| r.version.as_str()).collect::<Vec<_>>(), ["0.6.0", "0.5.0-rc.1"]);
        assert_eq!(r[0].date, "2026-10-11");
        assert_eq!(r[0].asset.as_ref().map(|a| a.1), Some(1000));
        assert!(r[0].sums.is_some());
        assert!(r[1].prerelease && r[1].asset.is_none(), "downloads only from the project's releases");
        assert!(parse_releases(json, None).unwrap()[0].asset.is_none());
        for bad in ["", "{}", "[", "null"] {
            assert!(parse_releases(bad, None).is_err(), "{bad}");
        }
    }

    #[test]
    fn checksums_are_found_by_exact_name() {
        let sums = format!("{SHA}  filmcraft-0.6.0-macos-universal.dmg\nzz  short\n{}  *SHA256SUMS.txt\n", SHA.to_uppercase());
        assert_eq!(checksum(&sums, "filmcraft-0.6.0-macos-universal.dmg").as_deref(), Some(SHA));
        assert_eq!(checksum(&sums, "SHA256SUMS.txt").as_deref(), Some(SHA));
        assert_eq!(checksum(&sums, "macos-universal.dmg"), None);
        assert_eq!(checksum(&sums, "short"), None);
    }

    #[test]
    fn versions_are_safe_folder_names() {
        for ok in ["0.6.0", "1.2.3-rc.1", "1.0.0+build.5"] {
            assert!(valid_version(ok), "{ok}");
        }
        for bad in ["", "..", "../0.6.0", "0.6.0/..", "v0.6.0", "0.6.0\\x", ".0"] {
            assert!(!valid_version(bad), "{bad}");
        }
    }

    #[test]
    fn unpacks_a_release_and_finds_its_app() {
        let tmp = std::env::temp_dir().join(format!("filmcraft-versions-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let exe_name = if cfg!(target_os = "macos") {
            "FilmCraft.app/Contents/MacOS/FilmCraft"
        } else if cfg!(windows) {
            "filmcraft-0.5.0/filmcraft.exe"
        } else {
            "filmcraft-0.5.0/bin/filmcraft"
        };
        let src = tmp.join("src");
        std::fs::create_dir_all(src.join(exe_name).parent().unwrap()).unwrap();
        std::fs::write(src.join(exe_name), b"app").unwrap();
        // a folder with the app's name must not be taken for it
        std::fs::create_dir_all(src.join("share/doc/filmcraft")).unwrap();
        let archive = tmp.join("release.tar.gz");
        assert!(std::process::Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(&src).arg(".").status().unwrap().success());
        install(&archive, &tmp, "0.5.0").unwrap();
        assert_eq!(find_executable(&tmp.join("0.5.0"), 4), Some(tmp.join("0.5.0").join(exe_name)));
        assert!(!tmp.join("0.5.0.unpacking").exists());
        // an archive without the app installs nothing
        std::fs::write(tmp.join("empty.txt"), b"").unwrap();
        let empty = tmp.join("empty.tar.gz");
        assert!(std::process::Command::new("tar").arg("-czf").arg(&empty).arg("-C").arg(&tmp).arg("empty.txt").status().unwrap().success());
        assert!(install(&empty, &tmp, "0.4.0").is_err());
        assert!(!tmp.join("0.4.0").exists() && !tmp.join("0.4.0.unpacking").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

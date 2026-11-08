//! Privilege-free filesystem locations.
//!
//! udm never needs administrator rights. Everything it writes lives either
//!
//! * next to the executable (**portable mode**), or
//! * under `%LOCALAPPDATA%\udm` on Windows 11 (a directory the logged-in
//!   user always owns), `$XDG_DATA_HOME/udm` on Linux,
//!   `~/Library/Application Support/udm` on macOS.
//!
//! There are no registry writes, no services, no scheduled tasks and no
//! machine-wide directories.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Marker file: if it sits next to the executable, udm runs in portable mode.
pub const PORTABLE_MARKER: &str = "udm.portable";

/// Pre-rename marker (quget 0.1.0). Still honoured so portable folders created
/// before the UDM rename keep their mode - and their data - after upgrading.
pub const LEGACY_PORTABLE_MARKER: &str = "quget.portable";
/// Pre-rename environment names, accepted as fallbacks.
pub const LEGACY_PORTABLE_ENV: &str = "QUGET_PORTABLE";
pub const LEGACY_DATA_DIR_ENV: &str = "QUGET_DATA_DIR";
pub const LEGACY_DOWNLOAD_DIR_ENV: &str = "QUGET_DOWNLOAD_DIR";
/// Pre-rename database file name; migrated in place on first start.
pub const LEGACY_DB_FILENAME: &str = "quget.redb";

/// Environment override: `UDM_PORTABLE=1` forces portable mode, `=0` forces
/// the per-user data directory.
pub const PORTABLE_ENV: &str = "UDM_PORTABLE";

/// Environment override for the data directory (`UDM_DATA_DIR`).
pub const DATA_DIR_ENV: &str = "UDM_DATA_DIR";

/// Resolved filesystem locations for this installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// Directory holding every file udm owns.
    pub root: PathBuf,
    /// The redb database (all tasks, segments, settings, Alt-Svc cache).
    pub db_file: PathBuf,
    /// Plain-text log file (local only, never uploaded).
    pub log_file: PathBuf,
    /// Scratch directory for imports/exports.
    pub temp_dir: PathBuf,
    /// Where new downloads go unless the user picks another folder.
    pub default_download_dir: PathBuf,
    /// `true` when running from a user-writable folder with the portable marker.
    pub portable: bool,
    /// Directory of the running executable (if it could be determined).
    pub exe_dir: Option<PathBuf>,
}

impl Paths {
    /// Discover locations without creating anything.
    pub fn discover() -> Result<Paths> {
        let exe_dir = current_exe_dir();
        let portable = detect_portable(exe_dir.as_deref());

        let root = if let Some(dir) = env_or_legacy(DATA_DIR_ENV, LEGACY_DATA_DIR_ENV) {
            if dir.trim().is_empty() {
                return Err(Error::Other(format!("{DATA_DIR_ENV} is set but empty")));
            }
            PathBuf::from(dir)
        } else if portable {
            match exe_dir.as_ref() {
                Some(dir) => dir.join("data"),
                None => per_user_data_root()?,
            }
        } else {
            per_user_data_root()?
        };

        let default_download_dir =
            env_or_legacy("UDM_DOWNLOAD_DIR", LEGACY_DOWNLOAD_DIR_ENV)
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(system_download_dir);

        Ok(Paths {
            db_file: migrate_legacy_db(&root),
            log_file: root.join("logs").join("udm.log"),
            temp_dir: root.join("tmp"),
            default_download_dir,
            portable,
            exe_dir,
            root,
        })
    }

    /// Create `root`, `logs` and `tmp`. Fails with a readable error if the
    /// location is not writable (e.g. someone put udm in `C:\Program Files`
    /// without the portable marker and without admin rights).
    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [&self.root, &self.temp_dir, self.log_file.parent().unwrap_or(&self.root)] {
            fs::create_dir_all(dir).map_err(|e| Error::IoPath {
                path: dunce::simplified(dir).to_path_buf(),
                source: e,
            })?;
        }
        Ok(())
    }

    /// Human-readable summary for `udm-cli doctor` and the About view.
    pub fn describe(&self) -> String {
        format!(
            "mode:        {}\n\
             data dir:    {}\n\
             database:    {}\n\
             log file:    {}\n\
             downloads:   {}\n\
             exe dir:     {}",
            if self.portable { "portable" } else { "per-user" },
            display(&self.root),
            display(&self.db_file),
            display(&self.log_file),
            display(&self.default_download_dir),
            self.exe_dir
                .as_ref()
                .map(|p| display(p))
                .unwrap_or_else(|| "<unknown>".to_string()),
        )
    }
}

/// `\\?\C:\foo` -> `C:\foo` for display purposes.
pub fn display(path: &Path) -> String {
    dunce::simplified(path).display().to_string()
}

fn current_exe_dir() -> Option<PathBuf> {
    let exe = env::current_exe().ok()?;
    exe.parent().map(|p| dunce::simplified(p).to_path_buf())
}

/// `UDM_*` first, then the pre-rename `QUGET_*` name.
fn env_or_legacy(new: &str, legacy: &str) -> Option<String> {
    env::var(new).ok().or_else(|| env::var(legacy).ok())
}

fn detect_portable(exe_dir: Option<&Path>) -> bool {
    if let Some(v) = env_or_legacy(PORTABLE_ENV, LEGACY_PORTABLE_ENV) {
        let v = v.trim().to_ascii_lowercase();
        return v == "1" || v == "true" || v == "yes";
    }
    if let Some(dir) = exe_dir {
        if dir.join(PORTABLE_MARKER).exists() || dir.join(LEGACY_PORTABLE_MARKER).exists() {
            return true;
        }
        // A read-only location (Program Files, Windows, a mounted image) can
        // never be a portable install.
        if !dir_is_writable(dir) {
            return false;
        }
        // No LOCALAPPDATA (rare, e.g. a service account) -> fall back to portable.
        if per_user_data_root().is_err() {
            return true;
        }
    }
    false
}

/// Cheap writability probe: create and remove a temp file.
pub fn dir_is_writable(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    let probe = dir.join(format!(".udm-write-test-{}", std::process::id()));
    match fs::write(&probe, b"") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

fn per_user_data_root() -> Result<PathBuf> {
    let base = if cfg!(target_os = "windows") {
        env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| {
                env::var_os("USERPROFILE").map(|p| PathBuf::from(p).join("AppData").join("Local"))
            })
            .ok_or_else(|| {
                Error::Other(
                    "cannot resolve a per-user data directory: neither %LOCALAPPDATA% nor \
                     %USERPROFILE% is set. Create a `udm.portable` marker next to udm.exe \
                     to run in portable mode, or set UDM_DATA_DIR."
                        .to_string(),
                )
            })?
    } else if cfg!(target_os = "macos") {
        let home = env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
            Error::Other("$HOME is not set; cannot resolve a per-user data directory".to_string())
        })?;
        home.join("Library").join("Application Support")
    } else {
        if let Ok(xdg) = env::var("XDG_DATA_HOME") {
            if !xdg.trim().is_empty() {
                PathBuf::from(xdg)
            } else {
                return Err(Error::Other("$XDG_DATA_HOME is empty".to_string()));
            }
        } else {
            let home = env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
                Error::Other("$HOME is not set; cannot resolve a per-user data directory".to_string())
            })?;
            home.join(".local").join("share")
        }
    };
    let dir = base.join("udm");
    // Pre-rename installs wrote to .../quget; keep using that directory when
    // the new one does not exist yet instead of stranding the user's history.
    if !dir.exists() {
        let legacy = base.join("quget");
        if legacy.is_dir() {
            return Ok(legacy);
        }
    }
    Ok(dir)
}

/// Rename a pre-rename `quget.redb` to `udm.redb` once, in place. If the
/// rename is impossible (read-only folder) the legacy path is used as-is.
fn migrate_legacy_db(root: &Path) -> PathBuf {
    let db = root.join("udm.redb");
    let legacy = root.join(LEGACY_DB_FILENAME);
    if !db.exists() && legacy.exists() {
        if fs::rename(&legacy, &db).is_ok() {
            return db;
        }
        return legacy;
    }
    db
}

/// The user's Downloads folder, resolved without any shell/COM calls so that no
/// privileges or extra dependencies are involved.
pub fn system_download_dir() -> PathBuf {
    if let Some(dir) = env_or_legacy("UDM_DOWNLOAD_DIR", LEGACY_DOWNLOAD_DIR_ENV) {
        return PathBuf::from(dir);
    }
    if cfg!(target_os = "windows") {
        if let Some(profile) = env::var_os("USERPROFILE").map(PathBuf::from) {
            let downloads = profile.join("Downloads");
            if downloads.is_dir() {
                return downloads;
            }
            return profile;
        }
    } else {
        // XDG user-dirs.dirs, if present.
        if let Some(home) = env::var_os("HOME").map(PathBuf::from) {
            if let Some(dir) = xdg_user_dir(&home, "DOWNLOAD") {
                return dir;
            }
            let downloads = home.join("Downloads");
            if downloads.is_dir() {
                return downloads;
            }
            return home;
        }
    }
    env::temp_dir()
}

#[cfg(not(target_os = "windows"))]
fn xdg_user_dir(home: &Path, key: &str) -> Option<PathBuf> {
    let dirs = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".config"));
    let content = fs::read_to_string(dirs.join("user-dirs.dirs")).ok()?;
    for line in content.lines() {
        let line = line.trim();
        let want = format!("XDG_{key}_DIR=");
        if let Some(value) = line.strip_prefix(&want) {
            let value = value.trim_matches('"');
            if let Some(rest) = value.strip_prefix("$HOME/") {
                return Some(home.join(rest));
            }
            if value.starts_with('/') {
                return Some(PathBuf::from(value));
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
fn xdg_user_dir(_home: &Path, _key: &str) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_yields_a_writable_layout() {
        let tmp = tempfile::tempdir().unwrap();
        env::set_var(DATA_DIR_ENV, tmp.path().join("data"));
        env::set_var(PORTABLE_ENV, "0");
        let p = Paths::discover().unwrap();
        p.ensure_dirs().unwrap();
        assert!(p.db_file.parent().unwrap().is_dir());
        assert!(p.temp_dir.is_dir());
        assert!(!p.portable);
        env::remove_var(DATA_DIR_ENV);
        env::remove_var(PORTABLE_ENV);
    }

    #[test]
    fn portable_marker_is_honoured() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(PORTABLE_MARKER), b"").unwrap();
        env::remove_var(DATA_DIR_ENV);
        env::set_var(PORTABLE_ENV, "1");
        let p = Paths::discover().unwrap();
        assert!(p.portable);
        env::remove_var(PORTABLE_ENV);
    }
}

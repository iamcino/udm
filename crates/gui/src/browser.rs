//! Browser integration (envelope rev.5, items 1-3): the Settings rows'
//! live state, the register/unregister spawn, and the portable
//! relocation repair that runs on every GUI start.
//!
//! The registry key paths and host names below are the same strings the
//! host binary uses in `crates/host/src/reg.rs` (the GUI crate cannot
//! depend on the host binary crate); keep the two in sync. The journal
//! and manifest helpers live in `usagi::handoff`, which owns the
//! `<data>/handoff` file layout.

use std::path::PathBuf;

use usagi::handoff::{last_connection_ms, repoint_manifest};
use usagi::Paths;

/// Browsers this build can register. Edge is a follow-on (item 6).
pub const BROWSERS: [&str; 2] = ["chrome", "firefox"];
/// Firefox registers this fixed extension ID; the row has no ID field.
pub const FIREFOX_EXTENSION_ID: &str = "udm-handoff@cino.pw";

/// The Native Messaging host name for one browser.
pub fn host_name(browser: &str) -> Option<&'static str> {
    match browser {
        "chrome" => Some("pw.cino.udm.handoff.chrome"),
        "firefox" => Some("pw.cino.udm.handoff.firefox"),
        _ => None,
    }
}

/// The HKCU host-key path for one browser.
pub fn registry_key(browser: &str) -> Option<String> {
    match browser {
        "chrome" => Some(
            r"Software\Google\Chrome\NativeMessagingHosts\pw.cino.udm.handoff.chrome".to_string(),
        ),
        "firefox" => {
            Some(r"Software\Mozilla\NativeMessagingHosts\pw.cino.udm.handoff.firefox".to_string())
        }
        _ => None,
    }
}

/// The host manifest path for one browser under the current data root.
pub fn manifest_path(paths: &Paths, browser: &str) -> PathBuf {
    paths
        .root
        .join("handoff")
        .join(format!("host-manifest-{browser}.json"))
}

/// The extension-ID rule, mirrored from the host (`reg.rs`): empty, or
/// containing a path separator, a space or a quote, is illegal. The GUI
/// refuses to register with such an ID; the host refuses it again at
/// `--register`. Never invent or hard-code a Chrome ID.
pub fn valid_extension_id(id: &str) -> bool {
    !id.is_empty() && !id.contains(['/', '\\', ' ', '"'])
}

/// Status-light tier for one browser (item 2). Tier 1 = no HKCU host
/// key; tier 2 = key present, no journal row; tier 3 = newest journal
/// row for this browser. `broken` is a fault shown on a registered
/// row (item 3), not a fourth tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    NotRegistered,
    Registered,
    Connected { last_ms: i64 },
}

#[derive(Clone, Copy, Debug)]
pub struct RowState {
    pub status: Status,
    pub broken: bool,
}

/// Read one row's live state from the registry and the journal.
/// Never touches the coordinator and never probes a host process.
/// host.log is not a source (it records refusals only).
pub fn row_state(paths: &Paths, browser: &str, broken: bool) -> RowState {
    if !key_exists(browser) {
        return RowState {
            status: Status::NotRegistered,
            broken: false,
        };
    }
    let status = match last_connection_ms(paths, browser) {
        Some(last_ms) => Status::Connected { last_ms },
        None => Status::Registered,
    };
    RowState { status, broken }
}

/// The extension ID of one browser's registered caller in
/// callers.json, if any (`"<browser>:<id>"`). Used to prefill the
/// Chrome ID field when the stored setting is empty.
pub fn registered_caller_ext_id(paths: &Paths, browser: &str) -> Option<String> {
    let prefix = format!("{browser}:");
    usagi::handoff::read_callers(paths)
        .into_iter()
        .find(|c| c.starts_with(&prefix))
        .map(|c| c[prefix.len()..].to_string())
}

/// Portable relocation repair (item 3): on every GUI start, for each
/// browser whose HKCU key exists, repoint the key value at the current
/// host-manifest path under the current data root and set the manifest
/// `path` field to the sibling udm-host.exe of the running app.
/// Returns the browsers that could not be repaired (exe or manifest
/// cannot be placed): their keys are left alone and their rows are
/// marked registration-broken; re-toggle is the remedy. No timer, no
/// second repair loop; not gated on the autostart setting.
pub fn repair_on_start(paths: &Paths) -> Vec<String> {
    let mut broken = Vec::new();
    for browser in BROWSERS {
        if !key_exists(browser) {
            continue;
        }
        let Some(exe) = host_exe_path() else {
            broken.push(browser.to_string());
            continue;
        };
        if !exe.exists() {
            broken.push(browser.to_string());
            continue;
        }
        let Ok(manifest) = repoint_manifest(paths, browser, &exe) else {
            broken.push(browser.to_string());
            continue;
        };
        if set_key_value(browser, &manifest.to_string_lossy()).is_err() {
            broken.push(browser.to_string());
        }
    }
    broken
}

/// The sibling udm-host.exe of the running app, if the executable
/// directory could be determined.
pub fn host_exe_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.with_file_name("udm-host.exe"))
}

/// Spawn the sibling udm-host.exe to register one browser and wait
/// for it. Never registers inside the GUI process (it would publish
/// udm.exe as the host).
pub fn spawn_register(browser: &str, ext_id: &str) -> Result<(), String> {
    let exe = host_exe_path().ok_or_else(|| "cannot locate udm-host.exe".to_string())?;
    let out = std::process::Command::new(&exe)
        .arg("--register")
        .arg(browser)
        .arg(ext_id)
        .output()
        .map_err(|e| format!("cannot run udm-host: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if detail.is_empty() {
            Err(format!("udm-host --register failed (exit {})", out.status))
        } else {
            Err(detail)
        }
    }
}

/// Spawn the sibling udm-host.exe to unregister one browser.
pub fn spawn_unregister(browser: &str) -> Result<(), String> {
    let exe = host_exe_path().ok_or_else(|| "cannot locate udm-host.exe".to_string())?;
    let out = std::process::Command::new(&exe)
        .arg("--unregister")
        .arg(browser)
        .output()
        .map_err(|e| format!("cannot run udm-host: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let detail = String::from_utf8_lossy(&out.stderr).trim().to_string();
        if detail.is_empty() {
            Err(format!(
                "udm-host --unregister failed (exit {})",
                out.status
            ))
        } else {
            Err(detail)
        }
    }
}

#[cfg(windows)]
mod platform {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegOpenKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER,
        KEY_READ, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    pub fn key_exists(browser: &str) -> bool {
        let Some(key_path) = super::registry_key(browser) else {
            return false;
        };
        let path = wide(&key_path);
        let mut key: HKEY = 0;
        let rc = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, KEY_READ, &mut key) };
        if rc == ERROR_SUCCESS {
            unsafe { RegCloseKey(key) };
            true
        } else {
            false
        }
    }

    pub fn set_key_value(browser: &str, value: &str) -> Result<(), String> {
        let Some(key_path) = super::registry_key(browser) else {
            return Err(format!("unsupported browser '{browser}'"));
        };
        let path = wide(&key_path);
        let data = wide(value);
        unsafe {
            let mut key: HKEY = 0;
            let rc = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                path.as_ptr(),
                0,
                std::ptr::null_mut(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut key,
                std::ptr::null_mut(),
            );
            if rc != ERROR_SUCCESS {
                return Err(format!("cannot open the host key (code {rc})"));
            }
            let rc = RegSetValueExW(
                key,
                std::ptr::null_mut(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            );
            RegCloseKey(key);
            if rc != ERROR_SUCCESS {
                return Err(format!("cannot write the host key value (code {rc})"));
            }
            Ok(())
        }
    }
}

#[cfg(not(windows))]
mod platform {
    pub fn key_exists(_browser: &str) -> bool {
        false
    }

    pub fn set_key_value(_browser: &str, _value: &str) -> Result<(), String> {
        Err("registration exists only on Windows".to_string())
    }
}

pub use platform::{key_exists, set_key_value};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_id_rule_mirrors_the_host() {
        assert!(valid_extension_id("abcdefghijklmnopabcdefghijklmnop"));
        assert!(valid_extension_id("udm-handoff@cino.pw"));
        assert!(!valid_extension_id(""));
        assert!(!valid_extension_id("has space"));
        assert!(!valid_extension_id("has/slash"));
        assert!(!valid_extension_id(r"has\backslash"));
        assert!(!valid_extension_id("has\"quote"));
    }

    #[test]
    fn host_names_and_keys_match_the_host_crate() {
        assert_eq!(host_name("chrome"), Some("pw.cino.udm.handoff.chrome"));
        assert_eq!(host_name("firefox"), Some("pw.cino.udm.handoff.firefox"));
        assert_eq!(host_name("edge"), None);
        assert_eq!(
            registry_key("chrome").as_deref(),
            Some(r"Software\Google\Chrome\NativeMessagingHosts\pw.cino.udm.handoff.chrome")
        );
        assert_eq!(
            registry_key("firefox").as_deref(),
            Some(r"Software\Mozilla\NativeMessagingHosts\pw.cino.udm.handoff.firefox")
        );
    }

    #[test]
    fn sibling_host_exe_is_beside_the_running_app() {
        let exe = host_exe_path().expect("current exe must be known in tests");
        assert_eq!(
            exe.file_name().and_then(|n| n.to_str()),
            Some("udm-host.exe")
        );
    }
}

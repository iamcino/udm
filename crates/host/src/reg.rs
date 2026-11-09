//! Registration for `udm-host` (G5 slice 2): the disclosed, user-invoked
//! opt-in that AGENTS.md §7 requires. Nothing here runs at app start and
//! nothing registers itself.
//!
//! For one browser, registration writes:
//!
//! * the manifest JSON into `<data>/handoff/host-manifest-<browser>.json`;
//! * the HKCU Native Messaging host key whose default value is that
//!   manifest path (Chrome: `Software\Google\Chrome\NativeMessagingHosts`,
//!   Firefox: `Software\Mozilla\NativeMessagingHosts`);
//! * the caller sidecar `udm-host.caller` next to the executable (this is
//!   how the running host knows which browser launched it);
//! * the caller into `<data>/handoff/callers.json`, the coordinator's
//!   allow-list source.
//!
//! Unregistration removes the registry key and the caller, and removes
//! the sidecar when it names that browser.

use serde_json::{json, Value};
use usagi::Paths;

pub struct BrowserSpec {
    pub browser: &'static str,
    pub host_name: &'static str,
    pub registry_key: String,
    pub caller: String,
}

/// Describe one supported browser's registration, or `None` for an
/// unsupported browser name.
pub fn describe(browser: &str, ext_id: &str) -> Option<BrowserSpec> {
    match browser {
        "chrome" => Some(BrowserSpec {
            browser: "chrome",
            host_name: "pw.cino.udm.handoff.chrome",
            registry_key:
                r"Software\Google\Chrome\NativeMessagingHosts\pw.cino.udm.handoff.chrome"
                    .to_string(),
            caller: format!("chrome:{ext_id}"),
        }),
        "firefox" => Some(BrowserSpec {
            browser: "firefox",
            host_name: "pw.cino.udm.handoff.firefox",
            registry_key:
                r"Software\Mozilla\NativeMessagingHosts\pw.cino.udm.handoff.firefox"
                    .to_string(),
            caller: format!("firefox:{ext_id}"),
        }),
        _ => None,
    }
}

/// The Native Messaging manifest for one browser. Chrome pins the single
/// allowed extension via `allowed_origins`; Firefox via
/// `allowed_extensions`. Exactly one extension can ever launch the host.
pub fn manifest_json(spec: &BrowserSpec, ext_id: &str, exe_path: &str) -> Value {
    let mut manifest = json!({
        "name": spec.host_name,
        "description": "UDM browser handoff host",
        "path": exe_path,
        "type": "stdio",
    });
    if spec.browser == "chrome" {
        manifest["allowed_origins"] = json!([format!("chrome-extension://{ext_id}/")]);
    } else {
        manifest["allowed_extensions"] = json!([ext_id]);
    }
    manifest
}

fn callers_path(paths: &Paths) -> std::path::PathBuf {
    paths.root.join("handoff").join("callers.json")
}

fn read_callers(paths: &Paths) -> Vec<String> {
    usagi::handoff::read_callers(paths)
}

fn write_callers(paths: &Paths, callers: &[String]) -> std::io::Result<()> {
    let dir = paths.root.join("handoff");
    std::fs::create_dir_all(&dir)?;
    let text = serde_json::to_string_pretty(&json!({"callers": callers}))
        .map_err(std::io::Error::other)?;
    std::fs::write(callers_path(paths), text)
}

fn sidecar_path() -> std::io::Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe.with_file_name("udm-host.caller"))
}

pub fn run(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("--register") if args.len() == 3 => {
            register(&args[1], &args[2])
        }
        Some("--unregister") if args.len() == 2 => unregister(&args[1]),
        _ => {
            eprintln!("usage: udm-host --register <chrome|firefox> <extension-id>");
            eprintln!("       udm-host --unregister <chrome|firefox>");
            eprintln!("       udm-host            (bridge mode, run by the browser)");
            2
        }
    }
}

fn register(browser: &str, ext_id: &str) -> i32 {
    let Some(spec) = describe(browser, ext_id) else {
        eprintln!("udm-host: unsupported browser '{browser}' (chrome or firefox)");
        return 2;
    };
    if ext_id.is_empty() || ext_id.contains(['/', '\\', ' ', '"']) {
        eprintln!("udm-host: extension id looks invalid");
        return 2;
    }
    let paths = match Paths::discover() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("udm-host: cannot locate the data directory: {e}");
            return 1;
        }
    };
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("udm-host: cannot locate this executable: {e}");
            return 1;
        }
    };
    let exe_str = exe.to_string_lossy().to_string();
    let dir = paths.root.join("handoff");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("udm-host: cannot create {}: {e}", dir.display());
        return 1;
    }
    let manifest_path = dir.join(format!("host-manifest-{browser}.json"));
    let manifest = manifest_json(&spec, ext_id, &exe_str);
    let text = serde_json::to_string_pretty(&manifest).unwrap_or_default();
    if let Err(e) = std::fs::write(&manifest_path, text) {
        eprintln!("udm-host: cannot write the manifest: {e}");
        return 1;
    }
    if let Err(e) = platform::set_host_key(&spec, &manifest_path.to_string_lossy()) {
        eprintln!("udm-host: registry write failed: {e}");
        return 1;
    }
    match sidecar_path() {
        Ok(sidecar) => {
            if let Err(e) = std::fs::write(&sidecar, format!("{}\n", spec.caller)) {
                eprintln!("udm-host: cannot write the caller sidecar: {e}");
                return 1;
            }
        }
        Err(e) => {
            eprintln!("udm-host: cannot locate the sidecar path: {e}");
            return 1;
        }
    }
    let mut callers = read_callers(&paths);
    if !callers.contains(&spec.caller) {
        callers.push(spec.caller.clone());
    }
    if let Err(e) = write_callers(&paths, &callers) {
        eprintln!("udm-host: cannot write the callers list: {e}");
        return 1;
    }
    println!(
        "registered {} for {} ({})",
        spec.host_name, spec.browser, spec.caller
    );
    println!("manifest: {}", manifest_path.display());
    0
}

fn unregister(browser: &str) -> i32 {
    // The extension id is not needed to undo: describe with a placeholder
    // to recover the key path shape, then strip any caller of this
    // browser from the list.
    let Some(spec) = describe(browser, "x") else {
        eprintln!("udm-host: unsupported browser '{browser}' (chrome or firefox)");
        return 2;
    };
    if let Err(e) = platform::delete_host_key(&spec) {
        eprintln!("udm-host: registry delete failed: {e}");
        return 1;
    }
    if let Ok(paths) = Paths::discover() {
        let prefix = format!("{browser}:");
        let callers: Vec<String> = read_callers(&paths)
            .into_iter()
            .filter(|c| !c.starts_with(&prefix))
            .collect();
        let _ = write_callers(&paths, &callers);
    }
    if let Ok(sidecar) = sidecar_path() {
        if let Ok(text) = std::fs::read_to_string(&sidecar) {
            if text.trim_start().starts_with(&format!("{browser}:")) {
                let _ = std::fs::remove_file(&sidecar);
            }
        }
    }
    println!("unregistered the {} handoff host", browser);
    0
}

#[cfg(windows)]
mod platform {
    use super::BrowserSpec;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteTreeW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn check(code: u32, what: &str) -> std::io::Result<()> {
        if code == 0 {
            Ok(())
        } else {
            Err(std::io::Error::other(format!("{what}: os error {code}")))
        }
    }

    pub fn set_host_key(spec: &BrowserSpec, manifest_path: &str) -> std::io::Result<()> {
        unsafe {
            let key = to_wide(&spec.registry_key);
            let mut hkey: HKEY = 0;
            let code = RegCreateKeyExW(
                HKEY_CURRENT_USER,
                key.as_ptr(),
                0,
                std::ptr::null_mut(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                std::ptr::null(),
                &mut hkey,
                std::ptr::null_mut(),
            );
            check(code, "create key")?;
            let value = to_wide(manifest_path);
            let bytes = (value.len() * 2) as u32;
            let code = RegSetValueExW(
                hkey,
                std::ptr::null_mut(),
                0,
                REG_SZ,
                value.as_ptr() as *const u8,
                bytes,
            );
            let close = RegCloseKey(hkey);
            let _ = close;
            check(code, "set manifest path")?;
            Ok(())
        }
    }

    pub fn delete_host_key(spec: &BrowserSpec) -> std::io::Result<()> {
        unsafe {
            let key = to_wide(&spec.registry_key);
            let code = RegDeleteTreeW(HKEY_CURRENT_USER, key.as_ptr());
            // 2 == ERROR_FILE_NOT_FOUND: already gone is success here.
            if code == 2 {
                return Ok(());
            }
            check(code, "delete key")
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::BrowserSpec;

    pub fn set_host_key(_spec: &BrowserSpec, _manifest_path: &str) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "registration exists only on Windows",
        ))
    }

    pub fn delete_host_key(_spec: &BrowserSpec) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "registration exists only on Windows",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_manifest_pins_the_single_extension() {
        let spec = describe("chrome", "abc123").unwrap();
        assert_eq!(spec.caller, "chrome:abc123");
        let m = manifest_json(&spec, "abc123", r"C:\udm\udm-host.exe");
        assert_eq!(m["allowed_origins"][0], "chrome-extension://abc123/");
        assert_eq!(m["type"], "stdio");
    }

    #[test]
    fn firefox_manifest_uses_allowed_extensions() {
        let spec = describe("firefox", "ext@example.com").unwrap();
        assert_eq!(spec.caller, "firefox:ext@example.com");
        let m = manifest_json(&spec, "ext@example.com", r"C:\udm\udm-host.exe");
        assert_eq!(m["allowed_extensions"][0], "ext@example.com");
    }

    #[test]
    fn unknown_browser_is_rejected() {
        assert!(describe("safari", "x").is_none());
    }
}

//! Start-with-Windows (G3 part C2), the EasyKey mechanism adapted to
//! UDM's invariants: a per-user HKCU Run entry only - no HKLM, no
//! service, no scheduled task, no administrator rights, and never
//! silently enabled (the Settings toggle is the only writer, default
//! off, with an on-screen disclosure).
//!
//! Behaviour contract (owner-approved in the G3 spec):
//! * The entry value is `UDM` under
//!   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, holding the
//!   CURRENT executable path in double quotes, plus the internal
//!   `--minimized` flag when the start_minimized setting is on.
//! * The entry is re-synced from the stored setting on every app start,
//!   so a portable copy moved to a new folder repairs its own entry on
//!   the next launch from the new location.
//! * The Settings toggle reflects the registry as it actually is when
//!   the view opens, not just the stored setting.
//!
//! All registry access is confined to this module (Windows); other
//! platforms get inert stubs that report "not enabled".

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
        HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_SZ,
    };

    const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
    const VALUE_NAME: &str = "UDM";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn open(key_path: &str, access: u32) -> Option<HKEY> {
        unsafe {
            let mut key: HKEY = 0;
            let path = wide(key_path);
            if RegOpenKeyExW(HKEY_CURRENT_USER, path.as_ptr(), 0, access, &mut key) == ERROR_SUCCESS
            {
                Some(key)
            } else {
                None
            }
        }
    }

    pub fn is_enabled() -> bool {
        let Some(key) = open(RUN_KEY, KEY_READ) else {
            return false;
        };
        let name = wide(VALUE_NAME);
        let mut len: u32 = 0;
        let rc = unsafe {
            RegQueryValueExW(
                key,
                name.as_ptr(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &mut len,
            )
        };
        unsafe { RegCloseKey(key) };
        rc == ERROR_SUCCESS
    }

    /// Make the registry match the settings: write the entry (current
    /// exe path, quoted; `--minimized` when asked) or remove it.
    pub fn sync(enabled: bool, start_minimized: bool) -> Result<(), String> {
        if !enabled {
            if let Some(key) = open(RUN_KEY, KEY_SET_VALUE) {
                let name = wide(VALUE_NAME);
                unsafe {
                    // Deleting an absent value is not an error here: the
                    // desired end state is "no entry".
                    RegDeleteValueW(key, name.as_ptr());
                    RegCloseKey(key);
                }
            }
            return Ok(());
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        let mut value = format!("\"{}\"", exe.to_string_lossy());
        if start_minimized {
            value.push_str(" --minimized");
        }
        let key = open(RUN_KEY, KEY_SET_VALUE)
            .ok_or_else(|| "cannot open the Run registry key".to_string())?;
        let name = wide(VALUE_NAME);
        let data = wide(&value);
        let rc = unsafe {
            RegSetValueExW(
                key,
                name.as_ptr(),
                0,
                REG_SZ,
                data.as_ptr() as *const u8,
                (data.len() * 2) as u32,
            )
        };
        unsafe { RegCloseKey(key) };
        if rc != ERROR_SUCCESS {
            return Err(format!("writing the Run entry failed (code {rc})"));
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn is_enabled() -> bool {
        false
    }

    pub fn sync(_enabled: bool, _start_minimized: bool) -> Result<(), String> {
        Ok(())
    }
}

pub use imp::{is_enabled, sync};

/// True when the running executable sits under the system Temp folder or
/// the user's Downloads folder - locations that get cleaned, which would
/// silently break a startup entry pointing there. Settings shows a soft
/// warning (never a block) in that case.
pub fn exe_in_temp_or_downloads() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let exe = exe.to_string_lossy().to_lowercase();
    let temp = std::env::temp_dir().to_string_lossy().to_lowercase();
    if !temp.is_empty() && exe.starts_with(&temp) {
        return true;
    }
    if let Some(home) = std::env::var_os("USERPROFILE") {
        let downloads = format!("{}\\downloads\\", home.to_string_lossy().to_lowercase());
        if exe.starts_with(&downloads) {
            return true;
        }
    }
    false
}

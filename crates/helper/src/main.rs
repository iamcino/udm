//! # UDM updater trampoline helper
//!
//! Wiring W2 (envelope item 8): when the running `udm.exe` refuses to
//! be renamed during an explicit Apply, the GUI spawns this helper
//! with one argument — the install base — and exits. The helper waits
//! for that parent to exit, drops the Prepared swap journal through
//! `recover_updater`, completes the swap with `apply_staged_with_trust`
//! and starts the new `udm.exe`. It never fetches anything, runs
//! per-user with no administrator rights, no service and no scheduled
//! task, and writes only on the install base. Breadcrumbs go to
//! `updater-helper.log` there because the GUI spawns it without a
//! console.

// The GUI spawns this without a console; keep it that way on release.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::path::{Path, PathBuf};

use usagi::update_apply::{apply_staged_with_trust, recover_updater, InstallLayout};
use usagi::update_manifest::TrustRoot;
use usagi::update_records::{ClientTrust, Ed25519Identity};

fn main() {
    let mut args = std::env::args_os();
    let _self = args.next();
    let base = match (args.next(), args.next()) {
        (Some(base), None) => PathBuf::from(base),
        _ => {
            eprintln!("usage: udm-updater-helper <install base>");
            std::process::exit(2);
        }
    };
    let layout = InstallLayout::new(&base);
    let log = layout.base().join("updater-helper.log");

    // No console when the GUI spawns this: a panic must still leave a
    // breadcrumb on the install base.
    let panic_log = log.clone();
    std::panic::set_hook(Box::new(move |info| {
        log_line(&panic_log, &format!("PANIC {info}"));
    }));

    log_line(&log, &format!("helper started for {}", base.display()));
    wait_for_parent();
    log_line(&log, "parent exited");

    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    // Same trust chain as the GUI/CLI apply, from the compiled pin.
    let trust = match pinned_trust(&layout) {
        Ok(trust) => trust,
        Err(e) => {
            log_line(&log, &format!("cannot load updater trust: {e}"));
            std::process::exit(1);
        }
    };

    // Drop the Prepared journal the refused swap left behind; the live
    // files are still the old build. Only then may the swap run again.
    match recover_updater(&layout, now_unix) {
        Ok(report) => log_line(&log, &format!("recovery: {:?}", report.outcome)),
        Err(e) => {
            log_line(&log, &format!("recovery failed: {e}"));
            std::process::exit(1);
        }
    }

    match apply_staged_with_trust(&layout, &trust, now_unix) {
        Ok(journal) => log_line(
            &log,
            &format!(
                "swap complete: version {} step {:?}",
                journal.new_version, journal.step
            ),
        ),
        Err(e) => {
            log_line(&log, &format!("apply failed: {e}"));
            std::process::exit(1);
        }
    }

    let exe = layout.base().join("udm.exe");
    match std::process::Command::new(&exe).spawn() {
        Ok(_) => log_line(&log, &format!("started the new build at {}", exe.display())),
        Err(e) => {
            log_line(&log, &format!("cannot start {}: {e}", exe.display()));
            std::process::exit(1);
        }
    }
}

fn pinned_trust(layout: &InstallLayout) -> Result<ClientTrust, usagi::update_records::RecordError> {
    let root = TrustRoot::pinned();
    let identity = Ed25519Identity::from_pinning(root.public_key, &root.pin)?;
    let mut trust = ClientTrust::bootstrap(identity)?;
    trust.load_into(&layout.updater_trust_file())?;
    Ok(trust)
}

fn log_line(log: &Path, msg: &str) {
    use std::io::Write;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = writeln!(file, "{now} {msg}");
    }
}

/// Block until the process that spawned this helper has exited. The
/// GUI quits right after `spawn_helper`, so the live exe is unlocked
/// once this returns.
#[cfg(windows)]
fn wait_for_parent() {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
    };

    // Find the parent PID in the process snapshot, then wait on the
    // process handle. A failed open means the parent is already gone.
    unsafe {
        let me = std::process::id();
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return;
        }
        let mut entry: PROCESSENTRY32W = std::mem::zeroed();
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut parent = None;
        if Process32FirstW(snapshot, &mut entry) != 0 {
            loop {
                if entry.th32ProcessID == me {
                    parent = Some(entry.th32ParentProcessID);
                    break;
                }
                if Process32NextW(snapshot, &mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        if let Some(pid) = parent {
            let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, pid);
            if handle != 0 {
                let wait = WaitForSingleObject(handle, INFINITE);
                debug_assert_eq!(wait, WAIT_OBJECT_0);
                CloseHandle(handle);
            }
        }
    }
}

/// Unix dev builds: a process is reparented when its parent exits, so
/// the parent PID changing is the exit signal. No libc needed.
#[cfg(unix)]
fn wait_for_parent() {
    let parent = std::os::unix::process::parent_id();
    while std::os::unix::process::parent_id() == parent {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(not(any(windows, unix)))]
fn wait_for_parent() {}

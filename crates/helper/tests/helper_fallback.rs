//! Wiring W2 item 8 gate: `ExeRenameRefused` → the trampoline helper
//! completes the swap, demonstrated on Windows in a scratch install
//! layout. The staged candidate is written with `stage_files` (TEST
//! keys only; the unsigned candidate skips the trust gate — the
//! TEST-key-signed swap path is proven in usagi's w2 apply tests).
//!
//! The test binary runs in two modes. The observer builds the scratch
//! install, copies the package-built helper to `layout.helper_path()`
//! and re-executes itself as a short-lived parent. That parent stages
//! the candidate, holds the live exe with a share-mode lock, hits
//! `ExeRenameRefused`, spawns the helper and exits. The helper waits
//! for the parent, recovers the Prepared journal, swaps and relaunches
//! the new `udm.exe` (a harmless real executable, so the start step
//! runs something that exits on its own). The observer waits for the
//! helper's final log line before touching the journal (redb permits
//! a single opener, so polling the database while the helper works
//! would race its journal writes), then records the first-start
//! receipt.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use usagi::update_apply::{
    apply_staged_with_trust, confirm_successful_start, current_journal, spawn_helper, stage_files,
    staged_candidate, ApplyError, ConfirmOutcome, InstallLayout, StageInput, Step,
};
use usagi::update_manifest::TrustRoot;
use usagi::update_records::{ClientTrust, Ed25519Identity};

const OLD_EXE: &[u8] = b"w2 helper old udm exe bytes";
const OLD_CLI: &[u8] = b"w2 helper old udm cli bytes";
const NEW_CLI: &[u8] = b"w2 helper new udm cli bytes";
const NOW: i64 = 1_700_086_400;

fn main() {
    #[cfg(windows)]
    {
        if std::env::var_os("UDM_W2_HELPER_PARENT").is_some() {
            parent_scenario();
        } else {
            observer_scenario();
        }
    }
    #[cfg(not(windows))]
    {
        println!("helper fallback gate: Windows-only (share-mode rename refusal)");
    }
}

#[cfg(windows)]
fn observer_scenario() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("install");
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("udm.exe"), OLD_EXE).unwrap();
    fs::write(base.join("udm-cli.exe"), OLD_CLI).unwrap();
    let layout = InstallLayout::new(&base);

    // The package-built helper takes its place in the scratch install.
    fs::copy(
        env!("CARGO_BIN_EXE_udm-updater-helper"),
        layout.helper_path(),
    )
    .unwrap();

    // The staged new udm.exe is a harmless real executable so the
    // helper's relaunch starts something that runs and exits.
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_owned());
    let new_exe = fs::read(Path::new(&system_root).join("System32").join("whoami.exe")).unwrap();
    let new_dir = root.path().join("new");
    fs::create_dir_all(&new_dir).unwrap();
    fs::write(new_dir.join("udm.exe"), &new_exe).unwrap();
    fs::write(new_dir.join("udm-cli.exe"), NEW_CLI).unwrap();

    // Re-exec this binary as the short-lived parent that stages, locks
    // the live exe, hits the refusal, spawns the helper and exits.
    let me = std::env::current_exe().unwrap();
    let status = std::process::Command::new(me)
        .env("UDM_W2_HELPER_PARENT", "1")
        .env("UDM_W2_HELPER_DIR", root.path())
        .status()
        .unwrap();
    assert!(status.success(), "the parent scenario failed");

    // The helper owns the journal database until its apply returns:
    // redb allows only one opener, so polling current_journal here
    // raced the helper's own journal writes. Wait on the helper's log
    // instead (plain file reads, no lock): "started the new build" is
    // its last action, and after "swap complete" it never opens the
    // database again.
    let log_path = base.join("updater-helper.log");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut log = String::new();
    let mut finished = false;
    while Instant::now() < deadline {
        log = fs::read_to_string(&log_path).unwrap_or_default();
        if log.contains("started the new build") {
            finished = true;
            break;
        }
        for marker in [
            "recovery failed",
            "apply failed",
            "cannot load updater trust",
            "cannot start",
            "PANIC",
        ] {
            assert!(
                !log.contains(marker),
                "the helper failed.\nhelper log:\n{log}"
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        finished,
        "the helper did not complete the swap.\nhelper log:\n{log}"
    );

    // The helper is done with the journal database; safe to read now.
    assert_eq!(
        current_journal(&layout).unwrap().unwrap().step,
        Step::NewStaged
    );
    assert_eq!(fs::read(base.join("udm.exe")).unwrap(), new_exe);
    assert_eq!(fs::read(base.join("udm-cli.exe")).unwrap(), NEW_CLI);
    assert_eq!(fs::read(base.join("udm.exe.old")).unwrap(), OLD_EXE);
    assert!(staged_candidate(&layout).unwrap().is_none());

    // The receipt: the relaunched build's first start confirms.
    assert_eq!(
        confirm_successful_start(&layout).unwrap(),
        ConfirmOutcome::Confirmed {
            version: "0.2.0".to_owned()
        }
    );
    assert!(current_journal(&layout).unwrap().is_none());
    assert!(!base.join("udm.exe.old").exists());
    assert!(!base.join("udm-cli.exe.old").exists());
    println!(
        "helper fallback: ExeRenameRefused -> helper swapped and relaunched; receipt confirmed"
    );
}

#[cfg(windows)]
fn parent_scenario() {
    let root = PathBuf::from(std::env::var_os("UDM_W2_HELPER_DIR").unwrap());
    let base = root.join("install");
    let layout = InstallLayout::new(&base);

    stage_files(&layout, "0.2.0", &stage_inputs(&root.join("new"))).unwrap();
    let trust = test_trust(&layout);

    let _lock = win_lock::Lock::hold(&base.join("udm.exe"));
    let err = apply_staged_with_trust(&layout, &trust, NOW).unwrap_err();
    assert!(
        matches!(err, ApplyError::ExeRenameRefused { .. }),
        "expected ExeRenameRefused, got {err:?}"
    );
    assert_eq!(
        current_journal(&layout).unwrap().unwrap().step,
        Step::Prepared
    );
    assert_eq!(fs::read(base.join("udm.exe")).unwrap(), OLD_EXE);
    assert_eq!(fs::read(base.join("udm-cli.exe")).unwrap(), OLD_CLI);

    spawn_helper(&layout).unwrap();
    println!("parent: refusal recorded, helper spawned; exiting");
    // The lock drops with this process; the helper proceeds once it
    // sees the parent exit.
}

#[cfg(windows)]
fn stage_inputs(dir: &Path) -> Vec<StageInput> {
    use sha2::Digest;

    ["udm.exe", "udm-cli.exe"]
        .iter()
        .map(|name| {
            let source = dir.join(name);
            let bytes = fs::read(&source).unwrap();
            StageInput {
                path: (*name).to_owned(),
                source,
                sha256: hex::encode(sha2::Sha256::digest(&bytes)),
                size: bytes.len() as u64,
            }
        })
        .collect()
}

/// A random TEST key through the same bootstrap chain production uses
/// with the compiled pin. The unsigned candidate never consults it.
#[cfg(windows)]
fn test_trust(layout: &InstallLayout) -> ClientTrust {
    use ring::signature::{Ed25519KeyPair, KeyPair as _};
    use sha2::{Digest, Sha256};

    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&ring::rand::SystemRandom::new()).unwrap();
    let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let mut public_key = [0u8; 32];
    public_key.copy_from_slice(key.public_key().as_ref());
    let root = TrustRoot {
        public_key,
        pin: hex::encode(Sha256::digest(public_key)),
    };
    let identity = Ed25519Identity::from_pinning(root.public_key, &root.pin).unwrap();
    let mut trust = ClientTrust::bootstrap(identity).unwrap();
    trust.load_into(&layout.updater_trust_file()).unwrap();
    trust
}

/// A Windows share-mode hold without FILE_SHARE_DELETE: renaming the
/// held file fails, exactly like the running exe. No admin rights.
#[cfg(windows)]
mod win_lock {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    extern "system" {
        fn CreateFileW(
            filename: *const u16,
            desired_access: u32,
            share_mode: u32,
            security_attributes: *mut std::ffi::c_void,
            creation_disposition: u32,
            flags_and_attributes: u32,
            template_file: *mut std::ffi::c_void,
        ) -> *mut std::ffi::c_void;
        fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
    }

    #[derive(Debug)]
    pub struct Lock(*mut std::ffi::c_void);

    impl Lock {
        pub fn hold(path: &Path) -> Self {
            let wide: Vec<u16> = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect();
            const GENERIC_READ: u32 = 0x8000_0000;
            const FILE_SHARE_READ: u32 = 0x1;
            const FILE_SHARE_WRITE: u32 = 0x2;
            const OPEN_EXISTING: u32 = 3;
            let handle = unsafe {
                CreateFileW(
                    wide.as_ptr(),
                    GENERIC_READ,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null_mut(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                )
            };
            assert!(
                handle as isize != -1,
                "cannot hold {} (error {})",
                path.display(),
                std::io::Error::last_os_error()
            );
            Lock(handle)
        }
    }

    impl Drop for Lock {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

//! Wiring W2 tests: the apply/relaunch/receipt path (envelope items 6
//! and 7) and the trampoline preconditions (item 8), driven with TEST
//! keys against the U8 loopback TLS fixture. Tests never call
//! `FetchSession::open`, never use the real pins, and never use the
//! real hosts.
//!
//! The receipt is the relaunched build's first successful start:
//! `recover_updater` leaves the journal at `NewStaged`, and
//! `confirm_successful_start` records it. The GUI and CLI source-shape
//! tests assert that both surfaces start the new exe after a
//! successful swap and that neither fetches during apply.

#[path = "common/mod.rs"]
mod common;

use std::fs;
use std::path::Path;

use common::u8_fixture::*;
use usagi::update_apply::{
    apply_staged_with_trust, confirm_successful_start, current_journal, recover_updater,
    spawn_helper, stage_files, staged_candidate, ApplyError, ConfirmOutcome, InstallLayout,
    RecoveryOutcome, StageInput, Step,
};
use usagi::update_check::{run_update_check, set_checks_enabled, CheckKind};
use usagi::update_manifest::TrustRoot;
use usagi::update_records::{ClientTrust, Ed25519Identity};

const NEW_EXE: &[u8] = b"w2 new udm exe bytes";
const NEW_CLI: &[u8] = b"w2 new udm cli bytes";
const OLD_EXE: &[u8] = b"w2 old udm exe bytes";
const OLD_CLI: &[u8] = b"w2 old udm cli bytes";
const FILES: &[(&str, &[u8])] = &[("udm.exe", NEW_EXE), ("udm-cli.exe", NEW_CLI)];

/// A scratch install with the old build's live files in place.
fn scratch_install(root: &Path) -> InstallLayout {
    let base = root.join("install");
    fs::create_dir_all(&base).unwrap();
    fs::write(base.join("udm.exe"), OLD_EXE).unwrap();
    fs::write(base.join("udm-cli.exe"), OLD_CLI).unwrap();
    InstallLayout::new(&base)
}

/// TEST-key trust through the same bootstrap chain the production
/// callers use with the compiled pin.
fn trust_from(root: &TrustRoot, layout: &InstallLayout) -> ClientTrust {
    let identity = Ed25519Identity::from_pinning(root.public_key, &root.pin).unwrap();
    let mut trust = ClientTrust::bootstrap(identity).unwrap();
    trust.load_into(&layout.updater_trust_file()).unwrap();
    trust
}

fn stage_inputs(dir: &Path) -> Vec<StageInput> {
    ["udm.exe", "udm-cli.exe"]
        .iter()
        .map(|name| {
            let source = dir.join(name);
            let bytes = fs::read(&source).unwrap();
            StageInput {
                path: (*name).to_owned(),
                source,
                sha256: sha256_bytes(&bytes),
                size: bytes.len() as u64,
            }
        })
        .collect()
}

/// Item 7: a TEST-key check stages the candidate; the explicit swap
/// reaches NewStaged; the relaunched build's startup recovery plus
/// first-start confirmation is the receipt.
#[tokio::test]
async fn apply_swaps_then_first_start_confirms_the_receipt() {
    let root = tempfile::tempdir().unwrap();
    let layout = scratch_install(root.path());
    let base = layout.base().to_path_buf();

    let certs = fixture_certs();
    let signer = publisher();
    let loop_a = spawn(certs.server.clone()).await;
    let loop_b = spawn(certs.server.clone()).await;
    let release = release_at(
        &[&signer],
        loop_a.addr.port(),
        FILES,
        &ManifestSpec::default(),
    );
    loop_a.set(serve_release(release.json.clone(), release.zip.clone()));
    loop_b.set(serve_release(release.json, release.zip));
    let origins = origins_for(loop_a.addr, loop_b.addr);
    let session = session(origins, &certs, signer.trust.clone());

    set_checks_enabled(&layout, true).unwrap();
    let outcome = run_update_check(&layout, NOW, || Ok(session)).await;
    assert_eq!(outcome.kind, CheckKind::Available);
    assert!(staged_candidate(&layout).unwrap().is_some());

    let trust = trust_from(&signer.trust, &layout);
    let journal = apply_staged_with_trust(&layout, &trust, NOW).unwrap();
    assert_eq!(journal.step, Step::NewStaged);
    assert_eq!(journal.new_version, "0.2.0");
    assert_eq!(fs::read(base.join("udm.exe")).unwrap(), NEW_EXE);
    assert_eq!(fs::read(base.join("udm-cli.exe")).unwrap(), NEW_CLI);
    assert_eq!(fs::read(base.join("udm.exe.old")).unwrap(), OLD_EXE);
    assert_eq!(fs::read(base.join("udm-cli.exe.old")).unwrap(), OLD_CLI);
    // The swap consumed the held candidate.
    assert!(staged_candidate(&layout).unwrap().is_none());

    // The new process's startup: recovery sees the swapped files and
    // waits for the first-start receipt; no user notice on this path.
    let report = recover_updater(&layout, NOW).unwrap();
    assert_eq!(
        report.outcome,
        RecoveryOutcome::AwaitingFirstStart {
            version: "0.2.0".to_owned()
        }
    );
    assert!(report.notice.is_none());

    // The receipt: the first successful start completes the swap and
    // reclaims the old build. A second start has nothing pending.
    let confirmed = confirm_successful_start(&layout).unwrap();
    assert_eq!(
        confirmed,
        ConfirmOutcome::Confirmed {
            version: "0.2.0".to_owned()
        }
    );
    assert!(current_journal(&layout).unwrap().is_none());
    assert!(!base.join("udm.exe.old").exists());
    assert!(!base.join("udm-cli.exe.old").exists());
    assert_eq!(
        confirm_successful_start(&layout).unwrap(),
        ConfirmOutcome::NothingPending
    );
}

/// Item 8 precondition: a locked live exe refuses the rename; the
/// journal stays at Prepared, every live file keeps its old bytes, and
/// the next start discards the pending swap while the staged candidate
/// survives for a later apply. Windows-only: the lock is a share-mode
/// hold that Unix renames would ignore.
#[cfg(windows)]
#[test]
fn exe_rename_refused_keeps_prepared_journal_and_old_build() {
    let root = tempfile::tempdir().unwrap();
    let layout = scratch_install(root.path());
    let base = layout.base().to_path_buf();

    let new_dir = root.path().join("new");
    fs::create_dir_all(&new_dir).unwrap();
    fs::write(new_dir.join("udm.exe"), NEW_EXE).unwrap();
    fs::write(new_dir.join("udm-cli.exe"), NEW_CLI).unwrap();
    stage_files(&layout, "0.2.0", &stage_inputs(&new_dir)).unwrap();
    // The unsigned candidate skips the trust gate, but the call still
    // takes a trust value; any TEST root will do.
    let trust = trust_from(&publisher().trust, &layout);

    let lock = win_lock::Lock::hold(&base.join("udm.exe"));
    let err = apply_staged_with_trust(&layout, &trust, NOW).unwrap_err();
    assert!(
        matches!(err, ApplyError::ExeRenameRefused { .. }),
        "expected ExeRenameRefused, got {err:?}"
    );
    let journal = current_journal(&layout)
        .unwrap()
        .expect("the Prepared journal stays");
    assert_eq!(journal.step, Step::Prepared);
    assert_eq!(fs::read(base.join("udm.exe")).unwrap(), OLD_EXE);
    assert_eq!(fs::read(base.join("udm-cli.exe")).unwrap(), OLD_CLI);
    assert!(!base.join("udm.exe.old").exists());
    assert!(staged_candidate(&layout).unwrap().is_some());

    // The next GUI start reconciles the refusal: the old build is kept.
    let report = recover_updater(&layout, NOW).unwrap();
    assert_eq!(
        report.outcome,
        RecoveryOutcome::DiscardedPrepared {
            version: "0.2.0".to_owned()
        }
    );
    assert!(report.notice.is_none());
    assert!(current_journal(&layout).unwrap().is_none());
    assert_eq!(fs::read(base.join("udm.exe")).unwrap(), OLD_EXE);

    // Once the lock is gone the same staged candidate applies.
    drop(lock);
    let journal = apply_staged_with_trust(&layout, &trust, NOW).unwrap();
    assert_eq!(journal.step, Step::NewStaged);
    assert_eq!(fs::read(base.join("udm.exe")).unwrap(), NEW_EXE);
    assert_eq!(
        confirm_successful_start(&layout).unwrap(),
        ConfirmOutcome::Confirmed {
            version: "0.2.0".to_owned()
        }
    );
}

/// Item 8: an install without the helper file reports HelperMissing
/// with the exact path the GUI would show; nothing is spawned.
#[test]
fn spawn_helper_reports_missing_helper_with_its_path() {
    let root = tempfile::tempdir().unwrap();
    let layout = InstallLayout::new(root.path());
    match spawn_helper(&layout) {
        Err(ApplyError::HelperMissing(path)) => assert_eq!(path, layout.helper_path()),
        other => panic!("expected HelperMissing, got {other:?}"),
    }
}

/// The GUI apply/relaunch/receipt shape, read as source (envelope
/// item 7): the swap sets the relaunch flag and quits; main.rs starts
/// the new exe only after the engine shut down; the new process's
/// startup confirmation is the receipt. Apply itself never fetches.
#[test]
fn gui_apply_relaunch_source_shape() {
    let gui = Path::new(env!("CARGO_MANIFEST_DIR")).join("../gui/src");
    let main_rs = fs::read_to_string(gui.join("main.rs")).unwrap();
    let state_rs = fs::read_to_string(gui.join("state.rs")).unwrap();

    let shutdown = main_rs
        .find("engine.shutdown()")
        .expect("engine shutdown call");
    let relaunch = main_rs
        .find("relaunch_after_apply.load")
        .expect("relaunch flag read");
    assert!(shutdown < relaunch, "the new exe starts after shutdown");
    assert!(main_rs.contains("layout.base().join(\"udm.exe\")"));
    // The receipt lives in the relaunched process's startup path.
    assert!(main_rs.contains("recover_updater(layout, now_unix)"));
    assert!(main_rs.contains("confirm_successful_start(layout)"));
    assert!(main_rs.contains("Step::NewStaged"));

    let apply = fn_body(&state_rs, "fn apply_update(");
    assert!(apply.contains("apply_staged_with_trust(layout, &trust, now_unix)"));
    assert!(apply.contains("relaunch_after_apply.store(true"));
    assert!(apply.contains("quit_event_loop"));
    assert!(apply.contains("spawn_helper(layout)"));
    assert!(apply.contains("HelperMissing(missing)"));
    assert!(apply.contains("Restart UDM to clear the pending swap"));
    assert!(!apply.contains("FetchSession"), "apply never fetches");
    assert!(!apply.contains(".fetch("), "apply never fetches");
    // A missing helper is reported; the GUI does not exit as if the
    // swap had been handed off.
    let missing_arm = &apply[apply.find("HelperMissing(missing)").unwrap()..];
    let missing_arm = &missing_arm[..missing_arm.find("Err(e) =>").unwrap()];
    assert!(!missing_arm.contains("quit_event_loop"));
}

/// The CLI apply shape, read as source (envelope items 6 and 7): no
/// fetch, no helper, the new exe starts on success, and the printed
/// sentence defers the receipt to the new build's first start.
#[test]
fn cli_apply_source_shape() {
    let cli = Path::new(env!("CARGO_MANIFEST_DIR")).join("../cli/src/main.rs");
    let src = fs::read_to_string(cli).unwrap();
    let apply = fn_body(&src, "fn cmd_update_apply()");
    assert!(apply.contains("staged_candidate(&layout)"));
    assert!(apply.contains("no update is staged"));
    assert!(apply.contains("apply_staged_with_trust(&layout, &trust, now_unix)"));
    assert!(apply.contains(".join(\"udm.exe\")"));
    assert!(apply.contains(".spawn()"));
    assert!(apply.contains("ExeRenameRefused"));
    assert!(apply.contains("its first start confirms the update"));
    assert!(
        !apply.contains("spawn_helper"),
        "the CLI never spawns the helper"
    );
    assert!(!apply.contains("FetchSession"), "apply never fetches");
    assert!(!apply.contains(".fetch("), "apply never fetches");
    assert!(
        !apply.contains("confirm_successful_start"),
        "the receipt belongs to the relaunched process, not the CLI"
    );
}

/// The source of one top-level function, up to the next `fn` item.
fn fn_body(src: &str, signature: &str) -> String {
    let start = src
        .find(signature)
        .unwrap_or_else(|| panic!("missing {signature}"));
    let rest = &src[start..];
    let end = rest.find("\nfn ").map(|i| i + 1).unwrap_or(rest.len());
    rest[..end].to_owned()
}

/// A Windows share-mode hold without FILE_SHARE_DELETE: renaming the
/// held file fails, which is exactly the running-exe refusal the
/// trampoline handles. No admin rights, no service.
#[cfg(windows)]
#[allow(unsafe_code)]
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

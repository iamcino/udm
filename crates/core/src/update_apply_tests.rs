//! U4 tests: held staging, the rename swap, and journal recovery at
//! every cut point the design names (REV1 sections 2.3 and 4).

use super::*;

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair as _};

use crate::update_fetch::VerifiedFile;
use crate::update_manifest::{Manifest, ManifestFile, TrustRoot, VerifyContext};
use crate::update_records::{ClientTrust, Ed25519Identity};

const NOW: i64 = 1_700_100_000;

const OLD_FILES: &[(&str, &[u8])] = &[
    ("udm.exe", b"old exe bytes"),
    ("udm-cli.exe", b"old cli bytes"),
    ("assets/a.txt", b"old asset a"),
    ("assets/b.txt", b"old asset b"),
];

const NEW_FILES: &[(&str, &[u8])] = &[
    ("udm.exe", b"new exe bytes"),
    ("udm-cli.exe", b"new cli bytes"),
    ("assets/a.txt", b"new asset a"),
    ("assets/b.txt", b"new asset b"),
    ("assets/c.txt", b"new asset c"),
];

const THIRD_FILES: &[(&str, &[u8])] = &[
    ("udm.exe", b"third exe bytes"),
    ("udm-cli.exe", b"third cli bytes"),
    ("assets/a.txt", b"third asset a"),
    ("assets/b.txt", b"third asset b"),
    ("assets/c.txt", b"third asset c"),
];

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn write_tree(base: &Path, files: &[(&str, &[u8])]) {
    for (rel, bytes) in files {
        let path = join_rel(base, rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, bytes).unwrap();
    }
}

fn fixture(dir: &Path) -> InstallLayout {
    let base = dir.join("install");
    write_tree(&base, OLD_FILES);
    InstallLayout::new(base)
}

fn stage_set(layout: &InstallLayout, dir: &Path, version: &str, files: &[(&str, &[u8])]) {
    let src = dir.join(format!("src-{version}"));
    write_tree(&src, files);
    let inputs: Vec<StageInput> = files
        .iter()
        .map(|(rel, bytes)| StageInput {
            path: (*rel).to_owned(),
            source: join_rel(&src, rel),
            sha256: sha(bytes),
            size: bytes.len() as u64,
        })
        .collect();
    stage_files(layout, version, &inputs).unwrap();
}

fn read_live(layout: &InstallLayout, rel: &str) -> Vec<u8> {
    fs::read(layout.target_for(rel)).unwrap()
}

fn assert_live(layout: &InstallLayout, files: &[(&str, &[u8])]) {
    for (rel, bytes) in files {
        assert_eq!(read_live(layout, rel), *bytes, "live file {rel}");
    }
}

fn old_siblings(layout: &InstallLayout) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![layout.base().to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if path != layout.staging_dir() {
                    stack.push(path);
                }
            } else if path.to_string_lossy().ends_with(".old") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

#[test]
fn staging_holds_the_candidate_and_touches_nothing_live() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);

    // No swap runs without the explicit apply: the live install is
    // byte-identical to the old build and no journal exists.
    assert_live(&layout, OLD_FILES);
    assert!(current_journal(&layout).unwrap().is_none());
    let candidate = staged_candidate(&layout).unwrap().unwrap();
    assert_eq!(candidate.version, "0.2.0");
    assert_eq!(candidate.files.len(), NEW_FILES.len());
}

#[test]
fn staging_rejects_a_second_candidate_and_an_unverified_source() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
    let src = dir.path().join("src2");
    write_tree(&src, NEW_FILES);
    let inputs: Vec<StageInput> = NEW_FILES
        .iter()
        .map(|(rel, bytes)| StageInput {
            path: (*rel).to_owned(),
            source: join_rel(&src, rel),
            sha256: sha(bytes),
            size: bytes.len() as u64,
        })
        .collect();
    assert!(matches!(
        stage_files(&layout, "0.2.0", &inputs),
        Err(ApplyError::AlreadyStaged)
    ));

    // A source whose bytes do not match its recorded hash never
    // reaches staging.
    let dir2 = tempfile::tempdir().unwrap();
    let layout2 = fixture(dir2.path());
    let mut bad = inputs.clone();
    bad[0].sha256 = sha(b"different bytes");
    assert!(matches!(
        stage_files(&layout2, "0.2.0", &bad),
        Err(ApplyError::HashMismatch { .. })
    ));
    assert!(staged_candidate(&layout2).unwrap().is_none());
}

#[test]
fn prepared_record_carries_version_hashes_and_targets_before_any_rename() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
    let err = apply_staged_at(&layout, Some(FailPoint::AfterPrepared), None).unwrap_err();
    assert!(matches!(err, ApplyError::FaultInjected));

    let journal = current_journal(&layout).unwrap().unwrap();
    assert_eq!(journal.step, Step::Prepared);
    assert_eq!(journal.new_version, "0.2.0");
    assert_eq!(journal.files.len(), NEW_FILES.len());
    for (rel, bytes) in NEW_FILES {
        let file = journal.files.iter().find(|f| f.path == *rel).unwrap();
        assert_eq!(file.sha256, sha(bytes), "journal hash for {rel}");
        assert_eq!(file.size, bytes.len() as u64);
        assert_eq!(file.target, layout.target_for(rel).to_str().unwrap());
    }
    // Nothing has been renamed yet.
    assert_live(&layout, OLD_FILES);
    assert!(old_siblings(&layout).is_empty());
}

#[test]
fn kill_after_prepared_discards_the_journal_and_keeps_the_old_build() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
    let _ = apply_staged_at(&layout, Some(FailPoint::AfterPrepared), None);

    let report = recover_updater(&layout, NOW).unwrap();
    assert_eq!(
        report.outcome,
        RecoveryOutcome::DiscardedPrepared {
            version: "0.2.0".to_owned()
        }
    );
    assert!(report.notice.is_none());
    assert!(current_journal(&layout).unwrap().is_none());
    assert_live(&layout, OLD_FILES);
    // The reconciliation was logged locally.
    let log = fs::read_to_string(layout.recovery_log()).unwrap();
    assert!(log.contains("discarded_prepared"), "log: {log}");
}

#[test]
fn kill_between_exe_rename_and_journal_write_still_keeps_the_old_build() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
    // Hand-build the cut state: journal at Prepared, exe already
    // renamed to its `.old` sibling.
    let candidate = staged_candidate(&layout).unwrap().unwrap();
    let journal = SwapJournal {
        version: 1,
        new_version: candidate.version.clone(),
        step: Step::Prepared,
        files: candidate
            .files
            .iter()
            .map(|entry| JournalFile {
                path: entry.path.clone(),
                sha256: entry.sha256.clone(),
                size: entry.size,
                target: layout.target_for(&entry.path).to_str().unwrap().to_owned(),
            })
            .collect(),
    };
    journal_write(&layout, &journal).unwrap();
    let exe = layout.target_for(EXE_PATH);
    fs::rename(&exe, old_sibling(&exe)).unwrap();

    let report = recover_updater(&layout, NOW).unwrap();
    assert_eq!(
        report.outcome,
        RecoveryOutcome::DiscardedPrepared {
            version: "0.2.0".to_owned()
        }
    );
    assert!(report.notice.is_none());
    assert_live(&layout, OLD_FILES);
    let log = fs::read_to_string(layout.recovery_log()).unwrap();
    assert!(log.contains("discarded_prepared"), "log: {log}");
}

#[test]
fn kill_after_old_renamed_completes_forward_when_hashes_verify() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
    let _ = apply_staged_at(&layout, Some(FailPoint::AfterOldRenamed), None);
    assert_eq!(
        current_journal(&layout).unwrap().unwrap().step,
        Step::OldRenamed
    );
    assert!(!layout.target_for(EXE_PATH).exists());

    let report = recover_updater(&layout, NOW).unwrap();
    assert_eq!(
        report.outcome,
        RecoveryOutcome::CompletedForward {
            version: "0.2.0".to_owned()
        }
    );
    assert!(report.notice.is_none());
    assert_live(&layout, NEW_FILES);
    let log = fs::read_to_string(layout.recovery_log()).unwrap();
    assert!(log.contains("completed_forward"), "log: {log}");
    assert_eq!(
        current_journal(&layout).unwrap().unwrap().step,
        Step::NewStaged
    );
    // The previous build is retained until the first successful
    // start is recorded.
    assert_eq!(old_siblings(&layout).len(), OLD_FILES.len());
    assert_eq!(
        confirm_successful_start(&layout).unwrap(),
        ConfirmOutcome::Confirmed {
            version: "0.2.0".to_owned()
        }
    );
    assert!(old_siblings(&layout).is_empty());
}

#[test]
fn kill_after_old_renamed_rolls_back_when_the_staged_candidate_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
    let _ = apply_staged_at(&layout, Some(FailPoint::AfterOldRenamed), None);
    fs::write(layout.staged_file(CLI_PATH), b"corrupted cli").unwrap();

    let report = recover_updater(&layout, NOW).unwrap();
    assert_eq!(
        report.outcome,
        RecoveryOutcome::RolledBack {
            version: "0.2.0".to_owned()
        }
    );
    assert_eq!(report.notice.as_deref(), Some(ROLLED_BACK_NOTICE));
    assert!(current_journal(&layout).unwrap().is_none());
    assert_live(&layout, OLD_FILES);
    assert!(old_siblings(&layout).is_empty());
    let log = fs::read_to_string(layout.recovery_log()).unwrap();
    assert!(log.contains("rolled_back"), "log: {log}");
}

#[test]
fn kill_during_cli_move_and_during_asset_moves_complete_forward() {
    for fail in [
        FailPoint::AfterExeInPlace,
        FailPoint::AfterCliInPlace,
        FailPoint::DuringAssets { moved: 1 },
        FailPoint::DuringAssets { moved: 2 },
    ] {
        let dir = tempfile::tempdir().unwrap();
        let layout = fixture(dir.path());
        stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
        let _ = apply_staged_at(&layout, Some(fail), None);
        let report = recover_updater(&layout, NOW).unwrap();
        assert_eq!(
            report.outcome,
            RecoveryOutcome::CompletedForward {
                version: "0.2.0".to_owned()
            },
            "failpoint {fail:?}"
        );
        assert!(report.notice.is_none(), "failpoint {fail:?}");
        assert_live(&layout, NEW_FILES);
        assert_eq!(
            current_journal(&layout).unwrap().unwrap().step,
            Step::NewStaged,
            "failpoint {fail:?}"
        );
        let log = fs::read_to_string(layout.recovery_log()).unwrap();
        assert!(
            log.contains("completed_forward"),
            "failpoint {fail:?}; log: {log}"
        );
    }
}

#[test]
fn kill_after_new_staged_awaits_the_first_start_receipt() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
    let _ = apply_staged_at(&layout, Some(FailPoint::AfterNewStaged), None);

    let report = recover_updater(&layout, NOW).unwrap();
    assert_eq!(
        report.outcome,
        RecoveryOutcome::AwaitingFirstStart {
            version: "0.2.0".to_owned()
        }
    );
    assert!(report.notice.is_none());
    assert_live(&layout, NEW_FILES);
    // Not completed, not reclaimed: the receipt is still owed.
    assert_eq!(
        current_journal(&layout).unwrap().unwrap().step,
        Step::NewStaged
    );
    assert_eq!(old_siblings(&layout).len(), OLD_FILES.len());
    let log = fs::read_to_string(layout.recovery_log()).unwrap();
    assert!(log.contains("awaiting_first_start"), "log: {log}");
}

#[test]
fn new_staged_with_a_corrupt_live_file_rolls_back_to_the_previous_build() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
    apply_staged(&layout).unwrap();
    fs::write(layout.target_for("assets/a.txt"), b"corrupted").unwrap();

    let report = recover_updater(&layout, NOW).unwrap();
    assert_eq!(
        report.outcome,
        RecoveryOutcome::RolledBack {
            version: "0.2.0".to_owned()
        }
    );
    assert_eq!(report.notice.as_deref(), Some(ROLLED_BACK_NOTICE));
    assert_live(&layout, OLD_FILES);
    // The file the update newly added is removed by the rollback.
    assert!(!layout.target_for("assets/c.txt").exists());
    assert!(old_siblings(&layout).is_empty());
    assert!(current_journal(&layout).unwrap().is_none());
    let log = fs::read_to_string(layout.recovery_log()).unwrap();
    assert!(log.contains("rolled_back"), "log: {log}");
}

#[test]
fn successful_start_records_completed_and_reclaims_the_old_build() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
    let journal = apply_staged(&layout).unwrap();
    // Completed is recorded only after the defined successful start,
    // never by the swap itself.
    assert_eq!(journal.step, Step::NewStaged);
    assert_eq!(old_siblings(&layout).len(), OLD_FILES.len());

    // A first start over corrupted files earns no receipt.
    fs::write(layout.target_for(CLI_PATH), b"broken cli").unwrap();
    assert!(confirm_successful_start(&layout).is_err());
    assert_eq!(
        current_journal(&layout).unwrap().unwrap().step,
        Step::NewStaged
    );
    fs::write(layout.target_for(CLI_PATH), b"new cli bytes").unwrap();

    assert_eq!(
        confirm_successful_start(&layout).unwrap(),
        ConfirmOutcome::Confirmed {
            version: "0.2.0".to_owned()
        }
    );
    assert!(current_journal(&layout).unwrap().is_none());
    assert!(old_siblings(&layout).is_empty());
    assert_live(&layout, NEW_FILES);
    // A second confirmation is a no-op.
    assert_eq!(
        confirm_successful_start(&layout).unwrap(),
        ConfirmOutcome::NothingPending
    );
}

#[test]
fn two_completed_swaps_never_hold_two_previous_builds() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    stage_set(&layout, dir.path(), "0.2.0", NEW_FILES);
    apply_staged(&layout).unwrap();
    confirm_successful_start(&layout).unwrap();
    assert!(old_siblings(&layout).is_empty());

    stage_set(&layout, dir.path(), "0.3.0", THIRD_FILES);
    apply_staged(&layout).unwrap();
    // Exactly one previous build is retained: the 0.2.0 files.
    let retained = old_siblings(&layout);
    assert_eq!(retained.len(), NEW_FILES.len());
    for path in &retained {
        let live = PathBuf::from(path.to_string_lossy().trim_end_matches(".old").to_owned());
        let rel = live
            .strip_prefix(layout.base())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let expected = NEW_FILES.iter().find(|(r, _)| *r == rel).unwrap().1;
        assert_eq!(fs::read(path).unwrap(), expected, "retained {rel}");
    }
    confirm_successful_start(&layout).unwrap();
    assert!(old_siblings(&layout).is_empty());
    assert_live(&layout, THIRD_FILES);
}

#[test]
fn recover_with_no_journal_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    let report = recover_updater(&layout, NOW).unwrap();
    assert_eq!(report.outcome, RecoveryOutcome::NoJournal);
    assert_live(&layout, OLD_FILES);
}

#[test]
fn helper_spawn_reports_a_missing_helper() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    assert!(matches!(
        spawn_helper(&layout),
        Err(ApplyError::HelperMissing(_))
    ));
}

#[test]
fn stage_adapter_accepts_a_verified_payload_and_refuses_a_set_mismatch() {
    // A real signed manifest, verified exactly as units U2/U3 do it.
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let mut public_key = [0u8; 32];
    public_key.copy_from_slice(key.public_key().as_ref());
    let trust = TrustRoot {
        public_key,
        pin: sha(&public_key),
    };
    let issued = NOW - 60;
    let mut manifest = Manifest {
        version: "0.2.0".to_owned(),
        manifest_epoch: 7,
        issued_at: issued,
        valid_until: issued + 7 * 86_400,
        files: NEW_FILES
            .iter()
            .map(|(rel, bytes)| ManifestFile {
                path: (*rel).to_owned(),
                sha256: sha(bytes),
                size: bytes.len() as u64,
            })
            .collect(),
        min_version: "0.1.0".to_owned(),
        urls: vec!["https://127.0.0.1:9/payload.zip".to_owned()],
        signatures: Vec::new(),
    };
    let signature = key.sign(&manifest.canonical_bytes());
    manifest.signatures = vec![crate::update_manifest::ManifestSignature {
        key_id: trust.pin.clone(),
        signature: hex::encode(signature.as_ref()),
    }];
    let verified = manifest
        .verify(
            &trust,
            &VerifyContext {
                now_unix: NOW,
                last_seen_epoch: 6,
                installed_version: "0.1.0",
            },
        )
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    let src = dir.path().join("extracted");
    write_tree(&src, NEW_FILES);
    let files: Vec<VerifiedFile> = NEW_FILES
        .iter()
        .map(|(rel, bytes)| VerifiedFile {
            path: (*rel).to_owned(),
            absolute: join_rel(&src, rel),
            sha256: sha(bytes),
            size: bytes.len() as u64,
        })
        .collect();
    let payload = VerifiedPayload {
        manifest: verified.clone(),
        archive_sha256: sha(b"archive"),
        files: files.clone(),
    };
    let client_trust = ClientTrust::bootstrap(
        Ed25519Identity::from_pinning(trust.public_key, &trust.pin).unwrap(),
    )
    .unwrap();
    let candidate = stage(&layout, &payload, &client_trust, NOW).unwrap();
    assert_eq!(candidate.version, "0.2.0");
    assert_live(&layout, OLD_FILES);

    // A payload whose file set differs from the manifest is refused
    // before anything is staged.
    let dir2 = tempfile::tempdir().unwrap();
    let layout2 = fixture(dir2.path());
    let mut short = files;
    short.pop();
    let payload2 = VerifiedPayload {
        manifest: verified,
        archive_sha256: sha(b"archive"),
        files: short,
    };
    assert!(matches!(
        stage(&layout2, &payload2, &client_trust, NOW),
        Err(ApplyError::SetMismatch(_))
    ));
    assert!(staged_candidate(&layout2).unwrap().is_none());
}

#[test]
fn staging_failure_names_the_source_when_the_source_cannot_open() {
    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    let src = dir.path().join("src-0.2.0");
    write_tree(&src, NEW_FILES);
    // The exe source is missing while the rest of the set is present,
    // so the copy must fail naming the missing source, not the
    // destination.
    let missing = dir.path().join("src-missing").join("udm.exe");
    let inputs: Vec<StageInput> = NEW_FILES
        .iter()
        .map(|(rel, bytes)| StageInput {
            path: (*rel).to_owned(),
            source: if *rel == "udm.exe" {
                missing.clone()
            } else {
                join_rel(&src, rel)
            },
            sha256: sha(bytes),
            size: bytes.len() as u64,
        })
        .collect();
    let err = stage_files(&layout, "0.2.0", &inputs).unwrap_err();
    assert!(matches!(
        err,
        ApplyError::Io { ref path, .. } if *path == missing
    ));
}

#[test]
fn copy_failure_names_the_side_that_cannot_open() {
    let dir = tempfile::tempdir().unwrap();
    // A missing source fails naming the source.
    let missing = dir.path().join("missing.exe");
    let dest = dir.path().join("dest.exe");
    let err = copy_file_split(&missing, &dest).unwrap_err();
    assert!(matches!(
        err,
        ApplyError::Io { ref path, .. } if *path == missing
    ));

    // A destination blocked by a directory fails naming the
    // destination, not the source.
    let src = dir.path().join("src.exe");
    fs::write(&src, b"bytes").unwrap();
    let blocked = dir.path().join("blocked.exe");
    fs::create_dir_all(&blocked).unwrap();
    let err = copy_file_split(&src, &blocked).unwrap_err();
    assert!(matches!(
        err,
        ApplyError::Io { ref path, .. } if *path == blocked
    ));
}

#[test]
fn a_revoked_signer_is_not_staged_and_the_live_build_stays() {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
    let mut public_key = [0u8; 32];
    public_key.copy_from_slice(key.public_key().as_ref());
    let root = TrustRoot {
        public_key,
        pin: sha(&public_key),
    };
    let recovery_pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).unwrap();
    let recovery_key = Ed25519KeyPair::from_pkcs8(recovery_pkcs8.as_ref()).unwrap();
    let mut recovery_public = [0u8; 32];
    recovery_public.copy_from_slice(recovery_key.public_key().as_ref());
    let recovery = Ed25519Identity::from_public_key(recovery_public);
    let issued = NOW - 60;
    let mut manifest = Manifest {
        version: "0.2.0".to_owned(),
        manifest_epoch: 7,
        issued_at: issued,
        valid_until: issued + 7 * 86_400,
        files: NEW_FILES
            .iter()
            .map(|(rel, bytes)| ManifestFile {
                path: (*rel).to_owned(),
                sha256: sha(bytes),
                size: bytes.len() as u64,
            })
            .collect(),
        min_version: "0.1.0".to_owned(),
        urls: vec!["https://127.0.0.1:9/payload.zip".to_owned()],
        signatures: Vec::new(),
    };
    let signature = key.sign(&manifest.canonical_bytes());
    manifest.signatures = vec![crate::update_manifest::ManifestSignature {
        key_id: root.pin.clone(),
        signature: hex::encode(signature.as_ref()),
    }];
    let verified = manifest
        .verify(
            &root,
            &VerifyContext {
                now_unix: NOW,
                last_seen_epoch: 6,
                installed_version: "0.1.0",
            },
        )
        .unwrap();
    let mut trust =
        ClientTrust::bootstrap(Ed25519Identity::from_pinning(root.public_key, &root.pin).unwrap())
            .unwrap();
    let canonical = format!(
        "{{\"issued_at\":{issued},\"key_id\":\"{}\",\"kind\":\"revocation\"}}",
        root.pin
    );
    let revocation_sig = hex::encode(recovery_key.sign(canonical.as_bytes()).as_ref());
    let revocation = format!(
        "{{\"issued_at\":{issued},\"key_id\":\"{}\",\"kind\":\"revocation\",\"signatures\":[{{\"key_id\":\"{}\",\"signature\":\"{revocation_sig}\"}}]}}",
        root.pin,
        recovery.pin()
    );
    trust
        .accept_revocation(revocation.as_bytes(), NOW, &recovery)
        .unwrap();

    let dir = tempfile::tempdir().unwrap();
    let layout = fixture(dir.path());
    let src = dir.path().join("extracted");
    write_tree(&src, NEW_FILES);
    let files: Vec<VerifiedFile> = NEW_FILES
        .iter()
        .map(|(rel, bytes)| VerifiedFile {
            path: (*rel).to_owned(),
            absolute: join_rel(&src, rel),
            sha256: sha(bytes),
            size: bytes.len() as u64,
        })
        .collect();
    let payload = VerifiedPayload {
        manifest: verified,
        archive_sha256: sha(b"archive"),
        files,
    };
    assert!(matches!(
        stage(&layout, &payload, &trust, NOW),
        Err(ApplyError::StagingRefused(_))
    ));
    assert_live(&layout, OLD_FILES);
    assert!(staged_candidate(&layout).unwrap().is_none());
}

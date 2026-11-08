//! Opt-in, test-binary-only actor. No crash controls or trust overrides ship.
//!
//! Optional knobs, exact spelling, unset means the reviewed row:
//! `UDM_CAMPAIGN_POLICY` = `Overwrite` (default) | `UniqueSuffix` | `Fail`
//! `UDM_CAMPAIGN_HASH` = `Blake3` (default) | `Sha256` | `Wrong` | `WrongSha256`
//! `complete` errors print `FinalizeRejected` (with `error`) and return.
//! They are not an unwrap panic. `FinalizeComplete.final_path` is the path
//! `complete` actually published, which `UniqueSuffix` changes.
//!
//! `UDM_CAMPAIGN_ACTION=chain` is the PR 5 slice 5a composer. Cuts are
//! `VerifiedVolatile`, `CheckpointedUnchecked`, `Policy`, or a finalize cut
//! reached only after a durable checkpoint and a representation bound.
//! `Policy` applies `UDM_CAMPAIGN_HASH` and calls `complete` only when the
//! partial matches. Fault cuts exit 86. `Policy` returns. This is not a power-off.
use super::*;
use std::io::{Seek, SeekFrom, Write};
use std::time::Instant;

const CUTS: [&str; 5] = [
    "Prepared",
    "Renamed",
    "MetadataSynced",
    "BeforeCompletion",
    "CompletionRecorded",
];

#[derive(Serialize, Deserialize)]
struct Manifest {
    version: u32,
    bytes: u64,
    old_sha256: String,
    new_sha256: String,
    new_blake3: String,
}

fn file_hash(path: &Path, algo: hash::HashAlgo) -> String {
    let mut file = File::open(path).unwrap();
    let mut digest = hash::hasher(algo);
    let mut buffer = vec![0; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer).unwrap();
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    digest.finish_hex()
}

fn setup(root: &Path, mib: u64) {
    assert!((1..=16384).contains(&mib), "payload must be 1..16384 MiB");
    // Refuse existing directories; never overwrite a user's data or trial.
    fs::create_dir(root).expect("trial root must not already exist");
    let store = Store::open(root.join("state.redb")).unwrap();
    let mut record = TaskRecord::new(
        1,
        "https://campaign.invalid/payload",
        root,
        Some("payload.bin".into()),
    );
    record.state = TaskState::Downloading;
    record.total_size = Some(mib * 1024 * 1024);
    let mut file = File::create(record.part_path()).unwrap();
    let block = vec![b'N'; 1024 * 1024];
    let mut sha = hash::hasher(hash::HashAlgo::Sha256);
    let mut blake = hash::hasher(hash::HashAlgo::Blake3);
    for _ in 0..mib {
        file.write_all(&block).unwrap();
        sha.update(&block);
        blake.update(&block);
    }
    file.sync_all().unwrap();
    fs::write(record.final_path(), b"old-campaign-target").unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(record.final_path())
        .unwrap()
        .sync_all()
        .unwrap();
    let manifest = Manifest {
        version: 1,
        bytes: mib * 1024 * 1024,
        old_sha256: file_hash(&record.final_path(), hash::HashAlgo::Sha256),
        new_sha256: sha.finish_hex(),
        new_blake3: blake.finish_hex(),
    };
    store.save(&[record], &[], true).unwrap();
    let mut output = File::create(root.join("manifest.json")).unwrap();
    output
        .write_all(&serde_json::to_vec_pretty(&manifest).unwrap())
        .unwrap();
    output.sync_all().unwrap();
    println!(
        "UDM_CAMPAIGN {}",
        serde_json::json!({"event":"SetupComplete", "bytes":manifest.bytes})
    );
}

fn manifest(root: &Path) -> Manifest {
    let m: Manifest =
        serde_json::from_slice(&fs::read(root.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(m.version, 1);
    m
}

fn boundary(step: &str, cut: &str, mode: &str, started: Instant) {
    // stdout must be streamed to the host BEFORE power-off. No guest marker
    // write/fsync is issued at a cut, which would perturb storage ordering.
    println!(
        "UDM_CAMPAIGN {}",
        serde_json::json!({
            "event":step, "elapsed_us":started.elapsed().as_micros(),
            "held":step == cut && mode == "hold",
            "namespace_directory_sync":cfg!(unix),
        })
    );
    std::io::stdout().flush().unwrap();
    if step != cut {
        return;
    }
    match mode {
        "exit" => std::process::exit(86),
        "hold" => loop {
            std::thread::park();
        },
        _ => panic!("unknown cut mode"),
    }
}

fn campaign_policy() -> ConflictPolicy {
    // Settings default to UniqueSuffix. The reviewed campaign rows hard-coded
    // Overwrite, so a missing knob has to keep that row.
    match std::env::var("UDM_CAMPAIGN_POLICY") {
        Err(std::env::VarError::NotPresent) => ConflictPolicy::Overwrite,
        Err(error) => panic!("UDM_CAMPAIGN_POLICY is not unicode: {error}"),
        Ok(value) => match value.as_str() {
            "Overwrite" => ConflictPolicy::Overwrite,
            "UniqueSuffix" => ConflictPolicy::UniqueSuffix,
            "Fail" => ConflictPolicy::Fail,
            other => panic!(
                "unknown UDM_CAMPAIGN_POLICY {other:?}; expected Overwrite, UniqueSuffix, or Fail"
            ),
        },
    }
}

fn policy_name(policy: ConflictPolicy) -> &'static str {
    match policy {
        ConflictPolicy::Overwrite => "Overwrite",
        ConflictPolicy::UniqueSuffix => "UniqueSuffix",
        ConflictPolicy::Fail => "Fail",
    }
}

/// Last hex digit flipped. The length stays legal for `ExpectedHash::new`,
/// and the value cannot match the manifest digest recovery checks.
fn corrupt_digest(hex: &str) -> String {
    assert!(
        !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()),
        "manifest digest is not hex"
    );
    let mut out = hex.to_string();
    let last = out.pop().unwrap();
    out.push(if last == '0' { '1' } else { '0' });
    assert_ne!(out, hex);
    out
}

struct CampaignHash {
    name: &'static str,
    content: ExpectedHash,
}

fn campaign_hash(manifest: &Manifest) -> CampaignHash {
    let value = match std::env::var("UDM_CAMPAIGN_HASH") {
        Err(std::env::VarError::NotPresent) => "Blake3".to_string(),
        Err(error) => panic!("UDM_CAMPAIGN_HASH is not unicode: {error}"),
        Ok(value) => value,
    };
    let (name, algo, hex) = match value.as_str() {
        "Blake3" => (
            "Blake3",
            hash::HashAlgo::Blake3,
            manifest.new_blake3.clone(),
        ),
        "Sha256" => (
            "Sha256",
            hash::HashAlgo::Sha256,
            manifest.new_sha256.clone(),
        ),
        // `Wrong` corrupts the default algorithm. `WrongSha256` is the negative
        // supplied-SHA-256 row; one exclusive knob cannot say both.
        "Wrong" => (
            "Wrong",
            hash::HashAlgo::Blake3,
            corrupt_digest(&manifest.new_blake3),
        ),
        "WrongSha256" => (
            "WrongSha256",
            hash::HashAlgo::Sha256,
            corrupt_digest(&manifest.new_sha256),
        ),
        other => panic!(
            "unknown UDM_CAMPAIGN_HASH {other:?}; expected Blake3, Sha256, Wrong, or WrongSha256"
        ),
    };
    CampaignHash {
        name,
        content: ExpectedHash::new(algo, hex).expect("campaign digest length"),
    }
}

fn emit_campaign(value: serde_json::Value) {
    println!("UDM_CAMPAIGN {value}");
    std::io::stdout().flush().unwrap();
}

fn run_trial(root: &Path, cut: &str, mode: &str) {
    assert!(CUTS.contains(&cut));
    assert!(matches!(mode, "exit" | "hold"));
    let m = manifest(root);
    let policy = campaign_policy();
    let selected = campaign_hash(&m);
    let store = Store::open(root.join("state.redb")).unwrap();
    let mut record = store.get_task(1).unwrap().unwrap();
    assert!(
        record.finalization.is_none(),
        "do not rerun a cut on a used trial"
    );
    let mut segments = store.get_segments(1).unwrap();
    let file = open_regular(&record.part_path(), true).unwrap();
    let started = Instant::now();
    assert_eq!(
        file_hash(&record.final_path(), hash::HashAlgo::Sha256),
        m.old_sha256
    );
    assert_eq!(
        file_hash(&record.part_path(), hash::HashAlgo::Blake3),
        m.new_blake3
    );
    boundary("IntegrityVerified", cut, mode, started);
    let source = record.part_path();
    let target = record.final_path();
    match complete(
        &store,
        &mut record,
        &mut segments,
        &source,
        &target,
        policy,
        selected.content,
        &file,
        |step| {
            boundary(&format!("{step:?}"), cut, mode, started);
            Ok(())
        },
    ) {
        Ok(final_path) => {
            // Printed before the CompletionRecorded cut: that cut exits 86
            // inside `boundary` and would otherwise hide the resolved path.
            emit_campaign(serde_json::json!({
                "event": "FinalizeComplete",
                "cut": cut,
                "elapsed_us": started.elapsed().as_micros(),
                "policy": policy_name(policy),
                "hash": selected.name,
                "final_path": final_path.display().to_string(),
            }));
            boundary("CompletionRecorded", cut, mode, started);
            panic!("cut was not reached");
        }
        Err(error) => {
            emit_campaign(serde_json::json!({
                "event": "FinalizeRejected",
                "cut": cut,
                "elapsed_us": started.elapsed().as_micros(),
                "policy": policy_name(policy),
                "hash": selected.name,
                "error": error.to_string(),
            }));
        }
    }
}

fn recover_trial(root: &Path) {
    let m = manifest(root);
    let store = Store::open(root.join("state.redb")).unwrap();
    let started = Instant::now();
    store.recover_finalizations().unwrap();
    let recovered_us = started.elapsed().as_micros();
    let first = store.get_task(1).unwrap().unwrap();
    assert_eq!(first.state, TaskState::Completed);
    assert_eq!(fs::metadata(first.final_path()).unwrap().len(), m.bytes);
    assert_eq!(
        file_hash(&first.final_path(), hash::HashAlgo::Sha256),
        m.new_sha256
    );
    let identity = identity_at(&first.final_path()).unwrap();
    store.recover_finalizations().unwrap();
    let second = store.get_task(1).unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    assert_eq!(identity, identity_at(&second.final_path()).unwrap());
    assert!(!second.part_path().exists());
    println!(
        "UDM_CAMPAIGN {}",
        serde_json::json!({"event":"RecoveryVerified", "recovery_us":recovered_us,
        "bytes":m.bytes, "sha256":m.new_sha256, "idempotent":true})
    );
}

#[test]
#[ignore = "explicit isolated trial root and action required; never part of normal tests"]
fn campaign_actor() {
    let root = PathBuf::from(std::env::var_os("UDM_CAMPAIGN_ROOT").expect("trial root required"));
    assert!(root.is_absolute());
    match std::env::var("UDM_CAMPAIGN_ACTION").unwrap().as_str() {
        "setup" => setup(
            &root,
            std::env::var("UDM_CAMPAIGN_MIB")
                .unwrap_or_else(|_| "1".into())
                .parse()
                .unwrap(),
        ),
        "run" => run_trial(
            &root,
            &std::env::var("UDM_CAMPAIGN_CUT").unwrap(),
            &std::env::var("UDM_CAMPAIGN_MODE").unwrap_or_else(|_| "exit".into()),
        ),
        "recover" => recover_trial(&root),
        "chain" => run_chain(&root),
        _ => panic!("unknown campaign action"),
    }
}

#[test]
fn campaign_process_cuts_recover_twice() {
    for cut in CUTS {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("trial");
        setup(&root, 1);
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "finalize::campaign_tests::campaign_actor",
                "--ignored",
                "--nocapture",
            ])
            .env("UDM_CAMPAIGN_ROOT", &root)
            .env("UDM_CAMPAIGN_ACTION", "run")
            .env("UDM_CAMPAIGN_CUT", cut)
            .env("UDM_CAMPAIGN_MODE", "exit")
            // Default-row proof. A driver shell may already hold the knobs.
            .env_remove("UDM_CAMPAIGN_POLICY")
            .env_remove("UDM_CAMPAIGN_HASH");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            child.creation_flags(0x08000000);
        }
        let output = child.output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(86),
            "{cut}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains(cut));
        recover_trial(&root);
    }
}

// ---------------------------------------------------------------------------
// PR 5 slice 5a. Process-kill chains. The child exits 86. The parent reopens
// through EngineBuilder, which is the composed startup path: finalize journal,
// interrupted-to-paused, then the durability checkpoint. No power-off.
// ---------------------------------------------------------------------------

const CAMPAIGN_ETAG: &str = "\"campaign-v1\"";
const INTERRUPTED: &str = "interrupted by a previous session";

fn run_chain(root: &Path) {
    let cut = std::env::var("UDM_CAMPAIGN_CUT").expect("chain cut required");
    let mode = std::env::var("UDM_CAMPAIGN_MODE").unwrap_or_else(|_| "exit".into());
    match cut.as_str() {
        "VerifiedVolatile" => volatile_exit(root),
        "CheckpointedUnchecked" => checkpoint_exit(root),
        "Policy" => policy_step(root),
        other if CUTS.contains(&other) => {
            install_checkpoint(root);
            run_trial(root, other, &mode);
        }
        other => panic!("unknown chain cut {other}"),
    }
}

/// Hash and bound live on this stack only. The extra byte is not synced.
/// Process exit is not a power loss, so that byte may still be on disk.
/// It was never a checkpoint, and this function does not save.
fn volatile_exit(root: &Path) -> ! {
    let m = manifest(root);
    let store = Store::open(root.join("state.redb")).unwrap();
    let mut record = store.get_task(1).unwrap().unwrap();
    assert!(record.progress_checkpoint.is_none());
    assert!(record.finalization.is_none());
    record.computed_hash = Some(m.new_blake3.clone());
    record.etag = Some("\"volatile\"".into());
    record.representation_bound = Some(crate::store::RepresentationBound::none_scope(
        record.final_url.clone(),
        "\"volatile\"",
        Some(m.bytes),
    ));
    let part = record.part_path();
    drop(record);
    drop(store);
    let mut file = open_regular(&part, true).unwrap();
    file.seek(SeekFrom::End(0)).unwrap();
    file.write_all(&[0xAB]).unwrap();
    drop(file);
    emit_campaign(serde_json::json!({
        "event": "VerifiedVolatile",
        "committed": false,
    }));
    std::process::exit(86);
}

fn checkpoint_exit(root: &Path) -> ! {
    install_checkpoint(root);
    emit_campaign(serde_json::json!({
        "event": "CheckpointedUnchecked",
        "computed_hash": null,
    }));
    std::process::exit(86);
}

/// Synced coverage with no computed hash. `finished` stays false: the
/// checkpoint is not a claim that the response ended or that a hash ran.
fn install_checkpoint(root: &Path) {
    let m = manifest(root);
    assert!(m.bytes > 1, "checkpoint needs a closed byte range");
    let store = Store::open(root.join("state.redb")).unwrap();
    let mut record = store.get_task(1).unwrap().unwrap();
    assert!(record.finalization.is_none(), "checkpoint preface runs once");
    assert!(
        record.progress_checkpoint.is_none(),
        "checkpoint preface runs once"
    );
    record.etag = Some(CAMPAIGN_ETAG.to_string());
    record.representation_bound = Some(crate::store::RepresentationBound::none_scope(
        record.final_url.clone(),
        CAMPAIGN_ETAG,
        Some(m.bytes),
    ));
    record.representation_restarts = 0;
    record.computed_hash = None;
    record.expected_hash = Some(
        ExpectedHash::new(hash::HashAlgo::Blake3, m.new_blake3.clone()).expect("manifest blake3"),
    );
    let mut segment = SegmentState::new(0, 0, m.bytes - 1);
    segment.done = m.bytes;
    segment.finished = false;
    let segments = vec![segment];
    let file = open_regular(&record.part_path(), true).unwrap();
    crate::durability::prepare(&mut record, &segments, Some(&file), File::sync_all)
        .expect("checkpoint sync");
    assert!(record.progress_checkpoint.is_some());
    store
        .save(&[record], &[(1, segments)], true)
        .expect("checkpoint commit");
}

/// The campaign hash knob is the policy gate for this harness. A mismatch
/// does not call `complete`, so the old final cannot be replaced. A match
/// uses the real journal. The download worker's compare-before-finalize is
/// the live-worker slice, not this one.
fn policy_step(root: &Path) {
    let m = manifest(root);
    let selected = campaign_hash(&m);
    let store = Store::open(root.join("state.redb")).unwrap();
    let mut record = store.get_task(1).unwrap().unwrap();
    assert!(
        record.finalization.is_none(),
        "policy runs before any journal"
    );
    assert!(
        record.progress_checkpoint.is_some(),
        "policy requires a checkpoint"
    );
    let actual = file_hash(&record.part_path(), selected.content.algo);
    if !selected.content.matches(&actual) {
        emit_campaign(serde_json::json!({
            "event": "FinalizeRejected",
            "hash": selected.name,
            "reason": "checksum mismatch before finalize",
        }));
        return;
    }
    let mut segments = store.get_segments(1).unwrap();
    let source = record.part_path();
    let target = record.final_path();
    let file = open_regular(&source, true).unwrap();
    match complete(
        &store,
        &mut record,
        &mut segments,
        &source,
        &target,
        campaign_policy(),
        selected.content,
        &file,
        |_| Ok(()),
    ) {
        Ok(final_path) => emit_campaign(serde_json::json!({
            "event": "FinalizeComplete",
            "hash": selected.name,
            "final_path": final_path.display().to_string(),
        })),
        Err(error) => emit_campaign(serde_json::json!({
            "event": "FinalizeRejected",
            "hash": selected.name,
            "error": error.to_string(),
        })),
    }
}

struct Opened {
    state: TaskState,
    received: u64,
    error: Option<String>,
    computed_hash: Option<String>,
    finished_at: Option<i64>,
    final_sha256: String,
    part_exists: bool,
    part_len: Option<u64>,
    part_blake3: Option<String>,
    checkpoint: bool,
    pending_finalize: Option<bool>,
    bound: Option<String>,
    restarts: u32,
    record_json: String,
    segment_json: String,
}

fn campaign_paths(root: &Path) -> crate::Paths {
    crate::Paths {
        root: root.to_path_buf(),
        db_file: root.join("state.redb"),
        log_file: root.join("logs").join("udm.log"),
        temp_dir: root.join("tmp"),
        default_download_dir: root.to_path_buf(),
        portable: true,
        exe_dir: Some(root.to_path_buf()),
    }
}

fn open_once(root: &Path) -> Opened {
    assert_eq!(crate::store::SCHEMA_VERSION, 2);
    let engine = crate::EngineBuilder::new()
        .paths(campaign_paths(root))
        .idle(true)
        .settings(crate::Settings {
            http3_enabled: false,
            use_system_proxy: false,
            ..crate::Settings::default()
        })
        .build()
        .expect("engine restart");
    let snap = engine.task(1).expect("campaign task survived restart");
    let state = snap.state;
    let received = snap.received;
    let error = snap.error.clone();
    let computed_hash = snap.computed_hash.clone();
    let finished_at = snap.finished_at;
    engine.shutdown();
    drop(engine);
    let store = Store::open(root.join("state.redb")).unwrap();
    let record = store.get_task(1).unwrap().unwrap();
    let segments = store.get_segments(1).unwrap();
    let part = record.part_path();
    let part_exists = part.exists();
    Opened {
        state,
        received,
        error,
        computed_hash,
        finished_at,
        final_sha256: file_hash(&record.final_path(), hash::HashAlgo::Sha256),
        part_exists,
        part_len: part_exists.then(|| fs::metadata(&part).unwrap().len()),
        part_blake3: part_exists.then(|| file_hash(&part, hash::HashAlgo::Blake3)),
        checkpoint: record.progress_checkpoint.is_some(),
        pending_finalize: record.finalization.as_ref().map(|intent| intent.pending()),
        bound: record
            .representation_bound
            .as_ref()
            .map(|bound| bound.strong_etag.clone()),
        restarts: record.representation_restarts,
        record_json: serde_json::to_string(&record).unwrap(),
        segment_json: serde_json::to_string(&segments).unwrap(),
    }
}

fn reopen_twice(root: &Path, label: &str) -> Opened {
    let first = open_once(root);
    let second = open_once(root);
    assert_eq!(first.state, second.state, "{label} state");
    assert_eq!(first.received, second.received, "{label} coverage");
    assert_eq!(first.error, second.error, "{label} error");
    assert_eq!(first.finished_at, second.finished_at, "{label} finished_at");
    assert_eq!(first.computed_hash, second.computed_hash, "{label} hash");
    assert_eq!(first.final_sha256, second.final_sha256, "{label} final");
    assert_eq!(first.part_len, second.part_len, "{label} partial");
    assert_eq!(first.checkpoint, second.checkpoint, "{label} checkpoint");
    assert_eq!(first.bound, second.bound, "{label} bound");
    assert_eq!(first.record_json, second.record_json, "{label} record");
    assert_eq!(first.segment_json, second.segment_json, "{label} segments");
    first
}

fn trial_root() -> (tempfile::TempDir, PathBuf) {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("trial");
    setup(&root, 1);
    (parent, root)
}

fn spawn_exact(
    test: &str,
    ignored: bool,
    root: &Path,
    action: &str,
    cut: Option<&str>,
    hash: Option<&str>,
) -> std::process::Output {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap());
    child.arg("--exact").arg(test);
    if ignored {
        child.arg("--ignored");
    }
    child
        .arg("--nocapture")
        .env("UDM_CAMPAIGN_ROOT", root)
        .env("UDM_CAMPAIGN_ACTION", action)
        .env_remove("UDM_CAMPAIGN_POLICY")
        .env_remove("UDM_CAMPAIGN_HASH")
        .env_remove("UDM_CAMPAIGN_CUT")
        .env_remove("UDM_CAMPAIGN_MODE");
    if let Some(cut) = cut {
        child.env("UDM_CAMPAIGN_CUT", cut);
        child.env("UDM_CAMPAIGN_MODE", "exit");
    }
    if let Some(hash) = hash {
        child.env("UDM_CAMPAIGN_HASH", hash);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        child.creation_flags(0x08000000);
    }
    child.output().unwrap()
}

fn assert_code(output: &std::process::Output, code: i32, label: &str) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{label}: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn kill_chain(root: &Path, cut: &str) {
    let output = spawn_exact(
        "finalize::campaign_tests::campaign_actor",
        true,
        root,
        "chain",
        Some(cut),
        None,
    );
    assert_code(&output, 86, cut);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains(cut),
        "{cut}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn chain_verified_volatile_kill_does_not_resurrect() {
    let (_keep, root) = trial_root();
    let m = manifest(&root);
    kill_chain(&root, "VerifiedVolatile");
    let opened = reopen_twice(&root, "volatile");
    assert_eq!(opened.state, TaskState::Paused);
    assert_eq!(opened.received, 0);
    assert!(opened.computed_hash.is_none());
    assert!(opened.bound.is_none());
    assert!(!opened.checkpoint);
    assert!(opened.pending_finalize.is_none());
    assert_eq!(opened.final_sha256, m.old_sha256);
    assert_eq!(opened.error.as_deref(), Some(INTERRUPTED));
    assert_eq!(opened.restarts, 0);
    assert!(opened.part_len.is_some_and(|len| (m.bytes..=m.bytes + 1).contains(&len)));
}

#[test]
fn chain_checkpointed_unchecked_waits_for_the_hash_policy() {
    let (_keep, root) = trial_root();
    let m = manifest(&root);
    kill_chain(&root, "CheckpointedUnchecked");
    let paused = reopen_twice(&root, "unchecked");
    assert_eq!(paused.state, TaskState::Paused);
    assert_eq!(paused.received, m.bytes);
    assert!(paused.computed_hash.is_none());
    assert!(paused.checkpoint);
    assert_eq!(paused.bound.as_deref(), Some(CAMPAIGN_ETAG));
    assert_eq!(paused.final_sha256, m.old_sha256);
    assert_eq!(paused.part_blake3.as_deref(), Some(m.new_blake3.as_str()));
    assert_eq!(paused.error.as_deref(), Some(INTERRUPTED));
    assert_eq!(paused.restarts, 0);
    assert!(paused.pending_finalize.is_none());

    let rejected = spawn_exact(
        "finalize::campaign_tests::campaign_actor",
        true,
        &root,
        "chain",
        Some("Policy"),
        Some("Wrong"),
    );
    assert_code(&rejected, 0, "wrong hash");
    assert!(String::from_utf8_lossy(&rejected.stdout).contains("FinalizeRejected"));
    let still = reopen_twice(&root, "wrong hash");
    assert_eq!(still.state, TaskState::Paused);
    assert_eq!(still.received, m.bytes);
    assert_eq!(still.final_sha256, m.old_sha256);
    assert_eq!(still.part_blake3.as_deref(), Some(m.new_blake3.as_str()));
    assert!(still.pending_finalize.is_none());
    assert!(still.checkpoint);

    let accepted = spawn_exact(
        "finalize::campaign_tests::campaign_actor",
        true,
        &root,
        "chain",
        Some("Policy"),
        Some("Blake3"),
    );
    assert_code(&accepted, 0, "blake3");
    assert!(String::from_utf8_lossy(&accepted.stdout).contains("FinalizeComplete"));
    let done = reopen_twice(&root, "blake3");
    assert_eq!(done.state, TaskState::Completed);
    assert!(done.error.is_none());
    assert_eq!(done.final_sha256, m.new_sha256);
    assert!(!done.part_exists);
    assert!(!done.checkpoint);
    assert_eq!(done.pending_finalize, Some(false));
    assert_eq!(done.bound.as_deref(), Some(CAMPAIGN_ETAG));
    assert_eq!(done.restarts, 0);
    assert_eq!(done.received, m.bytes);
    assert!(done.finished_at.is_some());
}

#[test]
fn chain_prepare_crash_adopts_once_per_phase() {
    for cut in CUTS {
        let (_keep, root) = trial_root();
        let m = manifest(&root);
        kill_chain(&root, cut);
        let opened = reopen_twice(&root, cut);
        assert_eq!(opened.state, TaskState::Completed, "{cut}");
        assert!(opened.error.is_none(), "{cut} {:?}", opened.error);
        assert_eq!(opened.final_sha256, m.new_sha256, "{cut}");
        assert_eq!(
            fs::metadata(root.join("payload.bin")).unwrap().len(),
            m.bytes,
            "{cut}"
        );
        assert!(!opened.part_exists, "{cut}");
        assert!(!opened.checkpoint, "{cut}");
        assert_eq!(opened.pending_finalize, Some(false), "{cut}");
        assert_eq!(opened.bound.as_deref(), Some(CAMPAIGN_ETAG), "{cut}");
        assert_eq!(opened.restarts, 0, "{cut}");
        assert_eq!(opened.received, m.bytes, "{cut}");
        assert!(opened.finished_at.is_some(), "{cut}");
    }
}

#[test]
fn chain_representation_change_after_prepared_kill_keeps_both_files() {
    let (_keep, root) = trial_root();
    let m = manifest(&root);
    kill_chain(&root, "Prepared");
    {
        let store = Store::open(root.join("state.redb")).unwrap();
        let mut record = store.get_task(1).unwrap().unwrap();
        record.etag = Some("\"campaign-v2\"".into());
        if let Some(bound) = record.representation_bound.as_mut() {
            bound.strong_etag = "\"campaign-v2\"".into();
        }
        store.save(&[record], &[], true).unwrap();
    }
    let opened = reopen_twice(&root, "representation");
    assert_eq!(opened.state, TaskState::Failed);
    assert!(opened.error.as_ref().is_some_and(|error| !error.is_empty()));
    assert_eq!(opened.final_sha256, m.old_sha256);
    assert_eq!(opened.part_blake3.as_deref(), Some(m.new_blake3.as_str()));
    assert_eq!(opened.bound.as_deref(), Some("\"campaign-v2\""));
    assert_eq!(opened.restarts, 0);
}

#[test]
fn chain_unrelated_final_after_prepared_kill_is_not_adopted() {
    let (_keep, root) = trial_root();
    let m = manifest(&root);
    kill_chain(&root, "Prepared");
    // A truncate of the tracked file keeps its NTFS index, so the journal
    // still sees the destination it was allowed to overwrite. A different
    // file at the same path is the case the identity check has to refuse.
    let final_path = root.join("payload.bin");
    let kept = root.join("original-destination");
    fs::rename(&final_path, &kept).unwrap();
    fs::write(&final_path, b"unrelated-final").unwrap();
    let opened = reopen_twice(&root, "unrelated");
    assert_eq!(opened.state, TaskState::Failed);
    assert_eq!(fs::read(&final_path).unwrap(), b"unrelated-final");
    assert_eq!(fs::read(&kept).unwrap(), b"old-campaign-target");
    assert_eq!(opened.part_blake3.as_deref(), Some(m.new_blake3.as_str()));
    assert_ne!(opened.final_sha256, m.new_sha256);
    assert_ne!(opened.final_sha256, m.old_sha256);
}

#[test]
fn chain_replaced_backing_after_checkpoint_kill_drops_coverage() {
    let (_keep, root) = trial_root();
    let m = manifest(&root);
    kill_chain(&root, "CheckpointedUnchecked");
    let part = root.join("payload.bin.udm.part");
    let kept = root.join("payload.bin.udm.part.old-backing");
    fs::rename(&part, &kept).unwrap();
    fs::write(&part, b"replaced-backing").unwrap();
    let opened = reopen_twice(&root, "replaced backing");
    assert_eq!(opened.state, TaskState::Paused);
    assert_eq!(opened.received, 0);
    assert_eq!(opened.final_sha256, m.old_sha256);
    assert_eq!(fs::read(&part).unwrap(), b"replaced-backing");
    assert_eq!(file_hash(&kept, hash::HashAlgo::Blake3), m.new_blake3);
    assert!(opened.computed_hash.is_none());
    assert_eq!(opened.error.as_deref(), Some(INTERRUPTED));
}

#[test]
fn chain_stale_generation_kill_leaves_no_journal() {
    let (_keep, root) = trial_root();
    let m = manifest(&root);
    let output = spawn_exact(
        "engine::tests::stale_generation_exits_without_a_journal",
        false,
        &root,
        "stale",
        None,
        None,
    );
    assert_code(&output, 86, "stale");
    assert!(String::from_utf8_lossy(&output.stdout).contains("StaleGenerationRefused"));
    let opened = reopen_twice(&root, "stale");
    assert_eq!(opened.state, TaskState::Paused);
    assert!(opened.pending_finalize.is_none());
    assert_eq!(opened.final_sha256, m.old_sha256);
    assert_eq!(opened.part_blake3.as_deref(), Some(m.new_blake3.as_str()));
    assert_eq!(opened.received, 0);
    assert!(opened.bound.is_none());
    assert!(!opened.checkpoint);
    assert_eq!(opened.error.as_deref(), Some(INTERRUPTED));
}

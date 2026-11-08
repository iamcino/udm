//! G5 slice 1 gates: the handoff coordinator against the frozen
//! contract (`review-artifacts/zed-ipc-handoff-freeze-2026-11-08.md`).
//! Every test names the freeze clause it locks. The engine is a fake
//! except in `engine_marker_round_trip`, which drives the real engine
//! hook, and `legacy_task_record_json_has_no_marker`, which locks the
//! schema-2 compatibility of the additive marker field.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use udm_core::error::{Error, Result};
use udm_core::handoff::{
    BrowserDisposition, Coordinator, CoordinatorConfig, EngineAdapter, EnginePort, FileStore,
    HandoffClock, HandoffIntent, HandoffRecord, HandoffState, HandoffStatus, MemoryStore,
};
use udm_core::store::SCHEMA_VERSION;
use udm_core::{EngineBuilder, Paths, Settings};

const ALICE: &str = "chrome:alice-ext";
const BOB: &str = "firefox:bob-ext";
const T0: i64 = 1_700_000_000_000;

fn intent() -> HandoffIntent {
    HandoffIntent {
        url: "https://example.com/file.bin".to_string(),
        method: "GET".to_string(),
        filename_hint: Some("file.bin".to_string()),
        size_hint: Some(65_536),
        browser_download_id: "browser-dl-1".to_string(),
        dest_dir: None,
    }
}

#[derive(Default)]
struct FakeEngine {
    tasks: Mutex<Vec<(String, u64)>>,
    next_id: Mutex<u64>,
    add_calls: Mutex<u32>,
    down: Mutex<bool>,
}

impl FakeEngine {
    fn with_task(tx: &str, id: u64) -> FakeEngine {
        let engine = FakeEngine::default();
        engine.tasks.lock().unwrap().push((tx.to_string(), id));
        engine
    }

    fn add_calls(&self) -> u32 {
        *self.add_calls.lock().unwrap()
    }

    fn task_count(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }

    fn set_down(&self, down: bool) {
        *self.down.lock().unwrap() = down;
    }
}

impl EnginePort for FakeEngine {
    fn add_handoff_task(&self, _intent: &HandoffIntent, tx: &str) -> Result<u64> {
        if *self.down.lock().unwrap() {
            return Err(Error::Other("fake engine down".into()));
        }
        *self.add_calls.lock().unwrap() += 1;
        let mut next = self.next_id.lock().unwrap();
        *next += 1;
        let id = 1000 + *next;
        self.tasks.lock().unwrap().push((tx.to_string(), id));
        Ok(id)
    }

    fn find_handoff_task(&self, tx: &str) -> Result<Option<u64>> {
        if *self.down.lock().unwrap() {
            return Err(Error::Other("fake engine down".into()));
        }
        Ok(self
            .tasks
            .lock()
            .unwrap()
            .iter()
            .find(|(t, _)| t == tx)
            .map(|(_, id)| *id))
    }
}

struct ManualClock {
    now: Mutex<i64>,
}

impl ManualClock {
    fn new() -> Self {
        ManualClock { now: Mutex::new(T0) }
    }

    fn advance(&self, ms: i64) {
        *self.now.lock().unwrap() += ms;
    }
}

impl HandoffClock for ManualClock {
    fn now_ms(&self) -> i64 {
        *self.now.lock().unwrap()
    }
}

fn coordinator(
    engine: Arc<FakeEngine>,
    clock: Arc<ManualClock>,
    lease_ms: u64,
) -> Coordinator {
    Coordinator::open(
        Box::new(MemoryStore::new()),
        engine,
        clock,
        CoordinatorConfig {
            lease: Duration::from_millis(lease_ms),
            allowed_callers: vec![ALICE.to_string(), BOB.to_string()],
        },
    )
    .unwrap()
}

fn pending_commit_record(tx: &str) -> HandoffRecord {
    let intent = intent();
    HandoffRecord {
        tx: tx.to_string(),
        caller: ALICE.to_string(),
        fingerprint: Some(intent.fingerprint()),
        intent: Some(intent),
        state: HandoffState::Committed,
        created_at_ms: T0,
        expires_at_ms: Some(T0 + 60_000),
        transition_at_ms: T0,
        disposition: BrowserDisposition::CancelledForHandoff,
        engine_task_id: None,
        association_pending: true,
        outcome: Some("commitment recorded".to_string()),
    }
}

#[test]
fn schema_version_stays_two() {
    assert_eq!(SCHEMA_VERSION, 2);
}

#[test]
fn prepare_persists_and_replay_returns_original_deadline() {
    let clock = Arc::new(ManualClock::new());
    let c = coordinator(Arc::new(FakeEngine::default()), clock.clone(), 60_000);
    let first = c.prepare(ALICE, "tx-1", intent()).unwrap();
    assert_eq!(first.status, HandoffStatus::Prepared);
    assert_eq!(first.expires_at_ms, Some(T0 + 60_000));

    clock.advance(10_000);
    let replay = c.prepare(ALICE, "tx-1", intent()).unwrap();
    assert_eq!(replay.status, HandoffStatus::Prepared);
    // The original deadline, never renewed by a retry (freeze §3).
    assert_eq!(replay.expires_at_ms, Some(T0 + 60_000));
}

#[test]
fn prepare_conflicts_on_changed_intent_even_after_termination() {
    let clock = Arc::new(ManualClock::new());
    let c = coordinator(Arc::new(FakeEngine::default()), clock, 60_000);
    c.prepare(ALICE, "tx-2", intent()).unwrap();
    let mut changed = intent();
    changed.size_hint = Some(1);
    let reply = c.prepare(ALICE, "tx-2", changed.clone()).unwrap();
    assert_eq!(reply.status, HandoffStatus::Conflict);

    c.abort(ALICE, "tx-2", true).unwrap();
    let reply = c.prepare(ALICE, "tx-2", changed).unwrap();
    assert_eq!(reply.status, HandoffStatus::Conflict);
    // The identical intent after termination reports the terminal state.
    let reply = c.prepare(ALICE, "tx-2", intent()).unwrap();
    assert_eq!(reply.status, HandoffStatus::Aborted);
}

#[test]
fn caller_namespace_is_isolated() {
    let clock = Arc::new(ManualClock::new());
    let c = coordinator(Arc::new(FakeEngine::default()), clock, 60_000);
    c.prepare(ALICE, "tx-3", intent()).unwrap();
    // For another caller the transaction does not exist (freeze §3).
    assert_eq!(c.query(BOB, "tx-3").unwrap().status, HandoffStatus::NotFound);
    assert_eq!(
        c.commit(BOB, "tx-3", true).unwrap().status,
        HandoffStatus::NotFound
    );
    // An unapproved caller is refused outright.
    assert!(c.query("chrome:stranger", "tx-3").is_err());
}

#[test]
fn commit_happy_path_adds_exactly_once() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    let c = coordinator(engine.clone(), clock, 60_000);
    c.prepare(ALICE, "tx-4", intent()).unwrap();
    let reply = c.commit(ALICE, "tx-4", true).unwrap();
    assert_eq!(reply.status, HandoffStatus::Committed);
    assert_eq!(reply.task_ref.as_deref(), Some("1001"));
    assert_eq!(engine.add_calls(), 1);

    let dup = c.commit(ALICE, "tx-4", true).unwrap();
    assert_eq!(dup.status, HandoffStatus::Committed);
    assert_eq!(dup.task_ref.as_deref(), Some("1001"));
    assert_eq!(engine.add_calls(), 1, "duplicate commit must not re-Add");
}

#[test]
fn commit_requires_the_cancellation_confirmation() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    let c = coordinator(engine.clone(), clock, 60_000);
    c.prepare(ALICE, "tx-5", intent()).unwrap();
    let reply = c.commit(ALICE, "tx-5", false).unwrap();
    assert_eq!(reply.status, HandoffStatus::Conflict);
    assert_eq!(engine.add_calls(), 0);
    assert_eq!(c.query(ALICE, "tx-5").unwrap().status, HandoffStatus::Prepared);
    // The real confirmation still lands afterwards.
    assert_eq!(
        c.commit(ALICE, "tx-5", true).unwrap().status,
        HandoffStatus::Committed
    );
}

#[test]
fn commit_unknown_is_not_found_and_aborted_conflicts() {
    let clock = Arc::new(ManualClock::new());
    let c = coordinator(Arc::new(FakeEngine::default()), clock, 60_000);
    assert_eq!(
        c.commit(ALICE, "nope", true).unwrap().status,
        HandoffStatus::NotFound
    );
    c.prepare(ALICE, "tx-6", intent()).unwrap();
    c.abort(ALICE, "tx-6", true).unwrap();
    assert_eq!(
        c.commit(ALICE, "tx-6", true).unwrap().status,
        HandoffStatus::Conflict
    );
}

#[test]
fn expired_commit_is_recovery_required_and_never_adds() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    let c = coordinator(engine.clone(), clock.clone(), 1_000);
    c.prepare(ALICE, "tx-7", intent()).unwrap();
    clock.advance(1_001);
    let reply = c.commit(ALICE, "tx-7", true).unwrap();
    assert_eq!(reply.status, HandoffStatus::RecoveryRequired);
    assert_eq!(reply.state, Some(HandoffState::Expired));
    assert_eq!(engine.add_calls(), 0, "an expired lease must not auto-start");
    assert_eq!(c.query(ALICE, "tx-7").unwrap().status, HandoffStatus::Expired);
}

#[test]
fn crash_gap_with_marker_present_heals_without_second_add() {
    // The engine Add landed durably before the crash; only the
    // journal's association write was lost (freeze §5 crash table).
    let engine = Arc::new(FakeEngine::with_task("tx-8", 4242));
    let clock = Arc::new(ManualClock::new());
    let c = Coordinator::open(
        Box::new(MemoryStore::with_records(vec![pending_commit_record("tx-8")])),
        engine.clone(),
        clock,
        CoordinatorConfig {
            lease: Duration::from_millis(60_000),
            allowed_callers: vec![ALICE.to_string()],
        },
    )
    .unwrap();
    let reply = c.query(ALICE, "tx-8").unwrap();
    assert_eq!(reply.status, HandoffStatus::Committed);
    assert_eq!(reply.task_ref.as_deref(), Some("4242"));
    assert_eq!(engine.add_calls(), 0, "marker found: recovery must not re-Add");
}

#[test]
fn crash_gap_without_marker_adds_exactly_once() {
    let engine = Arc::new(FakeEngine::default());
    let clock = Arc::new(ManualClock::new());
    let c = Coordinator::open(
        Box::new(MemoryStore::with_records(vec![pending_commit_record("tx-9")])),
        engine.clone(),
        clock,
        CoordinatorConfig {
            lease: Duration::from_millis(60_000),
            allowed_callers: vec![ALICE.to_string()],
        },
    )
    .unwrap();
    let reply = c.query(ALICE, "tx-9").unwrap();
    assert_eq!(reply.status, HandoffStatus::Committed);
    assert_eq!(engine.add_calls(), 1);
    let dup = c.commit(ALICE, "tx-9", true).unwrap();
    assert_eq!(dup.task_ref, reply.task_ref);
    assert_eq!(engine.add_calls(), 1);
}

#[test]
fn commit_while_engine_down_stays_pending_then_heals() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    let c = coordinator(engine.clone(), clock, 60_000);
    c.prepare(ALICE, "tx-10", intent()).unwrap();
    engine.set_down(true);
    let reply = c.commit(ALICE, "tx-10", true).unwrap();
    assert_eq!(reply.status, HandoffStatus::RecoveryRequired);
    engine.set_down(false);
    let reply = c.commit(ALICE, "tx-10", true).unwrap();
    assert_eq!(reply.status, HandoffStatus::Committed);
    assert_eq!(engine.add_calls(), 1);
    assert_eq!(engine.task_count(), 1);
}

#[test]
fn abort_tombstone_blocks_a_delayed_prepare() {
    let clock = Arc::new(ManualClock::new());
    let c = coordinator(Arc::new(FakeEngine::default()), clock, 60_000);
    let reply = c.abort(ALICE, "tx-11", false).unwrap();
    assert_eq!(reply.status, HandoffStatus::Aborted);
    let reply = c.prepare(ALICE, "tx-11", intent()).unwrap();
    assert_eq!(reply.status, HandoffStatus::Conflict);
}

#[test]
fn abort_after_commit_leaves_the_task_alone() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    let c = coordinator(engine.clone(), clock, 60_000);
    c.prepare(ALICE, "tx-12", intent()).unwrap();
    c.commit(ALICE, "tx-12", true).unwrap();
    let reply = c.abort(ALICE, "tx-12", true).unwrap();
    assert_eq!(reply.status, HandoffStatus::Committed);
    assert_eq!(engine.task_count(), 1);
}

#[test]
fn abort_prepared_requires_the_retains_confirmation() {
    let clock = Arc::new(ManualClock::new());
    let c = coordinator(Arc::new(FakeEngine::default()), clock, 60_000);
    c.prepare(ALICE, "tx-13", intent()).unwrap();
    // Without the confirmation the recovery intent is retained.
    let reply = c.abort(ALICE, "tx-13", false).unwrap();
    assert_eq!(reply.status, HandoffStatus::Prepared);
    let reply = c.abort(ALICE, "tx-13", true).unwrap();
    assert_eq!(reply.status, HandoffStatus::Aborted);
}

#[test]
fn duplicate_commit_after_task_removal_never_recreates() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    let c = coordinator(engine.clone(), clock, 60_000);
    c.prepare(ALICE, "tx-14", intent()).unwrap();
    let first = c.commit(ALICE, "tx-14", true).unwrap();
    // The user removes the engine task; the tombstone still answers.
    engine.tasks.lock().unwrap().clear();
    let replay = c.commit(ALICE, "tx-14", true).unwrap();
    assert_eq!(replay.status, HandoffStatus::Committed);
    assert_eq!(replay.task_ref, first.task_ref);
    assert_eq!(engine.add_calls(), 1, "removal must not resurrect the task");
}

#[test]
fn journal_file_round_trips_records_and_tombstones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("handoff").join("records.json");
    let clock = Arc::new(ManualClock::new());
    {
        let c = Coordinator::open(
            Box::new(FileStore::new(path.clone())),
            Arc::new(FakeEngine::default()),
            clock.clone(),
            CoordinatorConfig {
                lease: Duration::from_millis(60_000),
                allowed_callers: vec![ALICE.to_string()],
            },
        )
        .unwrap();
        c.prepare(ALICE, "tx-15", intent()).unwrap();
        c.abort(ALICE, "tx-16", true).unwrap();
    }
    let c = Coordinator::open(
        Box::new(FileStore::new(path)),
        Arc::new(FakeEngine::default()),
        clock,
        CoordinatorConfig {
            lease: Duration::from_millis(60_000),
            allowed_callers: vec![ALICE.to_string()],
        },
    )
    .unwrap();
    assert_eq!(
        c.query(ALICE, "tx-15").unwrap().status,
        HandoffStatus::Prepared
    );
    // The intent-less tombstone survived the round trip.
    assert_eq!(
        c.prepare(ALICE, "tx-16", intent()).unwrap().status,
        HandoffStatus::Conflict
    );
}

#[test]
fn fingerprint_is_stable_and_sensitive_to_every_field() {
    let base = intent().fingerprint();
    assert_eq!(base, intent().fingerprint());
    let mut variants = Vec::new();
    let mut v = intent();
    v.url = "https://example.com/other.bin".into();
    variants.push(v);
    let mut v = intent();
    v.filename_hint = None;
    variants.push(v);
    let mut v = intent();
    v.size_hint = None;
    variants.push(v);
    let mut v = intent();
    v.browser_download_id = "browser-dl-2".into();
    variants.push(v);
    let mut v = intent();
    v.dest_dir = Some("D:\\elsewhere".into());
    variants.push(v);
    for variant in variants {
        assert_ne!(variant.fingerprint(), base);
    }
    // Method case is normalized before fingerprinting.
    let mut lower = intent();
    lower.method = "get".into();
    lower.validate_and_normalize().unwrap();
    assert_eq!(lower.fingerprint(), base);
}

#[test]
fn legacy_task_record_json_has_no_marker() {
    let record = udm_core::store::TaskRecord::new(
        7,
        "https://example.com/x.bin",
        Path::new("/tmp"),
        None,
    );
    let mut value = serde_json::to_value(&record).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("handoff_tx")
        .expect("serialized record carries the marker field");
    let back: udm_core::store::TaskRecord = serde_json::from_value(value).unwrap();
    assert_eq!(back.handoff_tx, None);
}

#[test]
fn engine_marker_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    let paths = Paths {
        root: root.into(),
        db_file: root.join("test.redb"),
        log_file: root.join("logs/test.log"),
        temp_dir: root.join("tmp"),
        default_download_dir: root.join("downloads"),
        portable: true,
        exe_dir: Some(root.into()),
    };
    let settings = Settings {
        use_system_proxy: false,
        verify_checksums: false,
        default_download_dir: root.join("downloads").to_string_lossy().into(),
        ..Settings::default()
    };
    let engine = Arc::new(
        EngineBuilder::new()
            .paths(paths)
            .settings(settings)
            .build()
            .unwrap(),
    );
    let clock: Arc<dyn HandoffClock> = Arc::new(udm_core::handoff::SystemClock);
    let c = Coordinator::open(
        Box::new(MemoryStore::new()),
        Arc::new(EngineAdapter::new(engine.clone())),
        clock,
        CoordinatorConfig {
            lease: Duration::from_millis(60_000),
            allowed_callers: vec![ALICE.to_string()],
        },
    )
    .unwrap();
    let mut intent = intent();
    // Loopback discard port: the task row is what matters here; its
    // probe fails fast and harmlessly.
    intent.url = "http://127.0.0.1:9/handoff.bin".to_string();
    c.prepare(ALICE, "tx-real", intent).unwrap();
    let reply = c.commit(ALICE, "tx-real", true).unwrap();
    assert_eq!(reply.status, HandoffStatus::Committed);
    let task_ref = reply.task_ref.expect("associated task");
    let id: u64 = task_ref.parse().unwrap();
    assert!(engine.task(id).is_some());
    assert_eq!(engine.task_by_handoff("tx-real"), Some(id));
    let dup = c.commit(ALICE, "tx-real", true).unwrap();
    assert_eq!(dup.task_ref.as_deref(), Some(task_ref.as_str()));
}

#[test]
fn concurrent_duplicate_commits_add_exactly_once() {
    // Shen condition (2026-11-09): at-most-one-task must hold not only for
    // sequential crash recovery but also against concurrent materialization.
    // The coordinator serializes every operation under its single lock, so
    // two racing commits for the same transaction Add exactly once and both
    // observe the same recorded association.
    let engine = Arc::new(FakeEngine::default());
    let clock = Arc::new(ManualClock::new());
    let coord = Arc::new(coordinator(engine.clone(), clock, 60_000));
    coord.prepare(ALICE, "tx-race", intent()).unwrap();
    let mut handles = Vec::new();
    for _ in 0..2 {
        let c = coord.clone();
        handles.push(std::thread::spawn(move || {
            c.commit(ALICE, "tx-race", true).map(|r| r.task_ref)
        }));
    }
    let mut refs = Vec::new();
    for h in handles {
        refs.push(h.join().expect("commit thread panicked").expect("commit ok"));
    }
    assert_eq!(
        engine.add_calls(),
        1,
        "concurrent commits must Add exactly once"
    );
    assert_eq!(refs[0], refs[1], "both callers see the recorded association");
    assert_eq!(refs[0].as_deref(), Some("1001"));
}

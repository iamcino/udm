//! Stage 1 gates for the 0.3.0 handoff dialog: discard, decision
//! binding, cancellation attestation, recorded Add failures, and
//! discovery. The engine is a fake except in the adapter test, which
//! checks that a bound decision becomes the task's directory, name,
//! and paused state.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use usagi::error::{Error, Result};
use usagi::handoff::{
    BrowserDisposition, Coordinator, CoordinatorConfig, DiscoveryEntry, EngineAdapter, EnginePort,
    FileStore, HandoffClock, HandoffIntent, HandoffRecord, HandoffState, HandoffStatus,
    HandoffStore, MemoryStore, UserDecision,
};
use usagi::{EngineBuilder, Paths, Settings, TaskState};

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
        dest_dir: Some("C:/from-browser".to_string()),
    }
}

fn decision(dir: &str, name: &str, paused: bool) -> UserDecision {
    UserDecision {
        dest_dir: dir.to_string(),
        filename: name.to_string(),
        start_paused: paused,
    }
}

#[derive(Default)]
struct FakeEngine {
    tasks: Mutex<Vec<(String, u64)>>,
    next_id: Mutex<u64>,
    add_calls: Mutex<u32>,
    down: Mutex<bool>,
    last_decision: Mutex<Option<UserDecision>>,
}

impl FakeEngine {
    fn add_calls(&self) -> u32 {
        *self.add_calls.lock().unwrap()
    }

    fn task_count(&self) -> usize {
        self.tasks.lock().unwrap().len()
    }

    fn set_down(&self, down: bool) {
        *self.down.lock().unwrap() = down;
    }

    fn last_decision(&self) -> Option<UserDecision> {
        self.last_decision.lock().unwrap().clone()
    }
}

impl EnginePort for FakeEngine {
    fn add_handoff_task(
        &self,
        _intent: &HandoffIntent,
        tx: &str,
        decision: Option<&UserDecision>,
    ) -> Result<u64> {
        if *self.down.lock().unwrap() {
            return Err(Error::Other("fake engine down".into()));
        }
        *self.last_decision.lock().unwrap() = decision.cloned();
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
        ManualClock {
            now: Mutex::new(T0),
        }
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

fn open(
    store: Box<dyn HandoffStore>,
    engine: Arc<dyn EnginePort>,
    clock: Arc<dyn HandoffClock>,
    require_decision: bool,
) -> Coordinator {
    Coordinator::open(
        store,
        engine,
        clock,
        CoordinatorConfig {
            lease: Duration::from_millis(60_000),
            allowed_callers: vec![ALICE.to_string(), BOB.to_string()],
            require_decision,
        },
    )
    .unwrap()
}

fn coordinator(
    engine: Arc<FakeEngine>,
    clock: Arc<ManualClock>,
    require_decision: bool,
) -> Coordinator {
    open(
        Box::new(MemoryStore::new()),
        engine,
        clock,
        require_decision,
    )
}

fn record(
    tx: &str,
    caller: &str,
    state: HandoffState,
    created_at_ms: i64,
    expires_at_ms: Option<i64>,
) -> HandoffRecord {
    let intent = intent();
    HandoffRecord {
        tx: tx.to_string(),
        caller: caller.to_string(),
        intent: Some(intent.clone()),
        fingerprint: Some(intent.fingerprint()),
        state,
        created_at_ms,
        expires_at_ms,
        transition_at_ms: created_at_ms,
        disposition: BrowserDisposition::Unresolved,
        engine_task_id: None,
        association_pending: false,
        outcome: None,
        decision: None,
        materialize_error: None,
    }
}

#[test]
fn discard_from_prepared_replays_and_conflicts_on_commit() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    let c = coordinator(engine.clone(), clock.clone(), false);

    assert_eq!(
        c.discard(ALICE, "missing").unwrap().status,
        HandoffStatus::NotFound
    );

    c.prepare(ALICE, "tx-drop", intent(), false).unwrap();
    let first = c.discard(ALICE, "tx-drop").unwrap();
    assert_eq!(first.status, HandoffStatus::Discarded);
    assert_eq!(first.state, Some(HandoffState::Discarded));
    let replay = c.discard(ALICE, "tx-drop").unwrap();
    assert_eq!(replay, first);
    let commit = c.commit(ALICE, "tx-drop", true, None).unwrap();
    assert_eq!(commit.status, HandoffStatus::Conflict);
    assert_eq!(commit.state, Some(HandoffState::Discarded));
    assert_eq!(
        c.abort(ALICE, "tx-drop", true).unwrap().status,
        HandoffStatus::Discarded
    );
    assert_eq!(
        c.query(ALICE, "tx-drop").unwrap().status,
        HandoffStatus::Discarded
    );
    assert_eq!(engine.task_count(), 0);

    c.prepare(ALICE, "tx-keep", intent(), false).unwrap();
    c.commit(ALICE, "tx-keep", true, None).unwrap();
    let denied = c.discard(ALICE, "tx-keep").unwrap();
    assert_eq!(denied.status, HandoffStatus::Conflict);
    assert_eq!(denied.state, Some(HandoffState::Committed));
    assert_eq!(engine.task_count(), 1);
    assert_eq!(
        c.query(ALICE, "tx-keep").unwrap().status,
        HandoffStatus::Committed
    );

    c.prepare(ALICE, "tx-old", intent(), false).unwrap();
    clock.advance(60_000);
    let expired = c.discard(ALICE, "tx-old").unwrap();
    assert_eq!(expired.status, HandoffStatus::Expired);
    assert_eq!(expired.state, Some(HandoffState::Expired));

    c.abort(ALICE, "tx-abort", true).unwrap();
    assert_eq!(
        c.discard(ALICE, "tx-abort").unwrap().status,
        HandoffStatus::Aborted
    );
}

#[test]
fn decision_binds_on_commit_and_survives_a_journal_reload() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("handoff").join("records.json");
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    let chosen = decision("D:/inbox", "file:name.bin", true);
    let task_ref = {
        let c = open(
            Box::new(FileStore::new(path.clone())),
            engine.clone(),
            clock.clone(),
            true,
        );
        c.prepare(ALICE, "tx-bind", intent(), true).unwrap();
        let reply = c
            .commit(ALICE, "tx-bind", true, Some(chosen.clone()))
            .unwrap();
        assert_eq!(reply.status, HandoffStatus::Committed);
        reply.task_ref.unwrap()
    };
    let journal = std::fs::read_to_string(&path).unwrap();
    assert!(journal.contains("file_name.bin"), "{journal}");
    assert!(journal.contains("D:/inbox"), "{journal}");
    assert!(!journal.contains("file:name.bin"), "{journal}");

    let c = open(Box::new(FileStore::new(path)), engine.clone(), clock, true);
    let loaded = c.query(ALICE, "tx-bind").unwrap();
    assert_eq!(loaded.status, HandoffStatus::Committed);
    assert_eq!(loaded.task_ref.as_deref(), Some(task_ref.as_str()));
    let other = decision("D:/elsewhere", "file_name.bin", true);
    let conflict = c.commit(ALICE, "tx-bind", true, Some(other)).unwrap();
    assert_eq!(conflict.status, HandoffStatus::Conflict);
    assert_eq!(engine.task_count(), 1);
    let same = c.commit(ALICE, "tx-bind", true, Some(chosen)).unwrap();
    assert_eq!(same.status, HandoffStatus::Committed);
    assert_eq!(same.task_ref.as_deref(), Some(task_ref.as_str()));
    let absent = c.commit(ALICE, "tx-bind", true, None).unwrap();
    assert_eq!(absent.status, HandoffStatus::Committed);
    assert_eq!(engine.add_calls(), 1);
    assert_eq!(engine.last_decision().unwrap().filename, "file_name.bin");
    assert!(engine.last_decision().unwrap().start_paused);
}

#[test]
fn differing_decision_replay_does_not_mutate_while_pending() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    engine.set_down(true);
    let c = coordinator(engine.clone(), clock, false);
    c.prepare(ALICE, "tx-pend", intent(), false).unwrap();
    let bound = decision("D:/inbox", "clip.bin", false);
    let pending = c
        .commit(ALICE, "tx-pend", true, Some(bound.clone()))
        .unwrap();
    assert_eq!(pending.status, HandoffStatus::RecoveryRequired);
    assert_eq!(engine.task_count(), 0);

    let conflict = c
        .commit(
            ALICE,
            "tx-pend",
            true,
            Some(decision("D:/other", "clip.bin", false)),
        )
        .unwrap();
    assert_eq!(conflict.status, HandoffStatus::Conflict);
    assert_eq!(engine.task_count(), 0);
    assert_eq!(engine.add_calls(), 0);

    engine.set_down(false);
    let healed = c.commit(ALICE, "tx-pend", true, Some(bound)).unwrap();
    assert_eq!(healed.status, HandoffStatus::Committed);
    assert_eq!(engine.task_count(), 1);
    assert_eq!(engine.add_calls(), 1);
    assert_eq!(engine.last_decision().unwrap().dest_dir, "D:/inbox");
}

#[test]
fn require_decision_rejects_a_bare_commit_and_accepts_a_decided_one() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    let c = coordinator(engine.clone(), clock, true);
    c.prepare(ALICE, "tx-gate", intent(), true).unwrap();
    let bare = c.commit(ALICE, "tx-gate", true, None).unwrap();
    assert_eq!(bare.status, HandoffStatus::Conflict);
    assert!(bare.detail.unwrap().contains("user decision"));
    assert_eq!(
        c.query(ALICE, "tx-gate").unwrap().status,
        HandoffStatus::Prepared
    );
    assert_eq!(engine.task_count(), 0);

    let empty = c
        .commit(
            ALICE,
            "tx-gate",
            true,
            Some(decision("   ", "clip.bin", false)),
        )
        .unwrap();
    assert_eq!(empty.status, HandoffStatus::Conflict);
    assert_eq!(
        c.query(ALICE, "tx-gate").unwrap().status,
        HandoffStatus::Prepared
    );

    let accepted = c
        .commit(
            ALICE,
            "tx-gate",
            true,
            Some(decision("D:/inbox", "clip.bin", false)),
        )
        .unwrap();
    assert_eq!(accepted.status, HandoffStatus::Committed);
    assert_eq!(engine.task_count(), 1);
}

#[test]
fn require_decision_requires_prepare_attestation_and_legacy_does_not() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    let c = coordinator(engine.clone(), clock, true);
    c.prepare(ALICE, "tx-proof", intent(), false).unwrap();
    let refused = c
        .commit(
            ALICE,
            "tx-proof",
            true,
            Some(decision("D:/inbox", "clip.bin", false)),
        )
        .unwrap();
    assert_eq!(refused.status, HandoffStatus::Conflict);
    assert!(refused.detail.unwrap().contains("attestation"));
    assert_eq!(
        c.query(ALICE, "tx-proof").unwrap().status,
        HandoffStatus::Prepared
    );
    assert_eq!(engine.task_count(), 0);

    c.prepare(ALICE, "tx-proof", intent(), true).unwrap();
    let accepted = c
        .commit(
            ALICE,
            "tx-proof",
            true,
            Some(decision("D:/inbox", "clip.bin", true)),
        )
        .unwrap();
    assert_eq!(accepted.status, HandoffStatus::Committed);

    let legacy_clock = Arc::new(ManualClock::new());
    let legacy_engine = Arc::new(FakeEngine::default());
    let legacy = coordinator(legacy_engine.clone(), legacy_clock, false);
    legacy.prepare(ALICE, "tx-old", intent(), false).unwrap();
    let reply = legacy.commit(ALICE, "tx-old", true, None).unwrap();
    assert_eq!(reply.status, HandoffStatus::Committed);
    assert!(legacy_engine.last_decision().is_none());
    let restated = legacy
        .commit(
            ALICE,
            "tx-old",
            true,
            Some(decision("D:/inbox", "clip.bin", false)),
        )
        .unwrap();
    assert_eq!(restated.status, HandoffStatus::Conflict);
    assert_eq!(legacy_engine.task_count(), 1);
}

#[test]
fn legacy_journal_without_new_fields_commits_the_old_way() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("records.json");
    let body = serde_json::json!([{
        "tx": "tx-legacy",
        "caller": ALICE,
        "intent": {
            "url": "https://example.com/file.bin",
            "method": "GET",
            "filename_hint": "file.bin",
            "size_hint": 65536,
            "browser_download_id": "browser-dl-1",
            "dest_dir": null
        },
        "fingerprint": "not-consulted-by-commit",
        "state": "prepared",
        "created_at_ms": T0,
        "expires_at_ms": T0 + 60_000,
        "transition_at_ms": T0,
        "disposition": "unresolved",
        "engine_task_id": null,
        "association_pending": false,
        "outcome": null
    }]);
    std::fs::write(&path, serde_json::to_vec(&body).unwrap()).unwrap();

    let engine = Arc::new(FakeEngine::default());
    let c = open(
        Box::new(FileStore::new(path)),
        engine.clone(),
        Arc::new(ManualClock::new()),
        false,
    );
    let reply = c.commit(ALICE, "tx-legacy", true, None).unwrap();
    assert_eq!(reply.status, HandoffStatus::Committed);
    assert!(engine.last_decision().is_none());
    assert_eq!(engine.task_count(), 1);
}

#[test]
fn materialize_error_is_reported_and_cleared_by_retry() {
    let clock = Arc::new(ManualClock::new());
    let engine = Arc::new(FakeEngine::default());
    engine.set_down(true);
    let c = coordinator(engine.clone(), clock, false);
    c.prepare(ALICE, "tx-err", intent(), false).unwrap();
    let failed = c.commit(ALICE, "tx-err", true, None).unwrap();
    assert_eq!(failed.status, HandoffStatus::RecoveryRequired);
    assert!(failed.detail.unwrap().contains("fake engine down"));
    let queried = c.query(ALICE, "tx-err").unwrap();
    assert_eq!(queried.status, HandoffStatus::RecoveryRequired);
    assert!(queried.detail.unwrap().contains("fake engine down"));

    engine.set_down(false);
    let retried = c.retry_materialize(ALICE, "tx-err").unwrap();
    assert_eq!(retried.status, HandoffStatus::Committed);
    assert!(retried.task_ref.is_some());
    assert!(retried.detail.is_none());
    let queried = c.query(ALICE, "tx-err").unwrap();
    assert_eq!(queried.status, HandoffStatus::Committed);
    assert!(queried.detail.is_none());
    assert_eq!(engine.add_calls(), 1);

    let again = c.retry_materialize(ALICE, "tx-err").unwrap();
    assert_eq!(again.status, HandoffStatus::Committed);
    assert_eq!(engine.add_calls(), 1);
}

#[test]
fn discovery_lists_live_and_newly_expired_in_order() {
    let clock = Arc::new(ManualClock::new());
    let records = vec![
        record("pre", ALICE, HandoffState::Expired, T0 - 100, Some(T0 - 50)),
        record("due", ALICE, HandoffState::Prepared, T0 - 30, Some(T0 - 1)),
        record(
            "live-a",
            ALICE,
            HandoffState::Prepared,
            T0 - 20,
            Some(T0 + 60_000),
        ),
        record(
            "live-b",
            ALICE,
            HandoffState::Prepared,
            T0 - 10,
            Some(T0 + 60_000),
        ),
        record(
            "bob-live",
            BOB,
            HandoffState::Prepared,
            T0 - 25,
            Some(T0 + 60_000),
        ),
    ];
    let c = open(
        Box::new(MemoryStore::with_records(records)),
        Arc::new(FakeEngine::default()),
        clock.clone(),
        false,
    );

    let alice = c.discovery(ALICE).unwrap();
    assert_eq!(
        alice
            .iter()
            .map(|entry| (entry.tx.as_str(), entry.expired, entry.created_at_ms))
            .collect::<Vec<_>>(),
        vec![
            ("due", true, T0 - 30),
            ("live-a", false, T0 - 20),
            ("live-b", false, T0 - 10),
        ]
    );
    assert!(alice.iter().all(|entry| entry.caller == ALICE));
    assert!(alice.iter().all(|entry| entry.intent.is_some()));
    assert_eq!(alice[1].expires_at_ms, Some(T0 + 60_000));
    let bob = c.discovery(BOB).unwrap();
    assert_eq!(
        bob.iter()
            .map(|entry| (entry.tx.as_str(), entry.expired))
            .collect::<Vec<_>>(),
        vec![("bob-live", false)]
    );

    clock.advance(60_000);
    let alice = c.discovery(ALICE).unwrap();
    assert_eq!(
        alice
            .iter()
            .map(|entry| (entry.tx.as_str(), entry.expired))
            .collect::<Vec<_>>(),
        vec![("due", true), ("live-a", true), ("live-b", true)]
    );
    assert_eq!(alice[1].expires_at_ms, Some(T0 + 60_000));
    assert!(alice.iter().all(|entry| entry.tx != "pre"));
    let bob = c.discovery(BOB).unwrap();
    assert_eq!(bob.len(), 1);
    assert!(bob[0].expired);
    assert!(c.discovery("chrome:stranger").is_err());
}

#[test]
fn engine_adapter_applies_the_bound_decision() {
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
    let c = open(
        Box::new(MemoryStore::new()),
        Arc::new(EngineAdapter::new(engine.clone())),
        Arc::new(ManualClock::new()),
        false,
    );
    let mut prepared = intent();
    prepared.url = "http://127.0.0.1:9/handoff.bin".to_string();
    c.prepare(ALICE, "tx-adapt", prepared, true).unwrap();
    let dest = root.join("chosen");
    let reply = c
        .commit(
            ALICE,
            "tx-adapt",
            true,
            Some(decision(&dest.to_string_lossy(), "file:name.bin", true)),
        )
        .unwrap();
    assert_eq!(reply.status, HandoffStatus::Committed);
    let id: u64 = reply.task_ref.unwrap().parse().unwrap();
    let snap = engine.task(id).expect("task created from the decision");
    assert_eq!(snap.filename, "file_name.bin");
    assert_eq!(snap.state, TaskState::Paused);
    assert!(
        snap.dest_dir.replace('\\', "/").contains("chosen"),
        "dest was {}",
        snap.dest_dir
    );
    assert_eq!(engine.task_by_handoff("tx-adapt"), Some(id));
}

#[test]
fn discovery_entry_order_key_is_stable_for_equal_timestamps() {
    // Same preparation time: caller, then tx, breaks the tie.
    let records = vec![
        record("b", BOB, HandoffState::Prepared, T0, Some(T0 + 60_000)),
        record("a", ALICE, HandoffState::Prepared, T0, Some(T0 + 60_000)),
        record("c", ALICE, HandoffState::Prepared, T0, Some(T0 + 60_000)),
    ];
    let c = open(
        Box::new(MemoryStore::with_records(records)),
        Arc::new(FakeEngine::default()),
        Arc::new(ManualClock::new()),
        false,
    );
    let alice = c.discovery(ALICE).unwrap();
    assert_eq!(
        alice
            .iter()
            .map(|entry| entry.tx.as_str())
            .collect::<Vec<_>>(),
        vec!["a", "c"]
    );
    let _ordered: Vec<&DiscoveryEntry> = alice.iter().collect();
    assert!(_ordered.windows(2).all(|pair| {
        (
            pair[0].created_at_ms,
            pair[0].caller.as_str(),
            pair[0].tx.as_str(),
        ) <= (
            pair[1].created_at_ms,
            pair[1].caller.as_str(),
            pair[1].tx.as_str(),
        )
    }));
}

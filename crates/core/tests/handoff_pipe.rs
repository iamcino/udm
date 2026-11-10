//! G5 slice 2 transport gates (Windows only): the named pipe must pair,
//! dispatch the four frozen operations, and keep caller namespaces
//! isolated - end to end over the real pipe, not in-process calls.
#![cfg(windows)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use usagi::error::Result;
use usagi::handoff::{
    Coordinator, CoordinatorConfig, EnginePort, HandoffClock, HandoffIntent, MemoryStore,
};
use usagi::handoff_pipe::{pipe_name, serve, PipeClient};

const ALICE: &str = "chrome:alice-ext";
const BOB: &str = "firefox:bob-ext";
const SECRET: &str = "test-pairing-secret";

#[derive(Default)]
struct FakeEngine {
    tasks: Mutex<Vec<(String, u64)>>,
    adds: Mutex<u32>,
}

impl EnginePort for FakeEngine {
    fn add_handoff_task(&self, _intent: &HandoffIntent, tx: &str) -> Result<u64> {
        let mut adds = self.adds.lock().unwrap();
        *adds += 1;
        let id = 1000 + u64::from(*adds);
        self.tasks.lock().unwrap().push((tx.to_string(), id));
        Ok(id)
    }

    fn find_handoff_task(&self, tx: &str) -> Result<Option<u64>> {
        Ok(self
            .tasks
            .lock()
            .unwrap()
            .iter()
            .find(|(t, _)| t == tx)
            .map(|(_, id)| *id))
    }
}

struct FixedClock;
impl HandoffClock for FixedClock {
    fn now_ms(&self) -> i64 {
        1_700_000_000_000
    }
}

fn intent() -> Value {
    json!({
        "url": "https://example.com/file.bin",
        "method": "GET",
        "filename_hint": "file.bin",
        "size_hint": 65536,
        "browser_download_id": "browser-dl-1",
        "dest_dir": null,
    })
}

/// Connect once the server thread has created the first pipe instance.
/// (The production client deliberately fails fast on a missing pipe -
/// that is how the host produces `unavailable` - so the wait lives here.)
fn connect_when_ready(name: &str) -> PipeClient {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match PipeClient::connect(name) {
            Ok(c) => return c,
            Err(e) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "pipe never appeared: {e}"
                );
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[test]
fn pipe_pairs_dispatches_and_isolates_callers() {
    let engine = Arc::new(FakeEngine::default());
    let coordinator = Arc::new(
        Coordinator::open(
            Box::new(MemoryStore::new()),
            engine.clone(),
            Arc::new(FixedClock),
            CoordinatorConfig {
                lease: Duration::from_secs(60),
                allowed_callers: vec![ALICE.to_string(), BOB.to_string()],
            },
        )
        .unwrap(),
    );
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let stop = stop.clone();
        let coordinator = coordinator.clone();
        std::thread::spawn(move || serve(stop, coordinator, SECRET.to_string()))
    };
    let name = pipe_name().unwrap();

    // A caller with the wrong secret is refused at pairing.
    let mut bad = connect_when_ready(&name);
    assert_eq!(bad.pair("wrong-secret", ALICE).unwrap(), false);
    drop(bad);

    // A caller not on the allow-list is refused even with the secret.
    let mut stranger = connect_when_ready(&name);
    assert_eq!(stranger.pair(SECRET, "chrome:mallory-ext").unwrap(), false);
    drop(stranger);

    // Alice: prepare -> commit -> query over the wire.
    let mut alice = connect_when_ready(&name);
    assert_eq!(alice.pair(SECRET, ALICE).unwrap(), true);
    let prepared = alice
        .request(&json!({"v": 1, "op": "prepare", "tx": "tx-pipe", "intent": intent()}))
        .unwrap();
    assert_eq!(prepared["status"], "prepared");
    assert!(prepared["expires_at"].is_u64());
    let committed = alice
        .request(&json!({"v": 1, "op": "commit", "tx": "tx-pipe",
                          "confirm": {"browser_cancelled": true}}))
        .unwrap();
    assert_eq!(committed["status"], "committed");
    assert_eq!(committed["task_ref"], "1001");
    let queried = alice
        .request(&json!({"v": 1, "op": "query", "tx": "tx-pipe"}))
        .unwrap();
    assert_eq!(queried["status"], "committed");
    assert_eq!(queried["task_ref"], "1001");
    drop(alice);

    // Bob cannot see Alice's transaction: the namespace is (caller, tx).
    let mut bob = connect_when_ready(&name);
    assert_eq!(bob.pair(SECRET, BOB).unwrap(), true);
    let blind = bob
        .request(&json!({"v": 1, "op": "query", "tx": "tx-pipe"}))
        .unwrap();
    assert_eq!(blind["status"], "not-found");
    drop(bob);

    // A bad wire version is rejected without mutation.
    let mut alice2 = connect_when_ready(&name);
    assert_eq!(alice2.pair(SECRET, ALICE).unwrap(), true);
    let rejected = alice2
        .request(&json!({"v": 2, "op": "prepare", "tx": "tx-v2", "intent": intent()}))
        .unwrap();
    assert_eq!(rejected["status"], "conflict");
    let after = alice2
        .request(&json!({"v": 1, "op": "query", "tx": "tx-v2"}))
        .unwrap();
    assert_eq!(after["status"], "not-found");
    drop(alice2);

    // Shutdown: set the flag and wake the accept loop with one connection.
    stop.store(true, Ordering::Relaxed);
    let _ = PipeClient::connect(&name);
    let joined = server.join().expect("server thread panicked");
    assert!(joined.is_ok(), "serve failed: {joined:?}");
    assert_eq!(*engine.adds.lock().unwrap(), 1);
}

//! G2 category-routing acceptance fixtures (contract section "Nghiệm thu").
//!
//! Loopback HTTP server + real engine: Content-Disposition beats the URL
//! name, the file lands in the category subfolder with the right SHA-256,
//! settings changed after enqueue cannot reroute a pending task, an
//! explicit destination equal to the default bypasses routing, a junction
//! escaping the root fails the task, and legacy tasks stay put.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use sha2::Digest;
use usagi::category;
use usagi::config::Settings;
use usagi::engine::{AddRequest, Engine, EngineBuilder};
use usagi::paths::Paths;
use usagi::store::TaskState;

fn make_engine(tmp: &Path, auto: bool, categories: Option<Vec<category::CategoryRule>>) -> Engine {
    make_engine_with(tmp, auto, categories, None)
}

fn make_engine_with(
    tmp: &Path,
    auto: bool,
    categories: Option<Vec<category::CategoryRule>>,
    conflict: Option<usagi::config::ConflictPolicy>,
) -> Engine {
    let paths = Paths {
        root: tmp.into(),
        db_file: tmp.join("test.redb"),
        log_file: tmp.join("logs/test.log"),
        temp_dir: tmp.join("tmp"),
        default_download_dir: tmp.join("downloads"),
        portable: true,
        exe_dir: Some(tmp.into()),
    };
    let mut settings = Settings {
        http3_enabled: false,
        use_system_proxy: false,
        segments_per_task: 1,
        auto_categorize: auto,
        categories,
        default_download_dir: tmp.join("downloads").to_string_lossy().into(),
        ..Settings::default()
    };
    if let Some(policy) = conflict {
        settings.conflict_policy = policy;
    }
    EngineBuilder::new()
        .paths(paths)
        .settings(settings)
        .build()
        .unwrap()
}

/// Serve `requests` connections. Each HEAD gets headers only (with the given
/// Content-Disposition filename, if any); each GET gets the full payload.
/// Retries may add connections; extra accepts block on a thread we abandon.
fn serve(payload: Vec<u8>, cd_filename: Option<String>, requests: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for _ in 0..requests {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut req = Vec::new();
            let mut byte = [0u8; 1];
            while !req.ends_with(b"\r\n\r\n") {
                match stream.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => req.push(byte[0]),
                    Err(_) => break,
                }
            }
            let req_str = String::from_utf8_lossy(&req);
            let cd = cd_filename
                .as_ref()
                .map(|n| format!("Content-Disposition: attachment; filename=\"{n}\"\r\n"))
                .unwrap_or_default();
            if req_str.starts_with("HEAD ") {
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n{}Connection: close\r\n\r\n",
                    payload.len(),
                    cd
                );
            } else {
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
                    payload.len(),
                    cd
                );
                let _ = stream.write_all(&payload);
            }
        }
    });
    port
}

fn wait_terminal(engine: &Engine, id: u64) -> usagi::TaskSnapshot {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut events = String::new();
    loop {
        for ev in engine.drain_events(64) {
            events.push_str(&format!("{ev:?}\n"));
        }
        let task = engine.task(id).unwrap();
        if task.state.is_terminal() {
            return task;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "task {id} did not finish; state={:?} error={:?}\nengine events:\n{events}",
            task.state,
            task.error
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn retry_and_wait(engine: &Engine, id: u64) -> usagi::TaskSnapshot {
    // retry() enqueues a command. The previous Failed snapshot remains visible
    // until the supervisor processes it. Observe that command's Queued event
    // before accepting another terminal snapshot, even if the new run finishes
    // too quickly to observe a non-terminal state by polling.
    while !engine.drain_events(256).is_empty() {}
    engine.retry(id).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if engine.drain_events(256).iter().any(|event| {
            matches!(
                event,
                usagi::Event::TaskStateChanged { id: event_id, state: TaskState::Queued }
                    if *event_id == id
            )
        }) {
            return wait_terminal(engine, id);
        }
        assert!(
            std::time::Instant::now() < deadline,
            "retry command was not acknowledged for {id}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn fixture(auto: bool) -> (tempfile::TempDir, PathBuf, Engine) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("downloads");
    let engine = make_engine(dir.path(), auto, None);
    (dir, root, engine)
}

#[test]
fn content_disposition_wins_and_routes_by_category() {
    let (_dir, root, engine) = fixture(true);
    let payload = vec![0x5Au8; 48 * 1024];
    let expected = hex::encode(sha2::Sha256::digest(&payload));
    // URL says .bin; Content-Disposition says .mp3 -> Music wins.
    let port = serve(payload.clone(), Some("song.mp3".into()), 6);
    let id = engine
        .add(AddRequest::new(format!(
            "http://127.0.0.1:{port}/payload.bin"
        )))
        .unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Completed, "error: {:?}", task.error);
    let landed = root.join("Music").join("song.mp3");
    assert_eq!(PathBuf::from(&task.final_path), landed);
    let bytes = std::fs::read(&landed).unwrap();
    assert_eq!(bytes, payload);
    assert_eq!(hex::encode(sha2::Sha256::digest(&bytes)), expected);
    engine.shutdown();
}

#[test]
fn url_name_routes_when_no_content_disposition() {
    let (_dir, root, engine) = fixture(true);
    let payload = vec![0x11u8; 8192];
    let port = serve(payload.clone(), None, 6);
    let id = engine
        .add(AddRequest::new(format!(
            "http://127.0.0.1:{port}/files/movie.mkv?token=abc"
        )))
        .unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Completed, "error: {:?}", task.error);
    let landed = root.join("Videos").join("movie.mkv");
    assert!(landed.exists(), "expected file at {}", landed.display());
    assert_eq!(std::fs::read(&landed).unwrap(), payload);
    engine.shutdown();
}

#[test]
fn explicit_destination_equal_to_default_bypasses_routing() {
    let (_dir, root, engine) = fixture(true);
    let payload = vec![0x22u8; 8192];
    let port = serve(payload.clone(), None, 6);
    let id = engine
        .add(AddRequest::new(format!("http://127.0.0.1:{port}/song.mp3")).dest_dir(root.clone()))
        .unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Completed, "error: {:?}", task.error);
    // Straight into the root, no Music subfolder routing.
    assert_eq!(PathBuf::from(&task.final_path), root.join("song.mp3"));
    assert!(!root.join("Music").exists());
    engine.shutdown();
}

#[test]
fn settings_change_after_enqueue_does_not_reroute() {
    let dir = tempfile::tempdir().unwrap();
    let engine = make_engine(dir.path(), true, None);
    let payload = vec![0x33u8; 8192];
    let port = serve(payload.clone(), None, 6);
    // Enqueue paused: the snapshot freezes here.
    let id = engine
        .add(AddRequest::new(format!("http://127.0.0.1:{port}/song.mp3")).start_paused(true))
        .unwrap();
    // Owner turns auto off AND replaces the rules entirely.
    let mut settings = engine.settings();
    settings.auto_categorize = false;
    settings.categories = Some(vec![
        category::CategoryRule {
            id: "audio-x".into(),
            folder: "AudioX".into(),
            suffixes: vec![".mp3".into()],
        },
        category::CategoryRule {
            id: "other".into(),
            folder: "Other".into(),
            suffixes: vec![],
        },
    ]);
    engine.update_settings(settings).unwrap();
    thread::sleep(Duration::from_millis(300));
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Completed, "error: {:?}", task.error);
    let root = dir.path().join("downloads");
    // The enqueue-time snapshot (default rules -> Music) wins.
    assert_eq!(
        PathBuf::from(&task.final_path),
        root.join("Music").join("song.mp3")
    );
    assert!(!root.join("AudioX").exists());
    engine.shutdown();
}

/// Poll until `cond` holds for the task snapshot (or panic at the deadline).
fn wait_until(engine: &Engine, id: u64, what: &str, cond: impl Fn(&usagi::TaskSnapshot) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        engine.drain_events(64);
        let task = engine.task(id).unwrap();
        if cond(&task) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "timed out waiting for {what}; state={:?} error={:?}",
            task.state,
            task.error
        );
        thread::sleep(Duration::from_millis(25));
    }
}

/// Zed G2 review, P1: if the durable write that locks the route fails, a
/// retry must NOT trust the in-memory lock — it must re-run the lock and
/// fail again while persistence keeps failing, and no `.part` may ever be
/// claimed. Once writes work again the same task completes through the
/// normal lock path.
#[test]
fn failed_route_lock_persist_cannot_be_bypassed_by_retry() {
    let (_dir, root, engine) = fixture(true);
    let payload = vec![0x66u8; 8192];
    let port = serve(payload.clone(), None, 64);
    // Arm the store to fail every save for the whole first phase. The exact
    // number of engine saves is an implementation detail, so over-arm and
    // clear the fault explicitly between phases.
    let id = engine
        .add(AddRequest::new(format!("http://127.0.0.1:{port}/song.mp3")).start_paused(true))
        .unwrap();
    // Arm AFTER the enqueue persists: the fault targets the route-lock
    // write (and everything after it), not the add itself.
    engine.store().set_save_failure(true);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "error: {:?}", task.error);
    assert!(
        task.error.as_deref().unwrap_or("").contains("routing"),
        "unexpected error: {:?}",
        task.error
    );
    // Persistence is STILL failing: a user retry must fail the same way,
    // not slip past the lock and claim a partial.
    let task = retry_and_wait(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "error: {:?}", task.error);
    assert!(
        task.error.as_deref().unwrap_or("").contains("routing"),
        "retry error: {:?}",
        task.error
    );
    // Zero partials anywhere under the download root after both attempts.
    assert_eq!(
        count_partials(&root),
        0,
        "a partial was claimed despite the failed lock"
    );
    // Fault cleared: the same task now locks durably and completes.
    engine.store().set_save_failure(false);
    let task = retry_and_wait(&engine, id);
    assert_eq!(task.state, TaskState::Completed, "error: {:?}", task.error);
    let landed = root.join("Music").join("song.mp3");
    assert_eq!(std::fs::read(&landed).unwrap(), payload);
    engine.shutdown();
}

/// Zed G2 review, P2 (first half): cancelling a task must NOT clear its
/// routing metadata. The hazardous interleaving is cancel racing a live
/// attempt: the supervisor marks the task Canceled, then the worker's stop
/// handler writes the record again — under the old code that second write
/// reset a locked route to `Legacy`, silently un-routing the task. The
/// durable proof is at store level: after cancel plus a settle window in
/// which the stop handler has run, the on-disk record still sits at the
/// locked category destination (it was NOT reset to the provisional root).
#[test]
fn cancel_does_not_clear_a_locked_route() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("downloads");
    // Downloads root pre-created with a decoy partial at the *provisional*
    // root path — the path the task would claim if its route were lost.
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("song.mp3.udm.part"), b"decoy").unwrap();
    // Big payload on a non-range server: one slow-ish stream, so the task
    // is still Downloading when the cancel lands.
    let payload = vec![0x77u8; 32 * 1024 * 1024];
    let port = serve(payload, None, 4);
    let engine = make_engine(dir.path(), true, None);
    let id = engine
        .add(AddRequest::new(format!("http://127.0.0.1:{port}/song.mp3")))
        .unwrap();
    wait_until(&engine, id, "downloading state", |t| {
        t.state == TaskState::Downloading
    });
    engine.cancel(id).unwrap();
    wait_until(&engine, id, "canceled state", |t| {
        t.state == TaskState::Canceled
    });
    // Let the worker's stop handler run its final persist.
    thread::sleep(Duration::from_millis(500));
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.state, TaskState::Canceled);
    assert!(
        !matches!(
            stored.route,
            usagi::RouteState::Legacy | usagi::RouteState::Pending { .. }
        ),
        "cancel cleared the routing metadata: {:?}",
        stored.route
    );
    assert_eq!(
        stored.dest_dir(),
        root.join("Music"),
        "stored destination fell back to the provisional root"
    );
    engine.shutdown();
}

/// Count `.udm.part` files anywhere under `root`.
fn count_partials(root: &Path) -> usize {
    let mut stack = vec![root.to_path_buf()];
    let mut partials = 0;
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.to_string_lossy().ends_with(".udm.part") {
                    partials += 1;
                }
            }
        }
    }
    partials
}

/// Zed G2 re-review, P1 (both halves): a failed lock save must publish
/// NOTHING — not on disk, and not into the live record. Custom rules give
/// `.tar.gz` and `.gz` DIFFERENT categories, so a retry that classified
/// the collision-adjusted name (`archive.tar (1).gz`) instead of the
/// original would land in the wrong folder: the discriminating case the
/// round-2 fixture (plain filename, no collision) could not exercise.
#[test]
fn failed_save_keeps_original_name_and_route_unpublished() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("downloads");
    // An existing final file forces the collision suffix on every attempt.
    let archives = root.join("Archives");
    std::fs::create_dir_all(&archives).unwrap();
    std::fs::write(archives.join("archive.tar.gz"), b"pre-existing").unwrap();
    let rules = vec![
        category::CategoryRule {
            id: "archives".into(),
            folder: "Archives".into(),
            suffixes: vec![".tar.gz".into()],
        },
        category::CategoryRule {
            id: "gz-only".into(),
            folder: "GzOnly".into(),
            suffixes: vec![".gz".into()],
        },
        category::CategoryRule {
            id: "other".into(),
            folder: "Other".into(),
            suffixes: vec![],
        },
    ];
    let engine = make_engine(dir.path(), true, Some(rules));
    let payload = vec![0x88u8; 8192];
    let port = serve(payload.clone(), None, 64);
    let id = engine
        .add(AddRequest::new(format!("http://127.0.0.1:{port}/archive.tar.gz")).start_paused(true))
        .unwrap();
    engine.store().set_save_failure(true);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "error: {:?}", task.error);
    assert!(
        task.error.as_deref().unwrap_or("").contains("routing"),
        "unexpected error: {:?}",
        task.error
    );
    // Nothing was published: the stored record still has its PRE-LOCK
    // filename (empty — the name was only ever probe-derived, so a failed
    // lock must not freeze it as if it were user input), the provisional
    // root destination and an unlocked route, and no partial exists.
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.filename, "");
    assert_eq!(stored.dest_dir(), root);
    assert!(
        stored.route_unlocked(),
        "route was published despite the failed save: {:?}",
        stored.route
    );
    assert_eq!(count_partials(&root), 0, "a partial was claimed");
    // A retry while persistence still fails fails identically.
    let task = retry_and_wait(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "error: {:?}", task.error);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.filename, "");
    assert!(stored.route_unlocked());
    // Fault cleared: the retry classifies the ORIGINAL name (`.tar.gz` ->
    // Archives), applies the collision suffix inside that folder only,
    // and completes. The pre-existing file is untouched.
    engine.store().set_save_failure(false);
    let task = retry_and_wait(&engine, id);
    assert_eq!(task.state, TaskState::Completed, "error: {:?}", task.error);
    let landed = PathBuf::from(&task.final_path);
    assert_eq!(landed.parent().unwrap(), archives.as_path());
    let landed_name = landed.file_name().unwrap().to_string_lossy().to_string();
    assert!(
        landed_name.starts_with("archive.tar") && landed_name.ends_with(".gz"),
        "unexpected landed name: {landed_name}"
    );
    assert_eq!(std::fs::read(&landed).unwrap(), payload);
    assert_eq!(
        std::fs::read(archives.join("archive.tar.gz")).unwrap(),
        b"pre-existing"
    );
    assert!(
        !root.join("GzOnly").exists(),
        "retry reclassified by the collision-adjusted name"
    );
    engine.shutdown();
}

/// Zed G2 re-review, P2: under the Fail conflict policy, a name conflict
/// at lock time is a terminal ROUTING error — it must not enter the
/// automatic retry path as a generic conflict.
#[test]
fn lock_conflict_under_fail_policy_is_a_routing_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("downloads");
    let music = root.join("Music");
    std::fs::create_dir_all(&music).unwrap();
    std::fs::write(music.join("song.mp3"), b"pre-existing").unwrap();
    let engine = make_engine_with(
        dir.path(),
        true,
        None,
        Some(usagi::config::ConflictPolicy::Fail),
    );
    let payload = vec![0x99u8; 4096];
    let port = serve(payload, None, 8);
    let id = engine
        .add(AddRequest::new(format!("http://127.0.0.1:{port}/song.mp3")))
        .unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "error: {:?}", task.error);
    assert!(
        task.error.as_deref().unwrap_or("").contains("routing"),
        "conflict was not normalized to a routing error: {:?}",
        task.error
    );
    // Exactly one failed attempt's worth of error: the task did not burn
    // through automatic retries (attempts stay low) and nothing landed.
    assert_eq!(count_partials(&root), 0);
    assert_eq!(
        std::fs::read(music.join("song.mp3")).unwrap(),
        b"pre-existing"
    );
    engine.shutdown();
}

#[test]
fn auto_off_keeps_legacy_behaviour() {
    let (_dir, root, engine) = fixture(false);
    let payload = vec![0x44u8; 8192];
    let port = serve(payload.clone(), None, 6);
    let id = engine
        .add(AddRequest::new(format!("http://127.0.0.1:{port}/song.mp3")))
        .unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Completed, "error: {:?}", task.error);
    assert_eq!(PathBuf::from(&task.final_path), root.join("song.mp3"));
    engine.shutdown();
}

#[cfg(windows)]
#[test]
fn junction_escaping_root_fails_the_task() {
    let (_dir, root, engine) = fixture(true);
    std::fs::create_dir_all(&root).unwrap();
    let outside = _dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    // A junction named like the category folder, pointing outside the root.
    let status = std::process::Command::new("cmd")
        .args([
            "/C",
            "mklink",
            "/J",
            &root.join("Music").to_string_lossy(),
            &outside.to_string_lossy(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "mklink /J failed");
    let payload = vec![0x55u8; 4096];
    let port = serve(payload, None, 6);
    let id = engine
        .add(AddRequest::new(format!("http://127.0.0.1:{port}/song.mp3")))
        .unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "error: {:?}", task.error);
    assert!(
        task.error.as_deref().unwrap_or("").contains("routing"),
        "unexpected error: {:?}",
        task.error
    );
    // Nothing was written outside the root.
    assert!(!outside.join("song.mp3").exists());
    assert!(!outside.join("song.mp3.udm.part").exists());
    engine.shutdown();
}

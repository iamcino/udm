//! Usagi PR 4 slice 4a: the state publication matrix (amended spec section 5).
//!
//! The table drives `publish_exit` through [`Engine::oracle_apply_exit`]. The
//! scenarios around it drive the real supervisor and the real run loop: pause
//! of `Failed`, cancel of `Completed`, a definitive error that arrives after
//! pause, `Queued` surviving that error until the admit tick, cancel during
//! a retry sleep, and the `watch` resume set.

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use udm_core::{
    engine::{
        oracle_owns_attempt, watch_resumes, AddRequest, Engine, EngineBuilder, Event, LogLevel,
        OracleExit, OracleStop,
    },
    Settings, TaskState,
};

const BODY: &[u8] = b"udm-4a";
const HOLD: u8 = 0;
const FAIL: u8 = 1;
/// First two requests are 404. Later requests are the full body.
const COUNT: u8 = 2;
const RETRY: u8 = 3;
const SERVE: u8 = 4;

struct Server {
    url: String,
    mode: Arc<AtomicU8>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    gets: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn start(mode: u8, released: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
        let mode = Arc::new(AtomicU8::new(mode));
        let gate = Arc::new((Mutex::new(released), Condvar::new()));
        let gets = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (m, g, n, s) = (mode.clone(), gate.clone(), gets.clone(), stop.clone());
        let thread = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (m, g, n) = (m.clone(), g.clone(), n.clone());
                        thread::spawn(move || {
                            let _ = serve(stream, &m, &g, &n);
                        });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(e) => panic!("{e}"),
                }
            }
        });
        Self {
            url,
            mode,
            gate,
            gets,
            stop,
            thread: Some(thread),
        }
    }

    fn set_mode(&self, mode: u8) {
        self.mode.store(mode, Ordering::SeqCst);
    }

    fn release(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.release();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    mut stream: std::net::TcpStream,
    mode: &AtomicU8,
    gate: &(Mutex<bool>, Condvar),
    gets: &AtomicUsize,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        request.push(byte[0]);
        if request.len() > 64 * 1024 {
            return Ok(());
        }
    }
    let n = gets.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let (lock, cv) = gate;
        let mut guard = lock.lock().unwrap();
        while !*guard {
            let (next, wait) = cv.wait_timeout(guard, Duration::from_secs(20)).unwrap();
            guard = next;
            if wait.timed_out() {
                return Ok(());
            }
        }
    }
    let head = request.starts_with(b"HEAD ");
    let status = match mode.load(Ordering::SeqCst) {
        FAIL => 404,
        RETRY => 503,
        COUNT if n <= 2 => 404,
        _ => 200,
    };
    if status == 200 {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"fixed\"\r\nConnection: close\r\n\r\n",
            BODY.len()
        )?;
        if !head {
            stream.write_all(BODY)?;
        }
    } else {
        write!(
            stream,
            "HTTP/1.1 {status} nope\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )?;
    }
    let _ = stream.flush();
    Ok(())
}

fn engine(root: &std::path::Path, idle: bool) -> Engine {
    let paths = udm_core::paths::Paths {
        root: root.into(),
        db_file: root.join("test.redb"),
        log_file: root.join("logs/test.log"),
        temp_dir: root.join("tmp"),
        default_download_dir: root.join("downloads"),
        portable: true,
        exe_dir: Some(root.into()),
    };
    EngineBuilder::new()
        .paths(paths)
        .idle(idle)
        .settings(Settings {
            http3_enabled: false,
            use_system_proxy: false,
            learn_alt_svc: false,
            segments_per_task: 1,
            min_segment_bytes: 64 * 1024,
            verify_checksums: true,
            max_retries: 5,
            default_download_dir: root.join("downloads").to_string_lossy().into_owned(),
            ..Settings::default()
        })
        .build()
        .unwrap()
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < limit,
            "{what} not reached within {limit:?}"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn logs_of(events: &[Event]) -> Vec<(LogLevel, String)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Log { level, message } => Some((*level, message.clone())),
            _ => None,
        })
        .collect()
}

struct Row {
    name: &'static str,
    run_generation: u64,
    current_generation: u64,
    prior: TaskState,
    prior_error: Option<&'static str>,
    stop: OracleStop,
    definitive: Option<&'static str>,
    state: TaskState,
    error: Option<&'static str>,
    discard_log: bool,
    failed_event: bool,
    publishes: bool,
}

fn row(
    name: &'static str,
    prior: TaskState,
    stop: OracleStop,
    definitive: Option<&'static str>,
    state: TaskState,
    error: Option<&'static str>,
    discard_log: bool,
    failed_event: bool,
) -> Row {
    Row {
        name,
        run_generation: 1,
        current_generation: 1,
        prior,
        prior_error: error.filter(|_| state == prior).or(match prior {
            TaskState::Failed | TaskState::Completed | TaskState::Canceled => error,
            _ => None,
        }),
        stop,
        definitive,
        state,
        error,
        discard_log,
        failed_event,
        publishes: true,
    }
}

#[test]
fn publication_matrix_follows_section_5() {
    // `prior_error` is the string already stored. `error` is what must remain
    // after publication. A stale row publishes nothing, including no log.
    let rows = [
        Row {
            name: "stale generation writes nothing",
            run_generation: 1,
            current_generation: 2,
            prior: TaskState::Downloading,
            prior_error: Some("already"),
            stop: OracleStop::Pause,
            definitive: Some("stale-must-not-land"),
            state: TaskState::Downloading,
            error: Some("already"),
            discard_log: false,
            failed_event: false,
            publishes: false,
        },
        Row {
            name: "queued survives a definitive error",
            run_generation: 1,
            current_generation: 1,
            prior: TaskState::Queued,
            prior_error: None,
            stop: OracleStop::None,
            definitive: Some("queued-drop"),
            state: TaskState::Queued,
            error: None,
            discard_log: true,
            failed_event: false,
            publishes: true,
        },
        Row {
            name: "queued survives a clean stop",
            run_generation: 1,
            current_generation: 1,
            prior: TaskState::Queued,
            prior_error: None,
            stop: OracleStop::Pause,
            definitive: None,
            state: TaskState::Queued,
            error: None,
            discard_log: false,
            failed_event: false,
            publishes: true,
        },
        row(
            "completed survives fail",
            TaskState::Completed,
            OracleStop::None,
            Some("nope"),
            TaskState::Completed,
            Some("kept"),
            false,
            false,
        ),
        row(
            "canceled survives fail",
            TaskState::Canceled,
            OracleStop::Pause,
            Some("nope"),
            TaskState::Canceled,
            Some("kept"),
            false,
            false,
        ),
        row(
            "failed stays failed under pause",
            TaskState::Failed,
            OracleStop::Pause,
            Some("new"),
            TaskState::Failed,
            Some("kept"),
            false,
            false,
        ),
        row(
            "failed stays failed under shutdown",
            TaskState::Failed,
            OracleStop::Shutdown,
            Some("new"),
            TaskState::Failed,
            Some("kept"),
            false,
            false,
        ),
        row(
            "cancel wins over a definitive error",
            TaskState::Downloading,
            OracleStop::Cancel,
            Some("hidden"),
            TaskState::Canceled,
            None,
            false,
            false,
        ),
        row(
            "remove wins over a definitive error",
            TaskState::Probing,
            OracleStop::Remove,
            Some("hidden"),
            TaskState::Canceled,
            None,
            false,
            false,
        ),
        row(
            "pause does not hide a definitive error",
            TaskState::Downloading,
            OracleStop::Pause,
            Some("representation mismatch"),
            TaskState::Failed,
            Some("representation mismatch"),
            false,
            true,
        ),
        row(
            "shutdown does not hide a definitive error",
            TaskState::Probing,
            OracleStop::Shutdown,
            Some("unsupported coding"),
            TaskState::Failed,
            Some("unsupported coding"),
            false,
            true,
        ),
        row(
            "verifying plus a definitive error fails",
            TaskState::Verifying,
            OracleStop::None,
            Some("checksum"),
            TaskState::Failed,
            Some("checksum"),
            false,
            true,
        ),
        row(
            "pause of an owned run publishes paused",
            TaskState::Downloading,
            OracleStop::Pause,
            None,
            TaskState::Paused,
            None,
            false,
            false,
        ),
        row(
            "shutdown of an owned run publishes paused",
            TaskState::Verifying,
            OracleStop::Shutdown,
            None,
            TaskState::Paused,
            None,
            false,
            false,
        ),
        row(
            "clean stop publishes paused",
            TaskState::Probing,
            OracleStop::None,
            None,
            TaskState::Paused,
            None,
            false,
            false,
        ),
    ];
    // The `row()` helper stores `error` as the prior error only when the
    // state is unchanged. Rows that write Failed pass the same string as the
    // definitive error, and `row` would also plant it as prior_error. Clear
    // that for the write rows so the test shows the publication stored it.
    let rows: Vec<Row> = rows
        .into_iter()
        .map(|mut row| {
            if row.definitive == row.error && row.prior != row.state {
                row.prior_error = None;
            }
            if matches!(
                row.prior,
                TaskState::Completed | TaskState::Canceled | TaskState::Failed
            ) && row.error == Some("kept")
            {
                row.prior_error = Some("kept");
            }
            row
        })
        .collect();

    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), true);
    let id = engine
        .add(AddRequest::new("http://127.0.0.1:9/matrix.bin").start_paused(true))
        .unwrap();
    // Let the add-time Admit command land while the task is still paused.
    // A later row that leaves the task Queued must not be started by that command.
    thread::sleep(Duration::from_millis(100));
    let _ = engine.drain_events(512);
    for row in &rows {
        let _ = engine.drain_events(512);
        let snap = engine
            .oracle_apply_exit(
                id,
                OracleExit {
                    run_generation: row.run_generation,
                    current_generation: row.current_generation,
                    prior: row.prior,
                    prior_error: row.prior_error.map(str::to_owned),
                    stop: row.stop,
                    definitive: row.definitive.map(str::to_owned),
                },
            )
            .unwrap();
        let events = engine.drain_events(512);
        assert_eq!(snap.state, row.state, "{}", row.name);
        assert_eq!(snap.error.as_deref(), row.error, "{}", row.name);
        let discard = logs_of(&events).into_iter().any(|(level, message)| {
            level == LogLevel::Warn
                && message.contains("discarded error while state is queued")
                && row.definitive.is_some_and(|text| message.contains(text))
        });
        assert_eq!(discard, row.discard_log, "{}", row.name);
        let failed_event = events.iter().any(|event| {
            matches!(event, Event::TaskFailed { id: event_id, error }
                if *event_id == id && row.definitive == Some(error.as_str()))
        });
        assert_eq!(failed_event, row.failed_event, "{}", row.name);
        let state_event = events.iter().any(|event| {
            matches!(event, Event::TaskStateChanged { id: event_id, state }
                if *event_id == id && *state == row.state)
        });
        assert_eq!(state_event, row.publishes, "{}", row.name);
        if !row.publishes {
            assert!(
                events.iter().all(|event| !matches!(
                    event,
                    Event::TaskStateChanged { .. } | Event::TaskFailed { .. } | Event::Log { .. }
                )),
                "{} published {:?}",
                row.name,
                events
            );
        }
    }
    engine.shutdown();
}

#[test]
fn owns_attempt_matches_the_post_wake_recheck() {
    let states = [
        TaskState::Queued,
        TaskState::Probing,
        TaskState::Downloading,
        TaskState::Verifying,
        TaskState::Paused,
        TaskState::Completed,
        TaskState::Failed,
        TaskState::Canceled,
    ];
    for state in states {
        assert!(
            !oracle_owns_attempt(true, true, state),
            "cancelled token still owns {state:?}"
        );
        assert!(
            !oracle_owns_attempt(false, false, state),
            "stale generation still owns {state:?}"
        );
        let owns = matches!(
            state,
            TaskState::Probing | TaskState::Downloading | TaskState::Verifying
        );
        assert_eq!(oracle_owns_attempt(false, true, state), owns, "{state:?}");
    }
}

#[test]
fn pause_of_failed_emits_the_stored_state() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(FAIL, true);
    let engine = engine(dir.path(), false);
    let id = engine.add(AddRequest::new(server.url.clone())).unwrap();
    wait_until("404 reaches Failed", Duration::from_secs(15), || {
        engine.task(id).unwrap().state == TaskState::Failed
    });
    let error = engine.task(id).unwrap().error.clone();
    assert!(error.as_deref().unwrap().contains("404"), "{error:?}");
    settle(&engine);
    engine.pause(id).unwrap();
    let event = wait_state_event(&engine, id, Duration::from_secs(5));
    assert_eq!(event, TaskState::Failed);
    let task = engine.task(id).unwrap();
    assert_eq!(task.state, TaskState::Failed);
    assert_eq!(task.error, error);
    engine.shutdown();
}

#[test]
fn cancel_of_completed_leaves_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(SERVE, true);
    let engine = engine(dir.path(), false);
    let id = engine.add(AddRequest::new(server.url.clone())).unwrap();
    wait_until("download completed", Duration::from_secs(15), || {
        engine.task(id).unwrap().state == TaskState::Completed
    });
    let task = engine.task(id).unwrap();
    let bytes = std::fs::read(&task.final_path).unwrap();
    assert_eq!(bytes, BODY);
    settle(&engine);
    engine.cancel(id).unwrap();
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(400) {
        for event in engine.drain_events(256) {
            if let Event::TaskStateChanged {
                id: event_id,
                state,
            } = event
            {
                assert!(
                    !(event_id == id && state == TaskState::Canceled),
                    "cancel rewrote Completed"
                );
            }
        }
        assert_eq!(engine.task(id).unwrap().state, TaskState::Completed);
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read(&task.final_path).unwrap(), BODY);
    engine.shutdown();
}

#[test]
fn cancel_of_failed_still_cancels() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(FAIL, true);
    let engine = engine(dir.path(), false);
    let id = engine.add(AddRequest::new(server.url.clone())).unwrap();
    wait_until("failed", Duration::from_secs(15), || {
        engine.task(id).unwrap().state == TaskState::Failed
    });
    engine.cancel(id).unwrap();
    wait_until("canceled", Duration::from_secs(5), || {
        engine.task(id).unwrap().state == TaskState::Canceled
    });
    engine.shutdown();
}

/// The probe does not watch the task token, so pause lands while the run is
/// still inside `execute`. The 404 then returns with the token already
/// cancelled. Step 5 must store Failed over the supervisor's Paused.
#[test]
fn definitive_error_after_pause_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(HOLD, false);
    let engine = engine(dir.path(), false);
    let id = engine.add(AddRequest::new(server.url.clone())).unwrap();
    wait_until("probe is held", Duration::from_secs(10), || {
        server.gets.load(Ordering::SeqCst) >= 1
    });
    engine.pause(id).unwrap();
    wait_until("supervisor paused the row", Duration::from_secs(5), || {
        engine.task(id).unwrap().state == TaskState::Paused
    });
    server.set_mode(FAIL);
    server.release();
    wait_until("definitive error stored", Duration::from_secs(15), || {
        engine.task(id).unwrap().state.is_terminal()
    });
    let task = engine.task(id).unwrap();
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    assert!(
        task.error.as_deref().unwrap().contains("404"),
        "{:?}",
        task.error
    );
    engine.shutdown();
}

/// Resume while the old run is still blocked in the probe. The 404 belongs
/// to that run and must be logged, not stored. The admit tick then starts
/// the queued run, which completes.
#[test]
fn queued_survives_draining_failure_and_is_admitted() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(HOLD, false);
    let engine = engine(dir.path(), false);
    let id = engine.add(AddRequest::new(server.url.clone())).unwrap();
    wait_until("probe is held", Duration::from_secs(10), || {
        server.gets.load(Ordering::SeqCst) >= 1
    });
    engine.pause(id).unwrap();
    wait_until("paused", Duration::from_secs(5), || {
        engine.task(id).unwrap().state == TaskState::Paused
    });
    engine.resume(id).unwrap();
    wait_until("resumed during drain", Duration::from_secs(5), || {
        engine.task(id).unwrap().state == TaskState::Queued
    });
    let held = Instant::now();
    while held.elapsed() < Duration::from_millis(400) {
        let task = engine.task(id).unwrap();
        assert_eq!(
            task.state,
            TaskState::Queued,
            "admitted while the old run still held it"
        );
        assert_eq!(server.gets.load(Ordering::SeqCst), 1);
        assert!(task.error.is_none(), "{:?}", task.error);
        thread::sleep(Duration::from_millis(20));
    }
    server.set_mode(COUNT);
    server.release();
    let mut discard = false;
    let mut saw_failed = false;
    let start = Instant::now();
    loop {
        let task = engine.task(id).unwrap();
        if task.state == TaskState::Failed {
            saw_failed = true;
        }
        for (level, message) in logs_of(&engine.drain_events(256)) {
            if level == LogLevel::Warn
                && message.contains("discarded error while state is queued")
                && message.contains("404")
            {
                discard = true;
            }
        }
        if task.state == TaskState::Completed {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "state {:?}",
            task.state
        );
        thread::sleep(Duration::from_millis(15));
    }
    let task = engine.task(id).unwrap();
    assert!(discard, "draining error was not logged");
    assert!(!saw_failed, "draining run published Failed over Queued");
    assert!(task.error.is_none(), "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), BODY);
    engine.shutdown();
}

#[test]
fn cancel_during_retry_sleep_does_not_open_another_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(RETRY, true);
    let engine = engine(dir.path(), false);
    let id = engine.add(AddRequest::new(server.url.clone())).unwrap();
    wait_until("first attempt failed", Duration::from_secs(10), || {
        server.gets.load(Ordering::SeqCst) >= 2 && !engine.task(id).unwrap().state.is_terminal()
    });
    let seen = server.gets.load(Ordering::SeqCst);
    thread::sleep(Duration::from_millis(200));
    assert_eq!(
        server.gets.load(Ordering::SeqCst),
        seen,
        "the run left the retry sleep before cancel"
    );
    engine.cancel(id).unwrap();
    wait_until("canceled", Duration::from_secs(5), || {
        engine.task(id).unwrap().state == TaskState::Canceled
    });
    thread::sleep(Duration::from_secs(2));
    assert_eq!(server.gets.load(Ordering::SeqCst), seen);
    assert_eq!(engine.task(id).unwrap().state, TaskState::Canceled);
    engine.shutdown();
}

#[test]
fn owned_retry_after_sleep_opens_another_request() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(RETRY, true);
    let engine = engine(dir.path(), false);
    let id = engine.add(AddRequest::new(server.url.clone())).unwrap();
    wait_until("first attempt counted", Duration::from_secs(10), || {
        server.gets.load(Ordering::SeqCst) >= 2
    });
    let slept = Instant::now();
    wait_until("second attempt opened", Duration::from_secs(10), || {
        server.gets.load(Ordering::SeqCst) >= 4
    });
    assert!(
        slept.elapsed() >= Duration::from_millis(1000),
        "second attempt did not wait out the retry sleep ({:?})",
        slept.elapsed()
    );
    let _ = id;
    engine.shutdown();
}

#[test]
fn watch_resumes_excludes_failed() {
    for (state, resumes) in [
        (TaskState::Queued, true),
        (TaskState::Probing, true),
        (TaskState::Downloading, true),
        (TaskState::Verifying, true),
        (TaskState::Paused, true),
        (TaskState::Completed, false),
        (TaskState::Failed, false),
        (TaskState::Canceled, false),
    ] {
        assert_eq!(watch_resumes(state), resumes, "{state:?}");
    }

    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(FAIL, true);
    let engine = engine(dir.path(), false);
    let paused = engine
        .add(AddRequest::new(server.url.clone()).start_paused(true))
        .unwrap();
    let failed = engine.add(AddRequest::new(server.url.clone())).unwrap();
    wait_until("failed task", Duration::from_secs(15), || {
        engine.task(failed).unwrap().state == TaskState::Failed
    });
    assert_eq!(engine.task(paused).unwrap().state, TaskState::Paused);
    let selected: Vec<u64> = engine
        .snapshot()
        .iter()
        .filter(|task| watch_resumes(task.state))
        .map(|task| task.id)
        .collect();
    assert!(selected.contains(&paused), "{selected:?}");
    assert!(!selected.contains(&failed), "{selected:?}");

    // Explicit resume is still the way out. `watch` is not that caller.
    settle(&engine);
    engine.resume(failed).unwrap();
    let resumed = wait_state_event(&engine, failed, Duration::from_secs(5));
    assert_eq!(resumed, TaskState::Queued);
    engine.shutdown();
}

fn settle(engine: &Engine) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(200) {
        if engine.drain_events(256).is_empty() {
            thread::sleep(Duration::from_millis(20));
            if engine.drain_events(256).is_empty() {
                return;
            }
        }
    }
}

fn wait_state_event(engine: &Engine, id: u64, limit: Duration) -> TaskState {
    let start = Instant::now();
    loop {
        for event in engine.drain_events(256) {
            if let Event::TaskStateChanged {
                id: event_id,
                state,
            } = event
            {
                if event_id == id {
                    return state;
                }
            }
        }
        assert!(start.elapsed() < limit, "no state event for #{id}");
        thread::sleep(Duration::from_millis(10));
    }
}

//! Usagi PR 3: revocation is prompt and a new run generation is granted only
//! after the previous run drained. Fixtures hold responses open on a gate so
//! the oracle never depends on a server timing out; `stall_timeout_secs` is
//! raised far above every bound asserted here.
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc, Condvar, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use usagi::{
    config::Settings,
    engine::{AddRequest, Engine, EngineBuilder},
    paths::Paths,
    TaskState,
};

const SIZE: usize = 512 * 1024;
const SEGMENTS: u32 = 8;
/// Revocation must land well inside this; the stall timeout is 120 s.
const PROMPT: Duration = Duration::from_secs(5);

/// How the server answers ranged GETs.
const HOLD_BODY: u8 = 0; // headers + 1 byte, then wait on the gate
const HOLD_HEADERS: u8 = 1; // send nothing until the gate opens
const SERVE: u8 = 2; // whole range immediately

struct Server {
    url: String,
    mode: Arc<AtomicU8>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    gets: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn start(mode: u8) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/fenced.bin", listener.local_addr().unwrap());
        let mode = Arc::new(AtomicU8::new(mode));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gets = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (m, g, n, s) = (mode.clone(), gate.clone(), gets.clone(), stop.clone());
        let thread = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (m, g, n) = (m.clone(), g.clone(), n.clone());
                        // Connection threads may outlive the test's interest;
                        // write errors on sockets the client dropped are expected.
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

    fn release(&self) {
        *self.gate.0.lock().unwrap() = true;
        self.gate.1.notify_all();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.release();
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn serve(
    mut stream: TcpStream,
    mode: &AtomicU8,
    gate: &(Mutex<bool>, Condvar),
    gets: &AtomicUsize,
) -> std::io::Result<()> {
    // Win32 accepted sockets inherit non-blocking mode (see 7bd54a8).
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        request.push(byte[0]);
    }
    let request = String::from_utf8_lossy(&request).into_owned();
    if request.starts_with("HEAD ") {
        return write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\nAccept-Ranges: bytes\r\nETag: \"fixed\"\r\nConnection: close\r\n\r\n"
        );
    }
    gets.fetch_add(1, Ordering::SeqCst);
    let range = request
        .lines()
        .find_map(|l| {
            l.to_ascii_lowercase()
                .strip_prefix("range: bytes=")
                .map(str::to_owned)
        })
        .expect("ranged GET");
    let (start, end) = range.split_once('-').unwrap();
    let (start, end): (usize, usize) = (start.parse().unwrap(), end.parse().unwrap());
    let length = end - start + 1;
    let wait = || {
        let (lock, ready) = gate;
        let _ = ready.wait_timeout_while(lock.lock().unwrap(), Duration::from_secs(60), |r| !*r);
    };
    let held = mode.load(Ordering::SeqCst);
    if held == HOLD_HEADERS {
        wait();
    }
    write!(
        stream,
        "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/{SIZE}\r\nETag: \"fixed\"\r\nConnection: close\r\n\r\n"
    )?;
    if held == HOLD_BODY {
        stream.write_all(b"F")?;
        stream.flush()?;
        wait();
        stream.write_all(&vec![b'F'; length - 1])
    } else {
        stream.write_all(&vec![b'F'; length])
    }
}

fn engine(root: &std::path::Path) -> Engine {
    let paths = Paths {
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
        .settings(Settings {
            http3_enabled: false,
            use_system_proxy: false,
            segments_per_task: SEGMENTS,
            min_segment_bytes: 64 * 1024,
            stall_timeout_secs: 120,
            default_download_dir: root.join("downloads").to_string_lossy().into_owned(),
            ..Settings::default()
        })
        .build()
        .unwrap()
}

fn wait_until(what: &str, limit: Duration, mut done: impl FnMut() -> bool) -> Duration {
    let start = Instant::now();
    while !done() {
        assert!(
            start.elapsed() < limit,
            "{what} not reached within {limit:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    start.elapsed()
}

/// Start a task and wait until every segment request is held by the server.
fn start_held(engine: &Engine, server: &Server, held_body: bool) -> u64 {
    let id = engine.add(AddRequest::new(server.url.clone())).unwrap();
    wait_until("all segment requests held", Duration::from_secs(20), || {
        let task = engine.task(id).unwrap();
        server.gets.load(Ordering::SeqCst) >= SEGMENTS as usize
            && (!held_body
                || (task.active_requests == SEGMENTS && task.received >= SEGMENTS as u64))
    });
    id
}

#[test]
fn pause_and_cancel_revoke_held_bodies_promptly() {
    for cancel in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::start(HOLD_BODY);
        let engine = engine(dir.path());
        let id = start_held(&engine, &server, true);
        if cancel {
            engine.cancel(id)
        } else {
            engine.pause(id)
        }
        .unwrap();
        let took = wait_until("revoked run drained", PROMPT, || {
            engine.task(id).unwrap().active_requests == 0
        });
        let task = engine.task(id).unwrap();
        let expected = if cancel {
            TaskState::Canceled
        } else {
            TaskState::Paused
        };
        assert_eq!(task.state, expected, "{task:?}");
        eprintln!("cancel={cancel}: drained in {took:?}");
        engine.shutdown();
    }
}

#[test]
fn pause_revokes_requests_waiting_for_headers_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(HOLD_HEADERS);
    let engine = engine(dir.path());
    let id = start_held(&engine, &server, false);
    engine.pause(id).unwrap();
    // Before the fix a request waiting for headers was not revocable at all;
    // resume can only be admitted once the old run is gone, so a prompt
    // resume-to-downloading round trip proves the run drained.
    thread::sleep(Duration::from_millis(50));
    server.mode.store(SERVE, Ordering::SeqCst);
    engine.resume(id).unwrap();
    let took = wait_until("resumed run completed", PROMPT, || {
        engine.task(id).unwrap().state == TaskState::Completed
    });
    eprintln!("header-held pause -> resume -> completed in {took:?}");
    let task = engine.task(id).unwrap();
    assert_eq!(std::fs::read(&task.final_path).unwrap(), vec![b'F'; SIZE]);
    engine.shutdown();
}

#[test]
fn remove_reaps_a_held_task_promptly() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(HOLD_BODY);
    let engine = engine(dir.path());
    let id = start_held(&engine, &server, true);
    engine.remove(id, true).unwrap();
    let took = wait_until("removed task reaped", PROMPT, || engine.task(id).is_none());
    eprintln!("remove reaped in {took:?}");
    engine.shutdown();
}

/// The regrant after a revoked run is a new generation; the old run's
/// responses, released only after the new run owns the task, must not
/// disturb the new run's coverage or the final bytes.
#[test]
fn resume_after_pause_regrants_and_completes_intact() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start(HOLD_BODY);
    let engine = engine(dir.path());
    let id = start_held(&engine, &server, true);
    engine.pause(id).unwrap();
    wait_until("revoked run drained", PROMPT, || {
        engine.task(id).unwrap().active_requests == 0
    });
    let first_round = server.gets.load(Ordering::SeqCst);
    // Second run: hold again so the old gate release happens while it lives.
    engine.resume(id).unwrap();
    wait_until("second run holding", Duration::from_secs(20), || {
        server.gets.load(Ordering::SeqCst) >= first_round + SEGMENTS as usize
    });
    server.release();
    wait_until("resumed run completed", Duration::from_secs(20), || {
        engine.task(id).unwrap().state.is_terminal()
    });
    let task = engine.task(id).unwrap();
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(
        (
            task.segments_done,
            task.segments_total,
            task.active_requests
        ),
        (8, 8, 0)
    );
    assert_eq!(std::fs::read(&task.final_path).unwrap(), vec![b'F'; SIZE]);
    engine.shutdown();
}

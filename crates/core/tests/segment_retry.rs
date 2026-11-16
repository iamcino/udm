//! 0.2.2 regression: a stalled segment must be retried on its own, inside
//! the transfer, without consuming a task-level attempt.
//!
//! Field case 2026-11-16: stalls strike per connection, but the single-path
//! transfer used to execute at attempt level — one stalled segment killed
//! all eight, an 8-segment resume burned its 5 task attempts in ~4.5
//! minutes and failed, while a 1-segment download of the same file
//! completed. Here the server stalls the first two requests that touch
//! segment 2's range (headers + a partial chunk, then silence past the
//! stall timeout); the segment's own retries must carry the task to
//! completion without consuming a single task-level attempt.

use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};
use usagi::{
    engine::{AddRequest, Engine, EngineBuilder, Event},
    Settings, TaskState,
};

const TOTAL: usize = 256 * 1024;
const SEGMENT: u64 = 64 * 1024;
const STALL_START: u64 = 2 * SEGMENT;
const STALL_END: u64 = 3 * SEGMENT - 1;

fn body() -> Vec<u8> {
    (0..TOTAL).map(|i| (i % 251) as u8).collect()
}

struct Server {
    url: String,
    region_hits: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
        let region_hits = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (hits, s) = (region_hits.clone(), stop.clone());
        let payload = body();
        let thread = thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let hits = hits.clone();
                        let payload = payload.clone();
                        thread::spawn(move || {
                            let _ = serve(stream, &payload, &hits);
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
            region_hits,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(
    mut stream: std::net::TcpStream,
    payload: &[u8],
    region_hits: &AtomicUsize,
) -> std::io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte)?;
        request.push(byte[0]);
        if request.len() > 64 * 1024 {
            return Ok(());
        }
    }
    let text = String::from_utf8_lossy(&request);
    let head = text.starts_with("HEAD ");
    let range = text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.trim().eq_ignore_ascii_case("range") {
            let spec = value.trim().strip_prefix("bytes=")?;
            let (a, b) = spec.split_once('-')?;
            Some((a.parse::<u64>().ok()?, b.parse::<u64>().ok()?))
        } else {
            None
        }
    });
    if head {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"seg-v1\"\r\nConnection: close\r\n\r\n",
            payload.len()
        )?;
        return Ok(());
    }
    match range {
        Some((a, b)) => {
            let slice = &payload[a as usize..=(b as usize)];
            write!(
                stream,
                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\nContent-Range: bytes {}-{}/{}\r\nAccept-Ranges: bytes\r\nETag: \"seg-v1\"\r\nConnection: close\r\n\r\n",
                slice.len(),
                a,
                b,
                payload.len()
            )?;
            let touches_stall = a <= STALL_END && b >= STALL_START;
            if touches_stall && region_hits.fetch_add(1, Ordering::SeqCst) < 2 {
                // Headers + a partial chunk, then silence well past the
                // client's 5s stall timeout.
                stream.write_all(&slice[..8192])?;
                stream.flush()?;
                thread::sleep(Duration::from_secs(8));
                return Ok(());
            }
            stream.write_all(slice)?;
        }
        None => {
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nETag: \"seg-v1\"\r\nConnection: close\r\n\r\n",
                payload.len()
            )?;
            stream.write_all(payload)?;
        }
    }
    let _ = stream.flush();
    Ok(())
}

fn engine(root: &std::path::Path) -> Engine {
    let paths = usagi::paths::Paths {
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
            learn_alt_svc: false,
            segments_per_task: 4,
            min_segment_bytes: SEGMENT,
            stall_timeout_secs: 5,
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

#[test]
fn stalled_segment_is_retried_inside_the_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let server = Server::start();
    let engine = engine(dir.path());
    let id = engine.add(AddRequest::new(server.url.clone())).unwrap();
    // While the task runs, watch its snapshots: the segment retry must be
    // published on at least one of them (the note lives from the first
    // scheduled retry until the segment returns cleanly).
    let mut saw_retry_note = false;
    wait_until("task completed", Duration::from_secs(60), || {
        let task = engine.task(id).unwrap();
        if let Some(retry) = task.segment_retry {
            assert_eq!(retry.segment, 2, "the stalled segment is index 2");
            assert!(
                (1..=2).contains(&retry.attempt),
                "retry attempt out of range: {retry:?}"
            );
            saw_retry_note = true;
        }
        task.state == TaskState::Completed
    });
    assert!(
        saw_retry_note,
        "no snapshot ever published the segment retry"
    );
    let task = engine.task(id).unwrap();
    assert_eq!(
        std::fs::read(&task.final_path).unwrap(),
        body(),
        "downloaded bytes must be exact"
    );
    assert_eq!(
        server.region_hits.load(Ordering::SeqCst),
        3,
        "the stalled range must be requested three times: stall, stall, serve"
    );
    // The record's attempt counter is zeroed by completion, so it cannot
    // testify here; the engine log can. Recovery inside the transfer logs
    // "(segment retry N/M ...)"; the old attempt-level recovery logged
    // "(retry N/M ...)" at task level instead.
    let mut logs = Vec::new();
    loop {
        let batch = engine.drain_events(256);
        if batch.is_empty() {
            break;
        }
        for event in batch {
            if let Event::Log { message, .. } = event {
                logs.push(message);
            }
        }
    }
    let segment_retries = logs
        .iter()
        .filter(|m| m.contains("(segment retry "))
        .count();
    let task_retries = logs
        .iter()
        .filter(|m| m.contains("(retry ") && !m.contains("(segment retry "))
        .count();
    assert_eq!(
        segment_retries, 2,
        "both stalls must be recovered by segment retries; logs: {logs:?}"
    );
    assert_eq!(
        task_retries, 0,
        "no task-level attempt may be consumed; logs: {logs:?}"
    );
    engine.shutdown();
}

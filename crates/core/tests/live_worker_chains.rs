//! Usagi PR 5 slice 5b. Fault chains through the live download worker.
//! The seams are the ones named in the claim. A failing invariant is a
//! finding: these tests do not weaken it.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use usagi::engine::WorkerFault;
use usagi::store::{
    RepresentationBound, SegmentState, Store, TaskRecord, TaskState, SCHEMA_VERSION,
};
use usagi::{AddRequest, Engine, EngineBuilder, Paths, Settings, TaskSnapshot, TransportPref};

fn paths(root: &Path) -> Paths {
    Paths {
        root: root.into(),
        db_file: root.join("test.redb"),
        log_file: root.join("logs/test.log"),
        temp_dir: root.join("tmp"),
        default_download_dir: root.join("downloads"),
        portable: true,
        exe_dir: Some(root.into()),
    }
}

fn settings(root: &Path) -> Settings {
    Settings {
        http3_enabled: false,
        use_system_proxy: false,
        segments_per_task: 1,
        min_segment_bytes: 64 * 1024,
        max_retries: 0,
        verify_checksums: false,
        connect_timeout_secs: 3,
        stall_timeout_secs: 5,
        // Keep the background flush off the checkpoint assertions.
        persist_interval_ms: 60_000,
        default_download_dir: root.join("downloads").to_string_lossy().into(),
        ..Settings::default()
    }
}

fn start(root: &Path) -> Engine {
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    EngineBuilder::new()
        .paths(paths(root))
        .settings(settings(root))
        .build()
        .unwrap()
}

fn reopen(root: &Path) -> Engine {
    EngineBuilder::new()
        .paths(paths(root))
        .settings(settings(root))
        .idle(true)
        .build()
        .unwrap()
}

fn wait_terminal(engine: &Engine, id: u64) -> TaskSnapshot {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let task = engine.task(id).expect("task still exists");
        if task.state.is_terminal() {
            return task;
        }
        assert!(
            Instant::now() < deadline,
            "stuck in {} ({})",
            task.state.as_str(),
            task.error.clone().unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wait_fired(engine: &Engine, id: u64) -> TaskSnapshot {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let task = engine.task(id).expect("task still exists");
        if engine.worker_faults_fired() >= 1 && task.state.is_terminal() {
            return task;
        }
        assert!(
            Instant::now() < deadline,
            "fault fired {} time(s); state {} ({})",
            engine.worker_faults_fired(),
            task.state.as_str(),
            task.error.clone().unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        if key.eq_ignore_ascii_case(name) {
            Some(value.trim())
        } else {
            None
        }
    })
}

fn gets(requests: &[String]) -> Vec<String> {
    requests
        .iter()
        .filter(|request| request.starts_with("GET "))
        .cloned()
        .collect()
}

fn read_headers(stream: &mut TcpStream) -> String {
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .unwrap();
    stream.set_nodelay(true).unwrap();
    let mut request = Vec::new();
    while !request.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        if stream.read_exact(&mut byte).is_err() {
            break;
        }
        request.push(byte[0]);
        if request.len() > 16_384 {
            break;
        }
    }
    String::from_utf8_lossy(&request).into_owned()
}

fn reply(status: &str, headers: &str, body: &[u8]) -> Vec<u8> {
    let headers = if headers.is_empty() {
        String::new()
    } else {
        format!("{headers}\r\n")
    };
    let mut out = format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n").into_bytes();
    out.extend_from_slice(body);
    out
}

fn identity(request: &str, body: &[u8], etag: &str, extra: &str) -> Vec<u8> {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!("{extra}\r\n")
    };
    if request.starts_with("HEAD ") {
        return reply(
            "200 OK",
            &format!(
                "Accept-Ranges: bytes\r\nETag: {etag}\r\n{extra}Content-Length: {}",
                body.len()
            ),
            b"",
        );
    }
    let range = header(request, "range").unwrap_or("");
    let spec = range.strip_prefix("bytes=").unwrap_or("");
    if spec.is_empty() {
        return reply(
            "200 OK",
            &format!(
                "Accept-Ranges: bytes\r\nETag: {etag}\r\n{extra}Content-Length: {}",
                body.len()
            ),
            body,
        );
    }
    let (start, end) = spec.split_once('-').unwrap();
    let start: usize = start.parse().unwrap();
    let end: usize = if end.is_empty() {
        body.len() - 1
    } else {
        end.parse().unwrap()
    };
    let slice = &body[start..=end];
    reply(
        "206 Partial Content",
        &format!(
            "Accept-Ranges: bytes\r\nETag: {etag}\r\n{extra}Content-Range: bytes {start}-{end}/{}\r\nContent-Length: {}",
            body.len(),
            slice.len()
        ),
        slice,
    )
}

struct Loopback {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl Loopback {
    fn url(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/{name}", self.port)
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn spawn(handler: impl Fn(&str, &mut TcpStream) + Send + Sync + 'static) -> Loopback {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let join = std::thread::spawn(move || {
        let handler = Arc::new(handler);
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    if flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let request = read_headers(&mut stream);
                    if request.is_empty() {
                        continue;
                    }
                    recorded.lock().unwrap().push(request.clone());
                    handler(&request, &mut stream);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    Loopback {
        port,
        requests,
        stop,
        join: Some(join),
    }
}

fn spawn_body(body: Vec<u8>, etag: &'static str) -> Loopback {
    spawn(move |request, stream| {
        let bytes = identity(request, &body, etag, "");
        stream.write_all(&bytes).unwrap();
    })
}

struct Planted {
    url: String,
    filename: &'static str,
    total: u64,
    done: u64,
    prefix: Vec<u8>,
    etag: &'static str,
    restarts: u32,
}

fn plant(root: &Path, seed: &Planted) -> u64 {
    let downloads = root.join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let store = Store::open(root.join("test.redb")).unwrap();
    let id = store.next_task_id().unwrap();
    let mut record = TaskRecord::new(id, &seed.url, &downloads, Some(seed.filename.into()));
    record.state = TaskState::Paused;
    record.final_url = seed.url.clone();
    record.total_size = Some(seed.total);
    record.etag = Some(seed.etag.into());
    record.representation_restarts = seed.restarts;
    record.accept_ranges = true;
    record.segments_planned = 1;
    record.transport_pref = TransportPref::NoHttp3;
    record.representation_bound = Some(RepresentationBound::none_scope(
        seed.url.clone(),
        seed.etag,
        Some(seed.total),
    ));
    let mut segment = SegmentState::new(0, 0, seed.total - 1);
    segment.done = seed.done;
    let segments = vec![segment];
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(record.part_path())
        .unwrap();
    file.set_len(seed.total).unwrap();
    file.write_all(&seed.prefix).unwrap();
    file.sync_all().unwrap();
    record
        .prepare_progress_checkpoint(&segments, &file)
        .unwrap();
    store
        .save(&[record], &[(id, segments)], true)
        .unwrap();
    id
}

fn prefix_of(path: &Path, prefix: &[u8]) {
    let bytes = std::fs::read(path).expect("partial still present");
    assert!(
        bytes.len() >= prefix.len(),
        "partial shorter than the planted prefix ({} < {})",
        bytes.len(),
        prefix.len()
    );
    assert_eq!(&bytes[..prefix.len()], prefix, "planted prefix was overwritten");
}

fn assert_no_final(path: &Path) {
    assert!(
        !path.exists(),
        "unverified bytes were finalized at {}",
        path.display()
    );
}

fn wait_store_state(engine: &Engine, id: u64, state: TaskState) -> TaskRecord {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let stored = engine.store().get_task(id).unwrap().unwrap();
        if stored.state == state {
            return stored;
        }
        assert!(
            Instant::now() < deadline,
            "durable state stayed {} (wanted {})",
            stored.state.as_str(),
            state.as_str()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn durable_done(engine: &Engine, id: u64) -> u64 {
    let segments = engine.store().get_segments(id).unwrap();
    assert_eq!(segments.len(), 1, "coverage was split or dropped");
    segments[0].done
}

#[test]
fn schema_version_stays_2() {
    assert_eq!(SCHEMA_VERSION, 2);
}

#[test]
fn chain_416_mismatch_restarts_once_and_does_not_splice() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let new_body = b"NEW-REPRESENTATION".to_vec();
    let phase = Arc::new(AtomicU32::new(0));
    let body = new_body.clone();
    let server = spawn(move |request, stream| {
        let advanced = phase.load(Ordering::SeqCst) > 0;
        if request.starts_with("HEAD ") {
            let bytes = if advanced {
                reply(
                    "200 OK",
                    &format!(
                        "Accept-Ranges: bytes\r\nETag: \"v2\"\r\nContent-Length: {}",
                        body.len()
                    ),
                    b"",
                )
            } else {
                reply(
                    "200 OK",
                    "Accept-Ranges: bytes\r\nETag: \"v1\"\r\nContent-Length: 8",
                    b"",
                )
            };
            stream.write_all(&bytes).unwrap();
            return;
        }
        if !advanced {
            phase.store(1, Ordering::SeqCst);
            let bytes = reply(
                "416 Range Not Satisfiable",
                "Content-Range: bytes */16\r\nContent-Length: 0",
                b"",
            );
            stream.write_all(&bytes).unwrap();
            return;
        }
        let bytes = identity(request, &body, "\"v2\"", "");
        stream.write_all(&bytes).unwrap();
    });
    let url = server.url("switch.bin");
    let id = plant(
        root,
        &Planted {
            url: url.clone(),
            filename: "switch.bin",
            total: 8,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 0,
        },
    );
    let engine = start(root);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), new_body);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1);
    assert_eq!(stored.attempts, 0, "the 416 mismatch spent the network budget");
    assert_eq!(
        stored.representation_bound.as_ref().unwrap().strong_etag,
        "\"v2\""
    );
    assert!(!stored.part_path().exists(), "the old partial was kept beside the final");
    let ranges: Vec<_> = gets(&server.requests())
        .iter()
        .filter_map(|request| header(request, "range").map(str::to_string))
        .collect();
    assert!(
        ranges.iter().any(|range| range.starts_with("bytes=4-")),
        "the 416 was not the resume of the planted prefix: {ranges:?}"
    );
    engine.shutdown();
    drop(engine);

    let engine = reopen(root);
    let task = engine.task(id).unwrap();
    assert_eq!(task.state, TaskState::Completed);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), new_body);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1);
    assert!(
        engine.store().get_segments(id).unwrap().is_empty(),
        "reopen resurrected segment coverage"
    );
    engine.shutdown();
}

#[test]
fn chain_416_without_a_usable_total_keeps_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let server = spawn(|request, stream| {
        let bytes = if request.starts_with("HEAD ") {
            reply(
                "200 OK",
                "Accept-Ranges: bytes\r\nETag: \"v1\"\r\nContent-Length: 8",
                b"",
            )
        } else {
            reply(
                "416 Range Not Satisfiable",
                "Content-Range: bytes */*\r\nContent-Length: 0",
                b"",
            )
        };
        stream.write_all(&bytes).unwrap();
    });
    let id = plant(
        root,
        &Planted {
            url: server.url("nototal.bin"),
            filename: "nototal.bin",
            total: 8,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 0,
        },
    );
    let engine = start(root);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let error = task.error.unwrap_or_default();
    assert!(error.contains("416"), "{error}");
    let stored = wait_store_state(&engine, id, TaskState::Failed);
    assert_eq!(stored.representation_restarts, 0);
    assert_eq!(stored.attempts, 1);
    assert_eq!(durable_done(&engine, id), 4);
    prefix_of(&stored.part_path(), &[0x11; 4]);
    assert_no_final(&stored.final_path());
    assert_eq!(gets(&server.requests()).len(), 1);
    engine.shutdown();
}

#[test]
fn chain_416_matching_total_does_not_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let server = spawn(|request, stream| {
        let bytes = if request.starts_with("HEAD ") {
            reply(
                "200 OK",
                "Accept-Ranges: bytes\r\nETag: \"v1\"\r\nContent-Length: 8",
                b"",
            )
        } else {
            reply(
                "416 Range Not Satisfiable",
                "Content-Range: bytes */8\r\nContent-Length: 0",
                b"",
            )
        };
        stream.write_all(&bytes).unwrap();
    });
    let id = plant(
        root,
        &Planted {
            url: server.url("short.bin"),
            filename: "short.bin",
            total: 8,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 0,
        },
    );
    let engine = start(root);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let stored = wait_store_state(&engine, id, TaskState::Failed);
    assert_eq!(stored.representation_restarts, 0, "a matching 416 spent a restart");
    assert_eq!(stored.attempts, 1);
    assert_eq!(durable_done(&engine, id), 4);
    prefix_of(&stored.part_path(), &[0x11; 4]);
    assert_no_final(&stored.final_path());
    engine.shutdown();
}

#[test]
fn chain_416_second_mismatch_does_not_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let server = spawn(|request, stream| {
        let bytes = if request.starts_with("HEAD ") {
            reply(
                "200 OK",
                "Accept-Ranges: bytes\r\nETag: \"v1\"\r\nContent-Length: 8",
                b"",
            )
        } else {
            reply(
                "416 Range Not Satisfiable",
                "Content-Range: bytes */16\r\nContent-Length: 0",
                b"",
            )
        };
        stream.write_all(&bytes).unwrap();
    });
    let id = plant(
        root,
        &Planted {
            url: server.url("spent.bin"),
            filename: "spent.bin",
            total: 8,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 1,
        },
    );
    let engine = start(root);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let stored = wait_store_state(&engine, id, TaskState::Failed);
    assert_eq!(stored.representation_restarts, 1);
    assert_eq!(stored.attempts, 1);
    assert_eq!(
        stored.representation_bound.as_ref().unwrap().strong_etag,
        "\"v1\""
    );
    assert_eq!(durable_done(&engine, id), 4);
    prefix_of(&stored.part_path(), &[0x11; 4]);
    assert_no_final(&stored.final_path());
    assert_eq!(gets(&server.requests()).len(), 1, "a second restart downloaded");
    engine.shutdown();
}

#[test]
fn chain_encoding_mismatch_keeps_durable_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let server = spawn(|request, stream| {
        let bytes = if request.starts_with("HEAD ") {
            reply(
                "200 OK",
                "Accept-Ranges: bytes\r\nETag: \"v1\"\r\nContent-Length: 8",
                b"",
            )
        } else {
            reply(
                "206 Partial Content",
                "Content-Encoding: gzip\r\nETag: \"v1\"\r\nContent-Range: bytes 4-7/8\r\nContent-Length: 8",
                b"GZIPBODY",
            )
        };
        stream.write_all(&bytes).unwrap();
    });
    let id = plant(
        root,
        &Planted {
            url: server.url("coded.bin"),
            filename: "coded.bin",
            total: 8,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 0,
        },
    );
    let engine = start(root);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let error = task.error.unwrap_or_default();
    assert!(error.contains("encoded the body"), "{error}");
    assert_eq!(task.received, 4, "the encoded body was counted as coverage");
    let stored = wait_store_state(&engine, id, TaskState::Failed);
    assert_eq!(stored.representation_restarts, 0);
    assert_eq!(stored.attempts, 1);
    assert_eq!(durable_done(&engine, id), 4);
    prefix_of(&stored.part_path(), &[0x11; 4]);
    let partial = std::fs::read(stored.part_path()).unwrap();
    assert_ne!(&partial[4..8], b"GZIP");
    assert_no_final(&stored.final_path());
    engine.shutdown();
}

#[test]
fn chain_live_representation_restart_does_not_splice() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let body = vec![0x22; 8];
    let server = spawn_body(body.clone(), "\"v2\"");
    let url = server.url("changed.bin");
    let id = plant(
        root,
        &Planted {
            url,
            filename: "changed.bin",
            total: 8,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 0,
        },
    );
    let engine = start(root);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), body);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1);
    assert_eq!(stored.attempts, 0, "the live mismatch spent the network budget");
    assert_eq!(stored.representation_bound.as_ref().unwrap().strong_etag, "\"v2\"");
    assert_eq!(gets(&server.requests()).len(), 1, "the mismatch downloaded before the restart");
    engine.shutdown();
    drop(engine);

    let engine = reopen(root);
    let task = engine.task(id).unwrap();
    assert_eq!(task.state, TaskState::Completed);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), body);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1);
    assert!(engine.store().get_segments(id).unwrap().is_empty());
    engine.shutdown();
}

#[test]
fn chain_live_second_mismatch_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let server = spawn_body(vec![0x22; 8], "\"v2\"");
    let url = server.url("changed.bin");
    let id = plant(
        root,
        &Planted {
            url,
            filename: "changed.bin",
            total: 8,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 1,
        },
    );
    let engine = start(root);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let stored = wait_store_state(&engine, id, TaskState::Failed);
    assert_eq!(stored.representation_restarts, 1);
    assert_eq!(stored.attempts, 1);
    assert_eq!(stored.representation_bound.as_ref().unwrap().strong_etag, "\"v1\"");
    assert_eq!(durable_done(&engine, id), 4);
    prefix_of(&stored.part_path(), &[0x11; 4]);
    assert_no_final(&stored.final_path());
    assert!(gets(&server.requests()).is_empty(), "the spent budget still downloaded");
    engine.shutdown();
}

#[test]
fn chain_short_write_does_not_extend_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut body = vec![0x11; 4];
    body.extend(std::iter::repeat(0x22).take(12));
    let server = spawn_body(body.clone(), "\"v1\"");
    let url = server.url("shortwrite.bin");
    let id = plant(
        root,
        &Planted {
            url,
            filename: "shortwrite.bin",
            total: 16,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 0,
        },
    );
    let engine = start(root);
    engine.arm_worker_fault(WorkerFault::ShortWrite { accept: 1 });
    engine.resume(id).unwrap();
    let task = wait_fired(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let error = task.error.unwrap_or_default();
    assert!(error.contains("injected short write"), "{error}");
    let stored = wait_store_state(&engine, id, TaskState::Failed);
    assert_eq!(stored.representation_restarts, 0);
    assert_eq!(stored.attempts, 1);
    assert_eq!(engine.worker_faults_fired(), 1);
    assert_eq!(durable_done(&engine, id), 4);
    let partial = std::fs::read(stored.part_path()).unwrap();
    assert_eq!(&partial[..4], &[0x11; 4]);
    assert_eq!(partial[4], 0x22, "the accepted byte did not reach the file");
    assert_no_final(&stored.final_path());
    engine.shutdown();
}

#[test]
fn chain_enospc_keeps_prior_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let mut body = vec![0x11; 4];
    body.extend(std::iter::repeat(0x22).take(12));
    let server = spawn_body(body, "\"v1\"");
    let url = server.url("full.bin");
    let id = plant(
        root,
        &Planted {
            url,
            filename: "full.bin",
            total: 16,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 0,
        },
    );
    let engine = start(root);
    engine.arm_worker_fault(WorkerFault::StorageFull);
    engine.resume(id).unwrap();
    let task = wait_fired(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let error = task.error.unwrap_or_default();
    assert!(error.contains("injected ENOSPC"), "{error}");
    let stored = wait_store_state(&engine, id, TaskState::Failed);
    assert_eq!(stored.representation_restarts, 0);
    assert_eq!(stored.attempts, 1);
    assert_eq!(engine.worker_faults_fired(), 1);
    assert_eq!(durable_done(&engine, id), 4);
    let partial = std::fs::read(stored.part_path()).unwrap();
    assert_eq!(partial.len(), 16, "preallocation changed size past the plan");
    assert_eq!(&partial[..4], &[0x11; 4]);
    assert_eq!(partial[4], 0, "ENOSPC still stored a payload byte");
    assert_no_final(&stored.final_path());
    engine.shutdown();
}

/// A 206 whose chunked body is shorter than the Content-Range length.
/// `Content-Length` is omitted so the stack delivers a clean EOF and the
/// worker can flush the bytes it did accept before the early-end error.
fn spawn_short_body() -> Loopback {
    spawn(|request, stream| {
        let bytes = if request.starts_with("HEAD ") {
            reply(
                "200 OK",
                "Accept-Ranges: bytes\r\nETag: \"v1\"\r\nContent-Length: 8",
                b"",
            )
        } else {
            let mut out = b"HTTP/1.1 206 Partial Content\r\nAccept-Ranges: bytes\r\nETag: \"v1\"\r\nContent-Range: bytes 4-7/8\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
            out.extend_from_slice(b"2\r\n");
            out.extend_from_slice(&[0x22, 0x22]);
            out.extend_from_slice(b"\r\n0\r\n\r\n");
            out
        };
        stream.write_all(&bytes).unwrap();
    })
}

fn assert_advanced_snapshot_was_not_promoted(engine: &Engine, id: u64) {
    let task = wait_fired(engine, id);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let error = task.error.unwrap_or_default();
    assert!(
        error.contains("ended early"),
        "the worker did not flush a short body and then stop: {error}"
    );
    assert_eq!(task.received, 6, "session coverage was not the two new bytes");
    assert_eq!(engine.worker_faults_fired(), 1);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    // The probing save already committed Downloading with the planted prefix.
    // The failed advanced prepare must not replace that row.
    assert_eq!(
        stored.state,
        TaskState::Downloading,
        "the failed commit replaced the durable state ({})",
        stored.state.as_str()
    );
    assert_eq!(stored.representation_restarts, 0);
    assert_eq!(durable_done(engine, id), 4);
    prefix_of(&stored.part_path(), &[0x11; 4]);
    assert_no_final(&stored.final_path());
}

#[test]
fn chain_checkpoint_sync_failure_does_not_promote() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let server = spawn_short_body();
    let url = server.url("sync.bin");
    let id = plant(
        root,
        &Planted {
            url,
            filename: "sync.bin",
            total: 8,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 0,
        },
    );
    let engine = start(root);
    engine.arm_worker_fault(WorkerFault::CheckpointSync { min_done: 5 });
    engine.resume(id).unwrap();
    assert_advanced_snapshot_was_not_promoted(&engine, id);
    engine.shutdown();
    assert_no_final(&root.join("downloads").join("sync.bin"));
}

#[test]
fn chain_checkpoint_store_failure_does_not_promote() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let server = spawn_short_body();
    let url = server.url("store.bin");
    let id = plant(
        root,
        &Planted {
            url,
            filename: "store.bin",
            total: 8,
            done: 4,
            prefix: vec![0x11; 4],
            etag: "\"v1\"",
            restarts: 0,
        },
    );
    let engine = start(root);
    engine.arm_worker_fault(WorkerFault::CheckpointStore { min_done: 5 });
    engine.resume(id).unwrap();
    assert_advanced_snapshot_was_not_promoted(&engine, id);
    engine.shutdown();
    assert_no_final(&root.join("downloads").join("store.bin"));
}

#[test]
fn chain_finish_sync_failure_does_not_finalize() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let body = vec![0x42; 32];
    let server = spawn_body(body.clone(), "\"full\"");
    let engine = start(root);
    engine.arm_worker_fault(WorkerFault::FinishSync);
    let id = engine
        .add(
            AddRequest::new(server.url("full.bin"))
                .dest_dir(root.join("downloads"))
                .filename("full.bin")
                .segments(1)
                .transport_pref(TransportPref::NoHttp3),
        )
        .unwrap();
    let task = wait_fired(&engine, id);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let error = task.error.unwrap_or_default();
    assert!(error.contains("injected disk sync failure"), "{error}");
    let stored = wait_store_state(&engine, id, TaskState::Failed);
    assert_eq!(stored.state, TaskState::Failed);
    assert_eq!(stored.representation_restarts, 0);
    assert_eq!(stored.attempts, 1);
    assert!(stored.computed_hash.is_none(), "a failed finish sync published a hash");
    assert_eq!(engine.worker_faults_fired(), 1);
    assert_eq!(std::fs::read(stored.part_path()).unwrap(), body);
    assert_no_final(&stored.final_path());
    engine.shutdown();
    drop(engine);

    let engine = reopen(root);
    let task = engine.task(id).unwrap();
    assert_eq!(task.state, TaskState::Failed);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert!(stored.computed_hash.is_none());
    assert_eq!(std::fs::read(stored.part_path()).unwrap(), body);
    assert_no_final(Path::new(&task.final_path));
    engine.shutdown();
}

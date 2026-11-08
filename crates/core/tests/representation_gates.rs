//! Usagi PR 4 slice 4b. Public-engine proof of the representation gates.
//! `run_segment` itself is covered in `engine/representation_tests.rs`.

#[path = "common/mod.rs"]
mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sha2::Digest;
use udm_core::store::{
    RepresentationBound, SegmentState, Store, TaskRecord, TaskState, OPEN_ENDED, SCHEMA_VERSION,
};
use udm_core::{AddRequest, Engine, EngineBuilder, Paths, Settings, TaskSnapshot, TransportPref};

use common::dev_tls::HarnessCa;

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

fn settings(root: &Path, http3: bool) -> Settings {
    Settings {
        http3_enabled: http3,
        use_system_proxy: false,
        segments_per_task: 8,
        min_segment_bytes: 64 * 1024,
        max_retries: 0,
        verify_checksums: false,
        connect_timeout_secs: 3,
        stall_timeout_secs: 5,
        default_download_dir: root.join("downloads").to_string_lossy().into(),
        ..Settings::default()
    }
}

fn start(root: &Path, http3: bool) -> Engine {
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    EngineBuilder::new()
        .paths(paths(root))
        .settings(settings(root, http3))
        .build()
        .unwrap()
}

fn wait_terminal(engine: &Engine, id: u64, secs: u64) -> TaskSnapshot {
    let deadline = Instant::now() + Duration::from_secs(secs);
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

fn sha256(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
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
    stream.set_read_timeout(Some(Duration::from_secs(8))).unwrap();
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
    // Callers own `Content-Length`. A HEAD response reports the resource
    // length while this body slice stays empty, so it must not be replaced
    // with the length of `body`.
    let mut out = format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n").into_bytes();
    out.extend_from_slice(body);
    out
}

/// HEAD describes `body`. A ranged GET returns that slice and echoes `etag`.
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
            &format!("Accept-Ranges: bytes\r\nETag: {etag}\r\n{extra}Content-Length: {}", body.len()),
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

fn spawn_body(body: Vec<u8>, etag: &'static str, extra: &'static str) -> Loopback {
    spawn(move |request, stream| {
        let bytes = identity(request, &body, etag, extra);
        stream.write_all(&bytes).unwrap();
    })
}

struct Planted {
    url: String,
    filename: &'static str,
    total: Option<u64>,
    done: u64,
    end: u64,
    prefix: Vec<u8>,
    etag: Option<String>,
    bound_etag: Option<String>,
    bound_total: Option<u64>,
    bound_url: Option<String>,
    restarts: u32,
    attempts: u32,
    state: TaskState,
    last_modified: Option<String>,
    content_type: Option<String>,
    pref: TransportPref,
}

impl Planted {
    fn paused(url: &str) -> Self {
        Self {
            url: url.to_string(),
            filename: "payload.bin",
            total: None,
            done: 0,
            end: 0,
            prefix: Vec::new(),
            etag: None,
            bound_etag: None,
            bound_total: None,
            bound_url: None,
            restarts: 0,
            attempts: 0,
            state: TaskState::Paused,
            last_modified: None,
            content_type: None,
            pref: TransportPref::NoHttp3,
        }
    }
}

fn plant(root: &Path, seed: Planted) -> u64 {
    let downloads = root.join("downloads");
    std::fs::create_dir_all(&downloads).unwrap();
    let store = Store::open(root.join("test.redb")).unwrap();
    let id = store.next_task_id().unwrap();
    let mut record = TaskRecord::new(id, &seed.url, &downloads, Some(seed.filename.into()));
    record.state = seed.state;
    record.final_url = seed.url.clone();
    record.total_size = seed.total;
    record.etag = seed.etag;
    record.last_modified = seed.last_modified;
    record.content_type = seed.content_type;
    record.attempts = seed.attempts;
    record.representation_restarts = seed.restarts;
    record.accept_ranges = true;
    record.segments_planned = 8;
    record.transport_pref = seed.pref;
    if let Some(etag) = seed.bound_etag {
        record.representation_bound = Some(RepresentationBound::none_scope(
            seed.bound_url.unwrap_or_else(|| record.final_url.clone()),
            etag,
            seed.bound_total,
        ));
    }
    let mut segment = SegmentState::new(0, 0, seed.end);
    segment.done = seed.done;
    let segments = if seed.done == 0 && seed.prefix.is_empty() && seed.total.is_none() {
        Vec::new()
    } else {
        vec![segment]
    };
    if seed.done > 0 || !seed.prefix.is_empty() {
        let mut file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(record.part_path())
            .unwrap();
        let len = seed.total.unwrap_or(seed.done).max(seed.done).max(seed.prefix.len() as u64);
        file.set_len(len).unwrap();
        file.write_all(&seed.prefix).unwrap();
        file.sync_all().unwrap();
        if seed.done > 0 {
            record.prepare_progress_checkpoint(&segments, &file).unwrap();
        }
    }
    store.save(std::slice::from_ref(&record), &[(id, segments)], true).unwrap();
    id
}

fn download(root: &Path, url: &str, filename: &str) -> (Engine, u64) {
    let engine = start(root, false);
    let id = engine
        .add(
            AddRequest::new(url)
                .dest_dir(root.join("downloads"))
                .filename(filename)
                .segments(8)
                .transport_pref(TransportPref::NoHttp3),
        )
        .unwrap();
    (engine, id)
}

#[test]
fn schema_version_stays_2() {
    assert_eq!(SCHEMA_VERSION, 2);
}

#[test]
fn unbound_accept_ranges_plans_one_segment_and_one_worker() {
    let dir = tempfile::tempdir().unwrap();
    let body = vec![0x5Au8; 8 * 64 * 1024];
    let server = spawn(move |request, stream| {
        let is_head = request.starts_with("HEAD ");
        let payload: &[u8] = if is_head { b"" } else { &body };
        let length = body.len();
        let bytes = reply(
            "200 OK",
            &format!("Accept-Ranges: bytes\r\nContent-Length: {length}"),
            payload,
        );
        stream.write_all(&bytes).unwrap();
    });
    let (engine, id) = download(dir.path(), &server.url("wide.bin"), "wide.bin");
    let task = wait_terminal(&engine, id, 15);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    let file = std::fs::read(&task.final_path).unwrap();
    assert_eq!(sha256(&file), sha256(&vec![0x5Au8; 8 * 64 * 1024]));
    let gets = gets(&server.requests());
    assert_eq!(gets.len(), 1, "unbound Accept-Ranges started more than one worker: {gets:?}");
    assert!(
        gets[0].to_ascii_lowercase().contains("range: bytes=0-"),
        "the single worker did not cover the file from offset 0: {}",
        gets[0]
    );
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert!(stored.representation_bound.is_none());
    assert_eq!(stored.segments_planned, 1);
    assert_eq!(stored.representation_restarts, 0);
    engine.shutdown();
}

#[test]
fn weak_etag_and_last_modified_are_not_if_range() {
    let dir = tempfile::tempdir().unwrap();
    let body = vec![0x11u8; 2 * 64 * 1024];
    let server = spawn_body(body.clone(), "W/\"abc\"", "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT");
    let (engine, id) = download(dir.path(), &server.url("weak.bin"), "weak.bin");
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), body);
    let gets = gets(&server.requests());
    assert_eq!(gets.len(), 1, "{gets:?}");
    let lower = gets[0].to_ascii_lowercase();
    assert!(!lower.contains("if-range"), "weak tag or Last-Modified was sent as If-Range: {}", gets[0]);
    assert!(!gets[0].contains("Wed, 21 Oct 2015"));
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert!(stored.representation_bound.is_none());
    assert_eq!(stored.segments_planned, 1);
    engine.shutdown();
}

#[test]
fn vary_user_agent_does_not_bind() {
    let dir = tempfile::tempdir().unwrap();
    let body = vec![0x22u8; 2 * 64 * 1024];
    let server = spawn_body(body.clone(), "\"strong\"", "Vary: User-Agent");
    let (engine, id) = download(dir.path(), &server.url("vary.bin"), "vary.bin");
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), body);
    assert_eq!(gets(&server.requests()).len(), 1);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert!(stored.representation_bound.is_none(), "Vary: User-Agent published a bound");
    assert_eq!(stored.segments_planned, 1);
    let seen = server.requests();
    let get = &gets(&seen)[0];
    assert!(!get.to_ascii_lowercase().contains("if-range"));
    engine.shutdown();
}

#[test]
fn gzip_on_the_segment_response_is_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn(|request, stream| {
        if request.starts_with("HEAD ") {
            let bytes = reply(
                "200 OK",
                "Accept-Ranges: bytes\r\nETag: \"plain\"\r\nContent-Length: 8",
                b"",
            );
            stream.write_all(&bytes).unwrap();
            return;
        }
        let bytes = reply(
            "200 OK",
            "Content-Encoding: gzip\r\nETag: \"plain\"\r\nContent-Length: 8",
            b"GZIPBODY",
        );
        stream.write_all(&bytes).unwrap();
    });
    let (engine, id) = download(dir.path(), &server.url("coded.bin"), "coded.bin");
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Failed, "gzip must not be stored");
    let error = task.error.unwrap_or_default();
    assert!(error.contains("encoded the body"), "{error}");
    assert_eq!(task.received, 0);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 0, "coding must not spend the restart budget");
    let part = stored.part_path();
    if part.exists() {
        let bytes = std::fs::read(&part).unwrap();
        assert!(!bytes.windows(8).any(|window| window == b"GZIPBODY"), "gzip body was written");
    }
    engine.shutdown();
}

#[test]
fn none_to_some_before_any_flushed_byte_upgrades() {
    let dir = tempfile::tempdir().unwrap();
    let body = b"upgrade-me".to_vec();
    let server = spawn(move |request, stream| {
        let range = header(request, "range").unwrap_or("");
        let bytes = if request.starts_with("HEAD ") {
            reply("200 OK", "ETag: \"v\"", b"")
        } else if range == "bytes=0-0" {
            // No Content-Length: `*` is not a length, so the probe stays unbound
            // at `None` and the later full response is the None→Some upgrade.
            reply(
                "206 Partial Content",
                "ETag: \"v\"\r\nAccept-Ranges: bytes\r\nContent-Range: bytes 0-0/*",
                b"u",
            )
        } else {
            reply(
                "200 OK",
                &format!("ETag: \"v\"\r\nContent-Length: {}", body.len()),
                &body,
            )
        };
        stream.write_all(&bytes).unwrap();
    });
    let (engine, id) = download(dir.path(), &server.url("grow.bin"), "grow.bin");
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), b"upgrade-me");
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 0, "the initial upgrade must not restart");
    let bound = stored.representation_bound.expect("a strong tag binds");
    assert_eq!(bound.total_size, Some(b"upgrade-me".len() as u64));
    assert_eq!(bound.strong_etag, "\"v\"");
    assert_eq!(bound.header_scope, "none");
    engine.shutdown();
}

#[test]
fn none_to_some_after_a_flushed_byte_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let body = vec![0x22; 8];
    let server = spawn_body(body.clone(), "\"v\"", "");
    let url = server.url("grew.bin");
    let mut seed = Planted::paused(&url);
    seed.done = 3;
    seed.end = OPEN_ENDED;
    seed.total = None;
    seed.prefix = vec![0xFF; 3];
    seed.etag = Some("\"v\"".into());
    seed.bound_etag = Some("\"v\"".into());
    seed.bound_total = None;
    let id = plant(dir.path(), seed);
    let engine = start(dir.path(), false);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), body);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1);
    assert_eq!(stored.representation_bound.unwrap().total_size, Some(8));
    let seen = server.requests();
    let ranges: Vec<_> = gets(&seen)
        .iter()
        .filter_map(|request| header(request, "range").map(str::to_string))
        .collect();
    assert!(
        ranges.iter().any(|range| range.starts_with("bytes=0")),
        "the flushed None→Some mismatch must restart from 0, saw {ranges:?}"
    );
    engine.shutdown();
}

#[test]
fn legacy_none_restarts_from_zero_despite_a_stored_etag() {
    let dir = tempfile::tempdir().unwrap();
    let body = b"brand-new".to_vec();
    let server = spawn_body(body.clone(), "\"stored\"", "Last-Modified: Wed, 21 Oct 2015 07:28:00 GMT");
    let url = server.url("legacy.bin");
    let mut seed = Planted::paused(&url);
    seed.total = Some(body.len() as u64);
    seed.done = 4;
    seed.end = body.len() as u64 - 1;
    seed.prefix = vec![0xFF; 4];
    seed.etag = Some("\"stored\"".into());
    seed.last_modified = Some("Wed, 21 Oct 2015 07:28:00 GMT".into());
    let id = plant(dir.path(), seed);
    let engine = start(dir.path(), false);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), body, "legacy progress was appended");
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 0, "legacy discard is not the mismatch budget");
    let seen = server.requests();
    let ranges: Vec<_> = gets(&seen)
        .iter()
        .filter_map(|request| header(request, "range").map(str::to_string))
        .collect();
    assert!(
        ranges.iter().all(|range| range.starts_with("bytes=0")),
        "a stored strong ETag resumed the old offset: {ranges:?}"
    );
    engine.shutdown();
}

#[test]
fn bound_partial_resumes_only_the_flushed_range() {
    let dir = tempfile::tempdir().unwrap();
    let body = b"01234567".to_vec();
    let server = spawn_body(body.clone(), "\"v\"", "Content-Type: application/octet-stream");
    let url = server.url("resume.bin");
    let mut seed = Planted::paused(&url);
    seed.total = Some(8);
    seed.done = 4;
    seed.end = 7;
    seed.prefix = b"0123".to_vec();
    seed.etag = Some("\"v\"".into());
    seed.bound_etag = Some("\"v\"".into());
    seed.bound_total = Some(8);
    seed.content_type = Some("image/png".into());
    let id = plant(dir.path(), seed);
    let engine = start(dir.path(), false);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), body);
    assert_eq!(sha256(&body), sha256(b"01234567"));
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 0);
    assert_eq!(stored.representation_bound.unwrap().strong_etag, "\"v\"");
    let get = gets(&server.requests());
    let ranged = get.iter().find(|request| header(request, "range").is_some()).expect("range");
    assert_eq!(header(ranged, "range"), Some("bytes=4-7"));
    assert_eq!(header(ranged, "if-range"), Some("\"v\""));
    assert!(!ranged.to_ascii_lowercase().contains("if-range: wed"));
    engine.shutdown();
}

#[test]
fn http_200_at_a_nonzero_offset_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn(|request, stream| {
        let bytes = if request.starts_with("HEAD ") {
            reply(
                "200 OK",
                "Accept-Ranges: bytes\r\nETag: \"v\"\r\nContent-Length: 8",
                b"",
            )
        } else {
            reply("200 OK", "ETag: \"v\"\r\nContent-Length: 8", &[0x22; 8])
        };
        stream.write_all(&bytes).unwrap();
    });
    let url = server.url("whole.bin");
    let mut seed = Planted::paused(&url);
    seed.total = Some(8);
    seed.done = 4;
    seed.end = 7;
    seed.prefix = vec![0x11; 4];
    seed.etag = Some("\"v\"".into());
    seed.bound_etag = Some("\"v\"".into());
    seed.bound_total = Some(8);
    seed.restarts = 1;
    let id = plant(dir.path(), seed);
    let engine = start(dir.path(), false);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let error = task.error.unwrap_or_default();
    assert!(error.contains("ignored the Range"), "{error}");
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1, "200 at a non-zero offset is not a second restart");
    let part = std::fs::read(stored.part_path()).unwrap();
    assert_eq!(&part[..4], &[0x11; 4], "the 200 body was written at the old offset");
    assert!(part[4..].iter().all(|byte| *byte == 0), "the 200 body was appended");
    assert_eq!(gets(&server.requests()).len(), 1, "the spent budget restarted anyway");
    engine.shutdown();
}

#[test]
fn partial_206_wrong_start_writes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn(|request, stream| {
        let bytes = if request.starts_with("HEAD ") {
            reply(
                "200 OK",
                "Accept-Ranges: bytes\r\nETag: \"v\"\r\nContent-Length: 8",
                b"",
            )
        } else {
            reply(
                "206 Partial Content",
                "ETag: \"v\"\r\nContent-Range: bytes 1-7/8\r\nContent-Length: 7",
                &[0xAB; 7],
            )
        };
        stream.write_all(&bytes).unwrap();
    });
    let (engine, id) = download(dir.path(), &server.url("shifted.bin"), "shifted.bin");
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    assert_eq!(task.received, 0);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 0);
    let part = std::fs::read(stored.part_path()).unwrap_or_default();
    assert!(!part.contains(&0xAB), "the mis-ranged body was written");
    engine.shutdown();
}

#[test]
fn cancel_after_flushed_bytes_resumes_and_matches_hash() {
    let dir = tempfile::tempdir().unwrap();
    let body: Vec<u8> = (0u8..64).collect();
    let payload = body.clone();
    let server = spawn(move |request, stream| {
        if request.starts_with("HEAD ") {
            let bytes = reply(
                "200 OK",
                "Accept-Ranges: bytes\r\nETag: \"v\"\r\nContent-Length: 64",
                b"",
            );
            stream.write_all(&bytes).unwrap();
            return;
        }
        let start = header(request, "range")
            .and_then(|range| range.strip_prefix("bytes="))
            .and_then(|spec| spec.split_once('-'))
            .and_then(|(start, _)| start.parse::<usize>().ok())
            .unwrap_or(0);
        if start >= 16 {
            let slice = &payload[start..];
            let end = payload.len() - 1;
            let bytes = reply(
                "206 Partial Content",
                &format!(
                    "ETag: \"v\"\r\nContent-Range: bytes {start}-{end}/{}\r\nContent-Length: {}",
                    payload.len(),
                    slice.len()
                ),
                slice,
            );
            stream.write_all(&bytes).unwrap();
            return;
        }
        // Announce the full slice, then deliver only the flushed prefix and wait
        // until cancel closes the socket.
        let prefix = &payload[..16];
        let head = "HTTP/1.1 206 Partial Content\r\nETag: \"v\"\r\nContent-Range: bytes 0-63/64\r\nContent-Length: 64\r\nConnection: close\r\n\r\n";
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(prefix).unwrap();
        stream.flush().unwrap();
        let mut sink = [0u8; 64];
        let _ = stream.read(&mut sink);
    });
    let (engine, id) = download(dir.path(), &server.url("cancel.bin"), "cancel.bin");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let task = engine.task(id).unwrap();
        if task.received >= 16 {
            break;
        }
        assert!(Instant::now() < deadline, "cancel fixture never flushed 16 bytes ({})", task.state.as_str());
        std::thread::sleep(Duration::from_millis(10));
    }
    engine.cancel(id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let task = engine.task(id).unwrap();
        let segments = engine.store().get_segments(id).unwrap();
        if task.state == TaskState::Canceled
            && segments.first().is_some_and(|segment| segment.done == 16 && !segment.finished)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancel left state {} segments {segments:?}",
            task.state.as_str()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    engine.resume(id).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let task = engine.task(id).unwrap();
        if task.state != TaskState::Canceled {
            break;
        }
        assert!(Instant::now() < deadline, "resume left the task Canceled");
        std::thread::sleep(Duration::from_millis(20));
    }
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    let file = std::fs::read(&task.final_path).unwrap();
    assert_eq!(file, body);
    assert_eq!(sha256(&file), sha256(&body));
    let seen = server.requests();
    let ranges: Vec<_> = gets(&seen)
        .iter()
        .filter_map(|request| header(request, "range").map(str::to_string))
        .collect();
    assert!(
        ranges.iter().any(|range| range.starts_with("bytes=16-")),
        "resume did not continue from the flushed offset: {ranges:?}"
    );
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 0);
    engine.shutdown();
}

#[test]
fn first_mismatch_restarts_once_and_does_not_use_the_network_budget() {
    let dir = tempfile::tempdir().unwrap();
    let body = vec![0x22; 8];
    let server = spawn_body(body.clone(), "\"v2\"", "");
    let url = server.url("changed.bin");
    let mut seed = Planted::paused(&url);
    seed.total = Some(8);
    seed.done = 4;
    seed.end = 7;
    seed.prefix = vec![0x11; 4];
    seed.etag = Some("\"v1\"".into());
    seed.bound_etag = Some("\"v1\"".into());
    seed.bound_total = Some(8);
    let id = plant(dir.path(), seed);
    let engine = start(dir.path(), false);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(
        task.state,
        TaskState::Completed,
        "max_retries is 0; a mismatch must still restart once ({:?})",
        task.error
    );
    assert_eq!(std::fs::read(&task.final_path).unwrap(), body);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1);
    assert_eq!(stored.representation_bound.unwrap().strong_etag, "\"v2\"");
    engine.shutdown();
}

#[test]
fn second_mismatch_fails_without_another_restart() {
    let dir = tempfile::tempdir().unwrap();
    let server = spawn_body(vec![0x22; 8], "\"v2\"", "");
    let url = server.url("changed.bin");
    let mut seed = Planted::paused(&url);
    seed.total = Some(8);
    seed.done = 4;
    seed.end = 7;
    seed.prefix = vec![0x11; 4];
    seed.etag = Some("\"v1\"".into());
    seed.bound_etag = Some("\"v1\"".into());
    seed.bound_total = Some(8);
    seed.restarts = 1;
    let id = plant(dir.path(), seed);
    let engine = start(dir.path(), false);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Failed, "{:?}", task.error);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1, "the second mismatch must not spend another restart");
    assert_eq!(
        stored.representation_bound.as_ref().unwrap().strong_etag,
        "\"v1\""
    );
    assert!(gets(&server.requests()).is_empty(), "the failed mismatch still downloaded");
    let part = std::fs::read(stored.part_path()).unwrap();
    assert_eq!(&part[..4], &[0x11; 4]);
    engine.shutdown();
}

#[test]
fn quote_only_etag_difference_is_a_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let body = vec![0x22; 8];
    let server = spawn_body(body.clone(), "abc", "");
    let url = server.url("quotes.bin");
    let mut seed = Planted::paused(&url);
    seed.total = Some(8);
    seed.done = 2;
    seed.end = 7;
    seed.prefix = vec![0xFF; 2];
    seed.etag = Some("\"abc\"".into());
    seed.bound_etag = Some("\"abc\"".into());
    seed.bound_total = Some(8);
    let id = plant(dir.path(), seed);
    let engine = start(dir.path(), false);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), body, "quote-only tags were treated as the same validator");
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1);
    assert_eq!(stored.representation_bound.unwrap().strong_etag, "abc");
    engine.shutdown();
}

#[test]
fn query_in_the_final_url_is_part_of_the_bound() {
    let dir = tempfile::tempdir().unwrap();
    let body = b"query-bound".to_vec();
    let server = spawn_body(body.clone(), "\"v\"", "");
    let url = server.url("q.bin?token=secret");
    let mut seed = Planted::paused(&url);
    seed.total = Some(body.len() as u64);
    seed.done = 2;
    seed.end = body.len() as u64 - 1;
    seed.prefix = vec![0xFF; 2];
    seed.etag = Some("\"v\"".into());
    seed.bound_etag = Some("\"v\"".into());
    seed.bound_total = Some(body.len() as u64);
    seed.bound_url = Some(server.url("q.bin?token=other"));
    let id = plant(dir.path(), seed);
    let engine = start(dir.path(), false);
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id, 10);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(std::fs::read(&task.final_path).unwrap(), body);
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1, "a different query was treated as the same representation");
    assert!(stored.representation_bound.unwrap().final_url.contains("token=secret"));
    engine.shutdown();
}

#[test]
fn resume_does_not_zero_representation_restarts_and_retry_does() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let url = "http://127.0.0.1:1/closed.bin";
    let mut seed = Planted::paused(url);
    seed.filename = "closed.bin";
    seed.state = TaskState::Failed;
    seed.restarts = 1;
    seed.attempts = 4;
    let id = plant(root, seed);
    let engine = start(root, false);
    engine.resume(id).unwrap();
    let stored = wait_budget(&engine, id, 5, 1);
    assert_eq!(stored.representation_restarts, 1, "resume zeroed the representation budget");
    assert_eq!(stored.attempts, 5, "resume reset the network attempt counter");
    engine.shutdown();
    drop(engine);

    let engine = start(root, false);
    engine.retry(id).unwrap();
    let stored = wait_budget(&engine, id, 1, 0);
    assert_eq!(stored.representation_restarts, 0, "retry must zero the representation budget");
    assert_eq!(stored.attempts, 1, "retry zeroes attempts in the same write, then the failed run records one");
    engine.shutdown();
}

fn wait_budget(engine: &Engine, id: u64, attempts: u32, restarts: u32) -> TaskRecord {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let task = engine.task(id).unwrap();
        let stored = engine.store().get_task(id).unwrap().unwrap();
        if task.state == TaskState::Failed && stored.attempts == attempts && stored.representation_restarts == restarts
        {
            return stored;
        }
        assert!(
            Instant::now() < deadline,
            "budget stayed attempts={} restarts={} state={}",
            stored.attempts,
            stored.representation_restarts,
            task.state.as_str()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn h3_fallback_keeps_the_bound_and_the_partial() {
    let ca = HarnessCa::generate().expect("harness CA");
    let body: Vec<u8> = (0u8..32).collect();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = ca.server_config().expect("server config");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let payload = body.clone();
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let acceptor = tokio_rustls::TlsAcceptor::from(config);
            while !flag.load(Ordering::SeqCst) {
                let accepted = tokio::time::timeout(Duration::from_millis(50), listener.accept()).await;
                let Ok(Ok((tcp, _))) = accepted else {
                    continue;
                };
                let Ok(mut tls) = acceptor.accept(tcp).await else {
                    continue;
                };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    match tokio::io::AsyncReadExt::read(&mut tls, &mut byte).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {
                            request.push(byte[0]);
                            if request.ends_with(b"\r\n\r\n") || request.len() > 16_384 {
                                break;
                            }
                        }
                    }
                }
                let request = String::from_utf8_lossy(&request).into_owned();
                if request.is_empty() {
                    continue;
                }
                recorded.lock().unwrap().push(request.clone());
                let response = identity(&request, &payload, "\"h3\"", "");
                let _ = tokio::io::AsyncWriteExt::write_all(&mut tls, &response).await;
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut tls).await;
            }
        });
    });

    let dir = tempfile::tempdir().unwrap();
    let url = format!("https://localhost:{port}/partial.bin");
    let mut seed = Planted::paused(&url);
    seed.filename = "partial.bin";
    seed.total = Some(body.len() as u64);
    seed.done = 8;
    seed.end = body.len() as u64 - 1;
    seed.prefix = body[..8].to_vec();
    seed.etag = Some("\"h3\"".into());
    seed.bound_etag = Some("\"h3\"".into());
    seed.bound_total = Some(body.len() as u64);
    seed.restarts = 1;
    seed.pref = TransportPref::PreferHttp3;
    let id = plant(dir.path(), seed);
    let engine = start(dir.path(), true);
    engine
        .net()
        .install_test_root_der(ca.ca_der())
        .expect("test root");
    engine.resume(id).unwrap();
    let task = wait_terminal(&engine, id, 25);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    let file = std::fs::read(&task.final_path).unwrap();
    assert_eq!(file, body);
    assert_eq!(sha256(&file), sha256(&body));
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.representation_restarts, 1, "H3 fallback zeroed the representation budget");
    assert_eq!(stored.representation_bound.unwrap().strong_etag, "\"h3\"");
    let seen = requests.lock().unwrap().clone();
    let ranges: Vec<_> = gets(&seen)
        .iter()
        .filter_map(|request| header(request, "range").map(str::to_string))
        .collect();
    assert!(
        ranges.iter().any(|range| range.starts_with("bytes=8-")),
        "HTTP fallback did not continue the same partial: {ranges:?}"
    );
    engine.shutdown();
    stop.store(true, Ordering::SeqCst);
    let _ = TcpStream::connect(("127.0.0.1", port));
    let _ = server.join();
}

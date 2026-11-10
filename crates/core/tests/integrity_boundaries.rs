//! Correctness gates for malformed responses, retry budgets and legacy partial ownership.
use std::{
    fs,
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use usagi::{
    config::Settings,
    engine::{AddRequest, Engine, EngineBuilder, TaskSnapshot},
    paths::Paths,
    store::{RepresentationBound, SegmentState, Store, TaskRecord, TaskState},
};

struct Reply {
    bytes: Vec<u8>,
    tail: Option<Vec<u8>>,
    tail_delay: Duration,
}

fn reply(status: &str, headers: &str, body: &[u8]) -> Reply {
    let mut bytes = format!("HTTP/1.1 {status}\r\n{headers}Connection: close\r\n\r\n").into_bytes();
    bytes.extend_from_slice(body);
    Reply {
        bytes,
        tail: None,
        tail_delay: Duration::from_millis(300),
    }
}

fn head(size: usize) -> Reply {
    reply(
        "200 OK",
        &format!("Content-Length: {size}\r\nAccept-Ranges: bytes\r\n"),
        b"",
    )
}

fn head_bound(size: usize) -> Reply {
    reply(
        "200 OK",
        &format!("Content-Length: {size}\r\nAccept-Ranges: bytes\r\nETag: \"v\"\r\n"),
        b"",
    )
}

fn serve(replies: Vec<Reply>) -> (String, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
    let worker = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut requests = Vec::new();
        for response in replies {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "test server timed out");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("accept: {e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                assert_eq!(stream.read(&mut byte).unwrap(), 1);
                request.push(byte[0]);
                assert!(request.len() < 65536);
            }
            requests.push(String::from_utf8(request).unwrap());
            // Early client rejection is expected for malformed headers/bodies.
            let _ = stream.write_all(&response.bytes);
            if let Some(tail) = response.tail {
                let _ = stream.flush();
                thread::sleep(response.tail_delay);
                let _ = stream.write_all(&tail);
            }
        }
        requests
    });
    (url, worker)
}

fn engine(root: &Path, retries: u32) -> Engine {
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
        http3_enabled: false,
        use_system_proxy: false,
        max_retries: retries,
        segments_per_task: 1,
        default_download_dir: root.join("downloads").to_string_lossy().into(),
        ..Settings::default()
    };
    EngineBuilder::new()
        .paths(paths)
        .settings(settings)
        .build()
        .unwrap()
}

fn wait(engine: &Engine, id: u64) -> TaskSnapshot {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let task = engine.task(id).unwrap();
        if task.state.is_terminal() {
            return task;
        }
        assert!(
            Instant::now() < deadline,
            "task timed out: {:?}",
            task.state
        );
        thread::sleep(Duration::from_millis(10));
    }
}

fn seed(root: &Path, url: &str, copies: usize) -> Vec<u64> {
    let downloads = root.join("downloads");
    fs::create_dir_all(&downloads).unwrap();
    fs::write(downloads.join("file.bin.udm.part"), b"ABCD\0\0\0\0").unwrap();
    let store = Store::open(root.join("test.redb")).unwrap();
    let mut ids = Vec::new();
    for _ in 0..copies {
        let id = store.next_task_id().unwrap();
        let mut record = TaskRecord::new(id, url, &downloads, Some("file.bin".into()));
        record.state = TaskState::Paused;
        record.total_size = Some(8);
        let mut segment = SegmentState::new(0, 0, 7);
        segment.done = 4;
        let file = fs::OpenOptions::new()
            .write(true)
            .open(record.part_path())
            .unwrap();
        record
            .prepare_progress_checkpoint(&[segment], &file)
            .unwrap();
        store.save(&[record], &[(id, vec![segment])], true).unwrap();
        ids.push(id);
    }
    ids
}

/// Same partial as [`seed`], plus a strong-ETag bound so slice 4b may resume
/// it. A stored ETag without this bound is not a resume.
fn seed_bound(root: &Path, url: &str) -> u64 {
    let downloads = root.join("downloads");
    fs::create_dir_all(&downloads).unwrap();
    fs::write(downloads.join("file.bin.udm.part"), b"ABCD\0\0\0\0").unwrap();
    let store = Store::open(root.join("test.redb")).unwrap();
    let id = store.next_task_id().unwrap();
    let mut record = TaskRecord::new(id, url, &downloads, Some("file.bin".into()));
    record.state = TaskState::Paused;
    record.total_size = Some(8);
    record.etag = Some("\"v\"".into());
    record.final_url = url.to_string();
    record.accept_ranges = true;
    record.representation_bound = Some(RepresentationBound::none_scope(url, "\"v\"", Some(8)));
    let mut segment = SegmentState::new(0, 0, 7);
    segment.done = 4;
    let file = fs::OpenOptions::new()
        .write(true)
        .open(record.part_path())
        .unwrap();
    record
        .prepare_progress_checkpoint(&[segment], &file)
        .unwrap();
    store.save(&[record], &[(id, vec![segment])], true).unwrap();
    id
}

#[test]
fn malformed_partial_headers_never_write_resume_data() {
    for range in [
        "",
        "Content-Range: items 4-7/8\r\n",
        "Content-Range: bytes */8\r\n",
        "Content-Range: bytes 7-4/8\r\n",
        "Content-Range: bytes 4-7/invalid\r\n",
        "Content-Range: bytes 4-7/*\r\n",
        "Content-Range: bytes 4-6/8\r\n",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (url, server) = serve(vec![
            head(8),
            reply(
                "206 Partial Content",
                &format!("Content-Length: 4\r\n{range}"),
                b"ABCD",
            ),
        ]);
        let id = seed(dir.path(), &url, 1)[0];
        let engine = engine(dir.path(), 0);
        engine.resume(id).unwrap();
        let task = wait(&engine, id);
        assert_eq!(task.state, TaskState::Failed, "range={range}");
        assert_eq!(task.active_requests, 0, "rejected response leaked activity");
        assert!(task
            .error
            .as_deref()
            .unwrap()
            .contains("invalid download response"));
        // No bound: the stored prefix is discarded before the bad response.
        // The preallocated file stays empty, so the malformed body was not written.
        assert_eq!(
            fs::read(dir.path().join("downloads/file.bin.udm.part")).unwrap(),
            [0u8; 8]
        );
        assert!(!Path::new(&task.final_path).exists());
        engine.shutdown();
        let requests = server.join().unwrap();
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("range: bytes=0-7"),
            "legacy progress resumed instead of restarting: {}",
            requests[1]
        );
    }
}

#[test]
fn extra_body_after_full_flush_cannot_complete_or_checkpoint_the_tail() {
    let dir = tempfile::tempdir().unwrap();
    let mut body = reply(
        "206 Partial Content",
        "Content-Range: bytes 0-1048575/1048576\r\nTransfer-Encoding: chunked\r\n",
        b"100000\r\n",
    );
    body.bytes.extend(vec![b'A'; 1048576]);
    body.bytes.extend_from_slice(b"\r\n");
    body.tail = Some(b"1\r\nB\r\n0\r\n\r\n".to_vec());
    let (url, server) = serve(vec![head(1048576), body]);
    let engine = engine(dir.path(), 1);
    let id = engine.add(AddRequest::new(url)).unwrap();
    let task = wait(&engine, id);
    assert_eq!(task.state, TaskState::Failed);
    assert!(task.error.as_deref().unwrap().contains("exceeds"));
    assert!(!Path::new(&task.final_path).exists());
    assert!(engine.store().get_segments(id).unwrap()[0].done < 1048576);
    engine.shutdown();
    server.join().unwrap();
}

#[test]
fn protocol_restart_leaves_one_network_retry_for_503() {
    let dir = tempfile::tempdir().unwrap();
    let full = || {
        reply(
            "200 OK",
            "Content-Length: 8\r\nETag: \"v\"\r\n",
            b"ABCDEFGH",
        )
    };
    let (url, server) = serve(vec![
        head_bound(8),
        full(),
        head_bound(8),
        reply("503 Service Unavailable", "Content-Length: 0\r\n", b""),
        head_bound(8),
        full(),
    ]);
    let id = seed_bound(dir.path(), &url);
    let engine = engine(dir.path(), 1);
    engine.resume(id).unwrap();
    let task = wait(&engine, id);
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    assert_eq!(fs::read(task.final_path).unwrap(), b"ABCDEFGH");
    engine.shutdown();
    assert_eq!(server.join().unwrap().len(), 6);
}

#[test]
fn repeated_protocol_failures_remain_bounded_with_zero_network_retries() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![
        head_bound(8),
        reply(
            "200 OK",
            "Content-Length: 8\r\nETag: \"v\"\r\n",
            b"ABCDEFGH",
        ),
        head_bound(8),
        reply(
            "200 OK",
            "Content-Length: 12\r\nETag: \"v\"\r\n",
            b"ABCDEFGHIJKL",
        ),
    ]);
    let id = seed_bound(dir.path(), &url);
    let engine = engine(dir.path(), 0);
    engine.resume(id).unwrap();
    assert_eq!(wait(&engine, id).state, TaskState::Failed);
    engine.shutdown();
    assert_eq!(server.join().unwrap().len(), 4);
}

#[test]
fn duplicate_removal_preserves_legacy_owner_and_owner_can_resume() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![
        head_bound(8),
        reply(
            "206 Partial Content",
            "Content-Length: 4\r\nContent-Range: bytes 4-7/8\r\nETag: \"v\"\r\n",
            b"EFGH",
        ),
    ]);
    let first = seed_bound(dir.path(), &url);
    let engine = engine(dir.path(), 0);
    let duplicate = engine
        .add(
            AddRequest::new("https://example.invalid/other")
                .filename("file.bin")
                .start_paused(true),
        )
        .unwrap();
    engine.remove(duplicate, false).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.task(duplicate).is_some() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::read(dir.path().join("downloads/file.bin.udm.part")).unwrap(),
        b"ABCD\0\0\0\0"
    );
    engine.resume(first).unwrap();
    let task = wait(&engine, first);
    assert_eq!(task.state, TaskState::Completed);
    assert_eq!(fs::read(task.final_path).unwrap(), b"ABCDEFGH");
    engine.shutdown();
    server.join().unwrap();
}

#[test]
fn ambiguous_legacy_partials_are_neither_reused_nor_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![head(8)]);
    let ids = seed(dir.path(), &url, 2);
    let engine = engine(dir.path(), 0);
    engine.resume(ids[0]).unwrap();
    assert_eq!(wait(&engine, ids[0]).state, TaskState::Failed);
    engine.remove(ids[1], true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.task(ids[1]).is_some() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        fs::read(dir.path().join("downloads/file.bin.udm.part")).unwrap(),
        b"ABCD\0\0\0\0"
    );
    engine.shutdown();
    server.join().unwrap();
}

#[test]
fn new_task_does_not_claim_a_stray_partial_by_filename() {
    let dir = tempfile::tempdir().unwrap();
    let (url, server) = serve(vec![head(8)]);
    let engine = engine(dir.path(), 0);
    let downloads = dir.path().join("downloads");
    fs::create_dir_all(&downloads).unwrap();
    let partial = downloads.join("file.bin.udm.part");
    fs::write(&partial, b"unclaimed data").unwrap();
    let id = engine
        .add(AddRequest::new(url).filename("file.bin"))
        .unwrap();
    assert_eq!(wait(&engine, id).state, TaskState::Failed);
    assert_eq!(fs::read(partial).unwrap(), b"unclaimed data");
    engine.shutdown();
    server.join().unwrap();
}

#[test]
fn removing_a_stalled_worker_waits_for_its_last_persist() {
    let dir = tempfile::tempdir().unwrap();
    let mut body = reply("200 OK", "Content-Length: 8\r\n", b"ABCD");
    body.tail = Some(b"EFGH".to_vec());
    body.tail_delay = Duration::from_millis(2500);
    let (url, server) = serve(vec![head(8), body]);
    let engine = engine(dir.path(), 0);
    let id = engine.add(AddRequest::new(url)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(7);
    while engine.task(id).unwrap().received < 4 {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    engine.remove(id, true).unwrap();
    while engine.task(id).is_some() {
        assert!(Instant::now() < deadline, "pending removal never completed");
        thread::sleep(Duration::from_millis(10));
    }
    assert!(engine.store().get_task(id).unwrap().is_none());
    assert!(!dir.path().join("downloads/file.bin.udm.part").exists());
    engine.shutdown();
    server.join().unwrap();
}

#[test]
fn valid_exact_flush_boundary_completes_only_after_eof() {
    let dir = tempfile::tempdir().unwrap();
    let body = vec![b'A'; 1048576];
    let (url, server) = serve(vec![
        head(body.len()),
        reply(
            "206 Partial Content",
            "Content-Length: 1048576\r\nContent-Range: bytes 0-1048575/1048576\r\n",
            &body,
        ),
    ]);
    let engine = engine(dir.path(), 0);
    let id = engine.add(AddRequest::new(url)).unwrap();
    let task = wait(&engine, id);
    assert_eq!(task.state, TaskState::Completed);
    assert_eq!(fs::read(task.final_path).unwrap(), body);
    assert_eq!(task.received, 1048576);
    engine.shutdown();
    server.join().unwrap();
}

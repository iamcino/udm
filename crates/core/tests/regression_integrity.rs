// Regression tests for engine data integrity bugs identified in review-2026-11-04.md.
// All tests assert CORRECT, expected file content and recovery behavior.
use std::{
    io::{Read, Write},
    net::TcpListener,
    path::Path,
    thread,
    time::{Duration, Instant},
};
use usagi::{
    config::Settings,
    engine::{AddRequest, Engine, EngineBuilder},
    paths::Paths,
    store::{RepresentationBound, SegmentState, Store, TaskRecord, TaskState},
};

fn engine(root: &Path) -> Engine {
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
        segments_per_task: 1,
        max_retries: 2,
        default_download_dir: root.join("downloads").to_string_lossy().into(),
        ..Settings::default()
    };
    EngineBuilder::new()
        .paths(paths)
        .settings(settings)
        .build()
        .unwrap()
}

fn wait(engine: &Engine, id: u64) -> usagi::engine::TaskSnapshot {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let task = engine.task(id).unwrap();
        if task.state.is_terminal() {
            return task;
        }
        assert!(Instant::now() < deadline, "timed out: {:?}", task.state);
        thread::sleep(Duration::from_millis(20));
    }
}

fn server(
    total: usize,
    first: usize,
    body: Vec<u8>,
    status: &str,
) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
    let status = status.to_string();
    let handle = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        for index in 0..2 {
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "server accept timed out");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                match stream.read(&mut byte) {
                    Ok(1) => request.push(byte[0]),
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            let request = String::from_utf8(request).unwrap();
            if index == 0 {
                assert!(request.starts_with("HEAD "));
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n").unwrap();
            } else {
                assert!(request.starts_with("GET "));
                let range = if status.starts_with("206") {
                    format!(
                        "Content-Range: bytes {first}-{}/{total}\r\n",
                        first + body.len() - 1
                    )
                } else {
                    String::new()
                };
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\n{range}Connection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
        }
    });
    (url, handle)
}

fn seed(root: &Path, url: &str) -> u64 {
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    let store = Store::open(root.join("test.redb")).unwrap();
    let id = store.next_task_id().unwrap();
    let mut task = TaskRecord::new(id, url, &root.join("downloads"), Some("file.bin".into()));
    task.state = TaskState::Paused;
    task.total_size = Some(8);
    let mut segment = SegmentState::new(0, 0, 7);
    segment.done = 4;
    store.save(&[task], &[(id, vec![segment])], true).unwrap();
    id
}

fn seed_checkpointed(root: &Path, url: &str) -> u64 {
    let id = seed(root, url);
    let store = Store::open(root.join("test.redb")).unwrap();
    let mut task = store.get_task(id).unwrap().unwrap();
    let segments = store.get_segments(id).unwrap();
    std::fs::write(task.part_path(), b"ABCD\0\0\0\0").unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(task.part_path())
        .unwrap();
    // A stored prefix resumes only with a bound. The 200 below is then
    // RangeIgnored, which restarts without spending max_retries.
    task.etag = Some("\"v\"".into());
    task.final_url = url.to_string();
    task.representation_bound = Some(RepresentationBound::none_scope(url, "\"v\"", Some(8)));
    task.prepare_progress_checkpoint(&segments, &file).unwrap();
    store.save(&[task], &[(id, segments)], true).unwrap();
    id
}

#[test]
fn regression_missing_part_resets_progress_to_zero() {
    let dir = tempfile::tempdir().unwrap();
    // Missing .part must restart from byte 0, requesting 0-7 instead of 4-7
    let (url, server) = server(8, 0, b"ABCDEFGH".to_vec(), "206 Partial Content");
    let id = seed(dir.path(), &url);
    let engine = engine(dir.path());
    engine.resume(id).unwrap();
    let task = wait(&engine, id);
    println!("TASK ERROR ON FALLBACK: {:?}", task.error);
    assert_eq!(task.state, TaskState::Completed);
    let bytes = std::fs::read(&task.final_path).unwrap();
    // Must NOT contain zero bytes from stale resume
    assert_eq!(bytes, b"ABCDEFGH");
    engine.shutdown();
    server.join().unwrap();
}

#[test]
fn regression_range_ignored_falls_back_to_single_connection_and_completes() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!(
        "http://{}/range_fallback.bin",
        listener.local_addr().unwrap()
    );
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut _head_count = 0;
        let mut get_count = 0;
        while get_count < 2 {
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "server accept timed out");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                match stream.read(&mut byte) {
                    Ok(1) => request.push(byte[0]),
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            let request = String::from_utf8(request).unwrap();
            if request.starts_with("HEAD ") {
                _head_count += 1;
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nAccept-Ranges: bytes\r\nETag: \"v\"\r\nConnection: close\r\n\r\n").unwrap();
            } else if request.starts_with("GET ") {
                get_count += 1;
                if get_count == 1 {
                    assert!(
                        request.to_ascii_lowercase().contains("range: bytes=4-"),
                        "bound resume did not request the flushed range: {request}"
                    );
                    // Range ignored: 200 at a non-zero offset restarts from 0.
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nETag: \"v\"\r\nConnection: close\r\n\r\nABCDEFGH").unwrap();
                } else {
                    assert!(
                        request.to_ascii_lowercase().contains("range: bytes=0-")
                            || !request.to_ascii_lowercase().contains("range:")
                    );
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nETag: \"v\"\r\nConnection: close\r\n\r\nABCDEFGH").unwrap();
                }
            }
        }
    });
    let id = seed_checkpointed(dir.path(), &url);
    let engine = engine(dir.path());
    engine.resume(id).unwrap();
    let task = wait(&engine, id);
    println!("TASK ERROR ON FALLBACK: {:?}", task.error);
    assert_eq!(task.state, TaskState::Completed);
    let bytes = std::fs::read(&task.final_path).unwrap();
    assert_eq!(bytes, b"ABCDEFGH");
    engine.shutdown();
    server.join().unwrap();
}

fn engine_with_retries(root: &Path, retries: u32) -> Engine {
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
        segments_per_task: 1,
        max_retries: retries,
        default_download_dir: root.join("downloads").to_string_lossy().into(),
        ..Settings::default()
    };
    EngineBuilder::new()
        .paths(paths)
        .settings(settings)
        .build()
        .unwrap()
}

#[test]
fn regression_body_larger_than_planned_file_is_rejected_not_truncated() {
    let dir = tempfile::tempdir().unwrap();
    // Server advertises 8 bytes on HEAD but sends 12 bytes on GET; mock server terminates after 2 requests
    let (url, server) = server(8, 0, b"ABCDEFGHIJKL".to_vec(), "200 OK");
    let engine = engine_with_retries(dir.path(), 0);
    let id = engine.add(AddRequest::new(url)).unwrap();
    let task = wait(&engine, id);
    // Must NOT be marked Completed with 8 truncated bytes
    assert_ne!(task.state, TaskState::Completed);
    assert_eq!(task.state, TaskState::Failed);
    engine.shutdown();
    server.join().unwrap();
}

#[test]
fn regression_oversized_partial_response_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    // Server advertises 8 bytes on HEAD, but sends 12 bytes on 206 Partial Content
    let (url, server) = server(8, 0, b"ABCDEFGHIJKL".to_vec(), "206 Partial Content");
    let engine = engine_with_retries(dir.path(), 0);
    let id = engine.add(AddRequest::new(url)).unwrap();
    let task = wait(&engine, id);
    assert_ne!(task.state, TaskState::Completed);
    assert_eq!(task.state, TaskState::Failed);
    engine.shutdown();
    server.join().unwrap();
}

#[test]
fn regression_delete_never_started_task_does_not_delete_preexisting_file() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine_with_retries(dir.path(), 0);
    let target = dir.path().join("downloads/existing.bin");
    std::fs::create_dir_all(target.parent().unwrap()).unwrap();
    std::fs::write(&target, b"original user file").unwrap();

    let id = engine
        .add(
            AddRequest::new("https://example.invalid/file.bin")
                .filename("existing.bin")
                .start_paused(true),
        )
        .unwrap();

    // Attempt to remove with delete_files=true
    engine.remove(id, true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while engine.task(id).is_some() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }

    // Preexisting file must NOT be deleted
    assert!(
        target.exists(),
        "preexisting file was unexpectedly deleted!"
    );
    assert_eq!(std::fs::read(&target).unwrap(), b"original user file");
    engine.shutdown();
}

#[test]
fn regression_zero_retry_budget_still_performs_range_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/file.bin", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut get_count = 0;
        while get_count < 2 {
            let mut stream = loop {
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "server accept timed out");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(e) => panic!("{e}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                match stream.read(&mut byte) {
                    Ok(1) => request.push(byte[0]),
                    Ok(_) => break,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(e) => panic!("{e}"),
                }
            }
            let request = String::from_utf8(request).unwrap();
            if request.starts_with("HEAD ") {
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nAccept-Ranges: bytes\r\nETag: \"v\"\r\nConnection: close\r\n\r\n").unwrap();
            } else if request.starts_with("GET ") {
                get_count += 1;
                if get_count == 1 {
                    assert!(
                        request.to_ascii_lowercase().contains("range: bytes=4-"),
                        "bound resume did not request the flushed range: {request}"
                    );
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nETag: \"v\"\r\nConnection: close\r\n\r\nABCDEFGH").unwrap();
                } else {
                    assert!(
                        request.to_ascii_lowercase().contains("range: bytes=0-")
                            || !request.to_ascii_lowercase().contains("range:")
                    );
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nETag: \"v\"\r\nConnection: close\r\n\r\nABCDEFGH").unwrap();
                }
            }
        }
    });

    let id = seed_checkpointed(dir.path(), &url);
    // max_retries = 0: must still execute the 1 bounded single-connection recovery!
    let engine = engine_with_retries(dir.path(), 0);
    engine.resume(id).unwrap();
    let task = wait(&engine, id);
    assert_eq!(task.state, TaskState::Completed);
    let bytes = std::fs::read(&task.final_path).unwrap();
    assert_eq!(bytes, b"ABCDEFGH");
    engine.shutdown();
    server.join().unwrap();
}

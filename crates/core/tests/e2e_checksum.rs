use sha2::Digest;
use std::{io::Write, net::TcpListener, thread, time::Duration};
use usagi::{
    config::Settings,
    engine::{AddRequest, EngineBuilder},
    hash::ExpectedHash,
    paths::Paths,
};

#[test]
fn e2e_download_with_checksum_verification() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let url = format!("http://127.0.0.1:{port}/payload.bin");

    let payload = vec![0x42u8; 64 * 1024];
    let digest = sha2::Sha256::digest(&payload);
    let digest_hex = hex::encode(digest);
    let expected_hash = ExpectedHash::parse(&format!("sha256:{digest_hex}")).unwrap();

    let server_payload = payload.clone();
    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = Vec::new();
            while !req.ends_with(b"\r\n\r\n") {
                let mut b = [0];
                stream.read_exact(&mut b).unwrap();
                req.push(b[0]);
            }
            use std::io::Read;
            let req_str = String::from_utf8_lossy(&req);
            if req_str.starts_with("HEAD ") {
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n", server_payload.len()).unwrap();
            } else {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    server_payload.len()
                )
                .unwrap();
                stream.write_all(&server_payload).unwrap();
            }
        }
    });

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
        verify_checksums: true,
        default_download_dir: root.join("downloads").to_string_lossy().into(),
        ..Settings::default()
    };
    let engine = EngineBuilder::new()
        .paths(paths)
        .settings(settings)
        .build()
        .unwrap();

    let id = engine
        .add(AddRequest::new(url).expected_hash(expected_hash))
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let task = engine.task(id).unwrap();
        if task.state.is_terminal() {
            assert_eq!(task.state, usagi::store::TaskState::Completed);
            let downloaded = std::fs::read(&task.final_path).unwrap();
            assert_eq!(downloaded.len(), payload.len());
            assert_eq!(downloaded, payload);
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        thread::sleep(Duration::from_millis(20));
    }
    engine.shutdown();
    server.join().unwrap();
}

#[test]
fn active_requests_are_independent_of_finished_segments() {
    use std::{
        io::Read,
        sync::{Arc, Condvar, Mutex},
        time::Instant,
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/parallel.bin", listener.local_addr().unwrap());
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let server_gate = gate.clone();
    let server = thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut workers = Vec::new();
        for _ in 0..9 {
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "missing segment request");
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            let gate = server_gate.clone();
            workers.push(thread::spawn(move || {
                // Accepted sockets can inherit the listener's nonblocking mode
                // on Windows. This worker deliberately uses blocking reads.
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                if request.starts_with("HEAD ") {
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: 524288\r\nAccept-Ranges: bytes\r\nETag: \"fixed\"\r\nConnection: close\r\n\r\n").unwrap();
                    return;
                }
                let range = request.lines().find_map(|line| {
                    line.to_ascii_lowercase().strip_prefix("range: bytes=").map(str::to_owned)
                }).unwrap();
                let (start, end) = range.split_once('-').unwrap();
                let start: usize = start.parse().unwrap();
                let end: usize = end.parse().unwrap();
                let length = end - start + 1;
                write!(stream, "HTTP/1.1 206 Partial Content\r\nContent-Length: {length}\r\nContent-Range: bytes {start}-{end}/524288\r\nETag: \"fixed\"\r\nConnection: close\r\n\r\n").unwrap();
                stream.write_all(b"B").unwrap();
                let (lock, ready) = &*gate;
                let (released, timeout) = ready.wait_timeout_while(
                    lock.lock().unwrap(), Duration::from_secs(20), |released| !*released,
                ).unwrap();
                assert!(*released && !timeout.timed_out(), "test did not release responses");
                stream.write_all(&vec![b'B'; length - 1]).unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
    });
    let paths = Paths {
        root: root.into(),
        db_file: root.join("test.redb"),
        log_file: root.join("logs/test.log"),
        temp_dir: root.join("tmp"),
        default_download_dir: root.join("downloads"),
        portable: true,
        exe_dir: Some(root.into()),
    };
    let engine = EngineBuilder::new()
        .paths(paths)
        .settings(Settings {
            http3_enabled: false,
            use_system_proxy: false,
            segments_per_task: 8,
            default_download_dir: root.join("downloads").to_string_lossy().into_owned(),
            min_segment_bytes: 64 * 1024,
            ..Settings::default()
        })
        .build()
        .unwrap();
    let id = engine.add(AddRequest::new(url)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let task = engine.task(id).unwrap();
        if task.active_requests == 8 && task.received >= 8 {
            assert_eq!(task.segments_done, 0);
            assert_eq!(task.segments_total, 8);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "requests never became active: {task:?}"
        );
        thread::sleep(Duration::from_millis(5));
    }
    *gate.0.lock().unwrap() = true;
    gate.1.notify_all();
    loop {
        let task = engine.task(id).unwrap();
        if task.state.is_terminal() {
            assert_eq!(task.state, usagi::TaskState::Completed, "{:?}", task.error);
            assert_eq!(
                (
                    task.segments_done,
                    task.segments_total,
                    task.active_requests
                ),
                (8, 8, 0)
            );
            assert_eq!(std::fs::read(task.final_path).unwrap(), vec![b'B'; 524288]);
            break;
        }
        assert!(Instant::now() < deadline, "transfer did not complete");
        thread::sleep(Duration::from_millis(5));
    }
    engine.shutdown();
    server.join().unwrap();
}

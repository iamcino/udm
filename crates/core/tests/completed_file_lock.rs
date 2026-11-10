use sha2::Digest;
use std::{io::Write, net::TcpListener, thread, time::Duration};
use usagi::{
    config::Settings,
    engine::{AddRequest, EngineBuilder},
    hash::ExpectedHash,
    paths::Paths,
};

#[test]
fn completed_download_releases_file_while_engine_lives() {
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
    // Completion publication follows the durable finalize transaction.
    loop {
        let event = engine
            .events()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
        if matches!(event, usagi::engine::Event::TaskCompleted { id: done, .. } if done == id) {
            break;
        }
    }
    let path = std::path::PathBuf::from(engine.task(id).unwrap().final_path);
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Reject existing writers, as Windows executable/image consumers do.
        options.share_mode(0x1 | 0x4); // FILE_SHARE_READ | FILE_SHARE_DELETE
    }
    let file = options
        .open(&path)
        .expect("completed file still has an engine write handle");
    drop(file);
    let renamed = path.with_extension("released");
    std::fs::rename(&path, &renamed).expect("rename completed file while engine lives");
    std::fs::remove_file(&renamed).expect("delete completed file while engine lives");
    assert!(
        engine.task(id).is_some(),
        "task must remain in the live engine"
    );
    engine.shutdown();
    server.join().unwrap();
}

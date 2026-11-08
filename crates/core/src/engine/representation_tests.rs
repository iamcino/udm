//! Slice 4b fixtures that have to see `run_segment`, `reset_progress`, and
//! `persist_epoch`. Integration tests cannot reach those.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::*;
use crate::engine::{AddRequest, Engine, EngineBuilder};
use crate::paths::Paths;
use crate::store::records::{RepresentationBound, HEADER_SCOPE_NONE};

fn paused_engine(root: &std::path::Path) -> (Engine, u64) {
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    let engine = EngineBuilder::new()
        .paths(Paths {
            root: root.into(),
            db_file: root.join("test.redb"),
            log_file: root.join("logs/test.log"),
            temp_dir: root.join("tmp"),
            default_download_dir: root.join("downloads"),
            portable: true,
            exe_dir: Some(root.into()),
        })
        .idle(true)
        .build()
        .unwrap();
    let id = engine
        .add(
            AddRequest::new("http://127.0.0.1:9/representation.bin")
                .dest_dir(root.join("downloads"))
                .filename("representation.bin")
                .start_paused(true),
        )
        .unwrap();
    (engine, id)
}

fn ctx_for(engine: &Engine) -> Arc<RunCtx> {
    Arc::new(RunCtx {
        state: engine.state.clone(),
        settings: engine.settings(),
        generation: 0,
    })
}

fn bound_for(url: &str, etag: &str, total: Option<u64>) -> RepresentationBound {
    RepresentationBound::none_scope(url, etag, total)
}

fn plan_for(
    url: &str,
    part: std::path::PathBuf,
    total: Option<u64>,
    bound: Option<RepresentationBound>,
) -> Plan {
    Plan {
        request_url: url.to_string(),
        part_path: part.clone(),
        final_path: part,
        total,
        if_range: bound.as_ref().map(|bound| bound.strong_etag.clone()),
        pref: TransportPref::NoHttp3,
        bound,
        method_get: true,
    }
}

/// Accept one connection, record the request, and write `response`.
fn serve_once(response: Vec<u8>) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        let (mut stream, _) = loop {
            match listener.accept() {
                Ok(pair) => break pair,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "416 fixture opened no request");
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8192, "request never ended");
        }
        stream.write_all(&response).unwrap();
        String::from_utf8(request).unwrap()
    });
    (format!("http://127.0.0.1:{port}/f.bin"), handle)
}

fn http(status: &str, extra: &str, body: &str) -> Vec<u8> {
    let extra = if extra.is_empty() {
        String::new()
    } else {
        format!("{extra}\r\n")
    };
    format!(
        "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn arm_segment(engine: &Engine, id: u64, done: u64, end: u64, finished: bool) {
    let cell = engine.state.cell(id).unwrap();
    let part = cell.record.read().part_path();
    let (file, _) = engine.state.open_partial(id, 0, &part).unwrap();
    file.set_len(done.max(1)).unwrap();
    drop(file);
    let mut segment = SegmentState::new(0, 0, end);
    segment.done = done;
    segment.finished = finished;
    *cell.segments.write() = vec![segment];
}

#[test]
fn classify_416_done_ge_len_is_not_finished() {
    let bound = bound_for("http://127.0.0.1/a", "\"e\"", Some(8));
    assert_eq!(
        classify_416(Some(8), Some(&bound), 8, Some(8), true),
        RangeClass::NotSatisfiable,
        "a matching 416 is not mark_finished"
    );
    assert_eq!(
        classify_416(Some(16), Some(&bound), 8, Some(8), false),
        RangeClass::Mismatch
    );
    assert_eq!(
        classify_416(None, Some(&bound), 2, Some(8), true),
        RangeClass::NotSatisfiable
    );
    assert_eq!(bound.header_scope, HEADER_SCOPE_NONE);
}

#[test]
fn url_for_log_strips_the_query() {
    assert_eq!(
        url_for_log("http://127.0.0.1/a?token=secret#frag"),
        "http://127.0.0.1/a"
    );
    assert!(!url_for_log("http://127.0.0.1/a?token=secret").contains("secret"));
}

#[test]
fn etag_difference_names_quote_only_and_weak_prefix() {
    assert_eq!(etag_difference("\"abc\"", "abc"), "quote-only difference");
    assert_eq!(etag_difference("\"abc\"", "W/\"abc\""), "weak-prefix difference");
    assert_eq!(etag_difference("\"abc\"", "\"xyz\""), "verbatim difference");
}

#[tokio::test(flavor = "multi_thread")]
async fn run_segment_416_unbound_done_does_not_finish() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, id) = paused_engine(dir.path());
    let cell = engine.state.cell(id).unwrap();
    arm_segment(&engine, id, 8, 7, false);
    let (url, server) = serve_once(http(
        "416 Range Not Satisfiable",
        "Content-Range: bytes */16",
        "",
    ));
    let plan = plan_for(&url, cell.record.read().part_path(), Some(8), None);
    let error = run_segment(ctx_for(&engine), cell.clone(), plan, CancellationToken::new(), 0)
        .await
        .expect_err("unbound 416 is a mismatch");
    assert!(matches!(error, Error::RemoteChanged), "{error}");
    let segment = cell.segments.read()[0];
    assert_eq!(segment.done, 8, "416 must not add coverage");
    assert!(!segment.finished, "416 must not mark_finished");
    assert_eq!(cell.record.read().representation_restarts, 0);
    let request = server.join().unwrap();
    assert!(request.starts_with("GET "), "the early return must not skip this request");
    engine.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn run_segment_416_matching_bound_skips_the_network() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, id) = paused_engine(dir.path());
    let cell = engine.state.cell(id).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://127.0.0.1:{}/f.bin", listener.local_addr().unwrap().port());
    let bound = bound_for(&url, "\"e\"", Some(8));
    cell.record.write().representation_bound = Some(bound.clone());
    arm_segment(&engine, id, 8, 7, true);
    let plan = plan_for(&url, cell.record.read().part_path(), Some(8), Some(bound));
    let started = Instant::now();
    run_segment(ctx_for(&engine), cell.clone(), plan, CancellationToken::new(), 0)
        .await
        .expect("a matched complete segment returns before the network");
    assert!(started.elapsed() < Duration::from_millis(1500));
    assert!(
        matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "early return connected"
    );
    assert!(cell.segments.read()[0].finished);
    assert_eq!(cell.segments.read()[0].done, 8);
    engine.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn run_segment_416_bound_mismatch_does_not_finish() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, id) = paused_engine(dir.path());
    let cell = engine.state.cell(id).unwrap();
    let (url, server) = serve_once(http(
        "416 Range Not Satisfiable",
        "Content-Range: bytes */99",
        "",
    ));
    let live = bound_for(&url, "\"live\"", Some(8));
    let planned = bound_for(&url, "\"plan\"", Some(8));
    cell.record.write().representation_bound = Some(live);
    arm_segment(&engine, id, 2, 7, false);
    let plan = plan_for(&url, cell.record.read().part_path(), Some(8), Some(planned));
    let error = run_segment(ctx_for(&engine), cell.clone(), plan, CancellationToken::new(), 0)
        .await
        .expect_err("a mismatched 416 restarts");
    assert!(matches!(error, Error::RemoteChanged), "{error}");
    assert_eq!(cell.segments.read()[0].done, 2);
    assert!(!cell.segments.read()[0].finished);
    assert!(server.join().unwrap().starts_with("GET "));
    engine.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn run_segment_416_without_a_usable_total_is_not_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, id) = paused_engine(dir.path());
    let cell = engine.state.cell(id).unwrap();
    let (url, server) = serve_once(http(
        "416 Range Not Satisfiable",
        "Content-Range: bytes */*",
        "",
    ));
    let bound = bound_for(&url, "\"e\"", Some(8));
    cell.record.write().representation_bound = Some(bound.clone());
    arm_segment(&engine, id, 2, 7, false);
    let plan = plan_for(&url, cell.record.read().part_path(), Some(8), Some(bound));
    let error = run_segment(ctx_for(&engine), cell.clone(), plan, CancellationToken::new(), 0)
        .await
        .expect_err("an unsatisfiable range is not a restart");
    assert!(matches!(error, Error::RangeNotSatisfiable), "{error}");
    assert_eq!(cell.segments.read()[0].done, 2);
    assert!(!cell.segments.read()[0].finished);
    assert_eq!(cell.record.read().representation_restarts, 0);
    assert!(server.join().unwrap().starts_with("GET "));
    engine.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn run_segment_416_short_segment_with_matching_total_is_not_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, id) = paused_engine(dir.path());
    let cell = engine.state.cell(id).unwrap();
    let (url, _) = serve_once(http(
        "416 Range Not Satisfiable",
        "Content-Range: bytes */8",
        "",
    ));
    let bound = bound_for(&url, "\"e\"", Some(8));
    cell.record.write().representation_bound = Some(bound.clone());
    arm_segment(&engine, id, 2, 7, false);
    let plan = plan_for(&url, cell.record.read().part_path(), Some(8), Some(bound));
    let error = run_segment(ctx_for(&engine), cell.clone(), plan, CancellationToken::new(), 0)
        .await
        .expect_err("a matching total on a short segment is RangeNotSatisfiable");
    assert!(matches!(error, Error::RangeNotSatisfiable), "{error}");
    assert_eq!(cell.segments.read()[0].done, 2);
    assert!(!cell.segments.read()[0].finished);
    assert_eq!(cell.record.read().representation_restarts, 0);
    engine.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn run_segment_416_shortened_resource_is_a_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, id) = paused_engine(dir.path());
    let cell = engine.state.cell(id).unwrap();
    let (url, server) = serve_once(http(
        "416 Range Not Satisfiable",
        "Content-Range: bytes */40",
        "",
    ));
    let bound = bound_for(&url, "\"e\"", Some(100));
    cell.record.write().representation_bound = Some(bound.clone());
    arm_segment(&engine, id, 10, 99, false);
    let plan = plan_for(&url, cell.record.read().part_path(), Some(100), Some(bound));
    let error = run_segment(ctx_for(&engine), cell.clone(), plan, CancellationToken::new(), 0)
        .await
        .expect_err("a shorter 416 total contradicts the bound");
    assert!(matches!(error, Error::RemoteChanged), "{error}");
    assert_eq!(cell.segments.read()[0].done, 10);
    assert!(!cell.segments.read()[0].finished);
    assert!(server.join().unwrap().starts_with("GET "));
    engine.shutdown();
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_is_deleted_only_after_the_clear_is_durable() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, id) = paused_engine(dir.path());
    let cell = engine.state.cell(id).unwrap();
    let part = cell.record.read().part_path();
    let (file, _) = engine.state.open_partial(id, 0, &part).unwrap();
    file.set_len(4).unwrap();
    drop(file);
    {
        let mut record = cell.record.write();
        let mut segments = cell.segments.write();
        record.etag = Some("\"v\"".into());
        record.final_url = record.url.clone();
        record.total_size = Some(8);
        record.representation_bound = Some(bound_for(&record.final_url, "\"v\"", Some(8)));
        let mut segment = SegmentState::new(0, 0, 7);
        segment.done = 4;
        *segments = vec![segment];
        cell.bump_persist_epoch();
    }
    engine.state.persist(&cell, true).unwrap();
    assert!(part.exists());
    assert_eq!(
        engine.store().get_task(id).unwrap().unwrap().etag.as_deref(),
        Some("\"v\"")
    );

    engine.store().fail_next_saves(1);
    let failed = reset_progress(&ctx_for(&engine), &cell, true).await;
    assert!(failed.is_err(), "the injected save must fail before the delete");
    assert!(part.exists(), "a failed clear must leave the partial");
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert_eq!(stored.etag.as_deref(), Some("\"v\""));
    assert!(stored.representation_bound.is_some());
    assert_eq!(engine.store().get_segments(id).unwrap()[0].done, 4);

    reset_progress(&ctx_for(&engine), &cell, false)
        .await
        .expect("the durable clear deletes the partial");
    assert!(!part.exists(), "the partial is deleted only after the clear is durable");
    let stored = engine.store().get_task(id).unwrap().unwrap();
    assert!(stored.etag.is_none());
    assert!(stored.representation_bound.is_none());
    assert!(engine.store().get_segments(id).unwrap().is_empty());
    engine.shutdown();
}

#[test]
fn persist_epoch_retry_does_not_commit_the_pre_clear_pair() {
    let dir = tempfile::tempdir().unwrap();
    let (engine, id) = paused_engine(dir.path());
    let cell = engine.state.cell(id).unwrap();
    let part = cell.record.read().part_path();
    let (file, _) = engine.state.open_partial(id, 0, &part).unwrap();
    file.set_len(4).unwrap();
    drop(file);
    {
        let mut record = cell.record.write();
        let mut segments = cell.segments.write();
        record.etag = Some("\"keep\"".into());
        record.final_url = record.url.clone();
        record.total_size = Some(8);
        record.representation_bound = Some(bound_for(&record.final_url, "\"keep\"", Some(8)));
        let mut segment = SegmentState::new(0, 0, 7);
        segment.done = 4;
        *segments = vec![segment];
        cell.bump_persist_epoch();
    }
    engine.state.persist(&cell, true).unwrap();
    assert_eq!(
        engine.store().get_task(id).unwrap().unwrap().etag.as_deref(),
        Some("\"keep\"")
    );

    let watched = cell.clone();
    engine.store().arm_save_observer(Arc::new(move || {
        let mut record = watched.record.write();
        let mut segments = watched.segments.write();
        record.etag = None;
        record.representation_bound = None;
        record.total_size = None;
        *segments = Vec::new();
        watched.bump_persist_epoch();
    }));
    engine.state.persist(&cell, true).unwrap();
    let stored = engine.store().get_task(id).unwrap().unwrap();
    let segments = engine.store().get_segments(id).unwrap();
    assert!(stored.etag.is_none(), "the pre-clear validator was the last commit");
    assert!(stored.representation_bound.is_none());
    assert!(segments.is_empty(), "cleared progress was not the last commit");
    engine.shutdown();
}

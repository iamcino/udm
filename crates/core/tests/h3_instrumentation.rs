//! I2: one HTTP/3 handshake per origin, `stable_id` at insert, H3 gauge only.
//! I3: that id is in use only until the network terminal. The tail flush and
//! `active_requests` keep going. GOAWAY drains the id and refuses a new
//! request on it. The engine total stays `Unavailable`.
//!
//! The QUIC certificate comes from the I1 `HarnessCa`. Production builders
//! still trust the operating-system roots and nothing else.

#[path = "common/mod.rs"]
mod common;

use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use usagi::net::h3::{H3Pool, H3StableIds};
use usagi::net::{EngineConnections, ProtocolGauge, RequestSpec};
use usagi::{AddRequest, Engine, EngineBuilder, Paths, Settings, TaskState, TransportPref};

use common::dev_tls::{HarnessCa, HARNESS_MARKER};

/// Rendezvous budget for this suite's condition and operation waits. A
/// rendezvous wait returns the moment its condition holds (a tenth of a
/// second on an idle machine), so a generous ceiling costs nothing on the
/// passing path; it only keeps a loaded machine - the full workspace suite
/// compiling and running beside these tests - from expiring a wait whose
/// condition was about to hold. Semantic deadlines are not rendezvous
/// waits and keep their own values: the cancel promptness pair in
/// `cancel_drops_in_use_without_waiting_out_stall_timeout`, the pool
/// connect-timeout configurations, and the fixture servers' lifetimes.
const RENDEZVOUS: Duration = Duration::from_secs(30);

/// The tests in this file share one process. Run in parallel they contend
/// for the same cores, and that self-inflicted load is what starved the
/// rendezvous waits above (four flakes on 2026-11-09, always under load,
/// always passing standalone). Serialising the file's own tests removes
/// that half of the contention; the budget above covers the rest. It also
/// keeps the process-wide panic hook in the abort/drop test from recording
/// a panic that belongs to a different test running beside it.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[tokio::test]
async fn parallel_misses_open_one_quic_connection() -> Result<(), String> {
    let _serial = serial();
    let ca = HarnessCa::generate()?;
    let server = ca.spawn_h3().await?;
    let pool = harness_pool(&ca, Duration::from_secs(5))?;
    let url = origin_url(server.addr);
    let ids = join_fetches(&pool, &url, 2).await?;

    if server.accepted() != 1 {
        return Err(format!(
            "server accepted {} QUIC connections, want 1",
            server.accepted()
        ));
    }
    if server.requests() != 2 {
        return Err(format!("server saw {} requests, want 2", server.requests()));
    }
    if ids[0] != ids[1] {
        return Err(format!("parallel misses recorded two ids: {ids:?}"));
    }
    expect_ids(&pool, &[ids[0]])?;
    expect_membership(&pool, 1, 0)?;
    expect_engine_total_unavailable(&pool)?;
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn eight_streams_share_one_stable_id() -> Result<(), String> {
    let _serial = serial();
    let ca = HarnessCa::generate()?;
    let server = ca.spawn_h3().await?;
    let pool = harness_pool(&ca, Duration::from_secs(5))?;
    let url = origin_url(server.addr);
    let ids = join_fetches(&pool, &url, 8).await?;
    let id = ids[0];
    if ids.iter().any(|other| *other != id) {
        return Err(format!("eight streams did not share one id: {ids:?}"));
    }
    if server.accepted() != 1 {
        return Err(format!(
            "server accepted {} QUIC connections for eight streams, want 1",
            server.accepted()
        ));
    }
    if server.requests() != 8 {
        return Err(format!("server saw {} requests, want 8", server.requests()));
    }
    expect_ids(&pool, &[id])?;
    expect_membership(&pool, 1, 0)?;
    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn failed_handshake_records_no_stable_id() -> Result<(), String> {
    let _serial = serial();
    let trusted = HarnessCa::generate()?;
    let presented = HarnessCa::generate()?;
    let server = presented.spawn_h3().await?;
    let pool = harness_pool(&trusted, Duration::from_secs(2))?;
    let url = origin_url(server.addr);

    for attempt in 1..=2 {
        match fetch(&pool, &url).await {
            Ok(id) => {
                return Err(format!(
                    "attempt {attempt} recorded stable_id {id} for a certificate this client does not trust"
                ));
            }
            Err(_) => {}
        }
        expect_ids(&pool, &[])?;
        expect_membership(&pool, 0, 0)?;
    }

    let failed = wait_for_count(|| server.failed_handshakes(), 2).await;
    if failed != 2 {
        return Err(format!(
            "server saw {failed} failed handshakes, want 2 (the single-flight slot must be clear for the second dial)"
        ));
    }
    if server.accepted() != 0 {
        return Err(format!(
            "server accepted {} connections after failed handshakes",
            server.accepted()
        ));
    }
    server.shutdown().await;
    Ok(())
}

#[test]
fn production_builders_still_do_not_name_the_harness_ca() -> Result<(), String> {
    let _serial = serial();
    let harness = read_rel("crates/core/tests/common/dev_tls.rs")?;
    if !harness.contains("struct HarnessCa") {
        return Err("the dev harness no longer defines HarnessCa".to_string());
    }
    for needle in ["accept_invalid_certs", "CertAddEncodedCertificateToStore"] {
        if harness.contains(needle) {
            return Err(format!("dev harness contains `{needle}`"));
        }
    }

    let h3 = read_rel("crates/core/src/net/h3.rs")?;
    if h3.contains("HarnessCa") {
        return Err("h3.rs names HarnessCa".to_string());
    }
    let new_body = function_body(
        &h3,
        "pub fn new(connect_timeout: Duration, user_agent: &str)",
    )?;
    if !new_body.contains("tuned_client_config") {
        return Err(
            "H3Pool::new no longer installs the operating-system client config".to_string(),
        );
    }
    let stack = read_rel("crates/core/src/net/stack.rs")?;
    if !stack.contains("H3Pool::new") {
        return Err("NetStack no longer builds the pool with H3Pool::new".to_string());
    }
    if stack.contains("with_client_config") {
        return Err("NetStack calls with_client_config".to_string());
    }
    let engine = read_rel("crates/core/src/engine/mod.rs")?;
    let build_body = function_body(&engine, "pub fn build(self) -> Result<Engine>")?;
    if !build_body.contains("NetStack::new") {
        return Err("EngineBuilder::build no longer uses NetStack::new".to_string());
    }
    if build_body.contains("with_client_config") || build_body.contains("install_h3_client_config")
    {
        return Err("EngineBuilder::build installs a caller-supplied HTTP/3 config".to_string());
    }
    assert_tree_has_no_harness_ca(&workspace_root().join("crates/core/src"))?;
    Ok(())
}

#[test]
fn held_body_eight_streams_are_one_active_connection() -> Result<(), String> {
    let _serial = serial();
    let rt = harness_runtime()?;
    let ca = HarnessCa::generate()?;
    let server = rt.block_on(ca.spawn_h3_file(vec![b'B'; 8 * 64 * 1024], true))?;
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let engine = test_engine(dir.path(), 30, 8, TransportPref::Http3Only)?;
    let pool = install_harness(&engine, &ca)?;
    let url = format!("https://localhost:{}/eight.bin", server.addr.port());
    let id = engine
        .add(
            AddRequest::new(&url)
                .dest_dir(dir.path().join("downloads"))
                .filename("eight.bin")
                .segments(8)
                .transport_pref(TransportPref::Http3Only),
        )
        .map_err(|e| format!("add: {e}"))?;

    wait_until("eight held streams", RENDEZVOUS, || {
        engine
            .task(id)
            .is_some_and(|task| task.active_requests == 8)
            && pool.in_use_requests() == Some(8)
            && matches!(
                pool.connection_gauge(),
                ProtocolGauge::Known {
                    active: 1,
                    idle: 0,
                    draining: 0,
                    pooled: 1,
                    ..
                }
            )
    })?;
    let task = engine.task(id).ok_or("task disappeared")?;
    if task.active_requests != 8 {
        return Err(format!("active_requests = {}", task.active_requests));
    }
    if server.accepted() != 1 {
        return Err(format!(
            "server accepted {} QUIC connections for eight held streams",
            server.accepted()
        ));
    }
    // HEAD probe plus eight segment bodies. The eight are the ones still in use.
    if server.requests() != 9 {
        return Err(format!(
            "server saw {} requests, want the HEAD plus 8 held streams",
            server.requests()
        ));
    }
    expect_engine_unavailable(&engine)?;
    engine.shutdown();
    rt.block_on(server.shutdown());
    Ok(())
}

#[test]
fn network_eof_ends_in_use_while_the_flush_is_held() -> Result<(), String> {
    let _serial = serial();
    let rt = harness_runtime()?;
    let ca = HarnessCa::generate()?;
    let payload = vec![b'C'; 64 * 1024];
    let server = rt.block_on(ca.spawn_h3_file(payload.clone(), false))?;
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let engine = test_engine(dir.path(), 30, 1, TransportPref::Http3Only)?;
    let pool = install_harness(&engine, &ca)?;
    let gate = engine.net().arm_tail_flush_gate();
    let url = format!("https://localhost:{}/flush.bin", server.addr.port());
    let id = engine
        .add(
            AddRequest::new(&url)
                .dest_dir(dir.path().join("downloads"))
                .filename("flush.bin")
                .segments(1)
                .transport_pref(TransportPref::Http3Only),
        )
        .map_err(|e| format!("add: {e}"))?;

    let observed = wait_until("flush held after network EOF", RENDEZVOUS, || {
        gate.entered()
            && engine.task(id).is_some_and(|task| task.active_requests > 0)
            && pool.in_use_requests() == Some(0)
            && matches!(
                pool.connection_gauge(),
                ProtocolGauge::Known {
                    active: 0,
                    idle: 1,
                    draining: 0,
                    pooled: 1,
                    ..
                }
            )
    });
    let requests_while_held = engine.task(id).map(|task| task.active_requests);
    let in_use_while_held = pool.in_use_requests();
    gate.release();
    observed?;
    if requests_while_held.unwrap_or(0) == 0 {
        return Err("active_requests dropped at the network EOF, before the flush".to_string());
    }
    if in_use_while_held != Some(0) {
        return Err(format!(
            "in-use was {in_use_while_held:?} while the flush was held"
        ));
    }

    wait_until("download finished after the flush", RENDEZVOUS, || {
        engine
            .task(id)
            .is_some_and(|task| task.state == TaskState::Completed)
    })?;
    let task = engine.task(id).ok_or("task disappeared")?;
    let downloaded = std::fs::read(&task.final_path).map_err(|e| format!("read download: {e}"))?;
    if downloaded != payload {
        return Err(format!(
            "downloaded {} bytes, want {}",
            downloaded.len(),
            payload.len()
        ));
    }
    engine.shutdown();
    rt.block_on(server.shutdown());
    Ok(())
}

#[test]
fn cancel_drops_in_use_without_waiting_out_stall_timeout() -> Result<(), String> {
    let _serial = serial();
    let rt = harness_runtime()?;
    let ca = HarnessCa::generate()?;
    let server = rt.block_on(ca.spawn_h3_file(vec![b'D'; 64 * 1024], true))?;
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let engine = test_engine(dir.path(), 5, 1, TransportPref::Http3Only)?;
    let pool = install_harness(&engine, &ca)?;
    let url = format!("https://localhost:{}/cancel.bin", server.addr.port());
    let id = engine
        .add(
            AddRequest::new(&url)
                .dest_dir(dir.path().join("downloads"))
                .filename("cancel.bin")
                .segments(1)
                .transport_pref(TransportPref::Http3Only),
        )
        .map_err(|e| format!("add: {e}"))?;

    wait_until("request is on the network", RENDEZVOUS, || {
        engine
            .task(id)
            .is_some_and(|task| task.active_requests >= 1)
            && pool.in_use_requests() == Some(1)
    })?;
    let started = Instant::now();
    engine.cancel(id).map_err(|e| format!("cancel: {e}"))?;
    wait_until(
        "in-use dropped after cancel",
        Duration::from_secs(4),
        || pool.in_use_requests() == Some(0),
    )?;
    let elapsed = started.elapsed();
    if elapsed >= Duration::from_secs(2) {
        return Err(format!(
            "cancel took {elapsed:?} to drop in-use; stall_timeout is 5s and this must not wait it out"
        ));
    }
    match pool.connection_gauge() {
        ProtocolGauge::Known {
            active: 0,
            idle: 1,
            draining: 0,
            pooled: 1,
            ..
        } => {}
        other => {
            return Err(format!(
                "after cancel the id should be idle in the pool, got {other:?}"
            ));
        }
    }
    engine.shutdown();
    rt.block_on(server.shutdown());
    Ok(())
}

/// Darius I3-1. Abort and drop both call `finish_network(true)` while
/// h3-quinn 0.0.10 still has the quinn recv half inside the pending read.
/// `stop_sending` unwraps that empty slot. Tokio would catch the panic on a
/// worker, so the hook is what fails the test if the unwrap comes back,
/// including a `catch_unwind` wrapped around the call.
#[tokio::test]
async fn abort_or_drop_during_pending_chunk_read_does_not_panic() -> Result<(), String> {
    let _serial = serial();
    let ca = HarnessCa::generate()?;
    let server = ca.spawn_h3_file(vec![b'P'; 64 * 1024], true).await?;
    let pool = harness_pool(&ca, Duration::from_secs(5))?;
    let url = origin_url(server.addr);

    let mut aborted = accepted_response(&pool, &url).await?;
    let mut dropped = accepted_response(&pool, &url).await?;
    // One poll that returns Pending is the bug's precondition: h3-quinn has
    // moved the recv half into the read future, and dropping this future does
    // not put it back. A read that already finished would not panic.
    park_pending_chunk(&mut aborted)?;
    park_pending_chunk(&mut dropped)?;
    if pool.in_use_requests() != Some(2) {
        return Err(format!(
            "in-use {:?}, want 2 while both reads are pending",
            pool.in_use_requests()
        ));
    }

    let record = PanicRecord::install();
    let abort_panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        aborted.abort();
    }));
    let in_use_after_abort = pool.in_use_requests();
    let drop_panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(dropped);
    }));
    drop(aborted);
    let in_use_after_drop = pool.in_use_requests();
    let seen = record.into_recorded();

    let mut failures = Vec::new();
    if let Err(payload) = abort_panic {
        failures.push(format!("abort panicked: {}", panic_payload(payload)));
    }
    if let Err(payload) = drop_panic {
        failures.push(format!("drop panicked: {}", panic_payload(payload)));
    }
    if !seen.is_empty() {
        failures.push(format!("panic hook recorded: {seen:?}"));
    }
    if in_use_after_abort != Some(1) {
        failures.push(format!(
            "abort left in-use {in_use_after_abort:?}, want 1 (the response that is only dropped)"
        ));
    }
    if in_use_after_drop != Some(0) {
        failures.push(format!(
            "in-use after both terminals was {in_use_after_drop:?}, want 0"
        ));
    }
    if !failures.is_empty() {
        return Err(failures.join("; "));
    }

    server.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn goaway_drains_the_id_and_refuses_a_new_request() -> Result<(), String> {
    let _serial = serial();
    let ca = HarnessCa::generate()?;
    let server = ca.spawn_h3_file(b"goaway-body".to_vec(), true).await?;
    let pool = harness_pool(&ca, Duration::from_secs(5))?;
    let url = origin_url(server.addr);

    let held = tokio::time::timeout(RENDEZVOUS, pool.request(&RequestSpec::get(&url)))
        .await
        .map_err(|_| "first request timed out".to_string())?
        .map_err(|e| format!("first request: {e}"))?;
    let id = held.connection_id;
    if pool.in_use_requests() != Some(1) {
        return Err(format!(
            "held response in-use {:?}, want 1",
            pool.in_use_requests()
        ));
    }
    expect_known(
        &pool,
        ProtocolGauge::Known {
            active: 1,
            idle: 0,
            connecting: 0,
            draining: 0,
            pooled: 1,
        },
    )?;

    server.goaway().await?;
    // The driver has to read the GOAWAY before send_request observes it.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let refused = tokio::time::timeout(RENDEZVOUS, pool.request(&RequestSpec::get(&url)))
        .await
        .map_err(|_| "refused request timed out".to_string())?
        .map_err(|e| e.to_string());
    let Err(refused) = refused else {
        return Err("GOAWAY accepted a new request on the pooled id".to_string());
    };
    if !refused.contains(&id.to_string()) || !refused.contains("will not take a new request") {
        return Err(format!("refusal did not name id {id}: {refused}"));
    }
    // The accepted stream is still on the network, so the id is active.
    // `draining` is reported beside that and is not added into it.
    expect_known(
        &pool,
        ProtocolGauge::Known {
            active: 1,
            idle: 0,
            connecting: 0,
            draining: 1,
            pooled: 1,
        },
    )?;
    if pool.in_use_requests() != Some(1) {
        return Err(format!(
            "in-flight request was dropped by GOAWAY: {:?}",
            pool.in_use_requests()
        ));
    }
    expect_ids(&pool, &[id])?;
    if server.accepted() != 1 {
        return Err(format!(
            "GOAWAY opened another QUIC connection ({})",
            server.accepted()
        ));
    }
    let requests_after_refusal = server.requests();

    drop(held);
    if pool.in_use_requests() != Some(0) {
        return Err(format!(
            "dropping the accepted stream left in-use {:?}",
            pool.in_use_requests()
        ));
    }
    // No in-flight request: the draining id is outside the active sum, and it
    // is not idle either.
    expect_known(
        &pool,
        ProtocolGauge::Known {
            active: 0,
            idle: 0,
            connecting: 0,
            draining: 1,
            pooled: 1,
        },
    )?;

    let again = pool.request(&RequestSpec::get(&url)).await.map(|_| ());
    let Err(again) = again else {
        return Err("a later request reused the draining id".to_string());
    };
    let again = again.to_string();
    if !again.contains(&id.to_string()) || !again.contains("will not take a new request") {
        return Err(format!("second refusal did not name id {id}: {again}"));
    }
    if server.requests() != requests_after_refusal {
        return Err(format!(
            "the refused request reached the server (requests {} -> {})",
            requests_after_refusal,
            server.requests()
        ));
    }
    if server.accepted() != 1 {
        return Err(format!(
            "a new handshake was inserted ({})",
            server.accepted()
        ));
    }
    expect_ids(&pool, &[id])?;
    expect_engine_total_unavailable(&pool)?;
    server.shutdown().await;
    Ok(())
}

/// The two `Display` sentences `peer_refused_new_request` still matches, taken
/// from the pinned h3 0.0.8 rather than typed in by hand.
///
/// `RemoteClosing` cannot be named, and `StreamError::ConnectionError` is a
/// private tuple variant, so those two refusals are still recognised by text.
/// A later h3 that rewords either sentence fails this test instead of letting
/// GOAWAY draining miss the refusal.
#[tokio::test]
async fn pinned_h3_goaway_refusal_display_strings() -> Result<(), String> {
    let _serial = serial();
    let closing = capture_remote_closing_display().await?;
    let rejected = capture_rejected_connection_close_display().await?;
    const REMOTE_CLOSING: &str = "Remote is closing the connection";
    const REQUEST_REJECTED: &str =
        "Connection error: Remote error: ApplicationClose: H3_REQUEST_REJECTED";
    if closing != REMOTE_CLOSING {
        return Err(format!(
            "pinned h3 RemoteClosing Display is {closing:?}, want {REMOTE_CLOSING:?}"
        ));
    }
    if rejected != REQUEST_REJECTED {
        return Err(format!(
            "pinned h3 connection-level H3_REQUEST_REJECTED Display is {rejected:?}, want {REQUEST_REJECTED:?}"
        ));
    }
    let source = read_rel("crates/core/src/net/h3.rs")?;
    let start = source
        .find("fn peer_refused_new_request(")
        .ok_or("missing peer_refused_new_request")?;
    let rest = &source[start..];
    let end = rest.find("\nfn ").unwrap_or(rest.len());
    let matcher = &rest[..end];
    for sentence in [REMOTE_CLOSING, "H3_REQUEST_REJECTED"] {
        if !matcher.contains(&format!("\"{sentence}\"")) {
            return Err(format!(
                "peer_refused_new_request no longer matches the pinned sentence {sentence:?}"
            ));
        }
    }
    Ok(())
}

#[test]
fn two_tasks_on_one_connection_report_global_active_one() -> Result<(), String> {
    let _serial = serial();
    let rt = harness_runtime()?;
    let ca = HarnessCa::generate()?;
    let server = rt.block_on(ca.spawn_h3_file(vec![b'E'; 64 * 1024], true))?;
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let engine = test_engine(dir.path(), 30, 1, TransportPref::Http3Only)?;
    let pool = install_harness(&engine, &ca)?;
    let url = format!("https://localhost:{}/shared.bin", server.addr.port());
    let first = engine
        .add(
            AddRequest::new(&url)
                .dest_dir(dir.path().join("downloads"))
                .filename("one.bin")
                .segments(1)
                .transport_pref(TransportPref::Http3Only),
        )
        .map_err(|e| format!("add first: {e}"))?;
    let second = engine
        .add(
            AddRequest::new(&url)
                .dest_dir(dir.path().join("downloads"))
                .filename("two.bin")
                .segments(1)
                .transport_pref(TransportPref::Http3Only),
        )
        .map_err(|e| format!("add second: {e}"))?;

    wait_until("both tasks holding one connection", RENDEZVOUS, || {
        let both_held = engine
            .task(first)
            .is_some_and(|task| task.active_requests >= 1)
            && engine
                .task(second)
                .is_some_and(|task| task.active_requests >= 1);
        both_held
            && pool.in_use_requests() == Some(2)
            && matches!(
                pool.connection_gauge(),
                ProtocolGauge::Known {
                    active: 1,
                    pooled: 1,
                    ..
                }
            )
    })?;
    if server.accepted() != 1 {
        return Err(format!(
            "two tasks opened {} QUIC connections",
            server.accepted()
        ));
    }
    expect_engine_unavailable(&engine)?;
    engine.shutdown();
    rt.block_on(server.shutdown());
    Ok(())
}

#[test]
fn h3_failure_then_h1_leaves_the_engine_snapshot_unavailable() -> Result<(), String> {
    let _serial = serial();
    // A certificate failure is terminal under S2 and must not exercise
    // fallback. Hold a silent UDP endpoint instead: the transport timeout is
    // eligible for Http12 fallback without ignoring authentication failure.
    let quic = std::net::UdpSocket::bind("127.0.0.1:0")
        .map_err(|e| format!("silent QUIC endpoint: {e}"))?;
    let port = quic
        .local_addr()
        .map_err(|e| format!("UDP addr: {e}"))?
        .port();
    let fallback_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let fallback_v4 = listen_and_reset(
        (std::net::Ipv4Addr::LOCALHOST, port),
        Arc::clone(&fallback_hits),
    )?;
    // localhost may be v4 or v6. A v6 bind miss is fine when v4 is listening.
    let fallback_v6 = listen_and_reset(
        (std::net::Ipv6Addr::LOCALHOST, port),
        Arc::clone(&fallback_hits),
    )
    .ok();
    let dir = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
    let engine = test_engine(dir.path(), 5, 1, TransportPref::PreferHttp3)?;
    let url = format!("https://localhost:{port}/fallback.bin");
    let id = engine
        .add(
            AddRequest::new(&url)
                .dest_dir(dir.path().join("downloads"))
                .filename("fallback.bin")
                .segments(1)
                .transport_pref(TransportPref::PreferHttp3),
        )
        .map_err(|e| format!("add https: {e}"))?;
    wait_until("H3 failure reached a terminal task", RENDEZVOUS, || {
        engine
            .task(id)
            .is_some_and(|task| task.state == TaskState::Failed)
    })?;
    if fallback_hits.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        return Err(
            "HTTP/1-2 fallback never connected; H3 failure was not followed by H1/H2".to_string(),
        );
    }
    expect_engine_unavailable(&engine)?;

    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("http bind: {e}"))?;
    let http_port = listener
        .local_addr()
        .map_err(|e| format!("http addr: {e}"))?
        .port();
    let http_body = b"h1-ok".to_vec();
    let server_body = http_body.clone();
    let http_server = std::thread::spawn(move || serve_cleartext(listener, &server_body));
    let http_url = format!("http://127.0.0.1:{http_port}/plain.bin");
    let http_id = engine
        .add(
            AddRequest::new(&http_url)
                .dest_dir(dir.path().join("downloads"))
                .filename("plain.bin")
                .segments(1)
                .transport_pref(TransportPref::NoHttp3),
        )
        .map_err(|e| format!("add http: {e}"))?;
    wait_until("H1 download completed", RENDEZVOUS, || {
        engine
            .task(http_id)
            .is_some_and(|task| task.state == TaskState::Completed)
    })?;
    let http_task = engine.task(http_id).ok_or("h1 task disappeared")?;
    let downloaded =
        std::fs::read(&http_task.final_path).map_err(|e| format!("read h1 download: {e}"))?;
    if downloaded != http_body {
        return Err(format!("h1 body was {downloaded:?}"));
    }
    expect_engine_unavailable(&engine)?;
    engine.shutdown();
    drop(fallback_v4);
    drop(fallback_v6);
    drop(quic);
    let _ = http_server.join();
    Ok(())
}

fn harness_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("udm-i3-harness")
        .build()
        .map_err(|e| format!("harness runtime: {e}"))
}

fn test_engine(
    root: &Path,
    stall_timeout_secs: u64,
    segments: u32,
    pref: TransportPref,
) -> Result<Engine, String> {
    let downloads = root.join("downloads");
    EngineBuilder::new()
        .paths(Paths {
            root: root.to_path_buf(),
            db_file: root.join("test.redb"),
            log_file: root.join("logs/test.log"),
            temp_dir: root.join("tmp"),
            default_download_dir: downloads.clone(),
            portable: true,
            exe_dir: Some(root.to_path_buf()),
        })
        .settings(Settings {
            http3_enabled: true,
            use_system_proxy: false,
            learn_alt_svc: false,
            segments_per_task: segments,
            min_segment_bytes: 64 * 1024,
            write_buffer_bytes: 16 * 1024,
            verify_checksums: false,
            connect_timeout_secs: 5,
            stall_timeout_secs,
            max_retries: 0,
            max_redirects: 2,
            transport_pref: pref,
            default_download_dir: downloads.to_string_lossy().into_owned(),
            ..Settings::default()
        })
        .build()
        .map_err(|e| format!("engine: {e}"))
}

fn install_harness(engine: &Engine, ca: &HarnessCa) -> Result<Arc<H3Pool>, String> {
    engine
        .install_h3_client_config(ca.quic_client_config()?)
        .map_err(|e| format!("install h3 client: {e}"))
}

fn wait_until(
    label: &str,
    timeout: Duration,
    mut pred: impl FnMut() -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pred() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if pred() {
        Ok(())
    } else {
        Err(format!("timed out waiting for {label}"))
    }
}

fn expect_known(pool: &H3Pool, want: ProtocolGauge) -> Result<(), String> {
    // Wait while the gauge is unpublished (Unknown); a concrete gauge is
    // judged immediately, exactly as before - only the patience for the
    // pool to publish under load grows, from ~200ms to the rendezvous
    // budget.
    let deadline = Instant::now() + RENDEZVOUS;
    let mut last = pool.connection_gauge();
    while matches!(last, ProtocolGauge::Unknown) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
        last = pool.connection_gauge();
    }
    if last == want {
        return Ok(());
    }
    Err(format!("gauge {last:?}, want {want:?}"))
}

fn expect_engine_unavailable(engine: &Engine) -> Result<(), String> {
    let snapshot = engine.net().engine_connections();
    if snapshot.active().is_some() || snapshot.protocols().is_some() {
        return Err(format!("engine snapshot published a number: {snapshot:?}"));
    }
    Ok(())
}

fn listen_and_reset(
    addr: impl std::net::ToSocketAddrs,
    hits: Arc<std::sync::atomic::AtomicUsize>,
) -> Result<std::thread::JoinHandle<()>, String> {
    let listener = std::net::TcpListener::bind(addr).map_err(|e| format!("fallback bind: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("fallback nonblocking: {e}"))?;
    Ok(std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((stream, _)) => {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    drop(stream);
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    }))
}

fn serve_cleartext(listener: std::net::TcpListener, body: &[u8]) {
    use std::io::{Read, Write};
    let _ = listener.set_nonblocking(true);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut served = 0;
    while served < 6 && Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") && request.len() < 8192 {
                    match stream.read(&mut byte) {
                        Ok(1) => request.push(byte[0]),
                        _ => break,
                    }
                }
                let head = request.starts_with(b"HEAD ");
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                if !head {
                    let _ = stream.write_all(body);
                }
                served += 1;
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

fn harness_pool(ca: &HarnessCa, timeout: Duration) -> Result<Arc<H3Pool>, String> {
    let config = ca.quic_client_config()?;
    H3Pool::with_client_config(timeout, "udm-i2", config)
        .map(|pool| Arc::new(pool))
        .map_err(|e| format!("pool: {e}"))
}

fn origin_url(addr: std::net::SocketAddr) -> String {
    format!("https://localhost:{}/file", addr.port())
}

async fn join_fetches(pool: &Arc<H3Pool>, url: &str, n: usize) -> Result<Vec<usize>, String> {
    let barrier = Arc::new(tokio::sync::Barrier::new(n));
    let mut joins = Vec::with_capacity(n);
    for _ in 0..n {
        let pool = Arc::clone(pool);
        let barrier = Arc::clone(&barrier);
        let url = url.to_string();
        joins.push(tokio::spawn(async move {
            barrier.wait().await;
            fetch(&pool, &url).await
        }));
    }
    let mut ids = Vec::with_capacity(n);
    for join in joins {
        ids.push(join.await.map_err(|e| format!("task: {e}"))??);
    }
    Ok(ids)
}

async fn accepted_response(pool: &H3Pool, url: &str) -> Result<usagi::net::h3::H3Response, String> {
    let response = tokio::time::timeout(RENDEZVOUS, pool.request(&RequestSpec::get(url)))
        .await
        .map_err(|_| "request timed out".to_string())?
        .map_err(|e| format!("request: {e}"))?;
    if response.status.as_u16() != 200 {
        return Err(format!("status {}", response.status));
    }
    Ok(response)
}

/// Poll the body once. `Pending` means the quinn recv half is inside h3-quinn's
/// read future. The future is then dropped, which is the cancel shape, and
/// that drop does not restore the half.
fn park_pending_chunk(response: &mut usagi::net::h3::H3Response) -> Result<(), String> {
    let mut chunk = std::pin::pin!(response.chunk());
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    match chunk.as_mut().poll(&mut cx) {
        std::task::Poll::Pending => Ok(()),
        std::task::Poll::Ready(result) => Err(format!(
            "chunk read finished while the server held the body ({result:?}); h3-quinn would not still own the recv half"
        )),
    }
}

/// Process-wide for the critical section only. Restores the previous hook on
/// drop, including when the assertion fails. Records every panic so a
/// `catch_unwind` inside `stop_sending` still fails this test: the hook runs
/// before the unwind is caught.
struct PanicRecord {
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    previous: std::sync::Arc<
        std::sync::Mutex<
            Option<Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>>,
        >,
    >,
}

impl PanicRecord {
    fn install() -> Self {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let previous = std::sync::Arc::new(std::sync::Mutex::new(Some(std::panic::take_hook())));
        let seen_hook = std::sync::Arc::clone(&seen);
        let previous_hook = std::sync::Arc::clone(&previous);
        std::panic::set_hook(Box::new(move |info| {
            let line = match info.location() {
                Some(location) => format!("{info} ({location})"),
                None => format!("{info}"),
            };
            if let Ok(mut seen) = seen_hook.lock() {
                seen.push(line);
            }
            if let Ok(guard) = previous_hook.lock() {
                if let Some(prev) = guard.as_ref() {
                    prev(info);
                }
            }
        }));
        Self { seen, previous }
    }

    fn into_recorded(self) -> Vec<String> {
        let recorded = self
            .seen
            .lock()
            .map(|seen| seen.clone())
            .unwrap_or_default();
        drop(self);
        recorded
    }
}

impl Drop for PanicRecord {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.previous.lock() {
            if let Some(prev) = slot.take() {
                std::panic::set_hook(prev);
            }
        }
    }
}

fn panic_payload(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "non-string panic payload".to_string()
}

async fn fetch(pool: &H3Pool, url: &str) -> Result<usize, String> {
    let mut response = tokio::time::timeout(RENDEZVOUS, pool.request(&RequestSpec::get(url)))
        .await
        .map_err(|_| "request timed out".to_string())?
        .map_err(|e| format!("request failed: {e}"))?;
    if response.status.as_u16() != 200 {
        return Err(format!("status {}", response.status));
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await.map_err(|e| format!("body: {e}"))? {
            Some(chunk) => body.extend_from_slice(&chunk),
            None => break,
        }
    }
    let text = String::from_utf8(body).map_err(|e| format!("body is not utf-8: {e}"))?;
    if text != HARNESS_MARKER {
        return Err(format!("body was {text:?}"));
    }
    Ok(response.connection_id)
}

fn expect_ids(pool: &H3Pool, want: &[usize]) -> Result<(), String> {
    match pool.recorded_stable_ids() {
        H3StableIds::Known(ids) if ids == want => Ok(()),
        other => Err(format!("recorded ids {other:?}, want {want:?}")),
    }
}

fn expect_membership(pool: &H3Pool, pooled: u64, connecting: u64) -> Result<(), String> {
    match pool.connection_gauge() {
        ProtocolGauge::Known {
            active: 0,
            idle,
            connecting: got_connecting,
            draining: 0,
            pooled: got_pooled,
        } if got_pooled == pooled && got_connecting == connecting && idle == pooled => Ok(()),
        other => Err(format!(
            "gauge {other:?}, want pooled {pooled} connecting {connecting} with in-use ended"
        )),
    }
}

fn expect_engine_total_unavailable(pool: &H3Pool) -> Result<(), String> {
    let snapshot = EngineConnections::from_protocols(
        ProtocolGauge::Unknown,
        ProtocolGauge::Unknown,
        pool.connection_gauge(),
    );
    if snapshot.active().is_some() || snapshot.protocols().is_some() {
        return Err("engine snapshot published a number from the H3 gauge alone".to_string());
    }
    Ok(())
}

async fn wait_for_count(mut read: impl FnMut() -> usize, want: usize) -> usize {
    let deadline = Instant::now() + RENDEZVOUS;
    let mut seen = read();
    while seen < want && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
        seen = read();
    }
    seen
}

/// GOAWAY the client driver has already applied. h3 0.0.8 returns this as
/// `StreamError::RemoteClosing`, whose `Display` is the whole error text.
async fn capture_remote_closing_display() -> Result<String, String> {
    let ca = HarnessCa::generate()?;
    let server = ca.spawn_h3_file(b"pin".to_vec(), true).await?;
    let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
        .map_err(|e| format!("endpoint: {e}"))?;
    endpoint.set_default_client_config(ca.quic_client_config()?);
    let quinn_conn = tokio::time::timeout(
        RENDEZVOUS,
        endpoint
            .connect(server.addr, "localhost")
            .map_err(|e| format!("connect: {e}"))?,
    )
    .await
    .map_err(|_| "quic handshake timed out".to_string())?
    .map_err(|e| format!("quic handshake: {e}"))?;
    let (mut driver, mut sender) = tokio::time::timeout(
        RENDEZVOUS,
        h3::client::new(h3_quinn::Connection::new(quinn_conn)),
    )
    .await
    .map_err(|_| "h3 handshake timed out".to_string())?
    .map_err(|e| format!("h3 handshake: {e}"))?;
    let drive = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });

    let opened = http::Request::get("https://localhost/pin")
        .body(())
        .map_err(|e| format!("request: {e}"))?;
    let mut stream = tokio::time::timeout(RENDEZVOUS, sender.send_request(opened))
        .await
        .map_err(|_| "first request timed out".to_string())?
        .map_err(|e| format!("first request: {e}"))?;
    stream.finish().await.map_err(|e| format!("finish: {e}"))?;
    tokio::time::timeout(RENDEZVOUS, stream.recv_response())
        .await
        .map_err(|_| "response timed out".to_string())?
        .map_err(|e| format!("response: {e}"))?;

    server.goaway().await?;
    let deadline = Instant::now() + RENDEZVOUS;
    let mut last = String::new();
    while Instant::now() < deadline {
        let late = http::Request::get("https://localhost/late")
            .body(())
            .map_err(|e| format!("request: {e}"))?;
        match sender.send_request(late).await {
            Ok(_) => {
                drive.abort();
                server.shutdown().await;
                return Err("pinned h3 accepted a request after GOAWAY".to_string());
            }
            Err(err) => {
                last = err.to_string();
                if last == "Remote is closing the connection" {
                    drive.abort();
                    server.shutdown().await;
                    return Ok(last);
                }
                // The server can reset the new stream before the client driver
                // has applied GOAWAY. That reset is the typed code check.
                if !last.contains("H3_REQUEST_REJECTED") {
                    drive.abort();
                    server.shutdown().await;
                    return Err(format!(
                        "pinned h3 GOAWAY produced {last:?} before RemoteClosing"
                    ));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    drive.abort();
    server.shutdown().await;
    Err(format!(
        "pinned h3 0.0.8 did not surface RemoteClosing within the rendezvous budget (last error: {last:?})"
    ))
}

/// QUIC application close with code `H3_REQUEST_REJECTED`. h3 wraps that as
/// `StreamError::ConnectionError`, which this crate cannot match.
async fn capture_rejected_connection_close_display() -> Result<String, String> {
    let ca = HarnessCa::generate()?;
    let endpoint = quinn::Endpoint::server(
        ca.quic_server_config()?,
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 0)),
    )
    .map_err(|e| format!("quic bind: {e}"))?;
    let addr = endpoint
        .local_addr()
        .map_err(|e| format!("quic address: {e}"))?;
    let (release, mut release_rx) = tokio::sync::watch::channel(false);
    let server = tokio::spawn(async move {
        let Some(incoming) = endpoint.accept().await else {
            return;
        };
        let Ok(conn) = incoming.await else {
            return;
        };
        if !*release_rx.borrow_and_update() {
            let _ = release_rx.changed().await;
        }
        let Ok(code) = quinn::VarInt::from_u64(h3::error::Code::H3_REQUEST_REJECTED.value()) else {
            return;
        };
        conn.close(code, b"rejected");
        endpoint.wait_idle().await;
    });

    let mut client = quinn::Endpoint::client("0.0.0.0:0".parse().unwrap())
        .map_err(|e| format!("client endpoint: {e}"))?;
    client.set_default_client_config(ca.quic_client_config()?);
    let quinn_conn = tokio::time::timeout(
        RENDEZVOUS,
        client
            .connect(addr, "localhost")
            .map_err(|e| format!("connect: {e}"))?,
    )
    .await
    .map_err(|_| "quic handshake timed out".to_string())?
    .map_err(|e| format!("quic handshake: {e}"))?;
    let (mut driver, mut sender) = tokio::time::timeout(
        RENDEZVOUS,
        h3::client::new(h3_quinn::Connection::new(quinn_conn.clone())),
    )
    .await
    .map_err(|_| "h3 handshake timed out".to_string())?
    .map_err(|e| format!("h3 handshake: {e}"))?;
    let drive = tokio::spawn(async move {
        let _ = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
    });
    let _ = release.send(true);
    let deadline = Instant::now() + RENDEZVOUS;
    while quinn_conn.close_reason().is_none() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let finish = async {
        drive.abort();
        client.close(quinn::VarInt::from_u32(0), b"pin client done");
        let _ = tokio::time::timeout(Duration::from_secs(2), server).await;
    };
    if quinn_conn.close_reason().is_none() {
        finish.await;
        return Err("pinned peer did not close with H3_REQUEST_REJECTED".to_string());
    }
    let request = http::Request::get("https://localhost/rejected")
        .body(())
        .map_err(|e| format!("request: {e}"))?;
    let captured = match tokio::time::timeout(RENDEZVOUS, sender.send_request(request)).await {
        Ok(Err(err)) => Ok(err.to_string()),
        Ok(Ok(_)) => {
            Err("pinned h3 accepted a request after H3_REQUEST_REJECTED close".to_string())
        }
        Err(_) => Err("pinned send_request timed out after H3_REQUEST_REJECTED close".to_string()),
    };
    finish.await;
    captured
}

fn workspace_root() -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path
}

fn read_rel(rel: &str) -> Result<String, String> {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
}

fn function_body<'a>(text: &'a str, signature: &str) -> Result<&'a str, String> {
    let start = text
        .find(signature)
        .ok_or_else(|| format!("missing `{signature}`"))?;
    let rest = &text[start..];
    let after_line = rest.find('\n').unwrap_or(rest.len());
    let tail = &rest[after_line..];
    let end = [
        "\n    pub fn ",
        "\n    pub async fn ",
        "\npub fn ",
        "\nimpl ",
    ]
    .iter()
    .filter_map(|marker| tail.find(marker))
    .min()
    .unwrap_or(tail.len());
    Ok(&rest[..after_line + end])
}

fn assert_tree_has_no_harness_ca(dir: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            assert_tree_has_no_harness_ca(&path)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if text.contains("HarnessCa") {
            return Err(format!("{} names HarnessCa", path.display()));
        }
    }
    Ok(())
}

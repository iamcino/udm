#[path = "common/mod.rs"]
mod common;
#[path = "common/transport_fixture.rs"]
mod fixture;

use fixture::{Fixture, ETAG, SIZE};
use sha2::{Digest, Sha256};

async fn preflight(f: &Fixture) {
    let manifest = f.manifest();
    println!("S0 manifest: {manifest}");
    let mut responses = Vec::new();
    for peer in [f.v4, f.v6] {
        let response = f
            .client(peer)
            .get(f.url())
            .header("range", "bytes=0-1023")
            .header("if-range", ETAG)
            .send()
            .await
            .unwrap();
        assert_eq!(response.remote_addr(), Some(peer));
        assert_eq!(response.version(), http::Version::HTTP_2);
        assert_eq!(response.status(), 206);
        assert_eq!(response.headers()["etag"], ETAG);
        assert_eq!(response.headers()["content-encoding"], "identity");
        assert_eq!(
            response.headers()["content-range"],
            format!("bytes 0-1023/{SIZE}")
        );
        responses.push(response.bytes().await.unwrap());
    }
    assert_eq!(responses[0], responses[1]);
    assert_eq!(responses[0].as_ref(), &fixture::payload()[..1024]);
}

#[tokio::test]
async fn s0_dual_stack_same_representation_preflight_and_barriers() {
    let f = Fixture::start().await;
    preflight(&f).await;
    for gate in &f.gates {
        gate.events.lock().unwrap().clear();
        gate.hold();
    }
    let mut jobs = Vec::new();
    for (index, peer) in [f.v4, f.v6].into_iter().enumerate() {
        let client = f.client(peer);
        let url = f.url();
        let (start, end) = fixture::RANGES[index];
        jobs.push(tokio::spawn(async move {
            client
                .get(url)
                .header("range", format!("bytes={start}-{end}"))
                .header("if-range", ETAG)
                .send()
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap()
        }));
    }
    for stage in 1..=3 {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if f.gates[..2]
                    .iter()
                    .all(|g| g.events.lock().unwrap().iter().any(|e| e.stage == stage))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("NOT RUN: both fixture paths did not reach barrier");
        assert!(
            jobs.iter().all(|j| !j.is_finished()),
            "both paths overlap at barrier"
        );
        for gate in &f.gates[..2] {
            gate.release(stage);
        }
    }
    for (index, job) in jobs.into_iter().enumerate() {
        let (start, end) = fixture::RANGES[index];
        assert_eq!(
            job.await.unwrap().as_ref(),
            &fixture::payload()[start..=end]
        );
    }
    // Single-path four-range baseline, including the uneven tail. This is
    // fixture validation, never a claim of engine two-path safety.
    let mut output = Vec::new();
    for (start, end) in fixture::RANGES {
        let r = f
            .client(f.v4)
            .get(f.url())
            .header("range", format!("bytes={start}-{end}"))
            .header("if-range", ETAG)
            .send()
            .await
            .unwrap();
        output.extend_from_slice(&r.bytes().await.unwrap());
    }
    assert_eq!(output.len(), SIZE);
    assert_eq!(Sha256::digest(&output), Sha256::digest(fixture::payload()));
    for gate in &f.gates[..2] {
        let events = gate.events.lock().unwrap();
        assert!(events.iter().all(|e| e.request > 0
            && e.connection > 0
            && !e.uri.is_empty()
            && e.protocol == "h2"
            && ["v4", "v6"].contains(&e.family)));
        assert!(events.iter().any(|e| e.stage == 2
            && e.bytes == 1024
            && e.range.is_some()
            && e.if_range.as_deref() == Some(ETAG)));
        println!("S0 events: {events:?}");
    }
}

// Cases stay visible in ordinary cargo test output. Their names and reasons
// prevent unimplemented S1/S2 behavior from becoming a green proof by omission.
macro_rules! deferred {
    ($name:ident, $reason:literal) => {
        #[test]
        #[ignore = $reason]
        fn $name() {
            panic!($reason);
        }
    };
}
// S2 A1 mixed, A6/A7/A8/A13 executable drivers: transport_s2.rs.
// Mixed A2-A5/A9-A11 lifecycle extensions remain NOT RUN except the named
// representation and policy controls exercised in that file; see S2 evidence.
// S3 two independent H2 sessions on ONE fixed family/path, with accept-id/ALPN
// proof and named lifecycle/representation/checkpoint controls: transport_s3.rs.
// S4 multi-QUIC, S5 mux and WAN/benefit campaigns remain NOT RUN.
deferred!(
    a12_probe_not_pass_case,
    "NOT RUN: historical Http12-only option b probe; countersigned option a retained"
);
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s0_h3_fixture_and_single_path_engine_barriers() {
    use std::time::Duration;
    use usagi::engine::proof::{ProofBarrier, ProofPoint};
    use usagi::net::h3::H3Pool;
    use usagi::net::stack::RequestSpec;
    use usagi::{AddRequest, EngineBuilder, Paths, Settings};
    let f = Fixture::start().await;
    println!("S0 manifest before requests: {}", f.manifest());
    let pool = H3Pool::with_client_config(
        Duration::from_secs(5),
        "S0 proof",
        f.ca.quic_client_config().unwrap(),
    )
    .unwrap();
    let spec = RequestSpec::get(&format!("https://localhost:{}/proof.bin", f.h3.port()))
        .with_range(0, Some(1023))
        .with_if_range(Some(ETAG.into()));
    let mut opened = pool.request(&spec).await.unwrap();
    assert_eq!(opened.remote_addr, f.h3);
    assert_eq!(opened.status, 206);
    assert_eq!(opened.headers["etag"], ETAG);
    let mut bytes = Vec::new();
    while let Some(chunk) = opened.chunk().await.unwrap() {
        bytes.extend_from_slice(&chunk);
    }
    assert_eq!(bytes, &fixture::payload()[..1024]);
    pool.close_all("S0 fixture checked").await;

    for repetition in 0..3 {
        for point in [
            ProofPoint::BeforeWrite,
            ProofPoint::AfterWriteBeforeSync,
            ProofPoint::AfterSyncBeforeTransaction,
            ProofPoint::TransactionCommittedBeforeAcknowledgment,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let paths = Paths {
                root: root.into(),
                db_file: root.join("udm.redb"),
                log_file: root.join("logs/udm.log"),
                temp_dir: root.join("tmp"),
                default_download_dir: root.join("downloads"),
                portable: true,
                exe_dir: Some(root.into()),
            };
            let settings = Settings {
                http3_enabled: false,
                use_system_proxy: false,
                segments_per_task: 1,
                max_retries: 0,
                default_download_dir: root.join("downloads").to_string_lossy().into(),
                ..Settings::default()
            };
            let engine = EngineBuilder::new()
                .paths(paths)
                .settings(settings)
                .build()
                .unwrap();
            engine.net().install_test_root_der(f.ca.ca_der()).unwrap();
            let id = engine
                .add(
                    AddRequest::new(f.url())
                        .filename("proof.bin")
                        .start_paused(true),
                )
                .unwrap();
            let barrier = ProofBarrier::new(point);
            engine.install_proof_barrier(id, barrier.clone()).unwrap();
            engine.resume(id).unwrap();
            let reached = tokio::time::timeout(Duration::from_secs(30), async {
                while !barrier.entered() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await;
            let stored_at_barrier = reached.is_ok().then(|| {
                engine
                    .store()
                    .list_tasks()
                    .map(|tasks| tasks.into_iter().find(|task| task.record.id == id))
            });
            // Always release before assertions or shutdown, including a failed rendezvous.
            barrier.release();
            reached.expect("NOT RUN: actual engine barrier not reached");
            let stored = stored_at_barrier.unwrap().unwrap().unwrap();
            let committed: u64 = stored.segments.iter().map(|segment| segment.done).sum();
            if point == ProofPoint::TransactionCommittedBeforeAcknowledgment {
                assert!(stored.record.progress_checkpoint.is_some());
                assert_eq!(committed, barrier.bytes_at_barrier());
            } else if point != ProofPoint::BeforeWrite {
                assert!(committed <= barrier.bytes_at_barrier());
            }
            assert_ne!(stored.record.state, usagi::store::TaskState::Completed);
            assert!(
                barrier.bytes_at_barrier() > 0,
                "barrier must cover payload, not initial metadata"
            );
            tokio::time::timeout(Duration::from_secs(30), async {
                loop {
                    let task = engine.task(id).unwrap();
                    if task.state.is_terminal() {
                        assert_eq!(
                            task.state,
                            usagi::store::TaskState::Completed,
                            "{:?}",
                            task.error
                        );
                        assert_eq!(std::fs::read(task.final_path).unwrap(), fixture::payload());
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("engine baseline completes after barrier release");
            let evidence = engine.transport_open_evidence(id).unwrap();
            assert_eq!(evidence.pending, 0);
            assert_eq!(evidence.dropped, 0);
            assert!(!evidence.observations.is_empty());
            assert!(evidence.observations.iter().all(|o| o.task == id
                && o.protocol == Some(usagi::store::TransportKind::Http2)
                && o.open_to_headers.is_some()
                && o.observed_family.is_some()));
            assert_eq!(
                engine.net().engine_connections(),
                usagi::net::EngineConnections::Unavailable
            );
            println!("S0 repetition={repetition} barrier={point:?} PASS {evidence:?}");
            engine.shutdown();
        }
    }
}

#[tokio::test]
async fn s0_fixture_route_loss_controls_both_directions() {
    for repetition in 0..3 {
        for victim in 0..2 {
            for stage in 1..=3 {
                let f = Fixture::start().await;
                println!("S0 failure-control manifest: {}", f.manifest());
                f.gates[victim].hold();
                let peers = [f.v4, f.v6];
                let client = f.client(peers[victim]);
                let url = f.url();
                let victim_request = tokio::spawn(async move {
                    match client
                        .get(url)
                        .header("range", "bytes=0-65535")
                        .header("if-range", ETAG)
                        .send()
                        .await
                    {
                        Ok(response) => response.bytes().await.map(|_| ()),
                        Err(error) => Err(error),
                    }
                });
                for reached_stage in 1..=stage {
                    tokio::time::timeout(std::time::Duration::from_secs(30), async {
                        loop {
                            if f.gates[victim]
                                .events
                                .lock()
                                .unwrap()
                                .iter()
                                .any(|e| e.stage == reached_stage)
                            {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("NOT RUN: victim fixture barrier not reached");
                    if reached_stage < stage {
                        f.gates[victim].release(reached_stage);
                    }
                }
                f.kill_path(victim);
                assert!(
                    tokio::time::timeout(std::time::Duration::from_secs(5), victim_request)
                        .await
                        .expect("killed fixture route must terminate")
                        .unwrap()
                        .is_err()
                );
                let survivor = f
                    .client(peers[1 - victim])
                    .get(f.url())
                    .header("range", "bytes=65536-131071")
                    .header("if-range", ETAG)
                    .send()
                    .await
                    .unwrap()
                    .bytes()
                    .await
                    .unwrap();
                assert_eq!(survivor.as_ref(), &fixture::payload()[65536..=131071]);
                println!("S0 fixture control repetition={repetition} victim={victim} barrier={stage} PASS");
            }
        }
    }
}

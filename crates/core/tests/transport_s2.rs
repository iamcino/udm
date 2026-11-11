//! S2 bounded mixed-family Proof A: no WAN/benefit claim.
#[path = "common/mod.rs"]
mod common;
#[path = "common/transport_fixture.rs"]
mod fixture;
use fixture::{Fixture, ETAG, SIZE};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Arc, time::Duration};
use usagi::config::TransportPref;
use usagi::net::{
    altsvc::{origin_key, AltSvcEntry},
    h3::H3Pool,
    stack::RequestSpec,
};
use usagi::store::{
    records::{RepresentationBound, SegmentState, TaskRecord, TransportSummary},
    TaskState, TransportKind,
};
use usagi::{AddRequest, Engine, EngineBuilder, Paths, Settings};
fn engine(f: &Fixture, root: &Path, pref: TransportPref, trust_h3: bool) -> Engine {
    let e = EngineBuilder::new()
        .paths(Paths {
            root: root.into(),
            db_file: root.join("udm.redb"),
            log_file: root.join("logs/udm.log"),
            temp_dir: root.join("tmp"),
            default_download_dir: root.join("downloads"),
            portable: true,
            exe_dir: Some(root.into()),
        })
        .settings(Settings {
            http3_enabled: true,
            dual_stack_paths: true,
            use_system_proxy: false,
            transport_pref: pref,
            segments_per_task: 4,
            min_segment_bytes: 65536,
            write_buffer_bytes: 1024,
            max_retries: 0,
            persist_interval_ms: 200,
            default_download_dir: root.join("downloads").to_string_lossy().into(),
            ..Settings::default()
        })
        .build()
        .unwrap();
    e.net().install_test_root_der(f.ca.ca_der()).unwrap();
    if trust_h3 {
        e.net().install_h3_pool(Arc::new(
            H3Pool::with_client_config(
                Duration::from_secs(2),
                "S2-proof",
                f.ca.quic_client_config().unwrap(),
            )
            .unwrap(),
        ));
    }
    let mut entry = AltSvcEntry::new("localhost", f.h3.port(), 3600);
    entry.origin = origin_key("localhost", f.v4.port());
    e.net().alt_svc_store().put(entry);
    e
}
async fn until(mut p: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !p() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("S2 required rendezvous");
}
fn payload(f: &Fixture, path: usize) -> bool {
    f.gates[path]
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|e| e.stage == 1 && e.range.as_deref() != Some("bytes=0-0"))
}
fn add(e: &Engine, url: String) -> u64 {
    e.add(
        AddRequest::new(url)
            .filename("proof.bin")
            .start_paused(true),
    )
    .unwrap()
}
async fn terminal(e: &Engine, id: u64, expected: TaskState) {
    until(|| e.task(id).unwrap().state.is_terminal()).await;
    let task = e.task(id).unwrap();
    assert_eq!(task.state, expected, "{task:?}");
    assert_eq!(task.active_requests, 0);
    until(|| e.store().get_task(id).unwrap().unwrap().state == expected).await;
    if expected == TaskState::Completed {
        let bytes = std::fs::read(&task.final_path).unwrap();
        assert_eq!(bytes, fixture::payload());
        assert_eq!(Sha256::digest(&bytes), Sha256::digest(fixture::payload()));
    } else {
        assert!(!Path::new(&task.final_path).exists());
    }
    assert_eq!(
        e.net().engine_connections(),
        usagi::net::EngineConnections::Unavailable
    );
}
fn logical(e: &Engine, id: u64, url: &str) {
    let r = e.store().get_task(id).unwrap().unwrap();
    assert_eq!(r.final_url, url);
    assert_eq!(r.representation_bound.as_ref().unwrap().final_url, url);
    assert!(r.logical_url_identity);
    if let Some(checkpoint) = &r.progress_checkpoint {
        // Reproduce the documented fingerprint inputs, including logical final_url.
        let segments = e.store().get_segments(id).unwrap();
        let bytes = serde_json::to_vec(&(
            r.id,
            &r.url,
            &r.final_url,
            &r.dest_dir,
            &r.filename,
            r.total_size,
            &r.etag,
            &r.last_modified,
            &r.content_type,
            &r.expected_hash,
            &segments,
        ))
        .unwrap();
        assert_eq!(
            serde_json::to_value(checkpoint).unwrap()["snapshot"],
            blake3::hash(&bytes).to_hex().as_str()
        );
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a1_a7_a8_s2_mixed_success_keeps_v6_and_frozen_endpoint() {
    for rep in 0..3 {
        let Some(f) = Fixture::start().await else {
            return;
        };
        println!("S2 A1/A7/A8 rep={rep} {}", f.manifest());
        for i in [1, 2] {
            f.gates[i].hold();
        }
        let dir = tempfile::tempdir().unwrap();
        let e = engine(&f, dir.path(), TransportPref::Auto, true);
        let id = add(&e, f.url());
        e.resume(id).unwrap();
        until(|| payload(&f, 1) && payload(&f, 2)).await;
        until(|| e.task(id).unwrap().paths.len() == 2).await;
        let task = e.task(id).unwrap();
        assert_eq!(task.active_requests, 2);
        assert!(task.paths.iter().all(|p| p.active_ranges == 1));
        assert!(matches!(task.transport_summary, TransportSummary::Mixed(_)));
        assert_eq!(task.transport, None);
        logical(&e, id, &f.url());
        // The first payload headers have already been admitted. Later segment
        // responses update the logical cache to an unreachable endpoint.
        f.gates[2]
            .headers
            .lock()
            .unwrap()
            .push(("alt-svc".into(), "h3=\":9\"; ma=3600".into()));
        let mut advertisement = e
            .net()
            .open(
                &RequestSpec::get(&f.url())
                    .with_range(0, Some(0))
                    .with_if_range(Some(ETAG.into())),
                TransportPref::Auto,
            )
            .await
            .unwrap();
        assert_eq!(advertisement.remote_addr, Some(f.h3));
        assert_eq!(advertisement.final_url, f.url());
        assert_eq!(advertisement.body.chunk().await.unwrap().unwrap().len(), 1);
        advertisement.body.abort();
        for stage in 1..=3 {
            for i in [1, 2] {
                f.gates[i].release(stage);
            }
        }
        terminal(&e, id, TaskState::Completed).await;
        logical(&e, id, &f.url());
        let store = e.net().alt_svc_store();
        let key = origin_key("localhost", f.v4.port());
        assert_eq!(store.get(&key).unwrap().port, 9);
        assert!(store.get(&origin_key("localhost", f.h3.port())).is_none());
        let task = e.task(id).unwrap();
        assert_eq!(task.paths.len(), 2);
        assert!(task.paths.iter().all(|p| p.active_ranges == 0));
        assert_eq!(
            e.store()
                .get_task(id)
                .unwrap()
                .unwrap()
                .representation_restarts,
            0
        );
        let ev = e.transport_open_evidence(id).unwrap();
        assert_eq!(ev.dropped, 0);
        let mut ranges: Vec<_> = ev
            .observations
            .iter()
            .map(|o| {
                assert_eq!(
                    o.observed_family,
                    Some(if o.protocol == Some(TransportKind::Http3) {
                        "v4"
                    } else {
                        "v6"
                    })
                );
                o.range.unwrap()
            })
            .collect();
        ranges.sort();
        let mut end = 0;
        for (start, last) in ranges {
            assert_eq!(start, end);
            end = last.unwrap() + 1;
        }
        assert_eq!(end, SIZE as u64);
        for event in f.gates[2].events.lock().unwrap().iter() {
            assert!(
                event
                    .uri
                    .starts_with(&format!("https://localhost:{}/", f.v4.port())),
                "logical authority {}",
                event.uri
            );
            assert_eq!(event.protocol, "h3");
            assert_eq!(event.family, "v4");
            assert!(event.request > 0 && event.connection > 0);
            if event.range.is_some() {
                assert_eq!(event.if_range.as_deref(), Some(ETAG));
            }
        }
        println!("S2 A1/A7/A8 PASS rep={rep} task={task:?} evidence={ev:?}");
        e.shutdown();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r1_s2_never_h3_h3_only_and_same_family_transport_fallback() {
    for rep in 0..3 {
        for mode in ["never", "only", "fallback"] {
            let Some(f) = Fixture::start().await else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            let pref = match mode {
                "never" => TransportPref::NoHttp3,
                "only" => TransportPref::Http3Only,
                _ => TransportPref::Auto,
            };
            let e = engine(&f, dir.path(), pref, true);
            if mode == "fallback" {
                let mut entry = AltSvcEntry::new("localhost", 9, 3600);
                entry.origin = origin_key("localhost", f.v4.port());
                e.net().alt_svc_store().put(entry);
            }
            let id = add(&e, f.url());
            e.resume(id).unwrap();
            terminal(&e, id, TaskState::Completed).await;
            let ev = e.transport_open_evidence(id).unwrap();
            if mode == "only" {
                assert!(ev
                    .observations
                    .iter()
                    .all(|o| o.protocol == Some(TransportKind::Http3)));
                assert!(!payload(&f, 1));
            } else {
                assert!(payload(&f, 0) && payload(&f, 1));
                assert!(!payload(&f, 2));
                assert_eq!(e.task(id).unwrap().paths.len(), 2);
            }
            println!("S2 R1 PASS rep={rep} mode={mode} {ev:?}");
            e.shutdown();
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a6_s2_relative_redirect_uses_logical_base_on_every_hop() {
    for rep in 0..3 {
        let Some(f) = Fixture::start().await else {
            return;
        };
        for gate in &f.gates {
            *gate.redirect.lock().unwrap() = Some("proof.bin".into());
        }
        let dir = tempfile::tempdir().unwrap();
        let e = engine(&f, dir.path(), TransportPref::Auto, true);
        let url = f.url().replace("proof.bin", "redirect.bin");
        let mut response = e
            .net()
            .open(
                &RequestSpec::get(&url).with_range(0, Some(0)),
                TransportPref::Auto,
            )
            .await
            .unwrap();
        assert_eq!(response.final_url, f.url());
        assert_eq!(response.remote_addr, Some(f.h3));
        assert_eq!(
            response.body.chunk().await.unwrap().unwrap().as_ref(),
            &fixture::payload()[..1]
        );
        response.body.abort();
        let id = add(&e, url);
        e.resume(id).unwrap();
        terminal(&e, id, TaskState::Completed).await;
        logical(&e, id, &f.url());
        let events = f.gates[2].events.lock().unwrap();
        assert!(events.iter().any(|ev| ev.uri.ends_with("/redirect.bin")));
        assert!(events.iter().any(|ev| ev.uri.ends_with("/proof.bin")));
        assert!(events.iter().all(|ev| ev
            .uri
            .starts_with(&format!("https://localhost:{}/", f.v4.port()))));
        println!("S2 A6 relative PASS rep={rep} {events:?}");
        drop(events);
        e.shutdown();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a6_a7_s2_alternate_port_redirect_and_certificate_fail_closed() {
    for rep in 0..3 {
        for mode in ["absolute-alternate", "certificate", "different-host"] {
            let Some(f) = Fixture::start().await else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            let e = engine(&f, dir.path(), TransportPref::Auto, mode != "certificate");
            let url = if mode == "absolute-alternate" {
                *f.gates[2].redirect.lock().unwrap() =
                    Some(format!("https://localhost:{}/proof.bin", f.h3.port()));
                f.url().replace("proof.bin", "redirect.bin")
            } else {
                f.url()
            };
            if mode == "different-host" {
                e.net()
                    .alt_svc_store()
                    .put(AltSvcEntry::new("unused", 443, 0));
                f.gates[2]
                    .headers
                    .lock()
                    .unwrap()
                    .push(("alt-svc".into(), "h3=\"evil.invalid:9\"; ma=3600".into()));
            }
            let id = add(&e, url);
            e.resume(id).unwrap();
            if mode == "different-host" {
                terminal(&e, id, TaskState::Completed).await;
                assert_eq!(
                    e.net()
                        .alt_svc_store()
                        .get(&origin_key("localhost", f.v4.port()))
                        .unwrap()
                        .port,
                    f.h3.port()
                );
            } else {
                terminal(&e, id, TaskState::Failed).await;
                assert!(!payload(&f, 0) && !payload(&f, 1));
                let r = e.store().get_task(id).unwrap().unwrap();
                assert_eq!(
                    r.representation_restarts,
                    if mode == "certificate" { 0 } else { 1 }
                );
                if mode == "certificate" {
                    assert!(r.error.unwrap().contains("TLS"));
                }
            }
            println!("S2 A6/A7 PASS rep={rep} mode={mode}");
            e.shutdown();
        }
    }
}
fn seed_legacy(f: &Fixture, root: &Path, synthetic: bool) {
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    let store = usagi::store::Store::open(root.join("udm.redb")).unwrap();
    let id = store.next_task_id().unwrap();
    assert_eq!(id, 1);
    let mut r = TaskRecord::new(
        id,
        &f.url(),
        &root.join("downloads"),
        Some("proof.bin".into()),
    );
    r.logical_url_identity = false;
    r.segments_planned = 4;
    r.state = TaskState::Paused;
    r.final_url = if synthetic {
        format!("https://localhost:{}/proof.bin", f.h3.port())
    } else {
        f.url()
    };
    r.representation_bound = Some(RepresentationBound::none_scope(
        &r.final_url,
        ETAG,
        Some(SIZE as u64),
    ));
    r.total_size = Some(SIZE as u64);
    r.etag = Some(ETAG.into());
    r.transport = Some(TransportKind::Http3);
    r.accept_ranges = true;
    let mut file = std::fs::File::create(r.part_path()).unwrap();
    file.set_len(SIZE as u64).unwrap();
    if !synthetic {
        std::io::Write::write_all(&mut file, &fixture::payload()[..4096]).unwrap();
    }
    let mut segment = SegmentState::new(0, 0, SIZE as u64 - 1);
    segment.done = 4096;
    r.prepare_progress_checkpoint(&[segment], &file).unwrap();
    assert!(r.progress_checkpoint.is_some());
    store.save(&[r], &[(id, vec![segment])], true).unwrap();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a13_s2_synthetic_url_discards_coverage_without_spending_restart() {
    for rep in 0..3 {
        for mismatch in ["none", "etag", "total", "redirect"] {
            for pref in [TransportPref::Auto, TransportPref::NoHttp3] {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                let dir = tempfile::tempdir().unwrap();
                seed_legacy(&f, dir.path(), true);
                for gate in &f.gates {
                    gate.hold();
                }
                let e = engine(&f, dir.path(), pref, true);
                e.resume(1).unwrap();
                let v4 = if pref == TransportPref::Auto { 2 } else { 0 };
                until(|| {
                    (payload(&f, v4) && payload(&f, 1)) || e.task(1).unwrap().state.is_terminal()
                })
                .await;
                assert!(
                    payload(&f, v4) && payload(&f, 1),
                    "A13 rep={rep} pref={pref:?} task={:?} evidence={:?} events={:?}",
                    e.task(1),
                    e.transport_open_evidence(1),
                    f.gates
                        .iter()
                        .map(|g| g.events.lock().unwrap().clone())
                        .collect::<Vec<_>>()
                );
                until(|| e.transport_open_evidence(1).unwrap().observations.len() >= 2).await;
                logical(&e, 1, &f.url());
                let r = e.store().get_task(1).unwrap().unwrap();
                assert_eq!(r.representation_restarts, 0);
                assert!(r.progress_checkpoint.is_none());
                assert_eq!(e.task(1).unwrap().received, 0);
                assert!(e
                    .transport_open_evidence(1)
                    .unwrap()
                    .observations
                    .iter()
                    .any(|o| o.range.unwrap().0 == 0));
                if mismatch != "none" {
                    for gate in &f.gates {
                        *gate.fault.lock().unwrap() = Some(mismatch);
                    }
                }
                for gate in &f.gates {
                    gate.release(3);
                }
                terminal(
                    &e,
                    1,
                    if mismatch != "none" {
                        TaskState::Failed
                    } else {
                        TaskState::Completed
                    },
                )
                .await;
                assert_eq!(
                    e.store()
                        .get_task(1)
                        .unwrap()
                        .unwrap()
                        .representation_restarts,
                    if mismatch != "none" { 1 } else { 0 }
                );
                println!(
                    "S2 A13 PASS rep={rep} mismatch={mismatch} pref={pref:?} {:?}",
                    e.transport_open_evidence(1).unwrap()
                );
                e.shutdown();
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r1_s2_mixed_representation_negatives_both_paths() {
    for rep in 0..3 {
        for victim in [1, 2] {
            for fault in ["etag", "missing", "total", "200", "coding", "redirect"] {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                *f.gates[victim].fault.lock().unwrap() = Some(fault);
                let dir = tempfile::tempdir().unwrap();
                let e = engine(&f, dir.path(), TransportPref::Auto, true);
                let id = add(&e, f.url());
                e.resume(id).unwrap();
                let restartable = ["etag", "missing", "total", "200", "redirect"].contains(&fault);
                // After a representation restart the existing policy replans one range.
                // A fault confined to IPv6 can therefore finish safely via validated v4.
                terminal(
                    &e,
                    id,
                    if (victim == 1 && restartable) || fault == "200" {
                        TaskState::Completed
                    } else {
                        TaskState::Failed
                    },
                )
                .await;
                let r = e.store().get_task(id).unwrap().unwrap();
                assert_eq!(
                    r.representation_restarts,
                    if restartable { 1 } else { 0 },
                    "{r:?}"
                );
                let evidence = e.transport_open_evidence(id).unwrap();
                let segment = if victim == 1 { 1 } else { 0 };
                let first_join = evidence
                    .allocations
                    .iter()
                    .find(|a| a.segment == segment && a.action == "joined")
                    .unwrap();
                assert_eq!(
                    first_join.done, 0,
                    "invalid headers must admit zero coverage: {fault}"
                );
                assert!(e
                    .task(id)
                    .unwrap()
                    .paths
                    .iter()
                    .all(|p| p.active_ranges == 0));
                println!(
                    "S2 R1 negative PASS rep={rep} victim={victim} fault={fault} {evidence:?}"
                );
                e.shutdown();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a7_s2_committed_checkpoint_fingerprint_uses_logical_url() {
    use usagi::engine::proof::{ProofBarrier, ProofPoint};
    for rep in 0..3 {
        let Some(f) = Fixture::start().await else {
            return;
        };
        for i in [1, 2] {
            f.gates[i]
                .at_bytes
                .store(16384, std::sync::atomic::Ordering::Relaxed);
            f.gates[i].hold();
        }
        let dir = tempfile::tempdir().unwrap();
        let e = engine(&f, dir.path(), TransportPref::Auto, true);
        let id = add(&e, f.url());
        let gate = ProofBarrier::new(ProofPoint::TransactionCommittedBeforeAcknowledgment);
        e.install_proof_barrier(id, gate.clone()).unwrap();
        e.resume(id).unwrap();
        until(|| payload(&f, 1) && payload(&f, 2)).await;
        for i in [1, 2] {
            f.gates[i].release(1);
        }
        until(|| gate.entered()).await;
        let record = e.store().get_task(id).unwrap().unwrap();
        assert!(record.progress_checkpoint.is_some());
        assert!(e
            .store()
            .get_segments(id)
            .unwrap()
            .iter()
            .any(|s| s.done > 0));
        logical(&e, id, &f.url());
        println!(
            "S2 A7 checkpoint PASS rep={rep} record={record:?} segments={:?}",
            e.store().get_segments(id).unwrap()
        );
        gate.release();
        for i in [1, 2] {
            f.gates[i].release(3);
        }
        terminal(&e, id, TaskState::Completed).await;
        e.shutdown();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r1_s2_h3_only_failure_and_disabled_h3_do_not_fallback() {
    for rep in 0..3 {
        for mode in ["dead-endpoint", "disabled"] {
            let Some(f) = Fixture::start().await else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            let e = engine(&f, dir.path(), TransportPref::Http3Only, true);
            if mode == "dead-endpoint" {
                let mut entry = AltSvcEntry::new("localhost", 9, 3600);
                entry.origin = origin_key("localhost", f.v4.port());
                e.net().alt_svc_store().put(entry);
            } else {
                let mut settings = e.settings();
                settings.http3_enabled = false;
                e.update_settings(settings).unwrap();
                until(|| !e.net().settings().http3_enabled).await;
            }
            let id = add(&e, f.url());
            e.resume(id).unwrap();
            terminal(&e, id, TaskState::Failed).await;
            assert!(!payload(&f, 0) && !payload(&f, 1) && !payload(&f, 2));
            assert!(e.task(id).unwrap().paths.is_empty());
            println!("S2 R1 H3-only negative PASS rep={rep} mode={mode}");
            e.shutdown();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r1_s2_success_then_same_family_fallback_retains_one_path_and_history() {
    use usagi::engine::proof::{ProofBarrier, ProofPoint};
    for rep in 0..3 {
        let Some(f) = Fixture::start().await else {
            return;
        };
        f.gates[1].hold();
        let dir = tempfile::tempdir().unwrap();
        let e = engine(&f, dir.path(), TransportPref::Auto, true);
        let id = add(&e, f.url());
        let gate = ProofBarrier::for_segment(ProofPoint::AfterBodyBeforeFlush, 0);
        e.install_proof_barrier(id, gate.clone()).unwrap();
        e.resume(id).unwrap();
        until(|| gate.entered() && payload(&f, 1)).await;
        assert!(e
            .transport_open_evidence(id)
            .unwrap()
            .observations
            .iter()
            .any(|o| o.segment == 0 && o.protocol == Some(TransportKind::Http3)));
        f.close_h3();
        gate.release();
        until(|| payload(&f, 0)).await;
        f.gates[1].release(3);
        terminal(&e, id, TaskState::Completed).await;
        let task = e.task(id).unwrap();
        assert_eq!(task.paths.len(), 2);
        let v4 = task
            .paths
            .iter()
            .find(|p| p.family == usagi::net::http12::Family::V4)
            .unwrap();
        assert_eq!(
            v4.observed_protocols,
            vec![TransportKind::Http3, TransportKind::Http2]
        );
        assert_eq!(v4.endpoint, Some(f.v4));
        assert_eq!(v4.active_ranges, 0);
        assert_eq!(
            e.store()
                .get_task(id)
                .unwrap()
                .unwrap()
                .representation_restarts,
            0
        );
        assert!(matches!(task.transport_summary, TransportSummary::Mixed(_)));
        assert_eq!(task.transport, None);
        println!(
            "S2 R1 success/fallback PASS rep={rep} {task:?} {:?}",
            e.transport_open_evidence(id).unwrap()
        );
        e.shutdown();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s2_exact_original_legacy_url_preserves_proven_checkpoint_prefix() {
    for rep in 0..3 {
        for pref in [TransportPref::Auto, TransportPref::NoHttp3] {
            let Some(f) = Fixture::start().await else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            seed_legacy(&f, dir.path(), false);
            let e = engine(&f, dir.path(), pref, true);
            assert_eq!(e.task(1).unwrap().received, 4096);
            e.resume(1).unwrap();
            terminal(&e, 1, TaskState::Completed).await;
            logical(&e, 1, &f.url());
            let evidence = e.transport_open_evidence(1).unwrap();
            assert_eq!(evidence.observations.len(), 1);
            assert_eq!(
                evidence.observations[0].range,
                Some((4096, Some(SIZE as u64 - 1)))
            );
            assert_eq!(
                e.store()
                    .get_task(1)
                    .unwrap()
                    .unwrap()
                    .representation_restarts,
                0
            );
            println!("S2 legacy exact identity PASS rep={rep} pref={pref:?} {evidence:?}");
            e.shutdown();
        }
    }
}

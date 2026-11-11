//! S1 Http12 subset of Proof A. S2 and historical cases remain separate.
#[path = "common/mod.rs"]
mod common;
#[path = "common/transport_fixture.rs"]
mod fixture;
use fixture::{Fixture, SIZE};
use std::time::Duration;
use usagi::engine::proof::{ProofBarrier, ProofPoint};
use usagi::store::TaskState;
use usagi::{AddRequest, Engine, EngineBuilder, Paths, Settings};

fn engine(f: &Fixture, root: &std::path::Path) -> Engine {
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
            http3_enabled: false,
            dual_stack_paths: true,
            use_system_proxy: false,
            segments_per_task: 4,
            min_segment_bytes: 65536,
            write_buffer_bytes: 1024,
            max_retries: 0,
            persist_interval_ms: 60000,
            default_download_dir: root.join("downloads").to_string_lossy().into(),
            ..Settings::default()
        })
        .build()
        .unwrap();
    e.net().install_test_root_der(f.ca.ca_der()).unwrap();
    e
}
fn add(e: &Engine, f: &Fixture) -> u64 {
    e.add(
        AddRequest::new(f.url())
            .filename("proof.bin")
            .start_paused(true),
    )
    .unwrap()
}
async fn until(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("required S1 rendezvous");
}
fn payload_events(f: &Fixture, family: usize, stage: u8) -> bool {
    f.gates[family]
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| event.stage == stage && event.range.as_deref() != Some("bytes=0-0"))
}
async fn completed(e: &Engine, id: u64) {
    until(|| e.task(id).unwrap().state.is_terminal()).await;
    let task = e.task(id).unwrap();
    assert_eq!(task.state, TaskState::Completed, "{:?}", task.error);
    let output = std::fs::read(task.final_path).unwrap();
    assert_eq!(output, fixture::payload());
    assert_eq!(
        e.store()
            .get_task(id)
            .unwrap()
            .unwrap()
            .representation_restarts,
        0
    );
    assert_eq!(
        e.net().engine_connections(),
        usagi::net::EngineConnections::Unavailable
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a1_s1_two_real_families_disjoint_ranges() {
    for repetition in 0..3 {
        let Some(f) = Fixture::start().await else {
            return;
        };
        println!("S1 A1 repetition={repetition} {}", f.manifest());
        for gate in &f.gates[..2] {
            gate.hold();
        }
        let dir = tempfile::tempdir().unwrap();
        let e = engine(&f, dir.path());
        let id = add(&e, &f);
        e.resume(id).unwrap();
        until(|| (0..2).all(|i| payload_events(&f, i, 1))).await;
        until(|| e.task(id).unwrap().active_requests == 2).await;
        for stage in 1..=3 {
            until(|| (0..2).all(|i| payload_events(&f, i, stage))).await;
            for gate in &f.gates[..2] {
                gate.release(stage);
            }
        }
        completed(&e, id).await;
        let evidence = e.transport_open_evidence(id).unwrap();
        assert_eq!(evidence.pending, 0);
        assert_eq!(evidence.dropped, 0);
        let mut ranges: Vec<_> = evidence
            .observations
            .iter()
            .filter(|o| o.requested_family.is_some())
            .map(|o| {
                assert_eq!(o.generation, 1);
                assert_eq!(
                    o.observed_family,
                    Some(match o.requested_family.unwrap() {
                        usagi::net::http12::Family::V4 => "v4",
                        _ => "v6",
                    })
                );
                o.range.unwrap()
            })
            .collect();
        ranges.sort();
        assert_eq!(ranges.len(), 4);
        let mut end = 0;
        for (start, last) in ranges {
            assert_eq!(start, end);
            end = last.unwrap() + 1;
        }
        assert_eq!(end, SIZE as u64);
        assert_eq!(
            evidence
                .allocations
                .iter()
                .filter(|a| a.action == "grant")
                .count(),
            4
        );
        println!("S1 A1 PASS {evidence:?}");
        e.shutdown();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a2_a9_s1_join_reclaim_both_directions_three_stages() {
    for repetition in 0..3 {
        for victim in 0..2 {
            for stage in 1..=3 {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                println!(
                    "S1 A2/A9 rep={repetition} victim={victim} stage={stage} {}",
                    f.manifest()
                );
                for gate in &f.gates[..2] {
                    gate.hold();
                }
                let dir = tempfile::tempdir().unwrap();
                let e = engine(&f, dir.path());
                let id = add(&e, &f);
                let gate = ProofBarrier::new(ProofPoint::BeforeReclaim);
                e.install_proof_barrier(id, gate.clone()).unwrap();
                e.resume(id).unwrap();
                until(|| (0..2).all(|i| payload_events(&f, i, 1))).await;
                for next in 1..stage {
                    f.gates[victim].release(next);
                    until(|| payload_events(&f, victim, next + 1)).await;
                }
                f.kill_path(victim);
                until(|| gate.entered()).await;
                // Survivor remains in its original range/run while victim is joined.
                let before = e.transport_open_evidence(id).unwrap();
                assert!(before
                    .allocations
                    .iter()
                    .any(|a| a.segment == victim && a.action == "joined"));
                assert!(!before
                    .allocations
                    .iter()
                    .any(|a| a.action == "reclaim-committed-prefix"));
                let stored = e.store().get_task(id).unwrap().unwrap();
                let committed = e.store().get_segments(id).unwrap();
                let expected = if stored.progress_checkpoint.is_some() {
                    committed[victim].done
                } else {
                    0
                };
                // The sibling can finish its range before handoff. It is never restarted.
                f.gates[1 - victim].release(3);
                gate.release();
                completed(&e, id).await;
                let evidence = e.transport_open_evidence(id).unwrap();
                let reclaimed: Vec<_> = evidence
                    .allocations
                    .iter()
                    .filter(|a| a.action == "reclaim-committed-prefix")
                    .collect();
                assert_eq!(reclaimed.len(), 1);
                assert_eq!(reclaimed[0].segment, victim);
                assert_eq!(reclaimed[0].done, expected);
                assert_eq!(reclaimed[0].epoch, 1);
                assert!(evidence.allocations.iter().all(|a| a.generation == 1));
                assert_eq!(
                    evidence
                        .allocations
                        .iter()
                        .filter(|a| a.segment == 1 - victim && a.action == "grant")
                        .count(),
                    1
                );
                let requests: Vec<_> = evidence
                    .observations
                    .iter()
                    .filter(|o| o.segment == victim)
                    .collect();
                assert_eq!(requests.len(), 2);
                assert_eq!(
                    requests[1].range.unwrap().0,
                    committed[victim].start + expected
                );
                assert_ne!(requests[0].requested_family, requests[1].requested_family);
                println!("S1 A2/A9 PASS expected_prefix={expected} {evidence:?}");
                e.shutdown();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a3_a4_a5_s1_reject_bad_family_responses() {
    for repetition in 0..3 {
        for victim in 0..2 {
            for fault in [
                "etag", "weak", "missing", "total", "interval", "short", "excess", "200", "416",
                "coding", "vary",
            ] {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                *f.gates[victim].fault.lock().unwrap() = Some(fault);
                let dir = tempfile::tempdir().unwrap();
                let e = engine(&f, dir.path());
                let id = add(&e, &f);
                println!(
                    "S1 A3/A4/A5 rep={repetition} victim={victim} fault={fault} {}",
                    f.manifest()
                );
                e.resume(id).unwrap();
                until(|| e.task(id).unwrap().state.is_terminal()).await;
                let task = e.task(id).unwrap();
                let evidence = e.transport_open_evidence(id).unwrap();
                let first_join = evidence
                    .allocations
                    .iter()
                    .find(|a| a.segment == victim && a.action == "joined")
                    .expect("faulted worker joined before restart/reclaim");
                if !["short", "excess"].contains(&fault) {
                    assert_eq!(
                        first_join.done, 0,
                        "rejected headers must publish no coverage: {fault}"
                    );
                } else {
                    let first_request = evidence
                        .observations
                        .iter()
                        .find(|o| o.segment == victim)
                        .unwrap();
                    let (start, end) = first_request.range.unwrap();
                    assert!(
                        first_join.done < end.unwrap() - start + 1,
                        "malformed body cannot finish the original range"
                    );
                }
                if task.state == TaskState::Completed {
                    assert_eq!(std::fs::read(&task.final_path).unwrap(), fixture::payload());
                } else {
                    assert_eq!(task.state, TaskState::Failed, "fault={fault} {task:?}");
                }
                if task.state != TaskState::Completed {
                    assert!(!std::path::Path::new(&task.final_path).exists());
                }
                assert!(
                    e.store()
                        .get_task(id)
                        .unwrap()
                        .unwrap()
                        .representation_restarts
                        <= 1
                );
                println!(
                    "S1 A3/A4/A5 PASS {fault} {:?}",
                    e.transport_open_evidence(id).unwrap()
                );
                e.shutdown();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a10_s1_cancel_and_pause_drain_both_writers() {
    for repetition in 0..3 {
        for cancel in [false, true] {
            let Some(f) = Fixture::start().await else {
                return;
            };
            for gate in &f.gates[..2] {
                gate.hold();
            }
            let dir = tempfile::tempdir().unwrap();
            let e = engine(&f, dir.path());
            let id = add(&e, &f);
            e.resume(id).unwrap();
            until(|| (0..2).all(|i| payload_events(&f, i, 1))).await;
            if cancel {
                e.cancel(id).unwrap();
            } else {
                e.pause(id).unwrap();
            }
            until(|| {
                e.task(id).unwrap().active_requests == 0
                    && e.task(id).unwrap().state != TaskState::Downloading
            })
            .await;
            let task = e.task(id).unwrap();
            if task.state != TaskState::Completed {
                assert!(!std::path::Path::new(&task.final_path).exists());
            }
            for gate in &f.gates[..2] {
                gate.release(3);
            }
            if !cancel {
                e.resume(id).unwrap();
                completed(&e, id).await;
            }
            println!(
                "S1 A10 cancel={cancel} rep={repetition} PASS {:?}",
                e.transport_open_evidence(id).unwrap()
            );
            e.shutdown();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a10_s1_sync_and_store_failures_stop_without_route_repair() {
    for repetition in 0..3 {
        for fault in [
            usagi::engine::WorkerFault::CheckpointSync { min_done: 1 },
            usagi::engine::WorkerFault::CheckpointStore { min_done: 1 },
        ] {
            let Some(f) = Fixture::start().await else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            let e = engine(&f, dir.path());
            let id = add(&e, &f);
            e.arm_worker_fault(fault.clone());
            e.resume(id).unwrap();
            until(|| e.task(id).unwrap().state.is_terminal()).await;
            let task = e.task(id).unwrap();
            assert_eq!(task.state, TaskState::Failed);
            assert!(!std::path::Path::new(&task.final_path).exists());
            assert_eq!(e.worker_faults_fired(), 1);
            let evidence = e.transport_open_evidence(id).unwrap();
            assert!(!evidence
                .allocations
                .iter()
                .any(|a| a.action == "reclaim-committed-prefix"));
            println!("S1 A10 storage rep={repetition} fault={fault:?} PASS {evidence:?}");
            e.shutdown();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a11_s1_single_family_and_proxy_enabled_ineligible() {
    for repetition in 0..3 {
        for proxy in [false, true] {
            let Some(f) = Fixture::start().await else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            let e = engine(&f, dir.path());
            if proxy {
                let mut settings = e.settings();
                settings.use_system_proxy = true;
                let expected_interval = settings.persist_interval_ms;
                let expected_proxy = settings.use_system_proxy;
                e.update_settings(settings).unwrap();
                until(|| {
                    e.settings().persist_interval_ms == expected_interval
                        && e.settings().use_system_proxy == expected_proxy
                })
                .await;
                e.net().install_test_root_der(f.ca.ca_der()).unwrap();
            }
            let url = if proxy {
                f.url()
            } else {
                f.url().replace("localhost", "127.0.0.1")
            };
            let id = e
                .add(
                    AddRequest::new(url)
                        .filename("proof.bin")
                        .start_paused(true),
                )
                .unwrap();
            e.resume(id).unwrap();
            completed(&e, id).await;
            let evidence = e.transport_open_evidence(id).unwrap();
            assert!(evidence
                .observations
                .iter()
                .all(|o| o.requested_family.is_none()));
            assert!(evidence.allocations.iter().all(|a| a.action != "grant"));
            println!("S1 A11 rep={repetition} proxy_enabled={proxy} PASS {evidence:?}");
            e.shutdown();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a9_s1_checkpoint_boundaries_keep_committed_prefix() {
    for repetition in 0..3 {
        for victim in 0..2 {
            for point in [
                ProofPoint::AfterWriteBeforeSync,
                ProofPoint::AfterSyncBeforeTransaction,
                ProofPoint::TransactionCommittedBeforeAcknowledgment,
            ] {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                println!(
                    "S1 A9 checkpoint rep={repetition} victim={victim} point={point:?} {}",
                    f.manifest()
                );
                for g in &f.gates[..2] {
                    g.at_bytes
                        .store(16384, std::sync::atomic::Ordering::Relaxed);
                    g.hold();
                }
                let dir = tempfile::tempdir().unwrap();
                let e = engine(&f, dir.path());
                let mut settings = e.settings();
                settings.persist_interval_ms = 200;
                let expected_interval = settings.persist_interval_ms;
                let expected_proxy = settings.use_system_proxy;
                e.update_settings(settings).unwrap();
                until(|| {
                    e.settings().persist_interval_ms == expected_interval
                        && e.settings().use_system_proxy == expected_proxy
                })
                .await;
                e.net().install_test_root_der(f.ca.ca_der()).unwrap();
                let id = add(&e, &f);
                let barrier = ProofBarrier::new(point);
                e.install_proof_barrier(id, barrier.clone()).unwrap();
                e.resume(id).unwrap();
                until(|| (0..2).all(|i| payload_events(&f, i, 1))).await;
                for g in &f.gates[..2] {
                    g.release(1);
                }
                until(|| (0..2).all(|i| payload_events(&f, i, 2))).await;
                until(|| barrier.entered()).await;
                let record = e.store().get_task(id).unwrap().unwrap();
                let old = e.store().get_segments(id).unwrap();
                if point == ProofPoint::TransactionCommittedBeforeAcknowledgment {
                    assert!(record.progress_checkpoint.is_some());
                    assert!(old.iter().any(|s| s.done > 0));
                } else {
                    assert_eq!(old.iter().map(|s| s.done).sum::<u64>(), 0);
                }
                // Stop the persistence thread from taking another later snapshot after
                // this blocked one; its successful commit becomes the reclaim receipt.
                f.kill_path(victim);
                barrier.release();
                f.gates[1 - victim].release(3);
                completed(&e, id).await;
                let evidence = e.transport_open_evidence(id).unwrap();
                let reclaim = evidence
                    .allocations
                    .iter()
                    .find(|a| a.action == "reclaim-committed-prefix")
                    .unwrap();
                assert_eq!(reclaim.segment, victim);
                assert!(reclaim.done <= 16384);
                let committed = evidence
                    .allocations
                    .iter()
                    .take_while(|a| a.action != "reclaim-committed-prefix")
                    .filter(|a| a.action == "checkpoint-committed" && a.segment == victim)
                    .last()
                    .map_or(0, |a| a.done);
                assert_eq!(reclaim.done, committed);
                assert!(evidence.allocations.iter().all(|a| a.generation == 1));
                let attempts: Vec<_> = evidence
                    .observations
                    .iter()
                    .filter(|o| o.segment == victim)
                    .collect();
                assert_eq!(attempts.len(), 2);
                assert_eq!(
                    attempts[1].range.unwrap().0,
                    old[victim].start + reclaim.done
                );
                println!("S1 A9 checkpoint PASS {evidence:?}");
                e.shutdown();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a9_s1_complete_unflushed_drain_without_redundant_reclaim() {
    for repetition in 0..3 {
        for victim in 0..2 {
            let Some(f) = Fixture::start().await else {
                return;
            };
            let dir = tempfile::tempdir().unwrap();
            let e = engine(&f, dir.path());
            let id = add(&e, &f);
            let gate = ProofBarrier::for_segment(ProofPoint::AfterBodyBeforeFlush, victim);
            e.install_proof_barrier(id, gate.clone()).unwrap();
            e.resume(id).unwrap();
            until(|| gate.entered()).await;
            f.kill_path(victim);
            gate.release();
            completed(&e, id).await;
            let evidence = e.transport_open_evidence(id).unwrap();
            // EOF was already validated. Route loss cannot invalidate these bytes;
            // queued work may fail an open and transfer to the surviving family.
            assert_eq!(
                evidence
                    .allocations
                    .iter()
                    .filter(|a| a.segment == victim && a.action == "grant")
                    .count(),
                1
            );
            assert!(!evidence
                .allocations
                .iter()
                .any(|a| a.segment == victim && a.action == "reclaim-committed-prefix"));
            println!("S1 A9 complete-unflushed rep={repetition} victim={victim} PASS {evidence:?}");
            e.shutdown();
        }
    }
}

// Isolated actor: abrupt process exit prevents Engine::drop/shutdown from
// flushing the volatile tail. This is process-kill coverage, not power-loss QA.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "test-binary actor launched only by a10_s1_process_kill_recovery"]
async fn s1_crash_actor() {
    let root = std::path::PathBuf::from(std::env::var_os("UDM_S1_ACTOR_ROOT").expect("actor root"));
    let url = std::env::var("UDM_S1_ACTOR_URL").unwrap();
    let cut = std::env::var("UDM_S1_ACTOR_CUT").unwrap();
    let e = EngineBuilder::new()
        .paths(Paths {
            root: root.clone(),
            db_file: root.join("udm.redb"),
            log_file: root.join("logs/udm.log"),
            temp_dir: root.join("tmp"),
            default_download_dir: root.join("downloads"),
            portable: true,
            exe_dir: Some(root.clone()),
        })
        .settings(Settings {
            http3_enabled: false,
            dual_stack_paths: true,
            use_system_proxy: cut == "proxy",
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
    e.net()
        .install_test_root_der(&std::fs::read(root.join("fixture.der")).unwrap())
        .unwrap();
    if cut == "proxy" {
        let id = e
            .add(
                AddRequest::new(url)
                    .filename("proof.bin")
                    .start_paused(true),
            )
            .unwrap();
        e.resume(id).unwrap();
        until(|| e.task(id).unwrap().state.is_terminal()).await;
        assert_eq!(e.task(id).unwrap().state, TaskState::Failed);
        assert!(e
            .transport_open_evidence(id)
            .unwrap()
            .observations
            .iter()
            .all(|o| o.requested_family.is_none()));
        std::process::exit(87);
    }
    let point = match cut.as_str() {
        "before-sync" => ProofPoint::AfterWriteBeforeSync,
        "synced-uncommitted" => ProofPoint::AfterSyncBeforeTransaction,
        "committed-unacknowledged" => ProofPoint::TransactionCommittedBeforeAcknowledgment,
        "reclaim" => ProofPoint::BeforeReclaim,
        _ => panic!("unknown cut"),
    };
    let id = e
        .add(
            AddRequest::new(url)
                .filename("proof.bin")
                .start_paused(true),
        )
        .unwrap();
    let gate = ProofBarrier::new(point);
    e.install_proof_barrier(id, gate.clone()).unwrap();
    e.resume(id).unwrap();
    until(|| gate.entered()).await;
    let segments = e.store().get_segments(id).unwrap();
    let record = e.store().get_task(id).unwrap().unwrap();
    std::fs::write(
        root.join("cut.json"),
        serde_json::to_vec(&serde_json::json!({"record":record,"segments":segments})).unwrap(),
    )
    .unwrap();
    std::process::exit(86);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a10_s1_process_kill_recovery() {
    for repetition in 0..3 {
        for victim in 0..2 {
            for cut in [
                "before-sync",
                "synced-uncommitted",
                "committed-unacknowledged",
                "reclaim",
            ] {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                for gate in &f.gates[..2] {
                    gate.at_bytes
                        .store(16384, std::sync::atomic::Ordering::Relaxed);
                    gate.hold();
                }
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("fixture.der"), f.ca.ca_der()).unwrap();
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "s1_crash_actor", "--ignored", "--nocapture"])
                    .env("UDM_S1_ACTOR_ROOT", dir.path())
                    .env("UDM_S1_ACTOR_URL", f.url())
                    .env("UDM_S1_ACTOR_CUT", cut);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    command.creation_flags(0x08000000);
                }
                let actor = tokio::task::spawn_blocking(move || command.output().unwrap());
                until(|| (0..2).all(|i| payload_events(&f, i, 1))).await;
                for gate in &f.gates[..2] {
                    gate.release(1);
                }
                until(|| (0..2).all(|i| payload_events(&f, i, 2))).await;
                if cut == "reclaim" {
                    f.kill_path(victim);
                }
                let output = tokio::time::timeout(Duration::from_secs(30), actor)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(86),
                    "{} {}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                println!(
                    "S1 crash manifest rep={repetition} victim={victim} cut={cut} {}",
                    f.manifest()
                );
                let cut_record: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(dir.path().join("cut.json")).unwrap())
                        .unwrap();
                let committed: u64 = cut_record["segments"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s["done"].as_u64().unwrap())
                    .sum();
                if ["before-sync", "synced-uncommitted"].contains(&cut) {
                    assert_eq!(committed, 0);
                }
                if cut == "committed-unacknowledged" {
                    assert!(committed > 0);
                }
                for gate in &f.gates[..2] {
                    gate.release(3);
                }
                let e = engine(&f, dir.path());
                let stored = e.store().get_task(1).unwrap().unwrap();
                let recovered = e.store().get_segments(1).unwrap();
                println!("S1 crash stored={cut_record} recovered={recovered:?}");
                assert_eq!(recovered.iter().map(|s| s.done).sum::<u64>(), committed);
                assert!(!stored.final_path().exists());
                assert_eq!(stored.representation_restarts, 0);
                e.resume(1).unwrap();
                completed(&e, 1).await;
                println!("S1 A10 process-kill rep={repetition} victim={victim} cut={cut} committed={committed} PASS {:?}", e.transport_open_evidence(1).unwrap());
                e.shutdown();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s1_two_paths_consume_one_queue_slot() {
    let Some(f) = Fixture::start().await else {
        return;
    };
    for gate in &f.gates[..2] {
        gate.hold();
    }
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&f, dir.path());
    let mut settings = e.settings();
    settings.max_concurrent_tasks = 1;
    e.update_settings(settings).unwrap();
    until(|| e.settings().max_concurrent_tasks == 1).await;
    e.net().install_test_root_der(f.ca.ca_der()).unwrap();
    let first = add(&e, &f);
    let second = e
        .add(
            AddRequest::new(f.url())
                .filename("second.bin")
                .start_paused(true),
        )
        .unwrap();
    e.resume(first).unwrap();
    until(|| e.task(first).unwrap().active_requests == 2).await;
    e.resume(second).unwrap();
    until(|| e.task(second).unwrap().state == TaskState::Queued).await;
    assert_eq!(e.task(second).unwrap().active_requests, 0);
    for gate in &f.gates[..2] {
        gate.release(3);
    }
    completed(&e, first).await;
    completed(&e, second).await;
    assert_eq!(e.transport_open_evidence(first).unwrap().probe_bytes, 2);
    e.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a10_s1_periodic_checkpoint_failure_drains_both_paths() {
    for repetition in 0..3 {
        for fault in [
            usagi::engine::WorkerFault::CheckpointSync { min_done: 1 },
            usagi::engine::WorkerFault::CheckpointStore { min_done: 1 },
        ] {
            let Some(f) = Fixture::start().await else {
                return;
            };
            for gate in &f.gates[..2] {
                gate.at_bytes
                    .store(16384, std::sync::atomic::Ordering::Relaxed);
                gate.hold();
            }
            let dir = tempfile::tempdir().unwrap();
            let e = engine(&f, dir.path());
            let mut settings = e.settings();
            settings.persist_interval_ms = 200;
            e.update_settings(settings).unwrap();
            until(|| e.settings().persist_interval_ms == 200).await;
            e.net().install_test_root_der(f.ca.ca_der()).unwrap();
            let id = add(&e, &f);
            e.arm_worker_fault(fault.clone());
            e.resume(id).unwrap();
            until(|| (0..2).all(|i| payload_events(&f, i, 1))).await;
            for gate in &f.gates[..2] {
                gate.release(1);
            }
            until(|| e.task(id).unwrap().state.is_terminal()).await;
            let task = e.task(id).unwrap();
            assert_eq!(task.state, TaskState::Failed, "{task:?}");
            assert_eq!(task.active_requests, 0);
            assert!(!std::path::Path::new(&task.final_path).exists());
            let evidence = e.transport_open_evidence(id).unwrap();
            assert!(!evidence
                .allocations
                .iter()
                .any(|a| a.action == "reclaim-committed-prefix"));
            println!("S1 A10 periodic storage rep={repetition} fault={fault:?} PASS {evidence:?}");
            for gate in &f.gates[..2] {
                gate.release(3);
            }
            e.shutdown();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a5_a11_s1_unbound_tasks_never_admit_family_paths() {
    for repetition in 0..3 {
        for fault in ["no-validator", "weak-bound", "vary-bound"] {
            let Some(f) = Fixture::start().await else {
                return;
            };
            for gate in &f.gates[..2] {
                *gate.fault.lock().unwrap() = Some(fault);
            }
            let dir = tempfile::tempdir().unwrap();
            let e = engine(&f, dir.path());
            let id = add(&e, &f);
            e.resume(id).unwrap();
            completed(&e, id).await;
            let evidence = e.transport_open_evidence(id).unwrap();
            assert!(evidence
                .observations
                .iter()
                .all(|o| o.requested_family.is_none()));
            println!("S1 A5/A11 unbound rep={repetition} fault={fault} PASS {evidence:?}");
            e.shutdown();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a9_s1_goaway_and_reset_reclaim_on_other_family() {
    for repetition in 0..3 {
        for victim in 0..2 {
            let Some(f) = Fixture::start().await else {
                return;
            };
            *f.gates[victim].fault.lock().unwrap() = Some("goaway");
            let dir = tempfile::tempdir().unwrap();
            let e = engine(&f, dir.path());
            let id = add(&e, &f);
            e.resume(id).unwrap();
            completed(&e, id).await;
            let evidence = e.transport_open_evidence(id).unwrap();
            assert_eq!(
                evidence
                    .allocations
                    .iter()
                    .filter(|a| a.action == "reclaim-committed-prefix")
                    .count(),
                1
            );
            assert!(evidence.allocations.iter().all(|a| a.generation == 1));
            println!("S1 A9 GOAWAY and reset rep={repetition} victim={victim} PASS {evidence:?}");
            e.shutdown();
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a11_s1_configured_proxy_is_used_without_origin_bypass() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for repetition in 0..3 {
        let Some(f) = Fixture::start().await else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("fixture.der"), f.ca.ca_der()).unwrap();
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = proxy.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            // HEAD and then range probe both have to use this proxy.
            for _ in 0..2 {
                let (mut socket, _) = proxy.accept().await.unwrap();
                let mut bytes = vec![0; 4096];
                let read = socket.read(&mut bytes).await.unwrap();
                let request = String::from_utf8_lossy(&bytes[..read]).to_string();
                assert!(request.starts_with("CONNECT localhost:"), "{request}");
                requests.push(request);
                socket.write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            }
            requests
        });
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "s1_crash_actor", "--ignored", "--nocapture"])
            .env("UDM_S1_ACTOR_ROOT", dir.path())
            .env("UDM_S1_ACTOR_URL", f.url())
            .env("UDM_S1_ACTOR_CUT", "proxy")
            .env("HTTPS_PROXY", format!("http://{address}"))
            .env("https_proxy", format!("http://{address}"))
            .env("NO_PROXY", "")
            .env("no_proxy", "");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000);
        }
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::task::spawn_blocking(move || command.output().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            output.status.code(),
            Some(87),
            "{} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let requests = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(requests.len(), 2);
        assert!(f.gates[..2]
            .iter()
            .all(|g| g.events.lock().unwrap().is_empty()));
        println!("S1 A11 actual proxy rep={repetition} PASS {requests:?}");
    }
}

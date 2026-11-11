//! S3: bounded local H2 session experiment, one fixed family/path per run.
#[path = "common/mod.rs"]
mod common;
#[path = "common/transport_fixture.rs"]
mod fixture;
use fixture::{Fixture, SIZE};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use usagi::engine::proof::{ProofBarrier, ProofPoint};
use usagi::net::http12::Family;
use usagi::store::{TaskState, TransportKind};
use usagi::{AddRequest, Engine, EngineBuilder, Paths, Settings};
fn engine(f: &Fixture, root: &Path, family: Family) -> Engine {
    build(root, f.ca.ca_der(), family)
}
fn build(root: &Path, der: &[u8], family: Family) -> Engine {
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
            dual_stack_paths: false,
            use_system_proxy: false,
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
    e.net().install_test_root_der(der).unwrap();
    e.net().install_h2_experiment(family).unwrap();
    e
}
fn index(family: Family) -> usize {
    if family == Family::V4 {
        0
    } else {
        1
    }
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
    .expect("S3 required rendezvous");
}
fn events(f: &Fixture, family: Family, stage: u8) -> Vec<fixture::Event> {
    f.gates[index(family)]
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.stage == stage && e.range.as_deref() != Some("bytes=0-0"))
        .cloned()
        .collect()
}
fn victim_connection(f: &Fixture, family: Family, victim: usize) -> u64 {
    // The four equal planner intervals have a remainder only in the final tail.
    let prefix = format!("bytes={}-", victim * (SIZE / 4));
    events(f, family, 1)
        .iter()
        .find(|e| e.range.as_ref().is_some_and(|r| r.starts_with(&prefix)))
        .unwrap()
        .connection
}
async fn kill(f: &Fixture, family: Family, connection: u64) {
    f.gates[index(family)]
        .connections
        .lock()
        .unwrap()
        .remove(&connection)
        .unwrap()
        .send(())
        .unwrap();
    until(|| {
        f.gates[index(family)]
            .closed
            .lock()
            .unwrap()
            .contains(&connection)
    })
    .await;
}
fn structural(f: &Fixture, e: &Engine, id: u64, family: Family) {
    let observed = events(f, family, 1);
    assert_eq!(observed.len(), 2, "exactly two held range requests");
    assert_ne!(observed[0].connection, observed[1].connection);
    assert_ne!(observed[0].range, observed[1].range);
    let accepts = f.gates[index(family)].accepts.lock().unwrap();
    assert_eq!(
        accepts.len(),
        2,
        "metadata reuses the first experimental pool"
    );
    let pair: Vec<_> = observed
        .iter()
        .map(|r| {
            assert_eq!(r.protocol, "h2");
            assert_eq!(r.if_range.as_deref(), Some(fixture::ETAG));
            let a = accepts
                .iter()
                .find(|a| a.connection == r.connection)
                .unwrap();
            assert_eq!(a.alpn, b"h2");
            assert!(family.accepts(Some(a.peer)));
            assert_eq!(a.local, if family == Family::V4 { f.v4 } else { f.v6 });
            a
        })
        .collect();
    assert_ne!(pair[0].peer, pair[1].peer);
    assert!(f.gates[1 - index(family)]
        .accepts
        .lock()
        .unwrap()
        .is_empty());
    assert!(f.gates[2].events.lock().unwrap().is_empty(), "no H3");
    let task = e.task(id).unwrap();
    assert_eq!(task.active_requests, 2);
    assert_eq!(task.paths.len(), 1, "two sessions on ONE path");
    assert_eq!(task.paths[0].family, family);
    assert_eq!(task.paths[0].active_ranges, 2);
    assert_eq!(task.paths[0].observed_protocols, vec![TransportKind::Http2]);
    println!("S3 STRUCTURAL family={family:?} accepts={pair:?} requests={observed:?}");
}
async fn completed(e: &Engine, id: u64, url: &str) {
    until(|| e.task(id).unwrap().state.is_terminal()).await;
    let task = e.task(id).unwrap();
    assert_eq!(task.state, TaskState::Completed, "{task:?}");
    assert_eq!(task.active_requests, 0);
    let bytes = std::fs::read(&task.final_path).unwrap();
    assert_eq!(bytes, fixture::payload());
    assert_eq!(Sha256::digest(&bytes), Sha256::digest(fixture::payload()));
    let record = e.store().get_task(id).unwrap().unwrap();
    assert_eq!(record.final_url, url);
    assert_eq!(record.representation_bound.unwrap().final_url, url);
    assert_eq!(record.representation_restarts, 0);
    assert_eq!(
        e.net().engine_connections(),
        usagi::net::EngineConnections::Unavailable
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_a1_two_sessions_one_path_disjoint_durable_coverage() {
    for rep in 0..3 {
        for family in [Family::V4, Family::V6] {
            let Some(f) = Fixture::start().await else {
                return;
            };
            println!("S3 A1 rep={rep} family={family:?} {}", f.manifest());
            let g = &f.gates[index(family)];
            g.hold();
            let dir = tempfile::tempdir().unwrap();
            let e = engine(&f, dir.path(), family);
            let id = add(&e, &f);
            e.resume(id).unwrap();
            until(|| events(&f, family, 1).len() == 2 && e.task(id).unwrap().active_requests == 2)
                .await;
            structural(&f, &e, id, family);
            for stage in 1..=3 {
                until(|| events(&f, family, stage).len() == 2).await;
                g.release(stage);
            }
            completed(&e, id, &f.url()).await;
            let evidence = e.transport_open_evidence(id).unwrap();
            assert_eq!(evidence.pending, 0);
            assert_eq!(evidence.dropped, 0);
            let mut ranges: Vec<_> = evidence
                .observations
                .iter()
                .map(|o| {
                    assert_eq!(o.requested_family, Some(family));
                    assert_eq!(o.protocol, Some(TransportKind::Http2));
                    o.range.unwrap()
                })
                .collect();
            ranges.sort();
            let mut end = 0;
            assert_eq!(ranges.len(), 4);
            for (start, last) in ranges {
                assert_eq!(start, end);
                end = last.unwrap() + 1;
            }
            assert_eq!(end, SIZE as u64);
            assert!(e
                .store()
                .get_segments(id)
                .unwrap()
                .iter()
                .all(|s| s.finished));
            println!("S3 A1 PASS rep={rep} family={family:?} {evidence:?}");
            e.shutdown();
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_a2_a9_join_then_checkpoint_reclaim_both_sessions() {
    for rep in 0..3 {
        for family in [Family::V4, Family::V6] {
            for victim in 0..2 {
                for stage in 1..=3 {
                    let Some(f) = Fixture::start().await else {
                        return;
                    };
                    let g = &f.gates[index(family)];
                    g.hold();
                    let dir = tempfile::tempdir().unwrap();
                    let e = engine(&f, dir.path(), family);
                    let id = add(&e, &f);
                    let barrier = ProofBarrier::new(ProofPoint::BeforeReclaim);
                    e.install_proof_barrier(id, barrier.clone()).unwrap();
                    e.resume(id).unwrap();
                    until(|| events(&f, family, 1).len() == 2).await;
                    let connection = victim_connection(&f, family, victim);
                    for next in 1..stage {
                        g.release(next);
                        until(|| events(&f, family, next + 1).len() == 2).await;
                    }
                    kill(&f, family, connection).await;
                    until(|| barrier.entered()).await;
                    let evidence = e.transport_open_evidence(id).unwrap();
                    assert!(evidence
                        .allocations
                        .iter()
                        .any(|a| a.segment == victim && a.action == "joined"));
                    assert!(!evidence
                        .allocations
                        .iter()
                        .any(|a| a.action == "reclaim-committed-prefix"));
                    let stored = e.store().get_segments(id).unwrap();
                    g.release(3);
                    barrier.release();
                    completed(&e, id, &f.url()).await;
                    let evidence = e.transport_open_evidence(id).unwrap();
                    let reclaim: Vec<_> = evidence
                        .allocations
                        .iter()
                        .filter(|a| a.action == "reclaim-committed-prefix")
                        .collect();
                    assert_eq!(reclaim.len(), 1);
                    assert_eq!(reclaim[0].segment, victim);
                    let expected = evidence
                        .allocations
                        .iter()
                        .take_while(|a| a.action != "reclaim-committed-prefix")
                        .filter(|a| a.action == "checkpoint-committed" && a.segment == victim)
                        .last()
                        .map_or(0, |a| a.done);
                    assert_eq!(reclaim[0].done, expected);
                    assert_eq!(reclaim[0].epoch, 1);
                    assert!(evidence.allocations.iter().all(|a| a.generation == 1));
                    let attempts: Vec<_> = evidence
                        .observations
                        .iter()
                        .filter(|o| o.segment == victim)
                        .collect();
                    assert_eq!(attempts.len(), 2);
                    assert_eq!(
                        attempts[1].range.unwrap().0,
                        stored[victim].start + expected
                    );
                    println!("S3 A2/A9 PASS rep={rep} family={family:?} victim={victim} stage={stage} {evidence:?}");
                    e.shutdown();
                }
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_a3_a4_a5_a6_reject_invalid_representation() {
    for rep in 0..3 {
        for family in [Family::V4, Family::V6] {
            for victim in 0..2 {
                for fault in [
                    "etag", "weak", "missing", "total", "interval", "short", "excess", "200",
                    "416", "coding", "vary", "redirect",
                ] {
                    let Some(f) = Fixture::start().await else {
                        return;
                    };
                    *f.gates[index(family)].fault.lock().unwrap() = Some(fault);
                    f.gates[index(family)].bad_range_start.store(
                        (victim * (SIZE / 4)) as u64,
                        std::sync::atomic::Ordering::SeqCst,
                    );
                    let dir = tempfile::tempdir().unwrap();
                    let e = engine(&f, dir.path(), family);
                    let id = add(&e, &f);
                    e.resume(id).unwrap();
                    until(|| e.task(id).unwrap().state.is_terminal()).await;
                    let task = e.task(id).unwrap();
                    let evidence = e.transport_open_evidence(id).unwrap();
                    let first = evidence
                        .allocations
                        .iter()
                        .find(|a| a.action == "joined" && a.segment == victim)
                        .unwrap();
                    if !["short", "excess"].contains(&fault) {
                        assert_eq!(first.done, 0, "bad headers: {fault}");
                    } else {
                        let request = evidence
                            .observations
                            .iter()
                            .find(|o| o.segment == first.segment)
                            .unwrap()
                            .range
                            .unwrap();
                        assert!(
                            first.done < request.1.unwrap() - request.0 + 1,
                            "malformed body cannot finish range"
                        );
                    }
                    if task.state == TaskState::Completed {
                        assert_eq!(std::fs::read(&task.final_path).unwrap(), fixture::payload());
                    } else {
                        assert_eq!(task.state, TaskState::Failed);
                        assert!(!Path::new(&task.final_path).exists());
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
                    "S3 A3/A4/A5/A6 PASS rep={rep} family={family:?} victim={victim} fault={fault} {evidence:?}"
                );
                    e.shutdown();
                }
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_a10_pause_cancel_and_storage_failures() {
    for rep in 0..3 {
        for family in [Family::V4, Family::V6] {
            for cancel in [false, true] {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                let g = &f.gates[index(family)];
                g.hold();
                let dir = tempfile::tempdir().unwrap();
                let e = engine(&f, dir.path(), family);
                let id = add(&e, &f);
                e.resume(id).unwrap();
                until(|| events(&f, family, 1).len() == 2).await;
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
                assert!(!Path::new(&e.task(id).unwrap().final_path).exists());
                g.release(3);
                if !cancel {
                    e.resume(id).unwrap();
                    completed(&e, id, &f.url()).await;
                }
                println!("S3 A10 drain PASS rep={rep} family={family:?} cancel={cancel}");
                e.shutdown();
            }
            for fault in [
                usagi::engine::WorkerFault::CheckpointSync { min_done: 1 },
                usagi::engine::WorkerFault::CheckpointStore { min_done: 1 },
            ] {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                let dir = tempfile::tempdir().unwrap();
                let e = engine(&f, dir.path(), family);
                let id = add(&e, &f);
                let mut settings = e.settings();
                settings.max_retries = 2;
                e.update_settings(settings).unwrap();
                until(|| e.settings().max_retries == 2).await;
                e.net().install_test_root_der(f.ca.ca_der()).unwrap();
                e.arm_worker_fault(fault.clone());
                e.resume(id).unwrap();
                until(|| e.task(id).unwrap().state.is_terminal()).await;
                assert_eq!(e.task(id).unwrap().state, TaskState::Failed);
                assert_eq!(e.worker_faults_fired(), 1);
                until(|| e.store().get_task(id).unwrap().unwrap().state == TaskState::Failed).await;
                assert!(e.store().get_task(id).unwrap().unwrap().attempts <= 1);
                assert!(e
                    .transport_open_evidence(id)
                    .unwrap()
                    .allocations
                    .iter()
                    .all(|a| a.generation == 1));
                assert!(!Path::new(&e.task(id).unwrap().final_path).exists());
                assert!(!e
                    .transport_open_evidence(id)
                    .unwrap()
                    .allocations
                    .iter()
                    .any(|a| a.action == "reclaim-committed-prefix"));
                println!("S3 A10 storage PASS rep={rep} family={family:?} fault={fault:?}");
                e.shutdown();
            }
        }
    }
}
#[tokio::test]
async fn s3_a11_proxy_and_policy_fail_closed() {
    let Some(f) = Fixture::start().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&f, dir.path(), Family::V4);
    assert!(e.net().install_h2_experiment(Family::V6).is_err());
    let mut settings = e.settings();
    settings.use_system_proxy = true;
    e.update_settings(settings).unwrap();
    until(|| e.net().settings().use_system_proxy).await;
    let id = add(&e, &f);
    e.resume(id).unwrap();
    until(|| e.task(id).unwrap().state.is_terminal()).await;
    assert_eq!(e.task(id).unwrap().state, TaskState::Failed);
    assert!(e.task(id).unwrap().paths.is_empty());
    assert!(!e
        .transport_open_evidence(id)
        .unwrap()
        .allocations
        .iter()
        .any(|a| a.action == "grant"));
    println!("S3 A11 PASS proxy no experiment admission");
    e.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_a9_checkpoint_boundaries_and_complete_unflushed() {
    for rep in 0..3 {
        for family in [Family::V4, Family::V6] {
            for victim in 0..2 {
                for point in [
                    ProofPoint::AfterWriteBeforeSync,
                    ProofPoint::AfterSyncBeforeTransaction,
                    ProofPoint::TransactionCommittedBeforeAcknowledgment,
                ] {
                    let Some(f) = Fixture::start().await else {
                        return;
                    };
                    let g = &f.gates[index(family)];
                    g.hold();
                    g.at_bytes
                        .store(16384, std::sync::atomic::Ordering::Relaxed);
                    let dir = tempfile::tempdir().unwrap();
                    let e = engine(&f, dir.path(), family);
                    let id = add(&e, &f);
                    let barrier = ProofBarrier::new(point);
                    e.install_proof_barrier(id, barrier.clone()).unwrap();
                    e.resume(id).unwrap();
                    until(|| events(&f, family, 1).len() == 2).await;
                    let connection = victim_connection(&f, family, victim);
                    g.release(1);
                    until(|| events(&f, family, 2).len() == 2 && barrier.entered()).await;
                    let stored = e.store().get_segments(id).unwrap();
                    if point == ProofPoint::TransactionCommittedBeforeAcknowledgment {
                        assert!(e
                            .store()
                            .get_task(id)
                            .unwrap()
                            .unwrap()
                            .progress_checkpoint
                            .is_some());
                        assert!(stored.iter().any(|s| s.done > 0));
                    } else {
                        assert_eq!(stored.iter().map(|s| s.done).sum::<u64>(), 0);
                    }
                    kill(&f, family, connection).await;
                    barrier.release();
                    g.release(3);
                    completed(&e, id, &f.url()).await;
                    let evidence = e.transport_open_evidence(id).unwrap();
                    let reclaim = evidence
                        .allocations
                        .iter()
                        .find(|a| a.action == "reclaim-committed-prefix")
                        .unwrap();
                    assert_eq!(reclaim.segment, victim);
                    assert_eq!(reclaim.epoch, 1);
                    let checkpoint = evidence
                        .allocations
                        .iter()
                        .take_while(|a| a.action != "reclaim-committed-prefix")
                        .filter(|a| a.action == "checkpoint-committed" && a.segment == victim)
                        .last()
                        .map_or(0, |a| a.done);
                    assert_eq!(reclaim.done, checkpoint);
                    assert!(evidence.allocations.iter().all(|a| a.generation == 1));
                    println!("S3 A9 checkpoint PASS rep={rep} family={family:?} victim={victim} point={point:?} {evidence:?}");
                    e.shutdown();
                }
                let Some(f) = Fixture::start().await else {
                    return;
                };
                let dir = tempfile::tempdir().unwrap();
                let e = engine(&f, dir.path(), family);
                let id = add(&e, &f);
                let barrier = ProofBarrier::for_segment(ProofPoint::AfterBodyBeforeFlush, victim);
                e.install_proof_barrier(id, barrier.clone()).unwrap();
                e.resume(id).unwrap();
                until(|| barrier.entered()).await;
                let connection = victim_connection(&f, family, victim);
                kill(&f, family, connection).await;
                barrier.release();
                completed(&e, id, &f.url()).await;
                assert!(!e
                    .transport_open_evidence(id)
                    .unwrap()
                    .allocations
                    .iter()
                    .any(|a| a.segment == victim && a.action == "reclaim-committed-prefix"));
                println!(
                    "S3 A9 complete-unflushed PASS rep={rep} family={family:?} victim={victim}"
                );
                e.shutdown();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "isolated process actor invoked by s3_a10_process_exit_recovery"]
async fn s3_crash_actor() {
    let root = std::path::PathBuf::from(std::env::var_os("UDM_S3_ROOT").unwrap());
    let family = if std::env::var("UDM_S3_FAMILY").unwrap() == "v4" {
        Family::V4
    } else {
        Family::V6
    };
    let url = std::env::var("UDM_S3_URL").unwrap();
    let cut = std::env::var("UDM_S3_CUT").unwrap();
    let e = build(
        &root,
        &std::fs::read(root.join("fixture.der")).unwrap(),
        family,
    );
    let point = match cut.as_str() {
        "before-sync" => ProofPoint::AfterWriteBeforeSync,
        "synced-uncommitted" => ProofPoint::AfterSyncBeforeTransaction,
        "committed-unacknowledged" => ProofPoint::TransactionCommittedBeforeAcknowledgment,
        "reclaim" => ProofPoint::BeforeReclaim,
        _ => panic!("bad cut"),
    };
    let id = e
        .add(
            AddRequest::new(url)
                .filename("proof.bin")
                .start_paused(true),
        )
        .unwrap();
    let barrier = ProofBarrier::new(point);
    e.install_proof_barrier(id, barrier.clone()).unwrap();
    e.resume(id).unwrap();
    until(|| barrier.entered()).await;
    std::fs::write(root.join("cut.json"),serde_json::to_vec(&serde_json::json!({"record":e.store().get_task(id).unwrap().unwrap(),"segments":e.store().get_segments(id).unwrap()})).unwrap()).unwrap();
    std::process::exit(86);
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_a10_process_exit_recovery() {
    for rep in 0..3 {
        for family in [Family::V4, Family::V6] {
            for cut in [
                "before-sync",
                "synced-uncommitted",
                "committed-unacknowledged",
                "reclaim",
            ] {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                let g = &f.gates[index(family)];
                g.hold();
                g.at_bytes
                    .store(16384, std::sync::atomic::Ordering::Relaxed);
                let dir = tempfile::tempdir().unwrap();
                std::fs::write(dir.path().join("fixture.der"), f.ca.ca_der()).unwrap();
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--exact", "s3_crash_actor", "--ignored", "--nocapture"])
                    .env("UDM_S3_ROOT", dir.path())
                    .env("UDM_S3_URL", f.url())
                    .env(
                        "UDM_S3_FAMILY",
                        if family == Family::V4 { "v4" } else { "v6" },
                    )
                    .env("UDM_S3_CUT", cut);
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    command.creation_flags(0x08000000);
                }
                let actor = tokio::task::spawn_blocking(move || command.output().unwrap());
                until(|| events(&f, family, 1).len() == 2).await;
                let connection = victim_connection(&f, family, rep % 2);
                g.release(1);
                until(|| events(&f, family, 2).len() == 2).await;
                if cut == "reclaim" {
                    kill(&f, family, connection).await;
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
                let receipt: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(dir.path().join("cut.json")).unwrap())
                        .unwrap();
                // At the reclaim cut the periodic checkpoint may commit after
                // the actor captures cut.json but before process::exit. Read
                // the authoritative redb receipt AFTER actual process death.
                let crashed = usagi::store::Store::open(dir.path().join("udm.redb")).unwrap();
                let crash_record = crashed.get_task(1).unwrap().unwrap();
                let crash_segments = crashed.get_segments(1).unwrap();
                let committed: u64 = crash_segments.iter().map(|s| s.done).sum();
                if crash_record.progress_checkpoint.is_none() {
                    assert_eq!(committed, 0);
                }
                drop(crashed);
                if ["before-sync", "synced-uncommitted"].contains(&cut) {
                    assert_eq!(committed, 0);
                }
                if cut == "committed-unacknowledged" {
                    assert!(committed > 0);
                }
                g.release(3);
                let e = engine(&f, dir.path(), family);
                let recovered = e.store().get_segments(1).unwrap();
                let ranges = |segments: &[usagi::store::SegmentState]| {
                    segments
                        .iter()
                        .map(|s| (s.index, s.start, s.end, s.done, s.finished))
                        .collect::<Vec<_>>()
                };
                assert_eq!(
                    ranges(&recovered),
                    ranges(&crash_segments),
                    "every recovered range must match the committed redb receipt"
                );
                assert_eq!(
                    e.store()
                        .get_segments(1)
                        .unwrap()
                        .iter()
                        .map(|s| s.done)
                        .sum::<u64>(),
                    committed
                );
                assert!(!e
                    .store()
                    .get_task(1)
                    .unwrap()
                    .unwrap()
                    .final_path()
                    .exists());
                e.resume(1).unwrap();
                completed(&e, 1, &f.url()).await;
                println!("S3 A10 process-exit PASS rep={rep} family={family:?} cut={cut} committed={committed} receipt={receipt}");
                e.shutdown();
            }
        }
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_budget_serializes_experimental_transfers() {
    let Some(f) = Fixture::start().await else {
        return;
    };
    let g = &f.gates[0];
    g.hold();
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&f, dir.path(), Family::V4);
    let first = add(&e, &f);
    let second = e
        .add(
            AddRequest::new(f.url())
                .filename("second.bin")
                .start_paused(true),
        )
        .unwrap();
    e.resume(first).unwrap();
    until(|| events(&f, Family::V4, 1).len() == 2).await;
    e.resume(second).unwrap();
    until(|| e.task(second).unwrap().state == TaskState::Downloading).await;
    // Second task has finished metadata and waits at the experiment lease.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(e.task(first).unwrap().active_requests, 2);
    assert_eq!(e.task(second).unwrap().active_requests, 0);
    assert!(e
        .transport_open_evidence(second)
        .unwrap()
        .allocations
        .is_empty());
    assert_eq!(events(&f, Family::V4, 1).len(), 2);
    g.release(3);
    completed(&e, first, &f.url()).await;
    completed(&e, second, &f.url()).await;
    println!("S3 budget PASS single experiment transfer two ranges one path");
    e.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_a9_goaway_one_session_leaves_sibling_usable() {
    for rep in 0..3 {
        for family in [Family::V4, Family::V6] {
            for victim in 0..2 {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                let g = &f.gates[index(family)];
                g.hold();
                g.goaway_start.store(
                    (victim * (SIZE / 4)) as u64,
                    std::sync::atomic::Ordering::SeqCst,
                );
                let dir = tempfile::tempdir().unwrap();
                let e = engine(&f, dir.path(), family);
                let id = add(&e, &f);
                e.resume(id).unwrap();
                until(|| events(&f, family, 1).len() == 2).await;
                let sibling = victim_connection(&f, family, 1 - victim);
                let victim_id = victim_connection(&f, family, victim);
                g.release(3);
                completed(&e, id, &f.url()).await;
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
                let all = events(&f, family, 1);
                assert!(
                    all.iter().filter(|r| r.connection == sibling).count() >= 4,
                    "survivor carries reclaimed and queued ranges"
                );
                assert_eq!(all.iter().filter(|r| r.connection == victim_id).count(), 1);
                println!(
                    "S3 A9 GOAWAY PASS rep={rep} family={family:?} victim={victim} {evidence:?}"
                );
                e.shutdown();
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_h1_alpn_is_separate_and_cannot_pass_h2_experiment() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ca = common::dev_tls::HarnessCa::generate().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = tokio_rustls::TlsAcceptor::from(ca.server_config_alpn(&[b"http/1.1"]).unwrap());
    let h1 = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(AtomicUsize::new(0));
    let counts = (h1.clone(), refused.clone());
    let server = tokio::spawn(async move {
        let mut children = tokio::task::JoinSet::new();
        while let Ok((tcp, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            let (h1, refused) = counts.clone();
            children.spawn(async move {
                let Ok(mut tls)=acceptor.accept(tcp).await else {refused.fetch_add(1,Ordering::SeqCst);return;};
                assert_eq!(tls.get_ref().1.alpn_protocol(),Some(b"http/1.1".as_slice()));h1.fetch_add(1,Ordering::SeqCst);
                let mut buf=vec![0;8192];let n=tls.read(&mut buf).await.unwrap();
                assert!(std::str::from_utf8(&buf[..n]).unwrap().starts_with("HEAD "));
                tls.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {SIZE}\r\nETag: {}\r\nAccept-Ranges: bytes\r\nConnection: close\r\n\r\n",fixture::ETAG).as_bytes()).await.unwrap();
                let _=tls.shutdown().await;
            });
        }
    });
    let dir = tempfile::tempdir().unwrap();
    // Separate ordinary H1 control; never an H2 proof input.
    let settings = usagi::net::stack::NetSettings::from_settings(&Settings {
        http3_enabled: false,
        use_system_proxy: false,
        ..Settings::default()
    });
    let mut control = usagi::net::http12::Http12Client::new(&settings).unwrap();
    control
        .install_extra_root_der(&settings, ca.ca_der())
        .unwrap();
    let response = control
        .send(&usagi::net::stack::RequestSpec::head(&format!(
            "https://localhost:{port}/proof.bin"
        )))
        .await
        .unwrap();
    assert_eq!(response.version(), http::Version::HTTP_11);
    drop(response);
    let e = build(dir.path(), ca.ca_der(), Family::V4);
    let id = e
        .add(
            AddRequest::new(format!("https://localhost:{port}/proof.bin"))
                .filename("h1.bin")
                .start_paused(true),
        )
        .unwrap();
    e.resume(id).unwrap();
    until(|| e.task(id).unwrap().state.is_terminal()).await;
    assert_eq!(e.task(id).unwrap().state, TaskState::Failed);
    assert!(e.task(id).unwrap().paths.is_empty());
    assert!(h1.load(Ordering::SeqCst) > 0);
    assert!(refused.load(Ordering::SeqCst) > 0);
    assert!(!e
        .transport_open_evidence(id)
        .unwrap()
        .allocations
        .iter()
        .any(|a| a.action == "grant"));
    assert!(!Path::new(&e.task(id).unwrap().final_path).exists());
    println!(
        "S3 H1 SEPARATE PASS h1_metadata={} incompatible_h2_tls={}",
        h1.load(Ordering::SeqCst),
        refused.load(Ordering::SeqCst)
    );
    e.shutdown();
    server.abort();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_a11_unbound_and_wrong_family_ineligible() {
    for rep in 0..3 {
        for family in [Family::V4, Family::V6] {
            for fault in ["no-validator", "weak-bound", "vary-bound"] {
                let Some(f) = Fixture::start().await else {
                    return;
                };
                for g in &f.gates[..2] {
                    *g.fault.lock().unwrap() = Some(fault);
                }
                let dir = tempfile::tempdir().unwrap();
                let e = engine(&f, dir.path(), family);
                let id = add(&e, &f);
                e.resume(id).unwrap();
                until(|| e.task(id).unwrap().state.is_terminal()).await;
                assert_eq!(e.task(id).unwrap().state, TaskState::Failed);
                assert!(e.task(id).unwrap().paths.is_empty());
                assert!(!Path::new(&e.task(id).unwrap().final_path).exists());
                assert!(!e
                    .transport_open_evidence(id)
                    .unwrap()
                    .allocations
                    .iter()
                    .any(|a| a.action == "grant"));
                println!("S3 A11 unbound PASS rep={rep} family={family:?} fault={fault}");
                e.shutdown();
            }
        }
    }
    // Explicit v4 URL offered to a v6-only experiment cannot cross families.
    let Some(f) = Fixture::start().await else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let e = engine(&f, dir.path(), Family::V6);
    let id = e
        .add(
            AddRequest::new(f.url().replace("localhost", "127.0.0.1"))
                .filename("wrong.bin")
                .start_paused(true),
        )
        .unwrap();
    e.resume(id).unwrap();
    until(|| e.task(id).unwrap().state.is_terminal()).await;
    assert_eq!(e.task(id).unwrap().state, TaskState::Failed);
    assert!(e.task(id).unwrap().paths.is_empty());
    println!("S3 A11 wrong-family PASS");
    e.shutdown();
}

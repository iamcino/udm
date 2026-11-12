//! U8 verification harness: design section 4 cases that needed the
//! composed proof over a TLS fixture — `fetch` driven end to end
//! against served bytes, then `stage` — plus the epoch assertions on
//! the payload-mismatch path and the dual-signature fetch of case 13.
//!
//! Cases already proven by the in-crate unit tests are not re-run
//! here; they are named in the coverage map
//! (`review-artifacts/shen-u8-coverage-2026-11-12.md`). Case 10 and a
//! deterministic OS-process kill at the apply cuts are findings, not
//! tests, and appear in the READY report and the map.
//!
//! One surface note: `FetchSession::fetch` aggregates per-origin
//! failures into `FetchError::OriginsExhausted`, whose message embeds
//! each origin's refusal verbatim. The harness therefore asserts the
//! refusal twice over: the aggregated fetch error naming the refusal
//! for both origins (with zero payload hits where the case requires
//! it), and the exact `Refusal` variant from `verify_governed` over
//! the same served bytes with the session's own trust decision.

#[path = "common/mod.rs"]
mod common;

use common::u8_fixture::*;
use usagi::update_apply::{current_journal, stage, staged_candidate, InstallLayout};
use usagi::update_fetch::FetchError;
use usagi::update_manifest::{Manifest, Refusal, VerifyContext};

/// The exact per-origin refusal over the served bytes, verified with
/// the session's trust decision the way `fetch_manifest` itself does.
fn verify_served(
    session: &usagi::update_fetch::FetchSession,
    json: &[u8],
    now_unix: i64,
    last_seen_epoch: u64,
) -> Result<usagi::update_manifest::VerifiedManifest, Refusal> {
    let manifest = Manifest::from_json(json).unwrap();
    let decision = session
        .client_trust()
        .decision(now_unix, manifest.manifest_epoch);
    manifest.verify_governed(
        &decision,
        &[],
        &VerifyContext {
            now_unix,
            last_seen_epoch,
            installed_version: INSTALLED,
        },
    )
}

/// Assert the fetch-level refusal shape: both origins refused with
/// the same reason (its Display text appears once per origin) and no
/// payload was ever requested.
fn assert_refused_both(err: &FetchError, phrase: &str, primary: &Loop, secondary: &Loop) {
    let FetchError::OriginsExhausted(message) = err else {
        panic!("expected OriginsExhausted, got {err:?}");
    };
    assert_eq!(
        message.matches(phrase).count(),
        2,
        "refusal phrase {phrase:?} must appear once per origin in: {message}"
    );
    assert_eq!(counts(&primary.log, "/payload"), 0, "primary payload hits");
    assert_eq!(
        counts(&secondary.log, "/payload"),
        0,
        "secondary payload hits"
    );
}

/// Case 1 (HARNESS-NEW): a valid manifest fetched over the TLS
/// fixture, then staged — the composition itself. On this path the
/// epoch file already holds the manifest epoch when the payload GET
/// arrives, and staging leaves the live install untouched.
#[tokio::test]
async fn case01_valid_manifest_fetch_then_stage() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(std::sync::Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server.clone()).await;
    let root = tempfile::tempdir().unwrap();
    write_epoch(&root.path().join("epoch.json"), STORED_EPOCH);

    let release = release_at(
        &[&publisher],
        primary.addr.port(),
        PAYLOAD_FILES,
        &ManifestSpec::default(),
    );
    // Observe the durable epoch at the moment the payload is served.
    let epoch_path = root.path().join("epoch.json");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
    let seen_task = std::sync::Arc::clone(&seen);
    let json = release.json.clone();
    let zip = release.zip.clone();
    primary.set(std::sync::Arc::new(move |hit: &Hit| {
        if hit.path == "/payload.zip" {
            *seen_task.lock().unwrap() = Some(read_epoch(&epoch_path));
        }
        body_for(hit, &json, &zip)
    }));
    secondary.set(std::sync::Arc::new(|_| Answer::Drop));
    let session = session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        publisher.trust.clone(),
    );

    let payload = run_fetch_at(&session, root.path(), NOW).await.unwrap();
    assert_eq!(payload.files.len(), PAYLOAD_FILES.len());
    assert_eq!(
        *seen.lock().unwrap(),
        Some(7),
        "epoch stored before payload"
    );
    assert_eq!(read_epoch(&root.path().join("epoch.json")), 7);
    for (rel, bytes) in PAYLOAD_FILES {
        let written = payload.files.iter().find(|f| f.path == *rel).unwrap();
        assert_eq!(std::fs::read(&written.absolute).unwrap(), *bytes);
    }

    // Stage the verified payload into an install holding the old build.
    let layout = InstallLayout::new(root.path().join("install"));
    write_tree(layout.base(), OLD_FILES);
    let candidate = stage(&layout, &payload, &session.client_trust(), NOW).unwrap();
    assert_eq!(candidate.version, "0.2.0");
    assert_eq!(candidate.files.len(), PAYLOAD_FILES.len());
    let held = staged_candidate(&layout).unwrap().unwrap();
    assert_eq!(held.version, "0.2.0");
    for (rel, bytes) in PAYLOAD_FILES {
        assert_eq!(std::fs::read(layout.staged_file(rel)).unwrap(), *bytes);
    }
    // Nothing live moved and no swap journal exists.
    for (rel, bytes) in OLD_FILES {
        assert_eq!(std::fs::read(layout.target_for(rel)).unwrap(), *bytes);
    }
    assert!(current_journal(&layout).unwrap().is_none());
}

/// Case 2 (HARNESS-NEW on the fixture): a one-byte change to a
/// still-parseable signed body is a BadSignature refusal and no
/// payload is requested from either origin.
#[tokio::test]
async fn case02_tampered_manifest_is_bad_signature() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(std::sync::Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server.clone()).await;
    let root = tempfile::tempdir().unwrap();
    write_epoch(&root.path().join("epoch.json"), STORED_EPOCH);

    let release = release_at(
        &[&publisher],
        primary.addr.port(),
        PAYLOAD_FILES,
        &ManifestSpec::default(),
    );
    let tampered = String::from_utf8(release.json.clone())
        .unwrap()
        .replace("\"version\":\"0.2.0\"", "\"version\":\"0.2.1\"")
        .into_bytes();
    assert_ne!(tampered, release.json);
    primary.set(serve_release(tampered.clone(), release.zip.clone()));
    secondary.set(serve_release(tampered.clone(), release.zip));
    let session = session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        publisher.trust.clone(),
    );

    let err = run_fetch_at(&session, root.path(), NOW).await.unwrap_err();
    assert_refused_both(
        &err,
        "manifest signature does not verify for the pinned publisher key",
        &primary,
        &secondary,
    );
    assert_eq!(
        verify_served(&session, &tampered, NOW, STORED_EPOCH).unwrap_err(),
        Refusal::BadSignature
    );
    assert_eq!(read_epoch(&root.path().join("epoch.json")), STORED_EPOCH);
}

/// Case 4, whole-archive mismatch (HARNESS-NEW for the epoch
/// assertion): the archive is discarded, the epoch store stands, and
/// a later fetch of the same epoch is refused as a replay.
#[tokio::test]
async fn case04_whole_file_mismatch_keeps_epoch_and_replay_is_refused() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(std::sync::Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server.clone()).await;
    let root = tempfile::tempdir().unwrap();
    write_epoch(&root.path().join("epoch.json"), STORED_EPOCH);

    let spec = ManifestSpec {
        archive_hash: Some("ab".repeat(32)),
        ..ManifestSpec::default()
    };
    let release = release_at(&[&publisher], primary.addr.port(), PAYLOAD_FILES, &spec);
    primary.set(serve_release(release.json.clone(), release.zip.clone()));
    secondary.set(std::sync::Arc::new(|_| Answer::Drop));
    let session = session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        publisher.trust.clone(),
    );

    let err = run_fetch_at(&session, root.path(), NOW).await.unwrap_err();
    assert!(matches!(err, FetchError::WholeFileMismatch), "{err:?}");
    let out = root.path().join("out");
    assert!(
        !out.join(".u3-payload.partial").exists(),
        "partial discarded"
    );
    for (rel, _) in PAYLOAD_FILES {
        assert!(!out.join(rel).exists(), "no extracted file left: {rel}");
    }
    // Acceptance stored the epoch before the payload GET; the failed
    // payload does not undo it. That store is not a defect.
    assert_eq!(read_epoch(&root.path().join("epoch.json")), 7);
    let payload_hits = counts(&primary.log, "/payload");
    assert_eq!(payload_hits, 1);

    // A later fetch of the same epoch is a replay refusal. Both
    // origins now serve the manifest, so both refuse it.
    secondary.set(serve_release(release.json.clone(), release.zip.clone()));
    let err = run_fetch_at(&session, root.path(), NOW).await.unwrap_err();
    let FetchError::OriginsExhausted(message) = &err else {
        panic!("expected OriginsExhausted, got {err:?}");
    };
    assert_eq!(message.matches("does not exceed stored epoch").count(), 2);
    assert_eq!(counts(&primary.log, "/payload"), payload_hits);
    assert_eq!(
        verify_served(&session, &release.json, NOW, 7).unwrap_err(),
        Refusal::EpochNotIncreasing {
            manifest_epoch: 7,
            last_seen_epoch: 7
        }
    );
}

/// Case 4, member mismatch (HARNESS-NEW for the epoch assertion):
/// same epoch behaviour when one member's hash disagrees.
#[tokio::test]
async fn case04_member_mismatch_keeps_epoch_and_replay_is_refused() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(std::sync::Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server.clone()).await;
    let root = tempfile::tempdir().unwrap();
    write_epoch(&root.path().join("epoch.json"), STORED_EPOCH);

    let spec = ManifestSpec {
        member_override: Some((
            "udm.exe".to_owned(),
            "ab".repeat(32),
            PAYLOAD_FILES[0].1.len() as u64,
        )),
        ..ManifestSpec::default()
    };
    let release = release_at(&[&publisher], primary.addr.port(), PAYLOAD_FILES, &spec);
    primary.set(serve_release(release.json.clone(), release.zip.clone()));
    secondary.set(std::sync::Arc::new(|_| Answer::Drop));
    let session = session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        publisher.trust.clone(),
    );

    let err = run_fetch_at(&session, root.path(), NOW).await.unwrap_err();
    assert!(
        matches!(err, FetchError::MemberMismatch { ref path } if path == "udm.exe"),
        "{err:?}"
    );
    let out = root.path().join("out");
    assert!(
        !out.join(".u3-payload.partial").exists(),
        "partial discarded"
    );
    for (rel, _) in PAYLOAD_FILES {
        assert!(!out.join(rel).exists(), "no extracted file left: {rel}");
    }
    assert_eq!(read_epoch(&root.path().join("epoch.json")), 7);

    // Both origins serve the manifest for the replay, so both refuse.
    secondary.set(serve_release(release.json.clone(), release.zip.clone()));
    let err = run_fetch_at(&session, root.path(), NOW).await.unwrap_err();
    let FetchError::OriginsExhausted(message) = &err else {
        panic!("expected OriginsExhausted, got {err:?}");
    };
    assert_eq!(message.matches("does not exceed stored epoch").count(), 2);
    assert_eq!(
        verify_served(&session, &release.json, NOW, 7).unwrap_err(),
        Refusal::EpochNotIncreasing {
            manifest_epoch: 7,
            last_seen_epoch: 7
        }
    );
}

/// Case 5 (HARNESS-NEW on the fixture): an equal version and a lower
/// version are both refused as downgrades.
#[tokio::test]
async fn case05_downgrade_equal_and_lower_versions_refused() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(std::sync::Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server.clone()).await;
    let root = tempfile::tempdir().unwrap();
    write_epoch(&root.path().join("epoch.json"), STORED_EPOCH);
    let session = session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        publisher.trust.clone(),
    );

    for version in ["0.1.5", "0.1.0"] {
        let spec = ManifestSpec {
            version: version.to_owned(),
            ..ManifestSpec::default()
        };
        let release = release_at(&[&publisher], primary.addr.port(), PAYLOAD_FILES, &spec);
        primary.set(serve_release(release.json.clone(), release.zip.clone()));
        secondary.set(serve_release(release.json.clone(), release.zip));
        let err = run_fetch_at(&session, root.path(), NOW).await.unwrap_err();
        assert_refused_both(
            &err,
            "does not exceed installed version",
            &primary,
            &secondary,
        );
        assert_eq!(
            verify_served(&session, &release.json, NOW, STORED_EPOCH).unwrap_err(),
            Refusal::VersionNotIncreasing {
                manifest: version.to_owned(),
                installed: INSTALLED.to_owned()
            }
        );
        assert_eq!(read_epoch(&root.path().join("epoch.json")), STORED_EPOCH);
    }
}

/// Case 6 (HARNESS-NEW on the fixture): epoch replay — equal, lower
/// and zero epochs against a stored epoch are all refused.
#[tokio::test]
async fn case06_epoch_replay_equal_lower_and_zero_refused() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(std::sync::Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server.clone()).await;
    let root = tempfile::tempdir().unwrap();
    write_epoch(&root.path().join("epoch.json"), STORED_EPOCH);
    let session = session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        publisher.trust.clone(),
    );

    for epoch in [STORED_EPOCH, STORED_EPOCH - 1, 0] {
        let spec = ManifestSpec {
            epoch,
            ..ManifestSpec::default()
        };
        let release = release_at(&[&publisher], primary.addr.port(), PAYLOAD_FILES, &spec);
        primary.set(serve_release(release.json.clone(), release.zip.clone()));
        secondary.set(serve_release(release.json.clone(), release.zip));
        let err = run_fetch_at(&session, root.path(), NOW).await.unwrap_err();
        assert_refused_both(&err, "does not exceed stored epoch", &primary, &secondary);
        assert_eq!(
            verify_served(&session, &release.json, NOW, STORED_EPOCH).unwrap_err(),
            Refusal::EpochNotIncreasing {
                manifest_epoch: epoch,
                last_seen_epoch: STORED_EPOCH
            }
        );
        assert_eq!(read_epoch(&root.path().join("epoch.json")), STORED_EPOCH);
    }
}

/// Case 7 (HARNESS-NEW on the fixture): `valid_until <= now` is
/// expired, including equality.
#[tokio::test]
async fn case07_expired_valid_until_including_equality_refused() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(std::sync::Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server.clone()).await;
    let root = tempfile::tempdir().unwrap();
    write_epoch(&root.path().join("epoch.json"), STORED_EPOCH);
    let session = session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        publisher.trust.clone(),
    );

    for valid_until in [NOW, NOW - 1] {
        let spec = ManifestSpec {
            valid_until,
            ..ManifestSpec::default()
        };
        let release = release_at(&[&publisher], primary.addr.port(), PAYLOAD_FILES, &spec);
        primary.set(serve_release(release.json.clone(), release.zip.clone()));
        secondary.set(serve_release(release.json.clone(), release.zip));
        let err = run_fetch_at(&session, root.path(), NOW).await.unwrap_err();
        assert_refused_both(&err, "manifest expired at", &primary, &secondary);
        assert_eq!(
            verify_served(&session, &release.json, NOW, STORED_EPOCH).unwrap_err(),
            Refusal::Expired {
                valid_until,
                now: NOW
            }
        );
        assert_eq!(read_epoch(&root.path().join("epoch.json")), STORED_EPOCH);
    }
}

/// Case 8 (HARNESS-NEW on the fixture): `issued_at` more than one
/// day ahead of local time is refused; exactly one day ahead is
/// accepted by the predicate, shown by a successful fetch.
#[tokio::test]
async fn case08_issued_at_more_than_one_day_ahead_refused() {
    let certs = fixture_certs();
    let publisher = publisher();
    let primary = spawn(std::sync::Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server.clone()).await;
    let root = tempfile::tempdir().unwrap();
    write_epoch(&root.path().join("epoch.json"), STORED_EPOCH);
    let session = session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        publisher.trust.clone(),
    );

    let issued = NOW + 2 * 86_400;
    let spec = ManifestSpec {
        issued_at: issued,
        valid_until: issued + 7 * 86_400,
        ..ManifestSpec::default()
    };
    let release = release_at(&[&publisher], primary.addr.port(), PAYLOAD_FILES, &spec);
    primary.set(serve_release(release.json.clone(), release.zip.clone()));
    secondary.set(serve_release(release.json.clone(), release.zip));
    let err = run_fetch_at(&session, root.path(), NOW).await.unwrap_err();
    assert_refused_both(
        &err,
        "more than 1 day ahead of local time",
        &primary,
        &secondary,
    );
    assert_eq!(
        verify_served(&session, &release.json, NOW, STORED_EPOCH).unwrap_err(),
        Refusal::IssuedTooFarAhead {
            issued_at: issued,
            now: NOW
        }
    );
    assert_eq!(read_epoch(&root.path().join("epoch.json")), STORED_EPOCH);

    // Boundary: exactly one day ahead is accepted.
    let issued = NOW + 86_400;
    let spec = ManifestSpec {
        issued_at: issued,
        valid_until: issued + 7 * 86_400,
        ..ManifestSpec::default()
    };
    let release = release_at(&[&publisher], primary.addr.port(), PAYLOAD_FILES, &spec);
    primary.set(serve_release(release.json, release.zip));
    secondary.set(std::sync::Arc::new(|_| Answer::Drop));
    let payload = run_fetch_at(&session, root.path(), NOW).await.unwrap();
    assert_eq!(payload.files.len(), PAYLOAD_FILES.len());
    assert_eq!(read_epoch(&root.path().join("epoch.json")), 7);
}

/// Case 9 (HARNESS-NEW for the refused fetch): after the session
/// adopts a revocation record for its publisher, a fetch of a
/// manifest signed by that key is refused and no payload is hit.
#[tokio::test]
async fn case09_revoked_key_fetch_refused() {
    let certs = fixture_certs();
    let publisher = publisher();
    let recovery = common::u8_fixture::publisher();
    let primary = spawn(std::sync::Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server.clone()).await;
    let root = tempfile::tempdir().unwrap();
    write_epoch(&root.path().join("epoch.json"), STORED_EPOCH);
    let session = session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        publisher.trust.clone(),
    );
    session
        .set_recovery_key(recovery.trust.public_key, &recovery.trust.pin)
        .unwrap();
    let record = revocation_record_bytes(&publisher, &recovery, NOW - 60);
    session.accept_revocation_record(&record, NOW).unwrap();

    let release = release_at(
        &[&publisher],
        primary.addr.port(),
        PAYLOAD_FILES,
        &ManifestSpec::default(),
    );
    primary.set(serve_release(release.json.clone(), release.zip.clone()));
    secondary.set(serve_release(release.json.clone(), release.zip));
    let err = run_fetch_at(&session, root.path(), NOW).await.unwrap_err();
    assert_refused_both(
        &err,
        "is revoked; refusing the manifest",
        &primary,
        &secondary,
    );
    assert_eq!(
        verify_served(&session, &release.json, NOW, STORED_EPOCH).unwrap_err(),
        Refusal::RevokedKey {
            key_id: publisher.trust.pin.clone()
        }
    );
    assert_eq!(read_epoch(&root.path().join("epoch.json")), STORED_EPOCH);
}

/// Case 13 (fixture composition HARNESS-NEW): the rotation record
/// is adopted before cutover (30-day lead, one release cycle); the
/// fixture then serves a dual-signed manifest inside the window,
/// which the client verifies with both keys already known; a
/// successor-only manifest is served after the window beside it. A
/// client that first accepts the announcement at cutover does not
/// trust the successor.
#[tokio::test]
async fn case13_dual_signed_manifest_fetch_after_rotation() {
    let certs = fixture_certs();
    let old = publisher();
    let successor = publisher();
    let recovery = publisher();
    let primary = spawn(std::sync::Arc::clone(&certs.server)).await;
    let secondary = spawn(certs.server.clone()).await;
    let root = tempfile::tempdir().unwrap();
    write_epoch(&root.path().join("epoch.json"), STORED_EPOCH);

    let cutover_at = ISSUED + 31 * 86_400;
    let window_until = cutover_at + 10 * 86_400;
    let record = rotation_record_bytes(
        &old,
        &successor,
        ISSUED,
        cutover_at,
        window_until,
        STORED_EPOCH,
        7,
    );
    let session = session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        old.trust.clone(),
    );
    session
        .set_recovery_key(recovery.trust.public_key, &recovery.trust.pin)
        .unwrap();
    // Adopted strictly before cutover, against stored epoch 6.
    session
        .accept_rotation_record(&record, ISSUED + 86_400, STORED_EPOCH)
        .unwrap();

    // Inside the dual window: a manifest signed by both keys.
    let in_window = cutover_at + 86_400;
    let spec = ManifestSpec {
        issued_at: cutover_at,
        valid_until: cutover_at + 7 * 86_400,
        ..ManifestSpec::default()
    };
    let release = release_at(
        &[&old, &successor],
        primary.addr.port(),
        PAYLOAD_FILES,
        &spec,
    );
    primary.set(serve_release(release.json, release.zip));
    secondary.set(std::sync::Arc::new(|_| Answer::Drop));
    let payload = run_fetch_at(&session, root.path(), in_window)
        .await
        .unwrap();
    assert_eq!(payload.files.len(), PAYLOAD_FILES.len());
    let pins = payload.manifest.signer_pins();
    assert_eq!(pins.len(), 2, "both signatures authorized: {pins:?}");
    assert!(pins.contains(&old.trust.pin));
    assert!(pins.contains(&successor.trust.pin));
    assert_eq!(read_epoch(&root.path().join("epoch.json")), 7);

    // After the window: a manifest signed only by the successor.
    let after_window = window_until + 86_400;
    let spec = ManifestSpec {
        epoch: 8,
        issued_at: after_window - 86_400,
        valid_until: after_window + 6 * 86_400,
        ..ManifestSpec::default()
    };
    let release = release_at(&[&successor], primary.addr.port(), PAYLOAD_FILES, &spec);
    primary.set(serve_release(release.json, release.zip));
    let payload = run_fetch_at(&session, root.path(), after_window)
        .await
        .unwrap();
    assert_eq!(
        payload.manifest.signer_pins(),
        [successor.trust.pin.clone()].as_slice()
    );
    assert_eq!(read_epoch(&root.path().join("epoch.json")), 8);

    // A client that first sees the announcement at cutover does not
    // start trusting the successor.
    let late = common::u8_fixture::session(
        origins_for(primary.addr, secondary.addr),
        &certs,
        old.trust.clone(),
    );
    late.set_recovery_key(recovery.trust.public_key, &recovery.trust.pin)
        .unwrap();
    assert!(
        late.accept_rotation_record(&record, cutover_at, STORED_EPOCH)
            .is_err(),
        "first seen at cutover must be refused"
    );
}

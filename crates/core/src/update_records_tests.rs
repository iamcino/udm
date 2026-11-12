use super::*;

use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair};

use crate::update_manifest::{Manifest, ManifestFile, ManifestSignature, VerifyContext};
use crate::update_pin;

const ISSUED: i64 = 1_700_000_000;
const DAY: i64 = 86_400;

struct TestKey {
    pair: Ed25519KeyPair,
    id: Ed25519Identity,
}

fn test_key() -> TestKey {
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new()).expect("generate key");
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("parse key");
    let mut public_key = [0u8; 32];
    public_key.copy_from_slice(pair.public_key().as_ref());
    let id = Ed25519Identity::from_public_key(public_key);
    assert_ne!(id.pin(), update_pin::publisher_pin());
    assert_ne!(id.pin(), update_pin::recovery_pin());
    TestKey { pair, id }
}

fn rotation_bytes(
    signer: &TestKey,
    successor: &Ed25519Identity,
    issued_at: i64,
    cutover_at: i64,
    window_until: i64,
    announced_epoch: u64,
    cutover_epoch: u64,
) -> Vec<u8> {
    let bare = RotationRecord {
        successor: successor.clone(),
        issued_at,
        cutover_at,
        window_until,
        announced_epoch,
        cutover_epoch,
        signer_key_id: signer.id.pin().to_owned(),
        signature: String::new(),
    };
    let signature = hex::encode(signer.pair.sign(&bare.canonical_bytes()).as_ref());
    format!(
        "{{\"announced_epoch\":{announced_epoch},\"cutover_at\":{cutover_at},\
         \"cutover_epoch\":{cutover_epoch},\"issued_at\":{issued_at},\
         \"kind\":\"rotation\",\"new_public_key\":\"{}\",\"window_until\":{window_until},\
         \"signatures\":[{{\"key_id\":\"{}\",\"signature\":\"{signature}\"}}]}}",
        hex::encode(successor.public_key()),
        signer.id.pin()
    )
    .into_bytes()
}

fn good_rotation(signer: &TestKey, successor: &Ed25519Identity) -> Vec<u8> {
    rotation_bytes(
        signer,
        successor,
        ISSUED,
        ISSUED + MIN_ROTATION_LEAD_SECS,
        ISSUED + MIN_ROTATION_LEAD_SECS + 10 * DAY,
        6,
        7,
    )
}

fn revocation_bytes(recovery: &TestKey, key_id: &str, issued_at: i64) -> Vec<u8> {
    let bare = RevocationRecord {
        key_id: key_id.to_owned(),
        issued_at,
        signer_key_id: recovery.id.pin().to_owned(),
        signature: String::new(),
    };
    let signature = hex::encode(recovery.pair.sign(&bare.canonical_bytes()).as_ref());
    format!(
        "{{\"issued_at\":{issued_at},\"key_id\":\"{key_id}\",\"kind\":\"revocation\",\
         \"signatures\":[{{\"key_id\":\"{}\",\"signature\":\"{signature}\"}}]}}",
        recovery.id.pin()
    )
    .into_bytes()
}

fn signed_manifest(signers: &[&TestKey], epoch: u64, issued_at: i64) -> Manifest {
    let mut manifest = Manifest {
        version: "0.2.0".to_owned(),
        manifest_epoch: epoch,
        issued_at,
        valid_until: issued_at + 7 * DAY,
        files: vec![ManifestFile {
            path: "udm.exe".to_owned(),
            sha256: "ab".repeat(32),
            size: 4,
        }],
        min_version: "0.1.0".to_owned(),
        urls: vec!["https://example.invalid/udm.zip".to_owned()],
        signatures: Vec::new(),
    };
    let canonical = manifest.canonical_bytes();
    manifest.signatures = signers
        .iter()
        .map(|key| {
            let signature = hex::encode(key.pair.sign(&canonical).as_ref());
            ManifestSignature {
                key_id: key.id.pin().to_owned(),
                signature,
            }
        })
        .collect();
    manifest
}

fn ctx(now: i64, last_seen: u64) -> VerifyContext<'static> {
    VerifyContext {
        now_unix: now,
        last_seen_epoch: last_seen,
        installed_version: "0.1.0",
    }
}

fn trust_with(publisher: &TestKey, recovery: &TestKey) -> ClientTrust {
    let mut trust = ClientTrust::bootstrap(publisher.id.clone()).expect("bootstrap");
    trust.bind_recovery(&recovery.id);
    trust
}

#[test]
fn rotation_record_verifies_and_is_adopted_before_cutover() {
    let publisher = test_key();
    let successor = test_key();
    let recovery = test_key();
    let bytes = good_rotation(&publisher, &successor.id);
    let parsed = RotationRecord::from_json(&bytes).expect("parse");
    parsed
        .verify(&publisher.id, ISSUED + DAY)
        .expect("record verifies");
    assert_eq!(parsed.successor.pin(), successor.id.pin());

    let mut trust = trust_with(&publisher, &recovery);
    trust
        .accept_rotation(&bytes, ISSUED + DAY, 6, &recovery.id)
        .expect("adopt");
    assert_eq!(trust.successor_pin(), Some(successor.id.pin()));
    let before = trust.decision(ISSUED + DAY, 7);
    assert_eq!(before.phase, TrustPhase::AwaitingCutover);
    assert_eq!(before.authorizing.len(), 1);
    assert_eq!(before.authorizing[0].pin(), publisher.id.pin());
    assert_eq!(before.known.len(), 2);
}

#[test]
fn rotation_schedule_rejects_a_short_lead_a_missing_cycle_and_a_long_window() {
    let publisher = test_key();
    let successor = test_key();
    let now = ISSUED + DAY;
    let short_lead = rotation_bytes(
        &publisher,
        &successor.id,
        ISSUED,
        ISSUED + MIN_ROTATION_LEAD_SECS - 1,
        ISSUED + MIN_ROTATION_LEAD_SECS + DAY,
        6,
        7,
    );
    assert!(matches!(
        RotationRecord::from_json(&short_lead)
            .unwrap()
            .verify(&publisher.id, now),
        Err(RecordError::LeadTimeTooShort { .. })
    ));

    let same_epoch = rotation_bytes(
        &publisher,
        &successor.id,
        ISSUED,
        ISSUED + MIN_ROTATION_LEAD_SECS,
        ISSUED + MIN_ROTATION_LEAD_SECS + DAY,
        6,
        6,
    );
    assert!(matches!(
        RotationRecord::from_json(&same_epoch)
            .unwrap()
            .verify(&publisher.id, now),
        Err(RecordError::ReleaseCycleTooShort { .. })
    ));

    let long_window = rotation_bytes(
        &publisher,
        &successor.id,
        ISSUED,
        ISSUED + MIN_ROTATION_LEAD_SECS,
        ISSUED + MIN_ROTATION_LEAD_SECS + MAX_DUAL_WINDOW_SECS + 1,
        6,
        7,
    );
    assert!(matches!(
        RotationRecord::from_json(&long_window)
            .unwrap()
            .verify(&publisher.id, now),
        Err(RecordError::DualWindowInvalid { .. })
    ));

    let boundary = rotation_bytes(
        &publisher,
        &successor.id,
        ISSUED,
        ISSUED + MIN_ROTATION_LEAD_SECS,
        ISSUED + MIN_ROTATION_LEAD_SECS + MAX_DUAL_WINDOW_SECS,
        6,
        7,
    );
    RotationRecord::from_json(&boundary)
        .unwrap()
        .verify(&publisher.id, now)
        .expect("exact 30 day lead and 30 day window are accepted");
}

#[test]
fn a_client_that_first_sees_the_announcement_at_cutover_does_not_trust_the_new_key() {
    let publisher = test_key();
    let successor = test_key();
    let recovery = test_key();
    let bytes = good_rotation(&publisher, &successor.id);
    let mut trust = trust_with(&publisher, &recovery);
    let cutover = ISSUED + MIN_ROTATION_LEAD_SECS;
    let err = trust
        .accept_rotation(&bytes, cutover, 6, &recovery.id)
        .expect_err("at cutover is too late");
    assert!(matches!(err, RecordError::AnnouncementAfterCutover { .. }));
    assert!(!trust.has_rotation());

    let manifest = signed_manifest(&[&successor], 7, cutover);
    let err = manifest
        .verify_governed(
            &trust.decision(cutover, 7),
            &[recovery.id.pin().to_owned()],
            &ctx(cutover, 6),
        )
        .expect_err("unannounced successor");
    assert_eq!(err, crate::update_manifest::Refusal::UnannouncedPublisher);
}

#[test]
fn cutover_waits_for_both_the_clock_and_one_release_cycle() {
    let publisher = test_key();
    let successor = test_key();
    let recovery = test_key();
    let mut trust = trust_with(&publisher, &recovery);
    trust
        .accept_rotation(
            &good_rotation(&publisher, &successor.id),
            ISSUED + DAY,
            6,
            &recovery.id,
        )
        .unwrap();
    let cutover = ISSUED + MIN_ROTATION_LEAD_SECS;
    let recovery_pins = vec![recovery.id.pin().to_owned()];

    let early = signed_manifest(&[&successor], 7, ISSUED + DAY);
    assert_eq!(
        early
            .verify_governed(
                &trust.decision(ISSUED + DAY, 7),
                &recovery_pins,
                &ctx(ISSUED + DAY, 6)
            )
            .unwrap_err(),
        crate::update_manifest::Refusal::CutoverNotStarted
    );

    let old_only = signed_manifest(&[&publisher], 7, ISSUED + DAY);
    old_only
        .verify_governed(
            &trust.decision(ISSUED + DAY, 7),
            &recovery_pins,
            &ctx(ISSUED + DAY, 6),
        )
        .expect("retiring key still authorizes before cutover");

    let epoch_waiting = signed_manifest(&[&successor], 6, cutover);
    // Epoch 6 is not greater than a stored epoch of 6, so freshness of the
    // key is not the failure: the epoch check refuses it. Use last_seen 5
    // so the only reason a successor-only manifest fails is the cycle.
    let err = epoch_waiting
        .verify_governed(
            &trust.decision(cutover, 6),
            &recovery_pins,
            &ctx(cutover, 5),
        )
        .unwrap_err();
    assert_eq!(err, crate::update_manifest::Refusal::CutoverNotStarted);
}

#[test]
fn either_key_authorizes_during_the_dual_window_and_only_the_new_key_after_it() {
    let publisher = test_key();
    let successor = test_key();
    let recovery = test_key();
    let stranger = test_key();
    let mut trust = trust_with(&publisher, &recovery);
    trust
        .accept_rotation(
            &good_rotation(&publisher, &successor.id),
            ISSUED + DAY,
            6,
            &recovery.id,
        )
        .unwrap();
    let during = ISSUED + MIN_ROTATION_LEAD_SECS + DAY;
    let after = ISSUED + MIN_ROTATION_LEAD_SECS + 10 * DAY;
    let recovery_pins = vec![recovery.id.pin().to_owned()];

    for signers in [
        &[&publisher][..],
        &[&successor][..],
        &[&publisher, &successor][..],
    ] {
        let manifest = signed_manifest(signers, 7, during);
        manifest
            .verify_governed(&trust.decision(during, 7), &recovery_pins, &ctx(during, 6))
            .expect("either key during the window");
    }

    let stranger_manifest = signed_manifest(&[&stranger], 7, during);
    assert_eq!(
        stranger_manifest
            .verify_governed(&trust.decision(during, 7), &recovery_pins, &ctx(during, 6))
            .unwrap_err(),
        crate::update_manifest::Refusal::SignerNotTrusted
    );

    let retired = signed_manifest(&[&publisher], 8, after);
    assert_eq!(
        retired
            .verify_governed(&trust.decision(after, 8), &recovery_pins, &ctx(after, 7))
            .unwrap_err(),
        crate::update_manifest::Refusal::SignerNotTrusted
    );
    let current = signed_manifest(&[&successor], 8, after);
    current
        .verify_governed(&trust.decision(after, 8), &recovery_pins, &ctx(after, 7))
        .expect("successor authorizes after the window");
}

#[test]
fn tampered_rotation_and_the_wrong_signer_are_refused() {
    let publisher = test_key();
    let successor = test_key();
    let other = test_key();
    let recovery = test_key();
    let mut parsed = RotationRecord::from_json(&good_rotation(&publisher, &successor.id)).unwrap();
    let flipped = if parsed.signature.starts_with('a') {
        "b"
    } else {
        "a"
    };
    parsed.signature.replace_range(0..1, flipped);
    assert_eq!(
        parsed.verify(&publisher.id, ISSUED + DAY),
        Err(RecordError::BadSignature)
    );

    let mut trust = trust_with(&publisher, &recovery);
    let signed_by_other = rotation_bytes(
        &other,
        &successor.id,
        ISSUED,
        ISSUED + MIN_ROTATION_LEAD_SECS,
        ISSUED + MIN_ROTATION_LEAD_SECS + DAY,
        6,
        7,
    );
    assert!(matches!(
        trust.accept_rotation(&signed_by_other, ISSUED + DAY, 6, &recovery.id),
        Err(RecordError::WrongSigner | RecordError::BadSignature)
    ));

    let self_signed = good_rotation(&successor, &publisher.id);
    assert!(trust
        .accept_rotation(&self_signed, ISSUED + DAY, 6, &recovery.id)
        .is_err());
}

#[test]
fn recovery_key_cannot_sign_a_rotation_or_a_manifest_or_be_revoked() {
    let publisher = test_key();
    let recovery = test_key();
    let successor = test_key();
    let mut trust = trust_with(&publisher, &recovery);
    let by_recovery = good_rotation(&recovery, &successor.id);
    assert_eq!(
        trust
            .accept_rotation(&by_recovery, ISSUED + DAY, 6, &recovery.id)
            .unwrap_err(),
        RecordError::WrongSigner
    );

    let onto_recovery = good_rotation(&publisher, &recovery.id);
    assert!(matches!(
        trust.accept_rotation(&onto_recovery, ISSUED + DAY, 6, &recovery.id),
        Err(RecordError::SuccessorRejected(_))
    ));

    let manifest = signed_manifest(&[&recovery], 7, ISSUED + DAY);
    let err = manifest
        .verify_governed(
            &trust.decision(ISSUED + DAY, 7),
            &[recovery.id.pin().to_owned()],
            &ctx(ISSUED + DAY, 6),
        )
        .unwrap_err();
    assert_eq!(err, crate::update_manifest::Refusal::RecoveryKeyForbidden);

    let names_recovery = revocation_bytes(&recovery, recovery.id.pin(), ISSUED + DAY);
    assert_eq!(
        trust
            .accept_revocation(&names_recovery, ISSUED + DAY, &recovery.id)
            .unwrap_err(),
        RecordError::RevokesRecoveryKey
    );
    assert!(trust.revoked_pins().is_empty());
}

#[test]
fn revocation_is_signed_by_the_recovery_key_and_then_blocks_that_publisher() {
    let publisher = test_key();
    let recovery = test_key();
    let mut trust = trust_with(&publisher, &recovery);
    let by_publisher = revocation_bytes(&publisher, publisher.id.pin(), ISSUED + DAY);
    assert!(matches!(
        trust.accept_revocation(&by_publisher, ISSUED + DAY, &recovery.id),
        Err(RecordError::WrongSigner | RecordError::BadSignature)
    ));

    let record = revocation_bytes(&recovery, publisher.id.pin(), ISSUED + DAY);
    trust
        .accept_revocation(&record, ISSUED + DAY, &recovery.id)
        .expect("revoke");
    trust
        .accept_revocation(&record, ISSUED + DAY, &recovery.id)
        .expect("adopting the same revocation twice is a no-op");
    assert_eq!(trust.revoked_pins(), vec![publisher.id.pin().to_owned()]);

    let manifest = signed_manifest(&[&publisher], 7, ISSUED + DAY);
    assert!(matches!(
        manifest.verify_governed(
            &trust.decision(ISSUED + DAY, 7),
            &[recovery.id.pin().to_owned()],
            &ctx(ISSUED + DAY, 6),
        ),
        Err(crate::update_manifest::Refusal::RevokedKey { .. })
    ));
    assert_eq!(
        trust
            .allows_staging(ISSUED + DAY, 7, &[publisher.id.pin().to_owned()])
            .unwrap_err(),
        StagingRefusal::Revoked {
            key_id: publisher.id.pin().to_owned(),
        }
    );
}

#[test]
fn revoking_both_publisher_keys_leaves_no_in_band_replacement() {
    let publisher = test_key();
    let successor = test_key();
    let recovery = test_key();
    let stranger = test_key();
    let mut trust = trust_with(&publisher, &recovery);
    trust
        .accept_rotation(
            &good_rotation(&publisher, &successor.id),
            ISSUED + DAY,
            6,
            &recovery.id,
        )
        .unwrap();
    let during = ISSUED + MIN_ROTATION_LEAD_SECS + DAY;
    trust
        .accept_revocation(
            &revocation_bytes(&recovery, publisher.id.pin(), during),
            during,
            &recovery.id,
        )
        .unwrap();
    trust
        .accept_revocation(
            &revocation_bytes(&recovery, successor.id.pin(), during),
            during,
            &recovery.id,
        )
        .unwrap();
    let manifest = signed_manifest(&[&stranger], 7, during);
    assert_eq!(
        manifest
            .verify_governed(
                &trust.decision(during, 7),
                &[recovery.id.pin().to_owned()],
                &ctx(during, 6),
            )
            .unwrap_err(),
        crate::update_manifest::Refusal::NoTrustedPublisher
    );
}

#[test]
fn a_later_announcement_can_replace_one_before_cutover_and_not_after() {
    let publisher = test_key();
    let first = test_key();
    let second = test_key();
    let recovery = test_key();
    let mut trust = trust_with(&publisher, &recovery);
    trust
        .accept_rotation(
            &good_rotation(&publisher, &first.id),
            ISSUED + DAY,
            6,
            &recovery.id,
        )
        .unwrap();
    let older = rotation_bytes(
        &publisher,
        &second.id,
        ISSUED - DAY,
        ISSUED - DAY + MIN_ROTATION_LEAD_SECS,
        ISSUED - DAY + MIN_ROTATION_LEAD_SECS + DAY,
        6,
        7,
    );
    assert!(matches!(
        trust.accept_rotation(&older, ISSUED + DAY, 6, &recovery.id),
        Err(RecordError::NotNewer { .. } | RecordError::IssuedTooFarAhead { .. })
    ));
    let replacement = rotation_bytes(
        &publisher,
        &second.id,
        ISSUED + 2 * DAY,
        ISSUED + 2 * DAY + MIN_ROTATION_LEAD_SECS,
        ISSUED + 2 * DAY + MIN_ROTATION_LEAD_SECS + DAY,
        6,
        7,
    );
    trust
        .accept_rotation(&replacement, ISSUED + 2 * DAY, 6, &recovery.id)
        .expect("newer announcement before cutover");
    assert_eq!(trust.successor_pin(), Some(second.id.pin()));

    let during = ISSUED + 2 * DAY + MIN_ROTATION_LEAD_SECS + DAY;
    let third = test_key();
    let from_retiring = rotation_bytes(
        &publisher,
        &third.id,
        during,
        during + MIN_ROTATION_LEAD_SECS,
        during + MIN_ROTATION_LEAD_SECS + DAY,
        7,
        8,
    );
    assert_eq!(
        trust
            .accept_rotation(&from_retiring, during, 7, &recovery.id)
            .unwrap_err(),
        RecordError::RetiringKeyClosed
    );
    assert_eq!(trust.successor_pin(), Some(second.id.pin()));
}

#[test]
fn announced_epoch_must_be_the_stored_epoch_or_the_next_one() {
    let publisher = test_key();
    let successor = test_key();
    let recovery = test_key();
    let mut trust = trust_with(&publisher, &recovery);
    let ahead = rotation_bytes(
        &publisher,
        &successor.id,
        ISSUED,
        ISSUED + MIN_ROTATION_LEAD_SECS,
        ISSUED + MIN_ROTATION_LEAD_SECS + DAY,
        9,
        10,
    );
    assert!(matches!(
        trust.accept_rotation(&ahead, ISSUED + DAY, 6, &recovery.id),
        Err(RecordError::AnnouncedEpochUnbound { .. })
    ));
}

#[test]
fn trust_state_round_trips_and_a_corrupt_file_does_not_clear_it() {
    let publisher = test_key();
    let recovery = test_key();
    let mut trust = trust_with(&publisher, &recovery);
    trust
        .accept_revocation(
            &revocation_bytes(&recovery, publisher.id.pin(), ISSUED + DAY),
            ISSUED + DAY,
            &recovery.id,
        )
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("updater-trust.json");
    trust.store(&path).unwrap();
    let mut loaded = ClientTrust::bootstrap(publisher.id.clone()).unwrap();
    loaded.load_into(&path).unwrap();
    assert_eq!(loaded.revoked_pins(), vec![publisher.id.pin().to_owned()]);

    std::fs::write(&path, b"{not-json").unwrap();
    let err = loaded.load_into(&path).unwrap_err();
    assert!(matches!(err, RecordError::Unreadable(_)));
    assert_eq!(loaded.revoked_pins(), vec![publisher.id.pin().to_owned()]);
}

#[test]
fn an_issued_at_more_than_one_day_ahead_is_refused() {
    let publisher = test_key();
    let successor = test_key();
    let future = rotation_bytes(
        &publisher,
        &successor.id,
        ISSUED + 3 * DAY,
        ISSUED + 3 * DAY + MIN_ROTATION_LEAD_SECS,
        ISSUED + 3 * DAY + MIN_ROTATION_LEAD_SECS + DAY,
        6,
        7,
    );
    assert!(matches!(
        RotationRecord::from_json(&future)
            .unwrap()
            .verify(&publisher.id, ISSUED),
        Err(RecordError::IssuedTooFarAhead { .. })
    ));
}

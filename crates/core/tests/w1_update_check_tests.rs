//! Wiring W1 tests: U8 case 10 at last, as a real test — the factored
//! update check (`usagi::update_check::run_update_check`) driven with
//! `FetchSession::with_tls` and TEST keys against the U8 loopback TLS
//! fixture. Tests never call `FetchSession::open`, never use the real
//! pins, and never use the real hosts.
//!
//! Case 10's shape: the same factored check runs with an explicit off
//! flag file and with the flag on. Off: the check reports that checks
//! are disabled, returns without error, the counter in front of the
//! session factory stays 0, and the fixture records 0 hits. On: the
//! factory runs once, the fixture records the manifest hit, and the
//! verified candidate is staged. The enabled run is what makes the zero
//! meaningful. A third case proves the default: an ABSENT flag file
//! resolves ON, so the factory IS called. A parity case proves an
//! automatic-style invocation (default resolved state) and a manual
//! invocation (explicit enable) classify the same fixture identically.
//!
//! The startup assertions read the GUI's `main.rs` as source: the
//! envelope asks that `udm.exe` startup calls `recover_updater` (before
//! the engine build and before the window is created) and does not call
//! the session factory. Asserting the process's socket count is
//! explicitly not the test.

#[path = "common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use common::u8_fixture::*;
use usagi::update_apply::{staged_candidate, InstallLayout};
use usagi::update_check::{checks_enabled, run_update_check, set_checks_enabled, CheckKind};

const FILES: &[(&str, &[u8])] = &[
    ("udm.exe", b"w1 new udm exe bytes"),
    ("udm-cli.exe", b"w1 new udm cli bytes"),
];

/// Two fixture origins serving the same release, with a counting
/// session factory armed in front of a `with_tls` session.
struct Rig {
    _root: tempfile::TempDir,
    layout: InstallLayout,
    loop_a: Loop,
    loop_b: Loop,
    factory_calls: Arc<AtomicUsize>,
    session: Option<usagi::update_fetch::FetchSession>,
}

async fn rig(spec: &ManifestSpec) -> Rig {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().join("install");
    std::fs::create_dir_all(&base).unwrap();
    let layout = InstallLayout::new(&base);

    let certs = fixture_certs();
    let publisher = publisher();
    let loop_a = spawn(certs.server.clone()).await;
    let loop_b = spawn(certs.server.clone()).await;
    let release = release_at(&[&publisher], loop_a.addr.port(), FILES, spec);
    loop_a.set(serve_release(release.json.clone(), release.zip.clone()));
    loop_b.set(serve_release(release.json, release.zip));
    let origins = origins_for(loop_a.addr, loop_b.addr);
    let session = session(origins, &certs, publisher.trust.clone());
    Rig {
        _root: root,
        layout,
        loop_a,
        loop_b,
        factory_calls: Arc::new(AtomicUsize::new(0)),
        session: Some(session),
    }
}

impl Rig {
    fn manifest_hits(&self) -> usize {
        counts(&self.loop_a.log, "/manifest") + counts(&self.loop_b.log, "/manifest")
    }
    fn payload_hits(&self) -> usize {
        counts(&self.loop_a.log, "/payload.zip") + counts(&self.loop_b.log, "/payload.zip")
    }
    async fn run(&mut self, now_unix: i64) -> usagi::update_check::CheckOutcome {
        let calls = self.factory_calls.clone();
        let session = self.session.take().expect("one session per rig");
        run_update_check(&self.layout, now_unix, move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(session)
        })
        .await
    }
}

#[tokio::test]
async fn case10_flag_off_builds_no_session_and_opens_no_socket() {
    let mut rig = rig(&ManifestSpec::default()).await;
    // The opt-out is durable content: an explicit off file, not an
    // absent one (absence resolves ON).
    set_checks_enabled(&rig.layout, false).unwrap();
    assert!(!checks_enabled(&rig.layout));

    let outcome = rig.run(NOW).await;

    assert_eq!(outcome.kind, CheckKind::Disabled);
    assert!(
        outcome.notice.contains("disabled"),
        "notice: {}",
        outcome.notice
    );
    assert_eq!(rig.factory_calls.load(Ordering::SeqCst), 0);
    assert_eq!(rig.manifest_hits(), 0);
    assert_eq!(rig.payload_hits(), 0);
    assert!(staged_candidate(&rig.layout).unwrap().is_none());
}

#[tokio::test]
async fn case10_flag_on_fetches_verifies_and_stages() {
    let mut rig = rig(&newer_spec()).await;
    set_checks_enabled(&rig.layout, true).unwrap();
    assert!(checks_enabled(&rig.layout));

    let outcome = rig.run(NOW).await;

    assert_eq!(outcome.kind, CheckKind::Available);
    assert!(
        outcome
            .notice
            .starts_with(&format!("Update available: version {}", newer_version())),
        "notice: {}",
        outcome.notice
    );
    assert!(outcome.notice.contains("udm.exe ("));
    assert_eq!(rig.factory_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.manifest_hits(), 1);
    assert_eq!(rig.payload_hits(), 1);
    let staged = staged_candidate(&rig.layout)
        .unwrap()
        .expect("a staged candidate");
    assert_eq!(staged.version, newer_version());
    // The accepted manifest's epoch was persisted durably by fetch.
    assert_eq!(
        read_epoch(&usagi::update_check::epoch_path(&rig.layout)),
        ManifestSpec::default().epoch
    );
}

#[tokio::test]
async fn case10_flag_absent_resolves_on_and_fetches() {
    let mut rig = rig(&newer_spec()).await;
    // No flag file at all: the default resolves ON, so the factory IS
    // called against the fixture.
    assert!(!rig.layout.base().join("update-checks-enabled").exists());
    assert!(checks_enabled(&rig.layout));

    let outcome = rig.run(NOW).await;

    assert_eq!(outcome.kind, CheckKind::Available);
    assert_eq!(rig.factory_calls.load(Ordering::SeqCst), 1);
    assert_eq!(rig.manifest_hits(), 1);
    assert_eq!(rig.payload_hits(), 1);
    assert!(staged_candidate(&rig.layout).unwrap().is_some());
}

#[tokio::test]
async fn automatic_and_manual_invocations_classify_identically() {
    // The automatic path runs with the default resolved state (flag
    // absent); the manual path runs after an explicit enable. Both
    // invoke the same factored check against the same fixture, so the
    // outcome kinds must match.
    let specs = [
        ManifestSpec::default(),
        ManifestSpec {
            version: usagi::VERSION.to_owned(),
            ..ManifestSpec::default()
        },
    ];
    for spec in &specs {
        let mut automatic = rig(spec).await;
        let mut manual = rig(spec).await;
        set_checks_enabled(&manual.layout, true).unwrap();

        let auto_outcome = automatic.run(NOW).await;
        let manual_outcome = manual.run(NOW).await;

        assert_eq!(
            auto_outcome.kind, manual_outcome.kind,
            "version {} must classify identically on both paths",
            spec.version
        );
        assert_eq!(automatic.factory_calls.load(Ordering::SeqCst), 1);
        assert_eq!(manual.factory_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn flag_on_already_current_when_both_origins_refuse_on_version() {
    let spec = ManifestSpec {
        version: usagi::VERSION.to_owned(),
        ..ManifestSpec::default()
    };
    let mut rig = rig(&spec).await;
    set_checks_enabled(&rig.layout, true).unwrap();

    let outcome = rig.run(NOW).await;

    assert_eq!(outcome.kind, CheckKind::AlreadyCurrent);
    // The two-origin manifest refusal shape: both origins were asked,
    // and the payload URL recorded 0 hits.
    assert_eq!(rig.manifest_hits(), 2);
    assert_eq!(rig.payload_hits(), 0);
    assert!(staged_candidate(&rig.layout).unwrap().is_none());
}

#[test]
fn startup_reconciliation_source_shape() {
    let main_rs = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../gui/src/main.rs");
    let src = std::fs::read_to_string(&main_rs).unwrap();

    let recover = src
        .find("recover_updater(layout, now_unix)")
        .expect("startup calls recover_updater");
    let engine = src
        .find("EngineBuilder::new().paths(paths).build()")
        .expect("engine build call");
    let window = src.find("AppWindow::new()").expect("window creation");
    assert!(recover < engine, "recovery runs before the engine builds");
    assert!(
        recover < window,
        "recovery runs before the window is created"
    );

    assert!(
        !src.contains("FetchSession::"),
        "startup must not call the session factory"
    );

    let setup = src.find("state::setup(&ui").expect("state setup call");
    let confirm = src
        .find("confirm_successful_start(layout)")
        .expect("startup confirms a first start");
    assert!(confirm > setup, "confirmation happens once the UI is ready");
    assert!(
        src.contains("Step::NewStaged"),
        "confirmation is gated on the NewStaged journal step"
    );
}

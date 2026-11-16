//! # User-facing update check
//!
//! A checks flag file on the install base, and one factored check that
//! the CLI and the GUI both call. This module only calls
//! `FetchSession`, `stage`, and the trust binding the updater requires.
//!
//! Trigger semantics: checks resolve ON by default — an absent flag
//! file means ON. The GUI runs a check at startup and every 6 hours
//! while running, and both surfaces also run one on explicit manual
//! action. A flag file whose trimmed content is `off` is a durable
//! opt-out. The flag is read once at check start, so no check ever
//! STARTS while checks resolve OFF; a check already in flight when the
//! user turns checks off runs to completion. Applying an update is
//! always a separate manual act: a successful check only stages the
//! verified candidate for a later apply.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use tokio_util::sync::CancellationToken;
use url::Url;

use crate::update_apply::{stage, ApplyError, InstallLayout};
use crate::update_fetch::{FetchError, FetchInput, FetchSession, Origin};
use crate::VERSION;

/// File on the install base whose content resolves the checks state:
/// absent means ON (the default), trimmed content `off` means OFF (a
/// durable opt-out), any other content means ON. It is distinct from
/// `updater-trust.json`, from the epoch file, and from the data-dir
/// `udm.redb`, and it is not a `Settings` field: the settings Save
/// pipeline never writes it.
pub const CHECKS_FLAG_FILE: &str = "update-checks-enabled";

/// Durable `last_seen_epoch` file on the install base (design section
/// 2.1), shared by the CLI and the GUI. Not `updater-trust.json` and
/// not the data-dir database.
pub const EPOCH_FILE: &str = "update-epoch";

/// Scratch directory on the install base a check downloads into before
/// staging. It is not the live install and not the staging area.
pub const DOWNLOAD_DIR: &str = "update-download";

/// Local log each check outcome appends one line to (design section
/// 2.1: every outcome produces a local log entry; nothing leaves the
/// machine).
pub const CHECK_LOG_FILE: &str = "update-check.log";

/// Update manifest on the GitHub release host (owner decision O2: both
/// public release hosts, GitHub first).
pub const GITHUB_MANIFEST_URL: &str =
    "https://github.com/iamcino/udm/releases/latest/download/update-manifest.json";

/// Update manifest on the GitLab release host (owner decision O2,
/// second origin).
pub const GITLAB_MANIFEST_URL: &str =
    "https://gitlab.com/cino-group/udm/-/releases/permalink/latest/downloads/update-manifest.json";

fn flag_path(layout: &InstallLayout) -> PathBuf {
    layout.base().join(CHECKS_FLAG_FILE)
}

/// The durable epoch file for this install.
pub fn epoch_path(layout: &InstallLayout) -> PathBuf {
    layout.base().join(EPOCH_FILE)
}

/// The scratch download directory for this install.
pub fn download_dir(layout: &InstallLayout) -> PathBuf {
    layout.base().join(DOWNLOAD_DIR)
}

fn check_log_path(layout: &InstallLayout) -> PathBuf {
    layout.base().join(CHECK_LOG_FILE)
}

/// The resolved checks state for this install: the flag file is
/// absent (ON, the default), or present with trimmed content `off`
/// (OFF, a durable opt-out), or present with any other content (ON).
pub fn checks_enabled(layout: &InstallLayout) -> bool {
    match std::fs::read_to_string(flag_path(layout)) {
        Ok(content) => content.trim() != "off",
        Err(_) => true,
    }
}

/// Write the checks flag. `on` writes `on\n`; `off` writes `off\n`.
/// The opt-out is durable content, not deletion: removing the file
/// would resolve ON again at the next read. Opens no socket either
/// way.
pub fn set_checks_enabled(layout: &InstallLayout, enabled: bool) -> std::io::Result<()> {
    let path = flag_path(layout);
    if enabled {
        std::fs::write(&path, b"on\n")
    } else {
        std::fs::write(&path, b"off\n")
    }
}

/// The two production origins, in the O2 order: the GitHub release
/// manifest first, the GitLab release manifest second. They are passed
/// to `FetchSession::open` by the caller; `open` itself has no built-in
/// origin.
pub fn release_origins() -> Result<[Origin; 2], FetchError> {
    Ok([
        Origin::new(Url::parse(GITHUB_MANIFEST_URL).expect("compiled GitHub manifest URL parses"))?,
        Origin::new(Url::parse(GITLAB_MANIFEST_URL).expect("compiled GitLab manifest URL parses"))?,
    ])
}

/// Which of the section 2.1 outcomes a check produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckKind {
    /// Checks are disabled: nothing was fetched, no socket was opened.
    Disabled,
    /// A newer build verified and is held in staging.
    Available,
    /// Both origins refused with the version refusal: already current.
    AlreadyCurrent,
    /// Both origins refused with the version-floor refusal: the
    /// published version is below the minimum accepted version.
    BelowVersionFloor,
    /// The manifest (or the staged candidate) was refused.
    ManifestRefused,
    /// A payload file's hash did not match the signed manifest.
    PayloadMismatch,
    /// Anything else: transport, HTTP, local I/O, trust setup.
    Failed,
}

/// The result of one check: its kind and the full user-facing notice
/// text (the section 2.1 notice shapes). `Available` carries one line
/// per payload file after the headline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Outcome classification.
    pub kind: CheckKind,
    /// User-facing text.
    pub notice: String,
}

fn failed(reason: String) -> CheckOutcome {
    CheckOutcome {
        kind: CheckKind::Failed,
        notice: format!("Update check failed: {reason}"),
    }
}

/// Append the local log line every outcome writes (best effort: a log
/// failure never changes the outcome).
fn log_check(layout: &InstallLayout, now_unix: i64, outcome: &CheckOutcome) {
    let headline = outcome.notice.lines().next().unwrap_or("");
    let line = format!("{now_unix} check kind={:?} {headline}\n", outcome.kind);
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(check_log_path(layout))
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Run one update check against `layout`, building the fetch session
/// through `make_session`.
///
/// Factored this way so the production caller passes
/// `|| FetchSession::open(release_origins()?)` while tests pass a
/// `FetchSession::with_tls` factory against a loopback fixture and
/// count how often the factory ran: while checks resolve OFF the
/// factory is never called.
///
/// The resolved flag state is read once, at check start. While checks
/// resolve OFF no check starts: the session factory is not called, no
/// socket opens, and the outcome is [`CheckKind::Disabled`]. A check
/// already in flight when the user turns checks off runs to
/// completion. With checks resolved ON, the session's trust file is
/// bound (so a rotation or revocation already stored on this install is
/// enforced; binding downloads nothing), then `fetch` runs, and on
/// success the payload is staged with the session's client trust.
/// Nothing here applies the staged candidate.
pub async fn run_update_check<F>(
    layout: &InstallLayout,
    now_unix: i64,
    make_session: F,
) -> CheckOutcome
where
    F: FnOnce() -> Result<FetchSession, FetchError>,
{
    let outcome = run_check(layout, now_unix, make_session).await;
    log_check(layout, now_unix, &outcome);
    outcome
}

async fn run_check<F>(layout: &InstallLayout, now_unix: i64, make_session: F) -> CheckOutcome
where
    F: FnOnce() -> Result<FetchSession, FetchError>,
{
    if !checks_enabled(layout) {
        return CheckOutcome {
            kind: CheckKind::Disabled,
            notice: "Update checks are disabled.".to_owned(),
        };
    }
    let session = match make_session() {
        Ok(session) => session,
        Err(err) => return failed(format!("{err}")),
    };
    if let Err(err) = session.bind_trust_file(&layout.updater_trust_file()) {
        return failed(format!("{err}"));
    }
    let epoch_path = epoch_path(layout);
    let dest_dir = download_dir(layout);
    let cancel = CancellationToken::new();
    let input = FetchInput {
        installed_version: VERSION,
        now_unix,
        epoch_path: &epoch_path,
        dest_dir: &dest_dir,
        cancel: &cancel,
    };
    let payload = match session.fetch(input).await {
        Ok(payload) => payload,
        Err(err) => return classify_fetch_error(err),
    };
    let version = payload.manifest.manifest().version.clone();
    match stage(layout, &payload, &session.client_trust(), now_unix) {
        Ok(_) => {
            let mut notice = format!("Update available: version {version}");
            for file in &payload.files {
                notice.push_str(&format!("\n  {} ({} bytes)", file.path, file.size));
            }
            CheckOutcome {
                kind: CheckKind::Available,
                notice,
            }
        }
        Err(ApplyError::StagingRefused(reason)) => CheckOutcome {
            kind: CheckKind::ManifestRefused,
            notice: format!("Manifest refused: {reason}"),
        },
        Err(ApplyError::BelowVersionFloor { version, floor }) => CheckOutcome {
            kind: CheckKind::BelowVersionFloor,
            notice: format!(
                "Update refused: version {version} is below the minimum accepted version {floor}."
            ),
        },
        Err(err) => failed(format!("{err}")),
    }
}

/// Classify a `fetch` error into a section 2.1 notice.
///
/// `fetch` never returns `FetchError::Refused`: it folds every manifest
/// failure into `FetchError::OriginsExhausted`, whose payload is
/// `origin 0: {first}; origin 1: {second}` (either origin may be named
/// first). So the manifest outcomes are classified from that string,
/// and `fetch` is not changed. Payload-phase errors return directly
/// from `fetch` and are matched on the enum.
fn classify_fetch_error(err: FetchError) -> CheckOutcome {
    match err {
        FetchError::OriginsExhausted(ref inner) => classify_origins_exhausted(inner, &err),
        FetchError::WholeFileMismatch => CheckOutcome {
            kind: CheckKind::PayloadMismatch,
            notice: "Payload hash mismatch: the payload archive".to_owned(),
        },
        FetchError::MemberMismatch { ref path } => CheckOutcome {
            kind: CheckKind::PayloadMismatch,
            notice: format!("Payload hash mismatch: {path}"),
        },
        FetchError::ExtraMember(_) | FetchError::MissingMember(_) => CheckOutcome {
            kind: CheckKind::ManifestRefused,
            notice: format!("Manifest refused: {err}"),
        },
        _ => failed(format!("{err}")),
    }
}

fn classify_origins_exhausted(inner: &str, whole: &FetchError) -> CheckOutcome {
    let Some((first, second)) = split_origin_pieces(inner) else {
        return failed(format!("{whole}"));
    };
    if is_version_refusal(first) && is_version_refusal(second) {
        // Both origins refused because the published version does not
        // exceed the installed one. It is still a refusal and no
        // payload request was made; the user-facing reading is that
        // the install is already current.
        return CheckOutcome {
            kind: CheckKind::AlreadyCurrent,
            notice: format!("Already current: version {VERSION} is the latest published version."),
        };
    }
    if is_floor_refusal(first) && is_floor_refusal(second) {
        // Both origins refused because the published version is below
        // the compiled minimum accepted version. Distinct from
        // already-current: the install is not current, the target is
        // simply too old to ever be offered.
        return CheckOutcome {
            kind: CheckKind::BelowVersionFloor,
            notice: format!("Update refused: {first}"),
        };
    }
    if is_manifest_refusal(first) && is_manifest_refusal(second) {
        return CheckOutcome {
            kind: CheckKind::ManifestRefused,
            notice: format!("Manifest refused: {first}"),
        };
    }
    // Either piece is transport, HTTP, or anything that is not a
    // manifest refusal (including a version refusal on one origin and
    // a different failure on the other).
    failed(format!("{whole}"))
}

/// Split `origin N: {a}; origin M: {b}` into its two pieces, whichever
/// origin was tried first.
fn split_origin_pieces(inner: &str) -> Option<(&str, &str)> {
    let rest = inner
        .strip_prefix("origin 0: ")
        .or_else(|| inner.strip_prefix("origin 1: "))?;
    let (first, rest) = rest.split_once("; origin ")?;
    let second = rest
        .strip_prefix("0: ")
        .or_else(|| rest.strip_prefix("1: "))?;
    Some((first, second))
}

/// The version refusal's wording in `update_manifest::Refusal`.
fn is_version_refusal(piece: &str) -> bool {
    piece.contains("does not exceed installed version")
}

/// The floor refusal's wording in `update_manifest::Refusal`.
fn is_floor_refusal(piece: &str) -> bool {
    piece.contains("is below the minimum accepted version")
}

/// Phrases that appear only in `update_manifest::Refusal` display
/// texts, so a piece of the `OriginsExhausted` string carrying one of
/// them is a manifest refusal rather than a transport or HTTP failure.
const REFUSAL_MARKERS: &[&str] = &[
    "does not exceed installed version",
    "is below the minimum accepted version",
    "does not exceed stored epoch",
    "manifest is malformed:",
    "manifest carries no signature",
    "manifest signature does not verify",
    "is below the manifest minimum",
    "manifest expired at",
    "validity window exceeds 14 days",
    "more than 1 day ahead of local time",
    "recovery key never signs",
    "is revoked; refusing the manifest",
    "no publisher key is trusted; refusing the manifest",
    "no rotation announcement",
    "cutover has not started",
    "not from a publisher key this install currently trusts",
    "update pins are not self-consistent",
];

fn is_manifest_refusal(piece: &str) -> bool {
    REFUSAL_MARKERS.iter().any(|marker| piece.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exhausted(msg: &str) -> CheckOutcome {
        classify_fetch_error(FetchError::OriginsExhausted(msg.to_owned()))
    }

    #[test]
    fn both_origins_version_refusal_is_already_current() {
        let outcome = exhausted(
            "origin 0: manifest version 0.2.0 does not exceed installed version 0.2.0; \
             origin 1: manifest version 0.2.0 does not exceed installed version 0.2.0",
        );
        assert_eq!(outcome.kind, CheckKind::AlreadyCurrent);
        assert!(outcome.notice.starts_with("Already current:"));
    }

    #[test]
    fn both_origins_floor_refusal_is_below_version_floor() {
        let outcome = exhausted(
            "origin 0: manifest version 0.0.9 is below the minimum accepted version 0.1.0; \
             origin 1: manifest version 0.0.9 is below the minimum accepted version 0.1.0",
        );
        assert_eq!(outcome.kind, CheckKind::BelowVersionFloor);
        assert!(outcome.notice.starts_with("Update refused:"));
        assert!(outcome.notice.contains("0.0.9"));
    }

    #[test]
    fn floor_refusal_plus_other_refusal_is_manifest_refused() {
        let outcome = exhausted(
            "origin 0: manifest version 0.0.9 is below the minimum accepted version 0.1.0; \
             origin 1: manifest epoch 3 does not exceed stored epoch 7; refusing as replay",
        );
        assert_eq!(outcome.kind, CheckKind::ManifestRefused);
    }

    #[test]
    fn floor_refusal_plus_transport_failure_is_failed() {
        let outcome = exhausted(
            "origin 0: manifest version 0.0.9 is below the minimum accepted version 0.1.0; \
             origin 1: transport failure: connection reset",
        );
        assert_eq!(outcome.kind, CheckKind::Failed);
        assert!(outcome.notice.starts_with("Update check failed:"));
    }

    #[test]
    fn both_origins_other_refusal_is_manifest_refused() {
        let outcome = exhausted(
            "origin 1: manifest epoch 3 does not exceed stored epoch 7; refusing as replay; \
             origin 0: manifest epoch 3 does not exceed stored epoch 7; refusing as replay",
        );
        assert_eq!(outcome.kind, CheckKind::ManifestRefused);
        assert_eq!(
            outcome.notice,
            "Manifest refused: manifest epoch 3 does not exceed stored epoch 7; \
             refusing as replay"
        );
    }

    #[test]
    fn version_refusal_plus_transport_failure_is_failed() {
        let outcome = exhausted(
            "origin 0: manifest version 0.2.0 does not exceed installed version 0.2.0; \
             origin 1: transport failure: connection reset",
        );
        assert_eq!(outcome.kind, CheckKind::Failed);
        assert!(outcome.notice.starts_with("Update check failed:"));
    }

    #[test]
    fn transport_on_both_origins_is_failed() {
        let outcome = exhausted(
            "origin 0: HTTP 404 for https://example.invalid/manifest; \
             origin 1: transport failure: dns error",
        );
        assert_eq!(outcome.kind, CheckKind::Failed);
    }

    #[test]
    fn unparseable_exhausted_string_is_failed() {
        let outcome = exhausted("something else entirely");
        assert_eq!(outcome.kind, CheckKind::Failed);
    }

    #[test]
    fn payload_phase_variants_classify() {
        let whole = classify_fetch_error(FetchError::WholeFileMismatch);
        assert_eq!(whole.kind, CheckKind::PayloadMismatch);
        assert_eq!(whole.notice, "Payload hash mismatch: the payload archive");

        let member = classify_fetch_error(FetchError::MemberMismatch {
            path: "udm.exe".to_owned(),
        });
        assert_eq!(member.kind, CheckKind::PayloadMismatch);
        assert_eq!(member.notice, "Payload hash mismatch: udm.exe");

        let extra = classify_fetch_error(FetchError::ExtraMember("evil.exe".to_owned()));
        assert_eq!(extra.kind, CheckKind::ManifestRefused);
        assert!(extra
            .notice
            .starts_with("Manifest refused: archive file evil.exe"));

        let cancelled = classify_fetch_error(FetchError::Cancelled);
        assert_eq!(cancelled.kind, CheckKind::Failed);
    }

    #[test]
    fn flag_round_trip_on_a_scratch_layout() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        // Absent resolves ON, the default.
        assert!(checks_enabled(&layout));
        set_checks_enabled(&layout, true).unwrap();
        assert!(checks_enabled(&layout));
        assert_eq!(std::fs::read_to_string(flag_path(&layout)).unwrap(), "on\n");
        set_checks_enabled(&layout, false).unwrap();
        assert!(!checks_enabled(&layout));
        // Off is durable content, not deletion.
        assert_eq!(
            std::fs::read_to_string(flag_path(&layout)).unwrap(),
            "off\n"
        );
        // Off twice is not an error.
        set_checks_enabled(&layout, false).unwrap();
        assert!(!checks_enabled(&layout));
        set_checks_enabled(&layout, true).unwrap();
        assert!(checks_enabled(&layout));
    }

    #[test]
    fn flag_matrix_resolved_state() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let path = flag_path(&layout);

        // Absent: ON.
        assert!(!path.exists());
        assert!(checks_enabled(&layout));

        for (content, expected) in [
            ("on\n", true),
            ("on", true),
            ("off\n", false),
            ("off", false),
            (" \toff \n", false),
            ("", true),
            ("OFF\n", true),
            ("off off", true),
            ("anything else", true),
        ] {
            std::fs::write(&path, content).unwrap();
            assert_eq!(
                checks_enabled(&layout),
                expected,
                "content {content:?} should resolve {}",
                if expected { "ON" } else { "OFF" }
            );
        }

        // Deleting the file resurrects the default ON.
        std::fs::remove_file(&path).unwrap();
        assert!(checks_enabled(&layout));
    }
}

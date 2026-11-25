//! Updates the install in place from the project's releases.
//!
//! Every release is published on GitHub and on GitLab. A check fetches
//! `version.json` from both hosts at once and reads `versionName` from
//! the `latestWinVersion` object. The numerically newest acceptable
//! version wins; a version below [`MIN_ACCEPTED_VERSION`] is discarded
//! for that host, and the other host is still tried. A check stores
//! nothing: it is a pure read, so checking again always gives the same
//! answer. A published version that is not newer than the running one
//! is "already current". Valid versions that are all below the floor
//! are "no acceptable update", not a failure.
//!
//! Nothing is installed without asking. When the user applies, the
//! portable zip and its `.sha256` sidecar are downloaded from the host
//! the check selected. The other host is tried only when that pair is
//! incomplete. Once both files have been fetched, a checksum, archive,
//! signature, or install failure stops the job. The gate compares
//! `sha256(zip)` with the first 64 sidecar bytes before anything is
//! extracted, then verifies the Authenticode signature of every
//! executable with
//! `WinVerifyTrust` and compares the signer certificate's SPKI hash
//! (SHA-256 over the full DER encoding of the SubjectPublicKeyInfo)
//! against the pinned key below. The signing certificate is
//! self-issued, so chain-to-public-root trust is not the anchor; the
//! anchor is a cryptographically valid signature plus the SPKI pin,
//! with a mandatory RFC 3161 timestamp (validity is judged at signing
//! time, so a countersigned binary stays acceptable after the signing
//! certificate expires). Anything else fails closed. A tampered zip
//! served with recomputed sums still faces the signature check.
//!
//! Each executable is written under a fresh staging name and that
//! handle stays open through signature verification and a handle-based
//! rename into place, so the file checked is the file installed. The
//! current file steps aside to a `.old` sibling first. Any failure
//! rolls the swapped files back and says so when that rollback itself
//! fails. `.old` files, inactive staging directories, and stale Firefox
//! archives are deleted on the next start ([`finish_previous_update`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

use crate::VERSION;

/// A message-carrying error for the transport side of the updater (the
/// install gate has its own [`Failure`] type).
#[derive(Debug, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    /// Build an error from a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

type Result<T> = std::result::Result<T, Error>;

/// A site the releases are published on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// github.com/iamcino/udm
    GitHub,
    /// gitlab.com/cino-group/udm
    GitLab,
}

impl Source {
    /// Raw `version.json` on this host's default branch.
    fn version_json_url(self) -> &'static str {
        match self {
            Source::GitHub => "https://raw.githubusercontent.com/iamcino/udm/master/version.json",
            Source::GitLab => "https://gitlab.com/cino-group/udm/-/raw/main/version.json",
        }
    }

    /// Where the files of `version` are; the file name goes after it.
    fn download_base(self, version: &str) -> String {
        match self {
            Source::GitHub => {
                format!("https://github.com/iamcino/udm/releases/download/v{version}/")
            }
            Source::GitLab => {
                format!("https://gitlab.com/cino-group/udm/-/releases/v{version}/downloads/")
            }
        }
    }
}

/// The site the last check found the newest version on; tried first for
/// the download.
static PREFERRED: AtomicU8 = AtomicU8::new(0);

fn preferred() -> Source {
    if PREFERRED.load(Ordering::Relaxed) == 1 {
        Source::GitLab
    } else {
        Source::GitHub
    }
}

fn set_preferred(source: Source) {
    PREFERRED.store(
        if source == Source::GitLab { 1 } else { 0 },
        Ordering::Relaxed,
    );
}

/// Both sites, the preferred one first.
fn sources() -> [Source; 2] {
    match preferred() {
        Source::GitHub => [Source::GitHub, Source::GitLab],
        Source::GitLab => [Source::GitLab, Source::GitHub],
    }
}

/// The release asset carrying the whole install for `version`.
fn zip_name(version: &str) -> String {
    format!("udm-{version}-portable.zip")
}

/// Executables inside the portable zip whose signatures gate the
/// install: the code that will run from this install.
const PAYLOAD_EXES: [&str; 3] = ["udm.exe", "udm-cli.exe", "udm-host.exe"];

/// Largest update payload a download may be.
const MAX_DOWNLOAD: usize = 128 * 1024 * 1024;

/// Parse a `v`-optional three-part version.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.trim();
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut parts = s.split('.');
    let mut next = || {
        parts
            .next()
            .filter(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))?
            .parse()
            .ok()
    };
    let v = (next()?, next()?, next()?);
    parts.next().is_none().then_some(v)
}

/// The version of this build.
pub fn current() -> &'static str {
    VERSION
}

/// Compiled floor. A host offering a lower version is skipped; the
/// other host is still asked. Not ordering, rollback protection, or
/// persisted client state.
pub const MIN_ACCEPTED_VERSION: &str = "0.3.2";

/// EasyKey version name: exactly three decimal components, no leading
/// `v`, no surrounding whitespace, at most 16 characters. Major fits in
/// 16 bits, minor and patch in 8. The packed value is
/// `major << 16 | minor << 8 | patch`.
pub fn easykey_version_number(version: &str) -> Option<u32> {
    if version.is_empty() || version.len() > 16 {
        return None;
    }
    let mut parts = [0u32; 3];
    let mut part = 0usize;
    let mut saw_digit = false;
    for b in version.bytes() {
        if b.is_ascii_digit() {
            saw_digit = true;
            let next = parts[part]
                .checked_mul(10)?
                .checked_add((b - b'0') as u32)?;
            if next > 65535 {
                return None;
            }
            parts[part] = next;
        } else if b == b'.' && saw_digit && part < 2 {
            part += 1;
            saw_digit = false;
        } else {
            return None;
        }
    }
    if part != 2 || !saw_digit || parts[1] > 255 || parts[2] > 255 {
        return None;
    }
    Some((parts[0] << 16) | (parts[1] << 8) | parts[2])
}

fn min_accepted_number() -> u32 {
    easykey_version_number(MIN_ACCEPTED_VERSION).expect("MIN_ACCEPTED_VERSION is a valid version")
}

/// Whether `latest` is a newer EasyKey version than this build.
/// Anything that is not an EasyKey version name is not newer.
pub fn is_newer(latest: &str) -> bool {
    match (
        easykey_version_number(latest),
        easykey_version_number(current()),
    ) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// `versionName` inside the `latestWinVersion` object only. A
/// `versionName` in an earlier or later sibling object is not it.
fn parse_version_json(text: &str) -> std::result::Result<String, String> {
    let Some(object) = json_object_after_key(text, "latestWinVersion") else {
        return Err("version.json has no latestWinVersion object".into());
    };
    let Some(name) = json_string_field(&object, "versionName") else {
        return Err("latestWinVersion has no versionName".into());
    };
    if easykey_version_number(&name).is_none() {
        return Err(format!("invalid version name: {name}"));
    }
    Ok(name)
}

struct JsonScan<'a> {
    bytes: &'a [u8],
    i: usize,
}

fn skip_ws(scan: &mut JsonScan<'_>) {
    while scan.bytes.get(scan.i).is_some_and(u8::is_ascii_whitespace) {
        scan.i += 1;
    }
}

fn parse_json_string(scan: &mut JsonScan<'_>) -> Option<String> {
    if scan.bytes.get(scan.i) != Some(&b'"') {
        return None;
    }
    scan.i += 1;
    let mut out = String::new();
    while scan.i < scan.bytes.len() {
        let b = scan.bytes[scan.i];
        scan.i += 1;
        match b {
            b'"' => return Some(out),
            b'\\' => {
                let escaped = *scan.bytes.get(scan.i)?;
                scan.i += 1;
                match escaped {
                    b'"' | b'\\' | b'/' => out.push(escaped as char),
                    b'n' => out.push('\n'),
                    b't' => out.push('\t'),
                    b'r' => out.push('\r'),
                    _ => return None,
                }
            }
            byte => out.push(byte as char),
        }
    }
    None
}

fn skip_container(scan: &mut JsonScan<'_>, open: u8, close: u8) -> bool {
    if scan.bytes.get(scan.i) != Some(&open) {
        return false;
    }
    scan.i += 1;
    let mut depth = 1i32;
    while scan.i < scan.bytes.len() && depth > 0 {
        match scan.bytes[scan.i] {
            b'"' => {
                if parse_json_string(scan).is_none() {
                    return false;
                }
            }
            byte if byte == open => {
                depth += 1;
                scan.i += 1;
            }
            byte if byte == close => {
                depth -= 1;
                scan.i += 1;
            }
            _ => scan.i += 1,
        }
    }
    depth == 0
}

fn skip_json_value(scan: &mut JsonScan<'_>) -> bool {
    skip_ws(scan);
    match scan.bytes.get(scan.i).copied() {
        Some(b'{') => skip_container(scan, b'{', b'}'),
        Some(b'[') => skip_container(scan, b'[', b']'),
        Some(b'"') => parse_json_string(scan).is_some(),
        Some(b) if b == b'-' || b.is_ascii_digit() => {
            scan.i += 1;
            while scan.bytes.get(scan.i).is_some_and(|c| {
                c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E' | b'+' | b'-')
            }) {
                scan.i += 1;
            }
            true
        }
        Some(b'f' | b't' | b'n') => {
            while scan.bytes.get(scan.i).is_some_and(u8::is_ascii_alphabetic) {
                scan.i += 1;
            }
            true
        }
        _ => false,
    }
}

/// The object value of the first `"key": { ... }` in `text`.
fn json_object_after_key(text: &str, key: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut scan = JsonScan { bytes, i: 0 };
    while scan.i < bytes.len() {
        if bytes[scan.i] != b'"' {
            scan.i += 1;
            continue;
        }
        let value = parse_json_string(&mut scan)?;
        skip_ws(&mut scan);
        if value == key && bytes.get(scan.i) == Some(&b':') {
            scan.i += 1;
            skip_ws(&mut scan);
            if bytes.get(scan.i) != Some(&b'{') {
                continue;
            }
            let start = scan.i;
            if !skip_json_value(&mut scan) {
                return None;
            }
            return Some(text[start..scan.i].to_owned());
        }
    }
    None
}

/// The first string value of `"key"` inside `object` (the object text,
/// including its braces).
fn json_string_field(object: &str, key: &str) -> Option<String> {
    let bytes = object.as_bytes();
    let mut scan = JsonScan { bytes, i: 0 };
    while scan.i < bytes.len() {
        if bytes[scan.i] != b'"' {
            scan.i += 1;
            continue;
        }
        let value = parse_json_string(&mut scan)?;
        skip_ws(&mut scan);
        if value == key && bytes.get(scan.i) == Some(&b':') {
            scan.i += 1;
            skip_ws(&mut scan);
            if bytes.get(scan.i) != Some(&b'"') {
                return None;
            }
            return parse_json_string(&mut scan);
        }
    }
    None
}

/// The digest named by a sidecar: exactly the first 64 bytes, read as
/// hexadecimal, case-insensitively. No trimming and no skipped prefix.
/// Bytes after those 64 are ignored, so a `sha256sum` line is accepted.
fn sidecar_hex(bytes: &[u8]) -> Option<String> {
    let head = bytes.get(..64)?;
    if !head.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    std::str::from_utf8(head)
        .ok()
        .map(|text| text.to_ascii_lowercase())
}

fn sha256_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// What one host's `version.json` produced. `Ok` is a valid EasyKey
/// version name, which may still be below the floor.
struct HostRead {
    source: Source,
    outcome: std::result::Result<String, String>,
}

/// The combined read of both hosts.
#[derive(Debug)]
enum Discovery {
    /// Numerically newest acceptable version, and the host it was taken
    /// from (the preferred-source snapshot on a tie).
    Offer { version: String, source: Source },
    /// At least one valid version, and every valid version is below the floor.
    NoAcceptable { detail: String },
    /// Neither host produced a valid version.
    Failed { detail: String },
}

/// What [`latest`] returns to a caller that bypasses the checks flag.
#[derive(Debug, PartialEq, Eq)]
pub enum Latest {
    /// An acceptable version (at or above the floor).
    Version(String),
    /// Valid versions were read and all of them are below the floor.
    NoAcceptable,
}

fn host_label(source: Source) -> &'static str {
    match source {
        Source::GitHub => "GitHub",
        Source::GitLab => "GitLab",
    }
}

fn read_host(source: Source, fetch: &dyn Fn(&str, usize) -> Result<Vec<u8>>) -> HostRead {
    let body = match fetch(source.version_json_url(), 64 * 1024) {
        Ok(body) => body,
        Err(err) => {
            return HostRead {
                source,
                outcome: Err(err.to_string()),
            };
        }
    };
    let text = String::from_utf8_lossy(&body);
    HostRead {
        source,
        outcome: parse_version_json(&text),
    }
}

/// Select the newest acceptable version. Equal versions take `snapshot`'s
/// source. Below-floor versions are valid reads that are not offers.
fn combine(snapshot: Source, reports: [HostRead; 2]) -> Discovery {
    let mut acceptable: Vec<(u32, String, Source)> = Vec::new();
    let mut any_valid = false;
    let mut reasons = Vec::new();
    for report in reports {
        match report.outcome {
            Ok(version) => {
                any_valid = true;
                let number = easykey_version_number(&version).expect("parse accepted it");
                if number >= min_accepted_number() {
                    acceptable.push((number, version, report.source));
                } else {
                    reasons.push(format!(
                        "{}: {version} is below {MIN_ACCEPTED_VERSION}",
                        host_label(report.source)
                    ));
                }
            }
            Err(reason) => reasons.push(format!("{}: {reason}", host_label(report.source))),
        }
    }
    let Some(best) = acceptable.iter().map(|(n, _, _)| *n).max() else {
        let detail = reasons.join("; ");
        return if any_valid {
            Discovery::NoAcceptable { detail }
        } else {
            Discovery::Failed { detail }
        };
    };
    let tied: Vec<_> = acceptable
        .into_iter()
        .filter(|(number, _, _)| *number == best)
        .collect();
    let (_, version, source) = tied
        .iter()
        .find(|(_, _, source)| *source == snapshot)
        .cloned()
        .unwrap_or_else(|| tied[0].clone());
    Discovery::Offer { version, source }
}

type Fetch<'a> = dyn Fn(&str, usize) -> Result<Vec<u8>> + Sync + 'a;

/// Fetch both hosts' `version.json` at once, then combine. Takes one
/// preferred-source snapshot before the results are combined, and sets
/// PREFERRED only when an acceptable offer wins.
fn discover(fetch: &Fetch<'_>) -> Discovery {
    let snapshot = preferred();
    let reports = std::thread::scope(|scope| {
        let github = scope.spawn(|| read_host(Source::GitHub, fetch));
        let gitlab = scope.spawn(|| read_host(Source::GitLab, fetch));
        let github = github.join().unwrap_or_else(|_| HostRead {
            source: Source::GitHub,
            outcome: Err("update check failed".into()),
        });
        let gitlab = gitlab.join().unwrap_or_else(|_| HostRead {
            source: Source::GitLab,
            outcome: Err("update check failed".into()),
        });
        [github, gitlab]
    });
    let discovery = combine(snapshot, reports);
    if let Discovery::Offer { source, .. } = &discovery {
        set_preferred(*source);
    }
    discovery
}

fn production_fetch(url: &str, max: usize) -> Result<Vec<u8>> {
    http::get(url, max)
}

/// Asks GitHub and GitLab for `version.json`, at the same time.
/// Blocking; run it off the UI thread. Does not consult the checks
/// flag and does not write the check log.
pub fn latest() -> Result<Latest> {
    match discover(&production_fetch) {
        Discovery::Offer { version, .. } => Ok(Latest::Version(version)),
        Discovery::NoAcceptable { .. } => Ok(Latest::NoAcceptable),
        Discovery::Failed { detail } => Err(Error::new(detail)),
    }
}

/// What an explicit apply should do with a discovery result. A missing
/// acceptable offer is a no-op; it is not turned into a version string.
#[derive(Debug, PartialEq, Eq)]
pub enum ApplyChoice {
    /// Download and install this acceptable version.
    Install(String),
    /// The acceptable version is not newer than this build.
    AlreadyCurrent,
    /// Valid versions were read and all of them are below the floor.
    NoAcceptable,
}

pub fn apply_choice(found: &Latest) -> ApplyChoice {
    match found {
        Latest::NoAcceptable => ApplyChoice::NoAcceptable,
        Latest::Version(version) if is_newer(version) => ApplyChoice::Install(version.clone()),
        Latest::Version(_) => ApplyChoice::AlreadyCurrent,
    }
}

// ---------------------------------------------------------------------------
// Install layout, checks flag, check outcome
// ---------------------------------------------------------------------------

/// Prefix of one apply's staging directory. The rest of the name is an
/// unpredictable hex suffix created for that attempt, so a later apply
/// cannot reuse a directory a previous attempt still has open.
pub const STAGING_DIR_PREFIX: &str = "update-staging-";

/// How many times startup cleanup retries a locked `.old` file, and the
/// pause between those tries. A file still locked after this is kept
/// for the next start.
pub const CLEANUP_ATTEMPTS: u32 = 120;
pub const CLEANUP_DELAY_MS: u64 = 500;

/// File on the install base whose content resolves the checks state:
/// absent means ON (the default), trimmed content `off` means OFF (a
/// durable opt-out), any other content means ON. It is not a `Settings`
/// field: the settings Save pipeline never writes it.
pub const CHECKS_FLAG_FILE: &str = "update-checks-enabled";

/// Local log each check outcome appends one line to (every outcome
/// produces a local log entry; nothing leaves the machine).
pub const CHECK_LOG_FILE: &str = "update-check.log";

/// The install tree the updater works on: the directory the running
/// binaries live in.
#[derive(Clone, Debug)]
pub struct InstallLayout {
    base: PathBuf,
}

impl InstallLayout {
    /// A layout rooted at the install directory.
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self { base: base.into() }
    }

    /// The install directory.
    pub fn base(&self) -> &Path {
        &self.base
    }

    fn flag_path(&self) -> PathBuf {
        self.base.join(CHECKS_FLAG_FILE)
    }

    fn check_log_path(&self) -> PathBuf {
        self.base.join(CHECK_LOG_FILE)
    }
}

/// The resolved checks state for this install: the flag file is absent
/// (ON, the default), or present with trimmed content `off` (OFF, a
/// durable opt-out), or present with any other content (ON).
pub fn checks_enabled(layout: &InstallLayout) -> bool {
    match std::fs::read_to_string(layout.flag_path()) {
        Ok(content) => content.trim() != "off",
        Err(_) => true,
    }
}

/// Write the checks flag. `on` writes `on\n`; `off` writes `off\n`. The
/// opt-out is durable content, not deletion: removing the file would
/// resolve ON again at the next read. Opens no socket either way.
pub fn set_checks_enabled(layout: &InstallLayout, enabled: bool) -> std::io::Result<()> {
    let path = layout.flag_path();
    if enabled {
        std::fs::write(&path, b"on\n")
    } else {
        std::fs::write(&path, b"off\n")
    }
}

/// Which outcome a check produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckKind {
    /// Checks are disabled: nothing was fetched, no socket was opened.
    Disabled,
    /// A newer build is published.
    Available,
    /// The published version does not exceed the installed one.
    AlreadyCurrent,
    /// At least one host returned a valid version, and every valid
    /// version is below [`MIN_ACCEPTED_VERSION`]. Not a failure.
    NoAcceptableUpdate,
    /// Neither host returned a valid version: transport, HTTP, or a
    /// document that does not carry one.
    Failed,
}

/// The result of one check: its kind, the full user-facing notice text,
/// and the offered version when the kind is
/// [`CheckKind::Available`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckOutcome {
    /// Outcome classification.
    pub kind: CheckKind,
    /// User-facing text.
    pub notice: String,
    /// The published version, when one was read and offered.
    pub version: Option<String>,
    /// Per-host rejection or failure reasons for the local log. Empty
    /// when the notice's first line already carries them, or when there
    /// is nothing extra to record. Not user-facing error text.
    pub detail: String,
}

fn failed(reason: String) -> CheckOutcome {
    CheckOutcome {
        kind: CheckKind::Failed,
        notice: format!("Update check failed: {reason}"),
        version: None,
        detail: String::new(),
    }
}

/// The outcome for a read of the latest published version, against the
/// running one. Pure, so the classification is testable off the network.
fn outcome_for(latest: &str) -> CheckOutcome {
    if is_newer(latest) {
        CheckOutcome {
            kind: CheckKind::Available,
            notice: format!("Update available: version {latest}"),
            version: Some(latest.to_owned()),
            detail: String::new(),
        }
    } else {
        CheckOutcome {
            kind: CheckKind::AlreadyCurrent,
            notice: format!("Already current: version {VERSION} is the latest published version."),
            version: Some(latest.to_owned()),
            detail: String::new(),
        }
    }
}

/// Append the local log line every outcome writes (best effort: a log
/// failure never changes the outcome).
fn log_check(layout: &InstallLayout, now_unix: i64, outcome: &CheckOutcome) {
    use std::io::Write as _;
    let headline = outcome.notice.lines().next().unwrap_or("");
    let line = if outcome.detail.is_empty() {
        format!("{now_unix} check kind={:?} {headline}\n", outcome.kind)
    } else {
        format!(
            "{now_unix} check kind={:?} {headline} | {}\n",
            outcome.kind, outcome.detail
        )
    };
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(layout.check_log_path())
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Run one update check for `layout` through `fetch`. While checks
/// resolve OFF no check starts: `fetch` is not called and the outcome
/// is [`CheckKind::Disabled`].
fn run_check_with(layout: &InstallLayout, now_unix: i64, fetch: &Fetch<'_>) -> CheckOutcome {
    let outcome = if !checks_enabled(layout) {
        CheckOutcome {
            kind: CheckKind::Disabled,
            notice: "Update checks are disabled.".to_owned(),
            version: None,
            detail: String::new(),
        }
    } else {
        match discover(fetch) {
            Discovery::Offer { version, .. } => outcome_for(&version),
            Discovery::NoAcceptable { detail } => CheckOutcome {
                kind: CheckKind::NoAcceptableUpdate,
                notice: "No acceptable update is available.".to_owned(),
                version: None,
                detail,
            },
            Discovery::Failed { detail } => failed(detail),
        }
    };
    log_check(layout, now_unix, &outcome);
    outcome
}

/// Run one update check for `layout`. While checks resolve OFF no check
/// starts: no socket opens and the outcome is [`CheckKind::Disabled`].
/// A check already in flight when the user turns checks off runs to
/// completion. Nothing here downloads or installs; applying stays a
/// separate, explicit act. Blocking; run it off the UI thread.
pub fn run_check(layout: &InstallLayout, now_unix: i64) -> CheckOutcome {
    run_check_with(layout, now_unix, &production_fetch)
}

// ---------------------------------------------------------------------------
// Download, acceptance gate, swap
// ---------------------------------------------------------------------------

/// Why an update did not go through, each with its own advice.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Failure {
    /// A release file could not be fetched.
    #[error("download failed: {0}")]
    Download(Error),
    /// The archive's hash does not match its sidecar, the sidecar is
    /// not 64 hex bytes, the archive is not a usable zip, a member path
    /// is unsafe, or a payload executable is missing. Kept distinct from
    /// [`Failure::Signature`]: a checksum mismatch means a broken or
    /// substituted download, a signature failure means the file must not
    /// be trusted even though its hash matches the sidecar.
    #[error("the downloaded archive does not match the published checksums")]
    Mismatch,
    /// The Authenticode/SPKI gate refused a payload executable:
    /// unsigned, untrusted signature, SPKI mismatch, or a verification
    /// API error.
    #[error("the downloaded build is not signed with the pinned key")]
    Signature,
    /// The verified build could not be put in place. `restored` is
    /// false when putting the previous files back also failed.
    #[error("install failed: {detail}")]
    Install { detail: String, restored: bool },
}

impl Failure {
    /// The user-facing sentence for this failure, with its advice.
    pub fn message(&self) -> String {
        match self {
            Failure::Download(e) => format!(
                "The update could not be downloaded ({e}). Check the connection and try again."
            ),
            Failure::Mismatch => {
                "The downloaded archive does not match the published checksums; it was discarded. Try again, and prefer the other release site if it repeats."
                    .to_owned()
            }
            Failure::Signature => {
                "The downloaded build is not signed with the pinned key; it was discarded and nothing was installed."
                    .to_owned()
            }
            Failure::Install { detail, restored } => {
                if *restored {
                    format!(
                        "The update could not be installed ({detail}); the previous build is still in place."
                    )
                } else {
                    format!(
                        "The update could not be installed ({detail}); restoring the previous build failed."
                    )
                }
            }
        }
    }
}

/// The pinned code-signing key: SHA-256 over the full DER encoding of
/// the SubjectPublicKeyInfo of the UDM signing certificate (EC P-256;
/// the DER SPKI is 91 bytes). This pins the *key*, not the
/// certificate: a renewed certificate on the same key still matches.
///
/// Derived from two independent sources that agreed exactly
/// (2026-11-18): the published `UDM-codesign.cer` asset of release
/// v0.2.3, and the signer certificate embedded in the shipped signed
/// `udm.exe` of the same release (thumbprint
/// `9FEE47A048EB37ABD97751E89DD750F75A38C5D4`; the two certificate
/// files are byte-identical).
pub const PINNED_SPKI_SHA256_HEX: &str =
    "a4019dd8e3687adfa5658233c98f2f87182b3fa6350121925215337179dc1a87";

/// [`PINNED_SPKI_SHA256_HEX`] decoded, for constant-time comparison.
const PINNED_SPKI_SHA256: [u8; 32] = decode_hex32(PINNED_SPKI_SHA256_HEX);

const fn decode_hex32(s: &str) -> [u8; 32] {
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        let hi = hex_val(bytes[2 * i]);
        let lo = hex_val(bytes[2 * i + 1]);
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    out
}

const fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

/// Whether `spki_der` (full DER encoding of a SubjectPublicKeyInfo) is
/// the pinned key. The comparison is on the SPKI hash only:
/// certificate bytes, subject and validity do not enter into it.
fn spki_pin_matches(spki_der: &[u8]) -> bool {
    let digest = ring::digest::digest(&ring::digest::SHA256, spki_der);
    let bytes = digest.as_ref();
    if bytes.len() != PINNED_SPKI_SHA256.len() {
        return false;
    }
    // Fixed-length compare with no early exit.
    let mut diff = 0u8;
    for i in 0..PINNED_SPKI_SHA256.len() {
        diff |= bytes[i] ^ PINNED_SPKI_SHA256[i];
    }
    diff == 0
}

fn asset_url(source: Source, version: &str, name: &str) -> String {
    format!("{}{name}", source.download_base(version))
}

fn sidecar_name(version: &str) -> String {
    format!("{}.sha256", zip_name(version))
}

/// Both files of one host, or a download error if either is missing.
/// A later gate failure is not this function's concern.
fn fetch_pair(
    source: Source,
    version: &str,
    fetch: &Fetch<'_>,
) -> std::result::Result<(Vec<u8>, Vec<u8>), Failure> {
    let zip = fetch(
        &asset_url(source, version, &zip_name(version)),
        MAX_DOWNLOAD,
    )
    .map_err(Failure::Download)?;
    let sidecar = fetch(
        &asset_url(source, version, &sidecar_name(version)),
        64 * 1024,
    )
    .map_err(Failure::Download)?;
    Ok((zip, sidecar))
}

/// The acceptance gate, identical for both origins: the sidecar's first
/// 64 bytes must be the zip's SHA-256, then the archive is extracted
/// and every payload executable is handed to `verify` (the Authenticode
/// + SPKI step). `verify` is only called when the hash already matches,
/// so a tampered zip served with a recomputed sidecar still faces the
/// signature check.
///
/// `verify` is a parameter (rather than a direct call) so tests can
/// drive the gate with fixture outcomes; production passes
/// [`http::verify_file_signature`]. On success the staging directory
/// holds the verified tree.
fn firefox_archive_name(version: &str) -> String {
    format!("extension/udm-extension-firefox-{version}.zip")
}

/// A direct `extension/udm-extension-firefox-*.zip` member, not a nested path.
fn is_firefox_archive(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("extension/udm-extension-firefox-") else {
        return false;
    };
    let Some(version) = rest.strip_suffix(".zip") else {
        return false;
    };
    !version.is_empty() && !version.contains('/')
}

/// Three root executables, one regular root `udm.portable`, and exactly
/// one Firefox archive named for `version`. Checked before anything is
/// written, and before the swap.
fn require_payload(members: &[ZipMember], version: &str) -> std::result::Result<(), Failure> {
    let names: Vec<&str> = members.iter().map(|member| member.name.as_str()).collect();
    if names.iter().filter(|name| **name == "udm.portable").count() != 1 {
        return Err(Failure::Mismatch);
    }
    let expected = firefox_archive_name(version);
    let firefox: Vec<&str> = names
        .iter()
        .copied()
        .filter(|name| is_firefox_archive(name))
        .collect();
    if firefox.len() != 1 || firefox[0] != expected {
        return Err(Failure::Mismatch);
    }
    for exe in PAYLOAD_EXES {
        if !names.contains(&exe) {
            return Err(Failure::Mismatch);
        }
    }
    Ok(())
}

fn install_err(detail: impl Into<String>, restored: bool) -> Failure {
    Failure::Install {
        detail: detail.into(),
        restored,
    }
}

fn active_staging() -> &'static Mutex<Vec<PathBuf>> {
    static ACTIVE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    &ACTIVE
}

/// Same-process apply and cleanup share this lock. Cleanup cannot
/// delete a tree an apply still has open, and it is not held across a
/// verify callback.
fn update_lock() -> &'static Mutex<()> {
    static LOCK: Mutex<()> = Mutex::new(());
    &LOCK
}

fn forget_active(path: &Path) {
    active_staging()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .retain(|active| active != path);
}

fn release_staging(path: &Path) {
    forget_active(path);
    let _ = std::fs::remove_dir_all(path);
}

fn is_staging_name(name: &std::ffi::OsStr) -> bool {
    let Some(text) = name.to_str() else {
        return false;
    };
    let Some(rest) = text.strip_prefix(STAGING_DIR_PREFIX) else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
fn staging_present(base: &Path) -> bool {
    std::fs::read_dir(base)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| is_staging_name(&entry.file_name()))
}

/// A directory this apply created and still owns. The name is not
/// reusable: exclusive `create_dir`, and cleanup skips paths in the
/// active set.
fn create_staging(base: &Path) -> std::io::Result<PathBuf> {
    let _guard = update_lock().lock().unwrap_or_else(|err| err.into_inner());
    let rng = ring::rand::SystemRandom::new();
    for _ in 0..8 {
        let mut bytes = [0u8; 16];
        ring::rand::SecureRandom::fill(&rng, &mut bytes)
            .map_err(|_| std::io::Error::other("rng"))?;
        let name: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let path = base.join(format!("{STAGING_DIR_PREFIX}{name}"));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                active_staging()
                    .lock()
                    .unwrap_or_else(|err| err.into_inner())
                    .push(path.clone());
                return Ok(path);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(std::io::Error::other(
        "could not create a staging directory",
    ))
}

/// One payload executable, kept open from the write through the rename.
#[derive(Debug)]
struct HeldExe {
    path: PathBuf,
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(windows)]
impl Drop for HeldExe {
    fn drop(&mut self) {
        winfile::close(self.handle);
    }
}

#[derive(Debug)]
struct StagedTree {
    dir: PathBuf,
    exes: Vec<HeldExe>,
}

impl StagedTree {
    #[cfg(test)]
    fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for StagedTree {
    fn drop(&mut self) {
        self.exes.clear();
        release_staging(&self.dir);
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
mod winfile {
    use super::{HeldExe, Path};
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FileRenameInfo, FlushFileBuffers, SetFileInformationByHandle,
        SetFilePointerEx, WriteFile, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_BEGIN,
        FILE_RENAME_INFO, FILE_SHARE_READ,
    };
    #[cfg(test)]
    use windows_sys::Win32::Storage::FileSystem::{
        DeleteFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_SHARE_DELETE,
        FILE_SHARE_WRITE, OPEN_EXISTING,
    };

    const DELETE_ACCESS: u32 = 0x0001_0000;

    fn wide_path(path: &Path) -> Vec<u16> {
        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        wide.push(0);
        wide
    }

    pub(super) fn close(handle: HANDLE) {
        unsafe {
            CloseHandle(handle);
        }
    }

    pub(super) fn write_held(path: &Path, bytes: &[u8]) -> std::io::Result<HeldExe> {
        let wide = wide_path(path);
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE | DELETE_ACCESS,
                FILE_SHARE_READ,
                std::ptr::null(),
                CREATE_NEW,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let mut offset = 0usize;
        while offset < bytes.len() {
            let chunk = (bytes.len() - offset).min(1 << 20);
            let mut written = 0u32;
            let ok = unsafe {
                WriteFile(
                    handle,
                    bytes[offset..].as_ptr(),
                    chunk as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };
            if ok == 0 || written == 0 {
                close(handle);
                return Err(std::io::Error::last_os_error());
            }
            offset += written as usize;
        }
        let rewound = unsafe { SetFilePointerEx(handle, 0, std::ptr::null_mut(), FILE_BEGIN) };
        if rewound == 0 || unsafe { FlushFileBuffers(handle) } == 0 {
            close(handle);
            return Err(std::io::Error::last_os_error());
        }
        Ok(HeldExe {
            path: path.to_path_buf(),
            handle,
        })
    }

    pub(super) fn rename_by_handle(handle: HANDLE, target: &Path) -> std::io::Result<()> {
        let absolute = std::path::absolute(target)?;
        let wide = wide_path(&absolute);
        let name_bytes = (wide.len() - 1) * std::mem::size_of::<u16>();
        let size = std::mem::size_of::<FILE_RENAME_INFO>()
            + wide.len().saturating_sub(1) * std::mem::size_of::<u16>();
        let mut raw = vec![0u64; size.div_ceil(8)];
        unsafe {
            let info = raw.as_mut_ptr() as *mut FILE_RENAME_INFO;
            (*info).Anonymous.ReplaceIfExists = 0;
            (*info).RootDirectory = 0;
            (*info).FileNameLength = name_bytes as u32;
            std::ptr::copy_nonoverlapping(wide.as_ptr(), (*info).FileName.as_mut_ptr(), wide.len());
            let ok = SetFileInformationByHandle(
                handle,
                FileRenameInfo,
                raw.as_ptr() as *const _,
                size as u32,
            );
            if ok == 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn file_index(handle: HANDLE) -> std::io::Result<(u32, u32)> {
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        let ok = unsafe { GetFileInformationByHandle(handle, &mut info) };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok((info.nFileIndexHigh, info.nFileIndexLow))
    }

    #[cfg(test)]
    pub(super) fn indexes_match(handle: HANDLE, path: &Path) -> std::io::Result<bool> {
        let held = file_index(handle)?;
        let units = wide_path(path);
        let opened = unsafe {
            CreateFileW(
                units.as_ptr(),
                GENERIC_READ,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };
        if opened == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        let opened_index = file_index(opened);
        close(opened);
        Ok(opened_index? == held)
    }

    #[cfg(test)]
    pub(super) fn write_denied(path: &Path) -> bool {
        let units = wide_path(path);
        let handle = unsafe {
            CreateFileW(
                units.as_ptr(),
                GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return true;
        }
        close(handle);
        false
    }

    #[cfg(test)]
    pub(super) fn delete_denied(path: &Path) -> bool {
        let units = wide_path(path);
        unsafe { DeleteFileW(units.as_ptr()) == 0 }
    }

    /// Read handle with no sharing, so a cleanup delete has to wait.
    #[cfg(test)]
    pub(super) struct Lock {
        handle: HANDLE,
    }

    #[cfg(test)]
    impl Drop for Lock {
        fn drop(&mut self) {
            close(self.handle);
        }
    }

    #[cfg(test)]
    pub(super) fn lock_exclusive(path: &Path) -> std::io::Result<Lock> {
        let units = wide_path(path);
        let handle = unsafe {
            CreateFileW(
                units.as_ptr(),
                GENERIC_READ,
                0,
                std::ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Lock { handle })
    }
}

#[cfg(test)]
fn accept_and_stage(
    zip: &[u8],
    sidecar: &[u8],
    version: &str,
    layout: &InstallLayout,
    verify: &dyn Fn(&Path) -> std::result::Result<(), Failure>,
) -> std::result::Result<StagedTree, Failure> {
    stage(zip, sidecar, version, layout, verify, false)
}

fn stage(
    zip: &[u8],
    sidecar: &[u8],
    version: &str,
    layout: &InstallLayout,
    verify: &dyn Fn(&Path) -> std::result::Result<(), Failure>,
    spki: bool,
) -> std::result::Result<StagedTree, Failure> {
    let expected = sidecar_hex(sidecar).ok_or(Failure::Mismatch)?;
    if sha256_hex(zip) != expected {
        return Err(Failure::Mismatch);
    }
    let members = read_zip(zip).map_err(|_| Failure::Mismatch)?;
    require_payload(&members, version)?;
    let staging =
        create_staging(layout.base()).map_err(|err| install_err(err.to_string(), true))?;
    let mut exes: Vec<HeldExe> = Vec::new();
    let written = (|| -> std::result::Result<(), Failure> {
        let mut seen: Vec<&str> = Vec::new();
        for member in &members {
            let rel = safe_relative(&member.name).map_err(|_| Failure::Mismatch)?;
            if seen.contains(&member.name.as_str()) {
                return Err(Failure::Mismatch);
            }
            seen.push(member.name.as_str());
            let dest = staging.join(&rel);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|err| install_err(err.to_string(), true))?;
            }
            if PAYLOAD_EXES.contains(&member.name.as_str()) {
                #[cfg(windows)]
                {
                    let held = winfile::write_held(&dest, &member.bytes)
                        .map_err(|err| install_err(err.to_string(), true))?;
                    if spki {
                        http::verify_open_file(&held.path, held.handle)?;
                    } else {
                        verify(&held.path)?;
                    }
                    exes.push(held);
                }
                #[cfg(not(windows))]
                {
                    let _ = spki;
                    std::fs::write(&dest, &member.bytes)
                        .map_err(|err| install_err(err.to_string(), true))?;
                    verify(&dest)?;
                }
            } else {
                std::fs::write(&dest, &member.bytes)
                    .map_err(|err| install_err(err.to_string(), true))?;
            }
        }
        Ok(())
    })();
    if let Err(failure) = written {
        exes.clear();
        release_staging(&staging);
        return Err(failure);
    }
    Ok(StagedTree { dir: staging, exes })
}

/// Downloads `version`, checks it and puts it in place of the current
/// install. An invalid or below-floor version does no network or disk
/// work. The preferred host is tried first; the other host is tried
/// only when the zip and sidecar were not both fetched. After a
/// complete pair, checksum, extraction, signature, and install failures
/// are terminal. Blocking; run it off the UI thread.
#[cfg(test)]
fn download_and_install_with(
    layout: &InstallLayout,
    version: &str,
    fetch: &Fetch<'_>,
    verify: &dyn Fn(&Path) -> std::result::Result<(), Failure>,
) -> std::result::Result<(), Failure> {
    download_inner(layout, version, fetch, verify, false)
}

fn download_inner(
    layout: &InstallLayout,
    version: &str,
    fetch: &Fetch<'_>,
    verify: &dyn Fn(&Path) -> std::result::Result<(), Failure>,
    spki: bool,
) -> std::result::Result<(), Failure> {
    if easykey_version_number(version).is_none_or(|number| number < min_accepted_number()) {
        return Err(Failure::Download(Error::new(
            "the version is not an acceptable update target",
        )));
    }
    let mut failure = None;
    for source in sources() {
        let (zip, sidecar) = match fetch_pair(source, version, fetch) {
            Ok(pair) => pair,
            Err(err) => {
                failure = Some(err);
                continue;
            }
        };
        // EasyKey records the host once both files are in hand, before
        // the checksum. A later gate failure does not try the other host.
        set_preferred(source);
        let mut staged = stage(&zip, &sidecar, version, layout, verify, spki)?;
        let _guard = update_lock().lock().unwrap_or_else(|err| err.into_inner());
        let staging_dir = staged.dir.clone();
        swap_tree(layout, &staging_dir, &mut staged.exes)?;
        // The applying process still reports the old crate version, so
        // the archive just installed is kept by the version argument.
        sweep_firefox(layout.base(), version);
        return Ok(());
    }
    Err(failure
        .unwrap_or_else(|| Failure::Download(Error::new("no release host could be reached"))))
}

pub fn download_and_install(
    layout: &InstallLayout,
    version: &str,
) -> std::result::Result<(), Failure> {
    download_inner(
        layout,
        version,
        &production_fetch,
        &http::verify_file_signature,
        true,
    )
}

/// The `.old` sibling a file steps aside to during the swap.
fn old_sibling(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".old");
    PathBuf::from(name)
}

/// Every file under `dir`, as paths relative to `dir` (`/`-joined in
/// the zip's spelling, OS paths here).
fn collect_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            collect_files(&entry.path(), out)?;
        } else if kind.is_file() {
            out.push(entry.path());
        }
    }
    Ok(())
}

/// Put the staged tree in place of the install, file by file: each
/// current file steps aside to its `.old` sibling, the staged file
/// takes its name, and any failure rolls every swapped file back.
/// Executables that are still held open are renamed by that handle.
/// Files the new tree does not carry are left alone.
fn swap_tree(
    layout: &InstallLayout,
    staging: &Path,
    exes: &mut Vec<HeldExe>,
) -> std::result::Result<(), Failure> {
    let mut staged_files = Vec::new();
    collect_files(staging, &mut staged_files).map_err(|err| install_err(err.to_string(), true))?;
    staged_files.sort();
    let mut swapped: Vec<(PathBuf, PathBuf, bool)> = Vec::new();
    for staged in &staged_files {
        let rel = staged
            .strip_prefix(staging)
            .expect("staged file is under staging");
        let target = layout.base().join(rel);
        if let Some(parent) = target.parent() {
            if let Err(err) = std::fs::create_dir_all(parent) {
                return Err(abort_swap(exes, &swapped, err.to_string(), true));
            }
        }
        let backup = old_sibling(&target);
        let had_original = target.exists();
        if backup.exists() {
            if let Err(err) = std::fs::remove_file(&backup) {
                return Err(abort_swap(exes, &swapped, err.to_string(), true));
            }
        }
        if had_original {
            if let Err(err) = std::fs::rename(&target, &backup) {
                return Err(abort_swap(exes, &swapped, err.to_string(), true));
            }
        }
        if let Err(err) = place_staged(staged, &target, exes) {
            exes.clear();
            let mut restored = true;
            if had_original && std::fs::rename(&backup, &target).is_err() {
                restored = false;
            }
            if !undo(&swapped) {
                restored = false;
            }
            return Err(install_err(err.to_string(), restored));
        }
        swapped.push((target, backup, had_original));
    }
    Ok(())
}

/// Close held files, then put back what the swap already moved.
/// A held executable denies delete, so the handles go first.
fn abort_swap(
    exes: &mut Vec<HeldExe>,
    swapped: &[(PathBuf, PathBuf, bool)],
    detail: String,
    this_restored: bool,
) -> Failure {
    exes.clear();
    install_err(detail, undo(swapped) && this_restored)
}

/// Reverse a partial swap. False when a file could not be put back.
fn undo(swapped: &[(PathBuf, PathBuf, bool)]) -> bool {
    let mut restored = true;
    for (target, backup, had_original) in swapped.iter().rev() {
        if target.exists() && std::fs::remove_file(target).is_err() {
            restored = false;
        }
        if *had_original && std::fs::rename(backup, target).is_err() {
            restored = false;
        }
    }
    restored
}

fn place_staged(staged: &Path, target: &Path, exes: &[HeldExe]) -> std::io::Result<()> {
    #[cfg(windows)]
    if let Some(held) = exes.iter().find(|exe| exe.path == *staged) {
        return winfile::rename_by_handle(held.handle, target);
    }
    let _ = exes;
    std::fs::rename(staged, target)
}

/// Delete direct regular `udm-extension-firefox-*.zip` files in
/// `<install>\extension`, except the archive named for `keep_version`.
/// A deletion that fails is left for the next start.
fn sweep_firefox(install: &Path, keep_version: &str) {
    let dir = install.join("extension");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let keep = firefox_archive_name(keep_version);
    let keep_name = keep.rsplit_once('/').map(|(_, name)| name).unwrap_or(&keep);
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_file() {
            continue;
        }
        let Some(name) = entry.file_name().into_string().ok() else {
            continue;
        };
        if name == keep_name {
            continue;
        }
        if is_firefox_archive(&format!("extension/{name}")) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// One cleanup pass. False only when a `.old` file or an inactive
/// staging directory could not be removed. Firefox deletion failures
/// do not fail the pass.
fn cleanup_once(layout: &InstallLayout) -> bool {
    let _guard = update_lock().lock().unwrap_or_else(|err| err.into_inner());
    let old_ok = remove_old_files(layout.base());
    let staging_ok = remove_inactive_staging(layout.base());
    sweep_firefox(layout.base(), VERSION);
    old_ok && staging_ok
}

fn remove_inactive_staging(base: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(base) else {
        return true;
    };
    let active = active_staging()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    let mut ok = true;
    for entry in entries.flatten() {
        if !is_staging_name(&entry.file_name()) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if !kind.is_dir() {
            continue;
        }
        let path = entry.path();
        if active.iter().any(|active_path| active_path == &path) {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_err() {
            ok = false;
        }
    }
    ok
}

fn cleanup_loop(layout: &InstallLayout, attempts: u32) {
    for index in 0..attempts {
        if cleanup_once(layout) {
            break;
        }
        if index + 1 < attempts {
            std::thread::sleep(std::time::Duration::from_millis(CLEANUP_DELAY_MS));
        }
    }
}

/// On GUI start: remove `.old` files, inactive staging, and stale
/// Firefox archives. Retries stay off the caller. A file still locked
/// after [`CLEANUP_ATTEMPTS`] is kept for the next start.
pub fn finish_previous_update(layout: &InstallLayout) {
    let layout = layout.clone();
    std::thread::spawn(move || cleanup_loop(&layout, CLEANUP_ATTEMPTS));
}

/// On CLI start: one pass before the command returns. Further retries
/// of a locked file do not block process exit.
pub fn finish_previous_update_for_cli(layout: &InstallLayout) {
    if cleanup_once(layout) {
        return;
    }
    let layout = layout.clone();
    std::thread::spawn(move || {
        for _ in 1..CLEANUP_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(CLEANUP_DELAY_MS));
            if cleanup_once(&layout) {
                break;
            }
        }
    });
}

/// Remove every regular `*.old` file under `dir`. Directory links are
/// not followed. Returns false when any removal failed.
fn remove_old_files(dir: &Path) -> bool {
    let mut done = true;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    for entry in entries.flatten() {
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
            if !remove_old_files(&entry.path()) {
                done = false;
            }
        } else if kind.is_file()
            && entry.file_name().to_string_lossy().ends_with(".old")
            && std::fs::remove_file(entry.path()).is_err()
        {
            done = false;
        }
    }
    done
}

// ---------------------------------------------------------------------------
// Zip reading (stored + deflate) and member path safety
// ---------------------------------------------------------------------------

struct ZipMember {
    name: String,
    bytes: Vec<u8>,
}

/// A member path is usable only as a plain relative path: `/`
/// separators, no empty, `.` or `..` components, no backslash, no
/// leading separator. Anything else refuses the archive.
fn safe_relative(name: &str) -> std::result::Result<PathBuf, ()> {
    if name.is_empty() || name.starts_with('/') || name.contains('\\') {
        return Err(());
    }
    let mut path = PathBuf::new();
    for part in name.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(());
        }
        path.push(part);
    }
    Ok(path)
}

fn read_zip(data: &[u8]) -> std::result::Result<Vec<ZipMember>, String> {
    let eocd = find_eocd(data)?;
    let entries = u16_at(data, eocd + 10)? as usize;
    let cd_size = u32_at(data, eocd + 12)? as usize;
    let cd_offset = u32_at(data, eocd + 16)? as usize;
    if cd_offset.saturating_add(cd_size) > data.len() {
        return Err("central directory is out of range".into());
    }
    let mut cursor = cd_offset;
    let mut members = Vec::with_capacity(entries);
    for _ in 0..entries {
        if cursor + 46 > data.len() || u32_at(data, cursor)? != 0x0201_4b50 {
            return Err("bad central directory entry".into());
        }
        let flags = u16_at(data, cursor + 8)?;
        let method = u16_at(data, cursor + 10)?;
        let comp_size = u32_at(data, cursor + 20)? as usize;
        let name_len = u16_at(data, cursor + 28)? as usize;
        let extra_len = u16_at(data, cursor + 30)? as usize;
        let comment_len = u16_at(data, cursor + 32)? as usize;
        let local_offset = u32_at(data, cursor + 42)? as usize;
        let name_start = cursor + 46;
        let name_end = name_start + name_len;
        if name_end > data.len() {
            return Err("central name is out of range".into());
        }
        let name = std::str::from_utf8(&data[name_start..name_end])
            .map_err(|_| "zip name is not UTF-8".to_owned())?
            .to_owned();
        cursor = name_end + extra_len + comment_len;
        if name.ends_with('/') {
            continue;
        }
        if flags & 1 != 0 {
            return Err(format!("{name} is encrypted"));
        }
        let bytes = read_local(data, local_offset, method, comp_size)?;
        members.push(ZipMember { name, bytes });
    }
    Ok(members)
}

fn read_local(
    data: &[u8],
    offset: usize,
    method: u16,
    comp_size: usize,
) -> std::result::Result<Vec<u8>, String> {
    use std::io::Read as _;
    if offset + 30 > data.len() || u32_at(data, offset)? != 0x0403_4b50 {
        return Err("bad local header".into());
    }
    let name_len = u16_at(data, offset + 26)? as usize;
    let extra_len = u16_at(data, offset + 28)? as usize;
    let data_start = offset + 30 + name_len + extra_len;
    let data_end = data_start + comp_size;
    if data_end > data.len() {
        return Err("local data is out of range".into());
    }
    let compressed = &data[data_start..data_end];
    match method {
        0 => Ok(compressed.to_vec()),
        8 => {
            let mut decoder = flate2::read::DeflateDecoder::new(compressed);
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .map_err(|err| format!("deflate: {err}"))?;
            Ok(out)
        }
        other => Err(format!("unsupported method {other}")),
    }
}

fn find_eocd(data: &[u8]) -> std::result::Result<usize, String> {
    if data.len() < 22 {
        return Err("too small".into());
    }
    let start = data.len().saturating_sub(22 + 65535);
    let mut index = data.len() - 22;
    loop {
        if data[index..].starts_with(&0x0605_4b50u32.to_le_bytes()) {
            let comment_len = u16_at(data, index + 20)? as usize;
            if index + 22 + comment_len == data.len() {
                return Ok(index);
            }
        }
        if index == start {
            break;
        }
        index -= 1;
    }
    Err("missing end of central directory".into())
}

fn u16_at(data: &[u8], index: usize) -> std::result::Result<u16, String> {
    let bytes: [u8; 2] = data
        .get(index..index + 2)
        .ok_or_else(|| "truncated field".to_owned())?
        .try_into()
        .map_err(|_| "truncated field".to_owned())?;
    Ok(u16::from_le_bytes(bytes))
}

fn u32_at(data: &[u8], index: usize) -> std::result::Result<u32, String> {
    let bytes: [u8; 4] = data
        .get(index..index + 4)
        .ok_or_else(|| "truncated field".to_owned())?
        .try_into()
        .map_err(|_| "truncated field".to_owned())?;
    Ok(u32::from_le_bytes(bytes))
}

// ---------------------------------------------------------------------------
// HTTP (WinHTTP) + Authenticode/SPKI verification — Windows
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[allow(unsafe_code)]
mod http {
    use super::*;
    use std::ffi::c_void;
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Networking::WinHttp::*;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn os_error() -> Error {
        Error::new(std::io::Error::last_os_error().to_string())
    }

    /// Closes a WinHTTP handle when it goes out of scope.
    struct Handle(*mut c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the handle came from WinHTTP and is closed once.
                unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    fn handle(h: *mut c_void) -> Result<Handle> {
        if h.is_null() {
            Err(os_error())
        } else {
            Ok(Handle(h))
        }
    }

    struct Response {
        status: u32,
        request: Handle,
        _connection: Handle,
        _session: Handle,
    }

    /// Sends a GET to an `https://` URL and waits for the headers.
    fn send(url: &str, follow_redirects: bool) -> Result<Response> {
        let rest = url
            .strip_prefix("https://")
            .ok_or_else(|| Error::new("HTTPS only"))?;
        let (host, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let path = if path.is_empty() { "/" } else { path };
        let (host, path) = (wide(host), wide(path));
        let agent = wide(&format!("UDM/{}", current()));
        let verb = wide("GET");
        // SAFETY: every pointer passed is a NUL-terminated buffer that
        // lives for the call, and every handle is checked before use.
        unsafe {
            let session = handle(WinHttpOpen(
                agent.as_ptr(),
                WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
                null(),
                null(),
                0,
            ))?;
            WinHttpSetTimeouts(session.0, 10_000, 10_000, 15_000, 30_000);
            let connection = handle(WinHttpConnect(session.0, host.as_ptr(), 443, 0))?;
            let request = handle(WinHttpOpenRequest(
                connection.0,
                verb.as_ptr(),
                path.as_ptr(),
                null(),
                null(),
                null(),
                WINHTTP_FLAG_SECURE,
            ))?;
            if !follow_redirects {
                let off: u32 = WINHTTP_DISABLE_REDIRECTS;
                WinHttpSetOption(
                    request.0,
                    WINHTTP_OPTION_DISABLE_FEATURE,
                    &off as *const u32 as *const c_void,
                    4,
                );
            }
            if WinHttpSendRequest(request.0, null(), 0, null(), 0, 0, 0) == 0
                || WinHttpReceiveResponse(request.0, null_mut()) == 0
            {
                return Err(os_error());
            }
            let mut status: u32 = 0;
            let mut size = 4u32;
            WinHttpQueryHeaders(
                request.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                null(),
                &mut status as *mut u32 as *mut c_void,
                &mut size,
                null_mut(),
            );
            Ok(Response {
                status,
                request,
                _connection: connection,
                _session: session,
            })
        }
    }

    /// The body of `url`, following redirects (release files live on
    /// another host), as long as it fits in `max` bytes.
    pub fn get(url: &str, max: usize) -> Result<Vec<u8>> {
        let res = send(url, true)?;
        if res.status != 200 {
            return Err(Error::new(format!("HTTP {}", res.status)));
        }
        let mut body = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            // SAFETY: `buf` holds `buf.len()` bytes.
            let ok = unsafe {
                WinHttpReadData(
                    res.request.0,
                    buf.as_mut_ptr() as *mut c_void,
                    buf.len() as u32,
                    &mut read,
                )
            };
            if ok == 0 {
                return Err(os_error());
            }
            if read == 0 {
                return Ok(body);
            }
            body.extend_from_slice(&buf[..read as usize]);
            if body.len() > max {
                return Err(Error::new("file too large"));
            }
        }
    }

    // --- Authenticode + SPKI gate -------------------------------------
    //
    // Same house pattern as EasyKey's updater verification (verify,
    // compare pin, fail closed), but the comparison is an SPKI hash
    // rather than a certificate thumbprint. The signing certificate is
    // self-issued, so the default `WINTRUST_ACTION_GENERIC_VERIFY_V2`
    // policy cannot express the trust rule on its own (it reports an
    // untrusted root for a perfectly good self-issued signature,
    // exactly as `Get-AuthenticodeSignature` does with
    // `UnknownError`). The same flag/provider structure EasyKey uses is
    // therefore used here: `WINTRUST_ACTION_GENERIC_CHAIN_VERIFY` with
    // an isolated chain engine whose exclusive root is the file's own
    // signer certificate, plus a policy callback that checks
    // cryptographic validity, the SPKI pin, and a mandatory RFC 3161
    // timestamp whose chain verifies against normal Windows trust
    // (DigiCert). Trusting the extracted signer for chain building is
    // safe because acceptance still requires the SPKI pin: any other
    // key is rejected no matter what signed the file.
    //
    // `WinVerifyTrust` judges validity at signing time when a
    // countersignature is present, so a countersigned binary stays
    // acceptable after the signing certificate expires; no date of our
    // own is ever consulted.
    use windows_sys::core::{HRESULT, PCSTR};
    use windows_sys::Win32::Foundation::HWND;
    use windows_sys::Win32::Security::Cryptography::*;
    use windows_sys::Win32::Security::WinTrust::*;

    // `wintrust.h` values missing from windows-sys. Only the failure is
    // load-bearing (anything but `S_OK` fails closed); the exact codes
    // keep logs comparable with EasyKey's verifier.
    const TRUST_E_SUBJECT_NOT_TRUSTED: HRESULT = 0x800B0004u32 as i32;
    const TRUST_E_TIME_STAMP: HRESULT = 0x800B0009u32 as i32;
    const S_OK: HRESULT = 0;

    /// Callback argument: whether the policy accepted the file.
    struct PolicyArg {
        accepted: bool,
    }

    /// DER-encode the SubjectPublicKeyInfo of `cert` (any key type).
    fn spki_der_of(cert: *const CERT_CONTEXT) -> Option<Vec<u8>> {
        // SAFETY: `cert` is a live context from our own store lookup
        // below.
        unsafe {
            let info = (*cert).pCertInfo;
            if info.is_null() {
                return None;
            }
            let spki = &(*info).SubjectPublicKeyInfo as *const CERT_PUBLIC_KEY_INFO;
            let mut len: u32 = 0;
            if CryptEncodeObject(
                X509_ASN_ENCODING,
                X509_PUBLIC_KEY_INFO,
                spki as *const c_void,
                null_mut(),
                &mut len,
            ) == 0
                || len == 0
            {
                return None;
            }
            let mut der = vec![0u8; len as usize];
            if CryptEncodeObject(
                X509_ASN_ENCODING,
                X509_PUBLIC_KEY_INFO,
                spki as *const c_void,
                der.as_mut_ptr(),
                &mut len,
            ) == 0
            {
                return None;
            }
            der.truncate(len as usize);
            Some(der)
        }
    }

    fn oid_eq(a: PCSTR, b: PCSTR) -> bool {
        use std::ffi::CStr;
        use std::os::raw::c_char;
        // SAFETY: both are NUL-terminated OID strings from the OS or
        // headers.
        unsafe {
            !a.is_null()
                && !b.is_null()
                && CStr::from_ptr(a as *const c_char) == CStr::from_ptr(b as *const c_char)
        }
    }

    /// Policy callback: exactly one SHA-256 primary signature whose
    /// leaf carries the pinned SPKI, an `AUTHENTICODE` chain that
    /// verifies, and a mandatory RFC 3161 countersign whose chain
    /// verifies against system trust. Anything else fails closed
    /// without setting `accepted`.
    unsafe extern "system" fn spki_policy_callback(
        _prov_data: *mut CRYPT_PROVIDER_DATA,
        step_error: u32,
        _policy_settings: u32,
        signer_count: u32,
        signers: *mut *mut WTD_GENERIC_CHAIN_POLICY_SIGNER_INFO,
        policy_arg: *mut c_void,
    ) -> HRESULT {
        // SAFETY: WinVerifyTrust calls this with live provider data
        // and a live `PolicyArg`; every pointer is checked before use.
        unsafe {
            let ctx = &mut *(policy_arg as *mut PolicyArg);
            if step_error != 0 {
                return step_error as HRESULT;
            }
            // Release tooling produces one primary signature. Fail
            // closed on ambiguous multi-signatures rather than
            // accepting an unrelated one.
            if signer_count != 1 || signers.is_null() || (*signers).is_null() {
                return TRUST_E_SUBJECT_NOT_TRUSTED;
            }
            let signer = &*(*signers);
            if signer.dwError != 0 {
                return TRUST_E_SUBJECT_NOT_TRUSTED;
            }
            let chain = signer.pChainContext;
            if chain.is_null() || (*chain).cChain == 0 || (*chain).rgpChain.is_null() {
                return TRUST_E_SUBJECT_NOT_TRUSTED;
            }
            let simple = *(*chain).rgpChain;
            if simple.is_null() || (*simple).cElement == 0 || (*simple).rgpElement.is_null() {
                return TRUST_E_SUBJECT_NOT_TRUSTED;
            }
            let leaf = (*(*(*simple).rgpElement)).pCertContext;
            if leaf.is_null() {
                return TRUST_E_SUBJECT_NOT_TRUSTED;
            }
            match spki_der_of(leaf) {
                Some(der) if spki_pin_matches(&der) => {}
                _ => return TRUST_E_SUBJECT_NOT_TRUSTED,
            }
            let msg_info = signer.pMsgSignerInfo;
            if msg_info.is_null() || !oid_eq((*msg_info).HashAlgorithm.pszObjId, szOID_NIST_sha256)
            {
                return TRUST_E_SUBJECT_NOT_TRUSTED;
            }
            let mut extra: AUTHENTICODE_EXTRA_CERT_CHAIN_POLICY_PARA = std::mem::zeroed();
            extra.cbSize = std::mem::size_of::<AUTHENTICODE_EXTRA_CERT_CHAIN_POLICY_PARA>() as u32;
            extra.pSignerInfo = msg_info;
            let mut para: CERT_CHAIN_POLICY_PARA = std::mem::zeroed();
            para.cbSize = std::mem::size_of::<CERT_CHAIN_POLICY_PARA>() as u32;
            para.pvExtraPolicyPara = &mut extra as *mut _ as *mut c_void;
            let mut status: CERT_CHAIN_POLICY_STATUS = std::mem::zeroed();
            status.cbSize = std::mem::size_of::<CERT_CHAIN_POLICY_STATUS>() as u32;
            if CertVerifyCertificateChainPolicy(
                CERT_CHAIN_POLICY_AUTHENTICODE,
                chain,
                &para,
                &mut status,
            ) == 0
                || status.dwError != 0
            {
                return TRUST_E_SUBJECT_NOT_TRUSTED;
            }
            let mut has_rfc3161 = false;
            let attrs = &(*msg_info).UnauthAttrs;
            if !attrs.rgAttr.is_null() {
                for i in 0..attrs.cAttr {
                    let attr = &*attrs.rgAttr.add(i as usize);
                    if oid_eq(attr.pszObjId, szOID_RFC3161_counterSign) {
                        has_rfc3161 = true;
                        break;
                    }
                }
            }
            if !has_rfc3161 || signer.rgpCounterSigner.is_null() {
                return TRUST_E_TIME_STAMP;
            }
            // The timestamp chain must use normal Windows trust, never
            // the signer's own key: it is what keeps the verdict
            // meaningful after the signing certificate expires.
            for i in 0..signer.cCounterSigner {
                let counter = *signer.rgpCounterSigner.add(i as usize);
                if counter.is_null()
                    || (*counter).dwError != 0
                    || (*counter).pChainContext.is_null()
                {
                    continue;
                }
                let mut ts_para: CERT_CHAIN_POLICY_PARA = std::mem::zeroed();
                ts_para.cbSize = std::mem::size_of::<CERT_CHAIN_POLICY_PARA>() as u32;
                let mut ts_status: CERT_CHAIN_POLICY_STATUS = std::mem::zeroed();
                ts_status.cbSize = std::mem::size_of::<CERT_CHAIN_POLICY_STATUS>() as u32;
                if CertVerifyCertificateChainPolicy(
                    CERT_CHAIN_POLICY_AUTHENTICODE_TS,
                    (*counter).pChainContext,
                    &ts_para,
                    &mut ts_status,
                ) != 0
                    && ts_status.dwError == 0
                {
                    ctx.accepted = true;
                    return S_OK;
                }
            }
            TRUST_E_TIME_STAMP
        }
    }

    /// The file's signer certificate, from the embedded PKCS#7 (no
    /// trust involved: this is staging input for the engine below, not
    /// a verdict). The caller owns the returned context and the
    /// message/store handles.
    fn signer_and_handles(path_wide: &[u16]) -> (*mut CERT_CONTEXT, *mut c_void, HCERTSTORE) {
        signer_from_query(CERT_QUERY_OBJECT_FILE, path_wide.as_ptr() as *const c_void)
    }

    /// The signer embedded in `bytes`. Used when the file is already
    /// open with write and delete sharing denied, so a second open by
    /// path cannot see it.
    fn signer_from_blob(bytes: &[u8]) -> (*mut CERT_CONTEXT, *mut c_void, HCERTSTORE) {
        let blob = CRYPT_INTEGER_BLOB {
            cbData: bytes.len() as u32,
            pbData: bytes.as_ptr() as *mut u8,
        };
        signer_from_query(CERT_QUERY_OBJECT_BLOB, &blob as *const _ as *const c_void)
    }

    fn signer_from_query(
        kind: CERT_QUERY_OBJECT_TYPE,
        object: *const c_void,
    ) -> (*mut CERT_CONTEXT, *mut c_void, HCERTSTORE) {
        // SAFETY: every pointer passed is NUL-terminated or sized as
        // passed; every handle is checked before use; the caller
        // releases all three.
        unsafe {
            let mut encoding: CERT_QUERY_ENCODING_TYPE = std::mem::zeroed();
            let mut content: CERT_QUERY_CONTENT_TYPE = std::mem::zeroed();
            let mut format: CERT_QUERY_FORMAT_TYPE = std::mem::zeroed();
            let mut store: HCERTSTORE = std::mem::zeroed();
            let mut msg: *mut c_void = null_mut();
            let ok = CryptQueryObject(
                kind,
                object,
                CERT_QUERY_CONTENT_FLAG_PKCS7_SIGNED_EMBED,
                CERT_QUERY_FORMAT_FLAG_BINARY,
                0,
                &mut encoding,
                &mut content,
                &mut format,
                &mut store,
                &mut msg,
                null_mut(),
            );
            if ok == 0 || store.is_null() || msg.is_null() {
                if !store.is_null() {
                    CertCloseStore(store, 0);
                }
                if !msg.is_null() {
                    CryptMsgClose(msg);
                }
                return (null_mut(), null_mut(), std::mem::zeroed());
            }
            let mut info_len: u32 = 0;
            if CryptMsgGetParam(
                msg,
                CMSG_SIGNER_CERT_INFO_PARAM,
                0,
                null_mut(),
                &mut info_len,
            ) == 0
                || info_len == 0
            {
                CertCloseStore(store, 0);
                CryptMsgClose(msg);
                return (null_mut(), null_mut(), std::mem::zeroed());
            }
            let mut buf = vec![0u8; info_len as usize];
            if CryptMsgGetParam(
                msg,
                CMSG_SIGNER_CERT_INFO_PARAM,
                0,
                buf.as_mut_ptr() as *mut c_void,
                &mut info_len,
            ) == 0
            {
                CertCloseStore(store, 0);
                CryptMsgClose(msg);
                return (null_mut(), null_mut(), std::mem::zeroed());
            }
            let signer = CertFindCertificateInStore(
                store,
                X509_ASN_ENCODING,
                std::mem::zeroed(),
                CERT_FIND_SUBJECT_CERT,
                buf.as_ptr() as *const c_void,
                null(),
            );
            if signer.is_null() {
                CertCloseStore(store, 0);
                CryptMsgClose(msg);
                return (null_mut(), null_mut(), std::mem::zeroed());
            }
            (signer, msg, store)
        }
    }

    /// Bytes of an already-open file, then the pointer back at the start
    /// so WinTrust reads the same handle.
    fn read_handle(
        file: windows_sys::Win32::Foundation::HANDLE,
    ) -> std::result::Result<Vec<u8>, ()> {
        use windows_sys::Win32::Storage::FileSystem::{ReadFile, SetFilePointerEx, FILE_BEGIN};
        let mut body = Vec::new();
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let mut read = 0u32;
            let ok = unsafe {
                ReadFile(
                    file,
                    buf.as_mut_ptr(),
                    buf.len() as u32,
                    &mut read,
                    null_mut(),
                )
            };
            if ok == 0 {
                return Err(());
            }
            if read == 0 {
                break;
            }
            body.extend_from_slice(&buf[..read as usize]);
            if body.len() > MAX_DOWNLOAD {
                return Err(());
            }
        }
        let rewound = unsafe { SetFilePointerEx(file, 0, null_mut(), FILE_BEGIN) };
        if rewound == 0 {
            return Err(());
        }
        Ok(body)
    }

    /// Runs `WinVerifyTrust` with the isolated engine and the SPKI
    /// policy. Returns true only when the signature is
    /// cryptographically valid, the SPKI pin matches, and a
    /// system-trusted RFC 3161 timestamp is present.
    pub fn verify_file_signature(path: &Path) -> std::result::Result<(), Failure> {
        verify_open_file(path, 0)
    }

    /// Same gate as [`verify_file_signature`], reading the file through
    /// `file` when it is a live handle. `0` means the path alone.
    pub(super) fn verify_open_file(
        path: &Path,
        file: windows_sys::Win32::Foundation::HANDLE,
    ) -> std::result::Result<(), Failure> {
        let path_wide = wide(&path.to_string_lossy());
        let held_bytes = if file != 0 {
            match read_handle(file) {
                Ok(bytes) => Some(bytes),
                Err(()) => return Err(Failure::Signature),
            }
        } else {
            None
        };
        // SAFETY: every pointer passed is NUL-terminated or sized as
        // passed; every handle is checked before use and released
        // before returning.
        unsafe {
            let (signer, msg, store) = match held_bytes.as_deref() {
                Some(bytes) => signer_from_blob(bytes),
                None => signer_and_handles(&path_wide),
            };
            if signer.is_null() {
                return Err(Failure::Signature);
            }
            // Isolated engine trusting only this file's signer, the
            // same structure EasyKey uses for its pinned certificate.
            // Nothing is added to the machine or user certificate
            // stores.
            let roots = CertOpenStore(CERT_STORE_PROV_MEMORY, std::mem::zeroed(), 0, 0, null());
            let mut engine: HCERTCHAINENGINE = std::mem::zeroed();
            let mut ready = false;
            if !roots.is_null()
                && CertAddCertificateContextToStore(roots, signer, CERT_STORE_ADD_NEW, null_mut())
                    != 0
            {
                let mut config: CERT_CHAIN_ENGINE_CONFIG = std::mem::zeroed();
                config.cbSize = std::mem::size_of::<CERT_CHAIN_ENGINE_CONFIG>() as u32;
                config.hExclusiveRoot = roots;
                if CertCreateCertificateChainEngine(&config, &mut engine) != 0 && engine != 0 {
                    ready = true;
                }
            }
            let mut valid = false;
            if ready {
                let mut code_usage = szOID_PKIX_KP_CODE_SIGNING as *mut u8;
                let mut ts_usage = szOID_PKIX_KP_TIMESTAMP_SIGNING as *mut u8;
                let mut code_para: CERT_CHAIN_PARA = std::mem::zeroed();
                code_para.cbSize = std::mem::size_of::<CERT_CHAIN_PARA>() as u32;
                code_para.RequestedUsage.dwType = USAGE_MATCH_TYPE_AND;
                code_para.RequestedUsage.Usage.cUsageIdentifier = 1;
                code_para.RequestedUsage.Usage.rgpszUsageIdentifier = &mut code_usage;
                let mut ts_para: CERT_CHAIN_PARA = std::mem::zeroed();
                ts_para.cbSize = std::mem::size_of::<CERT_CHAIN_PARA>() as u32;
                ts_para.RequestedUsage.dwType = USAGE_MATCH_TYPE_AND;
                ts_para.RequestedUsage.Usage.cUsageIdentifier = 1;
                ts_para.RequestedUsage.Usage.rgpszUsageIdentifier = &mut ts_usage;
                let mut primary: WTD_GENERIC_CHAIN_POLICY_CREATE_INFO = std::mem::zeroed();
                primary.Anonymous.cbSize =
                    std::mem::size_of::<WTD_GENERIC_CHAIN_POLICY_CREATE_INFO>() as u32;
                primary.hChainEngine = engine;
                primary.pChainPara = &mut code_para;
                primary.dwFlags = CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL;
                let mut timestamp: WTD_GENERIC_CHAIN_POLICY_CREATE_INFO = std::mem::zeroed();
                timestamp.Anonymous.cbSize =
                    std::mem::size_of::<WTD_GENERIC_CHAIN_POLICY_CREATE_INFO>() as u32;
                timestamp.pChainPara = &mut ts_para;
                timestamp.dwFlags = CERT_CHAIN_CACHE_ONLY_URL_RETRIEVAL;
                let mut chain: WTD_GENERIC_CHAIN_POLICY_DATA = std::mem::zeroed();
                chain.Anonymous.cbSize =
                    std::mem::size_of::<WTD_GENERIC_CHAIN_POLICY_DATA>() as u32;
                chain.pSignerChainInfo = &mut primary;
                chain.pCounterSignerChainInfo = &mut timestamp;
                chain.pfnPolicyCallback = Some(spki_policy_callback);
                let mut arg = PolicyArg { accepted: false };
                chain.pvPolicyArg = &mut arg as *mut _ as *mut c_void;
                let mut file_info: WINTRUST_FILE_INFO = std::mem::zeroed();
                file_info.cbStruct = std::mem::size_of::<WINTRUST_FILE_INFO>() as u32;
                file_info.pcwszFilePath = path_wide.as_ptr();
                file_info.hFile = file;
                let mut trust: WINTRUST_DATA = std::mem::zeroed();
                trust.cbStruct = std::mem::size_of::<WINTRUST_DATA>() as u32;
                trust.pPolicyCallbackData = &mut chain as *mut _ as *mut c_void;
                trust.dwUIChoice = WTD_UI_NONE;
                trust.fdwRevocationChecks = WTD_REVOKE_NONE;
                trust.dwUnionChoice = WTD_CHOICE_FILE;
                trust.Anonymous.pFile = &mut file_info;
                trust.dwStateAction = WTD_STATEACTION_VERIFY;
                trust.dwProvFlags = std::mem::transmute::<u32, WINTRUST_DATA_PROVIDER_FLAGS>(
                    WTD_CACHE_ONLY_URL_RETRIEVAL | WTD_REVOCATION_CHECK_NONE,
                );
                trust.dwUIContext = WTD_UICONTEXT_EXECUTE;
                let mut action = WINTRUST_ACTION_GENERIC_CHAIN_VERIFY;
                valid = WinVerifyTrust(
                    null_mut::<c_void>() as HWND,
                    &mut action,
                    &mut trust as *mut _ as *mut c_void,
                ) == 0
                    && arg.accepted;
                trust.dwStateAction = WTD_STATEACTION_CLOSE;
                WinVerifyTrust(
                    null_mut::<c_void>() as HWND,
                    &mut action,
                    &mut trust as *mut _ as *mut c_void,
                );
            }
            CertFreeCertificateContext(signer);
            CryptMsgClose(msg);
            CertCloseStore(store, 0);
            if engine != 0 {
                CertFreeCertificateChainEngine(engine);
            }
            if !roots.is_null() {
                CertCloseStore(roots, 0);
            }
            if valid {
                Ok(())
            } else {
                Err(Failure::Signature)
            }
        }
    }
}

/// Off Windows there is nothing to update; this keeps `cargo test`
/// building.
#[cfg(not(windows))]
mod http {
    use super::*;

    pub fn get(_url: &str, _max: usize) -> Result<Vec<u8>> {
        Err(Error::new("Windows only"))
    }

    /// Fail closed off Windows: nothing may be accepted without the
    /// gate.
    pub fn verify_file_signature(_path: &Path) -> std::result::Result<(), Failure> {
        Err(Failure::Signature)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn hex_decode(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("v0.2.3"), Some((0, 2, 3)));
        assert_eq!(parse_version(" 2.0.0 "), Some((2, 0, 0)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("1.2.x"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn compares_with_this_exe() {
        assert!(is_newer("999.0.0"));
        assert!(!is_newer("0.0.1"));
        assert!(!is_newer(current()));
        assert!(!is_newer("garbage"));
    }

    fn host(source: Source, outcome: std::result::Result<&str, &str>) -> HostRead {
        HostRead {
            source,
            outcome: outcome.map(str::to_owned).map_err(str::to_owned),
        }
    }

    #[test]
    fn easykey_version_boundaries() {
        assert_eq!(
            easykey_version_number("0.3.2"),
            Some((0 << 16) | (3 << 8) | 2)
        );
        assert_eq!(easykey_version_number("65535.255.255"), Some(u32::MAX));
        assert_eq!(easykey_version_number("1.0.0"), Some(1 << 16));
        assert!(
            easykey_version_number("1.0.0").unwrap() > easykey_version_number("0.255.255").unwrap()
        );
        assert_eq!(
            easykey_version_number("01.02.03"),
            easykey_version_number("1.2.3")
        );
        for rejected in [
            "v0.3.3",
            " 0.3.3",
            "0.3.3 ",
            "0.3",
            "0.3.3.1",
            "",
            "0.256.0",
            "0.0.256",
            "65536.0.0",
            "0.3.3-beta",
            "1.2.x",
        ] {
            assert_eq!(easykey_version_number(rejected), None, "{rejected}");
        }
        // The permissive helper still accepts a leading v and surrounding
        // whitespace. Discovery does not use it.
        assert_eq!(parse_version("v0.3.3"), Some((0, 3, 3)));
        assert_eq!(parse_version(" 0.3.3 "), Some((0, 3, 3)));
        assert!(easykey_version_number("v0.3.3").is_none());
    }

    #[test]
    fn discovery_urls_point_at_each_hosts_default_branch() {
        // The public GitHub repo defaults to master, the public GitLab
        // repo defaults to main (its only branch). A wrong branch here
        // makes that host fail every discovery silently.
        assert_eq!(
            Source::GitHub.version_json_url(),
            "https://raw.githubusercontent.com/iamcino/udm/master/version.json"
        );
        assert_eq!(
            Source::GitLab.version_json_url(),
            "https://gitlab.com/cino-group/udm/-/raw/main/version.json"
        );
    }

    #[test]
    fn version_json_is_bound_to_latest_win_object() {
        let inside = "{\n\t\"latestWinVersion\": {\n\t\t\"versionName\": \"0.3.3\",\n\t\t\"versionCode\": 1\n\t}\n}";
        assert_eq!(parse_version_json(inside).unwrap(), "0.3.3");
        let earlier = "{\"other\": {\"versionName\": \"9.9.9\"}, \"latestWinVersion\": {\"versionName\": \"0.3.3\", \"versionCode\": 1}}";
        assert_eq!(parse_version_json(earlier).unwrap(), "0.3.3");
        let later =
            "{\"latestWinVersion\": {\"versionCode\": 1}, \"other\": {\"versionName\": \"9.9.9\"}}";
        assert!(
            parse_version_json(later).is_err(),
            "a later sibling is not the offer"
        );
        let both = "{\"latestWinVersion\": {\"versionName\": \"0.3.3\"}, \"other\": {\"versionName\": \"9.9.9\"}}";
        assert_eq!(parse_version_json(both).unwrap(), "0.3.3");
        assert!(parse_version_json("{\"other\": {\"versionName\": \"9.9.9\"}}").is_err());
        assert!(parse_version_json("{\"latestWinVersion\": {\"versionCode\": 1}}").is_err());
        assert!(
            parse_version_json("{\"latestWinVersion\": {\"versionName\": \"v0.3.3\"}}").is_err()
        );
        assert!(parse_version_json("{\"latestWinVersion\": {\"versionName\": \"0.3\"}}").is_err());
    }

    #[test]
    fn combine_picks_newest_and_breaks_ties_with_the_snapshot() {
        let reports = [
            host(Source::GitHub, Ok("0.3.3")),
            host(Source::GitLab, Ok("0.3.4")),
        ];
        match combine(Source::GitHub, reports) {
            Discovery::Offer { version, source } => {
                assert_eq!(version, "0.3.4");
                assert_eq!(source, Source::GitLab);
            }
            other => panic!("expected the newer GitLab offer, got {other:?}"),
        }
        for snapshot in [Source::GitHub, Source::GitLab] {
            let reports = [
                host(Source::GitHub, Ok("0.3.4")),
                host(Source::GitLab, Ok("0.3.4")),
            ];
            match combine(snapshot, reports) {
                Discovery::Offer { version, source } => {
                    assert_eq!(version, "0.3.4");
                    assert_eq!(source, snapshot);
                }
                other => panic!("tie should follow {snapshot:?}, got {other:?}"),
            }
        }
        match combine(
            Source::GitLab,
            [
                host(Source::GitHub, Ok("0.3.4")),
                host(Source::GitLab, Err("down")),
            ],
        ) {
            Discovery::Offer { version, source } => {
                assert_eq!(version, "0.3.4");
                assert_eq!(source, Source::GitHub);
            }
            other => panic!("a surviving offer wins, got {other:?}"),
        }
    }

    #[test]
    fn combine_classifies_floor_and_unreadable_hosts() {
        match combine(
            Source::GitHub,
            [
                host(Source::GitHub, Ok("0.3.1")),
                host(Source::GitLab, Ok("0.3.0")),
            ],
        ) {
            Discovery::NoAcceptable { detail } => {
                assert!(detail.contains("0.3.1"));
                assert!(detail.contains("GitLab"));
            }
            other => panic!("both below the floor, got {other:?}"),
        }
        match combine(
            Source::GitHub,
            [
                host(Source::GitHub, Ok("0.3.1")),
                host(Source::GitLab, Err("HTTP 500")),
            ],
        ) {
            Discovery::NoAcceptable { detail } => {
                assert!(detail.contains("below"));
                assert!(detail.contains("HTTP 500"));
            }
            other => panic!("one valid below-floor read is not a failure, got {other:?}"),
        }
        match combine(
            Source::GitHub,
            [
                host(Source::GitHub, Ok("0.3.1")),
                host(Source::GitLab, Ok("0.3.4")),
            ],
        ) {
            Discovery::Offer { version, source } => {
                assert_eq!(version, "0.3.4");
                assert_eq!(source, Source::GitLab);
            }
            other => panic!("the acceptable host wins, got {other:?}"),
        }
        match combine(
            Source::GitHub,
            [
                host(Source::GitHub, Err("HTTP 404")),
                host(Source::GitLab, Err("invalid version name: v1")),
            ],
        ) {
            Discovery::Failed { detail } => {
                assert!(detail.contains("HTTP 404"));
                assert!(detail.contains("invalid version name"));
            }
            other => panic!("no valid version is a failure, got {other:?}"),
        }
    }

    fn hold_preferred() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poison| poison.into_inner())
    }

    fn version_doc(name: &str) -> Vec<u8> {
        format!("{{\"latestWinVersion\":{{\"versionName\":\"{name}\",\"versionCode\":1}}}}")
            .into_bytes()
    }

    fn scripted(
        github: std::result::Result<Vec<u8>, String>,
        gitlab: std::result::Result<Vec<u8>, String>,
    ) -> impl Fn(&str, usize) -> Result<Vec<u8>> + Sync {
        move |url: &str, _: usize| {
            let picked = if url == Source::GitHub.version_json_url() {
                &github
            } else if url == Source::GitLab.version_json_url() {
                &gitlab
            } else {
                return Err(Error::new(format!("unexpected url {url}")));
            };
            picked.clone().map_err(Error::new)
        }
    }

    #[test]
    fn discover_fetches_in_parallel_and_keeps_the_snapshot_on_a_tie() {
        let _guard = hold_preferred();
        let arrived = AtomicUsize::new(0);
        let fetch = |url: &str, _: usize| -> Result<Vec<u8>> {
            arrived.fetch_add(1, Ordering::SeqCst);
            let start = std::time::Instant::now();
            while arrived.load(Ordering::SeqCst) < 2 {
                if start.elapsed() > std::time::Duration::from_secs(2) {
                    return Err(Error::new("fetches were not in parallel"));
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            if url == Source::GitHub.version_json_url() || url == Source::GitLab.version_json_url()
            {
                Ok(version_doc("0.3.4"))
            } else {
                Err(Error::new(format!("unexpected url {url}")))
            }
        };
        for snapshot in [Source::GitLab, Source::GitHub] {
            arrived.store(0, Ordering::SeqCst);
            set_preferred(snapshot);
            match discover(&fetch) {
                Discovery::Offer { version, source } => {
                    assert_eq!(version, "0.3.4");
                    assert_eq!(source, snapshot);
                    assert_eq!(preferred(), snapshot);
                }
                other => panic!("tie should stay on {snapshot:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn checks_off_fetches_nothing_and_still_logs() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        set_checks_enabled(&layout, false).unwrap();
        let calls = AtomicUsize::new(0);
        let fetch = |_url: &str, _: usize| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::new("should not be called"))
        };
        let outcome = run_check_with(&layout, 42, &fetch);
        assert_eq!(outcome.kind, CheckKind::Disabled);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let log = std::fs::read_to_string(layout.check_log_path()).unwrap();
        assert!(log.contains("42 check kind=Disabled"));
        assert_eq!(log.lines().count(), 1);
    }

    #[test]
    fn run_check_classifies_and_logs_each_outcome() {
        let _guard = hold_preferred();
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let available = run_check_with(
            &layout,
            1,
            &scripted(Ok(version_doc("9.9.9")), Err("down".into())),
        );
        assert_eq!(available.kind, CheckKind::Available);
        assert_eq!(available.version.as_deref(), Some("9.9.9"));
        let same = run_check_with(
            &layout,
            2,
            &scripted(Ok(version_doc(current())), Err("down".into())),
        );
        assert_eq!(same.kind, CheckKind::AlreadyCurrent);
        assert_eq!(same.version.as_deref(), Some(current()));
        let below = run_check_with(
            &layout,
            3,
            &scripted(Ok(version_doc("0.3.1")), Err("HTTP 500".into())),
        );
        assert_eq!(below.kind, CheckKind::NoAcceptableUpdate);
        assert_eq!(below.version, None);
        assert_eq!(below.notice, "No acceptable update is available.");
        assert!(below.detail.contains("0.3.1 is below 0.3.2"));
        assert!(below.detail.contains("HTTP 500"));
        let failed = run_check_with(
            &layout,
            4,
            &scripted(Err("nope".into()), Err("also".into())),
        );
        assert_eq!(failed.kind, CheckKind::Failed);
        assert!(failed.notice.starts_with("Update check failed:"));
        assert!(failed.notice.contains("nope"));
        assert!(failed.notice.contains("also"));
        assert!(failed.version.is_none());
        let log = std::fs::read_to_string(layout.check_log_path()).unwrap();
        assert!(log.contains("kind=Available"));
        assert!(log.contains("kind=AlreadyCurrent"));
        assert!(log.contains("kind=NoAcceptableUpdate"));
        assert!(log.contains("0.3.1 is below 0.3.2"));
        assert!(log.contains("kind=Failed"));
        assert!(log.contains("nope"));
        assert_eq!(log.lines().count(), 4);
    }

    #[test]
    fn apply_choice_does_not_invent_a_version() {
        assert_eq!(
            apply_choice(&Latest::NoAcceptable),
            ApplyChoice::NoAcceptable
        );
        assert_eq!(
            apply_choice(&Latest::Version(current().to_owned())),
            ApplyChoice::AlreadyCurrent
        );
        assert_eq!(
            apply_choice(&Latest::Version("9.9.9".into())),
            ApplyChoice::Install("9.9.9".into())
        );
        assert_eq!(
            apply_choice(&Latest::Version("0.3.1".into())),
            ApplyChoice::AlreadyCurrent
        );
    }

    #[test]
    fn sidecar_reads_the_first_64_hex_bytes_only() {
        let hex = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789ABCDEF";
        let lower = hex.to_ascii_lowercase();
        assert_eq!(sidecar_hex(hex.as_bytes()).as_deref(), Some(lower.as_str()));
        let with_name = format!("{hex}  udm-9.9.9-portable.zip\n");
        assert_eq!(
            sidecar_hex(with_name.as_bytes()).as_deref(),
            Some(lower.as_str())
        );
        assert_eq!(sidecar_hex(hex[..63].as_bytes()), None);
        assert_eq!(
            sidecar_hex(format!(" {hex}").as_bytes()),
            None,
            "no skipped prefix"
        );
        let mut spaced = hex.as_bytes().to_vec();
        spaced[0] = b' ';
        assert_eq!(sidecar_hex(&spaced), None, "no trimming");
    }

    fn pair_for(version: &str) -> (Vec<u8>, Vec<u8>) {
        let zip = payload_zip(version);
        let side = format!("{}  {}\n", sha256_hex(&zip), zip_name(version)).into_bytes();
        (zip, side)
    }

    #[test]
    fn uppercase_sidecar_is_accepted_and_a_prefix_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let zip = full_zip();
        let upper = sha256_hex(&zip).to_ascii_uppercase();
        let sidecar = format!("{upper}  {}\n", zip_name("9.9.9")).into_bytes();
        let calls = Cell::new(0);
        let verify = |_: &Path| {
            calls.set(calls.get() + 1);
            Ok(())
        };
        accept_and_stage(&zip, &sidecar, "9.9.9", &layout, &verify).unwrap();
        assert_eq!(calls.get(), 3);
        let shifted = format!(" {upper}\n").into_bytes();
        let calls = Cell::new(0);
        let verify = |_: &Path| {
            calls.set(calls.get() + 1);
            Ok(())
        };
        assert_eq!(
            accept_and_stage(&zip, &shifted, "9.9.9", &layout, &verify).unwrap_err(),
            Failure::Mismatch
        );
        assert_eq!(
            calls.get(),
            0,
            "extraction and signature wait on the sidecar"
        );
    }

    #[test]
    fn floor_refusal_does_no_network_or_disk_work() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let calls = AtomicUsize::new(0);
        let fetch = |_url: &str, _: usize| {
            calls.fetch_add(1, Ordering::SeqCst);
            Err(Error::new("should not be called"))
        };
        for version in ["0.3.1", "v0.3.4", "0.3", " 0.3.4", "0.3.4 "] {
            let err = download_and_install_with(&layout, version, &fetch, &|_| Ok(())).unwrap_err();
            assert!(
                matches!(err, Failure::Download(_)),
                "{version} should be refused, got {err}"
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn host_fallback_stops_once_a_pair_is_complete() {
        let _guard = hold_preferred();
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let (zip, side) = pair_for("0.3.4");
        let hits = std::sync::Mutex::new(Vec::<String>::new());

        set_preferred(Source::GitHub);
        let fetch = |url: &str, _: usize| {
            hits.lock().unwrap().push(url.to_owned());
            if url.contains("github.com") && url.ends_with(".zip") {
                return Err(Error::new("github zip missing"));
            }
            if url.ends_with(".sha256") {
                return Ok(side.clone());
            }
            if url.contains("portable.zip") {
                return Ok(zip.clone());
            }
            Err(Error::new(format!("unexpected {url}")))
        };
        download_and_install_with(&layout, "0.3.4", &fetch, &|_| Ok(())).unwrap();
        let seen = hits.lock().unwrap().clone();
        assert!(seen.iter().any(|url| {
            url == "https://github.com/iamcino/udm/releases/download/v0.3.4/udm-0.3.4-portable.zip"
        }));
        assert!(seen.iter().any(|url| {
            url == "https://gitlab.com/cino-group/udm/-/releases/v0.3.4/downloads/udm-0.3.4-portable.zip"
        }));
        assert!(seen.iter().any(|url| {
            url == "https://gitlab.com/cino-group/udm/-/releases/v0.3.4/downloads/udm-0.3.4-portable.zip.sha256"
        }));
        assert_eq!(preferred(), Source::GitLab);
        assert_eq!(
            std::fs::read(dir.path().join("udm.exe")).unwrap(),
            b"MZ-new-gui"
        );

        hits.lock().unwrap().clear();
        set_preferred(Source::GitHub);
        let bad = b"0000000000000000000000000000000000000000000000000000000000000000\n".to_vec();
        let fetch = |url: &str, _: usize| {
            hits.lock().unwrap().push(url.to_owned());
            if url.contains("gitlab.com") {
                return Err(Error::new("second host must not be tried"));
            }
            if url.ends_with(".sha256") {
                return Ok(bad.clone());
            }
            Ok(zip.clone())
        };
        let err = download_and_install_with(&layout, "0.3.4", &fetch, &|_| {
            panic!("signature runs only after the sidecar matches");
        })
        .unwrap_err();
        assert_eq!(err, Failure::Mismatch);
        assert!(hits
            .lock()
            .unwrap()
            .iter()
            .all(|url| url.contains("github.com")));
        assert_eq!(preferred(), Source::GitHub);

        hits.lock().unwrap().clear();
        let fetch = |url: &str, _: usize| {
            hits.lock().unwrap().push(url.to_owned());
            if url.contains("gitlab.com") {
                return Err(Error::new("second host must not be tried"));
            }
            if url.ends_with(".sha256") {
                return Ok(side.clone());
            }
            Ok(zip.clone())
        };
        let verified = AtomicUsize::new(0);
        let verify = |_path: &Path| {
            verified.fetch_add(1, Ordering::SeqCst);
            Err(Failure::Signature)
        };
        let err = download_and_install_with(&layout, "0.3.4", &fetch, &verify).unwrap_err();
        assert_eq!(err, Failure::Signature);
        assert!(verified.load(Ordering::SeqCst) > 0);
        assert!(hits
            .lock()
            .unwrap()
            .iter()
            .all(|url| url.contains("github.com")));
    }

    #[test]
    fn hashes() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn outcome_classifies() {
        assert_eq!(outcome_for("999.0.0").kind, CheckKind::Available);
        assert_eq!(outcome_for("999.0.0").version.as_deref(), Some("999.0.0"));
        assert_eq!(outcome_for("0.0.1").kind, CheckKind::AlreadyCurrent);
        assert_eq!(outcome_for(current()).kind, CheckKind::AlreadyCurrent);
    }

    /// The SPKI DER of the shipped UDM signing certificate (from the
    /// published `UDM-codesign.cer` of release v0.2.3; the signer
    /// embedded in the shipped `udm.exe` is byte-identical).
    const SHIPPED_SPKI_DER_HEX: &str = "3059301306072a8648ce3d020106082a8648ce3d03010703420004ee0e1c835ee768e6e2085a35195ddbb8f9963b76f2e036d9e853ce80b5963da554de42c1fc20e5ad3f484335d1a81a09b031e56954cc27ba6167cc7894f8dd06";

    #[test]
    fn pin_is_full_der_spki_hash() {
        let der = hex_decode(SHIPPED_SPKI_DER_HEX);
        assert_eq!(der.len(), 91);
        assert_eq!(sha256_hex(&der), PINNED_SPKI_SHA256_HEX);
        assert!(spki_pin_matches(&der));
    }

    #[test]
    fn pin_compares_key_not_certificate() {
        let mut der = hex_decode(SHIPPED_SPKI_DER_HEX);
        assert!(spki_pin_matches(&der));
        // One flipped bit in the key material: no longer the pinned key.
        let last = der.len() - 1;
        der[last] ^= 0x01;
        assert!(!spki_pin_matches(&der));
        // A different (valid-shaped) SPKI of another key: rejected.
        let other = hex_decode(
            "3059301306072a8648ce3d020106082a8648ce3d0301070342000400000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001",
        );
        assert!(!spki_pin_matches(&other));
    }

    /// Build a stored (method 0) zip with the given members.
    fn build_zip(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut central = Vec::new();
        for (name, bytes) in members {
            let offset = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(bytes);
            central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            central.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&offset.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_offset = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&(members.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_offset.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn sums_for(zip: &[u8], version: &str) -> Vec<u8> {
        format!("{}  {}\n", sha256_hex(zip), zip_name(version)).into_bytes()
    }

    fn payload_zip(version: &str) -> Vec<u8> {
        let firefox = firefox_archive_name(version);
        build_zip(&[
            ("udm.exe", b"MZ-new-gui"),
            ("udm-cli.exe", b"MZ-new-cli"),
            ("udm-host.exe", b"MZ-new-host"),
            ("udm.portable", b"portable"),
            (firefox.as_str(), b"PK-firefox"),
            ("extension/chrome/handoff.js", b"// handoff"),
            ("README.txt", b"readme"),
        ])
    }

    fn full_zip() -> Vec<u8> {
        payload_zip("9.9.9")
    }

    #[test]
    fn gate_checks_sums_before_signature() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let zip = full_zip();
        let bad_sums = b"0000000000000000000000000000000000000000000000000000000000000000  udm-9.9.9-portable.zip\n".to_vec();
        let calls = Cell::new(0);
        let verify = |_: &Path| -> std::result::Result<(), Failure> {
            calls.set(calls.get() + 1);
            Ok(())
        };
        let err = accept_and_stage(&zip, &bad_sums, "9.9.9", &layout, &verify).unwrap_err();
        assert_eq!(err, Failure::Mismatch);
        assert_eq!(
            calls.get(),
            0,
            "signature gate must not run on a hash mismatch"
        );
        assert!(!staging_present(layout.base()));
    }

    #[test]
    fn gate_rejects_tampered_zip_with_recomputed_sums() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let zip = full_zip();
        let sums = sums_for(&zip, "9.9.9");
        let err = accept_and_stage(&zip, &sums, "9.9.9", &layout, &|_| Err(Failure::Signature))
            .unwrap_err();
        assert_eq!(err, Failure::Signature);
        assert!(!staging_present(layout.base()));
    }

    #[test]
    fn gate_accepts_and_stages_the_tree() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let zip = full_zip();
        let sums = sums_for(&zip, "9.9.9");
        let calls = Cell::new(0);
        let verify = |_: &Path| -> std::result::Result<(), Failure> {
            calls.set(calls.get() + 1);
            Ok(())
        };
        let staging = accept_and_stage(&zip, &sums, "9.9.9", &layout, &verify).unwrap();
        assert_eq!(calls.get(), 3, "every payload exe is verified");
        assert_eq!(
            std::fs::read(staging.dir().join("udm.exe")).unwrap(),
            b"MZ-new-gui"
        );
        assert_eq!(
            std::fs::read(staging.dir().join("extension/chrome/handoff.js")).unwrap(),
            b"// handoff"
        );
        assert_eq!(
            std::fs::read(staging.dir().join("udm.portable")).unwrap(),
            b"portable"
        );
        assert_eq!(
            std::fs::read(staging.dir().join(firefox_archive_name("9.9.9"))).unwrap(),
            b"PK-firefox"
        );
    }

    #[test]
    fn gate_rejects_missing_exe_unsafe_path_and_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let ok = |_: &Path| -> std::result::Result<(), Failure> { Ok(()) };
        // Missing udm-host.exe.
        let zip = build_zip(&[("udm.exe", b"MZ"), ("udm-cli.exe", b"MZ")]);
        let sums = sums_for(&zip, "9.9.9");
        assert_eq!(
            accept_and_stage(&zip, &sums, "9.9.9", &layout, &ok).unwrap_err(),
            Failure::Mismatch
        );
        // Unsafe member path.
        let zip = build_zip(&[
            ("udm.exe", b"MZ"),
            ("udm-cli.exe", b"MZ"),
            ("udm-host.exe", b"MZ"),
            ("../evil.txt", b"x"),
        ]);
        let sums = sums_for(&zip, "9.9.9");
        assert_eq!(
            accept_and_stage(&zip, &sums, "9.9.9", &layout, &ok).unwrap_err(),
            Failure::Mismatch
        );
        // Duplicate member.
        let zip = build_zip(&[
            ("udm.exe", b"MZ"),
            ("udm.exe", b"MZ2"),
            ("udm-cli.exe", b"MZ"),
            ("udm-host.exe", b"MZ"),
        ]);
        let sums = sums_for(&zip, "9.9.9");
        assert_eq!(
            accept_and_stage(&zip, &sums, "9.9.9", &layout, &ok).unwrap_err(),
            Failure::Mismatch
        );
        // Backslash member (the 0.2.1 payload defect class).
        let zip = build_zip(&[
            ("udm.exe", b"MZ"),
            ("udm-cli.exe", b"MZ"),
            ("udm-host.exe", b"MZ"),
            ("extension\\chrome\\handoff.js", b"x"),
        ]);
        let sums = sums_for(&zip, "9.9.9");
        assert_eq!(
            accept_and_stage(&zip, &sums, "9.9.9", &layout, &ok).unwrap_err(),
            Failure::Mismatch
        );
        // Three executables, but no portable marker and no Firefox archive.
        let zip = build_zip(&[
            ("udm.exe", b"MZ"),
            ("udm-cli.exe", b"MZ"),
            ("udm-host.exe", b"MZ"),
        ]);
        let sums = sums_for(&zip, "9.9.9");
        assert_eq!(
            accept_and_stage(&zip, &sums, "9.9.9", &layout, &ok).unwrap_err(),
            Failure::Mismatch
        );
        // Marker present, Firefox archive named for a different version.
        let zip = build_zip(&[
            ("udm.exe", b"MZ"),
            ("udm-cli.exe", b"MZ"),
            ("udm-host.exe", b"MZ"),
            ("udm.portable", b"portable"),
            ("extension/udm-extension-firefox-0.3.2.zip", b"PK"),
        ]);
        let sums = sums_for(&zip, "9.9.9");
        assert_eq!(
            accept_and_stage(&zip, &sums, "9.9.9", &layout, &ok).unwrap_err(),
            Failure::Mismatch
        );
        // The target archive plus a second matching Firefox archive.
        let zip = build_zip(&[
            ("udm.exe", b"MZ"),
            ("udm-cli.exe", b"MZ"),
            ("udm-host.exe", b"MZ"),
            ("udm.portable", b"portable"),
            ("extension/udm-extension-firefox-9.9.9.zip", b"PK"),
            ("extension/udm-extension-firefox-0.3.2.zip", b"PK-old"),
        ]);
        let sums = sums_for(&zip, "9.9.9");
        assert_eq!(
            accept_and_stage(&zip, &sums, "9.9.9", &layout, &ok).unwrap_err(),
            Failure::Mismatch
        );
        // Marker not at the root.
        let zip = build_zip(&[
            ("udm.exe", b"MZ"),
            ("udm-cli.exe", b"MZ"),
            ("udm-host.exe", b"MZ"),
            ("sub/udm.portable", b"portable"),
            ("extension/udm-extension-firefox-9.9.9.zip", b"PK"),
        ]);
        let sums = sums_for(&zip, "9.9.9");
        assert_eq!(
            accept_and_stage(&zip, &sums, "9.9.9", &layout, &ok).unwrap_err(),
            Failure::Mismatch
        );
        // A payload that otherwise qualifies, with a duplicate member and an unsafe path.
        let zip = build_zip(&[
            ("udm.exe", b"MZ"),
            ("udm-cli.exe", b"MZ"),
            ("udm-host.exe", b"MZ"),
            ("udm.portable", b"portable"),
            ("extension/udm-extension-firefox-9.9.9.zip", b"PK"),
            ("extension/udm-extension-firefox-9.9.9.zip", b"PK2"),
        ]);
        let sums = sums_for(&zip, "9.9.9");
        assert_eq!(
            accept_and_stage(&zip, &sums, "9.9.9", &layout, &ok).unwrap_err(),
            Failure::Mismatch
        );
        let zip = build_zip(&[
            ("udm.exe", b"MZ"),
            ("udm-cli.exe", b"MZ"),
            ("udm-host.exe", b"MZ"),
            ("udm.portable", b"portable"),
            ("extension/udm-extension-firefox-9.9.9.zip", b"PK"),
            ("../evil.txt", b"x"),
        ]);
        let sums = sums_for(&zip, "9.9.9");
        assert_eq!(
            accept_and_stage(&zip, &sums, "9.9.9", &layout, &ok).unwrap_err(),
            Failure::Mismatch
        );
    }

    #[test]
    fn swap_replaces_files_and_keeps_backups() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        std::fs::write(dir.path().join("udm.exe"), b"old-gui").unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"keep").unwrap();
        let staging = dir.path().join("scratch-staging");
        std::fs::create_dir_all(staging.join("sub")).unwrap();
        std::fs::write(staging.join("udm.exe"), b"new-gui").unwrap();
        std::fs::write(staging.join("sub/new.txt"), b"new-file").unwrap();
        swap_tree(&layout, &staging, &mut Vec::new()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("udm.exe")).unwrap(),
            b"new-gui"
        );
        assert_eq!(
            std::fs::read(dir.path().join("udm.exe.old")).unwrap(),
            b"old-gui"
        );
        assert_eq!(
            std::fs::read(dir.path().join("sub/new.txt")).unwrap(),
            b"new-file"
        );
        assert_eq!(std::fs::read(dir.path().join("keep.txt")).unwrap(), b"keep");
        assert!(!dir.path().join("sub/new.txt.old").exists());
    }

    #[test]
    fn swap_rolls_back_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        std::fs::write(dir.path().join("a.txt"), b"old-a").unwrap();
        // `sub` exists as a FILE, so staging `sub/c.txt` cannot land.
        std::fs::write(dir.path().join("sub"), b"blocker").unwrap();
        let staging = dir.path().join("scratch-staging");
        std::fs::create_dir_all(staging.join("sub")).unwrap();
        std::fs::write(staging.join("a.txt"), b"new-a").unwrap();
        std::fs::write(staging.join("sub/c.txt"), b"new-c").unwrap();
        let err = swap_tree(&layout, &staging, &mut Vec::new()).unwrap_err();
        assert!(
            matches!(err, Failure::Install { restored: true, .. }),
            "got {err:?}"
        );
        assert!(err.message().contains("still in place"));
        assert_eq!(std::fs::read(dir.path().join("a.txt")).unwrap(), b"old-a");
        assert!(!dir.path().join("a.txt.old").exists());
    }

    #[test]
    fn cleanup_removes_old_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("udm.exe.old"), b"x").unwrap();
        std::fs::write(dir.path().join("udm.exe"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/f.js.old"), b"x").unwrap();
        assert!(remove_old_files(dir.path()));
        assert!(!dir.path().join("udm.exe.old").exists());
        assert!(!dir.path().join("sub/f.js.old").exists());
        assert!(dir.path().join("udm.exe").exists());
    }

    #[test]
    fn checks_flag_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        assert!(checks_enabled(&layout), "absent flag means ON");
        set_checks_enabled(&layout, false).unwrap();
        assert!(!checks_enabled(&layout));
        set_checks_enabled(&layout, true).unwrap();
        assert!(checks_enabled(&layout));
        std::fs::write(dir.path().join(CHECKS_FLAG_FILE), b"anything\n").unwrap();
        assert!(checks_enabled(&layout), "any other content means ON");
    }

    /// Real-artifact gate evidence, run on demand against a shipped
    /// signed exe: `UDM_TEST_SIGNED_EXE=<path to shipped udm.exe>`.
    /// `udm-cli.exe` and `udm-host.exe` beside it are used when present.
    /// A byte-flipped executable with a recomputed sidecar must fail the
    /// signature gate, not the checksum.
    #[cfg(windows)]
    #[test]
    fn real_artifact_signed_accepts_and_tampered_rejects() {
        let Ok(path) = std::env::var("UDM_TEST_SIGNED_EXE") else {
            return;
        };
        let path = PathBuf::from(path);
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("good.exe");
        std::fs::copy(&path, &good).unwrap();
        http::verify_file_signature(&good).unwrap();
        let mut bytes = std::fs::read(&path).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0x01;
        let bad = dir.path().join("bad.exe");
        std::fs::write(&bad, &bytes).unwrap();
        assert_eq!(
            http::verify_file_signature(&bad).unwrap_err(),
            Failure::Signature
        );

        let parent = path.parent().unwrap();
        let gui = std::fs::read(&path).unwrap();
        let cli = std::fs::read(parent.join("udm-cli.exe")).unwrap_or_else(|_| gui.clone());
        let host = std::fs::read(parent.join("udm-host.exe")).unwrap_or_else(|_| gui.clone());
        let firefox = firefox_archive_name("9.9.9");
        let zip = build_zip(&[
            ("udm.exe", &gui),
            ("udm-cli.exe", &cli),
            ("udm-host.exe", &host),
            ("udm.portable", b"portable"),
            (firefox.as_str(), b"PK-firefox"),
        ]);
        let sums = sums_for(&zip, "9.9.9");
        let install = dir.path().join("install");
        std::fs::create_dir_all(&install).unwrap();
        let layout = InstallLayout::new(&install);
        stage(
            &zip,
            &sums,
            "9.9.9",
            &layout,
            &http::verify_file_signature,
            true,
        )
        .unwrap();

        let mut tampered = gui.clone();
        tampered[mid] ^= 0x01;
        let bad_zip = build_zip(&[
            ("udm.exe", tampered.as_slice()),
            ("udm-cli.exe", &cli),
            ("udm-host.exe", &host),
            ("udm.portable", b"portable"),
            (firefox.as_str(), b"PK-firefox"),
        ]);
        let bad_sums = sums_for(&bad_zip, "9.9.9");
        assert_eq!(
            stage(
                &bad_zip,
                &bad_sums,
                "9.9.9",
                &layout,
                &http::verify_file_signature,
                true,
            )
            .unwrap_err(),
            Failure::Signature
        );
    }

    #[test]
    fn cleanup_retries_match_easykey() {
        assert_eq!(CLEANUP_ATTEMPTS, 120);
        assert_eq!(CLEANUP_DELAY_MS, 500);
    }

    #[test]
    fn staging_names_are_exclusive_and_unpredictable() {
        let dir = tempfile::tempdir().unwrap();
        let first = create_staging(dir.path()).unwrap();
        let second = create_staging(dir.path()).unwrap();
        assert_ne!(first.file_name(), second.file_name());
        assert!(is_staging_name(first.file_name().unwrap()));
        assert!(is_staging_name(second.file_name().unwrap()));
        let stale = dir.path().join("update-staging-ee");
        std::fs::create_dir(&stale).unwrap();
        std::fs::write(stale.join("x"), b"1").unwrap();
        std::fs::write(dir.path().join("update-staging-abcd"), b"no").unwrap();
        std::fs::create_dir(dir.path().join("notes")).unwrap();
        release_staging(&first);
        release_staging(&second);
        assert!(cleanup_once(&InstallLayout::new(dir.path())));
        assert!(!stale.exists());
        assert!(dir.path().join("update-staging-abcd").is_file());
        assert!(dir.path().join("notes").is_dir());
        assert!(dir.path().read_dir().unwrap().flatten().all(|entry| {
            !(entry.file_type().unwrap().is_dir() && is_staging_name(&entry.file_name()))
        }));
    }

    #[test]
    fn cleanup_skips_an_active_staging_directory() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let zip = full_zip();
        let sums = sums_for(&zip, "9.9.9");
        let layout_verify = layout.clone();
        let verify = move |_path: &Path| {
            assert!(cleanup_once(&layout_verify));
            Ok(())
        };
        let staged = accept_and_stage(&zip, &sums, "9.9.9", &layout, &verify).unwrap();
        let path = staged.dir().to_path_buf();
        assert!(path.join("udm.exe").is_file());
        assert!(cleanup_once(&layout));
        assert!(path.exists());
        drop(staged);
        assert!(!path.exists());
    }

    #[test]
    fn unrestored_swap_does_not_promise_the_previous_build() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("udm.exe");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("stuck"), b"x").unwrap();
        let backup = dir.path().join("udm.exe.old");
        assert!(!undo(&[(target, backup, true)]));
        let message = install_err("rename failed", false).message();
        assert!(message.contains("restoring the previous build failed"));
        assert!(!message.contains("still in place"));
    }

    /// The next patch after the running build, used as the install
    /// target in sweep tests so no fixture pins a release number.
    fn next_patch_version() -> String {
        let mut parts = VERSION.split('.');
        let major: u32 = parts.next().unwrap().parse().unwrap();
        let minor: u32 = parts.next().unwrap().parse().unwrap();
        let patch: u32 = parts.next().unwrap().parse().unwrap();
        format!("{major}.{minor}.{}", patch + 1)
    }

    fn seed_firefox(dir: &Path, name: &str, bytes: &[u8]) {
        let ext = dir.join("extension");
        std::fs::create_dir_all(&ext).unwrap();
        std::fs::write(ext.join(name), bytes).unwrap();
    }

    #[test]
    fn startup_cleanup_keeps_the_running_version() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        std::fs::write(dir.path().join(CHECKS_FLAG_FILE), b"off\n").unwrap();
        seed_firefox(
            dir.path(),
            &format!("udm-extension-firefox-{VERSION}.zip"),
            b"running",
        );
        seed_firefox(dir.path(), "udm-extension-firefox-0.0.1.zip", b"old");
        seed_firefox(dir.path(), "udm-extension-firefox-0.0.2.zip", b"newer");
        seed_firefox(dir.path(), "udm-extension-chrome.zip", b"chrome");
        let nested = dir.path().join("extension").join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("udm-extension-firefox-0.3.3.zip"), b"nested").unwrap();
        std::fs::create_dir(
            dir.path()
                .join("extension")
                .join("udm-extension-firefox-9.9.9.zip"),
        )
        .unwrap();
        assert!(cleanup_once(&layout));
        let ext = dir.path().join("extension");
        assert_eq!(
            std::fs::read(ext.join(format!("udm-extension-firefox-{VERSION}.zip"))).unwrap(),
            b"running"
        );
        assert!(!ext.join("udm-extension-firefox-0.0.1.zip").exists());
        assert!(!ext.join("udm-extension-firefox-0.0.2.zip").exists());
        assert_eq!(
            std::fs::read(ext.join("udm-extension-chrome.zip")).unwrap(),
            b"chrome"
        );
        assert_eq!(
            std::fs::read(nested.join("udm-extension-firefox-0.3.3.zip")).unwrap(),
            b"nested"
        );
        assert!(ext.join("udm-extension-firefox-9.9.9.zip").is_dir());
    }

    #[test]
    fn successful_swap_sweeps_older_firefox_archives_only() {
        let _guard = hold_preferred();
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        seed_firefox(dir.path(), "udm-extension-firefox-0.3.2.zip", b"old");
        seed_firefox(dir.path(), "udm-extension-firefox-0.3.3.zip", b"mid");
        seed_firefox(dir.path(), "udm-extension-firefox-0.3.4.zip", b"pre");
        seed_firefox(dir.path(), "udm-extension-chrome.zip", b"chrome");
        let ext = dir.path().join("extension");
        let nested = ext.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("udm-extension-firefox-0.3.2.zip"), b"nested").unwrap();
        std::fs::create_dir(ext.join("udm-extension-firefox-1.0.0.zip")).unwrap();
        std::fs::write(ext.join("chrome").join("keep.js"), b"keep").unwrap_or_else(|_| {
            std::fs::create_dir_all(ext.join("chrome")).unwrap();
            std::fs::write(ext.join("chrome").join("keep.js"), b"keep").unwrap();
        });
        let target = next_patch_version();
        let (zip, side) = pair_for(&target);
        let fetch = |url: &str, _: usize| {
            if url.ends_with(".sha256") {
                Ok(side.clone())
            } else if url.contains("portable.zip") {
                Ok(zip.clone())
            } else {
                Err(Error::new(format!("unexpected {url}")))
            }
        };
        download_and_install_with(&layout, &target, &fetch, &|_| Ok(())).unwrap();
        assert_eq!(
            std::fs::read(ext.join(format!("udm-extension-firefox-{target}.zip"))).unwrap(),
            b"PK-firefox"
        );
        assert!(!ext.join("udm-extension-firefox-0.3.2.zip").exists());
        assert!(!ext.join("udm-extension-firefox-0.3.3.zip").exists());
        assert_eq!(
            std::fs::read(ext.join("udm-extension-chrome.zip")).unwrap(),
            b"chrome"
        );
        assert_eq!(
            std::fs::read(nested.join("udm-extension-firefox-0.3.2.zip")).unwrap(),
            b"nested"
        );
        assert!(ext.join("udm-extension-firefox-1.0.0.zip").is_dir());
        assert_eq!(
            std::fs::read(ext.join("chrome").join("keep.js")).unwrap(),
            b"keep"
        );
        // The target is derived from the running VERSION (next patch),
        // so this test survives version bumps; the sweep keys on the target.
    }

    #[test]
    fn failed_gate_and_rolled_back_swap_do_not_sweep() {
        let _guard = hold_preferred();
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        seed_firefox(dir.path(), "udm-extension-firefox-0.3.2.zip", b"old");
        seed_firefox(dir.path(), "udm-extension-firefox-0.3.3.zip", b"mid");
        let (zip, side) = pair_for("0.3.4");
        let bad = b"0000000000000000000000000000000000000000000000000000000000000000\n".to_vec();
        let fetch = |url: &str, _: usize| {
            if url.ends_with(".sha256") {
                Ok(bad.clone())
            } else if url.contains("portable.zip") {
                Ok(zip.clone())
            } else {
                Err(Error::new(format!("unexpected {url}")))
            }
        };
        assert_eq!(
            download_and_install_with(&layout, "0.3.4", &fetch, &|_| Ok(())).unwrap_err(),
            Failure::Mismatch
        );
        let ext = dir.path().join("extension");
        assert_eq!(
            std::fs::read(ext.join("udm-extension-firefox-0.3.2.zip")).unwrap(),
            b"old"
        );
        assert_eq!(
            std::fs::read(ext.join("udm-extension-firefox-0.3.3.zip")).unwrap(),
            b"mid"
        );

        let backup = dir.path().join("udm.exe.old");
        std::fs::create_dir(&backup).unwrap();
        std::fs::write(backup.join("block"), b"x").unwrap();
        let fetch = |url: &str, _: usize| {
            if url.ends_with(".sha256") {
                Ok(side.clone())
            } else if url.contains("portable.zip") {
                Ok(zip.clone())
            } else {
                Err(Error::new(format!("unexpected {url}")))
            }
        };
        let err = download_and_install_with(&layout, "0.3.4", &fetch, &|_| Ok(())).unwrap_err();
        assert!(
            matches!(err, Failure::Install { restored: true, .. }),
            "got {err}"
        );
        assert_eq!(
            std::fs::read(ext.join("udm-extension-firefox-0.3.2.zip")).unwrap(),
            b"old"
        );
        assert_eq!(
            std::fs::read(ext.join("udm-extension-firefox-0.3.3.zip")).unwrap(),
            b"mid"
        );
        assert!(!ext.join("udm-extension-firefox-0.3.4.zip").exists());
    }

    #[test]
    fn gui_and_cli_cleanup_do_not_block_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        std::fs::write(dir.path().join("udm.exe.old"), b"x").unwrap();
        let started = std::time::Instant::now();
        finish_previous_update(&layout);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        for _ in 0..50 {
            if !dir.path().join("udm.exe.old").exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(!dir.path().join("udm.exe.old").exists());

        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        std::fs::write(dir.path().join("udm-cli.exe.old"), b"x").unwrap();
        finish_previous_update_for_cli(&layout);
        assert!(!dir.path().join("udm-cli.exe.old").exists());
    }

    #[cfg(windows)]
    #[test]
    fn held_handle_denies_write_and_delete_through_rename() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        let zip = full_zip();
        let sums = sums_for(&zip, "9.9.9");
        let saw = Cell::new(0);
        let verify = |path: &Path| {
            assert!(winfile::write_denied(path), "write during verification");
            assert!(winfile::delete_denied(path), "delete during verification");
            saw.set(saw.get() + 1);
            Ok(())
        };
        let mut staged = accept_and_stage(&zip, &sums, "9.9.9", &layout, &verify).unwrap();
        assert_eq!(saw.get(), 3);
        let staging_dir = staged.dir.clone();
        swap_tree(&layout, &staging_dir, &mut staged.exes).unwrap();
        let installed = dir.path().join("udm.exe");
        assert!(winfile::write_denied(&installed), "write after rename");
        assert!(winfile::delete_denied(&installed), "delete after rename");
        let held = staged
            .exes
            .iter()
            .find(|exe| exe.path.ends_with("udm.exe"))
            .unwrap();
        assert!(winfile::indexes_match(held.handle, &installed).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn locked_old_file_is_retried_without_blocking_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("udm.exe.old");
        std::fs::write(&path, b"locked").unwrap();
        let lock = winfile::lock_exclusive(&path).unwrap();
        let layout = InstallLayout::new(dir.path());
        assert!(!cleanup_once(&layout));
        assert!(path.exists());

        let started = std::time::Instant::now();
        finish_previous_update(&layout);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert!(path.exists());
        drop(lock);
        let mut gone = false;
        for _ in 0..40 {
            if !path.exists() {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(gone, "GUI retry did not remove the unlocked backup");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("udm-host.exe.old");
        std::fs::write(&path, b"locked").unwrap();
        let lock = winfile::lock_exclusive(&path).unwrap();
        let layout = InstallLayout::new(dir.path());
        let started = std::time::Instant::now();
        finish_previous_update_for_cli(&layout);
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        assert!(path.exists(), "the first pass leaves a locked file");
        drop(lock);
        let mut gone = false;
        for _ in 0..40 {
            if !path.exists() {
                gone = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        assert!(gone, "CLI retry did not remove the unlocked backup");
    }
}

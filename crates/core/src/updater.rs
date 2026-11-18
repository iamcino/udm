//! Updates the install in place from the project's releases, the way
//! Yeager does (owner direction 2026-11-18: replace the signed-manifest
//! updater wholesale; no manifest, no epoch, no version floor).
//!
//! Every release is published on GitHub and on GitLab. A check asks both
//! sites at once for the version their "latest release" link redirects
//! to; the newest version either of them has wins, and if one site cannot
//! be reached the other one is enough. A check stores nothing: it is a
//! pure read, so checking again always gives the same answer, and a
//! version that is not newer than the running one is simply
//! "already current".
//!
//! Nothing is installed without asking. When the user applies, the
//! portable zip and `SHA256SUMS.txt` are downloaded from the site the
//! check found the version on (or the other one if that fails), and the
//! acceptance gate is fixed, identical for both origins: verify the
//! zip's SHA-256 against `SHA256SUMS.txt`, extract it, then verify the
//! Authenticode signature of every executable in it with
//! `WinVerifyTrust` and compare the signer certificate's SPKI hash
//! (SHA-256 over the full DER encoding of the SubjectPublicKeyInfo)
//! against the pinned key below. The signing certificate is
//! self-issued, so chain-to-public-root trust is not the anchor; the
//! anchor is a cryptographically valid signature plus the SPKI pin,
//! with a mandatory RFC 3161 timestamp (validity is judged at signing
//! time, so a countersigned binary stays acceptable after the signing
//! certificate expires). Anything else fails closed. A tampered zip
//! served with recomputed sums still faces the signature check.
//!
//! The swap follows Yeager's rename pattern per file: the staged file
//! takes the target's place after the current file steps aside to a
//! `.old` sibling; any failure rolls the swapped files back. The `.old`
//! files and the staging directory are deleted on the next start
//! ([`finish_previous_update`]).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

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
    const ALL: [Source; 2] = [Source::GitHub, Source::GitLab];

    /// Redirects to the newest release's page.
    fn latest_url(self) -> &'static str {
        match self {
            Source::GitHub => "https://github.com/iamcino/udm/releases/latest",
            Source::GitLab => "https://gitlab.com/cino-group/udm/-/releases/permalink/latest",
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

/// Whether `latest` is newer than this build.
pub fn is_newer(latest: &str) -> bool {
    match (parse_version(latest), parse_version(current())) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

/// The version in a release page URL: `.../releases/tag/v1.2.3` on
/// GitHub, `.../-/releases/v1.2.3` on GitLab.
fn version_from_tag_url(url: &str) -> Option<String> {
    let url = url.trim();
    let tag = url
        .rsplit_once("/tag/")
        .or_else(|| url.rsplit_once("/-/releases/"))?
        .1;
    let tag = tag.split(['?', '#']).next()?;
    parse_version(tag).map(|(a, b, c)| format!("{a}.{b}.{c}"))
}

/// The checksum of `name` in a `sha256sum`-style list: `<64 hex>  <name>`
/// per line.
fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|line| {
        let (hash, file) = line.trim().split_once(char::is_whitespace)?;
        let file = file.trim().trim_start_matches('*');
        (file.eq_ignore_ascii_case(name)
            && hash.len() == 64
            && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
    })
}

fn sha256_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn latest_from(source: Source) -> Result<String> {
    let location = http::redirect_target(source.latest_url())?;
    version_from_tag_url(&location)
        .ok_or_else(|| Error::new("The release page returned an invalid version."))
}

/// The newest of the versions each site reports, and the site it came
/// from (GitHub when both have it). Errors only when neither site
/// answered.
fn newest(results: [(Source, Result<String>); 2]) -> Result<(String, Source)> {
    let mut best: Option<(String, Source)> = None;
    let mut error = None;
    for (source, result) in results {
        match result {
            Ok(version) => {
                if best
                    .as_ref()
                    .is_none_or(|(b, _)| parse_version(&version) > parse_version(b))
                {
                    best = Some((version, source));
                }
            }
            Err(e) => error = error.or(Some(e)),
        }
    }
    best.ok_or_else(|| error.unwrap_or_else(|| Error::new("no release site")))
}

/// Asks GitHub and GitLab for the latest release, at the same time.
/// Blocking; run it off the UI thread.
pub fn latest() -> Result<String> {
    let [github, gitlab] = std::thread::scope(|s| {
        let jobs = Source::ALL.map(|source| s.spawn(move || latest_from(source)));
        jobs.map(|job| {
            job.join()
                .unwrap_or_else(|_| Err(Error::new("update check failed")))
        })
    });
    let (version, source) = newest([(Source::GitHub, github), (Source::GitLab, gitlab)])?;
    set_preferred(source);
    Ok(version)
}

// ---------------------------------------------------------------------------
// Install layout, checks flag, check outcome
// ---------------------------------------------------------------------------

/// Staging directory on the install base: a verified payload waits here
/// between the acceptance gate and the swap. It is not the live install.
pub const STAGING_DIR: &str = "update-staging";

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

    /// Where a verified payload is staged before the swap.
    pub fn staging_dir(&self) -> PathBuf {
        self.base.join(STAGING_DIR)
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
    /// Anything else: transport, HTTP, local I/O.
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
    /// The published version, when one was read.
    pub version: Option<String>,
}

fn failed(reason: String) -> CheckOutcome {
    CheckOutcome {
        kind: CheckKind::Failed,
        notice: format!("Update check failed: {reason}"),
        version: None,
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
        }
    } else {
        CheckOutcome {
            kind: CheckKind::AlreadyCurrent,
            notice: format!("Already current: version {VERSION} is the latest published version."),
            version: Some(latest.to_owned()),
        }
    }
}

/// Append the local log line every outcome writes (best effort: a log
/// failure never changes the outcome).
fn log_check(layout: &InstallLayout, now_unix: i64, outcome: &CheckOutcome) {
    use std::io::Write as _;
    let headline = outcome.notice.lines().next().unwrap_or("");
    let line = format!("{now_unix} check kind={:?} {headline}\n", outcome.kind);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(layout.check_log_path())
    {
        let _ = file.write_all(line.as_bytes());
    }
}

/// Run one update check for `layout`. While checks resolve OFF no check
/// starts: no socket opens and the outcome is [`CheckKind::Disabled`].
/// A check already in flight when the user turns checks off runs to
/// completion. Nothing here downloads or installs; applying stays a
/// separate, explicit act. Blocking; run it off the UI thread.
pub fn run_check(layout: &InstallLayout, now_unix: i64) -> CheckOutcome {
    let outcome = if !checks_enabled(layout) {
        CheckOutcome {
            kind: CheckKind::Disabled,
            notice: "Update checks are disabled.".to_owned(),
            version: None,
        }
    } else {
        match latest() {
            Ok(version) => outcome_for(&version),
            Err(err) => failed(format!("{err}")),
        }
    };
    log_check(layout, now_unix, &outcome);
    outcome
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
    /// The archive's hash does not match the release listing, the
    /// archive is not a usable zip, a member path is unsafe, or a
    /// payload executable is missing: the bytes are not the published
    /// build. Kept distinct from [`Failure::Signature`]: a checksum
    /// mismatch means a broken or substituted download, a signature
    /// failure means the file must not be trusted even though its hash
    /// matches the release listing.
    #[error("the downloaded archive does not match the published checksums")]
    Mismatch,
    /// The Authenticode/SPKI gate refused a payload executable:
    /// unsigned, untrusted signature, SPKI mismatch, or a verification
    /// API error.
    #[error("the downloaded build is not signed with the pinned key")]
    Signature,
    /// The verified build could not be put in place (the previous
    /// build was restored).
    #[error("install failed: {0}")]
    Install(Error),
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
            Failure::Install(e) => format!(
                "The update could not be installed ({e}); the previous build is still in place."
            ),
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

fn download(
    source: Source,
    version: &str,
    layout: &InstallLayout,
) -> std::result::Result<PathBuf, Failure> {
    let base = source.download_base(version);
    let sums = http::get(&format!("{base}SHA256SUMS.txt"), 64 * 1024).map_err(Failure::Download)?;
    let zip = http::get(&format!("{base}{}", zip_name(version)), MAX_DOWNLOAD)
        .map_err(Failure::Download)?;
    accept_and_stage(&zip, &sums, version, layout, &http::verify_file_signature)
}

/// The acceptance gate, identical for both origins: verify the zip's
/// SHA-256 against `SHA256SUMS.txt`, extract it into the staging
/// directory, then hand every payload executable to `verify` (the
/// Authenticode + SPKI step). `verify` is only called when the hash
/// already matches, so a tampered zip served with recomputed sums
/// still faces the signature check.
///
/// `verify` is a parameter (rather than a direct call) so tests can
/// drive the gate with fixture outcomes; production passes
/// [`http::verify_file_signature`]. On success the staging directory
/// holds the verified tree.
fn accept_and_stage(
    zip: &[u8],
    sums: &[u8],
    version: &str,
    layout: &InstallLayout,
    verify: &dyn Fn(&Path) -> std::result::Result<(), Failure>,
) -> std::result::Result<PathBuf, Failure> {
    let expected = checksum_for(&String::from_utf8_lossy(sums), &zip_name(version))
        .ok_or(Failure::Mismatch)?;
    if sha256_hex(zip) != expected {
        return Err(Failure::Mismatch);
    }
    let members = read_zip(zip).map_err(|_| Failure::Mismatch)?;
    let staging = layout.staging_dir();
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| Failure::Install(Error::new(e.to_string())))?;
    let staged = (|| -> std::result::Result<(), Failure> {
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
                    .map_err(|e| Failure::Install(Error::new(e.to_string())))?;
            }
            std::fs::write(&dest, &member.bytes)
                .map_err(|e| Failure::Install(Error::new(e.to_string())))?;
        }
        for exe in PAYLOAD_EXES {
            let path = staging.join(exe);
            if !path.is_file() {
                return Err(Failure::Mismatch);
            }
            verify(&path)?;
        }
        Ok(())
    })();
    if let Err(failure) = staged {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(failure);
    }
    Ok(staging)
}

/// Downloads `version`, checks it and puts it in place of the current
/// install, from the site the check found it on, or the other one if
/// that fails. Blocking; run it off the UI thread. A download or gate
/// failure changes nothing on disk outside the staging directory,
/// which is removed; a swap failure restores the previous files.
pub fn download_and_install(
    layout: &InstallLayout,
    version: &str,
) -> std::result::Result<(), Failure> {
    let mut failure = None;
    let mut staged = None;
    for source in sources() {
        match download(source, version, layout) {
            Ok(path) => {
                set_preferred(source);
                staged = Some(path);
                break;
            }
            // A build that fails the checksum or the signature gate is
            // reported over a site that could not be reached; the two
            // hosts' results are not treated differently in any way.
            Err(f) => {
                let sticky = matches!(failure, Some(Failure::Mismatch) | Some(Failure::Signature));
                if !sticky {
                    failure = Some(f);
                }
            }
        }
    }
    let Some(staging) = staged else {
        return Err(failure.unwrap_or(Failure::Mismatch));
    };
    swap_tree(layout, &staging)?;
    let _ = std::fs::remove_dir_all(&staging);
    Ok(())
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
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// Put the staged tree in place of the install, file by file: each
/// current file steps aside to its `.old` sibling, the staged file
/// takes its name, and any failure rolls every swapped file back.
/// Files the new tree does not carry are left alone.
fn swap_tree(layout: &InstallLayout, staging: &Path) -> std::result::Result<(), Failure> {
    let mut staged_files = Vec::new();
    collect_files(staging, &mut staged_files)
        .map_err(|e| Failure::Install(Error::new(e.to_string())))?;
    staged_files.sort();
    let mut swapped: Vec<(PathBuf, PathBuf, bool)> = Vec::new();
    let attempt = (|| -> std::io::Result<()> {
        for staged in &staged_files {
            let rel = staged
                .strip_prefix(staging)
                .expect("staged file is under staging");
            let target = layout.base().join(rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let backup = old_sibling(&target);
            let had_original = target.exists();
            if backup.exists() {
                std::fs::remove_file(&backup)?;
            }
            if had_original {
                std::fs::rename(&target, &backup)?;
            }
            match std::fs::rename(staged, &target) {
                Ok(()) => swapped.push((target, backup, had_original)),
                Err(e) => {
                    if had_original {
                        let _ = std::fs::rename(&backup, &target);
                    }
                    return Err(e);
                }
            }
        }
        Ok(())
    })();
    if let Err(e) = attempt {
        for (target, backup, had_original) in swapped.iter().rev() {
            let _ = std::fs::remove_file(target);
            if *had_original {
                let _ = std::fs::rename(backup, target);
            }
        }
        return Err(Failure::Install(Error::new(e.to_string())));
    }
    Ok(())
}

/// On start: remove the `.old` files a swap left behind and any
/// staging remnant. The previous process may still be exiting, so keep
/// trying for a few seconds. Never blocks the caller.
pub fn finish_previous_update(layout: &InstallLayout) {
    let base = layout.base().to_path_buf();
    std::thread::spawn(move || {
        for _ in 0..50 {
            let mut done = true;
            if !remove_old_files(&base) {
                done = false;
            }
            let staging = base.join(STAGING_DIR);
            if staging.exists() && std::fs::remove_dir_all(&staging).is_err() {
                done = false;
            }
            if done {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
}

/// Remove every `*.old` file under `dir`. Returns false when any
/// removal failed (a file still held by an exiting process).
fn remove_old_files(dir: &Path) -> bool {
    let mut done = true;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return true;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if !remove_old_files(&path) {
                done = false;
            }
        } else if path
            .file_name()
            .is_some_and(|n| n.to_string_lossy().ends_with(".old"))
            && std::fs::remove_file(&path).is_err()
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

    /// Where `url` redirects to, without following it.
    pub fn redirect_target(url: &str) -> Result<String> {
        let res = send(url, false)?;
        if !(300..400).contains(&res.status) {
            return Err(Error::new(format!("HTTP {}", res.status)));
        }
        let mut buf = vec![0u16; 2048];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `size` bytes.
        let ok = unsafe {
            WinHttpQueryHeaders(
                res.request.0,
                WINHTTP_QUERY_LOCATION,
                null(),
                buf.as_mut_ptr() as *mut c_void,
                &mut size,
                null_mut(),
            )
        };
        if ok == 0 {
            return Err(os_error());
        }
        Ok(String::from_utf16_lossy(&buf[..size as usize / 2]))
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
                CERT_QUERY_OBJECT_FILE,
                path_wide.as_ptr() as *const c_void,
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

    /// Runs `WinVerifyTrust` with the isolated engine and the SPKI
    /// policy. Returns true only when the signature is
    /// cryptographically valid, the SPKI pin matches, and a
    /// system-trusted RFC 3161 timestamp is present.
    pub fn verify_file_signature(path: &Path) -> std::result::Result<(), Failure> {
        let path_wide = wide(&path.to_string_lossy());
        // SAFETY: every pointer passed is NUL-terminated or sized as
        // passed; every handle is checked before use and released
        // before returning.
        unsafe {
            let (signer, msg, store) = signer_and_handles(&path_wide);
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

    pub fn redirect_target(_url: &str) -> Result<String> {
        Err(Error::new("Windows only"))
    }

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

    #[test]
    fn reads_tag_from_redirect() {
        assert_eq!(
            version_from_tag_url("https://github.com/iamcino/udm/releases/tag/v0.2.3"),
            Some("0.2.3".to_owned())
        );
        assert_eq!(
            version_from_tag_url("https://gitlab.com/cino-group/udm/-/releases/v1.0.0"),
            Some("1.0.0".to_owned())
        );
        assert_eq!(version_from_tag_url("https://example.com/nope"), None);
    }

    #[test]
    fn picks_the_newest_site() {
        let (version, source) = newest([
            (Source::GitHub, Ok("0.2.3".to_owned())),
            (Source::GitLab, Ok("0.2.4".to_owned())),
        ])
        .unwrap();
        assert_eq!(version, "0.2.4");
        assert_eq!(source, Source::GitLab);
        let (version, source) = newest([
            (Source::GitHub, Ok("0.2.4".to_owned())),
            (Source::GitLab, Err(Error::new("down"))),
        ])
        .unwrap();
        assert_eq!(version, "0.2.4");
        assert_eq!(source, Source::GitHub);
        assert!(newest([
            (Source::GitHub, Err(Error::new("down"))),
            (Source::GitLab, Err(Error::new("down"))),
        ])
        .is_err());
    }

    #[test]
    fn finds_checksum() {
        let sums = "aaaa  other.zip\n0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef  udm-9.9.9-portable.zip\n";
        assert_eq!(
            checksum_for(sums, "udm-9.9.9-portable.zip"),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned())
        );
        assert_eq!(checksum_for(sums, "missing.zip"), None);
        assert_eq!(
            checksum_for("short  udm-9.9.9-portable.zip", "udm-9.9.9-portable.zip"),
            None
        );
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

    fn full_zip() -> Vec<u8> {
        build_zip(&[
            ("udm.exe", b"MZ-new-gui"),
            ("udm-cli.exe", b"MZ-new-cli"),
            ("udm-host.exe", b"MZ-new-host"),
            ("extension/chrome/handoff.js", b"// handoff"),
            ("README.txt", b"readme"),
        ])
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
        assert!(!layout.staging_dir().exists());
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
        assert!(!layout.staging_dir().exists());
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
            std::fs::read(staging.join("udm.exe")).unwrap(),
            b"MZ-new-gui"
        );
        assert_eq!(
            std::fs::read(staging.join("extension/chrome/handoff.js")).unwrap(),
            b"// handoff"
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
    }

    #[test]
    fn swap_replaces_files_and_keeps_backups() {
        let dir = tempfile::tempdir().unwrap();
        let layout = InstallLayout::new(dir.path());
        std::fs::write(dir.path().join("udm.exe"), b"old-gui").unwrap();
        std::fs::write(dir.path().join("keep.txt"), b"keep").unwrap();
        let staging = layout.staging_dir();
        std::fs::create_dir_all(staging.join("sub")).unwrap();
        std::fs::write(staging.join("udm.exe"), b"new-gui").unwrap();
        std::fs::write(staging.join("sub/new.txt"), b"new-file").unwrap();
        swap_tree(&layout, &staging).unwrap();
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
        let staging = layout.staging_dir();
        std::fs::create_dir_all(staging.join("sub")).unwrap();
        std::fs::write(staging.join("a.txt"), b"new-a").unwrap();
        std::fs::write(staging.join("sub/c.txt"), b"new-c").unwrap();
        let err = swap_tree(&layout, &staging).unwrap_err();
        assert!(matches!(err, Failure::Install(_)), "got {err:?}");
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
    #[cfg(windows)]
    #[test]
    fn real_artifact_signed_accepts_and_tampered_rejects() {
        let Ok(path) = std::env::var("UDM_TEST_SIGNED_EXE") else {
            return;
        };
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
    }
}

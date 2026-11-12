//! U3 fetch layer (REV1 sections 2.1 and 2.5).
//!
//! This module downloads an update only when the caller invokes it. It does
//! not poll. Origins are exactly two HTTPS manifest URLs supplied by the
//! caller, in the order to try first (GitHub, then GitLab, when the caller
//! follows the O2 order). There is no built-in origin and no third origin.
//!
//! A manifest is verified with [`Manifest::verify_governed`] against the
//! publisher keys the session currently trusts. Production starts from
//! [`TrustRoot::pinned`]. A rotation adopted through
//! [`FetchSession::accept_rotation_record`] can add the successor key
//! during its dual-signature window. The recovery key is never a manifest
//! trust root; [`FetchSession::accept_revocation_record`] uses it only to
//! verify a revocation, which then removes that publisher key. On
//! acceptance the epoch file is replaced durably before any payload byte
//! is requested. The
//! payload archive is hashed in full against the `sha256` query parameter
//! carried on the signed manifest URLs (`urls` is inside the signed
//! canonical bytes; the landed manifest has no separate archive-hash
//! field). That parameter is removed before the request. After the
//! whole-file check, the zip is extracted and every member is checked
//! against the manifest `files` list: path, SHA-256, and size, both
//! directions. The handoff is those files. Staging, swap, and rollback are
//! unit U4 and are not implemented here.
//!
//! TLS is rustls with the operating-system trust store. The publisher pin
//! is never added as a root. A redirect to a non-HTTPS URL is refused.
//! HTTPS redirects are followed.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use flate2::read::DeflateDecoder;
use http::header::{CONTENT_RANGE, RANGE};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::ClientConfig;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::net::tls;
use crate::update_manifest::{Manifest, Refusal, TrustRoot, VerifiedManifest};
use crate::update_pin;
use crate::update_records::{ClientTrust, Ed25519Identity};
use crate::DEFAULT_USER_AGENT;

/// Per-request budget: connect and read together (REV1 section 2.1).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Transient failures are retried at most twice, with these pauses.
pub const TRANSIENT_BACKOFF: [Duration; 2] = [Duration::from_secs(1), Duration::from_secs(3)];

/// How many extra attempts follow a transient failure.
pub const TRANSIENT_RETRIES: u32 = 2;

/// How many extra attempts follow an HTTP 5xx.
pub const SERVER_RETRIES: u32 = 1;

const ARCHIVE_HASH_PARAM: &str = "sha256";
const PARTIAL_NAME: &str = ".u3-payload.partial";
const NONCE_LEN: usize = 16;

/// One configured release origin. The manifest URL must be HTTPS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Origin {
    manifest_url: Url,
}

impl Origin {
    /// Accept an HTTPS manifest URL. Any other scheme is refused.
    pub fn new(manifest_url: Url) -> Result<Self, FetchError> {
        if manifest_url.scheme() != "https" {
            return Err(FetchError::NotHttps(manifest_url.to_string()));
        }
        Ok(Self { manifest_url })
    }

    /// The configured manifest URL, without the interactive nonce.
    pub fn manifest_url(&self) -> &Url {
        &self.manifest_url
    }
}

/// What the caller passes for one interactive fetch.
#[derive(Debug)]
pub struct FetchInput<'a> {
    /// Installed semantic version, compared by unit U2.
    pub installed_version: &'a str,
    /// Local time, Unix seconds, compared by unit U2.
    pub now_unix: i64,
    /// Durable `last_seen_epoch` file. Missing means epoch 0.
    pub epoch_path: &'a Path,
    /// Directory that receives the verified extracted files.
    pub dest_dir: &'a Path,
    /// Cancels a manifest fetch or a payload download.
    pub cancel: &'a CancellationToken,
}

/// Verified extracted files. This is the U3 handoff. It is not a staged install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPayload {
    /// Manifest that passed unit U2 verification.
    pub manifest: VerifiedManifest,
    /// SHA-256 of the whole archive that was accepted.
    pub archive_sha256: String,
    /// Extracted files whose path, hash, and size matched the manifest.
    pub files: Vec<VerifiedFile>,
}

/// One extracted file that matched its manifest entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedFile {
    /// Manifest path, slash-separated.
    pub path: String,
    /// Absolute path of the written file.
    pub absolute: PathBuf,
    /// SHA-256 of the file contents, lowercase hex.
    pub sha256: String,
    /// Size in bytes.
    pub size: u64,
}

/// Failures of the fetch layer. A [`FetchError::Refused`] manifest produces
/// no payload request from that manifest.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// The URL was not HTTPS.
    #[error("refusing non-HTTPS URL {0}")]
    NotHttps(String),
    /// A connect, timeout, reset, or other transport failure after retries.
    #[error("transport failure: {0}")]
    Transient(String),
    /// An HTTP status that is not a successful body.
    #[error("HTTP {status} for {url}")]
    HttpStatus {
        /// Status code.
        status: u16,
        /// Request URL.
        url: String,
    },
    /// A redirect to a non-HTTPS URL, or a redirect the policy rejected.
    #[error("refusing redirect: {0}")]
    RedirectRefused(String),
    /// The caller cancelled the fetch. Partial bytes are discarded.
    #[error("update fetch cancelled")]
    Cancelled,
    /// Unit U2 refused the manifest.
    #[error(transparent)]
    Refused(#[from] Refusal),
    /// No payload URL survived verification.
    #[error("verified manifest has no payload URL")]
    NoPayloadUrl,
    /// A payload URL did not carry a signed whole-file SHA-256.
    #[error("verified manifest payload URL has no sha256 parameter")]
    ArchiveHashMissing,
    /// The signed whole-file SHA-256 was not 64 lowercase hex characters.
    #[error("verified manifest payload sha256 is malformed")]
    ArchiveHashMalformed,
    /// Payload URLs disagree about the whole-file SHA-256.
    #[error("verified manifest payload URLs disagree on sha256")]
    ArchiveHashDisagree,
    /// The downloaded archive did not match the signed whole-file SHA-256.
    #[error("payload archive SHA-256 does not match the verified manifest")]
    WholeFileMismatch,
    /// The archive contains a file the manifest does not list.
    #[error("archive file {0} is not in the verified manifest")]
    ExtraMember(String),
    /// The manifest lists a file the archive does not contain.
    #[error("manifest path {0} is missing from the archive")]
    MissingMember(String),
    /// A member's hash or size does not match the manifest.
    #[error("archive file {path} does not match the verified manifest")]
    MemberMismatch {
        /// Manifest path.
        path: String,
    },
    /// The archive is not a zip this layer can read.
    #[error("payload is not a usable zip: {0}")]
    BadZip(String),
    /// A zip or manifest path is absolute, empty, or escapes the destination.
    #[error("refusing unsafe path {0}")]
    UnsafePath(String),
    /// The epoch file could not be read as a stored epoch.
    #[error("epoch file is unreadable: {0}")]
    BadEpoch(String),
    /// A filesystem error at `path`.
    #[error("filesystem error at {path}: {message}")]
    Io {
        /// Path involved.
        path: String,
        /// Operating-system message.
        message: String,
    },
    /// The process RNG could not supply a nonce.
    #[error("could not draw a manifest nonce")]
    Rng,
    /// The OS trust store could not build a TLS config.
    #[error("TLS configuration failed: {0}")]
    Tls(String),
    /// A rotation, revocation, or trust-state operation was refused.
    #[error("updater trust state refused: {0}")]
    Trust(String),
    /// The HTTP client could not be built.
    #[error("HTTP client configuration failed: {0}")]
    Client(String),
    /// Every configured origin failed.
    #[error("both update origins failed: {0}")]
    OriginsExhausted(String),
}

impl FetchError {
    fn io(path: &Path, err: std::io::Error) -> Self {
        Self::Io {
            path: path.display().to_string(),
            message: err.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct Pause {
    /// When set, sleeps are recorded and not waited. Production leaves this empty.
    record: Option<Arc<Mutex<Vec<Duration>>>>,
}

impl Pause {
    fn real() -> Self {
        Self { record: None }
    }

    #[cfg(test)]
    fn recorded() -> (Self, Arc<Mutex<Vec<Duration>>>) {
        let record = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                record: Some(Arc::clone(&record)),
            },
            record,
        )
    }

    async fn wait(&self, duration: Duration, cancel: &CancellationToken) -> Result<(), FetchError> {
        if cancel.is_cancelled() {
            return Err(FetchError::Cancelled);
        }
        if let Some(record) = &self.record {
            record
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(duration);
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(FetchError::Cancelled),
            _ = tokio::time::sleep(duration) => Ok(()),
        }
    }
}

/// One interactive update fetch over the two configured origins.
#[derive(Debug)]
pub struct FetchSession {
    origins: [Origin; 2],
    preferred: Mutex<usize>,
    client: reqwest::Client,
    trust: TrustRoot,
    pause: Pause,
    /// Recovery identity used to verify revocation records. Absent until
    /// [`Self::open`] or [`Self::set_recovery_key`] sets it. Manifests are
    /// never verified with this key.
    recovery: Mutex<Option<Ed25519Identity>>,
    /// Rotation and revocation state for this session.
    client_trust: Mutex<ClientTrust>,
    /// When set, accepting a record persists the trust state here.
    trust_path: Mutex<Option<PathBuf>>,
}

impl FetchSession {
    /// Production session: OS trust store, real backoff, publisher trust root.
    ///
    /// The publisher pin is not installed as a TLS root. `origins[0]` is tried
    /// first until a fetch in this session succeeds on the other origin.
    pub fn open(origins: [Origin; 2]) -> Result<Self, FetchError> {
        let session = Self::build(
            origins,
            os_trust_config()?,
            TrustRoot::pinned(),
            Pause::real(),
            true,
        )?;
        let recovery = Ed25519Identity::from_pinning(
            update_pin::recovery_public_key(),
            update_pin::recovery_pin(),
        )
        .map_err(|err| FetchError::Trust(err.to_string()))?;
        session.set_recovery(recovery);
        Ok(session)
    }

    /// Session with an explicit TLS config and trust root.
    ///
    /// Tests pass a fixture CA and a fixture key. The OS-trust constructor is
    /// [`Self::open`]. This method does not add the publisher pin to `tls`.
    pub fn with_tls(
        origins: [Origin; 2],
        tls: ClientConfig,
        trust: TrustRoot,
    ) -> Result<Self, FetchError> {
        Self::build(origins, tls, trust, Pause::real(), false)
    }

    fn build(
        origins: [Origin; 2],
        tls: ClientConfig,
        trust: TrustRoot,
        pause: Pause,
        system_proxy: bool,
    ) -> Result<Self, FetchError> {
        let identity = Ed25519Identity::from_pinning(trust.public_key, &trust.pin)
            .map_err(|err| FetchError::Trust(err.to_string()))?;
        let client_trust =
            ClientTrust::bootstrap(identity).map_err(|err| FetchError::Trust(err.to_string()))?;
        Ok(Self {
            origins,
            preferred: Mutex::new(0),
            client: build_client(tls, system_proxy)?,
            trust,
            pause,
            recovery: Mutex::new(None),
            client_trust: Mutex::new(client_trust),
            trust_path: Mutex::new(None),
        })
    }

    /// Install the recovery identity used to verify revocation records.
    ///
    /// Tests pass a freshly generated key. Production [`Self::open`] passes
    /// the compiled recovery pin. This does not make the recovery key a
    /// manifest trust root.
    pub fn set_recovery_key(&self, public_key: [u8; 32], pin: &str) -> Result<(), FetchError> {
        let recovery = Ed25519Identity::from_pinning(public_key, pin)
            .map_err(|err| FetchError::Trust(err.to_string()))?;
        self.set_recovery(recovery);
        Ok(())
    }

    fn set_recovery(&self, recovery: Ed25519Identity) {
        self.client_trust
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .bind_recovery(&recovery);
        *self
            .recovery
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(recovery);
    }

    /// Load trust state from `path` and persist later adoptions there.
    ///
    /// A missing file keeps the current in-memory state. A corrupt file or
    /// a file written for a different publisher pin refuses the load and
    /// does not clear a revocation already in memory.
    pub fn bind_trust_file(&self, path: &Path) -> Result<(), FetchError> {
        self.client_trust
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .load_into(path)
            .map_err(|err| FetchError::Trust(err.to_string()))?;
        *self
            .trust_path
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(path.to_owned());
        Ok(())
    }

    /// Verify and adopt a rotation announcement.
    ///
    /// The record must be seen strictly before its cutover. A client that
    /// calls this at or after cutover does not start trusting the new key.
    pub fn accept_rotation_record(
        &self,
        bytes: &[u8],
        now_unix: i64,
        last_seen_epoch: u64,
    ) -> Result<(), FetchError> {
        let recovery = self.recovery_identity()?;
        let mut trust = self
            .client_trust
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut next = trust.clone();
        next.accept_rotation(bytes, now_unix, last_seen_epoch, &recovery)
            .map_err(|err| FetchError::Trust(err.to_string()))?;
        self.persist_trust(&next)?;
        *trust = next;
        Ok(())
    }

    /// Verify a revocation and stop trusting the named publisher key.
    ///
    /// The installed build is left where it is: this method does not
    /// download or stage anything.
    pub fn accept_revocation_record(&self, bytes: &[u8], now_unix: i64) -> Result<(), FetchError> {
        let recovery = self.recovery_identity()?;
        let mut trust = self
            .client_trust
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut next = trust.clone();
        next.accept_revocation(bytes, now_unix, &recovery)
            .map_err(|err| FetchError::Trust(err.to_string()))?;
        self.persist_trust(&next)?;
        *trust = next;
        Ok(())
    }

    /// A copy of the session's trust state.
    pub fn client_trust(&self) -> ClientTrust {
        self.client_trust
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    fn recovery_identity(&self) -> Result<Ed25519Identity, FetchError> {
        self.recovery
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
            .ok_or_else(|| {
                FetchError::Trust(
                    "recovery key is not configured; refusing to adopt a trust record".to_owned(),
                )
            })
    }

    fn recovery_pins(&self) -> Vec<String> {
        let mut pins = vec![update_pin::recovery_pin().to_owned()];
        if let Some(recovery) = self
            .recovery
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .as_ref()
        {
            if !pins.iter().any(|pin| pin == recovery.pin()) {
                pins.push(recovery.pin().to_owned());
            }
        }
        pins
    }

    fn persist_trust(&self, trust: &ClientTrust) -> Result<(), FetchError> {
        let path = self
            .trust_path
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        if let Some(path) = path {
            trust
                .store(&path)
                .map_err(|err| FetchError::Trust(err.to_string()))?;
        }
        Ok(())
    }

    /// Fetch a manifest from the configured origins, then the payload.
    pub async fn fetch(&self, input: FetchInput<'_>) -> Result<VerifiedPayload, FetchError> {
        ensure_running(input.cancel)?;
        fs::create_dir_all(input.dest_dir).map_err(|e| FetchError::io(input.dest_dir, e))?;
        let order = self.origin_order();
        let mut failures = Vec::new();
        for index in order {
            match self.fetch_manifest(index, &input).await {
                Ok(verified) => {
                    *self.preferred.lock().unwrap_or_else(|p| p.into_inner()) = index;
                    return self.fetch_payload(verified, &input).await;
                }
                Err(FetchError::Cancelled) => {
                    discard_partial(&partial_path(input.dest_dir));
                    return Err(FetchError::Cancelled);
                }
                Err(err) => failures.push(format!("origin {index}: {err}")),
            }
        }
        Err(FetchError::OriginsExhausted(failures.join("; ")))
    }

    fn origin_order(&self) -> [usize; 2] {
        let preferred = *self.preferred.lock().unwrap_or_else(|p| p.into_inner());
        let preferred = if preferred == 1 { 1 } else { 0 };
        [preferred, 1 - preferred]
    }

    async fn fetch_manifest(
        &self,
        index: usize,
        input: &FetchInput<'_>,
    ) -> Result<VerifiedManifest, FetchError> {
        let url = self.origins[index].manifest_url.clone();
        let downloaded = self.get_buffered(&url, None, true, input.cancel).await?;
        if downloaded.status != 200 {
            return Err(FetchError::HttpStatus {
                status: downloaded.status,
                url: url.to_string(),
            });
        }
        let manifest = Manifest::from_json(&downloaded.body)?;
        let stored = load_epoch(input.epoch_path)?;
        let decision = {
            let trust_state = self
                .client_trust
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            if trust_state.compiled_pin() != self.trust.pin {
                return Err(FetchError::Trust(
                    "session publisher pin does not match the trust state".to_owned(),
                ));
            }
            trust_state.decision(input.now_unix, manifest.manifest_epoch)
        };
        let verified = manifest.verify_governed(
            &decision,
            &self.recovery_pins(),
            &crate::update_manifest::VerifyContext {
                now_unix: input.now_unix,
                last_seen_epoch: stored,
                installed_version: input.installed_version,
            },
        )?;
        // Durable epoch write before any payload request. Unit U2 is pure;
        // this is the side effect the design puts on the fetch caller.
        store_epoch(input.epoch_path, verified.epoch())?;
        Ok(verified)
    }

    async fn fetch_payload(
        &self,
        verified: VerifiedManifest,
        input: &FetchInput<'_>,
    ) -> Result<VerifiedPayload, FetchError> {
        ensure_running(input.cancel)?;
        let (expected_hash, urls) = archive_targets(verified.manifest())?;
        let partial = partial_path(input.dest_dir);
        let mut last_err = None;
        for url in &urls {
            match self.download_archive(url, &partial, input.cancel).await {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(FetchError::Cancelled) => {
                    discard_partial(&partial);
                    return Err(FetchError::Cancelled);
                }
                Err(err) => last_err = Some(err),
            }
        }
        if let Some(err) = last_err {
            return Err(err);
        }
        let actual = sha256_file(&partial)?;
        if actual != expected_hash {
            discard_partial(&partial);
            return Err(FetchError::WholeFileMismatch);
        }
        let bytes = fs::read(&partial).map_err(|e| FetchError::io(&partial, e))?;
        let members = read_zip(&bytes)?;
        let files = match place_members(input.dest_dir, verified.manifest(), &members) {
            Ok(files) => files,
            Err(err) => {
                discard_partial(&partial);
                return Err(err);
            }
        };
        discard_partial(&partial);
        Ok(VerifiedPayload {
            manifest: verified,
            archive_sha256: actual,
            files,
        })
    }

    async fn download_archive(
        &self,
        url: &Url,
        partial: &Path,
        cancel: &CancellationToken,
    ) -> Result<(), FetchError> {
        let mut transient = 0u32;
        let mut server = 0u32;
        let mut force_full = false;
        let mut range_restarted = false;
        loop {
            ensure_running(cancel)?;
            let offset = if force_full {
                0
            } else {
                file_len(partial).unwrap_or(0)
            };
            let range = if offset > 0 && !force_full {
                Some(offset)
            } else {
                None
            };
            let response = match self
                .client
                .get(url.clone())
                .header_range(range)
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    let classified = classify_transport(err);
                    if matches!(classified, FetchError::Transient(_))
                        && transient < TRANSIENT_RETRIES
                    {
                        self.pause
                            .wait(TRANSIENT_BACKOFF[transient as usize], cancel)
                            .await?;
                        transient += 1;
                        continue;
                    }
                    return Err(classified);
                }
            };
            let status = response.status().as_u16();
            if (500..600).contains(&status) {
                if server < SERVER_RETRIES {
                    self.pause.wait(TRANSIENT_BACKOFF[0], cancel).await?;
                    server += 1;
                    continue;
                }
                return Err(FetchError::HttpStatus {
                    status,
                    url: url.to_string(),
                });
            }
            if !(status == 200 || status == 206) {
                return Err(FetchError::HttpStatus {
                    status,
                    url: url.to_string(),
                });
            }
            if status == 206 {
                let header = response
                    .headers()
                    .get(CONTENT_RANGE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                if !content_range_matches(header, offset) {
                    if range_restarted {
                        return Err(FetchError::HttpStatus {
                            status,
                            url: url.to_string(),
                        });
                    }
                    truncate_file(partial)?;
                    force_full = true;
                    range_restarted = true;
                    let _ = response.bytes().await;
                    continue;
                }
            }
            let replace = status == 200 || force_full;
            if let Err(err) = stream_to_file(response, partial, replace, cancel).await {
                if matches!(err, FetchError::Cancelled) {
                    discard_partial(partial);
                    return Err(err);
                }
                if matches!(err, FetchError::Transient(_)) && transient < TRANSIENT_RETRIES {
                    self.pause
                        .wait(TRANSIENT_BACKOFF[transient as usize], cancel)
                        .await?;
                    transient += 1;
                    continue;
                }
                return Err(err);
            }
            return Ok(());
        }
    }

    async fn get_buffered(
        &self,
        url: &Url,
        range: Option<u64>,
        interactive_nonce: bool,
        cancel: &CancellationToken,
    ) -> Result<BufferedBody, FetchError> {
        let mut transient = 0u32;
        let mut server = 0u32;
        loop {
            ensure_running(cancel)?;
            let request_url = if interactive_nonce {
                with_nonce(url, &draw_nonce()?)?
            } else {
                url.clone()
            };
            let pending = self.client.get(request_url.clone()).header_range(range);
            match pending.send().await {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if (500..600).contains(&status) {
                        if server < SERVER_RETRIES {
                            self.pause.wait(TRANSIENT_BACKOFF[0], cancel).await?;
                            server += 1;
                            continue;
                        }
                        return Err(FetchError::HttpStatus {
                            status,
                            url: request_url.to_string(),
                        });
                    }
                    if (400..500).contains(&status) {
                        return Err(FetchError::HttpStatus {
                            status,
                            url: request_url.to_string(),
                        });
                    }
                    let body = read_body(response, cancel).await?;
                    return Ok(BufferedBody { status, body });
                }
                Err(err) => {
                    let classified = classify_transport(err);
                    if matches!(classified, FetchError::RedirectRefused(_)) {
                        return Err(classified);
                    }
                    if matches!(classified, FetchError::Transient(_))
                        && transient < TRANSIENT_RETRIES
                    {
                        self.pause
                            .wait(TRANSIENT_BACKOFF[transient as usize], cancel)
                            .await?;
                        transient += 1;
                        continue;
                    }
                    return Err(classified);
                }
            }
        }
    }
}

struct BufferedBody {
    status: u16,
    body: Vec<u8>,
}

trait RangeHeader {
    fn header_range(self, range: Option<u64>) -> Self;
}

impl RangeHeader for reqwest::RequestBuilder {
    fn header_range(self, range: Option<u64>) -> Self {
        match range {
            Some(start) => self.header(RANGE, format!("bytes={start}-")),
            None => self,
        }
    }
}

/// rustls client config whose only trust anchors are the OS store.
///
/// The publisher pin is not consulted and is not added.
pub fn os_trust_config() -> Result<ClientConfig, FetchError> {
    tls::client_config(&[b"h2", b"http/1.1"]).map_err(|err| FetchError::Tls(err.to_string()))
}

fn build_client(tls: ClientConfig, system_proxy: bool) -> Result<reqwest::Client, FetchError> {
    let mut builder = reqwest::Client::builder()
        .use_preconfigured_tls(tls)
        .redirect(https_redirect_policy())
        .https_only(true)
        .timeout(REQUEST_TIMEOUT)
        .user_agent(DEFAULT_USER_AGENT);
    if !system_proxy {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|err| FetchError::Client(err.to_string()))
}

fn https_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > 10 {
            attempt.error("too many redirects")
        } else if attempt.url().scheme() != "https" {
            attempt.error("refusing redirect to a non-HTTPS URL")
        } else {
            attempt.follow()
        }
    })
}

fn classify_transport(err: reqwest::Error) -> FetchError {
    if err.is_redirect() {
        FetchError::RedirectRefused(err.to_string())
    } else {
        FetchError::Transient(err.to_string())
    }
}

fn ensure_running(cancel: &CancellationToken) -> Result<(), FetchError> {
    if cancel.is_cancelled() {
        Err(FetchError::Cancelled)
    } else {
        Ok(())
    }
}

fn draw_nonce() -> Result<String, FetchError> {
    let mut bytes = [0u8; NONCE_LEN];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| FetchError::Rng)?;
    Ok(hex::encode(bytes))
}

fn with_nonce(url: &Url, nonce: &str) -> Result<Url, FetchError> {
    let mut url = url.clone();
    let mut pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key != "nonce")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    pairs.push(("nonce".to_owned(), nonce.to_owned()));
    url.set_query(None);
    {
        let mut serializer = url.query_pairs_mut();
        for (key, value) in &pairs {
            serializer.append_pair(key, value);
        }
    }
    Ok(url)
}

/// Split the signed whole-file hash out of each payload URL.
fn archive_targets(manifest: &Manifest) -> Result<(String, Vec<Url>), FetchError> {
    if manifest.urls.is_empty() {
        return Err(FetchError::NoPayloadUrl);
    }
    let mut expected = String::new();
    let mut urls = Vec::with_capacity(manifest.urls.len());
    for raw in &manifest.urls {
        let Ok(mut url) = Url::parse(raw) else {
            return Err(FetchError::NotHttps(raw.clone()));
        };
        if url.scheme() != "https" {
            return Err(FetchError::NotHttps(url.to_string()));
        }
        let mut hash = None;
        let mut kept = Vec::new();
        for (key, value) in url.query_pairs() {
            if key == ARCHIVE_HASH_PARAM {
                hash = Some(value.into_owned());
            } else {
                kept.push((key.into_owned(), value.into_owned()));
            }
        }
        let Some(hash) = hash else {
            return Err(FetchError::ArchiveHashMissing);
        };
        if !is_sha256_hex(&hash) {
            return Err(FetchError::ArchiveHashMalformed);
        }
        if expected.is_empty() {
            expected = hash;
        } else if expected != hash {
            return Err(FetchError::ArchiveHashDisagree);
        }
        url.set_query(None);
        if !kept.is_empty() {
            let mut serializer = url.query_pairs_mut();
            for (key, value) in &kept {
                serializer.append_pair(key, value);
            }
        }
        urls.push(url);
    }
    Ok((expected, urls))
}

fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct EpochDoc {
    last_seen_epoch: u64,
}

fn load_epoch(path: &Path) -> Result<u64, FetchError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<EpochDoc>(&bytes)
            .map(|doc| doc.last_seen_epoch)
            .map_err(|err| FetchError::BadEpoch(err.to_string())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(FetchError::io(path, err)),
    }
}

fn store_epoch(path: &Path, epoch: u64) -> Result<(), FetchError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| FetchError::io(dir, e))?;
    }
    let bytes = serde_json::to_vec(&EpochDoc {
        last_seen_epoch: epoch,
    })
    .map_err(|err| FetchError::BadEpoch(err.to_string()))?;
    let mut tmp_name = path.as_os_str().to_owned();
    tmp_name.push(".tmp");
    let tmp = PathBuf::from(tmp_name);
    {
        let mut file = File::create(&tmp).map_err(|e| FetchError::io(&tmp, e))?;
        file.write_all(&bytes)
            .map_err(|e| FetchError::io(&tmp, e))?;
        file.sync_all().map_err(|e| FetchError::io(&tmp, e))?;
    }
    fs::rename(&tmp, path).map_err(|e| FetchError::io(path, e))?;
    Ok(())
}

fn partial_path(dest_dir: &Path) -> PathBuf {
    dest_dir.join(PARTIAL_NAME)
}

fn discard_partial(path: &Path) {
    let _ = fs::remove_file(path);
}

fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|meta| meta.len())
}

fn truncate_file(path: &Path) -> Result<(), FetchError> {
    if path.exists() {
        let file = File::options()
            .write(true)
            .open(path)
            .map_err(|e| FetchError::io(path, e))?;
        file.set_len(0).map_err(|e| FetchError::io(path, e))?;
        file.sync_all().map_err(|e| FetchError::io(path, e))?;
    }
    Ok(())
}

fn content_range_matches(header: &str, start: u64) -> bool {
    let Some(rest) = header.trim().strip_prefix("bytes ") else {
        return false;
    };
    let Some((span, _)) = rest.split_once('/') else {
        return false;
    };
    let Some((start_text, end_text)) = span.split_once('-') else {
        return false;
    };
    let Ok(got) = start_text.trim().parse::<u64>() else {
        return false;
    };
    let Ok(end) = end_text.trim().parse::<u64>() else {
        return false;
    };
    got == start && end >= got
}

async fn stream_to_file(
    mut response: reqwest::Response,
    path: &Path,
    replace: bool,
    cancel: &CancellationToken,
) -> Result<(), FetchError> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| FetchError::io(dir, e))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| FetchError::io(path, e))?;
    if replace {
        file.set_len(0).map_err(|e| FetchError::io(path, e))?;
        file.seek(SeekFrom::Start(0))
            .map_err(|e| FetchError::io(path, e))?;
    } else {
        file.seek(SeekFrom::End(0))
            .map_err(|e| FetchError::io(path, e))?;
    }
    loop {
        ensure_running(cancel)?;
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(FetchError::Cancelled);
            }
            chunk = response.chunk() => chunk,
        };
        match next {
            Ok(Some(bytes)) => {
                file.write_all(&bytes)
                    .map_err(|e| FetchError::io(path, e))?;
            }
            Ok(None) => break,
            Err(err) => return Err(classify_transport(err)),
        }
    }
    file.sync_all().map_err(|e| FetchError::io(path, e))?;
    Ok(())
}

async fn read_body(
    mut response: reqwest::Response,
    cancel: &CancellationToken,
) -> Result<Vec<u8>, FetchError> {
    let mut body = Vec::new();
    loop {
        ensure_running(cancel)?;
        let next = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(FetchError::Cancelled);
            }
            chunk = response.chunk() => chunk,
        };
        match next {
            Ok(Some(bytes)) => body.extend_from_slice(&bytes),
            Ok(None) => return Ok(body),
            Err(err) => return Err(classify_transport(err)),
        }
    }
}

fn sha256_file(path: &Path) -> Result<String, FetchError> {
    let mut file = File::open(path).map_err(|e| FetchError::io(path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = file.read(&mut buf).map_err(|e| FetchError::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

struct ZipMember {
    name: String,
    bytes: Vec<u8>,
}

fn read_zip(data: &[u8]) -> Result<Vec<ZipMember>, FetchError> {
    let eocd = find_eocd(data)?;
    let entries = u16_at(data, eocd + 10)? as usize;
    let cd_size = u32_at(data, eocd + 12)? as usize;
    let cd_offset = u32_at(data, eocd + 16)? as usize;
    if cd_offset.saturating_add(cd_size) > data.len() {
        return Err(FetchError::BadZip(
            "central directory is out of range".into(),
        ));
    }
    let mut cursor = cd_offset;
    let mut members = Vec::with_capacity(entries);
    for _ in 0..entries {
        if cursor + 46 > data.len() || u32_at(data, cursor)? != 0x0201_4b50 {
            return Err(FetchError::BadZip("bad central directory entry".into()));
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
            return Err(FetchError::BadZip("central name is out of range".into()));
        }
        let name = std::str::from_utf8(&data[name_start..name_end])
            .map_err(|_| FetchError::BadZip("zip name is not UTF-8".into()))?
            .to_owned();
        cursor = name_end + extra_len + comment_len;
        if name.ends_with('/') {
            continue;
        }
        if flags & 1 != 0 {
            return Err(FetchError::BadZip(format!("{name} is encrypted")));
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
) -> Result<Vec<u8>, FetchError> {
    if offset + 30 > data.len() || u32_at(data, offset)? != 0x0403_4b50 {
        return Err(FetchError::BadZip("bad local header".into()));
    }
    let name_len = u16_at(data, offset + 26)? as usize;
    let extra_len = u16_at(data, offset + 28)? as usize;
    let data_start = offset + 30 + name_len + extra_len;
    let data_end = data_start + comp_size;
    if data_end > data.len() {
        return Err(FetchError::BadZip("local data is out of range".into()));
    }
    let compressed = &data[data_start..data_end];
    match method {
        0 => Ok(compressed.to_vec()),
        8 => {
            let mut decoder = DeflateDecoder::new(compressed);
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .map_err(|err| FetchError::BadZip(format!("deflate: {err}")))?;
            Ok(out)
        }
        other => Err(FetchError::BadZip(format!("unsupported method {other}"))),
    }
}

fn find_eocd(data: &[u8]) -> Result<usize, FetchError> {
    if data.len() < 22 {
        return Err(FetchError::BadZip("too small".into()));
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
    Err(FetchError::BadZip(
        "missing end of central directory".into(),
    ))
}

fn u16_at(data: &[u8], index: usize) -> Result<u16, FetchError> {
    let bytes: [u8; 2] = data
        .get(index..index + 2)
        .ok_or_else(|| FetchError::BadZip("truncated field".into()))?
        .try_into()
        .map_err(|_| FetchError::BadZip("truncated field".into()))?;
    Ok(u16::from_le_bytes(bytes))
}

fn u32_at(data: &[u8], index: usize) -> Result<u32, FetchError> {
    let bytes: [u8; 4] = data
        .get(index..index + 4)
        .ok_or_else(|| FetchError::BadZip("truncated field".into()))?
        .try_into()
        .map_err(|_| FetchError::BadZip("truncated field".into()))?;
    Ok(u32::from_le_bytes(bytes))
}

fn place_members(
    dest_dir: &Path,
    manifest: &Manifest,
    members: &[ZipMember],
) -> Result<Vec<VerifiedFile>, FetchError> {
    let mut from_zip: BTreeMap<String, &[u8]> = BTreeMap::new();
    for member in members {
        if from_zip
            .insert(member.name.clone(), member.bytes.as_slice())
            .is_some()
        {
            return Err(FetchError::BadZip(format!("duplicate {}", member.name)));
        }
    }
    let mut from_manifest: BTreeMap<&str, (&str, u64)> = BTreeMap::new();
    for file in &manifest.files {
        if safe_relative(&file.path).is_err() {
            return Err(FetchError::UnsafePath(file.path.clone()));
        }
        if from_manifest
            .insert(file.path.as_str(), (file.sha256.as_str(), file.size))
            .is_some()
        {
            return Err(FetchError::BadZip(format!(
                "duplicate manifest path {}",
                file.path
            )));
        }
    }
    for name in from_zip.keys() {
        if !from_manifest.contains_key(name.as_str()) {
            return Err(FetchError::ExtraMember(name.clone()));
        }
    }
    for name in from_manifest.keys() {
        if !from_zip.contains_key(*name) {
            return Err(FetchError::MissingMember((*name).to_owned()));
        }
    }
    let mut written = Vec::new();
    for (name, bytes) in &from_zip {
        let (expected_hash, expected_size) = from_manifest[name.as_str()];
        let actual_hash = sha256_bytes(bytes);
        if actual_hash != expected_hash || bytes.len() as u64 != expected_size {
            for path in &written {
                let _ = fs::remove_file(path);
            }
            return Err(FetchError::MemberMismatch { path: name.clone() });
        }
        let relative = safe_relative(name)?;
        let absolute = dest_dir.join(&relative);
        if let Some(parent) = absolute.parent() {
            fs::create_dir_all(parent).map_err(|e| FetchError::io(parent, e))?;
        }
        {
            let mut file = File::create(&absolute).map_err(|e| FetchError::io(&absolute, e))?;
            file.write_all(bytes)
                .map_err(|e| FetchError::io(&absolute, e))?;
            file.sync_all().map_err(|e| FetchError::io(&absolute, e))?;
        }
        written.push(absolute.clone());
    }
    let mut files = Vec::with_capacity(written.len());
    for (name, bytes) in &from_zip {
        files.push(VerifiedFile {
            path: name.clone(),
            absolute: dest_dir.join(safe_relative(name)?),
            sha256: sha256_bytes(bytes),
            size: bytes.len() as u64,
        });
    }
    Ok(files)
}

fn safe_relative(path: &str) -> Result<PathBuf, FetchError> {
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') || path.contains('\\') {
        return Err(FetchError::UnsafePath(path.to_owned()));
    }
    let mut out = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => out.push(part),
            _ => return Err(FetchError::UnsafePath(path.to_owned())),
        }
    }
    if out.as_os_str().is_empty() {
        return Err(FetchError::UnsafePath(path.to_owned()));
    }
    Ok(out)
}

#[cfg(test)]
#[path = "update_fetch_tests.rs"]
mod tests;

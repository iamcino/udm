//! Browser-handoff coordinator (G5 slice 1).
//!
//! The server side of the frozen browser-handoff IPC contract
//! (`review-artifacts/zed-ipc-handoff-freeze-2026-11-08.md`, accepted
//! 2026-11-08). Adapter details deferred by the freeze are settled in
//! `review-artifacts/garen-g5-design-handoff-2026-11-09.md`:
//!
//! * the wire discriminator is the `op` field of a versioned JSON object;
//! * the intent fingerprint is SHA-256 over a canonical JSON form built
//!   from the validated intent struct, never from raw request bytes;
//! * the commit/task association boundary is the `handoff_tx` marker the
//!   engine writes in the same durable `put_task` that creates the task,
//!   which is the recovery evidence the freeze §5 requires.
//!
//! Invariants held here: transaction IDs are opaque strings; engine task
//! IDs stay `u64` (the wire sees them only as decimal-string `task_ref`);
//! `SCHEMA_VERSION` stays 2; the lease is an injected configuration
//! parameter with no default (its production value is an open owner
//! decision, freeze §5); a lost commit surfaces as `recovery_required`
//! and nothing else — there is no `HandoffPending` engine state and no
//! recovery UI in this slice.
//!
//! The coordinator owns no transport. Slice 2 brings the named pipe, the
//! pairing handshake that establishes the caller identity used here, and
//! the `udm-host` bridge. Caller identity is therefore an input to every
//! operation, never a field of the request payload (freeze §6).
//!
//! Concurrency: every coordinator operation - including materialization and
//! the open-time recovery pass - runs under the coordinator's single lock,
//! and materialization is only ever invoked with that lock held. The marker
//! lookup-then-Add sequence therefore cannot interleave inside the one
//! coordinator a data instance is allowed to have (freeze: one coordinator
//! per user/data-directory instance), so at most one engine task is ever
//! created per transaction. The only writer of the marker is
//! [`EngineAdapter::add_handoff_task`], called solely by the coordinator.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::engine::{AddRequest, Engine};
use crate::error::{Error, Result};
use crate::util;

/// The validated, immutable download intent bound to a transaction
/// (freeze §4). Only `GET` is supported in v1. Differences that survive
/// validation are material: they change the fingerprint and therefore
/// conflict on replay (freeze §3).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffIntent {
    pub url: String,
    pub method: String,
    pub filename_hint: Option<String>,
    pub size_hint: Option<u64>,
    /// The browser's own identity for its download (opaque to udm).
    pub browser_download_id: String,
    /// Destination intent as an absolute path, when the browser
    /// supplies one. Engine routing rules (G2) stay authoritative.
    pub dest_dir: Option<String>,
}

impl HandoffIntent {
    /// Upper-case the method and check the intent is one this build can
    /// honour. Called by `prepare` before fingerprinting, so the
    /// fingerprint always covers the validated form.
    pub fn validate_and_normalize(&mut self) -> Result<()> {
        self.method = self.method.to_ascii_uppercase();
        if self.method != "GET" {
            return Err(Error::Other(format!(
                "handoff: unsupported method {:?} (v1 is GET only)",
                self.method
            )));
        }
        if self.url.trim().is_empty() {
            return Err(Error::Other("handoff: empty URL".into()));
        }
        // The engine's own URL validation: scheme and shape eligibility
        // are decided in exactly one place.
        crate::net::stack::validate(&self.url)?;
        if self.browser_download_id.trim().is_empty() {
            return Err(Error::Other(
                "handoff: empty browser download identity".into(),
            ));
        }
        Ok(())
    }

    /// SHA-256 (hex) over the canonical JSON of this intent: keys in
    /// alphabetical order, no insignificant whitespace, strings
    /// JSON-escaped. Built from the struct, never from raw bytes, so
    /// equal fingerprints mean equal validated intents (freeze §3,
    /// design handoff §2).
    pub fn fingerprint(&self) -> String {
        fn js(v: &str) -> String {
            serde_json::to_string(v).expect("string serialization cannot fail")
        }
        fn jo(v: &Option<String>) -> String {
            match v {
                Some(s) => js(s),
                None => "null".to_string(),
            }
        }
        let canonical = format!(
            "{{\"browser_download_id\":{},\"dest_dir\":{},\"filename_hint\":{},\"method\":{},\"size_hint\":{},\"url\":{}}}",
            js(&self.browser_download_id),
            jo(&self.dest_dir),
            jo(&self.filename_hint),
            js(&self.method),
            self.size_hint
                .map(|n| n.to_string())
                .unwrap_or_else(|| "null".to_string()),
            js(&self.url),
        );
        let digest = Sha256::digest(canonical.as_bytes());
        hex::encode(digest)
    }
}

/// Destination, filename, and start mode chosen for a handoff.
/// Bound once, on the first accepted commit, then reused for replay
/// and materialization. `filename` is stored only after the engine's
/// filename sanitizer has accepted it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserDecision {
    pub dest_dir: String,
    pub filename: String,
    pub start_paused: bool,
}

impl UserDecision {
    /// Reject an empty destination and normalize `filename` with
    /// [`crate::util::sanitize_filename`]. A filename that sanitizes
    /// to empty is a validation error. The normalized filename is what
    /// replay compares and what materialization uses.
    pub fn validate_and_normalize(&mut self) -> Result<()> {
        if self.dest_dir.trim().is_empty() {
            return Err(Error::Other("handoff: decision dest_dir is empty".into()));
        }
        let filename = util::sanitize_filename(&self.filename);
        if filename.is_empty() {
            return Err(Error::Other(
                "handoff: decision filename sanitizes to empty".into(),
            ));
        }
        self.filename = filename;
        Ok(())
    }
}

/// Durable handoff phase (freeze §3). These are not engine
/// [`crate::TaskState`] values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HandoffState {
    Prepared,
    Committed,
    Aborted,
    Expired,
    /// Dialog Cancel (0.3.0): the handoff was dropped before commit.
    /// Distinct from [`HandoffState::Aborted`], which means the browser
    /// kept its own download. A durable tombstone, same as the others.
    Discarded,
}

/// What the coordinator durably knows about who owns the download on
/// the browser side (freeze §4). Never fabricated from a host exit or
/// a transport timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserDisposition {
    /// Not yet established either way.
    Unresolved,
    /// The browser confirmed it cancelled its download for the handoff.
    CancelledForHandoff,
    /// The browser confirmed it keeps its download (abort path).
    Retained,
}

/// The durable logical record of one transaction (freeze §4). A record
/// in a terminal state is the transaction's tombstone: it keeps the
/// key, caller, intent binding and outcome so replays are answered
/// forever, and task removal never resurrects the handoff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffRecord {
    /// Opaque transaction ID (never parsed, never an engine ID).
    pub tx: String,
    /// Authenticated caller identity established by the transport.
    pub caller: String,
    /// The accepted intent. `None` only on an abort tombstone written
    /// before any prepare: the absence is recorded explicitly so a
    /// delayed prepare for the same key is rejected (freeze §4).
    pub intent: Option<HandoffIntent>,
    /// Fingerprint of the accepted intent; `None` exactly when
    /// `intent` is `None`.
    pub fingerprint: Option<String>,
    pub state: HandoffState,
    pub created_at_ms: i64,
    /// Original lease deadline. Never extended by a retry or a
    /// coordinator restart (freeze §3/§5).
    pub expires_at_ms: Option<i64>,
    pub transition_at_ms: i64,
    pub disposition: BrowserDisposition,
    /// The one engine task associated with this handoff, once
    /// materialized. Absence is not evidence that Add is safe to
    /// repeat; `association_pending` carries that distinction.
    pub engine_task_id: Option<u64>,
    /// The commitment is durable but the association write has not
    /// landed yet (or materialization has not succeeded). Recovery
    /// resolves it through the engine's `handoff_tx` marker.
    pub association_pending: bool,
    /// Durable terminal/recovery note, sufficient for deterministic
    /// retries (freeze §4).
    pub outcome: Option<String>,
    /// Decision bound by the first accepted commit (0.3.0). Absent on
    /// pre-0.3.0 journals and on legacy commits that supplied none.
    #[serde(default)]
    pub decision: Option<UserDecision>,
    /// Last engine Add failure while the association is still pending.
    /// Cleared when materialization succeeds. Absent on older journals.
    #[serde(default)]
    pub materialize_error: Option<String>,
}

/// The reply vocabulary of the four operations (freeze §3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HandoffStatus {
    Prepared,
    Committed,
    Aborted,
    Expired,
    /// The transaction was discarded before commit (0.3.0 dialog Cancel).
    Discarded,
    NotFound,
    Conflict,
    RecoveryRequired,
}

impl HandoffStatus {
    fn of(state: HandoffState) -> HandoffStatus {
        match state {
            HandoffState::Prepared => HandoffStatus::Prepared,
            HandoffState::Committed => HandoffStatus::Committed,
            HandoffState::Aborted => HandoffStatus::Aborted,
            HandoffState::Expired => HandoffStatus::Expired,
            HandoffState::Discarded => HandoffStatus::Discarded,
        }
    }
}

/// One operation's answer. Every reply identifies the transaction and
/// the authoritative state (freeze §3).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandoffReply {
    pub status: HandoffStatus,
    pub tx: String,
    /// The authoritative handoff state, when the transaction exists.
    pub state: Option<HandoffState>,
    /// Engine task ID as a decimal string (freeze §2.2), once
    /// associated.
    pub task_ref: Option<String>,
    /// The original lease deadline, for `Prepared` transactions.
    pub expires_at_ms: Option<i64>,
    pub detail: Option<String>,
}

impl HandoffReply {
    fn of_record(status: HandoffStatus, rec: &HandoffRecord) -> HandoffReply {
        HandoffReply {
            status,
            tx: rec.tx.clone(),
            state: Some(rec.state),
            task_ref: rec.engine_task_id.map(|id| id.to_string()),
            expires_at_ms: rec.expires_at_ms,
            detail: None,
        }
    }

    fn bare(status: HandoffStatus, tx: &str) -> HandoffReply {
        HandoffReply {
            status,
            tx: tx.to_string(),
            state: None,
            task_ref: None,
            expires_at_ms: None,
            detail: None,
        }
    }

    fn with_detail(mut self, detail: impl Into<String>) -> HandoffReply {
        self.detail = Some(detail.into());
        self
    }
}

/// Durable storage for handoff records. Implementations must make
/// `persist` durable (write + fsync + atomic replace) before returning:
/// the coordinator ACKs `prepare` and reports a successful `commit`
/// only after the corresponding persist has landed (freeze §3/§5).
pub trait HandoffStore: Send {
    fn load(&mut self) -> Result<Vec<HandoffRecord>>;
    fn persist(&mut self, records: &[HandoffRecord]) -> Result<()>;
}

/// In-memory store for tests and transport prototyping. Not durable;
/// never used by the production coordinator.
#[derive(Debug)]
pub struct MemoryStore {
    records: Vec<HandoffRecord>,
}

impl MemoryStore {
    pub fn new() -> Self {
        MemoryStore {
            records: Vec::new(),
        }
    }

    pub fn with_records(records: Vec<HandoffRecord>) -> Self {
        MemoryStore { records }
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl HandoffStore for MemoryStore {
    fn load(&mut self) -> Result<Vec<HandoffRecord>> {
        Ok(self.records.clone())
    }

    fn persist(&mut self, records: &[HandoffRecord]) -> Result<()> {
        self.records = records.to_vec();
        Ok(())
    }
}

/// The production store: one JSON document holding every record,
/// replaced atomically on each transition (write the temp file, fsync
/// it, rename over the live file — the PR 2 finalize-journal
/// discipline). Records are few and small; a full rewrite per
/// transition is the honest cost of never journaling a partial state.
#[derive(Debug)]
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        FileStore { path }
    }

    /// `<data root>/handoff/records.json`.
    pub fn journal_path(paths: &crate::Paths) -> PathBuf {
        paths.root.join("handoff").join("records.json")
    }
}

impl HandoffStore for FileStore {
    fn load(&mut self) -> Result<Vec<HandoffRecord>> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
                Error::Other(format!(
                    "handoff: journal {} is unreadable: {e}",
                    self.path.display()
                ))
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(Error::io(self.path.clone(), e)),
        }
    }

    fn persist(&mut self, records: &[HandoffRecord]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(dir.to_path_buf(), e))?;
        }
        let bytes = serde_json::to_vec(records)
            .map_err(|e| Error::Other(format!("handoff: journal encode failed: {e}")))?;
        let mut tmp_os = self.path.clone().into_os_string();
        tmp_os.push(".tmp");
        let tmp = PathBuf::from(tmp_os);
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp).map_err(|e| Error::io(tmp.clone(), e))?;
            f.write_all(&bytes).map_err(|e| Error::io(tmp.clone(), e))?;
            f.sync_all().map_err(|e| Error::io(tmp.clone(), e))?;
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| Error::io(self.path.clone(), e))?;
        // Best-effort directory fsync so the rename itself is durable.
        // Windows has no directory handle fsync; the file fsync above
        // plus the atomic rename is the platform contract there.
        #[cfg(unix)]
        if let Some(dir) = self.path.parent() {
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
        }
        Ok(())
    }
}

/// Time source, injected so expiry is testable without sleeping.
pub trait HandoffClock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Wall clock.
#[derive(Debug)]
pub struct SystemClock;

impl HandoffClock for SystemClock {
    fn now_ms(&self) -> i64 {
        util::now_millis()
    }
}

/// The coordinator's one engine boundary. The commit protocol depends
/// on `find_handoff_task` being truthful after a crash: it reads the
/// durable `handoff_tx` marker, not an in-memory cache.
pub trait EnginePort: Send + Sync {
    /// Create the engine task for a committed handoff, with the
    /// transaction marker written in the same durable store write
    /// that creates the task. A bound decision replaces the intent's
    /// destination and filename and sets whether the task starts paused.
    fn add_handoff_task(
        &self,
        intent: &HandoffIntent,
        tx: &str,
        decision: Option<&UserDecision>,
    ) -> Result<u64>;
    /// The task already carrying this transaction's marker, if any.
    fn find_handoff_task(&self, tx: &str) -> Result<Option<u64>>;
}

/// [`EnginePort`] over the real [`Engine`].
pub struct EngineAdapter {
    engine: Arc<Engine>,
}

impl std::fmt::Debug for EngineAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineAdapter").finish_non_exhaustive()
    }
}

impl EngineAdapter {
    pub fn new(engine: Arc<Engine>) -> Self {
        EngineAdapter { engine }
    }
}

impl EnginePort for EngineAdapter {
    fn add_handoff_task(
        &self,
        intent: &HandoffIntent,
        tx: &str,
        decision: Option<&UserDecision>,
    ) -> Result<u64> {
        let mut request = AddRequest::new(intent.url.clone());
        if let Some(decision) = decision {
            request = request
                .dest_dir(decision.dest_dir.clone())
                .filename(decision.filename.clone())
                .start_paused(decision.start_paused);
        } else {
            if let Some(name) = &intent.filename_hint {
                request = request.filename(name.clone());
            }
            if let Some(dir) = &intent.dest_dir {
                request = request.dest_dir(dir.clone());
            }
        }
        request = request.handoff_tx(tx.to_string());
        self.engine.add(request)
    }

    fn find_handoff_task(&self, tx: &str) -> Result<Option<u64>> {
        Ok(self.engine.task_by_handoff(tx))
    }
}

/// Coordinator configuration. `lease` is required at this layer: the
/// freeze (§5) reserved its production value to the owner, who set it
/// to 60 s on 2026-11-09 - that default lives in `Settings`, and this
/// struct still takes the value explicitly. Deadlines are never
/// renewed implicitly.
#[derive(Debug)]
pub struct CoordinatorConfig {
    pub lease: Duration,
    /// Authenticated caller identities the transport may present
    /// (design handoff §3). Anything else is refused before any
    /// record is read or written.
    pub allowed_callers: Vec<String>,
    /// When set, committing a `Prepared` record without a user decision
    /// is a conflict and mutates nothing. Off keeps the pre-0.3.0 commit
    /// path so a non-GUI caller still works.
    pub require_decision: bool,
}

impl std::fmt::Debug for Coordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Coordinator").finish_non_exhaustive()
    }
}

struct Inner {
    records: Vec<HandoffRecord>,
    store: Box<dyn HandoffStore>,
    /// Authenticated caller identities the transport may present
    /// (design handoff section 3). Lives under this mutex so a
    /// running-session register/unregister can replace the list
    /// while the pipe is serving: pairing and check_caller read it
    /// from here, never from a stale snapshot.
    allowed_callers: Vec<String>,
    /// `(caller, tx)` keys that moved `Prepared` → `Expired` during
    /// this process. Seeded by the open-time reconciliation and
    /// extended by every later expiry. Records already expired in the
    /// journal at load are not inserted.
    expiry_notices: HashSet<(String, String)>,
}

/// The single coordinator for one user/data-directory instance
/// (freeze §2.4). All operations serialize on one lock: commit vs
/// abort vs expiry has exactly one durable winner, returned to all
/// retries (freeze §3).
pub struct Coordinator {
    inner: Mutex<Inner>,
    engine: Arc<dyn EnginePort>,
    clock: Arc<dyn HandoffClock>,
    lease_ms: i64,
    require_decision: bool,
}

impl Coordinator {
    /// Load the journal and reconcile any committed-but-unassociated
    /// records left by a crash (freeze §5) before serving.
    pub fn open(
        mut store: Box<dyn HandoffStore>,
        engine: Arc<dyn EnginePort>,
        clock: Arc<dyn HandoffClock>,
        config: CoordinatorConfig,
    ) -> Result<Coordinator> {
        let records = store.load()?;
        let coordinator = Coordinator {
            inner: Mutex::new(Inner {
                records,
                store,
                allowed_callers: config.allowed_callers,
                expiry_notices: HashSet::new(),
            }),
            engine,
            clock,
            lease_ms: i64::try_from(config.lease.as_millis()).unwrap_or(i64::MAX),
            require_decision: config.require_decision,
        };
        coordinator.recover();
        // Expiry is reconciled here, not only on a later touch, so a
        // record that was still Prepared in the journal but is past
        // its deadline becomes a session notice before anyone asks.
        coordinator.reconcile_deadlines();
        Ok(coordinator)
    }

    fn check_caller(&self, caller: &str) -> Result<()> {
        let inner = self.inner.lock().expect("handoff lock poisoned");
        if inner.allowed_callers.iter().any(|c| c == caller) {
            Ok(())
        } else {
            Err(Error::Other(format!(
                "handoff: unapproved caller {caller:?}"
            )))
        }
    }

    fn check_tx(tx: &str) -> Result<()> {
        if tx.is_empty() || tx.len() > 128 || tx.chars().any(|c| c.is_control()) {
            return Err(Error::Other("handoff: invalid transaction id".into()));
        }
        Ok(())
    }

    fn find(records: &[HandoffRecord], caller: &str, tx: &str) -> Option<usize> {
        records
            .iter()
            .position(|r| r.caller == caller && r.tx == tx)
    }

    fn persist(inner: &mut Inner) -> Result<()> {
        inner.store.persist(&inner.records)
    }

    /// Materialize a `Prepared` record whose deadline has passed.
    /// Returns true when a transition was persisted. This is the only
    /// place a record becomes `Expired`, so the session notice set is
    /// updated here and nowhere else.
    fn reconcile_expiry(inner: &mut Inner, idx: usize, now: i64) -> Result<bool> {
        let due = inner.records[idx].state == HandoffState::Prepared
            && inner.records[idx]
                .expires_at_ms
                .map(|expires| now >= expires)
                .unwrap_or(false);
        if !due {
            return Ok(false);
        }
        let notice = (
            inner.records[idx].caller.clone(),
            inner.records[idx].tx.clone(),
        );
        {
            let rec = &mut inner.records[idx];
            rec.state = HandoffState::Expired;
            rec.transition_at_ms = now;
            rec.outcome = Some("lease expired before commit".to_string());
        }
        inner.expiry_notices.insert(notice);
        Self::persist(inner)?;
        Ok(true)
    }

    /// Walk every record and expire those past their deadline. Used at
    /// open, where a persist failure must not refuse to serve: the
    /// in-memory transition and its notice remain, and a later
    /// successful persist writes the journal.
    fn reconcile_deadlines(&self) {
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().expect("handoff lock poisoned");
        let len = inner.records.len();
        for idx in 0..len {
            let _ = Self::reconcile_expiry(&mut inner, idx, now);
        }
    }

    /// Append a recorded Add failure to a recovery detail. The base
    /// sentence stays stable for callers that only match the prefix.
    fn detail_with_materialize_error(base: &str, rec: &HandoffRecord) -> String {
        match rec.materialize_error.as_deref() {
            Some(err) if !err.is_empty() => format!("{base}: {err}"),
            _ => base.to_string(),
        }
    }

    /// Complete a durable commitment by establishing its one engine
    /// association. Marker first (a previous Add already landed),
    /// Add second (exactly once, only when no marker exists). A bound
    /// decision is applied on that Add. Returns true when the
    /// association is recorded. An Add failure is written onto the
    /// record as `materialize_error` and persisted; it is not an error
    /// of the commitment itself. Success clears that field.
    fn materialize(&self, inner: &mut Inner, idx: usize) -> bool {
        if inner.records[idx].engine_task_id.is_some() {
            inner.records[idx].association_pending = false;
            inner.records[idx].materialize_error = None;
            let _ = Self::persist(inner);
            return true;
        }
        let (tx, intent, decision) = {
            let rec = &inner.records[idx];
            (rec.tx.clone(), rec.intent.clone(), rec.decision.clone())
        };
        let found = self.engine.find_handoff_task(&tx).unwrap_or(None);
        let added = match found {
            Some(id) => Some(Ok(id)),
            None => match intent {
                Some(intent) => Some(
                    self.engine
                        .add_handoff_task(&intent, &tx, decision.as_ref()),
                ),
                None => None,
            },
        };
        match added {
            Some(Ok(id)) => {
                let rec = &mut inner.records[idx];
                rec.engine_task_id = Some(id);
                rec.association_pending = false;
                rec.materialize_error = None;
                // If this write fails the journal still says pending;
                // recovery re-finds the marker and rewrites it. The
                // association itself is already durable in the engine.
                let _ = Self::persist(inner);
                true
            }
            Some(Err(err)) => {
                let rec = &mut inner.records[idx];
                rec.materialize_error = Some(err.to_string());
                let _ = Self::persist(inner);
                false
            }
            None => false,
        }
    }

    /// Recovery pass: resolve every committed-but-unassociated record
    /// through the marker, or through exactly one Add when no marker
    /// exists. Runs at open; a record whose engine is unavailable
    /// stays pending and reports `recovery_required` on query.
    fn recover(&self) {
        let mut inner = self.inner.lock().expect("handoff lock poisoned");
        for idx in 0..inner.records.len() {
            if inner.records[idx].state == HandoffState::Committed
                && inner.records[idx].association_pending
            {
                self.materialize(&mut inner, idx);
            }
        }
    }

    /// Freeze §3 `prepare`: validate, fingerprint, durably record the
    /// intent, then ACK. A replay with identical intent returns the
    /// current state and the original deadline; a replay with a
    /// different intent conflicts without mutation, even after the
    /// transaction terminated.
    ///
    /// `browser_cancelled` is the extension's attestation that it
    /// already cancelled and erased the browser download. On creation
    /// it sets the disposition to `CancelledForHandoff`. On an
    /// identical-intent replay of a still-`Prepared` record whose
    /// disposition is still `Unresolved`, a true attestation upgrades
    /// that disposition and persists it. It does not renew the lease.
    pub fn prepare(
        &self,
        caller: &str,
        tx: &str,
        mut intent: HandoffIntent,
        browser_cancelled: bool,
    ) -> Result<HandoffReply> {
        self.check_caller(caller)?;
        Self::check_tx(tx)?;
        intent.validate_and_normalize()?;
        let fingerprint = intent.fingerprint();
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().expect("handoff lock poisoned");
        if let Some(idx) = Self::find(&inner.records, caller, tx) {
            Self::reconcile_expiry(&mut inner, idx, now)?;
            let rec = &inner.records[idx];
            if rec.intent.is_none() {
                return Ok(HandoffReply::of_record(HandoffStatus::Conflict, rec)
                    .with_detail("an abort tombstone already holds this key"));
            }
            if rec.fingerprint.as_deref() != Some(fingerprint.as_str()) {
                return Ok(HandoffReply::of_record(HandoffStatus::Conflict, rec)
                    .with_detail("same key, different intent"));
            }
            // A late attestation can still be bound while the handoff
            // is prepared and the disposition was never established.
            // Any other state keeps the disposition already recorded.
            if browser_cancelled
                && inner.records[idx].state == HandoffState::Prepared
                && inner.records[idx].disposition == BrowserDisposition::Unresolved
            {
                inner.records[idx].disposition = BrowserDisposition::CancelledForHandoff;
                Self::persist(&mut inner)?;
            }
            let rec = &inner.records[idx];
            return Ok(HandoffReply::of_record(HandoffStatus::of(rec.state), rec));
        }
        let expires_at_ms = now.saturating_add(self.lease_ms);
        let rec = HandoffRecord {
            tx: tx.to_string(),
            caller: caller.to_string(),
            intent: Some(intent),
            fingerprint: Some(fingerprint),
            state: HandoffState::Prepared,
            created_at_ms: now,
            expires_at_ms: Some(expires_at_ms),
            transition_at_ms: now,
            disposition: if browser_cancelled {
                BrowserDisposition::CancelledForHandoff
            } else {
                BrowserDisposition::Unresolved
            },
            engine_task_id: None,
            association_pending: false,
            outcome: None,
            decision: None,
            materialize_error: None,
        };
        inner.records.push(rec);
        Self::persist(&mut inner)?;
        let rec = inner.records.last().expect("just pushed");
        Ok(HandoffReply::of_record(HandoffStatus::Prepared, rec))
    }

    /// Freeze §3 `commit`, amended in 0.3.0: the commitment is persisted
    /// (and fsynced by the store) before the engine is touched and
    /// before success is reported. A supplied [`UserDecision`] is
    /// validated and bound in that same write. Materialization then
    /// follows the marker protocol in the design handoff §4, so a crash
    /// anywhere in this operation converges to exactly one associated
    /// task.
    ///
    /// Replay of a `Committed` record with an identical decision, or
    /// with no decision, returns the existing outcome. A supplied
    /// decision that differs from the bound one (including supplying
    /// one when none was bound) conflicts and mutates nothing.
    pub fn commit(
        &self,
        caller: &str,
        tx: &str,
        browser_cancelled: bool,
        decision: Option<UserDecision>,
    ) -> Result<HandoffReply> {
        self.check_caller(caller)?;
        Self::check_tx(tx)?;
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().expect("handoff lock poisoned");
        let idx = match Self::find(&inner.records, caller, tx) {
            Some(idx) => idx,
            None => return Ok(HandoffReply::bare(HandoffStatus::NotFound, tx)),
        };
        Self::reconcile_expiry(&mut inner, idx, now)?;
        match inner.records[idx].state {
            HandoffState::Aborted => {
                let rec = &inner.records[idx];
                Ok(HandoffReply::of_record(HandoffStatus::Conflict, rec)
                    .with_detail("commit against an aborted handoff"))
            }
            HandoffState::Discarded => {
                let rec = &inner.records[idx];
                Ok(HandoffReply::of_record(HandoffStatus::Conflict, rec)
                    .with_detail("commit against a discarded handoff"))
            }
            HandoffState::Expired => {
                let rec = &inner.records[idx];
                Ok(
                    HandoffReply::of_record(HandoffStatus::RecoveryRequired, rec).with_detail(
                        Self::detail_with_materialize_error(
                            "lease expired before commit; no automatic start",
                            rec,
                        ),
                    ),
                )
            }
            HandoffState::Committed => {
                if let Some(mut supplied) = decision {
                    let matches = supplied.validate_and_normalize().is_ok()
                        && inner.records[idx].decision.as_ref() == Some(&supplied);
                    if !matches {
                        let rec = &inner.records[idx];
                        return Ok(HandoffReply::of_record(HandoffStatus::Conflict, rec)
                            .with_detail(
                                "commit replay decision differs from the bound decision",
                            ));
                    }
                }
                if inner.records[idx].association_pending {
                    self.materialize(&mut inner, idx);
                }
                let rec = &inner.records[idx];
                if rec.association_pending {
                    Ok(
                        HandoffReply::of_record(HandoffStatus::RecoveryRequired, rec).with_detail(
                            Self::detail_with_materialize_error(
                                "commitment durable; association pending",
                                rec,
                            ),
                        ),
                    )
                } else {
                    Ok(HandoffReply::of_record(HandoffStatus::Committed, rec))
                }
            }
            HandoffState::Prepared => {
                // Dialog mode does not treat the commit flag as proof.
                // The disposition has to have been attested at prepare.
                if self.require_decision
                    && inner.records[idx].disposition != BrowserDisposition::CancelledForHandoff
                {
                    let rec = &inner.records[idx];
                    return Ok(
                        HandoffReply::of_record(HandoffStatus::Conflict, rec).with_detail(
                            "commit requires a prepare-time browser-cancelled attestation",
                        ),
                    );
                }
                if !browser_cancelled {
                    let rec = &inner.records[idx];
                    return Ok(HandoffReply::of_record(HandoffStatus::Conflict, rec)
                        .with_detail("commit requires the browser-cancelled confirmation"));
                }
                let bound = match decision {
                    Some(mut decision) => {
                        if let Err(err) = decision.validate_and_normalize() {
                            let rec = &inner.records[idx];
                            return Ok(HandoffReply::of_record(HandoffStatus::Conflict, rec)
                                .with_detail(err.to_string()));
                        }
                        Some(decision)
                    }
                    None if self.require_decision => {
                        let rec = &inner.records[idx];
                        return Ok(HandoffReply::of_record(HandoffStatus::Conflict, rec)
                            .with_detail("commit requires a user decision"));
                    }
                    None => None,
                };
                {
                    let rec = &mut inner.records[idx];
                    rec.state = HandoffState::Committed;
                    rec.disposition = BrowserDisposition::CancelledForHandoff;
                    rec.decision = bound;
                    rec.association_pending = true;
                    rec.transition_at_ms = now;
                    rec.outcome = Some("commitment recorded".to_string());
                }
                // The durable commitment. If this write fails nothing
                // was promised and the engine has not been touched.
                Self::persist(&mut inner)?;
                self.materialize(&mut inner, idx);
                let rec = &inner.records[idx];
                if rec.association_pending {
                    Ok(
                        HandoffReply::of_record(HandoffStatus::RecoveryRequired, rec).with_detail(
                            Self::detail_with_materialize_error(
                                "commitment durable; association pending",
                                rec,
                            ),
                        ),
                    )
                } else {
                    Ok(HandoffReply::of_record(HandoffStatus::Committed, rec))
                }
            }
        }
    }

    /// Freeze §3 `abort`: effective only before commitment, and only
    /// with the explicit browser-retains-download confirmation —
    /// anything weaker retains the recovery intent. Aborting an
    /// unknown key writes the tombstone that blocks a delayed
    /// `prepare` from resurrecting the transaction.
    pub fn abort(&self, caller: &str, tx: &str, browser_retains: bool) -> Result<HandoffReply> {
        self.check_caller(caller)?;
        Self::check_tx(tx)?;
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().expect("handoff lock poisoned");
        let idx = match Self::find(&inner.records, caller, tx) {
            Some(idx) => idx,
            None => {
                let rec = HandoffRecord {
                    tx: tx.to_string(),
                    caller: caller.to_string(),
                    intent: None,
                    fingerprint: None,
                    state: HandoffState::Aborted,
                    created_at_ms: now,
                    expires_at_ms: None,
                    transition_at_ms: now,
                    disposition: if browser_retains {
                        BrowserDisposition::Retained
                    } else {
                        BrowserDisposition::Unresolved
                    },
                    engine_task_id: None,
                    association_pending: false,
                    outcome: Some("aborted before prepare".to_string()),
                    decision: None,
                    materialize_error: None,
                };
                inner.records.push(rec);
                Self::persist(&mut inner)?;
                let rec = inner.records.last().expect("just pushed");
                return Ok(HandoffReply::of_record(HandoffStatus::Aborted, rec));
            }
        };
        Self::reconcile_expiry(&mut inner, idx, now)?;
        match inner.records[idx].state {
            HandoffState::Committed => {
                let rec = &inner.records[idx];
                Ok(HandoffReply::of_record(HandoffStatus::Committed, rec)
                    .with_detail("abort after commit has no effect on the task"))
            }
            HandoffState::Aborted => {
                let rec = &inner.records[idx];
                Ok(HandoffReply::of_record(HandoffStatus::Aborted, rec))
            }
            HandoffState::Discarded => {
                let rec = &inner.records[idx];
                Ok(HandoffReply::of_record(HandoffStatus::Discarded, rec))
            }
            HandoffState::Prepared | HandoffState::Expired => {
                if !browser_retains {
                    let rec = &inner.records[idx];
                    return Ok(HandoffReply::of_record(HandoffStatus::of(rec.state), rec)
                        .with_detail(
                            "recovery intent retained: abort requires the \
                         browser-retains-download confirmation",
                        ));
                }
                {
                    let rec = &mut inner.records[idx];
                    rec.state = HandoffState::Aborted;
                    rec.disposition = BrowserDisposition::Retained;
                    rec.transition_at_ms = now;
                    rec.outcome = Some("browser retained the download".to_string());
                }
                Self::persist(&mut inner)?;
                let rec = &inner.records[idx];
                Ok(HandoffReply::of_record(HandoffStatus::Aborted, rec))
            }
        }
    }

    /// Dialog Cancel (0.3.0): drop a still-prepared handoff. The
    /// transition is persisted with the same commitment write as
    /// `commit` before success is reported. Only `Prepared` (after
    /// expiry reconciliation) may discard. A replay of `Discarded`
    /// returns the recorded outcome. `Committed` conflicts with no
    /// mutation. `Aborted` and `Expired` answer that state.
    /// `abort` stays the browser-retains path and is not used here.
    pub fn discard(&self, caller: &str, tx: &str) -> Result<HandoffReply> {
        self.check_caller(caller)?;
        Self::check_tx(tx)?;
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().expect("handoff lock poisoned");
        let idx = match Self::find(&inner.records, caller, tx) {
            Some(idx) => idx,
            None => return Ok(HandoffReply::bare(HandoffStatus::NotFound, tx)),
        };
        Self::reconcile_expiry(&mut inner, idx, now)?;
        match inner.records[idx].state {
            HandoffState::Prepared => {
                {
                    let rec = &mut inner.records[idx];
                    rec.state = HandoffState::Discarded;
                    rec.transition_at_ms = now;
                    rec.outcome = Some("discarded before commit".to_string());
                }
                Self::persist(&mut inner)?;
                let rec = &inner.records[idx];
                Ok(HandoffReply::of_record(HandoffStatus::Discarded, rec))
            }
            HandoffState::Discarded => {
                let rec = &inner.records[idx];
                Ok(HandoffReply::of_record(HandoffStatus::Discarded, rec))
            }
            HandoffState::Committed => {
                let rec = &inner.records[idx];
                Ok(HandoffReply::of_record(HandoffStatus::Conflict, rec)
                    .with_detail("discard against a committed handoff"))
            }
            HandoffState::Aborted | HandoffState::Expired => {
                let rec = &inner.records[idx];
                Ok(HandoffReply::of_record(HandoffStatus::of(rec.state), rec))
            }
        }
    }

    /// Freeze §3 `query`: read-only. Never creates, starts, or renews;
    /// expiry is materialized before the read so the answer is the
    /// authoritative current state, not a cached one.
    pub fn query(&self, caller: &str, tx: &str) -> Result<HandoffReply> {
        self.check_caller(caller)?;
        Self::check_tx(tx)?;
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().expect("handoff lock poisoned");
        let idx = match Self::find(&inner.records, caller, tx) {
            Some(idx) => idx,
            None => return Ok(HandoffReply::bare(HandoffStatus::NotFound, tx)),
        };
        Self::reconcile_expiry(&mut inner, idx, now)?;
        let rec = &inner.records[idx];
        if rec.association_pending {
            return Ok(
                HandoffReply::of_record(HandoffStatus::RecoveryRequired, rec).with_detail(
                    Self::detail_with_materialize_error(
                        "commitment durable; association pending",
                        rec,
                    ),
                ),
            );
        }
        Ok(HandoffReply::of_record(HandoffStatus::of(rec.state), rec))
    }

    /// Re-run materialization for a committed handoff whose engine
    /// association has not landed. Answers like `query`: `Committed`
    /// with `task_ref` once the association is recorded, or
    /// `RecoveryRequired` carrying the recorded Add error while it is
    /// still pending. Any other state is reported as it stands. This
    /// does not renew a lease.
    pub fn retry_materialize(&self, caller: &str, tx: &str) -> Result<HandoffReply> {
        self.check_caller(caller)?;
        Self::check_tx(tx)?;
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().expect("handoff lock poisoned");
        let idx = match Self::find(&inner.records, caller, tx) {
            Some(idx) => idx,
            None => return Ok(HandoffReply::bare(HandoffStatus::NotFound, tx)),
        };
        Self::reconcile_expiry(&mut inner, idx, now)?;
        if inner.records[idx].state == HandoffState::Committed
            && inner.records[idx].association_pending
        {
            self.materialize(&mut inner, idx);
        }
        let rec = &inner.records[idx];
        if rec.association_pending {
            return Ok(
                HandoffReply::of_record(HandoffStatus::RecoveryRequired, rec).with_detail(
                    Self::detail_with_materialize_error(
                        "commitment durable; association pending",
                        rec,
                    ),
                ),
            );
        }
        Ok(HandoffReply::of_record(HandoffStatus::of(rec.state), rec))
    }

    /// Live prepared handoffs for `caller`, plus records that expired
    /// during this coordinator's lifetime (including the open-time
    /// pass). Records that were already expired in the journal at load
    /// are not notices. Read-only for the caller: deadlines are
    /// reconciled, never renewed. Order is preparation time, then
    /// caller, then transaction id.
    pub fn discovery(&self, caller: &str) -> Result<Vec<DiscoveryEntry>> {
        self.check_caller(caller)?;
        let now = self.clock.now_ms();
        let mut inner = self.inner.lock().expect("handoff lock poisoned");
        let len = inner.records.len();
        for idx in 0..len {
            Self::reconcile_expiry(&mut inner, idx, now)?;
        }
        let mut entries = Vec::new();
        for rec in &inner.records {
            if rec.caller != caller {
                continue;
            }
            let noticed = inner
                .expiry_notices
                .contains(&(rec.caller.clone(), rec.tx.clone()));
            let expired_notice = noticed && rec.state == HandoffState::Expired;
            if rec.state != HandoffState::Prepared && !expired_notice {
                continue;
            }
            entries.push(DiscoveryEntry {
                caller: rec.caller.clone(),
                tx: rec.tx.clone(),
                intent: rec.intent.clone(),
                created_at_ms: rec.created_at_ms,
                expires_at_ms: rec.expires_at_ms,
                expired: expired_notice,
            });
        }
        entries.sort_by(|a, b| {
            a.created_at_ms
                .cmp(&b.created_at_ms)
                .then(a.caller.cmp(&b.caller))
                .then(a.tx.cmp(&b.tx))
        });
        Ok(entries)
    }
}

/// One row of [`Coordinator::discovery`]. Not an engine task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveryEntry {
    pub caller: String,
    pub tx: String,
    pub intent: Option<HandoffIntent>,
    pub created_at_ms: i64,
    pub expires_at_ms: Option<i64>,
    pub expired: bool,
}

impl Coordinator {
    /// Whether `caller` is on this coordinator's allow-list. The pipe
    /// transport checks this during the pairing handshake; a request
    /// payload never carries a caller (freeze section 6).
    pub fn caller_allowed(&self, caller: &str) -> bool {
        let inner = self.inner.lock().expect("handoff lock poisoned");
        inner.allowed_callers.iter().any(|c| c == caller)
    }

    /// Replace the allow-list on a live coordinator (browser
    /// integration item 1): after a register/unregister while the
    /// pipe is serving, the GUI hands the freshly read callers.json
    /// list here; pairing on the next connection refuses any caller
    /// no longer listed. The list is the durable callers.json
    /// content, so a coordinator restart restores the same set.
    pub fn set_allowed_callers(&self, callers: Vec<String>) {
        let mut inner = self.inner.lock().expect("handoff lock poisoned");
        inner.allowed_callers = callers;
    }

    /// The transactions a user must be shown as needing recovery: the
    /// durable commitment exists but its engine association has not
    /// landed — exactly the set `query` answers `recovery_required`
    /// for (any record with `association_pending` set). Owner decision
    /// 2026-11-09: the GUI renders these as read-only rows in the task
    /// list. Presentation only; this never mutates a record, never
    /// starts a task, and never renews a deadline.
    pub fn recovery_entries(&self) -> Vec<RecoveryEntry> {
        let inner = self.inner.lock().expect("handoff lock poisoned");
        inner
            .records
            .iter()
            .filter(|rec| rec.association_pending)
            .map(RecoveryEntry::of_record)
            .collect()
    }
}

/// One recovery-row presentation record (see
/// [`Coordinator::recovery_entries`]). A flattened, read-only view of a
/// handoff record — deliberately NOT an engine task: it carries no
/// task state and no engine ID, because association has not landed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryEntry {
    pub tx: String,
    pub caller: String,
    pub url: Option<String>,
    pub filename_hint: Option<String>,
    pub size_hint: Option<u64>,
    /// When the record last transitioned (the durable commitment time
    /// for a lost commit), epoch milliseconds.
    pub committed_at_ms: i64,
    pub outcome: Option<String>,
}

impl RecoveryEntry {
    fn of_record(rec: &HandoffRecord) -> RecoveryEntry {
        RecoveryEntry {
            tx: rec.tx.clone(),
            caller: rec.caller.clone(),
            url: rec.intent.as_ref().map(|i| i.url.clone()),
            filename_hint: rec.intent.as_ref().and_then(|i| i.filename_hint.clone()),
            size_hint: rec.intent.as_ref().and_then(|i| i.size_hint),
            committed_at_ms: rec.transition_at_ms,
            outcome: rec.outcome.clone(),
        }
    }
}

/// Generate a fresh 256-bit pairing secret (hex, 64 chars). Entropy comes
/// from the OS-seeded `RandomState` keys (std draws them from the OS RNG),
/// stretched through SHA-256 with the current time and process id. The
/// coordinator side writes the secret into the pairing file at startup and
/// replaces it on every start; the host re-reads the file per connection,
/// so a coordinator restart heals without re-registration.
pub fn generate_pairing_secret() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut material = String::new();
    for _ in 0..4 {
        let state = RandomState::new();
        let mut h1 = state.build_hasher();
        h1.write_u64(util::now_millis() as u64);
        let a = h1.finish();
        let mut h2 = state.build_hasher();
        h2.write_u32(std::process::id());
        let b = h2.finish();
        material.push_str(&format!("{a:016x}{b:016x}"));
    }
    let digest = Sha256::digest(material.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest.iter() {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Read the registered caller list (`<data>/handoff/callers.json`, written
/// by `udm-host --register`) - the coordinator's allow-list source. A
/// missing or malformed file means no callers.
pub fn read_callers(paths: &crate::Paths) -> Vec<String> {
    let path = paths.root.join("handoff").join("callers.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    value
        .get("callers")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Newest journal `created_at_ms` for any caller of one browser
/// (browser integration item 2, tier 3): the status light reads the
/// journal file directly, with no coordinator dependency and no
/// host-process probe. `None` means no journal row for the browser
/// (tier 2 when the registry key exists). A missing or malformed
/// journal is also `None`, never an error.
pub fn last_connection_ms(paths: &crate::Paths, browser: &str) -> Option<i64> {
    let mut store = FileStore::new(FileStore::journal_path(paths));
    let records = store.load().ok()?;
    let prefix = format!("{browser}:");
    records
        .iter()
        .filter(|r| r.caller.starts_with(&prefix))
        .map(|r| r.created_at_ms)
        .max()
}

/// Repoint one browser host manifest at the sibling `udm-host.exe`
/// (browser integration item 3, portable relocation repair): set the
/// manifest's `path` field to `exe_path` and write it back. The
/// caller then repoints the HKCU key value at the manifest path.
/// Fails when the manifest is missing or malformed, so the caller
/// leaves the key alone and marks the row registration-broken.
pub fn repoint_manifest(
    paths: &crate::Paths,
    browser: &str,
    exe_path: &std::path::Path,
) -> Result<std::path::PathBuf> {
    let manifest_path = paths
        .root
        .join("handoff")
        .join(format!("host-manifest-{browser}.json"));
    let text =
        std::fs::read_to_string(&manifest_path).map_err(|e| Error::io(manifest_path.clone(), e))?;
    let mut value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| Error::Other(format!("handoff: manifest unreadable: {e}")))?;
    value["path"] = serde_json::Value::String(exe_path.to_string_lossy().to_string());
    let out = serde_json::to_string_pretty(&value)
        .map_err(|e| Error::Other(format!("handoff: manifest encode failed: {e}")))?;
    std::fs::write(&manifest_path, out).map_err(|e| Error::io(manifest_path.clone(), e))?;
    Ok(manifest_path)
}

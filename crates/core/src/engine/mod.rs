//! Engine: the task queue, in-memory state shared with the UI, and the public
//! (synchronous) API used by the Slint front-end and the CLI.
//!
//! Threading model
//! * The engine owns a Tokio multi-thread runtime that lives on its own
//!   threads. The UI thread never blocks on network I/O.
//! * A single **supervisor task** owns lifecycle decisions (start / pause /
//!   cancel / remove / admit-from-queue) and receives [`Command`]s over a
//!   channel.
//! * Each running task is one Tokio task that spawns one Tokio task per
//!   segment. Progress is published through atomics + `RwLock`s in
//!   [`TaskCell`], which the UI reads with [`Engine::snapshot`] - no callbacks
//!   crossing into the UI thread except [`Event`]s.

pub mod proof;
pub mod speed;
pub(crate) mod task;

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::{Mutex, RwLock};
use tokio::runtime::Runtime;
use tokio_util::sync::CancellationToken;

use crate::config::{Settings, TransportPref};
use crate::engine::speed::SpeedMeter;
use crate::error::{Error, Result};
use crate::hash::ExpectedHash;
use crate::net::{AltSvcStore, NetStack};
use crate::paths::Paths;
use crate::store::records::{RouteState, SegmentState, TaskRecord, TaskState, TransportKind};
use crate::store::{Store, StoredTask};
use crate::util;

/// Why a task's cancellation token fired.
pub(crate) mod stop_reason {
    pub const NONE: u8 = 0;
    pub const PAUSE: u8 = 1;
    pub const CANCEL: u8 = 2;
    pub const SHUTDOWN: u8 = 3;
    pub const REMOVE: u8 = 4;
}

/// A message for the UI / CLI. Delivered over a `flume` channel that the
/// front-end drains from its own thread.
#[derive(Clone, Debug)]
pub enum Event {
    /// A human-readable log line (also written to the local log file).
    Log {
        level: LogLevel,
        message: String,
    },
    TaskAdded {
        id: u64,
    },
    TaskStateChanged {
        id: u64,
        state: TaskState,
    },
    TaskCompleted {
        id: u64,
        path: String,
    },
    TaskFailed {
        id: u64,
        error: String,
    },
    TaskRemoved {
        id: u64,
    },
    /// The set of queued/running tasks changed (UI may want to refresh counts).
    QueueChanged,
    SettingsChanged,
    Shutdown,
}

/// Severity for [`Event::Log`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
}

impl LogLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Error => "error",
            LogLevel::Warn => "warn",
            LogLevel::Info => "info",
            LogLevel::Debug => "debug",
        }
    }
}

/// Everything the UI needs to render one row.
#[derive(Clone, Debug)]
pub struct TaskSnapshot {
    pub id: u64,
    pub url: String,
    pub filename: String,
    pub dest_dir: String,
    pub state: TaskState,
    pub state_label: String,
    pub total_size: Option<u64>,
    pub received: u64,
    /// `0.0..=1.0`, or `None` when the size is unknown.
    pub fraction: Option<f64>,
    pub speed_bps: f64,
    pub eta_secs: f64,
    pub segments_done: u32,
    pub segments_total: u32,
    /// Open segment response bodies, excluding probes. H2/H3 can multiplex
    /// these requests on one connection, so this is not a socket count.
    pub active_requests: u32,
    pub error: Option<String>,
    pub transport: Option<TransportKind>,
    pub transport_summary: crate::store::records::TransportSummary,
    /// Current range activity and observed protocol history, at most one row per family.
    pub paths: Vec<crate::net::lane::PathSnapshot>,
    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub final_path: String,
    pub expected_hash: Option<String>,
    pub computed_hash: Option<String>,
    pub attempts: u32,
    /// Bytes per second when verifying a checksum (0 otherwise).
    pub verify_bps: f64,
}

/// Request to enqueue a download.
#[derive(Clone, Debug)]
pub struct AddRequest {
    pub url: String,
    pub dest_dir: Option<PathBuf>,
    pub filename: Option<String>,
    /// Override for `settings.segments_per_task`.
    pub segments: Option<u32>,
    pub expected_hash: Option<ExpectedHash>,
    pub transport_pref: Option<TransportPref>,
    /// Add to the list without starting.
    pub start_paused: bool,
    pub note: String,
    /// Browser-handoff transaction creating this task (G5); written
    /// into the durable record in the same store write as the task.
    pub handoff_tx: Option<String>,
}

impl AddRequest {
    pub fn new(url: impl Into<String>) -> Self {
        AddRequest {
            url: url.into(),
            dest_dir: None,
            filename: None,
            segments: None,
            expected_hash: None,
            transport_pref: None,
            start_paused: false,
            note: String::new(),
            handoff_tx: None,
        }
    }

    pub fn dest_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.dest_dir = Some(dir.into());
        self
    }

    pub fn filename(mut self, name: impl Into<String>) -> Self {
        self.filename = Some(name.into());
        self
    }

    pub fn segments(mut self, n: u32) -> Self {
        self.segments = Some(n);
        self
    }

    pub fn expected_hash(mut self, hash: ExpectedHash) -> Self {
        self.expected_hash = Some(hash);
        self
    }

    pub fn transport_pref(mut self, pref: TransportPref) -> Self {
        self.transport_pref = Some(pref);
        self
    }

    pub fn start_paused(mut self, yes: bool) -> Self {
        self.start_paused = yes;
        self
    }

    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.note = note.into();
        self
    }

    pub fn handoff_tx(mut self, tx: impl Into<String>) -> Self {
        self.handoff_tx = Some(tx.into());
        self
    }
}

/// Commands understood by the supervisor task.
#[derive(Debug)]
pub(crate) enum Command {
    Admit(u64),
    Pause(u64),
    Resume(u64),
    Cancel(u64),
    Retry(u64),
    Remove(u64, bool),
    Move {
        id: u64,
        dest_dir: PathBuf,
        filename: Option<String>,
    },
    SetSettings(Settings),
    Flush,
    Shutdown,
}

/// Per-task runtime state.
pub(crate) struct TaskCell {
    pub id: u64,
    pub record: RwLock<TaskRecord>,
    pub segments: RwLock<Vec<SegmentState>>,
    /// Bytes on disk for this task (sum of segment progress).
    pub received: AtomicU64,
    pub(super) active_requests: AtomicU32,
    pub(super) lanes: Arc<crate::net::lane::LaneTable>,
    /// Scheduler-owned per-range leases. Changed only after the old worker joins.
    pub(super) allocation_epochs: Mutex<Vec<u64>>,
    pub(super) storage_failure: Mutex<Option<(u64, String)>>,
    pub(super) uses_family_paths: AtomicBool,
    proof_barrier: Mutex<Option<Arc<proof::ProofBarrier>>>,
    /// Runtime run generation (Usagi PR 3). `spawn_task` advances it once per
    /// granted run, and only after the previous run has drained (`running`).
    /// Coverage, partial claim and finalize writes carry the generation of
    /// the run that produced them and are refused once it is superseded.
    /// Not persisted: a process exit ends every write of its runs, and
    /// recovery trusts only durable checkpoints (PR 1).
    pub(super) generation: AtomicU64,
    /// Snapshot fence for the representation bound and the segment vector.
    /// Not the run generation, and not stored. Writers of the bound, the
    /// restart counter, `etag`, `final_url`, `total_size`, or the segment
    /// vector bump it with `Release` while they still hold both data locks
    /// (record first). Savers retry when it moves.
    pub(super) persist_epoch: AtomicU64,
    pub speed: RwLock<SpeedMeter>,
    pub cancel: RwLock<CancellationToken>,
    pub stop_reason: AtomicU8,
    pub running: AtomicBool,
    pub dirty: AtomicBool,
    pub verify_progress: Arc<AtomicU64>,
    pub handle: RwLock<Option<tokio::task::JoinHandle<()>>>,
    /// Held from claim so syncing cannot silently follow a replaced pathname.
    checkpoint_file: Mutex<Option<fs::File>>,
}

impl std::fmt::Debug for TaskCell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TaskCell")
            .field("id", &self.id)
            .field("state", &self.record.read().state)
            .field("received", &self.received.load(Ordering::Relaxed))
            .finish()
    }
}

impl TaskCell {
    pub(super) fn proof_gate(&self, point: proof::ProofPoint) -> Option<Arc<proof::ProofBarrier>> {
        self.proof_barrier
            .lock()
            .as_ref()
            .filter(|gate| gate.point == point)
            .cloned()
    }
    fn new(record: TaskRecord, segments: Vec<SegmentState>) -> Arc<Self> {
        let checkpoint_file =
            if record.state != TaskState::Completed && record.progress_checkpoint.is_some() {
                fs::OpenOptions::new()
                    .write(true)
                    .open(record.part_path())
                    .ok()
            } else {
                None
            };
        let received: u64 = if record.state == TaskState::Completed {
            record.total_size.unwrap_or(0)
        } else {
            segments.iter().map(|s| s.done).sum()
        };
        Arc::new(TaskCell {
            id: record.id,
            record: RwLock::new(record),
            segments: RwLock::new(segments),
            received: AtomicU64::new(received),
            active_requests: AtomicU32::new(0),
            lanes: Arc::new(crate::net::lane::LaneTable::default()),
            allocation_epochs: Mutex::new(Vec::new()),
            storage_failure: Mutex::new(None),
            uses_family_paths: AtomicBool::new(false),
            proof_barrier: Mutex::new(None),
            generation: AtomicU64::new(0),
            persist_epoch: AtomicU64::new(0),
            speed: RwLock::new(SpeedMeter::default()),
            cancel: RwLock::new(CancellationToken::new()),
            stop_reason: AtomicU8::new(stop_reason::NONE),
            running: AtomicBool::new(false),
            dirty: AtomicBool::new(false),
            verify_progress: Arc::new(AtomicU64::new(0)),
            handle: RwLock::new(None),
            checkpoint_file: Mutex::new(checkpoint_file),
        })
    }

    /// Refuse work from a superseded run. Call it while holding the segments
    /// write lock: `spawn_task` bumps the generation under that same lock, so
    /// the check and the guarded write cannot straddle a bump.
    pub(crate) fn fence(&self, generation: u64) -> Result<()> {
        let current = self.generation.load(Ordering::Acquire);
        if current == generation {
            Ok(())
        } else {
            Err(Error::StaleGeneration {
                stale: generation,
                current,
            })
        }
    }

    pub(super) fn allocation_fence(&self, index: usize, epoch: u64) -> Result<()> {
        if self
            .allocation_epochs
            .lock()
            .get(index)
            .copied()
            .unwrap_or(0)
            == epoch
        {
            Ok(())
        } else {
            Err(Error::Cancelled)
        }
    }

    /// Caller holds the record lock and the segments lock, record first.
    pub(crate) fn bump_persist_epoch(&self) {
        self.persist_epoch.fetch_add(1, Ordering::Release);
        self.dirty.store(true, Ordering::Relaxed);
    }

    pub(crate) fn token(&self) -> CancellationToken {
        self.cancel.read().clone()
    }

    pub(crate) fn state(&self) -> TaskState {
        self.record.read().state
    }

    pub(crate) fn snapshot(&self) -> TaskSnapshot {
        let record = self.record.read();
        let received = self.received.load(Ordering::Relaxed);
        let segments = self.segments.read();
        let speed = self.speed.read();
        let verify_bps = if record.state == TaskState::Verifying {
            speed.bps()
        } else {
            0.0
        };
        let segments_total = if segments.is_empty() {
            record.segments_planned.max(1)
        } else {
            segments.len() as u32
        };
        let segments_done = if record.state == TaskState::Completed {
            segments_total
        } else {
            segments.iter().filter(|s| s.is_done()).count() as u32
        };
        let fraction = record.fraction(received);
        let remaining = record.total_size.map(|t| t.saturating_sub(received));
        let eta_secs = match (record.state, remaining) {
            (TaskState::Downloading, Some(rem)) => speed.eta_seconds(rem),
            _ => f64::INFINITY,
        };
        TaskSnapshot {
            id: record.id,
            url: record.url.clone(),
            filename: record.filename.clone(),
            dest_dir: record.dest_dir.clone(),
            state: record.state,
            state_label: record.state.label().to_string(),
            total_size: record.total_size,
            received,
            fraction,
            speed_bps: if record.state == TaskState::Downloading {
                speed.bps()
            } else {
                0.0
            },
            eta_secs,
            segments_done,
            segments_total,
            active_requests: self.active_requests.load(Ordering::Relaxed),
            error: record.error.clone(),
            transport: record.transport,
            transport_summary: record.transport_summary.clone(),
            paths: self.lanes.paths(),
            created_at: record.created_at,
            started_at: record.started_at,
            finished_at: record.finished_at,
            final_path: util::display_path(&record.final_path()),
            expected_hash: record.expected_hash.as_ref().map(|h| h.display()),
            computed_hash: record.computed_hash.clone(),
            attempts: record.attempts,
            verify_bps,
        }
    }
}

/// One injectable fault for the live download worker.
///
/// Production never arms this. An empty slot leaves every checkpoint, save,
/// segment write, and finish sync on its existing call. Slice 5b declares
/// the variants; they are how a verification chain reaches the real worker
/// without a permit registry.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub enum WorkerFault {
    /// The next armed segment write accepts at most `accept` bytes, then
    /// the following write fails.
    ShortWrite { accept: u64 },
    /// The next armed segment write fails with `ErrorKind::StorageFull`
    /// and writes nothing.
    StorageFull,
    /// The next checkpoint prepare whose segment `done` sum is at least
    /// `min_done` fails its data sync and is not committed.
    CheckpointSync { min_done: u64 },
    /// The next checkpoint prepare whose segment `done` sum is at least
    /// `min_done` syncs, then the database commit is skipped.
    CheckpointStore { min_done: u64 },
    /// The next pre-rename sync in `finish` fails before hashing or renaming.
    FinishSync,
}

/// Which armed write `run_segment` should wrap. Absent means the plain file.
pub(super) enum SegmentWriteFault {
    Short { accept: u64 },
    StorageFull,
}

/// State shared by the supervisor, workers and the UI.
pub(crate) struct SharedState {
    pub store: Arc<Store>,
    pub net: NetStack,
    pub settings: RwLock<Settings>,
    pub cells: RwLock<HashMap<u64, Arc<TaskCell>>>,
    /// Task ids, newest first.
    pub order: RwLock<Vec<u64>>,
    pub events: flume::Sender<Event>,
    pub stopping: AtomicBool,
    /// A legacy filename is reusable only by its sole persisted segment owner.
    /// `None` reserves ambiguous old data without letting either task touch it.
    partial_owners: Mutex<HashMap<PathBuf, Option<u64>>>,
    pending_removals: Mutex<HashMap<u64, bool>>,
    /// Empty unless a test armed [`WorkerFault`]. Not a production control.
    worker_fault: Mutex<Option<WorkerFault>>,
    worker_faults_fired: AtomicU32,
    /// Serialize task snapshots with deletion so a concurrent shutdown flush
    /// cannot write a record back after removal committed.
    persistence: Mutex<()>,
}

impl std::fmt::Debug for SharedState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedState")
            .field("tasks", &self.cells.read().len())
            .finish()
    }
}

impl SharedState {
    fn open_partial(&self, id: u64, generation: u64, path: &Path) -> Result<(fs::File, u64)> {
        let key = partial_key(path);
        let mut owners = self.partial_owners.lock();
        // A superseded run must not swap the checkpoint handle of the
        // current run, nor claim a partial on its behalf. Hold the segments
        // lock across the claim so the fence cannot straddle a bump.
        let cell = self.cell(id);
        let _coverage = cell.as_ref().map(|cell| cell.segments.read());
        if let Some(cell) = &cell {
            cell.fence(generation)?;
        }
        let file = match owners.get(&key) {
            Some(Some(owner)) if *owner == id => {
                match fs::OpenOptions::new().write(true).open(path) {
                    Ok(file) => file,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(path)
                        .map_err(|e| Error::io(path, e))?,
                    Err(e) => return Err(Error::io(path, e)),
                }
            }
            Some(_) => return Err(Error::Conflict(path.to_path_buf())),
            None => {
                // A stray file is not ours simply because its name matches.
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(path)
                    .map_err(|e| {
                        if e.kind() == std::io::ErrorKind::AlreadyExists {
                            Error::Conflict(path.to_path_buf())
                        } else {
                            Error::io(path, e)
                        }
                    })?
            }
        };
        let len = file.metadata().map_err(|e| Error::io(path, e))?.len();
        if let Some(cell) = &cell {
            *cell.checkpoint_file.lock() = Some(file.try_clone().map_err(|e| Error::io(path, e))?);
        }
        owners.insert(key, Some(id));
        Ok((file, len))
    }

    fn owns_partial(&self, id: u64, path: &Path) -> bool {
        self.partial_owners.lock().get(&partial_key(path)) == Some(&Some(id))
    }

    fn release_partial(&self, id: u64, path: &Path) {
        let key = partial_key(path);
        let mut owners = self.partial_owners.lock();
        if owners.get(&key) == Some(&Some(id)) {
            owners.remove(&key);
        }
    }

    pub fn cell(&self, id: u64) -> Option<Arc<TaskCell>> {
        self.cells.read().get(&id).cloned()
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().clone()
    }

    pub fn emit(&self, event: Event) {
        if let Err(e) = self.events.send(event) {
            tracing::debug!("event dropped (no listener): {e}");
        }
    }

    pub fn log(&self, level: LogLevel, message: impl Into<String>) {
        let message = message.into();
        match level {
            LogLevel::Error => tracing::error!("{message}"),
            LogLevel::Warn => tracing::warn!("{message}"),
            LogLevel::Info => tracing::info!("{message}"),
            LogLevel::Debug => tracing::debug!("{message}"),
        }
        self.emit(Event::Log { level, message });
    }

    /// Snapshot of every known task, newest first.
    pub fn snapshot(&self) -> Vec<TaskSnapshot> {
        let cells = self.cells.read();
        let order = self.order.read();
        let mut out: Vec<TaskSnapshot> = order
            .iter()
            .filter_map(|id| cells.get(id))
            .map(|cell| cell.snapshot())
            .collect();
        // Any task that is not in `order` (should not happen) still shows up.
        if out.len() != cells.len() {
            let seen: std::collections::HashSet<u64> = out.iter().map(|s| s.id).collect();
            for (id, cell) in cells.iter() {
                if !seen.contains(id) {
                    out.push(cell.snapshot());
                }
            }
        }
        out
    }

    pub fn totals(&self) -> Totals {
        let mut totals = Totals::default();
        for cell in self.cells.read().values() {
            let record = cell.record.read();
            totals.tasks += 1;
            match record.state {
                TaskState::Downloading | TaskState::Probing | TaskState::Verifying => {
                    totals.active += 1;
                    totals.active_bytes += cell.received.load(Ordering::Relaxed);
                    totals.active_speed += cell.speed.read().bps();
                }
                TaskState::Queued => totals.queued += 1,
                TaskState::Paused => totals.paused += 1,
                TaskState::Completed => {
                    totals.completed += 1;
                    totals.completed_bytes += record.total_size.unwrap_or(0);
                }
                TaskState::Failed => totals.failed += 1,
                TaskState::Canceled => totals.canceled += 1,
            }
        }
        totals
    }

    /// Write one task's record + segments to redb.
    ///
    /// Supervisor commands use this. A running task uses [`Self::persist_owned`]
    /// so a stale generation saves nothing.
    pub fn persist(&self, cell: &TaskCell, durable: bool) -> Result<()> {
        self.persist_owned(cell, durable, None)
    }

    /// Like [`Self::persist`]. `generation` is the run that is saving. A stale
    /// generation saves nothing and does not clear `dirty`.
    pub(super) fn persist_owned(
        &self,
        cell: &TaskCell,
        durable: bool,
        generation: Option<u64>,
    ) -> Result<()> {
        let _persistence = self.persistence.lock();
        if self.cell(cell.id).is_none() {
            return Ok(());
        }
        self.save_locked(&[cell], durable, generation)
    }

    /// Called only after this range's writer has joined. The persistence mutex
    /// prevents a concurrent save from re-publishing its uncommitted cursor.
    /// The DB receipt, never live progress, is the handoff source.
    pub(super) fn reclaim_range(
        &self,
        cell: &TaskCell,
        generation: u64,
        index: usize,
    ) -> Result<u64> {
        let _persistence = self.persistence.lock();
        let stored = self
            .store
            .get_task(cell.id)?
            .ok_or(Error::NotFound(cell.id))?;
        let committed = self.store.get_segments(cell.id)?;
        let record = cell.record.read();
        let mut segments = cell.segments.write();
        cell.fence(generation)?;
        if stored.representation_bound != record.representation_bound {
            return Err(Error::RemoteChanged);
        }
        let range = segments
            .get_mut(index)
            .ok_or_else(|| Error::Other("missing reclaimed range".into()))?;
        let prefix = committed.get(index).filter(|s| {
            stored.progress_checkpoint.is_some() && s.start == range.start && s.end == range.end
        });
        range.done = prefix.map_or(0, |s| s.done);
        range.finished = prefix.is_some_and(|s| s.finished);
        let mut epochs = cell.allocation_epochs.lock();
        epochs[index] += 1;
        let epoch = epochs[index];
        range.allocation_epoch = epoch;
        cell.lanes.allocation(
            generation,
            index,
            epoch,
            "reclaim-committed-prefix",
            range.done,
        );
        cell.received
            .store(segments.iter().map(|s| s.done).sum(), Ordering::Relaxed);
        cell.bump_persist_epoch();
        Ok(epoch)
    }

    fn prepare_checkpoint(
        &self,
        cell: &TaskCell,
        record: &mut TaskRecord,
        segments: &[SegmentState],
    ) -> Result<()> {
        let file = cell.checkpoint_file.lock();
        match file.as_ref() {
            Some(file) => {
                // A planted prefix persists below `min_done` and must keep
                // the real sync. Only the advanced snapshot is the fault.
                let done = segments.iter().map(|segment| segment.done).sum();
                if self.take_checkpoint_sync(done) {
                    crate::durability::prepare(record, segments, Some(file), |_| {
                        Err(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            "injected disk sync failure",
                        ))
                    })
                } else {
                    record.prepare_progress_checkpoint(segments, file)
                }
            }
            None => crate::durability::prepare(record, segments, None, fs::File::sync_all),
        }
    }

    fn take_checkpoint_sync(&self, done: u64) -> bool {
        let mut slot = self.worker_fault.lock();
        let fires = matches!(
            slot.as_ref(),
            Some(WorkerFault::CheckpointSync { min_done }) if done >= *min_done
        );
        if fires {
            *slot = None;
            self.worker_faults_fired.fetch_add(1, Ordering::SeqCst);
        }
        fires
    }

    /// Sync has already succeeded for this snapshot. Skipping `Store::save`
    /// is the store-failure half of the checkpoint protocol.
    fn reject_advanced_checkpoint_store(&self, ready: &[Snap<'_>]) -> bool {
        let mut slot = self.worker_fault.lock();
        let fires = match slot.as_ref() {
            Some(WorkerFault::CheckpointStore { min_done }) => {
                let min_done = *min_done;
                ready.iter().any(|snap| {
                    snap.segments
                        .iter()
                        .map(|segment| segment.done)
                        .sum::<u64>()
                        >= min_done
                })
            }
            _ => false,
        };
        if fires {
            *slot = None;
            self.worker_faults_fired.fetch_add(1, Ordering::SeqCst);
        }
        fires
    }

    pub(super) fn take_segment_write_fault(&self) -> Option<SegmentWriteFault> {
        let mut slot = self.worker_fault.lock();
        let taken = match slot.as_ref() {
            Some(WorkerFault::ShortWrite { accept }) => {
                Some(SegmentWriteFault::Short { accept: *accept })
            }
            Some(WorkerFault::StorageFull) => Some(SegmentWriteFault::StorageFull),
            _ => None,
        };
        if taken.is_some() {
            *slot = None;
            self.worker_faults_fired.fetch_add(1, Ordering::SeqCst);
        }
        taken
    }

    pub(super) fn take_finish_sync_fault(&self) -> bool {
        let mut slot = self.worker_fault.lock();
        if matches!(slot.as_ref(), Some(WorkerFault::FinishSync)) {
            *slot = None;
            self.worker_faults_fired.fetch_add(1, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn arm_worker_fault(&self, fault: WorkerFault) {
        *self.worker_fault.lock() = Some(fault);
    }

    fn worker_faults_fired(&self) -> u32 {
        self.worker_faults_fired.load(Ordering::SeqCst)
    }

    /// Save one consistent epoch of each cell. If the epoch moves during the
    /// write, the newer pair is saved before this returns. A stale run
    /// (`generation` does not match) saves nothing.
    fn save_locked(
        &self,
        cells: &[&TaskCell],
        durable: bool,
        generation: Option<u64>,
    ) -> Result<()> {
        if cells.is_empty() {
            return Ok(());
        }
        loop {
            let mut ready = Vec::with_capacity(cells.len());
            for cell in cells {
                match snapshot_for_save(cell, generation) {
                    Shot::Stale => {
                        if cells.len() == 1 {
                            return Ok(());
                        }
                    }
                    Shot::Ready(snap) => ready.push(snap),
                }
            }
            if ready.is_empty() {
                return Ok(());
            }
            for snap in &ready {
                snap.cell.dirty.store(false, Ordering::Relaxed);
            }
            let mut records = Vec::with_capacity(ready.len());
            let mut segments = Vec::with_capacity(ready.len());
            for snap in &ready {
                let mut record = snap.record.clone();
                let written: u64 = snap.segments.iter().map(|segment| segment.done).sum();
                if written > 0 && record.finalization.is_none() {
                    if let Some(gate) = snap
                        .cell
                        .proof_gate(proof::ProofPoint::AfterWriteBeforeSync)
                    {
                        gate.wait_sync(written);
                    }
                }
                if let Err(error) = self.prepare_checkpoint(snap.cell, &mut record, &snap.segments)
                {
                    for snap in &ready {
                        snap.cell.dirty.store(true, Ordering::Relaxed);
                    }
                    return Err(error);
                }
                records.push(record);
                segments.push((snap.cell.id, snap.segments.clone()));
            }
            for (snap, record) in ready.iter().zip(&records) {
                if record.progress_checkpoint.is_some() {
                    if let Some(gate) = snap
                        .cell
                        .proof_gate(proof::ProofPoint::AfterSyncBeforeTransaction)
                    {
                        gate.wait_sync(snap.segments.iter().map(|segment| segment.done).sum());
                    }
                }
            }
            if self.reject_advanced_checkpoint_store(&ready) {
                for snap in &ready {
                    snap.cell.dirty.store(true, Ordering::Relaxed);
                }
                return Err(Error::Storage("injected save failure (test fault)".into()));
            }
            if let Err(error) = self.store.save(&records, &segments, durable) {
                for snap in &ready {
                    snap.cell.dirty.store(true, Ordering::Relaxed);
                }
                return Err(error);
            }
            for snap in &ready {
                let epochs = snap.cell.allocation_epochs.lock();
                for (index, range) in snap.segments.iter().enumerate() {
                    if !epochs.is_empty() {
                        snap.cell.lanes.allocation(
                            snap.cell.generation.load(Ordering::Acquire),
                            index,
                            epochs.get(index).copied().unwrap_or(0),
                            "checkpoint-committed",
                            range.done,
                        );
                    }
                }
            }
            for (snap, record) in ready.iter().zip(&records) {
                if record.progress_checkpoint.is_some() {
                    if let Some(gate) = snap
                        .cell
                        .proof_gate(proof::ProofPoint::TransactionCommittedBeforeAcknowledgment)
                    {
                        gate.wait_sync(snap.segments.iter().map(|segment| segment.done).sum());
                    }
                }
            }
            let moved = ready
                .iter()
                .any(|snap| snap.cell.persist_epoch.load(Ordering::Acquire) != snap.epoch);
            if !moved {
                return Ok(());
            }
            for snap in &ready {
                snap.cell.dirty.store(true, Ordering::Relaxed);
            }
        }
    }

    pub(super) fn finalize_file(
        &self,
        cell: &TaskCell,
        generation: u64,
        source: &Path,
        target: &Path,
        content: ExpectedHash,
        policy: crate::config::ConflictPolicy,
    ) -> Result<PathBuf> {
        let _persistence = self.persistence.lock();
        if self.cell(cell.id).is_none() {
            return Err(Error::NotFound(cell.id));
        }
        if cell.token().is_cancelled() {
            return Err(Error::Cancelled);
        }
        let mut record = cell.record.write();
        let mut segments = cell.segments.write();
        // Fence under the persistence mutex and the segments lock, before
        // the Prepared intent: a superseded run can neither journal nor
        // publish a completion.
        cell.fence(generation)?;
        let file = cell.checkpoint_file.lock();
        let held = file
            .as_ref()
            .ok_or_else(|| Error::Finalize("no claimed backing handle".into()))?;
        let finalized = crate::finalize::complete(
            &self.store,
            &mut record,
            &mut segments,
            source,
            target,
            policy,
            content,
            held,
            |_| Ok(()),
        )
        .map_err(|e| Error::Finalize(e.to_string()));
        // `complete` writes `total_size` and clears the segment vector while
        // both locks are held. The epoch bump is the pair a later saver clones.
        cell.bump_persist_epoch();
        finalized
    }

    pub(super) fn recover_finalization(&self, cell: &TaskCell, generation: u64) -> Result<PathBuf> {
        let _persistence = self.persistence.lock();
        let mut live = cell.record.write();
        let mut segments = cell.segments.write();
        cell.fence(generation)?;
        let recovered = crate::finalize::recover_record(&self.store, &live)?;
        *live = recovered;
        let completed = live.state == TaskState::Completed;
        if completed {
            segments.clear();
        }
        let failure = if completed {
            None
        } else {
            Some(
                live.error
                    .clone()
                    .unwrap_or_else(|| "unresolved transaction".into()),
            )
        };
        cell.bump_persist_epoch();
        match failure {
            Some(message) => Err(Error::Finalize(message)),
            None => Ok(live.final_path()),
        }
    }

    /// Atomically publish a staged record mutation (G2 route lock, Zed
    /// re-review round 3, P1).
    ///
    /// `stage` receives a clone of the live record and returns the candidate
    /// to persist, or `None` to abort without touching anything. The
    /// candidate is saved while holding the persistence mutex â€” the same
    /// mutex [`SharedState::persist`], [`SharedState::flush_dirty`] and
    /// [`SharedState::flush_all`] hold for their whole snapshot+save â€” so no
    /// concurrent flush or command persist can observe or durably write the
    /// candidate before its own save has succeeded. Only after the save
    /// returns Ok are the staged fields copied back into the live record,
    /// still under the mutex: concurrent changes to *other* fields (a pause
    /// or cancel landing mid-lock) survive, because publication copies only
    /// the fields named in `apply`. On a failed save the live record keeps
    /// its prior route, destination and filename untouched.
    pub fn persist_staged(
        &self,
        cell: &TaskCell,
        generation: Option<u64>,
        stage: impl Fn(&mut TaskRecord) -> Option<()>,
        apply: impl Fn(&mut TaskRecord, &TaskRecord),
    ) -> Result<()> {
        let _persistence = self.persistence.lock();
        if self.cell(cell.id).is_none() {
            return Ok(());
        }
        loop {
            let snap = match snapshot_for_save(cell, generation) {
                Shot::Stale => {
                    return match generation {
                        Some(generation) => cell.fence(generation).map(|_| ()),
                        None => Ok(()),
                    };
                }
                Shot::Ready(snap) => snap,
            };
            let mut candidate = snap.record.clone();
            if stage(&mut candidate).is_none() {
                return Ok(());
            }
            cell.dirty.store(false, Ordering::Relaxed);
            if let Err(error) = self.prepare_checkpoint(cell, &mut candidate, &snap.segments) {
                cell.dirty.store(true, Ordering::Relaxed);
                return Err(error);
            }
            if let Err(error) = self.store.save(
                &[candidate.clone()],
                &[(cell.id, snap.segments.clone())],
                true,
            ) {
                cell.dirty.store(true, Ordering::Relaxed);
                return Err(error);
            }
            if cell.persist_epoch.load(Ordering::Acquire) != snap.epoch {
                cell.dirty.store(true, Ordering::Relaxed);
                continue;
            }
            let mut live = cell.record.write();
            let _segments = cell.segments.read();
            apply(&mut live, &candidate);
            return Ok(());
        }
    }

    /// Flush every dirty task.
    ///
    /// The commit is durable on purpose: redb's `Durability::None` keeps data
    /// *only in process memory* until a later durable commit, so a non-durable
    /// progress flush would be worthless after a crash or a hard kill - the
    /// exact scenario resume exists for. One small fsync per interval is a
    /// fair price for not re-downloading gigabytes.
    pub fn flush_dirty(&self) {
        let _persistence = self.persistence.lock();
        let dirty: Vec<Arc<TaskCell>> = self
            .cells
            .read()
            .values()
            .filter(|c| c.dirty.load(Ordering::Relaxed))
            .cloned()
            .collect();
        if dirty.is_empty() {
            return;
        }
        let refs: Vec<&TaskCell> = dirty.iter().map(|cell| cell.as_ref()).collect();
        let total_done: u64 = refs
            .iter()
            .map(|cell| {
                cell.segments
                    .read()
                    .iter()
                    .map(|segment| segment.done)
                    .sum::<u64>()
            })
            .sum();
        match self.save_locked(&refs, true, None) {
            Ok(()) => {
                tracing::trace!(tasks = dirty.len(), total_done, "progress flushed");
            }
            Err(e) => {
                tracing::warn!("progress flush failed: {e}");
                // A failed checkpoint is storage failure, not a dead route.
                // S1 must drain both writers before publishing that error.
                self.stop_family_writers_for_storage(&refs, &e);
            }
        }
    }

    fn stop_family_writers_for_storage(&self, cells: &[&TaskCell], error: &Error) {
        for cell in cells {
            if cell.uses_family_paths.load(Ordering::Acquire)
                && cell.running.load(Ordering::Acquire)
            {
                *cell.storage_failure.lock() =
                    Some((cell.generation.load(Ordering::Acquire), error.short()));
                cell.token().cancel();
            }
        }
    }

    /// Durable flush of everything (shutdown, pause, completion).
    pub fn flush_all(&self) {
        let _persistence = self.persistence.lock();
        let cells: Vec<Arc<TaskCell>> = self.cells.read().values().cloned().collect();
        let refs: Vec<&TaskCell> = cells.iter().map(|cell| cell.as_ref()).collect();
        if let Err(e) = self.save_locked(&refs, true, None) {
            tracing::error!("final flush failed: {e}");
            self.stop_family_writers_for_storage(&refs, &e);
        }
    }
}

struct Snap<'a> {
    cell: &'a TaskCell,
    record: TaskRecord,
    segments: Vec<SegmentState>,
    epoch: u64,
}

enum Shot<'a> {
    Stale,
    Ready(Snap<'a>),
}

/// Clone the record and the segments under both locks. If `persist_epoch`
/// moves, the pair is discarded and the read is repeated. A run whose
/// generation is stale saves nothing.
fn snapshot_for_save(cell: &TaskCell, generation: Option<u64>) -> Shot<'_> {
    loop {
        let epoch = cell.persist_epoch.load(Ordering::Acquire);
        let record_guard = cell.record.read();
        let segments_guard = cell.segments.read();
        if let Some(generation) = generation {
            if cell.fence(generation).is_err() {
                return Shot::Stale;
            }
        }
        let record = record_guard.clone();
        let segments = segments_guard.clone();
        let after = cell.persist_epoch.load(Ordering::Acquire);
        drop(segments_guard);
        drop(record_guard);
        if epoch == after {
            return Shot::Ready(Snap {
                cell,
                record,
                segments,
                epoch,
            });
        }
    }
}

/// Aggregate numbers for the status bar.
#[derive(Clone, Copy, Debug, Default)]
pub struct Totals {
    pub tasks: u64,
    pub active: u64,
    pub queued: u64,
    pub paused: u64,
    pub completed: u64,
    pub failed: u64,
    pub canceled: u64,
    pub active_bytes: u64,
    pub completed_bytes: u64,
    pub active_speed: f64,
}

/// The download manager. Clone-free, thread-safe, `Send + Sync`.
pub struct Engine {
    state: Arc<SharedState>,
    cmd: flume::Sender<Command>,
    events: flume::Receiver<Event>,
    store: Arc<Store>,
    paths: Paths,
    runtime: RwLock<Option<Runtime>>,
}

impl std::fmt::Debug for Engine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Engine")
            .field("tasks", &self.state.cells.read().len())
            .field("paths", &self.paths.root)
            .finish()
    }
}

/// Builder for [`Engine`].
#[derive(Default)]
pub struct EngineBuilder {
    paths: Option<Paths>,
    settings: Option<Settings>,
    worker_threads: Option<usize>,
    alt_svc: Option<Arc<dyn AltSvcStore>>,
    /// Do not start transferring anything (used by `udm-cli list`).
    idle: bool,
}

impl std::fmt::Debug for EngineBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineBuilder")
            .field("paths", &self.paths)
            .field("worker_threads", &self.worker_threads)
            .field("idle", &self.idle)
            .finish()
    }
}

impl EngineBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn paths(mut self, paths: Paths) -> Self {
        self.paths = Some(paths);
        self
    }

    pub fn settings(mut self, settings: Settings) -> Self {
        self.settings = Some(settings);
        self
    }

    pub fn worker_threads(mut self, n: usize) -> Self {
        self.worker_threads = Some(n);
        self
    }

    /// Override the `Alt-Svc` store (default: the redb-backed one).
    pub fn alt_svc_store(mut self, store: Arc<dyn AltSvcStore>) -> Self {
        self.alt_svc = Some(store);
        self
    }

    /// Load tasks but do not resume anything automatically.
    pub fn idle(mut self, yes: bool) -> Self {
        self.idle = yes;
        self
    }

    pub fn build(self) -> Result<Engine> {
        let paths = match self.paths {
            Some(p) => p,
            None => Paths::discover()?,
        };
        paths.ensure_dirs()?;

        let store = Arc::new(Store::open(&paths.db_file)?);
        store.recover_finalizations()?;
        // A crash or a hard power-off must never lead to silent auto-resume.
        let recovered = store.recover_interrupted()?;
        if recovered > 0 {
            tracing::info!("{recovered} interrupted task(s) were set to Paused");
        }

        let mut settings = store.load_settings()?.unwrap_or_default();
        if let Some(override_settings) = self.settings {
            settings = override_settings;
            settings.sanitize();
            store.save_settings(&settings)?;
        }
        settings.sanitize();

        let alt_svc: Arc<dyn AltSvcStore> = self.alt_svc.unwrap_or_else(|| store.clone());

        // The runtime must exist *before* the network stack: quinn spawns its
        // endpoint driver onto the current tokio runtime, and the GUI calls
        // build() from a plain (non-async) main thread - without this, QUIC
        // fails with "no async runtime found" and HTTP/3 silently disappears
        // from every GUI session. The enter() guard makes this runtime the
        // ambient one for NetStack::new.
        let worker_threads = self
            .worker_threads
            .unwrap_or_else(|| available_parallelism().clamp(2, 8));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(worker_threads)
            .thread_name("udm-engine")
            .enable_all()
            .build()
            .map_err(|e| Error::Other(format!("cannot start the engine runtime: {e}")))?;
        let net = {
            let _rt_guard = runtime.enter();
            NetStack::new(&settings, alt_svc)?
        };

        let (events_tx, events_rx) = flume::unbounded::<Event>();
        let (cmd_tx, cmd_rx) = flume::unbounded::<Command>();

        let state = Arc::new(SharedState {
            store: store.clone(),
            net,
            settings: RwLock::new(settings.clone()),
            cells: RwLock::new(HashMap::new()),
            order: RwLock::new(Vec::new()),
            events: events_tx,
            stopping: AtomicBool::new(false),
            partial_owners: Mutex::new(HashMap::new()),
            pending_removals: Mutex::new(HashMap::new()),
            persistence: Mutex::new(()),
            worker_fault: Mutex::new(None),
            worker_faults_fired: AtomicU32::new(0),
        });

        // Load persisted tasks (oldest first) into memory.
        {
            let mut cells = state.cells.write();
            let mut order = state.order.write();
            for StoredTask {
                mut record,
                mut segments,
            } in store.list_tasks()?
            {
                let id = record.id;
                if !segments.is_empty()
                    && !record.filename.is_empty()
                    && record.state != TaskState::Completed
                {
                    let key = partial_key(&record.part_path());
                    state
                        .partial_owners
                        .lock()
                        .entry(key)
                        .and_modify(|owner| *owner = None)
                        .or_insert(Some(id));
                }
                if record.state != TaskState::Completed {
                    if crate::durability::recover(&mut record, &mut segments) {
                        state.log(
                            LogLevel::Warn,
                            format!("#{id} unproven progress will be re-fetched; partial retained"),
                        );
                    }
                }
                let cell = TaskCell::new(record, segments);
                cells.insert(id, cell.clone());
                order.insert(0, id);
            }
        }

        let supervisor_state = state.clone();
        runtime.spawn(supervise(supervisor_state, cmd_rx, self.idle));

        Ok(Engine {
            state,
            cmd: cmd_tx,
            events: events_rx,
            store,
            paths,
            runtime: RwLock::new(Some(runtime)),
        })
    }
}

fn available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

/// Resolve directory aliases, including case aliases on Windows, before ownership checks.
fn partial_key(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let parent = absolute.parent().unwrap_or(Path::new("."));
    let canonical = dunce::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let key = canonical.join(absolute.file_name().unwrap_or_default());
    #[cfg(windows)]
    let key = PathBuf::from(key.to_string_lossy().to_lowercase());
    key
}

/// States `udm-cli watch` resumes on its own.
///
/// `Failed` is excluded. Explicit retry is the way out of a failure.
/// [`Engine::resume`] on a `Failed` task stays available to that caller.
pub fn watch_resumes(state: TaskState) -> bool {
    !state.is_terminal()
}

/// Why the oracle says a draining run stopped. Production commands do not
/// construct this.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub enum OracleStop {
    None,
    Pause,
    Cancel,
    Shutdown,
    Remove,
}

/// One draining-run publication, applied by [`Engine::oracle_apply_exit`].
///
/// The integration oracle cannot see `pub(crate)` task state. This is the
/// real `publish_exit`, not a second copy of the matrix. Production commands
/// do not call it.
#[doc(hidden)]
#[derive(Clone, Debug)]
pub struct OracleExit {
    pub run_generation: u64,
    pub current_generation: u64,
    pub prior: TaskState,
    pub prior_error: Option<String>,
    pub stop: OracleStop,
    /// `Some` is a definitive error. `None` is a clean stop.
    pub definitive: Option<String>,
}

/// Post-wake ownership predicate used by the retry loop.
///
/// `token_cancelled` is the cancel token, `generation_ok` is `TaskCell::fence`,
/// and `state` is the record, in that order.
#[doc(hidden)]
pub fn oracle_owns_attempt(token_cancelled: bool, generation_ok: bool, state: TaskState) -> bool {
    task::owns_attempt(token_cancelled, generation_ok, state)
}

impl Engine {
    /// Convenience: discover paths and start.
    pub fn start() -> Result<Engine> {
        EngineBuilder::new().build()
    }

    pub fn paths(&self) -> &Paths {
        &self.paths
    }

    pub fn store(&self) -> Arc<Store> {
        self.store.clone()
    }

    /// Arm one live-worker fault. Production never calls this. The slot
    /// starts empty, and an empty slot does not change writes, syncs, or saves.
    #[doc(hidden)]
    pub fn arm_worker_fault(&self, fault: WorkerFault) {
        self.state.arm_worker_fault(fault);
    }

    /// How many armed faults have been consumed. A chain uses this to prove
    /// the injection ran, not only that the task ended.
    #[doc(hidden)]
    pub fn worker_faults_fired(&self) -> u32 {
        self.state.worker_faults_fired()
    }

    pub fn net(&self) -> NetStack {
        self.state.net.clone()
    }

    /// Build an HTTP/3 pool on this engine's runtime and install it.
    ///
    /// [`EngineBuilder::build`] still calls [`NetStack::new`], which calls
    /// [`crate::net::h3::H3Pool::new`] and trusts the operating-system roots.
    /// A dev test passes a [`quinn::ClientConfig`] it built itself. This
    /// method does not inspect that config's trust anchors.
    pub fn install_h3_client_config(
        &self,
        client: quinn::ClientConfig,
    ) -> Result<Arc<crate::net::h3::H3Pool>> {
        let runtime = self.runtime.read();
        let runtime = runtime
            .as_ref()
            .ok_or_else(|| Error::Other("the engine runtime has stopped".to_string()))?;
        // Quinn's endpoint driver has to spawn on the runtime that will poll
        // the connection. The test thread is not that runtime.
        let _enter = runtime.enter();
        let settings = self.settings();
        let pool = Arc::new(crate::net::h3::H3Pool::with_client_config(
            Duration::from_secs(settings.connect_timeout_secs),
            &settings.user_agent,
            client,
        )?);
        self.state.net.install_h3_pool(Arc::clone(&pool));
        Ok(pool)
    }

    pub fn settings(&self) -> Settings {
        self.state.settings()
    }

    pub fn events(&self) -> &flume::Receiver<Event> {
        &self.events
    }

    /// Drain up to `max` pending events without blocking.
    pub fn drain_events(&self, max: usize) -> Vec<Event> {
        let mut out = Vec::new();
        while out.len() < max {
            match self.events.try_recv() {
                Ok(e) => out.push(e),
                Err(_) => break,
            }
        }
        out
    }

    pub fn snapshot(&self) -> Vec<TaskSnapshot> {
        self.state.snapshot()
    }

    pub fn totals(&self) -> Totals {
        self.state.totals()
    }

    /// Local proof evidence only; not persisted or included in IPC snapshots.
    #[doc(hidden)]
    pub fn transport_open_evidence(&self, id: u64) -> Option<crate::net::lane::OpenEvidence> {
        self.state.cell(id).map(|cell| cell.lanes.snapshot())
    }

    /// Arm one explicit proof barrier. Never armed by application code.
    #[doc(hidden)]
    pub fn install_proof_barrier(&self, id: u64, barrier: Arc<proof::ProofBarrier>) -> Result<()> {
        let cell = self
            .state
            .cell(id)
            .ok_or_else(|| Error::Other("proof task missing".into()))?;
        *cell.proof_barrier.lock() = Some(barrier);
        Ok(())
    }
    pub fn task(&self, id: u64) -> Option<TaskSnapshot> {
        self.state.cell(id).map(|c| c.snapshot())
    }

    /// The task created for a browser-handoff transaction, if any
    /// (G5). Reads the durable store, not an in-memory cache: this is
    /// the handoff coordinator's crash-recovery evidence.
    pub fn task_by_handoff(&self, tx: &str) -> Option<u64> {
        let tasks = self.store.list_tasks().ok()?;
        tasks
            .iter()
            .find(|t| t.record.handoff_tx.as_deref() == Some(tx))
            .map(|t| t.record.id)
    }

    // ------------------------------------------------------------ mutations

    /// Enqueue a download. Returns the new task id.
    pub fn add(&self, request: AddRequest) -> Result<u64> {
        if self.state.stopping.load(Ordering::Relaxed) {
            return Err(Error::Other("the engine is shutting down".into()));
        }
        let url = crate::net::stack::validate(&request.url)?.to_string();
        let settings = self.settings();

        // G2: with an explicit destination (or auto-categorize off) the task
        // takes today's path exactly. With auto on and no explicit
        // destination, the record is anchored at the download root and the
        // route stays pending: `Engine::add` snapshots the effective config
        // (absolute root + validated rules) so later settings changes cannot
        // reroute this task, and the worker locks the route after the probe,
        // before any partial is claimed. No folder is created here.
        let auto_route = request.dest_dir.is_none() && settings.auto_categorize;
        let dest_dir = request
            .dest_dir
            .clone()
            .unwrap_or_else(|| settings.download_dir());
        if !auto_route {
            fs::create_dir_all(&dest_dir)
                .map_err(|e| Error::io(dunce::simplified(&dest_dir).to_path_buf(), e))?;
        }

        let id = self.store.next_task_id()?;
        let filename = request
            .filename
            .as_deref()
            .map(util::sanitize_filename)
            .filter(|f| !f.is_empty());

        let mut record = TaskRecord::new(id, &url, &dest_dir, filename);
        if auto_route {
            let rules = crate::category::validate_categories(&settings.category_rules())?;
            record.route = RouteState::Pending {
                root: util::display_path(&dest_dir),
                rules,
            };
        }
        record.expected_hash = request.expected_hash.clone();
        record.transport_pref = request.transport_pref.unwrap_or(settings.transport_pref);
        record.note = request.note.clone();
        record.handoff_tx = request.handoff_tx.clone();
        record.segments_planned = request.segments.unwrap_or(settings.segments_per_task);
        record.state = if request.start_paused {
            TaskState::Paused
        } else {
            TaskState::Queued
        };

        self.store.put_task(&record)?;
        let cell = TaskCell::new(record, Vec::new());
        self.state.cells.write().insert(id, cell);
        self.state.order.write().insert(0, id);
        self.state.emit(Event::TaskAdded { id });
        self.state.emit(Event::QueueChanged);
        let _ = self.cmd.send(Command::Admit(id));
        Ok(id)
    }

    pub fn pause(&self, id: u64) -> Result<()> {
        self.require(id)?;
        let _ = self.cmd.send(Command::Pause(id));
        Ok(())
    }

    pub fn resume(&self, id: u64) -> Result<()> {
        self.require(id)?;
        let _ = self.cmd.send(Command::Resume(id));
        Ok(())
    }

    pub fn cancel(&self, id: u64) -> Result<()> {
        self.require(id)?;
        let _ = self.cmd.send(Command::Cancel(id));
        Ok(())
    }

    /// Reset the error and try again.
    pub fn retry(&self, id: u64) -> Result<()> {
        self.require(id)?;
        let _ = self.cmd.send(Command::Retry(id));
        Ok(())
    }

    pub fn remove(&self, id: u64, delete_files: bool) -> Result<()> {
        self.require(id)?;
        let _ = self.cmd.send(Command::Remove(id, delete_files));
        Ok(())
    }

    /// Move a task's destination (only while it is not transferring).
    pub fn move_to(&self, id: u64, dest_dir: PathBuf, filename: Option<String>) -> Result<()> {
        self.require(id)?;
        let _ = self.cmd.send(Command::Move {
            id,
            dest_dir,
            filename,
        });
        Ok(())
    }

    pub fn update_settings(&self, settings: Settings) -> Result<()> {
        let _ = self.cmd.send(Command::SetSettings(settings));
        Ok(())
    }

    /// Force a durable write of all progress.
    pub fn flush(&self) -> Result<()> {
        let _ = self.cmd.send(Command::Flush);
        Ok(())
    }

    /// Ask the engine to stop everything and wait for it to settle.
    ///
    /// Idempotent, and callable from any thread (the GUI's close handler, a
    /// `Drop`, or a Ctrl-C handler in the CLI).
    pub fn shutdown(&self) {
        self.state.stopping.store(true, Ordering::Relaxed);
        let _ = self.cmd.send(Command::Shutdown);
        // Give the supervisor a moment to cancel workers and flush redb.
        let deadline = std::time::Instant::now() + Duration::from_secs(6);
        while std::time::Instant::now() < deadline {
            let running = self
                .state
                .cells
                .read()
                .values()
                .filter(|c| c.running.load(Ordering::Relaxed))
                .count();
            if running == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // A run that has not drained by now is quarantined: it is never
        // regranted (the process is exiting) and its in-flight writes are
        // not trusted beyond what the flush below checkpoints from synced data.
        for cell in self.state.cells.read().values() {
            if cell.running.load(Ordering::Acquire) {
                tracing::warn!(
                    "#{} run generation {} did not drain before shutdown",
                    cell.id,
                    cell.generation.load(Ordering::Acquire)
                );
            }
        }
        self.state.flush_all();
        if let Some(runtime) = self.runtime.write().take() {
            // Dropping a tokio Runtime from inside another runtime's async
            // context panics, and `shutdown()` is called from the CLI's
            // async main. Do it on a plain thread instead.
            let _ = std::thread::Builder::new()
                .name("udm-engine-shutdown".into())
                .spawn(move || runtime.shutdown_timeout(Duration::from_secs(3)))
                .and_then(|h| h.join().map_err(|_| std::io::Error::other("join failed")));
        }
        self.state.emit(Event::Shutdown);
    }

    fn require(&self, id: u64) -> Result<()> {
        if self.state.cell(id).is_none() {
            return Err(Error::NotFound(id));
        }
        Ok(())
    }

    /// Apply one draining-run publication to `id`. See [`OracleExit`].
    #[doc(hidden)]
    pub fn oracle_apply_exit(&self, id: u64, exit: OracleExit) -> Result<TaskSnapshot> {
        let cell = self.state.cell(id).ok_or(Error::NotFound(id))?;
        // A draining run holds `running`. Keeping it set stops the supervisor
        // from admitting the task in the middle of the publication.
        let was_running = cell.running.swap(true, Ordering::SeqCst);
        cell.generation
            .store(exit.current_generation, Ordering::Release);
        {
            let mut record = cell.record.write();
            record.state = exit.prior;
            record.error = exit.prior_error;
        }
        let reason = match exit.stop {
            OracleStop::None => stop_reason::NONE,
            OracleStop::Pause => stop_reason::PAUSE,
            OracleStop::Cancel => stop_reason::CANCEL,
            OracleStop::Shutdown => stop_reason::SHUTDOWN,
            OracleStop::Remove => stop_reason::REMOVE,
        };
        cell.stop_reason.store(reason, Ordering::SeqCst);
        let ctx = Arc::new(task::RunCtx {
            state: self.state.clone(),
            settings: self.settings(),
            generation: exit.run_generation,
        });
        if let Some(text) = exit.definitive {
            let error = Error::Other(text);
            task::publish_exit(&ctx, &cell, task::ExitKind::Definitive(&error));
        } else {
            task::publish_exit(&ctx, &cell, task::ExitKind::Clean);
        }
        if !was_running {
            cell.running.store(false, Ordering::SeqCst);
        }
        Ok(cell.snapshot())
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        if self.runtime.read().is_some() {
            self.shutdown();
        }
    }
}

// ---------------------------------------------------------------------------
// supervisor
// ---------------------------------------------------------------------------

async fn supervise(state: Arc<SharedState>, rx: flume::Receiver<Command>, idle: bool) {
    let mut ticker = tokio::time::interval(Duration::from_millis(200));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_sample = util::now_mono();
    let mut last_flush = util::now_mono();

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let now = util::now_mono();
                let elapsed = now.saturating_duration_since(last_sample).as_secs_f64();
                last_sample = now;
                sample_speeds(&state, elapsed);
                reap_removals(&state);
                if !idle {
                    admit_queued(&state);
                }
                // Flush progress once the configured interval has elapsed.
                // (The interval is a real cadence: raising it above 1s must
                // slow flushing down, never switch it off.)
                let interval = state.settings().persist_interval_ms.clamp(200, 60_000);
                if now.saturating_duration_since(last_flush).as_millis() >= u128::from(interval) {
                    last_flush = now;
                    state.flush_dirty();
                }
            }
            result = rx.recv_async() => {
                let cmd = match result {
                    Ok(cmd) => cmd,
                    Err(_) => break, // every Engine handle is gone
                };
                if handle_command(&state, cmd).await {
                    break;
                }
            }
        }
    }

    // Shutdown: stop workers, wait briefly, flush.
    state.stopping.store(true, Ordering::Relaxed);
    for cell in state.cells.read().values() {
        cell.stop_reason
            .store(stop_reason::SHUTDOWN, Ordering::SeqCst);
        cell.token().cancel();
    }
    let deadline = util::now_mono() + Duration::from_secs(5);
    while util::now_mono() < deadline {
        let running = state
            .cells
            .read()
            .values()
            .filter(|c| c.running.load(Ordering::Relaxed))
            .count();
        if running == 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    state.net.shutdown().await;
    reap_removals(&state);
    state.flush_all();
    state.emit(Event::Shutdown);
    tracing::info!("engine stopped");
}

fn sample_speeds(state: &SharedState, elapsed: f64) {
    if elapsed <= 0.0 {
        return;
    }
    for cell in state.cells.read().values() {
        let counter = if cell.state() == TaskState::Verifying {
            cell.verify_progress.load(Ordering::Relaxed)
        } else {
            cell.received.load(Ordering::Relaxed)
        };
        cell.speed.write().sample(counter);
    }
}

/// Start queued tasks while a slot is free (oldest first).
fn admit_queued(state: &Arc<SharedState>) {
    let settings = state.settings();
    let running = state
        .cells
        .read()
        .values()
        .filter(|c| c.running.load(Ordering::Relaxed))
        .count();
    let free = (settings.max_concurrent_tasks as usize).saturating_sub(running);
    if free == 0 {
        return;
    }
    let candidates: Vec<u64> = {
        let cells = state.cells.read();
        let order = state.order.read();
        order
            .iter()
            .rev() // oldest first
            .filter(|id| {
                cells
                    .get(id)
                    .map(|c| {
                        !c.running.load(Ordering::Relaxed) && matches!(c.state(), TaskState::Queued)
                    })
                    .unwrap_or(false)
            })
            .copied()
            .collect()
    };
    for id in candidates.into_iter().take(free) {
        spawn_task(state, id);
    }
}

fn spawn_task(state: &Arc<SharedState>, id: u64) {
    let cell = match state.cell(id) {
        Some(cell) => cell,
        None => return,
    };
    if cell.running.swap(true, Ordering::SeqCst) {
        return; // already running
    }
    // Grant a new run generation only now that `running` was clear: every
    // worker of the previous run has been joined (drained) by `transfer`.
    // Bump under the segments lock, which every fenced coverage write holds,
    // so a fence check and the write it guards cannot straddle the bump.
    let generation = {
        let _coverage = cell.segments.write();
        cell.generation.fetch_add(1, Ordering::AcqRel) + 1
    };
    let token = CancellationToken::new();
    *cell.cancel.write() = token.clone();
    cell.stop_reason.store(stop_reason::NONE, Ordering::SeqCst);
    cell.speed.write().reset();

    let ctx = Arc::new(task::RunCtx {
        state: state.clone(),
        settings: state.settings(),
        generation,
    });
    let handle = tokio::spawn(task::run(ctx, id, token));
    *cell.handle.write() = Some(handle);
    state.emit(Event::QueueChanged);
}

/// Returns `true` when the supervisor should exit.
async fn handle_command(state: &Arc<SharedState>, cmd: Command) -> bool {
    match cmd {
        Command::Admit(id) => {
            if let Some(cell) = state.cell(id) {
                if cell.state() == TaskState::Queued {
                    admit_queued(state);
                }
            }
        }
        Command::Pause(id) => {
            if let Some(cell) = state.cell(id) {
                if cell.running.load(Ordering::Relaxed) {
                    cell.stop_reason.store(stop_reason::PAUSE, Ordering::SeqCst);
                    cell.token().cancel();
                }
                let stored = {
                    let mut record = cell.record.write();
                    if !record.state.is_terminal() {
                        record.state = TaskState::Paused;
                        record.error = None;
                    }
                    record.state
                };
                cell.dirty.store(true, Ordering::Relaxed);
                let _ = state.persist(&cell, true);
                // The event is the state just stored. A terminal task is left
                // alone, so a Failed row must not be announced as Paused.
                state.emit(Event::TaskStateChanged { id, state: stored });
                state.emit(Event::QueueChanged);
            }
        }
        Command::Resume(id) => resume_task(state, id, false),
        Command::Retry(id) => resume_task(state, id, true),
        Command::Cancel(id) => {
            if let Some(cell) = state.cell(id) {
                // Completed stays completed, and its files stay. Cancel of
                // Failed, Paused, Queued, and the active states is unchanged.
                if cell.state() != TaskState::Completed {
                    cell.stop_reason
                        .store(stop_reason::CANCEL, Ordering::SeqCst);
                    cell.token().cancel();
                    {
                        let mut record = cell.record.write();
                        record.state = TaskState::Canceled;
                    }
                    let _ = state.persist(&cell, true);
                    state.emit(Event::TaskStateChanged {
                        id,
                        state: TaskState::Canceled,
                    });
                    state.emit(Event::QueueChanged);
                }
            }
        }
        Command::Remove(id, delete_files) => {
            let maybe_cell = state.cells.read().get(&id).cloned();
            if let Some(cell) = maybe_cell {
                cell.stop_reason
                    .store(stop_reason::REMOVE, Ordering::SeqCst);
                cell.token().cancel();
                // Defer cleanup until workers acknowledge cancellation; their last
                // persist must happen before we delete the record or release the file.
                state
                    .pending_removals
                    .lock()
                    .entry(id)
                    .and_modify(|files| *files |= delete_files)
                    .or_insert(delete_files);
                reap_removals(state);
            }
        }
        Command::Move {
            id,
            dest_dir,
            filename,
        } => {
            if let Some(cell) = state.cell(id) {
                // G2: a task whose category route is not durably locked has
                // no locked destination to move yet; the manual Move
                // operation is out of G2 scope for it.
                let route_pending = cell.record.read().route_unlocked();
                let finalize_pending = cell
                    .record
                    .read()
                    .finalization
                    .as_ref()
                    .is_some_and(crate::finalize::FinalizeIntent::pending);
                if finalize_pending {
                    state.log(
                        LogLevel::Warn,
                        format!("#{id} must resolve its finalization before moving"),
                    );
                } else if route_pending {
                    state.log(
                        LogLevel::Warn,
                        format!("#{id} cannot be moved before its category route is locked"),
                    );
                } else if cell.running.load(Ordering::Relaxed) {
                    state.log(
                        LogLevel::Warn,
                        format!("#{} is transferring; pause it before moving", id),
                    );
                } else {
                    if let Err(e) = fs::create_dir_all(&dest_dir) {
                        state.log(
                            LogLevel::Error,
                            format!("cannot create {}: {e}", util::display_path(&dest_dir)),
                        );
                    } else {
                        {
                            let mut record = cell.record.write();
                            record.dest_dir = util::display_path(&dest_dir);
                            // A manual destination change does not inherit proof
                            // for an earlier artifact in a different directory.
                            record.finalization = None;
                            if let Some(name) = filename {
                                record.filename = util::sanitize_filename(&name);
                            }
                            record.error = None;
                        }
                        let _ = state.persist(&cell, true);
                        state.emit(Event::TaskStateChanged {
                            id,
                            state: cell.state(),
                        });
                    }
                }
            }
        }
        Command::SetSettings(mut settings) => {
            settings.sanitize();
            if let Err(e) = state.store.save_settings(&settings) {
                state.log(LogLevel::Error, format!("cannot save settings: {e}"));
            }
            if let Err(e) = state.net.update_settings(&settings) {
                state.log(LogLevel::Warn, format!("settings applied partially: {e}"));
            }
            *state.settings.write() = settings;
            state.emit(Event::SettingsChanged);
            admit_queued(state);
        }
        Command::Flush => state.flush_all(),
        Command::Shutdown => return true,
    }
    false
}

/// Put a task back into the queue. `reset_attempts` is what "Retry" does.
fn resume_task(state: &Arc<SharedState>, id: u64, reset_attempts: bool) {
    if state.pending_removals.lock().contains_key(&id) {
        state.log(LogLevel::Warn, format!("#{id} is being removed"));
        return;
    }
    let Some(cell) = state.cell(id) else { return };
    let accepted = {
        let mut record = cell.record.write();
        let _segments = cell.segments.write();
        if record.state.is_resumable() || record.state == TaskState::Failed {
            if reset_attempts {
                // Same write as the attempt budget. Resume does not touch it.
                record.attempts = 0;
                record.representation_restarts = 0;
                cell.bump_persist_epoch();
            }
            record.error = None;
            record.state = TaskState::Queued;
            true
        } else {
            false
        }
    };
    if !accepted {
        state.log(
            LogLevel::Warn,
            format!(
                "#{} cannot resume from state `{}`",
                id,
                cell.state().as_str()
            ),
        );
        return;
    }
    cell.stop_reason.store(stop_reason::NONE, Ordering::SeqCst);
    let _ = state.persist(&cell, true);
    state.emit(Event::TaskStateChanged {
        id,
        state: TaskState::Queued,
    });
    admit_queued(state);
}

fn reap_removals(state: &Arc<SharedState>) {
    let pending: Vec<(u64, bool)> = state
        .pending_removals
        .lock()
        .iter()
        .map(|(id, files)| (*id, *files))
        .collect();
    for (id, delete_files) in pending {
        let Some(cell) = state.cell(id) else {
            state.pending_removals.lock().remove(&id);
            continue;
        };
        if cell.running.load(Ordering::Acquire) {
            continue;
        }
        let _persistence = state.persistence.lock();
        let (part, final_path, completed) = {
            let record = cell.record.read();
            (
                record.part_path(),
                record.final_path(),
                record.state == TaskState::Completed,
            )
        };
        if let Err(e) = state.store.delete_task(id) {
            state.log(LogLevel::Error, format!("#{id} remove failed: {e}"));
            state.pending_removals.lock().remove(&id);
            continue;
        }
        if (delete_files || state.settings().delete_part_on_remove) && state.owns_partial(id, &part)
        {
            remove_file_quietly(state, &part);
        }
        state.release_partial(id, &part);
        if delete_files && completed {
            remove_file_quietly(state, &final_path);
        }
        state.cells.write().remove(&id);
        state.order.write().retain(|x| *x != id);
        state.pending_removals.lock().remove(&id);
        state.emit(Event::TaskRemoved { id });
        state.emit(Event::QueueChanged);
    }
}

fn remove_file_quietly(state: &SharedState, path: &Path) {
    match path.try_exists() {
        Ok(true) => {}
        _ => return,
    }
    if let Err(e) = std::fs::remove_file(path) {
        state.log(
            LogLevel::Warn,
            format!("cannot delete {}: {e}", util::display_path(path)),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_engine(dir: &Path) -> Engine {
        let paths = Paths {
            root: dir.to_path_buf(),
            db_file: dir.join("test.redb"),
            log_file: dir.join("logs").join("udm.log"),
            temp_dir: dir.join("tmp"),
            default_download_dir: dir.join("downloads"),
            portable: true,
            exe_dir: Some(dir.to_path_buf()),
        };
        fs::create_dir_all(paths.default_download_dir.clone()).unwrap();
        EngineBuilder::new()
            .paths(paths)
            .idle(true)
            .build()
            .unwrap()
    }

    #[test]
    fn adds_tasks_and_keeps_them_across_restarts() {
        let dir = tempfile::tempdir().unwrap();
        let id = {
            let engine = make_engine(dir.path());
            let id = engine
                .add(AddRequest::new("https://example.com/file.bin").start_paused(true))
                .unwrap();
            assert_eq!(engine.snapshot().len(), 1);
            let snap = engine.task(id).unwrap();
            assert_eq!(snap.state, TaskState::Paused);
            assert_eq!(snap.filename, ""); // resolved on first probe
            engine.shutdown();
            id
        };
        // Reopen: the task is still there.
        let engine = make_engine(dir.path());
        let snap = engine.task(id).expect("task survived a restart");
        assert_eq!(snap.url, "https://example.com/file.bin");
        assert_eq!(snap.state, TaskState::Paused);
        engine.shutdown();
    }

    #[test]
    fn rejects_non_http_urls() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_engine(dir.path());
        assert!(engine.add(AddRequest::new("ftp://example.com/x")).is_err());
        assert!(engine.add(AddRequest::new("")).is_err());
        engine.shutdown();
    }

    #[test]
    fn removing_an_unstarted_task_preserves_unclaimed_partial() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_engine(dir.path());
        let downloads = dir.path().join("downloads");
        let id = engine
            .add(
                AddRequest::new("https://example.com/f.bin")
                    .dest_dir(downloads.clone())
                    .filename("f.bin")
                    .start_paused(true),
            )
            .unwrap();
        std::fs::write(downloads.join("f.bin.udm.part"), b"partial").unwrap();
        // `remove` is processed by the supervisor; drive it via the channel.
        engine.remove(id, true).unwrap();
        for _ in 0..100 {
            if engine.task(id).is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(engine.task(id).is_none());
        assert!(downloads.join("f.bin.udm.part").exists());
        engine.shutdown();
    }

    #[test]
    fn removing_a_legacy_owner_deletes_its_partial() {
        let dir = tempfile::tempdir().unwrap();
        let downloads = dir.path().join("downloads");
        fs::create_dir_all(&downloads).unwrap();
        let store = Store::open(dir.path().join("test.redb")).unwrap();
        let id = store.next_task_id().unwrap();
        let mut record = TaskRecord::new(
            id,
            "https://example.com/f",
            &downloads,
            Some("f.bin".into()),
        );
        record.state = TaskState::Paused;
        let segment = SegmentState::new(0, 0, 6);
        store
            .save(&[record.clone()], &[(id, vec![segment])], true)
            .unwrap();
        fs::write(record.part_path(), b"partial").unwrap();
        drop(store);
        let engine = make_engine(dir.path());
        engine.remove(id, false).unwrap();
        for _ in 0..100 {
            if engine.task(id).is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(engine.task(id).is_none());
        assert!(!record.part_path().exists());
        engine.shutdown();
    }

    #[test]
    fn only_one_task_can_claim_a_new_partial() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_engine(dir.path());
        let path = dir.path().join("downloads/shared.udm.part");
        let (file, _) = engine.state.open_partial(1, 0, &path).unwrap();
        assert!(matches!(
            engine.state.open_partial(2, 0, &path),
            Err(Error::Conflict(_))
        ));
        drop(file);
        engine.shutdown();
    }

    #[test]
    fn completed_record_does_not_reinstall_checkpoint_handle() {
        let dir = tempfile::tempdir().unwrap();
        let mut record = TaskRecord::new(
            1,
            "https://example.com/file",
            dir.path(),
            Some("file.bin".into()),
        );
        fs::write(record.part_path(), b"data").unwrap();
        let file = fs::OpenOptions::new()
            .write(true)
            .open(record.part_path())
            .unwrap();
        let mut segments = vec![SegmentState::new(0, 0, 3)];
        segments[0].done = 4;
        record
            .prepare_progress_checkpoint(&segments, &file)
            .unwrap();
        assert!(record.progress_checkpoint.is_some());
        drop(file);
        // Paused records still need their backing handle for checkpoint sync.
        record.state = TaskState::Paused;
        let paused = TaskCell::new(record.clone(), segments.clone());
        assert!(paused.checkpoint_file.lock().is_some());
        drop(paused);
        // Even a legacy/inconsistent Completed row with a surviving partial
        // and receipt must not install a writer during startup.
        record.state = TaskState::Completed;
        let completed = TaskCell::new(record, segments);
        assert!(completed.checkpoint_file.lock().is_none());
    }

    /// Usagi PR 3: a superseded run can neither claim the partial (swapping
    /// the current checkpoint handle) nor journal or recover a finalize.
    #[test]
    fn superseded_generation_is_refused_at_claim_and_finalize() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_engine(dir.path());
        let id = engine
            .add(AddRequest::new("http://127.0.0.1:9/fenced.bin").start_paused(true))
            .unwrap();
        let cell = engine.state.cell(id).unwrap();
        let part = dir.path().join("downloads/fenced.bin.udm.part");
        cell.record.write().filename = "fenced.bin".into();
        cell.record.write().dest_dir = util::display_path(&dir.path().join("downloads"));

        // Current generation is 0; a run granted generation 1 supersedes it.
        cell.generation.store(1, Ordering::Release);
        let stale = engine.state.open_partial(id, 0, &part);
        assert!(matches!(
            stale,
            Err(Error::StaleGeneration {
                stale: 0,
                current: 1
            })
        ));
        assert!(
            cell.checkpoint_file.lock().is_none(),
            "stale claim installed a handle"
        );
        assert!(
            !engine.state.owns_partial(id, &part),
            "stale claim took ownership"
        );
        assert!(!part.exists(), "stale claim created the backing");

        let (mut held, _) = engine.state.open_partial(id, 1, &part).unwrap();
        std::io::Write::write_all(&mut held, b"current").unwrap();
        held.sync_all().unwrap();
        cell.generation.store(2, Ordering::Release);
        let target = dir.path().join("downloads/fenced.bin");
        let refused = engine.state.finalize_file(
            &cell,
            1,
            &part,
            &target,
            ExpectedHash::parse(&format!("blake3:{}", blake3::hash(b"current").to_hex())).unwrap(),
            crate::config::ConflictPolicy::default(),
        );
        assert!(
            matches!(
                refused,
                Err(Error::StaleGeneration {
                    stale: 1,
                    current: 2
                })
            ),
            "{refused:?}"
        );
        assert!(
            cell.record.read().finalization.is_none(),
            "stale finalize journaled an intent"
        );
        assert!(
            part.exists() && !target.exists(),
            "stale finalize renamed the backing"
        );
        assert!(matches!(
            engine.state.recover_finalization(&cell, 1),
            Err(Error::StaleGeneration { .. })
        ));
        drop(held);
        engine.shutdown();
    }

    #[test]
    fn late_persist_cannot_resurrect_a_removed_task() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_engine(dir.path());
        let id = engine
            .add(AddRequest::new("https://example.com/f").start_paused(true))
            .unwrap();
        let stale_cell = engine.state.cell(id).unwrap();
        engine.remove(id, false).unwrap();
        for _ in 0..100 {
            if engine.task(id).is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(engine.task(id).is_none());
        engine.state.persist(&stale_cell, true).unwrap();
        engine.state.flush_all();
        assert!(engine.store.get_task(id).unwrap().is_none());
        engine.shutdown();
    }

    #[test]
    fn totals_reflect_state() {
        let dir = tempfile::tempdir().unwrap();
        let engine = make_engine(dir.path());
        engine
            .add(AddRequest::new("https://example.com/a").start_paused(true))
            .unwrap();
        engine
            .add(AddRequest::new("https://example.com/b").start_paused(true))
            .unwrap();
        let totals = engine.totals();
        assert_eq!(totals.tasks, 2);
        assert_eq!(totals.paused, 2);
        engine.shutdown();
    }

    #[test]
    fn settings_updates_are_persisted() {
        let dir = tempfile::tempdir().unwrap();
        {
            let engine = make_engine(dir.path());
            let mut s = engine.settings();
            s.segments_per_task = 12;
            engine.update_settings(s).unwrap();
            for _ in 0..100 {
                if engine.settings().segments_per_task == 12 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(engine.settings().segments_per_task, 12);
            engine.shutdown();
            // The redb file lock is released when the Engine is dropped.
        }

        let engine2 = make_engine(dir.path());
        assert_eq!(engine2.settings().segments_per_task, 12);
        engine2.shutdown();
    }

    #[test]
    fn a_second_engine_on_the_same_database_is_refused_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let first = make_engine(dir.path());
        let paths = Paths {
            root: dir.path().to_path_buf(),
            db_file: dir.path().join("test.redb"),
            log_file: dir.path().join("logs").join("udm.log"),
            temp_dir: dir.path().join("tmp"),
            default_download_dir: dir.path().join("downloads"),
            portable: true,
            exe_dir: Some(dir.path().to_path_buf()),
        };
        let err = EngineBuilder::new().paths(paths).idle(true).build();
        assert!(
            matches!(err, Err(Error::DatabaseLocked(_))),
            "expected DatabaseLocked, got {err:?}"
        );
        first.shutdown();
    }

    /// Usagi PR 5 slice 5a. A superseded generation must not journal a finalize.
    /// The process then dies so the parent can reopen the trial. With no env
    /// this returns immediately and the suite does not need a trial root.
    #[test]
    fn stale_generation_exits_without_a_journal() {
        let Ok(root) = std::env::var("UDM_CAMPAIGN_ROOT") else {
            return;
        };
        if std::env::var("UDM_CAMPAIGN_ACTION").ok().as_deref() != Some("stale") {
            return;
        }
        let root = PathBuf::from(root);
        let paths = Paths {
            root: root.clone(),
            db_file: root.join("state.redb"),
            log_file: root.join("logs").join("udm.log"),
            temp_dir: root.join("tmp"),
            default_download_dir: root.clone(),
            portable: true,
            exe_dir: Some(root),
        };
        let engine = EngineBuilder::new()
            .paths(paths)
            .idle(true)
            .settings(Settings {
                http3_enabled: false,
                use_system_proxy: false,
                ..Settings::default()
            })
            .build()
            .unwrap();
        let cell = engine.state.cell(1).expect("campaign task");
        let (part, target, before) = {
            let record = cell.record.read();
            let target = record.final_path();
            let before = fs::read(&target).unwrap();
            (record.part_path(), target, before)
        };
        // Generation 2 is the current run. Generation 1 is the one the kill
        // must not allow to publish.
        cell.generation.store(2, Ordering::Release);
        let refused = engine.state.finalize_file(
            &cell,
            1,
            &part,
            &target,
            ExpectedHash::parse(&format!("blake3:{}", blake3::hash(b"stale").to_hex())).unwrap(),
            crate::config::ConflictPolicy::Overwrite,
        );
        assert!(
            matches!(
                refused,
                Err(Error::StaleGeneration {
                    stale: 1,
                    current: 2
                })
            ),
            "{refused:?}"
        );
        assert!(
            cell.record.read().finalization.is_none(),
            "stale finalize journaled an intent"
        );
        assert_eq!(fs::read(&target).unwrap(), before, "stale finalize renamed");
        println!(
            "UDM_CAMPAIGN {}",
            serde_json::json!({
                "event": "StaleGenerationRefused",
                "stale": 1,
                "current": 2,
            })
        );
        let _ = std::io::Write::flush(&mut std::io::stdout());
        // Exit skips destructors, including a shutdown flush. The refusal
        // above must already be the durable state.
        std::process::exit(86);
    }
}

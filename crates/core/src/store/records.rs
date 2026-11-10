//! Persisted records: what a task is, where it lives, and how much of each
//! segment has already hit the disk.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::TransportPref;
use crate::hash::ExpectedHash;
use crate::paths;

/// Sentinel `end` value meaning "read until the server closes the stream"
/// (used when the response has no `Content-Length`).
pub const OPEN_ENDED: u64 = u64::MAX;

/// Lifecycle of a single download.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    /// Waiting for a free slot in the queue.
    Queued,
    /// Asking the server about size / range support.
    Probing,
    /// Transferring.
    Downloading,
    /// Computing the checksum after the last byte arrived.
    Verifying,
    /// Stopped by the user; the `.part` file is kept and can be resumed.
    Paused,
    /// Finished, verified and renamed to its final name.
    Completed,
    /// Stopped because of an error (the `.part` file is kept when resumable).
    Failed,
    /// Stopped by the user and not meant to resume.
    Canceled,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Probing => "probing",
            TaskState::Downloading => "downloading",
            TaskState::Verifying => "verifying",
            TaskState::Paused => "paused",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Canceled => "canceled",
        }
    }

    /// Label shown in the UI.
    pub fn label(&self) -> &'static str {
        match self {
            TaskState::Queued => "Queued",
            TaskState::Probing => "Contacting server",
            TaskState::Downloading => "Downloading",
            TaskState::Verifying => "Verifying",
            TaskState::Paused => "Paused",
            TaskState::Completed => "Completed",
            TaskState::Failed => "Failed",
            TaskState::Canceled => "Canceled",
        }
    }

    /// A worker is actively moving bytes.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            TaskState::Probing | TaskState::Downloading | TaskState::Verifying
        )
    }

    /// Nothing more will happen without user action.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled
        )
    }

    /// Resuming is possible.
    pub fn is_resumable(&self) -> bool {
        matches!(
            self,
            TaskState::Queued | TaskState::Paused | TaskState::Failed | TaskState::Canceled
        )
    }

    /// States a task may be in when udm starts up after a crash: they are
    /// downgraded to `Paused` so nothing resumes silently.
    pub fn was_interrupted(&self) -> bool {
        self.is_active()
    }
}

/// Category-routing state of a task (G2).
///
/// Records written before G2 have no route metadata and load as
/// [`RouteState::Legacy`] — the task keeps its current behaviour forever and
/// is never re-routed or moved. A task added while auto-categorize was on
/// (and without an explicit destination) starts as
/// [`RouteState::Pending`] carrying the frozen policy snapshot from enqueue
/// time; the worker replaces it with [`RouteState::Locked`] in the same
/// durable write that fixes the final filename and destination, before any
/// partial file is claimed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "state")]
pub enum RouteState {
    /// Pre-G2 record, or routing not applicable (explicit destination /
    /// auto off). Never re-routed.
    Legacy,
    /// Awaiting the worker's route lock. Carries the enqueue-time snapshot:
    /// the absolute download root and the validated category rules, frozen so
    /// later settings changes cannot reroute this task.
    Pending {
        root: String,
        rules: Vec<crate::category::CategoryRule>,
    },
    /// Route locked before the partial was claimed.
    Locked { category_id: String, root: String },
    /// The route was selected in memory but its durable write failed, so the
    /// lock was never persisted (Zed G2 review, P1). Carries the same
    /// snapshot as [`RouteState::Pending`]: a retry or resume must re-run
    /// the lock — including a fresh durable persist — instead of trusting a
    /// RAM-only `Locked`, and a task in this state owns no partial. Only a
    /// successful persist moves a task out of this state into
    /// [`RouteState::Locked`]; loading one from disk behaves exactly like
    /// `Pending`.
    ///
    /// Round 2 published this state into the live record after a failed
    /// save; round 3 instead stages the candidate outside the shared record
    /// and publishes it atomically under the persistence mutex (see
    /// `SharedState::persist_staged`), so the live record never carries a
    /// speculative route at all. The variant is kept for records already
    /// written by the round-2 build.
    Unpersisted {
        root: String,
        rules: Vec<crate::category::CategoryRule>,
    },
}

impl Default for RouteState {
    fn default() -> Self {
        RouteState::Legacy
    }
}

/// Which protocol actually delivered the bytes (shown in the UI, useful to see
/// whether HTTP/3 was negotiated).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportKind {
    Http3,
    Http2,
    Http11,
    Unknown,
}

/// Protocol history admitted in this run. Additive schema-2 field: old records
/// default to Unknown and old readers ignore it. The legacy scalar is None for
/// Mixed, never the last protocol to win a race.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TransportSummary {
    #[default]
    Unknown,
    Single(TransportKind),
    Mixed(Vec<TransportKind>),
}

impl TransportSummary {
    pub(crate) fn observe(&mut self, protocol: TransportKind) {
        if protocol == TransportKind::Unknown {
            return;
        }
        match self {
            Self::Unknown => *self = Self::Single(protocol),
            Self::Single(previous) if *previous != protocol => {
                *self = Self::Mixed(vec![*previous, protocol])
            }
            Self::Mixed(protocols) if !protocols.contains(&protocol) => protocols.push(protocol),
            _ => {}
        }
    }
    pub(crate) fn legacy(&self) -> Option<TransportKind> {
        match self {
            Self::Single(protocol) => Some(*protocol),
            _ => None,
        }
    }
}

impl TransportKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransportKind::Http3 => "HTTP/3 (QUIC)",
            TransportKind::Http2 => "HTTP/2",
            TransportKind::Http11 => "HTTP/1.1",
            TransportKind::Unknown => "-",
        }
    }

    pub fn short(&self) -> &'static str {
        match self {
            TransportKind::Http3 => "H3",
            TransportKind::Http2 => "H2",
            TransportKind::Http11 => "H1",
            TransportKind::Unknown => "?",
        }
    }

    /// Map an HTTP version string (`HTTP/3.0`, `HTTP/2.0`, ...) or a numeric
    /// version to a [`TransportKind`].
    pub fn from_version_text(text: &str) -> TransportKind {
        let t = text.to_ascii_lowercase();
        if t.contains('3') {
            TransportKind::Http3
        } else if t.contains('2') {
            TransportKind::Http2
        } else if t.contains("1.1") {
            TransportKind::Http11
        } else if t.contains('1') {
            TransportKind::Http11
        } else {
            TransportKind::Unknown
        }
    }
}

impl Default for TransportKind {
    fn default() -> Self {
        TransportKind::Unknown
    }
}

/// One contiguous byte range of the file, owned by one connection.
///
/// `done` counts bytes written *contiguously from `start`*, which makes resume
/// trivially correct: the next request starts at `start + done`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentState {
    pub index: u32,
    /// First byte of this segment (inclusive).
    pub start: u64,
    /// Last byte of this segment (inclusive), or [`OPEN_ENDED`] when the total
    /// size is unknown.
    pub end: u64,
    /// Bytes already on disk, counted from `start`.
    pub done: u64,
    /// The server closed the stream after the last byte of this segment.
    pub finished: bool,
    /// Additive schema-2 lease identity. Omitting zero preserves legacy
    /// checkpoint fingerprints; nonzero epochs are committed with coverage.
    #[serde(default, skip_serializing_if = "zero_epoch")]
    pub allocation_epoch: u64,
}

fn zero_epoch(epoch: &u64) -> bool {
    *epoch == 0
}

impl SegmentState {
    pub fn new(index: u32, start: u64, end: u64) -> Self {
        SegmentState {
            index,
            start,
            end,
            done: 0,
            finished: false,
            allocation_epoch: 0,
        }
    }

    /// Segment length in bytes (`None` when open-ended).
    pub fn len(&self) -> Option<u64> {
        if self.end == OPEN_ENDED {
            None
        } else {
            Some(self.end.saturating_sub(self.start) + 1)
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == Some(0)
    }

    /// Absolute file offset of the next byte to write.
    pub fn resume_at(&self) -> u64 {
        self.start.saturating_add(self.done)
    }

    pub fn is_done(&self) -> bool {
        match self.len() {
            Some(len) => self.finished || self.done >= len,
            None => self.finished,
        }
    }

    /// `bytes=start-end` value for this segment, honouring what is already done.
    pub fn range_header(&self) -> String {
        let from = self.resume_at();
        if self.end == OPEN_ENDED {
            format!("bytes={from}-")
        } else {
            format!("bytes={from}-{}", self.end)
        }
    }
}

/// Header scope published by slice 4b. Nothing is hashed into it.
pub const HEADER_SCOPE_NONE: &str = "none";

/// Identity of the bytes a task is allowed to keep.
///
/// `None` on the record is not a wildcard: coverage stored without a bound
/// is not reused. `total_size: None` matches only another unknown length,
/// except the one upgrade allowed before any byte has been flushed.
/// `SCHEMA_VERSION` stays 2; old rows omit this object and deserialize as none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepresentationBound {
    pub final_url: String,
    /// Verbatim strong ETag. Never a `W/` value, never canonicalized.
    pub strong_etag: String,
    pub total_size: Option<u64>,
    /// Slice 4b locks this to [`HEADER_SCOPE_NONE`].
    pub header_scope: String,
}

impl RepresentationBound {
    pub fn none_scope(
        final_url: impl Into<String>,
        strong_etag: impl Into<String>,
        total_size: Option<u64>,
    ) -> Self {
        Self {
            final_url: final_url.into(),
            strong_etag: strong_etag.into(),
            total_size,
            header_scope: HEADER_SCOPE_NONE.to_string(),
        }
    }
}

/// Everything udm remembers about one download.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TaskRecord {
    /// Durable local transaction retained across rename/completion crash gaps.
    #[serde(default)]
    pub finalization: Option<crate::finalize::FinalizeIntent>,
    /// Proof that the stored segment snapshot followed a successful data sync.
    /// Absent on legacy records. Never infer this proof from legacy progress.
    #[serde(default)]
    pub progress_checkpoint: Option<crate::durability::ProgressCheckpoint>,
    pub id: u64,
    /// URL as typed by the user.
    pub url: String,
    /// URL after redirects (used for the actual requests).
    pub final_url: String,
    /// File name inside `dest_dir` (never contains a path separator).
    pub filename: String,
    /// Destination directory.
    pub dest_dir: String,

    pub state: TaskState,
    pub total_size: Option<u64>,
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,

    pub created_at: i64,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,

    /// Requested by the user (paste a checksum to verify the download).
    pub expected_hash: Option<ExpectedHash>,
    /// Computed after the transfer, kept so "Verify again" is cheap to explain.
    pub computed_hash: Option<String>,

    /// Last error message, cleared on success.
    pub error: Option<String>,
    /// Ordinary failed attempts; a representation restart does not consume this budget.
    pub attempts: u32,
    /// One representation restart for the life of the task. Explicit Retry
    /// is the only writer that sets this back to 0. Missing on legacy rows.
    #[serde(default)]
    pub representation_restarts: u32,
    /// Bound coverage. Missing on every row written before this gate, which
    /// deserializes as none and is not resumed.
    #[serde(default)]
    pub representation_bound: Option<RepresentationBound>,

    /// Browser-handoff transaction that created this task (G5). Missing
    /// on every row written before G5, which deserializes as none. The
    /// marker is written in the same durable `put_task` that creates
    /// the task, so the handoff coordinator can recover the commit/task
    /// association after a crash (freeze section 5).
    #[serde(default)]
    pub handoff_tx: Option<String>,

    /// Transport that is currently/last delivering bytes.
    pub transport: Option<TransportKind>,
    #[serde(default)]
    pub transport_summary: TransportSummary,
    /// Per-task transport override.
    pub transport_pref: TransportPref,

    /// Free-form note (e.g. "imported from list.txt").
    pub note: String,
    /// Number of segments actually planned (may differ from the setting).
    pub segments_planned: u32,

    /// Category-routing state (G2). Missing in pre-G2 records, which load as
    /// [`RouteState::Legacy`].
    #[serde(default)]
    pub route: RouteState,
}

impl TaskRecord {
    /// Synchronize the claimed backing file for this exact segment snapshot.
    /// Persist this record and the same segments together in a durable Store
    /// transaction. This receipt alone does not mean the DB commit succeeded.
    pub fn prepare_progress_checkpoint(
        &mut self,
        segments: &[SegmentState],
        file: &std::fs::File,
    ) -> crate::Result<()> {
        crate::durability::prepare(self, segments, Some(file), std::fs::File::sync_all)
    }

    pub fn new(id: u64, url: &str, dest_dir: &Path, filename: Option<String>) -> Self {
        TaskRecord {
            finalization: None,
            progress_checkpoint: None,
            id,
            url: url.to_string(),
            final_url: url.to_string(),
            filename: filename.unwrap_or_default(),
            dest_dir: paths::display(dest_dir),
            state: TaskState::Queued,
            total_size: None,
            accept_ranges: false,
            etag: None,
            last_modified: None,
            content_type: None,
            created_at: crate::util::now_millis(),
            started_at: None,
            finished_at: None,
            expected_hash: None,
            computed_hash: None,
            error: None,
            attempts: 0,
            representation_restarts: 0,
            representation_bound: None,
            handoff_tx: None,
            transport: None,
            transport_summary: TransportSummary::Unknown,
            transport_pref: TransportPref::Auto,
            note: String::new(),
            segments_planned: 1,
            route: RouteState::Legacy,
        }
    }

    pub fn dest_dir(&self) -> PathBuf {
        PathBuf::from(&self.dest_dir)
    }

    /// Final destination path.
    pub fn final_path(&self) -> PathBuf {
        self.dest_dir().join(&self.filename)
    }

    /// Partial file path used while transferring.
    pub fn part_path(&self) -> PathBuf {
        self.dest_dir()
            .join(crate::util::part_filename(&self.filename))
    }

    /// `true` while the task's category route is not durably locked
    /// (pending, or selected in memory but never persisted). Such a task
    /// owns no partial: nothing may open, reset, delete or recover a file at
    /// its provisional path.
    pub fn route_unlocked(&self) -> bool {
        matches!(
            self.route,
            RouteState::Pending { .. } | RouteState::Unpersisted { .. }
        )
    }

    /// `0.0..=1.0`, or `None` when the size is unknown.
    pub fn fraction(&self, received: u64) -> Option<f64> {
        match self.total_size {
            Some(total) if total > 0 => Some((received as f64 / total as f64).clamp(0.0, 1.0)),
            _ => None,
        }
    }

    /// Fingerprint used to detect "the server swapped the file under us".
    pub fn validator(&self) -> Option<String> {
        match (&self.etag, &self.last_modified) {
            (Some(etag), _) => Some(format!("etag={etag}")),
            (None, Some(lm)) => Some(format!("lm={lm}")),
            (None, None) => None,
        }
    }
}

/// True when `path` looks like a udm partial file.
pub fn is_part_file(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.ends_with(".udm.part"))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_ranges_and_resume_offsets() {
        let mut s = SegmentState::new(0, 100, 199);
        assert_eq!(s.len(), Some(100));
        assert_eq!(s.range_header(), "bytes=100-199");
        s.done = 40;
        assert_eq!(s.resume_at(), 140);
        assert_eq!(s.range_header(), "bytes=140-199");
        assert!(!s.is_done());
        s.done = 100;
        assert!(s.is_done());
    }

    #[test]
    fn open_ended_segment() {
        let s = SegmentState::new(0, 0, OPEN_ENDED);
        assert_eq!(s.len(), None);
        assert_eq!(s.range_header(), "bytes=0-");
        assert!(!s.is_done());
    }

    #[test]
    fn record_paths() {
        let r = TaskRecord::new(
            7,
            "https://x.example/a/b.iso",
            Path::new("/tmp/dl"),
            Some("b.iso".into()),
        );
        assert_eq!(r.final_path(), PathBuf::from("/tmp/dl/b.iso"));
        assert_eq!(r.part_path(), PathBuf::from("/tmp/dl/b.iso.udm.part"));
        assert!(is_part_file(&r.part_path()));
        assert!(!is_part_file(&r.final_path()));
    }

    #[test]
    fn transport_kind_from_version() {
        assert_eq!(
            TransportKind::from_version_text("HTTP/3.0"),
            TransportKind::Http3
        );
        assert_eq!(
            TransportKind::from_version_text("HTTP/2.0"),
            TransportKind::Http2
        );
        assert_eq!(
            TransportKind::from_version_text("HTTP/1.1"),
            TransportKind::Http11
        );
    }

    #[test]
    fn legacy_record_without_route_loads_as_legacy() {
        // A pre-G2 record JSON has no `route` field at all.
        let mut value = serde_json::to_value(TaskRecord::new(
            3,
            "https://x.example/f.zip",
            Path::new("/tmp/dl"),
            Some("f.zip".into()),
        ))
        .unwrap();
        value.as_object_mut().unwrap().remove("route");
        let back: TaskRecord = serde_json::from_value(value).unwrap();
        assert_eq!(back.route, RouteState::Legacy);
        // Pending and Locked round-trip distinctly (no empty-string sentinel).
        let mut pending = back;
        pending.route = RouteState::Pending {
            root: "/tmp/dl".into(),
            rules: Vec::new(),
        };
        let json = serde_json::to_string(&pending).unwrap();
        let back2: TaskRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back2.route, pending.route);
        // Unpersisted round-trips too, and both unlocked states report as
        // owning no partial.
        let mut unpersisted = back2;
        assert!(unpersisted.route_unlocked());
        unpersisted.route = RouteState::Unpersisted {
            root: "/tmp/dl".into(),
            rules: Vec::new(),
        };
        assert!(unpersisted.route_unlocked());
        let json = serde_json::to_string(&unpersisted).unwrap();
        let back3: TaskRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back3.route, unpersisted.route);
        let mut locked = back3;
        locked.route = RouteState::Locked {
            category_id: "music".into(),
            root: "/tmp/dl".into(),
        };
        assert!(!locked.route_unlocked());
    }

    #[test]
    fn fraction_and_validator() {
        let mut r = TaskRecord::new(1, "u", Path::new("/tmp"), None);
        assert_eq!(r.fraction(0), None);
        r.total_size = Some(200);
        assert_eq!(r.fraction(50), Some(0.25));
        r.etag = Some("\"abc\"".into());
        assert_eq!(r.validator().as_deref(), Some("etag=\"abc\""));
    }

    #[test]
    fn legacy_row_deserializes_without_a_representation_bound() {
        let record = TaskRecord::new(
            1,
            "https://example.test/a",
            Path::new("/tmp"),
            Some("a.bin".into()),
        );
        let mut value = serde_json::to_value(&record).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("representation_bound");
        object.remove("representation_restarts");
        let loaded: TaskRecord = serde_json::from_value(value).unwrap();
        assert!(loaded.representation_bound.is_none());
        assert_eq!(loaded.representation_restarts, 0);
        let bound = RepresentationBound::none_scope("https://example.test/a", "\"v\"", Some(4));
        let mut round = loaded;
        round.representation_bound = Some(bound.clone());
        round.representation_restarts = 1;
        let again: TaskRecord =
            serde_json::from_str(&serde_json::to_string(&round).unwrap()).unwrap();
        assert_eq!(again.representation_bound, Some(bound));
        assert_eq!(again.representation_restarts, 1);
        assert_eq!(
            again.representation_bound.unwrap().header_scope,
            HEADER_SCOPE_NONE
        );
    }
}

#[cfg(test)]
mod transport_summary_compatibility {
    use super::*;
    #[test]
    fn schema2_summary_is_additive_and_mixed_never_claims_last_protocol() {
        let zero = SegmentState::new(0, 0, 7);
        assert_eq!(
            serde_json::to_string(&zero).unwrap(),
            "{\"index\":0,\"start\":0,\"end\":7,\"done\":0,\"finished\":false}"
        );
        let mut leased = zero;
        leased.allocation_epoch = 3;
        let decoded: SegmentState =
            serde_json::from_slice(&serde_json::to_vec(&leased).unwrap()).unwrap();
        assert_eq!(decoded.allocation_epoch, 3);
        let record = TaskRecord::new(1, "https://example.test/a", Path::new("."), None);
        let mut legacy = serde_json::to_value(&record).unwrap();
        legacy.as_object_mut().unwrap().remove("transport_summary");
        let decoded: TaskRecord = serde_json::from_value(legacy).unwrap();
        assert_eq!(decoded.transport_summary, TransportSummary::Unknown);
        let mut summary = TransportSummary::Unknown;
        summary.observe(TransportKind::Http2);
        summary.observe(TransportKind::Http2);
        assert_eq!(summary, TransportSummary::Single(TransportKind::Http2));
        summary.observe(TransportKind::Http11);
        summary.observe(TransportKind::Http2);
        assert_eq!(
            summary,
            TransportSummary::Mixed(vec![TransportKind::Http2, TransportKind::Http11])
        );
        assert_eq!(summary.legacy(), None);
        let mut record = decoded;
        record.transport_summary = summary.clone();
        record.transport = None;
        let decoded: TaskRecord =
            serde_json::from_slice(&serde_json::to_vec(&record).unwrap()).unwrap();
        assert_eq!(decoded.transport_summary, summary);
        assert_eq!(decoded.transport, None);
        // Older scalar consumers still decode their field and ignore the additive one.
        #[derive(Deserialize)]
        struct Scalar {
            transport: Option<TransportKind>,
        }
        let old: Scalar = serde_json::from_value(serde_json::to_value(record).unwrap()).unwrap();
        assert_eq!(old.transport, None);
    }
}

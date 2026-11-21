//! Association book for the per-download detail window.
//!
//! A window exists only because the handoff dialog's Download action
//! accepted a commit. It is keyed by that dialog's `(caller, tx)` until
//! an authoritative query for the same pair supplies `task_ref`. Nothing
//! here matches a filename, a URL, or the newest task. A reply for a
//! different transaction cannot bind this window, and a window the user
//! has closed is not opened again by a later reply or snapshot.

use usagi::engine::TaskSnapshot;
use usagi::handoff::{HandoffReply, HandoffStatus};
use usagi::store::records::TaskState;
use usagi::util;

use crate::handoff_dialog::{
    logical_to_physical, place_outer, AppliedPlacement, MonitorSnap, NativeSize, PxSize,
};
use crate::task_actions::caps_for_state;

/// Shown when a control was enabled at paint and the task can no longer
/// accept that action at the click. The catalog translates this key.
pub const ACTION_UNAVAILABLE: &str = "This action is no longer available.";

/// Default client size, logical pixels. Min is strictly below max so the
/// native window stays resizable.
pub const DETAIL_DEFAULT_LOGICAL_W: f64 = 600.0;
pub const DETAIL_DEFAULT_LOGICAL_H: f64 = 360.0;
pub const DETAIL_MIN_LOGICAL_W: f64 = 480.0;
pub const DETAIL_MIN_LOGICAL_H: f64 = 320.0;
/// Each window after the first is shifted by this many logical pixels
/// down and to the right of the centered origin.
pub const DETAIL_CASCADE_LOGICAL: f64 = 24.0;

/// Shown where a value is not applicable. Unknown numbers use the
/// translated word instead, so a missing total never becomes `0%`.
pub const MISSING: &str = "\u{2014}";

const RESUME_HELP: &str = "HTTP range support. This does not promise a future resume will succeed.";

/// Who asked for a detail window. Only the handoff Download action opens one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetailSource {
    HandoffDownload,
    HandoffAddPaused,
    ManualAdd,
    ListActivation,
}

pub fn opens_detail_window(source: DetailSource) -> bool {
    match source {
        DetailSource::HandoffDownload => true,
        DetailSource::HandoffAddPaused | DetailSource::ManualAdd | DetailSource::ListActivation => {
            false
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssocKey {
    pub caller: String,
    pub tx: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Commit was accepted. No task id until this pair's query says so.
    Waiting {
        materialize_error: String,
    },
    Bound {
        task_id: u64,
    },
    /// Discarded, aborted, expired, missing, or conflicting handoff.
    /// The window stays up and offers only Close.
    HandoffClosed {
        message: String,
    },
    /// The bound id left the engine snapshot. Do not pick a replacement.
    TaskGone {
        task_id: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub key: AssocKey,
    pub filename: String,
    pub url: String,
    pub phase: Phase,
    pub closed_by_user: bool,
    pub cascade_index: u32,
    pub action_error: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApplyEffect {
    Unchanged,
    StillWaiting,
    Bound,
    HandoffClosed,
    IgnoredMismatch,
    IgnoredClosed,
    IgnoredAbsent,
}

#[derive(Clone, Debug, Default)]
pub struct Book {
    slots: Vec<Slot>,
    next_cascade: u32,
}

impl Book {
    pub fn request_open(
        &mut self,
        source: DetailSource,
        key: AssocKey,
        filename: String,
        url: String,
        materialize_error: String,
    ) -> Option<u32> {
        if !opens_detail_window(source) {
            return None;
        }
        // A live window, a closed window, or a terminal handoff for this
        // pair already occupies the key. Refresh must not create another.
        if self.slots.iter().any(|slot| slot.key == key) {
            return None;
        }
        let cascade_index = self.next_cascade;
        self.next_cascade = self.next_cascade.saturating_add(1);
        self.slots.push(Slot {
            key,
            filename,
            url,
            phase: Phase::Waiting { materialize_error },
            closed_by_user: false,
            cascade_index,
            action_error: String::new(),
        });
        Some(cascade_index)
    }

    /// Drop a slot whose native window could not be created. The user
    /// did not close it, so this is not a tombstone.
    pub fn revoke_unshown(&mut self, key: &AssocKey) -> bool {
        let Some(index) = self.slots.iter().position(|slot| {
            &slot.key == key && !slot.closed_by_user && matches!(slot.phase, Phase::Waiting { .. })
        }) else {
            return false;
        };
        self.slots.remove(index);
        true
    }

    pub fn close(&mut self, key: &AssocKey) -> bool {
        let Some(slot) = self.slots.iter_mut().find(|slot| &slot.key == key) else {
            return false;
        };
        if slot.closed_by_user {
            return false;
        }
        slot.closed_by_user = true;
        true
    }

    pub fn apply_reply(&mut self, key: &AssocKey, reply: &HandoffReply) -> ApplyEffect {
        let Some(slot) = self.slots.iter_mut().find(|slot| &slot.key == key) else {
            return ApplyEffect::IgnoredAbsent;
        };
        // The reply names its transaction. A result queried for another
        // tx must not bind, even when it arrives later and carries an id.
        if reply.tx != slot.key.tx {
            return ApplyEffect::IgnoredMismatch;
        }
        if slot.closed_by_user {
            return ApplyEffect::IgnoredClosed;
        }
        if !matches!(slot.phase, Phase::Waiting { .. }) {
            return ApplyEffect::Unchanged;
        }
        if let Some(task_id) = committed_task_id(reply) {
            slot.phase = Phase::Bound { task_id };
            return ApplyEffect::Bound;
        }
        if let Some(message) = terminal_message(reply.status) {
            slot.phase = Phase::HandoffClosed {
                message: message.to_string(),
            };
            return ApplyEffect::HandoffClosed;
        }
        slot.phase = Phase::Waiting {
            materialize_error: reply.detail.clone().unwrap_or_default(),
        };
        ApplyEffect::StillWaiting
    }

    /// Full engine snapshot, not the filtered list. A missing id means
    /// the task was removed. A different id with the same name is not it.
    pub fn observe_present(&mut self, present_ids: &[u64]) {
        for slot in &mut self.slots {
            if slot.closed_by_user {
                continue;
            }
            if let Phase::Bound { task_id } = slot.phase {
                if !present_ids.contains(&task_id) {
                    slot.phase = Phase::TaskGone { task_id };
                }
            }
        }
    }

    pub fn waiting_keys(&self) -> Vec<AssocKey> {
        self.slots
            .iter()
            .filter(|slot| !slot.closed_by_user && matches!(slot.phase, Phase::Waiting { .. }))
            .map(|slot| slot.key.clone())
            .collect()
    }

    #[cfg(test)]
    pub fn open_slots(&self) -> Vec<Slot> {
        self.slots
            .iter()
            .filter(|slot| !slot.closed_by_user)
            .cloned()
            .collect()
    }

    pub fn slot(&self, key: &AssocKey) -> Option<Slot> {
        self.slots.iter().find(|slot| &slot.key == key).cloned()
    }

    pub fn bound_id(&self, key: &AssocKey) -> Option<u64> {
        match self.slot(key).as_ref().map(|slot| &slot.phase) {
            Some(Phase::Bound { task_id }) => Some(*task_id),
            _ => None,
        }
    }

    pub fn set_action_error(&mut self, key: &AssocKey, message: String) {
        if let Some(slot) = self.slots.iter_mut().find(|slot| &slot.key == key) {
            slot.action_error = message;
        }
    }

    pub fn clear_action_error(&mut self, key: &AssocKey) {
        if let Some(slot) = self.slots.iter_mut().find(|slot| &slot.key == key) {
            slot.action_error.clear();
        }
    }
}

/// `Committed` plus a decimal `task_ref` is the only bind. Recovery-required
/// stays pending even if a stray id is present: the association has not landed.
fn committed_task_id(reply: &HandoffReply) -> Option<u64> {
    if reply.status != HandoffStatus::Committed {
        return None;
    }
    let text = reply.task_ref.as_deref()?.trim();
    if text.is_empty() {
        return None;
    }
    text.parse::<u64>().ok()
}

fn terminal_message(status: HandoffStatus) -> Option<&'static str> {
    match status {
        HandoffStatus::Discarded => Some("This handoff was discarded."),
        HandoffStatus::Aborted => Some("This handoff failed before a download started."),
        HandoffStatus::Expired => Some("This handoff expired before a download started."),
        HandoffStatus::NotFound => Some("This handoff was not found."),
        HandoffStatus::Conflict => Some("This handoff could not be associated."),
        _ => None,
    }
}

/// Same speed line the list and the bottom panel already show.
pub fn snapshot_speed_text(snapshot: &TaskSnapshot, vietnamese: bool) -> String {
    if snapshot.verify_bps > 0.0 {
        format!(
            "{} {}",
            if vietnamese { "xác minh" } else { "verifying" },
            util::human_speed(snapshot.verify_bps)
        )
    } else if snapshot.state == TaskState::Downloading && snapshot.speed_bps > 0.0 {
        util::human_speed(snapshot.speed_bps)
    } else {
        String::new()
    }
}

/// Same ETA line the list and the bottom panel already show.
pub fn snapshot_eta_text(snapshot: &TaskSnapshot, vietnamese: bool) -> String {
    if snapshot.state == TaskState::Downloading
        && snapshot.speed_bps > 1.0
        && snapshot.eta_secs.is_finite()
    {
        if vietnamese {
            format!("còn {}", util::human_eta(snapshot.eta_secs))
        } else {
            format!("{} left", util::human_eta(snapshot.eta_secs))
        }
    } else {
        String::new()
    }
}

/// Yes / No / Unknown from the snapshot flags the engine already computed.
/// `ranges_observed == false` is Unknown, including when `accept_ranges`
/// is still the record default. This does not look at segments again.
pub fn resume_support_key(ranges_observed: bool, accept_ranges: bool) -> &'static str {
    if !ranges_observed {
        "Unknown"
    } else if accept_ranges {
        "Yes"
    } else {
        "No"
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DetailModel {
    pub title: String,
    pub url: String,
    pub status: String,
    pub size_text: String,
    pub downloaded_text: String,
    pub percent_text: String,
    /// Progress-bar fraction. Negative means unknown: the bar stays hidden
    /// so an unknown size is not drawn as 0% or as a guessed sliver.
    pub percent: f32,
    pub speed_text: String,
    pub eta_text: String,
    pub resume_text: String,
    pub resume_help: String,
    pub destination: String,
    pub category: String,
    pub notice: String,
    pub error: String,
    pub waiting: bool,
    pub failed: bool,
    pub done: bool,
    pub can_pause: bool,
    pub can_resume: bool,
    pub can_cancel: bool,
    pub can_retry: bool,
    pub can_open_folder: bool,
    /// "Retry" only while the shared predicate says this task is Failed.
    /// Every other state keeps the Resume label, enabled or not.
    pub resume_label: &'static str,
}

pub fn view_model(
    slot: &Slot,
    snapshot: Option<&TaskSnapshot>,
    category: &str,
    vietnamese: bool,
) -> DetailModel {
    match &slot.phase {
        Phase::Bound { task_id } => match snapshot {
            Some(snapshot) if snapshot.id == *task_id => {
                live_model(slot, snapshot, category, vietnamese)
            }
            _ => gone_model(slot, vietnamese),
        },
        Phase::TaskGone { .. } => gone_model(slot, vietnamese),
        Phase::HandoffClosed { message } => closed_model(slot, message, vietnamese),
        Phase::Waiting { materialize_error } => waiting_model(slot, materialize_error, vietnamese),
    }
}

fn waiting_model(slot: &Slot, materialize_error: &str, vietnamese: bool) -> DetailModel {
    let unknown = text("unknown", vietnamese);
    DetailModel {
        title: title_or_waiting(&slot.filename, vietnamese),
        url: slot.url.clone(),
        status: text("Waiting for recovery", vietnamese),
        size_text: unknown.clone(),
        downloaded_text: unknown.clone(),
        percent_text: unknown,
        percent: -1.0,
        speed_text: text("unknown", vietnamese),
        eta_text: text("unknown", vietnamese),
        resume_text: text("Unknown", vietnamese),
        resume_help: text(RESUME_HELP, vietnamese),
        destination: text("unknown", vietnamese),
        category: text("unknown", vietnamese),
        notice: materialize_error.to_string(),
        error: shown_action_error("", &slot.action_error, vietnamese),
        waiting: true,
        failed: false,
        done: false,
        can_pause: false,
        can_resume: false,
        can_cancel: false,
        can_retry: false,
        can_open_folder: false,
        resume_label: "Resume",
    }
}

fn closed_model(slot: &Slot, message: &str, vietnamese: bool) -> DetailModel {
    DetailModel {
        title: title_or_waiting(&slot.filename, vietnamese),
        url: slot.url.clone(),
        status: text(message, vietnamese),
        size_text: MISSING.into(),
        downloaded_text: MISSING.into(),
        percent_text: MISSING.into(),
        percent: -1.0,
        speed_text: MISSING.into(),
        eta_text: MISSING.into(),
        resume_text: MISSING.into(),
        resume_help: text(RESUME_HELP, vietnamese),
        destination: MISSING.into(),
        category: MISSING.into(),
        notice: String::new(),
        error: shown_action_error("", &slot.action_error, vietnamese),
        waiting: false,
        failed: false,
        done: false,
        can_pause: false,
        can_resume: false,
        can_cancel: false,
        can_retry: false,
        can_open_folder: false,
        resume_label: "Resume",
    }
}

fn gone_model(slot: &Slot, vietnamese: bool) -> DetailModel {
    DetailModel {
        title: title_or_waiting(&slot.filename, vietnamese),
        url: slot.url.clone(),
        status: text("Task no longer in list", vietnamese),
        size_text: MISSING.into(),
        downloaded_text: MISSING.into(),
        percent_text: MISSING.into(),
        percent: -1.0,
        speed_text: MISSING.into(),
        eta_text: MISSING.into(),
        resume_text: MISSING.into(),
        resume_help: text(RESUME_HELP, vietnamese),
        destination: MISSING.into(),
        category: MISSING.into(),
        notice: String::new(),
        error: shown_action_error("", &slot.action_error, vietnamese),
        waiting: false,
        failed: false,
        done: false,
        can_pause: false,
        can_resume: false,
        can_cancel: false,
        can_retry: false,
        can_open_folder: false,
        resume_label: "Resume",
    }
}

fn live_model(
    slot: &Slot,
    snapshot: &TaskSnapshot,
    category: &str,
    vietnamese: bool,
) -> DetailModel {
    let title = if !snapshot.filename.is_empty() {
        snapshot.filename.clone()
    } else {
        title_or_waiting(&slot.filename, vietnamese)
    };
    let (percent, percent_text) = if snapshot.state == TaskState::Completed {
        (1.0, "100%".to_string())
    } else if let Some(fraction) = snapshot.fraction {
        let fraction = (fraction as f32).clamp(0.0, 1.0);
        let pct = (f64::from(fraction) * 100.0).round() as i64;
        (fraction, format!("{pct}%"))
    } else {
        (-1.0, text("unknown", vietnamese))
    };
    let speed = snapshot_speed_text(snapshot, vietnamese);
    let eta = snapshot_eta_text(snapshot, vietnamese);
    let destination = if !snapshot.final_path.is_empty() {
        snapshot.final_path.clone()
    } else if !snapshot.dest_dir.is_empty() {
        snapshot.dest_dir.clone()
    } else {
        text("unknown", vietnamese)
    };
    let task_error = snapshot.error.clone().unwrap_or_default();
    let caps = caps_for_state(snapshot.state);
    DetailModel {
        title,
        url: snapshot.url.clone(),
        status: text(snapshot.state.label(), vietnamese),
        size_text: match snapshot.total_size {
            Some(total) => util::human_bytes(total as f64),
            None => text("unknown", vietnamese),
        },
        downloaded_text: util::human_bytes(snapshot.received as f64),
        percent_text,
        percent,
        speed_text: if speed.is_empty() {
            MISSING.into()
        } else {
            speed
        },
        eta_text: if eta.is_empty() { MISSING.into() } else { eta },
        resume_text: text(
            resume_support_key(snapshot.ranges_observed, snapshot.accept_ranges),
            vietnamese,
        ),
        resume_help: text(RESUME_HELP, vietnamese),
        destination,
        category: if category.is_empty() {
            text("unknown", vietnamese)
        } else {
            category.to_string()
        },
        notice: String::new(),
        error: shown_action_error(&task_error, &slot.action_error, vietnamese),
        waiting: false,
        failed: snapshot.state == TaskState::Failed,
        done: snapshot.state == TaskState::Completed,
        can_pause: caps.can_pause,
        can_resume: caps.can_resume,
        can_cancel: caps.can_cancel,
        can_retry: caps.can_retry,
        can_open_folder: caps.can_open_folder,
        resume_label: if caps.can_retry { "Retry" } else { "Resume" },
    }
}

/// What the detail window asks the shared task handler to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportRequest {
    Pause,
    ResumeOrRetry,
    Cancel,
    OpenFolder,
}

/// The call to make, if the shared predicate still allows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportCall {
    Pause,
    Resume,
    Cancel,
    Retry,
    OpenFolder,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransportDecision {
    pub call: Option<TransportCall>,
    /// Set when the click must not reach the engine. The window shows this
    /// key and leaves the painted state on the snapshot.
    pub error: Option<&'static str>,
}

/// Decide a detail-window click from the same predicates as the toolbar.
///
/// A bound task that has already left the snapshot still returns a call:
/// the engine handler is what reports "task N not found". A click does
/// not change the displayed state; the caller paints the snapshot it has.
pub fn decide_transport(
    slot: &Slot,
    snapshot: Option<&TaskSnapshot>,
    request: TransportRequest,
) -> TransportDecision {
    let Phase::Bound { task_id } = &slot.phase else {
        return blocked();
    };
    let Some(snapshot) = snapshot else {
        return TransportDecision {
            call: Some(call_without_state(request)),
            error: None,
        };
    };
    if snapshot.id != *task_id {
        return blocked();
    }
    let caps = caps_for_state(snapshot.state);
    let call = match request {
        TransportRequest::Pause if caps.can_pause => TransportCall::Pause,
        TransportRequest::Cancel if caps.can_cancel => TransportCall::Cancel,
        TransportRequest::OpenFolder if caps.can_open_folder => TransportCall::OpenFolder,
        TransportRequest::ResumeOrRetry if caps.can_retry => TransportCall::Retry,
        TransportRequest::ResumeOrRetry if caps.can_resume => TransportCall::Resume,
        _ => return blocked(),
    };
    TransportDecision {
        call: Some(call),
        error: None,
    }
}

fn blocked() -> TransportDecision {
    TransportDecision {
        call: None,
        error: Some(ACTION_UNAVAILABLE),
    }
}

fn call_without_state(request: TransportRequest) -> TransportCall {
    match request {
        TransportRequest::Pause => TransportCall::Pause,
        TransportRequest::Cancel => TransportCall::Cancel,
        TransportRequest::ResumeOrRetry => TransportCall::Resume,
        TransportRequest::OpenFolder => TransportCall::OpenFolder,
    }
}

fn title_or_waiting(filename: &str, vietnamese: bool) -> String {
    if filename.is_empty() {
        text("Waiting for recovery", vietnamese)
    } else {
        filename.to_string()
    }
}

fn shown_action_error(task: &str, action: &str, vietnamese: bool) -> String {
    let action = if action.is_empty() {
        String::new()
    } else {
        text(action, vietnamese)
    };
    join_errors(task, &action)
}

fn join_errors(task: &str, action: &str) -> String {
    match (task.is_empty(), action.is_empty()) {
        (true, true) => String::new(),
        (false, true) => task.to_string(),
        (true, false) => action.to_string(),
        (false, false) => format!("{task}\n{action}"),
    }
}

fn text(key: &str, vietnamese: bool) -> String {
    if vietnamese {
        crate::i18n::vi(key).to_string()
    } else {
        key.to_string()
    }
}

pub fn cascade_delta(index: u32, step_px: i32) -> (i32, i32) {
    let shift = (index as i32).saturating_mul(step_px.max(0));
    (shift, shift)
}

pub fn clamp_origin(
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    work: crate::handoff_dialog::PxRect,
) -> (i32, i32) {
    (
        clamp_axis(x, width, work.left, work.right),
        clamp_axis(y, height, work.top, work.bottom),
    )
}

fn clamp_axis(origin: i32, size: i32, start: i32, end: i32) -> i32 {
    let size = size.max(1);
    if end <= start {
        return start;
    }
    let room = end.saturating_sub(start);
    if room <= size {
        return start;
    }
    origin.clamp(start, end.saturating_sub(size))
}

/// Center the first window in the handoff monitor's work area. Later
/// windows step down-right by 24 logical pixels, then clamp into that
/// same work area. One conversion, from the monitor scale.
pub fn detail_placement(
    target: &MonitorSnap,
    native: NativeSize,
    cascade_index: u32,
) -> AppliedPlacement {
    let want_w = logical_to_physical(DETAIL_DEFAULT_LOGICAL_W, target.scale);
    let want_h = logical_to_physical(DETAIL_DEFAULT_LOGICAL_H, target.scale);
    let min_w = logical_to_physical(DETAIL_MIN_LOGICAL_W, target.scale);
    let min_h = logical_to_physical(DETAIL_MIN_LOGICAL_H, target.scale);
    let frame_w = (native.outer_width as i32)
        .saturating_sub(native.inner_width as i32)
        .max(0);
    let frame_h = (native.outer_height as i32)
        .saturating_sub(native.inner_height as i32)
        .max(0);
    let placed = place_outer(
        target.work,
        PxSize {
            width: want_w.saturating_add(frame_w),
            height: want_h.saturating_add(frame_h),
        },
        PxSize {
            width: min_w.saturating_add(frame_w),
            height: min_h.saturating_add(frame_h),
        },
    );
    let step = logical_to_physical(DETAIL_CASCADE_LOGICAL, target.scale);
    let (dx, dy) = cascade_delta(cascade_index, step);
    let (x, y) = clamp_origin(
        placed.x.saturating_add(dx),
        placed.y.saturating_add(dy),
        placed.outer_width,
        placed.outer_height,
        target.work,
    );
    AppliedPlacement {
        x,
        y,
        inner_width: (placed.outer_width - frame_w).max(min_w),
        inner_height: (placed.outer_height - frame_h).max(min_h),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi::store::records::TransportSummary;

    fn key(caller: &str, tx: &str) -> AssocKey {
        AssocKey {
            caller: caller.into(),
            tx: tx.into(),
        }
    }

    fn reply(
        tx: &str,
        status: HandoffStatus,
        task_ref: Option<&str>,
        detail: Option<&str>,
    ) -> HandoffReply {
        HandoffReply {
            status,
            tx: tx.into(),
            state: None,
            task_ref: task_ref.map(str::to_string),
            expires_at_ms: None,
            detail: detail.map(str::to_string),
        }
    }

    fn open_download(book: &mut Book, caller: &str, tx: &str) -> u32 {
        book.request_open(
            DetailSource::HandoffDownload,
            key(caller, tx),
            "report.pdf".into(),
            "https://example.test/report.pdf".into(),
            String::new(),
        )
        .expect("download opens a window")
    }

    #[test]
    fn only_the_handoff_download_action_opens_a_window() {
        let mut book = Book::default();
        assert!(!opens_detail_window(DetailSource::ManualAdd));
        assert!(!opens_detail_window(DetailSource::HandoffAddPaused));
        assert!(!opens_detail_window(DetailSource::ListActivation));
        assert!(opens_detail_window(DetailSource::HandoffDownload));
        for source in [
            DetailSource::ManualAdd,
            DetailSource::HandoffAddPaused,
            DetailSource::ListActivation,
        ] {
            assert!(book
                .request_open(
                    source,
                    key("chrome", "tx-add"),
                    "a.bin".into(),
                    "https://ex/a".into(),
                    String::new()
                )
                .is_none());
        }
        assert!(book.open_slots().is_empty());
        assert_eq!(book.slots.len(), 0);
    }

    #[test]
    fn pending_download_waits_without_a_task_id() {
        let mut book = Book::default();
        let index = open_download(&mut book, "chrome", "tx-1");
        assert_eq!(index, 0);
        let slot = book.slot(&key("chrome", "tx-1")).unwrap();
        assert!(matches!(slot.phase, Phase::Waiting { .. }));
        assert!(book.bound_id(&key("chrome", "tx-1")).is_none());
        let model = view_model(&slot, None, "", false);
        assert!(model.waiting);
        assert_eq!(model.status, "Waiting for recovery");
        assert_eq!(model.title, "report.pdf");
        assert_eq!(model.url, "https://example.test/report.pdf");
        assert_eq!(model.percent, -1.0);
        assert_eq!(model.percent_text, "unknown");
        assert_ne!(model.percent_text, "0%");
        assert_eq!(model.downloaded_text, "unknown");
        assert!(!model.can_pause);
        assert!(!model.can_resume);
        assert!(!model.can_cancel);
        assert!(!model.can_retry);
        assert!(!model.can_open_folder);
        assert_eq!(model.resume_label, "Resume");
        assert_eq!(model.resume_text, "Unknown");
    }

    #[test]
    fn query_binds_the_exact_id_and_a_different_tx_does_not() {
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-a");
        assert_eq!(
            book.apply_reply(
                &key("chrome", "tx-a"),
                &reply("tx-b", HandoffStatus::Committed, Some("99"), None)
            ),
            ApplyEffect::IgnoredMismatch
        );
        assert!(book.bound_id(&key("chrome", "tx-a")).is_none());
        // A recovery reply that merely mentions some other id is not a bind.
        assert_eq!(
            book.apply_reply(
                &key("chrome", "tx-a"),
                &reply(
                    "tx-a",
                    HandoffStatus::RecoveryRequired,
                    Some("77"),
                    Some("commitment durable; association pending: disk full")
                )
            ),
            ApplyEffect::StillWaiting
        );
        let waiting = book.slot(&key("chrome", "tx-a")).unwrap();
        assert!(matches!(
            waiting.phase,
            Phase::Waiting { ref materialize_error } if materialize_error.contains("disk full")
        ));
        assert!(book.bound_id(&key("chrome", "tx-a")).is_none());
        assert_eq!(
            book.apply_reply(
                &key("chrome", "tx-a"),
                &reply("tx-a", HandoffStatus::Committed, Some("42"), None)
            ),
            ApplyEffect::Bound
        );
        assert_eq!(book.bound_id(&key("chrome", "tx-a")), Some(42));
        // Duplicate and a later id for this tx do not move the binding.
        assert_eq!(
            book.apply_reply(
                &key("chrome", "tx-a"),
                &reply("tx-a", HandoffStatus::Committed, Some("42"), None)
            ),
            ApplyEffect::Unchanged
        );
        assert_eq!(
            book.apply_reply(
                &key("chrome", "tx-a"),
                &reply("tx-a", HandoffStatus::Committed, Some("100"), None)
            ),
            ApplyEffect::Unchanged
        );
        assert_eq!(book.bound_id(&key("chrome", "tx-a")), Some(42));
        // A late result for another transaction still cannot retarget it.
        assert_eq!(
            book.apply_reply(
                &key("chrome", "tx-a"),
                &reply("tx-late", HandoffStatus::Committed, Some("5"), None)
            ),
            ApplyEffect::IgnoredMismatch
        );
        assert_eq!(book.bound_id(&key("chrome", "tx-a")), Some(42));
        assert_eq!(book.open_slots().len(), 1);
    }

    #[test]
    fn a_reply_does_not_create_a_window() {
        let mut book = Book::default();
        assert_eq!(
            book.apply_reply(
                &key("chrome", "tx-missing"),
                &reply("tx-missing", HandoffStatus::Committed, Some("3"), None)
            ),
            ApplyEffect::IgnoredAbsent
        );
        assert!(book.open_slots().is_empty());
    }

    #[test]
    fn discarded_and_failed_handoffs_offer_only_close() {
        let cases = [
            (HandoffStatus::Discarded, "This handoff was discarded."),
            (
                HandoffStatus::Aborted,
                "This handoff failed before a download started.",
            ),
            (
                HandoffStatus::Expired,
                "This handoff expired before a download started.",
            ),
            (HandoffStatus::NotFound, "This handoff was not found."),
            (
                HandoffStatus::Conflict,
                "This handoff could not be associated.",
            ),
        ];
        for (index, (status, message)) in cases.into_iter().enumerate() {
            let mut book = Book::default();
            let tx = format!("tx-{index}");
            open_download(&mut book, "firefox", &tx);
            assert_eq!(
                book.apply_reply(&key("firefox", &tx), &reply(&tx, status, Some("8"), None)),
                ApplyEffect::HandoffClosed
            );
            let slot = book.slot(&key("firefox", &tx)).unwrap();
            assert!(book.bound_id(&key("firefox", &tx)).is_none());
            let model = view_model(&slot, None, "Videos", false);
            assert_eq!(model.status, message);
            assert!(!model.can_pause && !model.can_resume && !model.can_cancel);
            assert!(!model.can_retry);
            assert!(!model.can_open_folder);
            assert!(!model.waiting);
            assert_eq!(model.percent, -1.0);
        }
    }

    #[test]
    fn closing_before_bind_is_not_resurrected() {
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-early");
        assert!(book.close(&key("chrome", "tx-early")));
        assert_eq!(
            book.apply_reply(
                &key("chrome", "tx-early"),
                &reply("tx-early", HandoffStatus::Committed, Some("42"), None)
            ),
            ApplyEffect::IgnoredClosed
        );
        assert!(book.bound_id(&key("chrome", "tx-early")).is_none());
        assert!(book.open_slots().is_empty());
        assert!(book
            .request_open(
                DetailSource::HandoffDownload,
                key("chrome", "tx-early"),
                "report.pdf".into(),
                "https://example.test/report.pdf".into(),
                String::new(),
            )
            .is_none());
        book.observe_present(&[42]);
        assert!(book.open_slots().is_empty());
        assert!(book.bound_id(&key("chrome", "tx-early")).is_none());
    }

    #[test]
    fn closing_after_bind_is_not_resurrected_by_status_changes() {
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-done");
        book.apply_reply(
            &key("chrome", "tx-done"),
            &reply("tx-done", HandoffStatus::Committed, Some("7"), None),
        );
        assert!(book.close(&key("chrome", "tx-done")));
        book.observe_present(&[7]);
        book.apply_reply(
            &key("chrome", "tx-done"),
            &reply("tx-done", HandoffStatus::Committed, Some("7"), None),
        );
        assert!(book.open_slots().is_empty());
        let slot = book.slot(&key("chrome", "tx-done")).unwrap();
        assert!(slot.closed_by_user);
        assert!(book.bound_id(&key("chrome", "tx-done")).is_some());
        assert!(book
            .open_slots()
            .iter()
            .all(|open| open.key.tx != "tx-done"));
    }

    #[test]
    fn a_removed_task_is_not_replaced() {
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-gone");
        book.apply_reply(
            &key("chrome", "tx-gone"),
            &reply("tx-gone", HandoffStatus::Committed, Some("7"), None),
        );
        let mut other = snap(8, TaskState::Downloading, Some(0.5), true, true);
        other.filename = "other.bin".into();
        other.url = "https://example.test/other.bin".into();
        book.observe_present(&[other.id]);
        let slot = book.slot(&key("chrome", "tx-gone")).unwrap();
        assert!(matches!(slot.phase, Phase::TaskGone { task_id: 7 }));
        let model = view_model(&slot, Some(&other), "Videos", false);
        assert_eq!(model.status, "Task no longer in list");
        assert!(!model.can_pause && !model.can_resume && !model.can_cancel);
        assert!(!model.can_retry && !model.can_open_folder);
        assert_eq!(model.title, "report.pdf");
        assert_eq!(model.url, "https://example.test/report.pdf");
        assert_ne!(model.resume_text, "Yes");
        assert_eq!(book.bound_id(&key("chrome", "tx-gone")), None);
        // Coming back under the same id does not recreate the row's actions.
        book.observe_present(&[7, 8]);
        let slot = book.slot(&key("chrome", "tx-gone")).unwrap();
        assert!(matches!(slot.phase, Phase::TaskGone { task_id: 7 }));
        let own = snap(7, TaskState::Failed, None, false, true);
        let model = view_model(&slot, Some(&own), "Videos", false);
        assert!(!model.can_pause && !model.can_resume && !model.can_cancel);
        assert!(!model.can_retry);
        assert_eq!(model.resume_label, "Resume");
        assert_eq!(model.status, "Task no longer in list");
    }

    #[test]
    fn terminal_actions_follow_the_bound_task_only() {
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-act");
        book.apply_reply(
            &key("chrome", "tx-act"),
            &reply("tx-act", HandoffStatus::Committed, Some("7"), None),
        );
        let slot = book.slot(&key("chrome", "tx-act")).unwrap();
        let failed = snap(7, TaskState::Failed, Some(0.4), true, false);
        let failed_view = view_model(&slot, Some(&failed), "Documents", false);
        assert!(failed_view.can_retry);
        assert!(!failed_view.can_pause && !failed_view.can_resume && !failed_view.can_cancel);
        assert_eq!(failed_view.resume_label, "Retry");
        assert!(!failed_view.can_open_folder);
        assert!(failed_view.failed);
        assert_eq!(failed_view.error, "boom");

        let done = snap(7, TaskState::Completed, Some(1.0), false, false);
        let done_view = view_model(&slot, Some(&done), "Documents", false);
        assert!(!done_view.can_retry);
        assert!(!done_view.can_pause && !done_view.can_resume && !done_view.can_cancel);
        assert_eq!(done_view.resume_label, "Resume");
        assert!(done_view.can_open_folder);
        assert!(done_view.done);
        assert_eq!(done_view.percent_text, "100%");

        let live = snap(7, TaskState::Downloading, None, false, false);
        let live_view = view_model(&slot, Some(&live), "Documents", false);
        assert!(live_view.can_pause && live_view.can_cancel);
        assert!(!live_view.can_resume && !live_view.can_retry && !live_view.can_open_folder);
        assert_eq!(live_view.percent, -1.0);
        assert_eq!(live_view.percent_text, "unknown");
        assert_ne!(live_view.percent_text, "0%");
        assert_eq!(live_view.speed_text, MISSING);

        let paused = snap(7, TaskState::Paused, Some(0.0), true, true);
        let paused_view = view_model(&slot, Some(&paused), "Documents", false);
        assert!(paused_view.can_resume && paused_view.can_open_folder);
        assert!(!paused_view.can_pause && !paused_view.can_cancel && !paused_view.can_retry);
        assert_eq!(paused_view.resume_label, "Resume");
        assert_eq!(paused_view.percent_text, "0%");
    }

    #[test]
    fn transport_controls_follow_the_shared_predicates() {
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-matrix");
        book.apply_reply(
            &key("chrome", "tx-matrix"),
            &reply("tx-matrix", HandoffStatus::Committed, Some("7"), None),
        );
        let slot = book.slot(&key("chrome", "tx-matrix")).unwrap();
        let cases = [
            (TaskState::Queued, true, false, true, false, false),
            (TaskState::Probing, true, false, true, false, false),
            (TaskState::Downloading, true, false, true, false, false),
            (TaskState::Verifying, true, false, true, false, false),
            (TaskState::Paused, false, true, false, false, true),
            (TaskState::Canceled, false, true, false, false, false),
            (TaskState::Failed, false, false, false, true, false),
            (TaskState::Completed, false, false, false, false, true),
        ];
        for (state, pause, resume, cancel, retry, folder) in cases {
            let caps = caps_for_state(state);
            assert_eq!(
                (
                    caps.can_pause,
                    caps.can_resume,
                    caps.can_cancel,
                    caps.can_retry,
                    caps.can_open_folder
                ),
                (pause, resume, cancel, retry, folder),
                "{state:?}"
            );
            let snapshot = snap(7, state, Some(0.2), false, true);
            let model = view_model(&slot, Some(&snapshot), "Documents", false);
            assert_eq!(model.can_pause, pause, "{state:?} pause");
            assert_eq!(model.can_resume, resume, "{state:?} resume");
            assert_eq!(model.can_cancel, cancel, "{state:?} cancel");
            assert_eq!(model.can_retry, retry, "{state:?} retry");
            assert_eq!(model.can_open_folder, folder, "{state:?} folder");
            assert_eq!(
                model.resume_label,
                if retry { "Retry" } else { "Resume" },
                "{state:?} label"
            );
            assert_eq!(
                decide_transport(&slot, Some(&snapshot), TransportRequest::Pause).call,
                pause.then_some(TransportCall::Pause),
                "{state:?}"
            );
            assert_eq!(
                decide_transport(&slot, Some(&snapshot), TransportRequest::Cancel).call,
                cancel.then_some(TransportCall::Cancel),
                "{state:?}"
            );
            assert_eq!(
                decide_transport(&slot, Some(&snapshot), TransportRequest::OpenFolder).call,
                folder.then_some(TransportCall::OpenFolder),
                "{state:?}"
            );
            let resume_call =
                decide_transport(&slot, Some(&snapshot), TransportRequest::ResumeOrRetry);
            if retry {
                assert_eq!(resume_call.call, Some(TransportCall::Retry), "{state:?}");
                assert_ne!(model.resume_label, "Resume");
            } else if resume {
                assert_eq!(resume_call.call, Some(TransportCall::Resume), "{state:?}");
            } else {
                assert_eq!(resume_call.call, None, "{state:?}");
                assert_eq!(resume_call.error, Some(ACTION_UNAVAILABLE));
            }
        }
    }

    #[test]
    fn pending_association_disables_every_task_control() {
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-wait");
        let slot = book.slot(&key("chrome", "tx-wait")).unwrap();
        let model = view_model(&slot, None, "", false);
        assert!(model.waiting);
        assert!(!model.can_pause);
        assert!(!model.can_resume);
        assert!(!model.can_cancel);
        assert!(!model.can_retry);
        assert!(!model.can_open_folder);
        assert_eq!(model.resume_label, "Resume");
        for request in [
            TransportRequest::Pause,
            TransportRequest::ResumeOrRetry,
            TransportRequest::Cancel,
            TransportRequest::OpenFolder,
        ] {
            let decision = decide_transport(&slot, None, request);
            assert_eq!(decision.call, None);
            assert_eq!(decision.error, Some(ACTION_UNAVAILABLE));
        }
    }

    #[test]
    fn pause_renders_from_the_next_snapshot_and_not_from_the_click() {
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-pause");
        book.apply_reply(
            &key("chrome", "tx-pause"),
            &reply("tx-pause", HandoffStatus::Committed, Some("7"), None),
        );
        let slot = book.slot(&key("chrome", "tx-pause")).unwrap();
        let downloading = snap(7, TaskState::Downloading, Some(0.4), true, true);
        let painted = view_model(&slot, Some(&downloading), "Documents", false);
        assert_eq!(painted.status, "Downloading");
        assert!(painted.can_pause && painted.can_cancel);
        let decision = decide_transport(&slot, Some(&downloading), TransportRequest::Pause);
        assert_eq!(decision.call, Some(TransportCall::Pause));
        assert_eq!(decision.error, None);
        // The command is only queued. The same snapshot still paints Downloading.
        let queued = view_model(&slot, Some(&downloading), "Documents", false);
        assert_eq!(queued, painted);
        assert_ne!(queued.status, "Paused");
        assert!(!queued.can_resume);
        let paused = snap(7, TaskState::Paused, Some(0.4), true, true);
        let published = view_model(&slot, Some(&paused), "Documents", false);
        assert_eq!(published.status, "Paused");
        assert!(published.can_resume);
        assert!(!published.can_pause);
        assert!(!published.can_cancel);
        assert_eq!(
            decide_transport(&slot, Some(&paused), TransportRequest::Pause).error,
            Some(ACTION_UNAVAILABLE)
        );
    }

    #[test]
    fn an_error_between_paint_and_click_is_bounced_into_the_window() {
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-bounce");
        book.apply_reply(
            &key("chrome", "tx-bounce"),
            &reply("tx-bounce", HandoffStatus::Committed, Some("7"), None),
        );
        let slot = book.slot(&key("chrome", "tx-bounce")).unwrap();
        let downloading = snap(7, TaskState::Downloading, Some(0.4), true, true);
        let painted = view_model(&slot, Some(&downloading), "Documents", false);
        assert!(painted.can_pause);
        assert!(painted.error.is_empty());

        // The task failed after the button was painted. Pause must not run,
        // and the window must not claim the task is paused.
        let failed = snap(7, TaskState::Failed, Some(0.4), true, true);
        let rejected = decide_transport(&slot, Some(&failed), TransportRequest::Pause);
        assert_eq!(rejected.call, None);
        assert_eq!(rejected.error, Some(ACTION_UNAVAILABLE));
        let mut bounced = slot.clone();
        bounced.action_error = rejected.error.unwrap().to_string();
        let shown = view_model(&bounced, Some(&failed), "Documents", false);
        assert_eq!(shown.status, "Failed");
        assert_ne!(shown.status, "Paused");
        assert!(shown.error.contains(ACTION_UNAVAILABLE));
        assert!(shown.can_retry);
        assert_eq!(shown.resume_label, "Retry");
        assert!(!shown.can_pause);

        let vietnamese = view_model(&bounced, Some(&failed), "Documents", true);
        assert!(vietnamese.error.contains("không còn thực hiện được"));
        assert!(!vietnamese.error.contains(ACTION_UNAVAILABLE));

        // The task left the snapshot between paint and click. The decision
        // still asks for the engine handler, whose NotFound text is what
        // the window shows. The painted state stays the snapshot's state.
        let missing = decide_transport(&slot, None, TransportRequest::Pause);
        assert_eq!(missing.call, Some(TransportCall::Pause));
        assert_eq!(missing.error, None);
        let mut missing_slot = slot.clone();
        missing_slot.action_error = "task 7 not found".into();
        let still = view_model(&missing_slot, Some(&downloading), "Documents", false);
        assert_eq!(still.status, "Downloading");
        assert!(still.error.contains("task 7 not found"));
        assert_ne!(still.status, "Paused");
        assert!(still.can_pause);
    }

    #[test]
    fn resume_support_uses_the_snapshot_flags() {
        assert_eq!(resume_support_key(false, false), "Unknown");
        assert_eq!(resume_support_key(false, true), "Unknown");
        assert_eq!(resume_support_key(true, false), "No");
        assert_eq!(resume_support_key(true, true), "Yes");
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-range");
        book.apply_reply(
            &key("chrome", "tx-range"),
            &reply("tx-range", HandoffStatus::Committed, Some("7"), None),
        );
        let slot = book.slot(&key("chrome", "tx-range")).unwrap();
        let unknown = view_model(
            &slot,
            Some(&snap(7, TaskState::Paused, None, false, false)),
            "Other",
            false,
        );
        assert_eq!(unknown.resume_text, "Unknown");
        let no = view_model(
            &slot,
            Some(&snap(7, TaskState::Paused, None, false, true)),
            "Other",
            true,
        );
        assert_eq!(no.resume_text, "Không");
        let yes = view_model(
            &slot,
            Some(&snap(7, TaskState::Downloading, Some(0.2), true, true)),
            "Other",
            false,
        );
        assert_eq!(yes.resume_text, "Yes");
        assert!(yes.resume_help.contains("does not promise"));
        // A snapshot of a different id cannot supply this window's Yes.
        let stranger = snap(99, TaskState::Downloading, Some(0.2), true, true);
        let ignored = view_model(&slot, Some(&stranger), "Videos", false);
        assert_eq!(ignored.status, "Task no longer in list");
        assert_ne!(ignored.resume_text, "Yes");
    }

    #[test]
    fn final_name_and_path_win_and_speed_matches_an_active_transfer() {
        let mut book = Book::default();
        open_download(&mut book, "chrome", "tx-name");
        book.apply_reply(
            &key("chrome", "tx-name"),
            &reply("tx-name", HandoffStatus::Committed, Some("7"), None),
        );
        let mut snapshot = snap(7, TaskState::Downloading, Some(0.5), true, true);
        snapshot.filename = "report (1).pdf".into();
        snapshot.final_path = "D:/Downloads/Documents/report (1).pdf".into();
        snapshot.dest_dir = "D:/Downloads".into();
        snapshot.speed_bps = 2048.0;
        snapshot.eta_secs = 12.0;
        snapshot.total_size = Some(4096);
        snapshot.received = 2048;
        let slot = book.slot(&key("chrome", "tx-name")).unwrap();
        let model = view_model(&slot, Some(&snapshot), "Documents", false);
        assert_eq!(model.title, "report (1).pdf");
        assert_eq!(model.destination, "D:/Downloads/Documents/report (1).pdf");
        assert_eq!(model.category, "Documents");
        assert_eq!(model.percent_text, "50%");
        assert!(!model.speed_text.is_empty() && model.speed_text != MISSING);
        assert!(model.eta_text.contains("left"));
        assert_eq!(model.size_text, util::human_bytes(4096.0));
        assert_eq!(model.downloaded_text, util::human_bytes(2048.0));
    }

    #[test]
    fn cascade_is_24_logical_pixels_down_right_and_clamped() {
        let monitor = MonitorSnap {
            work: crate::handoff_dialog::PxRect {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            },
            scale: 1.0,
        };
        let native = NativeSize {
            inner_width: 600,
            inner_height: 360,
            outer_width: 600,
            outer_height: 360,
        };
        let first = detail_placement(&monitor, native, 0);
        let second = detail_placement(&monitor, native, 1);
        let third = detail_placement(&monitor, native, 2);
        assert_eq!((second.x - first.x, second.y - first.y), (24, 24));
        assert_eq!((third.x - first.x, third.y - first.y), (48, 48));
        assert_eq!(first.inner_width, 600);
        assert_eq!(first.inner_height, 360);

        let tight = MonitorSnap {
            work: crate::handoff_dialog::PxRect {
                left: 10,
                top: 20,
                right: 810,
                bottom: 520,
            },
            scale: 1.0,
        };
        let origin = detail_placement(&tight, native, 0);
        let far = detail_placement(&tight, native, 30);
        assert!(far.x >= tight.work.left);
        assert!(far.y >= tight.work.top);
        assert!(far.x + 600 <= tight.work.right);
        assert!(far.y + 360 <= tight.work.bottom);
        assert!(far.x > origin.x || far.x + 600 == tight.work.right);
        assert!(far.y > origin.y || far.y + 360 == tight.work.bottom);

        let mut book = Book::default();
        assert_eq!(open_download(&mut book, "chrome", "one"), 0);
        book.close(&key("chrome", "one"));
        assert_eq!(open_download(&mut book, "chrome", "two"), 1);
        assert_eq!(open_download(&mut book, "chrome", "three"), 2);
    }

    #[test]
    fn detail_strings_have_vietnamese() {
        for key in [
            "Waiting for recovery",
            "Task no longer in list",
            "Resume Support",
            "Yes",
            "No",
            "Unknown",
            "unknown",
            RESUME_HELP,
            "This handoff was discarded.",
            "This handoff failed before a download started.",
            "This handoff expired before a download started.",
            "This handoff was not found.",
            "This handoff could not be associated.",
            "Close",
            "This action is no longer available.",
            "Total size",
            "Downloaded",
            "Status",
            "Category",
            "Percent",
        ] {
            assert_ne!(crate::i18n::vi(key), key, "{key}");
        }
    }

    fn snap(
        id: u64,
        state: TaskState,
        fraction: Option<f64>,
        accept_ranges: bool,
        ranges_observed: bool,
    ) -> TaskSnapshot {
        TaskSnapshot {
            id,
            url: "https://example.test/report.pdf".into(),
            filename: "report.pdf".into(),
            dest_dir: "D:/Downloads".into(),
            state,
            state_label: state.label().into(),
            total_size: None,
            received: 0,
            fraction,
            speed_bps: 0.0,
            eta_secs: f64::INFINITY,
            segments_done: 0,
            segments_total: 1,
            active_requests: 0,
            error: if state == TaskState::Failed {
                Some("boom".into())
            } else {
                None
            },
            transport: None,
            transport_summary: TransportSummary::Unknown,
            paths: Vec::new(),
            created_at: 0,
            started_at: None,
            finished_at: None,
            final_path: String::new(),
            expected_hash: None,
            computed_hash: None,
            attempts: 1,
            segment_retry: None,
            verify_bps: 0.0,
            accept_ranges,
            ranges_observed,
        }
    }
}

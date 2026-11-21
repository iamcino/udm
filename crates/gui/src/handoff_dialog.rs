//! Handoff confirmation dialog: queue, destination decision, and the
//! checks that must be true before a commit or a discard.
//!
//! The window calls [`Coordinator`] methods directly. Nothing here holds
//! the coordinator lock across a user wait or a probe. A timer or a
//! cached discovery row is never enough to commit or discard: the
//! transition asks [`transition_gate`] after `Coordinator::query`.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::AssertUnwindSafe;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use usagi::category::{classify, validate_categories, CategoryRule};
use usagi::handoff::{DiscoveryEntry, HandoffIntent, HandoffState, HandoffStatus, UserDecision};
use usagi::util::{self, display_path};

use crate::state::Gui;

/// Newest-first cap for `settings.recent_download_dirs`.
pub const RECENT_DIR_CAP: usize = 8;

/// One ordered, de-duplicated view of discovery.
///
/// Order is preparation time, then caller, then transaction id. The same
/// `(caller, tx)` is kept once. A later live copy replaces an expired
/// copy of that key so a stale notice cannot hide a still-prepared
/// transaction, and a second live copy cannot open a second dialog.
pub fn queue_from_discovery(entries: &[DiscoveryEntry]) -> Vec<DiscoveryEntry> {
    let mut sorted = entries.to_vec();
    sorted.sort_by(|a, b| {
        a.created_at_ms
            .cmp(&b.created_at_ms)
            .then(a.caller.cmp(&b.caller))
            .then(a.tx.cmp(&b.tx))
    });
    let mut seen: BTreeMap<(String, String), usize> = BTreeMap::new();
    let mut out: Vec<DiscoveryEntry> = Vec::with_capacity(sorted.len());
    for entry in sorted {
        let key = (entry.caller.clone(), entry.tx.clone());
        if let Some(&position) = seen.get(&key) {
            if out[position].expired && !entry.expired {
                out[position] = entry;
            }
            continue;
        }
        seen.insert(key, out.len());
        out.push(entry);
    }
    out
}

/// The next Prepared row to present. Expired rows and keys in `skip`
/// (the row already on screen, or one this session already closed)
/// stay out of the dialog.
pub fn next_live<'a>(
    queue: &'a [DiscoveryEntry],
    skip: &BTreeSet<(String, String)>,
) -> Option<&'a DiscoveryEntry> {
    queue
        .iter()
        .find(|entry| !entry.expired && !skip.contains(&(entry.caller.clone(), entry.tx.clone())))
}

/// What a fresh discovery pass says about the row currently on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrontUpdate {
    StillLive,
    LockExpired,
    /// No longer in discovery: committed, discarded, or otherwise gone.
    /// Close the window. Do not commit or discard again.
    ClosedElsewhere,
}

pub fn reconcile_front(caller: &str, tx: &str, queue: &[DiscoveryEntry]) -> FrontUpdate {
    match queue
        .iter()
        .find(|entry| entry.caller == caller && entry.tx == tx)
    {
        Some(entry) if entry.expired => FrontUpdate::LockExpired,
        Some(_) => FrontUpdate::StillLive,
        None => FrontUpdate::ClosedElsewhere,
    }
}

/// Authoritative answer of `Coordinator::query` before a button acts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransitionGate {
    /// Still `Prepared`. The button may commit or discard.
    Proceed,
    /// Expired. Lock submit. Closing only dismisses.
    LockExpired,
    /// Already committed, discarded, or otherwise not Prepared.
    /// Close. Do not send another transition.
    AlreadyHandled,
}

pub fn transition_gate(status: HandoffStatus) -> TransitionGate {
    match status {
        HandoffStatus::Prepared => TransitionGate::Proceed,
        HandoffStatus::Expired => TransitionGate::LockExpired,
        HandoffStatus::Committed
        | HandoffStatus::Discarded
        | HandoffStatus::Aborted
        | HandoffStatus::RecoveryRequired
        | HandoffStatus::NotFound
        | HandoffStatus::Conflict => TransitionGate::AlreadyHandled,
    }
}

/// A commit landed when the record is `Committed`, including the
/// association-pending reply. An expiry reply is not a success.
pub fn commit_accepted(status: HandoffStatus, state_committed: bool) -> bool {
    state_committed
        && matches!(
            status,
            HandoffStatus::Committed | HandoffStatus::RecoveryRequired
        )
}

/// Whether Slint should hide the dialog after the close callback returns.
///
/// `HideWindow` is applied after the callback. [`abandon`] may already
/// have shown the next record inside that callback. Hiding then conceals
/// the successor while `open` stays set, and [`tick`] never presents
/// another record until the process restarts.
///
/// Hide only when the close finished and no row is current. A rejected
/// close, or a successor already shown, stays on screen.
pub fn close_request_hides_window(close_completed: bool, successor_open: bool) -> bool {
    close_completed && !successor_open
}

/// Whether this paint must run `apply-appearance`.
///
/// A new dialog window starts with language index 0, which is also the
/// English main window, and with `Lang.english` false. Static labels read
/// that flag through `I18n.tr`. A check that applies only when the indexes
/// differ therefore skips the first English show, and the labels stay
/// Vietnamese while the countdown, built with the main window's translator,
/// is English. The first paint of each window instance applies even when
/// nothing differs. Later paints apply only when an index changed.
pub fn appearance_pending(applied: bool, differs: bool) -> bool {
    !applied || differs
}

/// window declares the same numbers. One logical-to-physical conversion
/// uses the target monitor's scale; refresh never converts again.
pub const HANDOFF_DEFAULT_LOGICAL_W: f64 = 560.0;
pub const HANDOFF_DEFAULT_LOGICAL_H: f64 = 460.0;
pub const HANDOFF_MIN_LOGICAL_W: f64 = 460.0;
pub const HANDOFF_MIN_LOGICAL_H: f64 = 360.0;

/// What one poll tick does with the dialog. A row that is already open
/// is only repainted. Opening the next head is the only path that may
/// place and focus the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TickAction {
    OpenNext,
    Refresh,
}

pub fn tick_action(has_open_row: bool) -> TickAction {
    if has_open_row {
        TickAction::Refresh
    } else {
        TickAction::OpenNext
    }
}

/// Work area in physical pixels, virtual-screen coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PxRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PxSize {
    pub width: i32,
    pub height: i32,
}

/// Where the first presentation looked for a monitor. The hidden main
/// window is not one of these sources.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MonitorSource {
    Foreground,
    Cursor,
    Primary,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MonitorSnap {
    pub work: PxRect,
    pub scale: f64,
}

/// Foreground monitor, then the cursor's monitor, then the primary work
/// area. `None` means that source was not available. There is no argument
/// for the main window: a hidden main window must not become the default.
pub fn choose_monitor(
    foreground: Option<MonitorSnap>,
    cursor: Option<MonitorSnap>,
    primary: MonitorSnap,
) -> (MonitorSource, MonitorSnap) {
    if let Some(snap) = foreground {
        (MonitorSource::Foreground, snap)
    } else if let Some(snap) = cursor {
        (MonitorSource::Cursor, snap)
    } else {
        (MonitorSource::Primary, primary)
    }
}

/// One logical-to-physical conversion. A non-positive or non-finite scale
/// is 1, so a failed DPI read still yields a pixel size.
pub fn logical_to_physical(logical: f64, scale: f64) -> i32 {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    let px = (logical * scale).round();
    if !px.is_finite() || px <= 0.0 {
        1
    } else if px >= i32::MAX as f64 {
        i32::MAX
    } else {
        px as i32
    }
}

/// The native client size is usable for frame measurement. Zero means the
/// window has not been realized yet, and placement waits for the single
/// continuation instead of focusing on a poll tick.
pub fn native_size_settled(inner_w: u32, inner_h: u32, outer_w: u32, outer_h: u32) -> bool {
    inner_w > 0 && inner_h > 0 && outer_w > 0 && outer_h > 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OuterPlacement {
    pub x: i32,
    pub y: i32,
    pub outer_width: i32,
    pub outer_height: i32,
}

/// Center `outer` in the work area. Shrink it to the work area when that
/// stays at or above `min_outer`. When the work area is smaller than the
/// minimum, keep the minimum and pin the origin to the work area's
/// top-left. The scroll body, not this function, keeps the buttons
/// reachable inside the window.
pub fn place_outer(work: PxRect, outer: PxSize, min_outer: PxSize) -> OuterPlacement {
    let area_w = work.right.saturating_sub(work.left).max(0);
    let area_h = work.bottom.saturating_sub(work.top).max(0);
    let width = fit_edge(outer.width, min_outer.width, area_w);
    let height = fit_edge(outer.height, min_outer.height, area_h);
    OuterPlacement {
        x: centered_origin(work.left, area_w, width),
        y: centered_origin(work.top, area_h, height),
        outer_width: width,
        outer_height: height,
    }
}

fn fit_edge(outer: i32, min_outer: i32, area: i32) -> i32 {
    let outer = outer.max(1);
    let min_outer = min_outer.max(1);
    if area <= 0 || outer <= area {
        outer
    } else if min_outer <= area {
        area
    } else {
        min_outer
    }
}

fn centered_origin(origin: i32, area: i32, size: i32) -> i32 {
    if area <= 0 || size >= area {
        return origin;
    }
    origin.saturating_add((area - size) / 2)
}

/// Settled native size plus the frame it implies, turned into one
/// position and one client size for the target monitor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeSize {
    pub inner_width: u32,
    pub inner_height: u32,
    pub outer_width: u32,
    pub outer_height: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AppliedPlacement {
    pub x: i32,
    pub y: i32,
    pub inner_width: i32,
    pub inner_height: i32,
}

pub fn placement_for(target: &MonitorSnap, native: NativeSize) -> AppliedPlacement {
    let want_w = logical_to_physical(HANDOFF_DEFAULT_LOGICAL_W, target.scale);
    let want_h = logical_to_physical(HANDOFF_DEFAULT_LOGICAL_H, target.scale);
    let min_w = logical_to_physical(HANDOFF_MIN_LOGICAL_W, target.scale);
    let min_h = logical_to_physical(HANDOFF_MIN_LOGICAL_H, target.scale);
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
    AppliedPlacement {
        x: placed.x,
        y: placed.y,
        inner_width: (placed.outer_width - frame_w).max(min_w),
        inner_height: (placed.outer_height - frame_h).max(min_h),
    }
}

/// Whether this queue head still needs its one presentation.
///
/// `Now` places and calls `focus_window` once. `Later` arms exactly one
/// continuation for when the native window did not exist yet. `NotNow` is
/// a refresh, a repeat of the same head, or a continuation that is no
/// longer the head. Nothing here runs from the poll loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FirstShow {
    Now,
    Later,
    NotNow,
}

#[derive(Debug, Default)]
pub(crate) struct FirstShowGate {
    done: Option<(String, String)>,
    armed: Option<(String, String)>,
}

pub(crate) fn decide_first_show(
    gate: &mut FirstShowGate,
    caller: &str,
    tx: &str,
    native_settled: bool,
) -> FirstShow {
    let key = (caller.to_string(), tx.to_string());
    if gate.done.as_ref() == Some(&key) || gate.armed.as_ref() == Some(&key) {
        return FirstShow::NotNow;
    }
    // A new head replaces a continuation that has not run yet.
    gate.armed = None;
    if native_settled {
        gate.done = Some(key);
        FirstShow::Now
    } else {
        gate.armed = Some(key);
        FirstShow::Later
    }
}

pub(crate) fn decide_continuation(gate: &mut FirstShowGate, caller: &str, tx: &str) -> FirstShow {
    let key = (caller.to_string(), tx.to_string());
    if gate.armed.as_ref() != Some(&key) || gate.done.as_ref() == Some(&key) {
        return FirstShow::NotNow;
    }
    gate.armed = None;
    gate.done = Some(key);
    FirstShow::Now
}

pub(crate) fn disarm_continuation(gate: &mut FirstShowGate, caller: &str, tx: &str) {
    let key = (caller.to_string(), tx.to_string());
    if gate.armed.as_ref() == Some(&key) {
        gate.armed = None;
    }
}

/// Refresh, countdown, and probe paints. Never `Now` or `Later`.
pub(crate) fn decide_refresh(_gate: &FirstShowGate) -> FirstShow {
    FirstShow::NotNow
}

/// Submit buttons lock once `now` has reached the record deadline.
/// A missing deadline does not lock: discovery still has to say expired.
pub fn submit_locked(now_ms: i64, expires_at_ms: Option<i64>) -> bool {
    expires_at_ms.is_some_and(|deadline| now_ms >= deadline)
}

/// Whole seconds left until the deadline. Zero once the deadline has passed.
pub fn countdown_seconds(now_ms: i64, expires_at_ms: Option<i64>) -> i64 {
    let Some(deadline) = expires_at_ms else {
        return 0;
    };
    deadline.saturating_sub(now_ms).max(0) / 1000
}

pub fn format_countdown(seconds: i64) -> String {
    let seconds = seconds.max(0);
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Where the size figure came from. A probe that disagrees with the
/// browser hint is shown as the probe value, with that disagreement
/// named. Neither source is treated as proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeProvenance {
    FromBrowser,
    Probed,
    ProbedDiffers,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SizeView {
    pub bytes: Option<u64>,
    pub provenance: SizeProvenance,
}

/// `probed` is `None` until the worker returns. `Some(None)` means the
/// probe finished and the server gave no length.
pub fn size_view(browser: Option<u64>, probed: Option<Option<u64>>) -> SizeView {
    if let Some(Some(probed_size)) = probed {
        let provenance = if browser.is_some() && browser != Some(probed_size) {
            SizeProvenance::ProbedDiffers
        } else {
            SizeProvenance::Probed
        };
        return SizeView {
            bytes: Some(probed_size),
            provenance,
        };
    }
    if let Some(browser_size) = browser {
        SizeView {
            bytes: Some(browser_size),
            provenance: SizeProvenance::FromBrowser,
        }
    } else {
        SizeView {
            bytes: None,
            provenance: SizeProvenance::Unknown,
        }
    }
}

/// Filename field seed: the browser hint when it has text, otherwise
/// the URL's last path segment. Empty when neither yields a name.
pub fn initial_filename(intent: &HandoffIntent) -> String {
    if let Some(hint) = intent
        .filename_hint
        .as_deref()
        .map(str::trim)
        .filter(|hint| !hint.is_empty())
    {
        return hint.to_string();
    }
    url::Url::parse(&intent.url)
        .ok()
        .and_then(|parsed| util::filename_from_url(&parsed))
        .unwrap_or_default()
}

/// Unaccepted destination choices for the dialog that is on screen.
///
/// `category_explicit` and `dir_override` survive later filename and
/// probe updates. Until either is set, a filename change classifies
/// again and the destination preview follows. Turning Use Category off
/// stores the download root itself unless a manual directory is set,
/// so a global auto-categorize setting cannot route the file elsewhere.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DialogDraft {
    pub use_category: bool,
    pub category_explicit: bool,
    pub category_id: Option<String>,
    pub dir_override: bool,
    pub manual_dir: String,
    pub filename_raw: String,
    pub filename_edited: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecisionPreview {
    pub dest_dir: String,
    pub filename: String,
    pub filename_differs: bool,
    pub category_id: Option<String>,
}

impl DialogDraft {
    pub fn initial(use_category: bool, filename_raw: &str, rules: &[CategoryRule]) -> Self {
        let mut draft = DialogDraft {
            use_category,
            category_explicit: false,
            category_id: None,
            dir_override: false,
            manual_dir: String::new(),
            filename_raw: filename_raw.to_string(),
            filename_edited: false,
        };
        draft.recompute_inference(rules);
        draft
    }

    /// Classify the current filename when the user has not picked a
    /// category and has not typed a directory. Either of those sticks.
    pub fn recompute_inference(&mut self, rules: &[CategoryRule]) {
        if self.category_explicit || self.dir_override {
            return;
        }
        let filename = util::sanitize_filename(&self.filename_raw);
        self.category_id = classify(&filename, rules).map(|rule| rule.id.clone());
    }

    pub fn note_filename_edit(&mut self, raw: &str, rules: &[CategoryRule]) {
        self.filename_raw = raw.to_string();
        self.filename_edited = true;
        self.recompute_inference(rules);
    }

    /// Fill the filename from a probe only while the user has not edited it.
    /// Returns whether the field changed. A directory override is left alone.
    pub fn note_probe_filename(&mut self, hint: &str, rules: &[CategoryRule]) -> bool {
        if self.filename_edited {
            return false;
        }
        self.filename_raw = hint.to_string();
        self.recompute_inference(rules);
        true
    }

    pub fn note_category_pick(&mut self, id: Option<String>) {
        self.category_id = id.clone();
        self.category_explicit = id.is_some();
    }

    /// A non-empty directory is a manual override and sticks. Clearing
    /// the field drops the override so category routing can resume.
    pub fn note_dir_edit(&mut self, dir: &str) {
        let dir = dir.trim();
        if dir.is_empty() {
            self.dir_override = false;
            self.manual_dir.clear();
            return;
        }
        self.dir_override = true;
        self.manual_dir = dir.to_string();
    }

    /// Settings or rule edits may refresh an unaccepted preview. An
    /// explicit category or a manual directory is not replaced.
    pub fn refresh_rules(&mut self, rules: &[CategoryRule]) {
        self.recompute_inference(rules);
    }

    pub fn preview(&self, root: &Path, rules: &[CategoryRule]) -> DecisionPreview {
        let filename = util::sanitize_filename(&self.filename_raw);
        let filename_differs = filename != self.filename_raw;
        let dest_dir = if self.dir_override {
            self.manual_dir.clone()
        } else if self.use_category {
            match self
                .category_id
                .as_deref()
                .and_then(|id| rules.iter().find(|rule| rule.id == id))
            {
                Some(rule) => display_path(&root.join(&rule.folder)),
                None => display_path(root),
            }
        } else {
            display_path(root)
        };
        DecisionPreview {
            dest_dir,
            filename,
            filename_differs,
            category_id: self.category_id.clone(),
        }
    }

    pub fn decision(
        &self,
        root: &Path,
        rules: &[CategoryRule],
        start_paused: bool,
    ) -> UserDecision {
        let preview = self.preview(root, rules);
        UserDecision {
            dest_dir: preview.dest_dir,
            filename: preview.filename,
            start_paused,
        }
    }
}

pub fn category_index(rules: &[CategoryRule], id: Option<&str>) -> Option<usize> {
    let id = id?;
    rules.iter().position(|rule| rule.id == id)
}

/// Stable id for a quick-added folder: lower case, non-alphanumerics
/// collapsed to single hyphens. The folder text itself is the folder name.
pub fn category_id_from_folder(folder: &str) -> String {
    let mut id = String::new();
    let mut hyphen = false;
    for ch in folder.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            id.push(ch.to_ascii_lowercase());
            hyphen = false;
        } else if !hyphen && !id.is_empty() {
            id.push('-');
            hyphen = true;
        }
    }
    while id.ends_with('-') {
        id.pop();
    }
    id
}

fn parse_extensions(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            if part.starts_with('.') {
                part.to_string()
            } else {
                format!(".{part}")
            }
        })
        .collect()
}

/// Validate the whole effective set after appending one rule.
///
/// The returned set is the normalized one `validate_categories` accepts.
/// On conflict nothing here is persisted; the caller keeps the previous set.
/// The new rule is inserted just before `other` when that rule exists.
pub fn quick_add_rules(
    existing: &[CategoryRule],
    folder: &str,
    extensions: &str,
) -> Result<Vec<CategoryRule>, String> {
    let folder = folder.trim();
    if folder.is_empty() {
        return Err("category folder name is empty".into());
    }
    let id = category_id_from_folder(folder);
    if id.is_empty() {
        return Err("category id derived from the folder is empty".into());
    }
    let rule = CategoryRule {
        id,
        folder: folder.to_string(),
        suffixes: parse_extensions(extensions),
    };
    let mut rules = existing.to_vec();
    if let Some(position) = rules.iter().position(|rule| rule.id == "other") {
        rules.insert(position, rule);
    } else {
        rules.push(rule);
    }
    validate_categories(&rules).map_err(|error| error.to_string())
}

/// What one quick-add did after validation and the settings write.
///
/// A conflict is [`QuickAddApplied::Rejected`] and the write is not
/// attempted. A failed write is also rejected, and the previous rules
/// stay. Only a write that succeeded is [`QuickAddApplied::Stored`].
pub enum QuickAddApplied {
    Rejected {
        error: String,
    },
    Stored {
        rules: Vec<CategoryRule>,
        added_id: String,
    },
}

/// Validate, then accept the rule only when `write` succeeds.
///
/// `write` receives the normalized set and performs the settings save.
/// Queuing that save is not success: the closure has to return the
/// disk-write result. On `Err` the rule is not added.
pub fn commit_quick_add(
    existing: &[CategoryRule],
    folder: &str,
    extensions: &str,
    write: impl FnOnce(&[CategoryRule]) -> Result<(), String>,
) -> QuickAddApplied {
    let normalized = match quick_add_rules(existing, folder, extensions) {
        Ok(rules) => rules,
        Err(error) => return QuickAddApplied::Rejected { error },
    };
    if let Err(error) = write(&normalized) {
        return QuickAddApplied::Rejected { error };
    }
    QuickAddApplied::Stored {
        added_id: category_id_from_folder(folder.trim()),
        rules: normalized,
    }
}

/// Most-recent-first, duplicates removed, capped at [`RECENT_DIR_CAP`].
/// An empty directory does not change the list.
pub fn push_recent_dir(existing: &[String], dir: &str) -> Vec<String> {
    let dir = dir.trim();
    if dir.is_empty() {
        return existing.to_vec();
    }
    let mut out = Vec::with_capacity(RECENT_DIR_CAP.min(existing.len() + 1));
    out.push(dir.to_string());
    for item in existing {
        if item == dir {
            continue;
        }
        if out.len() == RECENT_DIR_CAP {
            break;
        }
        out.push(item.clone());
    }
    out
}

// ---------------------------------------------------------------------
// Window session. Pure helpers above decide; this section only applies them.

struct OpenRow {
    caller: String,
    tx: String,
    url: String,
    browser_size: Option<u64>,
    expires_at_ms: Option<i64>,
    locked: bool,
}

struct ProbeDone {
    generation: u64,
    caller: String,
    tx: String,
    landed: bool,
    filename: Option<String>,
    size: Option<u64>,
}

/// Dialog state for one GUI session. The window is created on the first
/// live handoff and kept hidden afterwards. Discovery updates replace
/// `queue`; they do not focus the window.
pub struct DialogSession {
    window: Option<Rc<crate::HandoffWindow>>,
    queue: Vec<DiscoveryEntry>,
    failed_callers: BTreeSet<String>,
    skip: BTreeSet<(String, String)>,
    open: Option<OpenRow>,
    draft: DialogDraft,
    rules: Vec<CategoryRule>,
    rules_sig: String,
    /// Category id saved by quick-add before the engine has applied it.
    held_category: Option<String>,
    probe_generation: u64,
    probe_rx: Option<mpsc::Receiver<ProbeDone>>,
    probed: Option<Option<u64>>,
    suppress_dir: bool,
    suppress_name: bool,
    suppress_category: bool,
    suppress_use: bool,
    closing: bool,
    close_quietly: bool,
    quick_open: bool,
    error: String,
    status: String,
    status_until: Option<Instant>,
    category_sig: String,
    recent_sig: String,
    /// This window instance has run `apply-appearance` at least once.
    /// Stays false until that first paint, including when the indexes
    /// already match the window defaults.
    appearance_applied: bool,
    /// One place-and-focus per queue head. Refresh does not touch it.
    first_show: FirstShowGate,
    /// Monitor this dialog was presented on. The detail window inherits it.
    presented_monitor: Option<MonitorSnap>,
}

impl Default for DialogSession {
    fn default() -> Self {
        DialogSession {
            window: None,
            queue: Vec::new(),
            failed_callers: BTreeSet::new(),
            skip: BTreeSet::new(),
            open: None,
            draft: DialogDraft::initial(false, "", &[]),
            rules: Vec::new(),
            rules_sig: String::new(),
            held_category: None,
            probe_generation: 0,
            probe_rx: None,
            probed: None,
            suppress_dir: false,
            suppress_name: false,
            suppress_category: false,
            suppress_use: false,
            closing: false,
            close_quietly: false,
            quick_open: false,
            error: String::new(),
            status: String::new(),
            status_until: None,
            category_sig: String::new(),
            recent_sig: String::new(),
            appearance_applied: false,
            first_show: FirstShowGate::default(),
            presented_monitor: None,
        }
    }
}

/// Replace the queue from one discovery pass. A caller whose read failed
/// is not treated as "everything of theirs is gone".
pub fn note_discovery(g: &Gui, entries: &[DiscoveryEntry], failed: &BTreeSet<String>) {
    let queue = queue_from_discovery(entries);
    let mut ui = g.handoff_ui.borrow_mut();
    ui.failed_callers = failed.clone();
    if let Some(open) = ui.open.as_mut() {
        if !failed.contains(&open.caller) {
            match reconcile_front(&open.caller, &open.tx, &queue) {
                FrontUpdate::LockExpired => open.locked = true,
                FrontUpdate::ClosedElsewhere => ui.close_quietly = true,
                FrontUpdate::StillLive => {}
            }
        }
    }
    ui.queue = queue;
}

/// Countdown, probe results, and the next dialog. A row that is already
/// open is only repainted: that path calls [`decide_refresh`], which
/// cannot place or focus. A new head goes through [`open_front`].
pub fn tick(g: &Rc<Gui>) {
    if g.handoff.borrow().is_none() {
        return;
    }
    if g.handoff_ui.borrow().close_quietly {
        g.handoff_ui.borrow_mut().close_quietly = false;
        abandon(g);
        return;
    }
    drain_probe(g);
    refresh_lock(g);
    // The scrutinee borrow lives until the match ends. OpenNext stores
    // the dialog window with a second borrow of this RefCell; holding
    // the first one panics at that store (U8: no dialog while running,
    // and a silent exit when setup presents Prepared rows).
    let action = tick_action(g.handoff_ui.borrow().open.is_some());
    match action {
        TickAction::OpenNext => open_next(g),
        TickAction::Refresh => {
            // The result is fixed. Calling it here keeps the refresh
            // path on the function the tests assert never focuses.
            let _ = decide_refresh(&g.handoff_ui.borrow().first_show);
            sync_rules(g);
            paint(g);
        }
    }
}

fn window(g: &Gui) -> Option<Rc<crate::HandoffWindow>> {
    g.handoff_ui.borrow().window.as_ref().map(Rc::clone)
}

/// Put the open handoff dialog back in front without moving it.
///
/// Showing a detail window can activate that new window. When a successor
/// dialog was just presented, it has to end in front of the detail window.
/// This does not count as a new presentation and does not change position.
pub(crate) fn refocus_open_dialog(g: &Gui) {
    if g.handoff_ui.borrow().open.is_none() {
        return;
    }
    let Some(window) = window(g) else { return };
    use slint::winit_030::WinitWindowAccessor;
    let _ = window.window().with_winit_window(|winit_window| {
        winit_window.focus_window();
    });
    window.invoke_grab_focus();
}

fn ensure_window(g: &Rc<Gui>) {
    if g.handoff_ui.borrow().window.is_some() {
        return;
    }
    let created = match crate::HandoffWindow::new() {
        Ok(window) => window,
        Err(error) => {
            tracing::error!(%error, "handoff: cannot create the dialog window");
            return;
        }
    };
    created
        .global::<crate::I18n>()
        .on_translate(|key, language| {
            if language == 1 {
                crate::i18n::vi(key.as_str()).into()
            } else {
                key
            }
        });
    let gui = Rc::clone(g);
    created.window().on_close_requested(move || {
        let completed = request_close(&gui);
        let successor_open = gui.handoff_ui.borrow().open.is_some();
        if close_request_hides_window(completed, successor_open) {
            slint::CloseRequestResponse::HideWindow
        } else {
            slint::CloseRequestResponse::KeepWindowShown
        }
    });
    let gui = Rc::clone(g);
    created.on_copy_url(move || copy_url(&gui));
    let gui = Rc::clone(g);
    created.on_use_category_toggled(move |checked| {
        if gui.handoff_ui.borrow().suppress_use {
            return;
        }
        gui.handoff_ui.borrow_mut().draft.use_category = checked;
    });
    let gui = Rc::clone(g);
    created.on_category_picked(move |index| {
        if gui.handoff_ui.borrow().suppress_category || index < 0 {
            return;
        }
        let mut ui = gui.handoff_ui.borrow_mut();
        let id = ui.rules.get(index as usize).map(|rule| rule.id.clone());
        ui.draft.note_category_pick(id);
    });
    let gui = Rc::clone(g);
    created.on_dir_edited(move || {
        if gui.handoff_ui.borrow().suppress_dir
            || gui
                .handoff_ui
                .borrow()
                .open
                .as_ref()
                .is_some_and(|open| open.locked)
        {
            return;
        }
        let Some(window) = window(&gui) else { return };
        let text = window.get_dir_text().to_string();
        gui.handoff_ui.borrow_mut().draft.note_dir_edit(&text);
    });
    let gui = Rc::clone(g);
    created.on_browse(move || browse_dir(&gui));
    let gui = Rc::clone(g);
    created.on_recent_picked(move |value| {
        gui.handoff_ui
            .borrow_mut()
            .draft
            .note_dir_edit(value.as_str());
    });
    let gui = Rc::clone(g);
    created.on_filename_edited(move || {
        if gui.handoff_ui.borrow().suppress_name
            || gui
                .handoff_ui
                .borrow()
                .open
                .as_ref()
                .is_some_and(|open| open.locked)
        {
            return;
        }
        let Some(window) = window(&gui) else { return };
        let text = window.get_filename_text().to_string();
        let mut ui = gui.handoff_ui.borrow_mut();
        let rules = ui.rules.clone();
        ui.draft.note_filename_edit(&text, &rules);
    });
    let gui = Rc::clone(g);
    created.on_download(move || submit(&gui, false));
    let gui = Rc::clone(g);
    created.on_add_paused(move || submit(&gui, true));
    let gui = Rc::clone(g);
    created.on_dismiss(move || {
        let _ = request_close(&gui);
    });
    let gui = Rc::clone(g);
    created.on_quick_toggle(move || {
        let open = {
            let mut ui = gui.handoff_ui.borrow_mut();
            if ui.open.as_ref().is_some_and(|row| row.locked) {
                return;
            }
            ui.quick_open = !ui.quick_open;
            ui.error.clear();
            ui.quick_open
        };
        if let Some(window) = window(&gui) {
            window.set_quick_open(open);
        }
    });
    let gui = Rc::clone(g);
    created.on_quick_save(move || save_quick_add(&gui));
    let gui = Rc::clone(g);
    created.on_quick_cancel(move || {
        let mut ui = gui.handoff_ui.borrow_mut();
        ui.quick_open = false;
        if let Some(window) = ui.window.as_ref().map(Rc::clone) {
            drop(ui);
            window.set_quick_error(SharedString::default());
            window.set_quick_open(false);
        }
    });
    g.handoff_ui.borrow_mut().window = Some(Rc::new(created));
}

fn open_next(g: &Rc<Gui>) {
    let entry = {
        let ui = g.handoff_ui.borrow();
        next_live(&ui.queue, &ui.skip).cloned()
    };
    if let Some(entry) = entry {
        open_front(g, entry);
    }
}

fn open_front(g: &Rc<Gui>, entry: DiscoveryEntry) {
    ensure_window(g);
    if g.handoff_ui.borrow().window.is_none() || g.handoff_ui.borrow().open.is_some() {
        return;
    }
    let settings = g.engine.settings();
    let filename = entry
        .intent
        .as_ref()
        .map(initial_filename)
        .unwrap_or_default();
    let url = entry
        .intent
        .as_ref()
        .map(|intent| intent.url.clone())
        .unwrap_or_default();
    let browser_size = entry.intent.as_ref().and_then(|intent| intent.size_hint);
    {
        let mut ui = g.handoff_ui.borrow_mut();
        adopt_rules(&settings, &mut ui);
        let rules = ui.rules.clone();
        ui.draft = DialogDraft::initial(settings.auto_categorize, &filename, &rules);
        ui.probed = None;
        ui.error.clear();
        ui.status.clear();
        ui.status_until = None;
        ui.quick_open = false;
        ui.category_sig.clear();
        ui.recent_sig.clear();
        ui.open = Some(OpenRow {
            caller: entry.caller.clone(),
            tx: entry.tx.clone(),
            url: url.clone(),
            browser_size,
            expires_at_ms: entry.expires_at_ms,
            locked: false,
        });
    }
    paint(g);
    let Some(window) = window(g) else { return };
    // Monitor choice is taken before show(), so the dialog itself is not
    // the foreground window we measure, and the hidden main window is not
    // consulted at all. The detail window inherits this same monitor.
    let (source, target) = current_presentation_target();
    g.handoff_ui.borrow_mut().presented_monitor = Some(target);
    let _ = window.show();
    let caller = entry.caller.clone();
    let tx = entry.tx.clone();
    let settled = handoff_native_settled(&window);
    let decision = {
        let mut ui = g.handoff_ui.borrow_mut();
        decide_first_show(&mut ui.first_show, &caller, &tx, settled)
    };
    match decision {
        FirstShow::Now => {
            if !raise_handoff_window(&window, source, &target) {
                tracing::error!(
                    caller,
                    tx,
                    "handoff: the native window looked settled but focus_window was not called"
                );
            }
        }
        FirstShow::Later => {
            tracing::info!(
                caller,
                tx,
                monitor = ?source,
                "handoff: native window not ready; one presentation continuation armed"
            );
            if !defer_first_show(g, caller.clone(), tx.clone(), source, target) {
                // Startup refresh runs before the event loop can queue
                // the continuation. Leave the head unopened so the first
                // timer tick presents it once, instead of arming a
                // continuation that will never run.
                tracing::info!(
                    caller,
                    tx,
                    "handoff: presentation deferred until the event loop can schedule it"
                );
                retract_unpresented(g, &caller, &tx);
            }
        }
        FirstShow::NotNow => {}
    }
    start_probe(g, &url, &entry.caller, &entry.tx);
}

fn handoff_native_settled(window: &crate::HandoffWindow) -> bool {
    use slint::winit_030::WinitWindowAccessor;
    window
        .window()
        .with_winit_window(|winit_window| {
            let inner = winit_window.inner_size();
            let outer = winit_window.outer_size();
            native_size_settled(inner.width, inner.height, outer.width, outer.height)
        })
        .unwrap_or(false)
}

/// Place, then `focus_window`, then the existing internal grab-focus.
/// Returns false when the native window is still missing or zero-sized.
fn raise_handoff_window(
    window: &crate::HandoffWindow,
    source: MonitorSource,
    target: &MonitorSnap,
) -> bool {
    use slint::winit_030::WinitWindowAccessor;
    let raised = window.window().with_winit_window(|winit_window| {
        let inner = winit_window.inner_size();
        let outer = winit_window.outer_size();
        if !native_size_settled(inner.width, inner.height, outer.width, outer.height) {
            return false;
        }
        let placed = placement_for(
            target,
            NativeSize {
                inner_width: inner.width,
                inner_height: inner.height,
                outer_width: outer.width,
                outer_height: outer.height,
            },
        );
        apply_placement(winit_window, placed);
        winit_window.focus_window();
        tracing::info!(
            monitor = ?source,
            x = placed.x,
            y = placed.y,
            inner_w = placed.inner_width,
            inner_h = placed.inner_height,
            scale = target.scale,
            "handoff: first presentation focused the dialog"
        );
        true
    });
    if raised == Some(true) {
        window.invoke_grab_focus();
        true
    } else {
        false
    }
}

pub(crate) fn apply_placement(
    winit_window: &slint::winit_030::winit::window::Window,
    placed: AppliedPlacement,
) {
    use slint::winit_030::winit::dpi::{PhysicalPosition, PhysicalSize};
    let inner = winit_window.inner_size();
    let want_w = placed.inner_width.max(1) as u32;
    let want_h = placed.inner_height.max(1) as u32;
    if inner.width != want_w || inner.height != want_h {
        // Physical pixels, already converted once from the logical default
        // by the target monitor's scale. Windows applies this with
        // SWP_ASYNCWINDOWPOS; the position below uses the same outer size.
        let _ = winit_window.request_inner_size(PhysicalSize::new(want_w, want_h));
    }
    winit_window.set_outer_position(PhysicalPosition::new(placed.x, placed.y));
}

/// The one deferred presentation. It is not the 150 ms poll: it waits
/// until this window's winit handle exists, then focuses at most once,
/// and only if that queue head is still the open row.
fn retract_unpresented(g: &Gui, caller: &str, tx: &str) {
    let window = {
        let mut ui = g.handoff_ui.borrow_mut();
        let current = ui
            .open
            .as_ref()
            .is_some_and(|open| open.caller == caller && open.tx == tx);
        if !current {
            return;
        }
        ui.open = None;
        disarm_continuation(&mut ui.first_show, caller, tx);
        ui.window.as_ref().map(Rc::clone)
    };
    if let Some(window) = window {
        let _ = window.hide();
    }
}

/// Returns false when the continuation could not be queued. The caller
/// puts the head back; a later tick presents it.
fn defer_first_show(
    g: &Rc<Gui>,
    caller: String,
    tx: String,
    source: MonitorSource,
    target: MonitorSnap,
) -> bool {
    let Some(window) = window(g) else {
        return false;
    };
    let weak = window.as_weak();
    let gui = Rc::downgrade(g);
    let caller_log = caller.clone();
    let tx_log = tx.clone();
    let scheduled = slint::spawn_local(async move {
        let Some(window) = weak.upgrade() else {
            return;
        };
        let ready = {
            use slint::winit_030::WinitWindowAccessor;
            window.window().winit_window().await.is_ok()
        };
        if !ready {
            tracing::error!(
                caller,
                tx,
                "handoff: presentation continuation lost the native window"
            );
            return;
        }
        let Some(g) = gui.upgrade() else {
            return;
        };
        let decision = {
            let mut ui = g.handoff_ui.borrow_mut();
            let current = ui
                .open
                .as_ref()
                .is_some_and(|open| open.caller == caller && open.tx == tx);
            if !current {
                disarm_continuation(&mut ui.first_show, &caller, &tx);
                FirstShow::NotNow
            } else {
                decide_continuation(&mut ui.first_show, &caller, &tx)
            }
        };
        if decision != FirstShow::Now {
            return;
        }
        if !raise_handoff_window(&window, source, &target) {
            // The handle exists but the size is still zero. Focus once
            // anyway, using the logical default and an empty frame, and
            // do not arm another wait.
            raise_handoff_window_unsettled(&window, source, &target);
        }
    });
    match scheduled {
        Ok(_) => true,
        Err(error) => {
            tracing::error!(
                %error,
                caller = caller_log,
                tx = tx_log,
                "handoff: the one presentation continuation was not scheduled"
            );
            false
        }
    }
}

fn raise_handoff_window_unsettled(
    window: &crate::HandoffWindow,
    source: MonitorSource,
    target: &MonitorSnap,
) {
    use slint::winit_030::WinitWindowAccessor;
    let raised = window.window().with_winit_window(|winit_window| {
        let placed = placement_for(
            target,
            NativeSize {
                inner_width: 0,
                inner_height: 0,
                outer_width: 0,
                outer_height: 0,
            },
        );
        apply_placement(winit_window, placed);
        winit_window.focus_window();
        tracing::info!(
            monitor = ?source,
            x = placed.x,
            y = placed.y,
            "handoff: first presentation focused before the native size settled"
        );
        true
    });
    if raised == Some(true) {
        window.invoke_grab_focus();
    }
}

/// Monitor for a first presentation. Order is the foreground window's
/// monitor, then the cursor, then the primary work area. The main window
/// is not an input: a tray-hidden main window must not decide the screen.
pub(crate) fn presentation_target() -> (MonitorSource, MonitorSnap) {
    current_presentation_target()
}

fn current_presentation_target() -> (MonitorSource, MonitorSnap) {
    #[cfg(windows)]
    {
        let foreground = foreground_monitor();
        let cursor = cursor_monitor();
        let primary = primary_monitor().unwrap_or_else(fallback_primary);
        return choose_monitor(foreground, cursor, primary);
    }
    #[cfg(not(windows))]
    {
        choose_monitor(None, None, fallback_primary())
    }
}

fn fallback_primary() -> MonitorSnap {
    MonitorSnap {
        work: PxRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        },
        scale: 1.0,
    }
}

#[cfg(windows)]
fn foreground_monitor() -> Option<MonitorSnap> {
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTONULL};
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, IsWindowVisible};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd == 0 || IsWindowVisible(hwnd) == 0 {
            return None;
        }
        monitor_snap(MonitorFromWindow(hwnd, MONITOR_DEFAULTTONULL))
    }
}

#[cfg(windows)]
fn cursor_monitor() -> Option<MonitorSnap> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL};
    use windows_sys::Win32::UI::WindowsAndMessaging::GetCursorPos;
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut pt) == 0 {
            return None;
        }
        monitor_snap(MonitorFromPoint(pt, MONITOR_DEFAULTTONULL))
    }
}

#[cfg(windows)]
fn primary_monitor() -> Option<MonitorSnap> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{MonitorFromWindow, MONITOR_DEFAULTTOPRIMARY};
    use windows_sys::Win32::UI::WindowsAndMessaging::{SystemParametersInfoW, SPI_GETWORKAREA};
    // A null hwnd with DEFAULTTOPRIMARY is the primary monitor, not
    // whichever monitor happens to contain the origin.
    let snap = unsafe { monitor_snap(MonitorFromWindow(0, MONITOR_DEFAULTTOPRIMARY)) };
    if snap.is_some() {
        return snap;
    }
    unsafe {
        let mut area = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        if SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            &mut area as *mut RECT as *mut core::ffi::c_void,
            0,
        ) == 0
        {
            return None;
        }
        let work = PxRect {
            left: area.left,
            top: area.top,
            right: area.right,
            bottom: area.bottom,
        };
        if work.right <= work.left || work.bottom <= work.top {
            return None;
        }
        Some(MonitorSnap { work, scale: 1.0 })
    }
}

#[cfg(windows)]
fn monitor_snap(monitor: isize) -> Option<MonitorSnap> {
    use windows_sys::Win32::Foundation::RECT;
    use windows_sys::Win32::Graphics::Gdi::{GetMonitorInfoW, MONITORINFO};
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    if monitor == 0 {
        return None;
    }
    unsafe {
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rcWork: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            dwFlags: 0,
        };
        if GetMonitorInfoW(monitor, &mut info) == 0 {
            return None;
        }
        let work = PxRect {
            left: info.rcWork.left,
            top: info.rcWork.top,
            right: info.rcWork.right,
            bottom: info.rcWork.bottom,
        };
        if work.right <= work.left || work.bottom <= work.top {
            return None;
        }
        let mut dpi_x = 0u32;
        let mut dpi_y = 0u32;
        let scale = if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) == 0
            && dpi_y > 0
        {
            dpi_y as f64 / 96.0
        } else {
            1.0
        };
        Some(MonitorSnap { work, scale })
    }
}

fn refresh_lock(g: &Gui) {
    let now = util::now_millis();
    let mut ui = g.handoff_ui.borrow_mut();
    if let Some(open) = ui.open.as_mut() {
        if submit_locked(now, open.expires_at_ms) {
            open.locked = true;
        }
    }
}

fn sync_rules(g: &Gui) {
    let settings = g.engine.settings();
    let mut ui = g.handoff_ui.borrow_mut();
    let previous = ui.rules_sig.clone();
    adopt_rules(&settings, &mut ui);
    if ui.rules_sig != previous {
        let rules = ui.rules.clone();
        ui.draft.refresh_rules(&rules);
        ui.category_sig.clear();
    }
}

fn adopt_rules(settings: &usagi::config::Settings, ui: &mut DialogSession) {
    let live = settings.category_rules();
    if let Some(id) = ui.held_category.clone() {
        if live.iter().any(|rule| rule.id == id) {
            ui.held_category = None;
            ui.rules = live;
        }
    } else {
        ui.rules = live;
    }
    ui.rules_sig = ui
        .rules
        .iter()
        .map(|rule| format!("{}:{}:{}", rule.id, rule.folder, rule.suffixes.join(",")))
        .collect::<Vec<_>>()
        .join("|");
}

fn paint(g: &Gui) {
    let Some(window) = window(g) else { return };
    let settings = g.engine.settings();
    let root = settings.download_dir();
    let now = util::now_millis();
    let snap = {
        let mut ui = g.handoff_ui.borrow_mut();
        let Some((url, browser_size, expires_at_ms, locked_row)) = ui.open.as_ref().map(|open| {
            (
                open.url.clone(),
                open.browser_size,
                open.expires_at_ms,
                open.locked,
            )
        }) else {
            return;
        };
        if ui.status_until.is_some_and(|until| Instant::now() >= until) {
            ui.status.clear();
            ui.status_until = None;
        }
        let locked = locked_row || submit_locked(now, expires_at_ms);
        let preview = ui.draft.preview(&root, &ui.rules);
        let dir = if ui.draft.dir_override {
            ui.draft.manual_dir.clone()
        } else {
            preview.dest_dir.clone()
        };
        let labels: Vec<String> = {
            let vietnamese =
                g.ui.upgrade()
                    .is_some_and(|main| main.get_language_index() == 1);
            ui.rules
                .iter()
                .map(|rule| crate::state::category_item_label(&rule.id, vietnamese))
                .collect()
        };
        let label_sig = labels.join("\u{1e}");
        let category_model = if label_sig != ui.category_sig {
            ui.category_sig = label_sig;
            Some(labels)
        } else {
            None
        };
        let recent = settings.recent_download_dirs.clone();
        let recent_sig = recent.join("\u{1e}");
        let recent_model = if recent_sig != ui.recent_sig {
            ui.recent_sig = recent_sig;
            Some(recent)
        } else {
            None
        };
        let category_index = category_index(&ui.rules, ui.draft.category_id.as_deref())
            .map(|index| index as i32)
            .unwrap_or(-1);
        let size = size_line(g, size_view(browser_size, ui.probed));
        let sanitized = if preview.filename_differs {
            format!("{} {}", tr(g, "Will save as"), preview.filename)
        } else {
            String::new()
        };
        let preview_text = display_path(&Path::new(&preview.dest_dir).join(&preview.filename));
        Paint {
            url,
            countdown: format!(
                "{} {}",
                tr(g, "Time left"),
                format_countdown(countdown_seconds(now, expires_at_ms))
            ),
            size,
            sanitized,
            preview_text,
            error: ui.error.clone(),
            status: ui.status.clone(),
            expired: locked,
            use_category: ui.draft.use_category,
            category_model,
            category_index,
            dir,
            recent_model,
            filename: ui.draft.filename_raw.clone(),
            quick_open: ui.quick_open,
            lang: g
                .ui
                .upgrade()
                .map(|main| main.get_language_index())
                .unwrap_or(0),
            theme: g
                .ui
                .upgrade()
                .map(|main| main.get_theme_index())
                .unwrap_or(0),
            accent: g
                .ui
                .upgrade()
                .map(|main| main.get_accent_index())
                .unwrap_or(0),
        }
    };
    let differs = window.get_language_index() != snap.lang
        || window.get_theme_index() != snap.theme
        || window.get_accent_index() != snap.accent;
    if appearance_pending(g.handoff_ui.borrow().appearance_applied, differs) {
        window.set_language_index(snap.lang);
        window.set_theme_index(snap.theme);
        window.set_accent_index(snap.accent);
        window.invoke_apply_appearance();
        g.handoff_ui.borrow_mut().appearance_applied = true;
    }
    window.set_url_text(SharedString::from(snap.url.as_str()));
    window.set_countdown_text(SharedString::from(snap.countdown.as_str()));
    window.set_size_text(SharedString::from(snap.size.as_str()));
    window.set_sanitized_text(SharedString::from(snap.sanitized.as_str()));
    window.set_preview_text(SharedString::from(snap.preview_text.as_str()));
    window.set_error_text(SharedString::from(snap.error.as_str()));
    window.set_status_text(SharedString::from(snap.status.as_str()));
    window.set_expired(snap.expired);
    window.set_quick_open(snap.quick_open);
    {
        let mut ui = g.handoff_ui.borrow_mut();
        ui.suppress_use = true;
        ui.suppress_category = true;
        ui.suppress_dir = true;
        ui.suppress_name = true;
    }
    if let Some(labels) = snap.category_model {
        window.set_category_labels(string_model(&labels));
    }
    if let Some(labels) = snap.recent_model {
        window.set_recent_labels(string_model(&labels));
    }
    window.set_use_category(snap.use_category);
    if window.get_category_index() != snap.category_index {
        window.set_category_index(snap.category_index);
    }
    if window.get_dir_text().as_str() != snap.dir {
        window.set_dir_text(SharedString::from(snap.dir.as_str()));
    }
    if window.get_filename_text().as_str() != snap.filename {
        window.set_filename_text(SharedString::from(snap.filename.as_str()));
    }
    {
        let mut ui = g.handoff_ui.borrow_mut();
        ui.suppress_use = false;
        ui.suppress_category = false;
        ui.suppress_dir = false;
        ui.suppress_name = false;
    }
}

struct Paint {
    url: String,
    countdown: String,
    size: String,
    sanitized: String,
    preview_text: String,
    error: String,
    status: String,
    expired: bool,
    use_category: bool,
    category_model: Option<Vec<String>>,
    category_index: i32,
    dir: String,
    recent_model: Option<Vec<String>>,
    filename: String,
    quick_open: bool,
    lang: i32,
    theme: i32,
    accent: i32,
}

fn string_model(items: &[String]) -> ModelRc<SharedString> {
    let rows: Vec<SharedString> = items.iter().map(SharedString::from).collect();
    ModelRc::from(Rc::new(VecModel::from(rows)))
}

fn tr(g: &Gui, key: &str) -> String {
    g.ui.upgrade()
        .map(|ui| ui.invoke_translate(key.into()).to_string())
        .unwrap_or_else(|| key.to_string())
}

fn size_line(g: &Gui, view: SizeView) -> String {
    let provenance = match view.provenance {
        SizeProvenance::FromBrowser => tr(g, "From the browser"),
        SizeProvenance::Probed => tr(g, "Probed"),
        SizeProvenance::ProbedDiffers => tr(g, "Probed, differs from the browser"),
        SizeProvenance::Unknown => tr(g, "Size unknown"),
    };
    match view.bytes {
        Some(bytes) => format!("{} - {provenance}", util::human_bytes(bytes as f64)),
        None => provenance,
    }
}

fn copy_url(g: &Gui) {
    let url = g
        .handoff_ui
        .borrow()
        .open
        .as_ref()
        .map(|open| open.url.clone())
        .unwrap_or_default();
    if url.is_empty() {
        return;
    }
    crate::state::copy_to_clipboard(&url);
    let mut ui = g.handoff_ui.borrow_mut();
    ui.status = tr(g, "URL copied");
    ui.status_until = Some(Instant::now() + Duration::from_secs(2));
    if let Some(window) = ui.window.as_ref().map(Rc::clone) {
        let status = ui.status.clone();
        drop(ui);
        window.set_status_text(SharedString::from(status));
    }
}

fn browse_dir(g: &Rc<Gui>) {
    let title = tr(g, "Choose a download folder");
    let Some(dir) = rfd::FileDialog::new().set_title(title).pick_folder() else {
        return;
    };
    g.handoff_ui
        .borrow_mut()
        .draft
        .note_dir_edit(&dir.to_string_lossy());
    paint(g);
}

fn save_quick_add(g: &Rc<Gui>) {
    let Some(window) = window(g) else { return };
    let folder = window.get_quick_folder().to_string();
    let extensions = window.get_quick_extensions().to_string();
    let existing = g.handoff_ui.borrow().rules.clone();
    let applied = commit_quick_add(&existing, &folder, &extensions, |normalized| {
        let mut settings = g.engine.settings();
        settings.categories = Some(normalized.to_vec());
        g.engine
            .save_settings(settings)
            .map_err(|error| error.to_string())
    });
    let (rules, id) = match applied {
        QuickAddApplied::Rejected { error } => {
            window.set_quick_error(SharedString::from(error));
            return;
        }
        QuickAddApplied::Stored { rules, added_id } => (rules, added_id),
    };
    {
        let mut ui = g.handoff_ui.borrow_mut();
        ui.rules = rules;
        ui.rules_sig.clear();
        ui.held_category = Some(id.clone());
        ui.category_sig.clear();
        ui.draft.use_category = true;
        ui.draft.note_category_pick(Some(id));
        ui.quick_open = false;
        ui.error.clear();
    }
    window.set_quick_error(SharedString::default());
    window.set_quick_folder(SharedString::default());
    window.set_quick_extensions(SharedString::default());
    paint(g);
}

fn submit(g: &Rc<Gui>, start_paused: bool) {
    let (caller, tx, locked, expires) = {
        let ui = g.handoff_ui.borrow();
        let Some(open) = ui.open.as_ref() else { return };
        (
            open.caller.clone(),
            open.tx.clone(),
            open.locked,
            open.expires_at_ms,
        )
    };
    if locked || submit_locked(util::now_millis(), expires) {
        if let Some(open) = g.handoff_ui.borrow_mut().open.as_mut() {
            open.locked = true;
        }
        paint(g);
        return;
    }
    let Some(coordinator) = g.handoff.borrow().clone() else {
        set_error(g, "handoff is not running");
        return;
    };
    let reply = match coordinator.query(&caller, &tx) {
        Ok(reply) => reply,
        Err(error) => {
            set_error(g, &error.to_string());
            return;
        }
    };
    match transition_gate(reply.status) {
        TransitionGate::Proceed => {}
        TransitionGate::LockExpired => {
            if let Some(open) = g.handoff_ui.borrow_mut().open.as_mut() {
                open.locked = true;
            }
            paint(g);
            return;
        }
        TransitionGate::AlreadyHandled => {
            abandon(g);
            return;
        }
    }
    let decision = {
        let ui = g.handoff_ui.borrow();
        let root = g.engine.settings().download_dir();
        ui.draft.decision(&root, &ui.rules, start_paused)
    };
    let reply = match coordinator.commit(&caller, &tx, true, Some(decision.clone())) {
        Ok(reply) => reply,
        Err(error) => {
            set_error(g, &error.to_string());
            return;
        }
    };
    if commit_accepted(reply.status, reply.state == Some(HandoffState::Committed)) {
        // Only Download opens a detail window. Add (start paused) commits
        // and returns to the list, the same as a manual Add Download.
        let open_detail = crate::detail_assoc::opens_detail_window(if start_paused {
            crate::detail_assoc::DetailSource::HandoffAddPaused
        } else {
            crate::detail_assoc::DetailSource::HandoffDownload
        });
        let (filename, url, monitor, successor, materialize_error) = if open_detail {
            let ui = g.handoff_ui.borrow();
            let url = ui
                .open
                .as_ref()
                .map(|open| open.url.clone())
                .unwrap_or_default();
            let mut skip = ui.skip.clone();
            skip.insert((caller.clone(), tx.clone()));
            let successor = next_live(&ui.queue, &skip).is_some();
            let materialize_error = if reply.status == HandoffStatus::RecoveryRequired {
                reply.detail.clone().unwrap_or_default()
            } else {
                String::new()
            };
            (
                decision.filename.clone(),
                url,
                ui.presented_monitor,
                successor,
                materialize_error,
            )
        } else {
            (String::new(), String::new(), None, false, String::new())
        };
        remember_dir(g, &decision.dest_dir);
        // The next handoff, when there is one, is shown and focused here,
        // before the detail window. The detail window then stays off the
        // center focus so that dialog keeps the foreground.
        abandon(g);
        if open_detail {
            let assoc = crate::detail_assoc::AssocKey {
                caller: caller.clone(),
                tx: tx.clone(),
            };
            crate::detail_window::open_from_handoff_download(
                g,
                assoc.clone(),
                filename,
                url,
                materialize_error,
                monitor,
                !successor,
            );
            // Read-only query for this pair. Not a second commit, and not
            // a search of the task list.
            if let Some(coordinator) = g.handoff.borrow().clone() {
                match coordinator.query(&caller, &tx) {
                    Ok(fresh) => crate::detail_window::apply_authoritative(g, &assoc, &fresh),
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            caller,
                            tx,
                            "detail window: association query failed; staying in waiting"
                        );
                    }
                }
            }
        }
        return;
    }
    if reply.status == HandoffStatus::Expired || reply.state == Some(HandoffState::Expired) {
        if let Some(open) = g.handoff_ui.borrow_mut().open.as_mut() {
            open.locked = true;
        }
        paint(g);
        return;
    }
    set_error(
        g,
        reply
            .detail
            .as_deref()
            .unwrap_or("the handoff was not saved"),
    );
}

fn request_close(g: &Rc<Gui>) -> bool {
    if g.handoff_ui.borrow().closing {
        return true;
    }
    let Some((caller, tx, locked, expires)) = ({
        let ui = g.handoff_ui.borrow();
        ui.open.as_ref().map(|open| {
            (
                open.caller.clone(),
                open.tx.clone(),
                open.locked,
                open.expires_at_ms,
            )
        })
    }) else {
        return true;
    };
    if locked || submit_locked(util::now_millis(), expires) {
        abandon(g);
        return true;
    }
    let Some(coordinator) = g.handoff.borrow().clone() else {
        set_error(g, "handoff is not running");
        return false;
    };
    let reply = match coordinator.query(&caller, &tx) {
        Ok(reply) => reply,
        Err(error) => {
            set_error(g, &error.to_string());
            return false;
        }
    };
    match transition_gate(reply.status) {
        TransitionGate::Proceed => {}
        TransitionGate::LockExpired | TransitionGate::AlreadyHandled => {
            abandon(g);
            return true;
        }
    }
    match coordinator.discard(&caller, &tx) {
        Ok(reply)
            if reply.status == HandoffStatus::Discarded
                || reply.status == HandoffStatus::Expired =>
        {
            abandon(g);
            true
        }
        Ok(reply)
            if transition_gate(reply.status) == TransitionGate::AlreadyHandled
                || transition_gate(reply.status) == TransitionGate::LockExpired =>
        {
            abandon(g);
            true
        }
        Ok(reply) => {
            set_error(
                g,
                reply
                    .detail
                    .as_deref()
                    .unwrap_or("the handoff was not discarded"),
            );
            false
        }
        Err(error) => {
            set_error(g, &error.to_string());
            false
        }
    }
}

fn abandon(g: &Rc<Gui>) {
    if g.handoff_ui.borrow().closing {
        return;
    }
    {
        let mut ui = g.handoff_ui.borrow_mut();
        ui.closing = true;
        if let Some(open) = ui.open.take() {
            ui.skip.insert((open.caller, open.tx));
        }
        ui.probe_generation = ui.probe_generation.wrapping_add(1);
        ui.probe_rx = None;
        ui.quick_open = false;
        ui.error.clear();
        ui.probed = None;
    }
    if let Some(window) = window(g) {
        let _ = window.hide();
        window.set_quick_open(false);
    }
    reload_discovery(g);
    g.handoff_ui.borrow_mut().closing = false;
    open_next(g);
}

fn reload_discovery(g: &Gui) {
    let Some(coordinator) = g.handoff.borrow().clone() else {
        note_discovery(g, &[], &BTreeSet::new());
        return;
    };
    let callers = usagi::handoff::read_callers(&g.paths);
    let mut all = Vec::new();
    let mut failed = BTreeSet::new();
    for caller in callers {
        match coordinator.discovery(&caller) {
            Ok(entries) => all.extend(entries),
            Err(error) => {
                tracing::debug!(%caller, %error, "handoff discovery skipped");
                failed.insert(caller);
            }
        }
    }
    note_discovery(g, &all, &failed);
}

fn remember_dir(g: &Gui, dir: &str) {
    let mut settings = g.engine.settings();
    let next = push_recent_dir(&settings.recent_download_dirs, dir);
    if next == settings.recent_download_dirs {
        return;
    }
    settings.recent_download_dirs = next;
    if let Err(error) = g.engine.update_settings(settings) {
        tracing::warn!(%error, "handoff: recent directory was not saved");
    }
}

fn set_error(g: &Gui, message: &str) {
    g.handoff_ui.borrow_mut().error = message.to_string();
    if let Some(window) = window(g) {
        window.set_error_text(SharedString::from(message));
    }
}

fn start_probe(g: &Gui, url: &str, caller: &str, tx: &str) {
    let generation = {
        let mut ui = g.handoff_ui.borrow_mut();
        ui.probe_generation = ui.probe_generation.wrapping_add(1);
        ui.probed = None;
        ui.probe_generation
    };
    let net = g.engine.net();
    let pref = g.engine.settings().transport_pref;
    let timeout = Duration::from_secs(g.engine.settings().connect_timeout_secs.max(1));
    let (sender, receiver) = mpsc::channel();
    g.handoff_ui.borrow_mut().probe_rx = Some(receiver);
    let url = url.to_string();
    let caller = caller.to_string();
    let tx = tx.to_string();
    if std::thread::Builder::new()
        .name("udm-handoff-probe".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                probe_blocking(net, &url, pref, timeout)
            }));
            let (landed, filename, size) = match result {
                Ok(Some((filename, size))) => (true, filename, size),
                Ok(None) => (false, None, None),
                Err(_) => {
                    tracing::warn!("handoff: probe thread panicked");
                    (false, None, None)
                }
            };
            let _ = sender.send(ProbeDone {
                generation,
                caller,
                tx,
                landed,
                filename,
                size,
            });
        })
        .is_err()
    {
        tracing::warn!("handoff: could not start the probe thread");
    }
}

fn probe_blocking(
    net: usagi::net::NetStack,
    url: &str,
    pref: usagi::config::TransportPref,
    timeout: Duration,
) -> Option<(Option<String>, Option<u64>)> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()?;
    runtime.block_on(async {
        match tokio::time::timeout(timeout, net.probe(url, pref)).await {
            Ok(Ok(info)) => Some((info.filename_hint, info.size)),
            _ => None,
        }
    })
}

fn drain_probe(g: &Gui) {
    let pending = {
        let ui = g.handoff_ui.borrow();
        let Some(rx) = ui.probe_rx.as_ref() else {
            return;
        };
        let mut pending = Vec::new();
        while let Ok(item) = rx.try_recv() {
            pending.push(item);
        }
        pending
    };
    let mut dirty = false;
    for item in pending {
        let mut ui = g.handoff_ui.borrow_mut();
        let current = ui
            .open
            .as_ref()
            .map(|open| (open.caller.clone(), open.tx.clone()));
        if item.generation != ui.probe_generation
            || current.as_ref().map(|(c, t)| (c.as_str(), t.as_str()))
                != Some((item.caller.as_str(), item.tx.as_str()))
        {
            continue;
        }
        if !item.landed {
            continue;
        }
        ui.probed = Some(item.size);
        if let Some(hint) = item.filename.filter(|hint| !hint.trim().is_empty()) {
            let rules = ui.rules.clone();
            if ui.draft.note_probe_filename(&hint, &rules) {
                dirty = true;
            }
        } else {
            dirty = true;
        }
    }
    if dirty {
        paint(g);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use usagi::category::default_categories;
    use usagi::handoff::HandoffStatus;

    fn intent(url: &str, filename: Option<&str>, size: Option<u64>) -> HandoffIntent {
        HandoffIntent {
            url: url.into(),
            method: "GET".into(),
            filename_hint: filename.map(str::to_string),
            size_hint: size,
            browser_download_id: "dl-1".into(),
            dest_dir: None,
        }
    }

    fn entry(caller: &str, tx: &str, at: i64, expired: bool) -> DiscoveryEntry {
        DiscoveryEntry {
            caller: caller.into(),
            tx: tx.into(),
            intent: Some(intent("https://example.com/file.bin", None, None)),
            created_at_ms: at,
            expires_at_ms: Some(at + 600_000),
            expired,
        }
    }

    /// Session-shaped stand-in for `abandon` plus the platform close.
    /// No commit and no discard: the closed key is only skipped.
    struct Presentation {
        open: Option<(String, String)>,
        skip: BTreeSet<(String, String)>,
        visible: bool,
    }

    impl Presentation {
        fn present(&mut self, entry: &DiscoveryEntry) {
            self.open = Some((entry.caller.clone(), entry.tx.clone()));
            self.visible = true;
        }

        fn close_window_without_decision(&mut self, queue: &[DiscoveryEntry]) {
            if let Some(current) = self.open.take() {
                self.skip.insert(current);
            }
            self.visible = false;
            if let Some(next) = next_live(queue, &self.skip) {
                self.open = Some((next.caller.clone(), next.tx.clone()));
                self.visible = true;
            }
            if close_request_hides_window(true, self.open.is_some()) {
                self.visible = false;
            }
        }
    }

    #[test]
    fn closing_the_window_without_a_decision_presents_the_next_record() {
        let queue = vec![
            entry("chrome", "a", 1, false),
            entry("chrome", "b", 2, false),
        ];
        let mut shown = Presentation {
            open: None,
            skip: BTreeSet::new(),
            visible: false,
        };
        let first = next_live(&queue, &shown.skip).unwrap();
        shown.present(first);
        assert_eq!(shown.open.as_ref().map(|(_, tx)| tx.as_str()), Some("a"));
        assert!(shown.visible);

        shown.close_window_without_decision(&queue);

        assert_eq!(shown.open.as_ref().map(|(_, tx)| tx.as_str()), Some("b"));
        assert!(
            shown.visible,
            "window close hid the successor while open stayed set"
        );
        assert!(shown.skip.contains(&("chrome".into(), "a".into())));
        assert!(!shown.skip.contains(&("chrome".into(), "b".into())));
    }

    /// A new window: indexes already equal English, `Lang.english` still false.
    struct FreshDialog {
        applied: bool,
        english: bool,
    }

    impl FreshDialog {
        fn show_matching_english(&mut self) {
            let differs = false;
            if appearance_pending(self.applied, differs) {
                self.applied = true;
                self.english = true;
            }
        }
    }

    #[test]
    fn first_show_applies_english_labels_when_indexes_match_the_window_defaults() {
        let mut dialog = FreshDialog {
            applied: false,
            english: false,
        };
        dialog.show_matching_english();
        assert!(
            dialog.english,
            "first English show left static labels in Vietnamese"
        );
        assert!(dialog.applied);
        assert!(!appearance_pending(dialog.applied, false));
        assert!(appearance_pending(dialog.applied, true));
    }

    #[test]
    fn queue_orders_and_dedupes_by_caller_and_tx() {
        let rows = queue_from_discovery(&[
            entry("firefox", "b", 30, false),
            entry("chrome", "b", 10, false),
            entry("chrome", "a", 20, false),
            entry("chrome", "b", 10, true),
            entry("chrome", "a", 5, false),
        ]);
        let keys: Vec<_> = rows
            .iter()
            .map(|row| {
                (
                    row.caller.as_str(),
                    row.tx.as_str(),
                    row.created_at_ms,
                    row.expired,
                )
            })
            .collect();
        assert_eq!(
            keys,
            vec![
                ("chrome", "a", 5, false),
                ("chrome", "b", 10, false),
                ("firefox", "b", 30, false),
            ]
        );
        let skip = BTreeSet::from([("chrome".into(), "a".into())]);
        let next = next_live(&rows, &skip).unwrap();
        assert_eq!((next.caller.as_str(), next.tx.as_str()), ("chrome", "b"));
        let expired = vec![
            entry("chrome", "old", 1, true),
            entry("chrome", "new", 2, false),
        ];
        let queued = queue_from_discovery(&expired);
        assert!(next_live(&queued, &BTreeSet::new()).unwrap().tx.eq("new"));
    }

    #[test]
    fn reconcile_locks_expired_and_closes_when_the_row_is_gone() {
        let queue = vec![
            entry("chrome", "live", 1, false),
            entry("chrome", "old", 2, true),
        ];
        assert_eq!(
            reconcile_front("chrome", "live", &queue),
            FrontUpdate::StillLive
        );
        assert_eq!(
            reconcile_front("chrome", "old", &queue),
            FrontUpdate::LockExpired
        );
        assert_eq!(
            reconcile_front("chrome", "gone", &queue),
            FrontUpdate::ClosedElsewhere
        );
    }

    #[test]
    fn query_must_still_be_prepared_before_a_transition() {
        assert_eq!(
            transition_gate(HandoffStatus::Prepared),
            TransitionGate::Proceed
        );
        assert_eq!(
            transition_gate(HandoffStatus::Expired),
            TransitionGate::LockExpired
        );
        for status in [
            HandoffStatus::Committed,
            HandoffStatus::Discarded,
            HandoffStatus::RecoveryRequired,
            HandoffStatus::Conflict,
        ] {
            assert_eq!(transition_gate(status), TransitionGate::AlreadyHandled);
        }
        assert!(commit_accepted(HandoffStatus::Committed, true));
        assert!(commit_accepted(HandoffStatus::RecoveryRequired, true));
        assert!(!commit_accepted(HandoffStatus::RecoveryRequired, false));
        assert!(!commit_accepted(HandoffStatus::Expired, false));
    }

    #[test]
    fn expiry_locks_submit_at_the_deadline() {
        assert!(!submit_locked(1_000, Some(2_000)));
        assert!(submit_locked(2_000, Some(2_000)));
        assert!(submit_locked(2_001, Some(2_000)));
        assert!(!submit_locked(9_000, None));
        assert_eq!(countdown_seconds(1_000, Some(65_000)), 64);
        assert_eq!(countdown_seconds(65_000, Some(65_000)), 0);
        assert_eq!(format_countdown(64), "1:04");
    }

    #[test]
    fn decision_covers_category_and_manual_override() {
        let rules = default_categories();
        let root = Path::new(r"C:\Downloads");
        let mut on = DialogDraft::initial(true, "song.mp3", &rules);
        let download = on.decision(root, &rules, false);
        assert_eq!(download.dest_dir, display_path(&root.join("Music")));
        assert_eq!(download.filename, "song.mp3");
        assert!(!download.start_paused);

        on.note_dir_edit(r"D:\Manual");
        let overridden = on.decision(root, &rules, false);
        assert_eq!(overridden.dest_dir, r"D:\Manual");

        let mut off = DialogDraft::initial(false, "song.mp3", &rules);
        let root_only = off.decision(root, &rules, true);
        assert_eq!(root_only.dest_dir, display_path(root));
        assert!(root_only.start_paused);
        assert_ne!(root_only.dest_dir, display_path(&root.join("Music")));

        off.note_dir_edit(r"D:\Manual");
        let off_override = off.decision(root, &rules, true);
        assert_eq!(off_override.dest_dir, r"D:\Manual");
    }

    #[test]
    fn filename_edits_recompute_until_a_pick_or_override_sticks() {
        let rules = default_categories();
        let root = Path::new(r"C:\Downloads");
        let mut draft = DialogDraft::initial(true, "song.mp3", &rules);
        assert_eq!(draft.category_id.as_deref(), Some("music"));

        draft.note_filename_edit("photo.png", &rules);
        assert_eq!(draft.category_id.as_deref(), Some("images"));
        assert_eq!(
            draft.preview(root, &rules).dest_dir,
            display_path(&root.join("Images"))
        );

        draft.note_category_pick(Some("videos".into()));
        draft.note_filename_edit("notes.pdf", &rules);
        assert_eq!(draft.category_id.as_deref(), Some("videos"));
        assert_eq!(
            draft.preview(root, &rules).dest_dir,
            display_path(&root.join("Videos"))
        );

        draft.note_dir_edit(r"D:\keep");
        draft.note_filename_edit("clip.mkv", &rules);
        assert_eq!(draft.category_id.as_deref(), Some("videos"));
        assert_eq!(draft.preview(root, &rules).dest_dir, r"D:\keep");

        let mut inferred = DialogDraft::initial(true, "song.mp3", &rules);
        inferred.note_dir_edit(r"D:\keep");
        inferred.note_filename_edit("photo.png", &rules);
        assert_eq!(inferred.category_id.as_deref(), Some("music"));
        assert_eq!(inferred.manual_dir, r"D:\keep");

        assert!(!inferred.note_probe_filename("from-server.bin", &rules));
        assert!(inferred.filename_edited);
        inferred.filename_edited = false;
        assert!(inferred.note_probe_filename("from-server.bin", &rules));
        assert_eq!(inferred.filename_raw, "from-server.bin");
        assert_eq!(inferred.manual_dir, r"D:\keep");
        assert_eq!(inferred.category_id.as_deref(), Some("music"));

        let mut fresh = DialogDraft::initial(true, "song.mp3", &rules);
        assert!(fresh.note_probe_filename("clip.mkv", &rules));
        assert_eq!(fresh.category_id.as_deref(), Some("videos"));

        let mut sticky = DialogDraft::initial(true, "song.mp3", &rules);
        sticky.note_category_pick(Some("archives".into()));
        sticky.refresh_rules(&rules);
        assert_eq!(sticky.category_id.as_deref(), Some("archives"));

        let mut open = DialogDraft::initial(true, "song.mp3", &rules);
        let only_images = vec![CategoryRule {
            id: "images".into(),
            folder: "Images".into(),
            suffixes: vec![".mp3".into(), ".png".into()],
        }];
        open.refresh_rules(&only_images);
        assert_eq!(open.category_id.as_deref(), Some("images"));

        let dirty = DialogDraft::initial(true, "a<b>.mp3", &rules);
        let preview = dirty.preview(root, &rules);
        assert_eq!(preview.filename, "a_b_.mp3");
        assert!(preview.filename_differs);
    }

    #[test]
    fn quick_add_reports_a_suffix_conflict_and_keeps_the_normalized_set() {
        let rules = default_categories();
        let conflict = quick_add_rules(&rules, "Extra", "pdf").unwrap_err();
        assert!(conflict.contains(".pdf"), "{conflict}");
        assert!(quick_add_rules(&rules, "Documents", "epub")
            .unwrap_err()
            .contains("duplicated"));
        let added = quick_add_rules(&rules, "Ebooks", "cbz, .azw3").unwrap();
        assert_eq!(rules.len(), default_categories().len());
        let ebooks = added.iter().find(|rule| rule.id == "ebooks").unwrap();
        assert_eq!(ebooks.folder, "Ebooks");
        assert_eq!(
            ebooks.suffixes,
            vec![".cbz".to_string(), ".azw3".to_string()]
        );
        assert_eq!(
            added.iter().position(|rule| rule.id == "ebooks"),
            Some(added.len() - 2)
        );
        assert_eq!(added.last().unwrap().id, "other");
    }

    /// Dialog fields quick-add mutates. A rejected save leaves the rules
    /// and the open editor alone and shows `error`.
    struct QuickAddSession {
        rules: Vec<CategoryRule>,
        error: String,
        open: bool,
        use_category: bool,
        category_id: Option<String>,
    }

    impl QuickAddSession {
        fn apply(&mut self, applied: QuickAddApplied) {
            match applied {
                QuickAddApplied::Rejected { error } => {
                    self.error = error;
                }
                QuickAddApplied::Stored { rules, added_id } => {
                    self.category_id = Some(added_id);
                    self.rules = rules;
                    self.error.clear();
                    self.use_category = true;
                    self.open = false;
                }
            }
        }
    }

    struct EngineDir {
        root: std::path::PathBuf,
    }

    impl Drop for EngineDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn idle_engine() -> (EngineDir, usagi::Engine) {
        let root = std::env::temp_dir().join(format!(
            "udm-quick-add-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join("downloads")).unwrap();
        let engine = usagi::EngineBuilder::new()
            .paths(usagi::Paths {
                root: root.clone(),
                db_file: root.join("test.redb"),
                log_file: root.join("logs").join("test.log"),
                temp_dir: root.join("tmp"),
                default_download_dir: root.join("downloads"),
                portable: true,
                exe_dir: Some(root.clone()),
            })
            .idle(true)
            .worker_threads(2)
            .build()
            .expect("idle engine");
        (EngineDir { root }, engine)
    }

    #[test]
    fn quick_add_shows_a_save_failure_and_adds_no_rule() {
        let (_dir, engine) = idle_engine();
        let existing = engine.settings().category_rules();
        let mut session = QuickAddSession {
            rules: existing.clone(),
            error: String::new(),
            open: true,
            use_category: false,
            category_id: None,
        };

        engine.store().fail_next_settings_saves(1);
        session.apply(commit_quick_add(
            &session.rules,
            "Ebooks",
            "cbz",
            |normalized| {
                let mut settings = engine.settings();
                settings.categories = Some(normalized.to_vec());
                engine
                    .save_settings(settings)
                    .map_err(|error| error.to_string())
            },
        ));
        assert!(
            session.error.contains("injected settings save failure"),
            "{}",
            session.error
        );
        assert!(session.open);
        assert!(session.category_id.is_none());
        assert!(session.rules.iter().all(|rule| rule.id != "ebooks"));
        assert_eq!(session.rules, existing);
        assert!(
            engine
                .settings()
                .category_rules()
                .iter()
                .all(|rule| rule.id != "ebooks"),
            "failed write published the rule"
        );

        let expected = quick_add_rules(&existing, "Ebooks", "cbz, .azw3").unwrap();
        session.apply(commit_quick_add(
            &session.rules,
            "Ebooks",
            "cbz, .azw3",
            |normalized| {
                let mut settings = engine.settings();
                settings.categories = Some(normalized.to_vec());
                engine
                    .save_settings(settings)
                    .map_err(|error| error.to_string())
            },
        ));
        assert!(session.error.is_empty());
        assert!(!session.open);
        assert!(session.use_category);
        assert_eq!(session.category_id.as_deref(), Some("ebooks"));
        assert_eq!(session.rules, expected);
        assert_eq!(engine.settings().category_rules(), expected);

        session.open = true;
        let persisted = session.rules.clone();
        session.apply(commit_quick_add(&session.rules, "Extra", "pdf", |_| {
            panic!("a validation conflict persisted a write");
        }));
        assert!(session.error.contains(".pdf"), "{}", session.error);
        assert_eq!(session.rules, persisted);
        assert!(session.open);
        assert_eq!(session.category_id.as_deref(), Some("ebooks"));
        assert_eq!(engine.settings().category_rules(), expected);
    }

    #[test]
    fn recent_dirs_are_newest_first_deduped_and_capped() {
        let mut dirs = Vec::new();
        dirs = push_recent_dir(&dirs, r"C:\a");
        dirs = push_recent_dir(&dirs, r"C:\b");
        assert_eq!(dirs, vec![r"C:\b".to_string(), r"C:\a".to_string()]);
        dirs = push_recent_dir(&dirs, r"C:\a");
        assert_eq!(dirs, vec![r"C:\a".to_string(), r"C:\b".to_string()]);
        dirs = push_recent_dir(&dirs, "  ");
        assert_eq!(dirs.len(), 2);
        for index in 0..10 {
            dirs = push_recent_dir(&dirs, &format!(r"C:\d{index}"));
        }
        assert_eq!(dirs.len(), RECENT_DIR_CAP);
        assert_eq!(dirs[0], r"C:\d9");
        assert!(!dirs.iter().any(|dir| dir == r"C:\a"));
    }

    #[test]
    fn size_line_names_browser_probe_or_unknown() {
        assert_eq!(
            size_view(Some(10), None).provenance,
            SizeProvenance::FromBrowser
        );
        assert_eq!(
            size_view(Some(10), Some(Some(10))).provenance,
            SizeProvenance::Probed
        );
        let differs = size_view(Some(10), Some(Some(12)));
        assert_eq!(differs.provenance, SizeProvenance::ProbedDiffers);
        assert_eq!(differs.bytes, Some(12));
        assert_eq!(size_view(None, None).provenance, SizeProvenance::Unknown);
        assert_eq!(
            size_view(Some(10), Some(None)).provenance,
            SizeProvenance::FromBrowser
        );
    }

    fn snap(left: i32, top: i32, right: i32, bottom: i32, scale: f64) -> MonitorSnap {
        MonitorSnap {
            work: PxRect {
                left,
                top,
                right,
                bottom,
            },
            scale,
        }
    }

    #[test]
    fn handoff_window_declares_the_resizable_u1_geometry() {
        let source = include_str!("../ui/handoff.slint");
        for line in [
            "preferred-width: 560px;",
            "preferred-height: 460px;",
            "min-width: 460px;",
            "min-height: 360px;",
            "max-width: 100000px;",
            "max-height: 100000px;",
        ] {
            assert!(source.contains(line), "missing {line}");
        }
        // min < max is the resizable flag winit's window_is_resizable reads.
        assert!(
            source.contains("Resizable:"),
            "window definition must record why max above min is the resizable flag"
        );
        let scroll = source.find("ScrollView").expect("scrolling body");
        let download = source.find("root.download()").expect("download button");
        assert!(
            scroll < download,
            "the action row must sit outside the scrolling body"
        );
        assert!(
            !source.contains("HorizontalLayout { vertical-stretch: 1; }"),
            "the empty stretch that pinned the buttons to the bottom is back"
        );
        assert_eq!(HANDOFF_DEFAULT_LOGICAL_W, 560.0);
        assert_eq!(HANDOFF_DEFAULT_LOGICAL_H, 460.0);
        assert_eq!(HANDOFF_MIN_LOGICAL_W, 460.0);
        assert_eq!(HANDOFF_MIN_LOGICAL_H, 360.0);
    }

    #[test]
    fn presentation_monitor_skips_a_hidden_main_window() {
        let foreground = snap(0, 0, 1920, 1040, 1.0);
        let cursor = snap(-1920, 0, 0, 1080, 1.25);
        let primary = snap(0, 0, 1280, 720, 1.0);
        assert_eq!(
            choose_monitor(Some(foreground), Some(cursor), primary).0,
            MonitorSource::Foreground
        );
        let (source, chosen) = choose_monitor(None, Some(cursor), primary);
        assert_eq!(source, MonitorSource::Cursor);
        assert_eq!(chosen.work.left, -1920);
        let (source, chosen) = choose_monitor(None, None, primary);
        assert_eq!(source, MonitorSource::Primary);
        assert_eq!(chosen.work.right, 1280);
    }

    #[test]
    fn place_outer_centers_and_clamps_to_the_work_area() {
        let work = PxRect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let placed = place_outer(
            work,
            PxSize {
                width: 560,
                height: 460,
            },
            PxSize {
                width: 460,
                height: 360,
            },
        );
        assert_eq!((placed.x, placed.y), (680, 290));
        assert_eq!((placed.outer_width, placed.outer_height), (560, 460));

        let left = place_outer(
            PxRect {
                left: -1920,
                top: 0,
                right: 0,
                bottom: 1080,
            },
            PxSize {
                width: 800,
                height: 600,
            },
            PxSize {
                width: 400,
                height: 300,
            },
        );
        assert_eq!((left.x, left.y), (-1360, 240));

        let fitted = place_outer(
            PxRect {
                left: 10,
                top: 20,
                right: 710,
                bottom: 520,
            },
            PxSize {
                width: 800,
                height: 600,
            },
            PxSize {
                width: 400,
                height: 300,
            },
        );
        assert_eq!((fitted.x, fitted.y), (10, 20));
        assert_eq!((fitted.outer_width, fitted.outer_height), (700, 500));

        let too_small = place_outer(
            PxRect {
                left: 5,
                top: 6,
                right: 200,
                bottom: 180,
            },
            PxSize {
                width: 800,
                height: 600,
            },
            PxSize {
                width: 500,
                height: 400,
            },
        );
        assert_eq!((too_small.x, too_small.y), (5, 6));
        assert_eq!((too_small.outer_width, too_small.outer_height), (500, 400));
    }

    #[test]
    fn logical_size_converts_once_per_target_scale() {
        assert_eq!(logical_to_physical(560.0, 1.0), 560);
        assert_eq!(logical_to_physical(560.0, 1.5), 840);
        assert_eq!(logical_to_physical(460.0, 1.25), 575);
        assert_eq!(logical_to_physical(560.0, 0.0), 560);
        assert_eq!(logical_to_physical(560.0, f64::NAN), 560);
        assert!(!native_size_settled(0, 460, 16, 499));
        assert!(native_size_settled(560, 460, 576, 499));

        let target = snap(0, 0, 1920, 1080, 1.0);
        let placed = placement_for(
            &target,
            NativeSize {
                inner_width: 560,
                inner_height: 460,
                outer_width: 576,
                outer_height: 499,
            },
        );
        assert_eq!(placed.inner_width, 560);
        assert_eq!(placed.inner_height, 460);
        assert_eq!(placed.x, (1920 - 576) / 2);
        assert_eq!(placed.y, (1080 - 499) / 2);

        let hidpi = snap(0, 0, 2560, 1440, 2.0);
        let placed = placement_for(
            &hidpi,
            NativeSize {
                inner_width: 800,
                inner_height: 600,
                outer_width: 816,
                outer_height: 639,
            },
        );
        assert_eq!(placed.inner_width, 1120);
        assert_eq!(placed.inner_height, 920);
        let outer_w = 1120 + 16;
        let outer_h = 920 + 39;
        assert_eq!(placed.x, (2560 - outer_w) / 2);
        assert_eq!(placed.y, (1440 - outer_h) / 2);
    }

    #[test]
    fn first_presentation_focuses_once_and_refresh_does_not() {
        let mut gate = FirstShowGate::default();
        let mut focused = 0;
        assert_eq!(tick_action(false), TickAction::OpenNext);
        if decide_first_show(&mut gate, "chrome", "tx-1", true) == FirstShow::Now {
            focused += 1;
        }
        assert_eq!(focused, 1);
        for _ in 0..4 {
            assert_eq!(tick_action(true), TickAction::Refresh);
            assert_eq!(decide_refresh(&gate), FirstShow::NotNow);
            assert_eq!(
                decide_first_show(&mut gate, "chrome", "tx-1", true),
                FirstShow::NotNow
            );
        }
        assert_eq!(focused, 1);

        assert_eq!(
            decide_first_show(&mut gate, "firefox", "tx-2", false),
            FirstShow::Later
        );
        assert_eq!(
            decide_first_show(&mut gate, "firefox", "tx-2", false),
            FirstShow::NotNow,
            "a second poll must not arm another focus"
        );
        if decide_continuation(&mut gate, "firefox", "tx-2") == FirstShow::Now {
            focused += 1;
        }
        assert_eq!(
            decide_continuation(&mut gate, "firefox", "tx-2"),
            FirstShow::NotNow
        );
        assert_eq!(decide_refresh(&gate), FirstShow::NotNow);
        assert_eq!(focused, 2);

        assert_eq!(
            decide_first_show(&mut gate, "chrome", "tx-3", true),
            FirstShow::Now,
            "a successor head gets its own presentation"
        );
    }

    #[test]
    fn a_superseded_continuation_does_not_focus() {
        let mut gate = FirstShowGate::default();
        assert_eq!(
            decide_first_show(&mut gate, "chrome", "old", false),
            FirstShow::Later
        );
        assert_eq!(
            decide_first_show(&mut gate, "chrome", "new", true),
            FirstShow::Now
        );
        assert_eq!(
            decide_continuation(&mut gate, "chrome", "old"),
            FirstShow::NotNow
        );
        disarm_continuation(&mut gate, "chrome", "old");
        assert_eq!(
            decide_continuation(&mut gate, "chrome", "new"),
            FirstShow::NotNow
        );
    }

    const CALLER: &str = "chrome:test-ext";

    fn handoff_engine(name: &str) -> (std::path::PathBuf, std::sync::Arc<usagi::Engine>) {
        let root = std::env::temp_dir().join(format!(
            "udm-s6-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("downloads")).unwrap();
        std::fs::create_dir_all(root.join("handoff")).unwrap();
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        std::fs::write(
            root.join("handoff").join("callers.json"),
            format!(r#"{{"callers":["{CALLER}"]}}"#),
        )
        .unwrap();
        let paths = usagi::Paths {
            root: root.clone(),
            db_file: root.join("test.redb"),
            log_file: root.join("logs").join("test.log"),
            temp_dir: root.join("tmp"),
            default_download_dir: root.join("downloads"),
            portable: true,
            exe_dir: Some(root.clone()),
        };
        let engine = usagi::EngineBuilder::new()
            .paths(paths)
            .idle(true)
            .build()
            .unwrap();
        let mut settings = engine.settings();
        settings.connect_timeout_secs = 3;
        engine.update_settings(settings).unwrap();
        (root, std::sync::Arc::new(engine))
    }

    fn stored_record(
        tx: &str,
        state: HandoffState,
        at: i64,
        expires: i64,
        url: &str,
        size: u64,
    ) -> usagi::handoff::HandoffRecord {
        let mut intent = intent(url, None, Some(size));
        intent.validate_and_normalize().unwrap();
        usagi::handoff::HandoffRecord {
            tx: tx.to_string(),
            caller: CALLER.to_string(),
            fingerprint: Some(intent.fingerprint()),
            intent: Some(intent),
            state,
            created_at_ms: at,
            expires_at_ms: Some(expires),
            transition_at_ms: at,
            disposition: usagi::handoff::BrowserDisposition::CancelledForHandoff,
            engine_task_id: None,
            association_pending: false,
            outcome: if state == HandoffState::Expired {
                Some("lease expired before commit".to_string())
            } else {
                None
            },
            decision: None,
            materialize_error: None,
        }
    }

    fn open_journal(
        engine: &std::sync::Arc<usagi::Engine>,
        records: Vec<usagi::handoff::HandoffRecord>,
    ) -> std::sync::Arc<usagi::handoff::Coordinator> {
        std::sync::Arc::new(
            usagi::handoff::Coordinator::open(
                Box::new(usagi::handoff::MemoryStore::with_records(records)),
                std::sync::Arc::new(usagi::handoff::EngineAdapter::new(engine.clone())),
                std::sync::Arc::new(usagi::handoff::SystemClock),
                usagi::handoff::CoordinatorConfig {
                    lease: std::time::Duration::from_secs(600),
                    allowed_callers: vec![CALLER.to_string()],
                    require_decision: true,
                },
            )
            .unwrap(),
        )
    }

    fn discover_and_tick(g: &Rc<Gui>) {
        reload_discovery(g);
        tick(g);
    }

    fn open_tx(g: &Gui) -> Option<String> {
        g.handoff_ui
            .borrow()
            .open
            .as_ref()
            .map(|row| row.tx.clone())
    }

    /// The dialog callbacks own an `Rc<Gui>`, and `Gui` owns the window.
    /// Drop the window before the test `Rc` so the cycle does not keep a
    /// component alive into the next test.
    struct ReleaseDialog<'a>(&'a Gui);

    impl Drop for ReleaseDialog<'_> {
        fn drop(&mut self) {
            self.0.handoff_ui.borrow_mut().window = None;
            crate::state::test_platform::set_event_loop_proxy(true);
        }
    }

    /// Prepared rows already in the journal, including two song.mp3 rows
    /// that share a fingerprint and one prepared row that is already past
    /// its deadline. Opening the coordinator expires the deadline. The
    /// first presentable head is the still-live song, not the old row.
    #[test]
    fn boot_with_prepared_rows_presents_the_queue_head() {
        crate::state::test_platform::install();
        let (root, engine) = handoff_engine("boot");
        let now = usagi::util::now_millis();
        let records = vec![
            stored_record(
                "song-old",
                HandoffState::Expired,
                now - 100_000,
                now - 90_000,
                "http://127.0.0.1:1/song.mp3",
                3_000_011,
            ),
            stored_record(
                "song-due",
                HandoffState::Prepared,
                now - 50_000,
                now - 1_000,
                "http://127.0.0.1:1/song-due.mp3",
                3_000_011,
            ),
            stored_record(
                "song-live",
                HandoffState::Prepared,
                now - 20_000,
                now + 600_000,
                "http://127.0.0.1:1/song.mp3",
                3_000_011,
            ),
            stored_record(
                "movie-live",
                HandoffState::Prepared,
                now - 10_000,
                now + 600_000,
                "http://127.0.0.1:1/movie.mp4",
                4_000_011,
            ),
        ];
        assert_eq!(
            records[0].fingerprint, records[2].fingerprint,
            "the expired and prepared song rows share a fingerprint"
        );
        assert_ne!(records[2].fingerprint, records[3].fingerprint);
        let coordinator = open_journal(&engine, records);
        assert_eq!(
            coordinator.query(CALLER, "song-due").unwrap().status,
            HandoffStatus::Expired,
            "a prepared row past its deadline expires when the coordinator opens"
        );
        let discovered = coordinator.discovery(CALLER).unwrap();
        assert!(
            discovered.iter().all(|entry| entry.tx != "song-old"),
            "a row that was already expired in the journal is not rediscovered"
        );
        let g = crate::state::gui_for_test(engine, Some(coordinator.clone()));
        let _release = ReleaseDialog(&g);
        crate::state::test_platform::set_event_loop_proxy(false);
        discover_and_tick(&g);
        assert!(
            open_tx(&g).is_none(),
            "when the presentation cannot be scheduled the head stays unopened"
        );
        crate::state::test_platform::set_event_loop_proxy(true);
        discover_and_tick(&g);
        assert_eq!(open_tx(&g).as_deref(), Some("song-live"));
        assert!(g.handoff_ui.borrow().window.is_some());
        let gate = {
            let ui = g.handoff_ui.borrow();
            (ui.first_show.done.clone(), ui.first_show.armed.clone())
        };
        tick(&g);
        let gate_after = {
            let ui = g.handoff_ui.borrow();
            (ui.first_show.done.clone(), ui.first_show.armed.clone())
        };
        assert_eq!(gate, gate_after, "a refresh does not present the head again");
        assert_eq!(open_tx(&g).as_deref(), Some("song-live"));
        submit(&g, true);
        assert_eq!(
            coordinator.query(CALLER, "song-live").unwrap().status,
            HandoffStatus::Committed
        );
        assert_eq!(open_tx(&g).as_deref(), Some("movie-live"));
        assert!(request_close(&g));
        assert_eq!(
            coordinator.query(CALLER, "movie-live").unwrap().status,
            HandoffStatus::Discarded
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// The app is already up and the queue is empty. A prepare that
    /// lands in the store is presented on the next discovery pass.
    #[test]
    fn live_prepare_presents_the_dialog() {
        crate::state::test_platform::install();
        crate::state::test_platform::set_event_loop_proxy(true);
        let (root, engine) = handoff_engine("live");
        let coordinator = open_journal(&engine, Vec::new());
        let g = crate::state::gui_for_test(engine, Some(coordinator.clone()));
        let _release = ReleaseDialog(&g);
        discover_and_tick(&g);
        assert!(open_tx(&g).is_none());
        assert!(g.handoff_ui.borrow().window.is_none());
        let mut intent = intent("http://127.0.0.1:1/report.pdf", None, Some(2_000_013));
        intent.validate_and_normalize().unwrap();
        let reply = coordinator
            .prepare(CALLER, "tx-live", intent, true)
            .unwrap();
        assert_eq!(reply.status, HandoffStatus::Prepared);
        discover_and_tick(&g);
        assert_eq!(open_tx(&g).as_deref(), Some("tx-live"));
        assert!(g.handoff_ui.borrow().window.is_some());
        tick(&g);
        assert_eq!(open_tx(&g).as_deref(), Some("tx-live"));
        let _ = std::fs::remove_dir_all(root);
    }
}

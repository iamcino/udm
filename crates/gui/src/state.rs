//! Wires the engine to the Slint UI.
//!
//! The UI thread never blocks: it polls the engine on a 150 ms timer (the
//! engine's own snapshot API is lock-free and cheap) and only shows native
//! dialogs (folder pickers) modally, which is what users expect on Windows.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use usagi::category::{classify, CategoryRule};
use usagi::config::{ConflictPolicy, Settings, TransportPref};
use usagi::engine::{AddRequest, Engine, Event, TaskSnapshot};
use usagi::handoff::{Coordinator, DiscoveryEntry, RecoveryEntry};
use usagi::hash::ExpectedHash;
use usagi::store::records::TaskState;
use usagi::updater::{CheckKind, InstallLayout};
use usagi::util;

use crate::task_actions::{caps_for_state, StripCaps};
use crate::{AppWindow, CategoryNav, EndpointRow, StatusInfo, TaskDetail, TaskRow};

const REFRESH_MS: u64 = 150;
const SLOW_REFRESH_MS: u64 = 2000;
/// Expired-handoff discovery is read on this cadence, not on every 150 ms tick.
const NOTICE_POLL: Duration = Duration::from_secs(1);

/// Sidebar status axis. Finished is the existing completed value.
const LIBRARY_ALL: i32 = 0;
const LIBRARY_ACTIVE: i32 = 1;
const LIBRARY_FINISHED: i32 = 2;
const LIBRARY_FAILED: i32 = 3;
/// `state != Completed`, including Failed. Not the Active predicate.
const LIBRARY_UNFINISHED: i32 = 4;

/// Virtual bucket for an empty filename or a `classify` result of `None`.
const OTHER_CATEGORY: &str = "other";

const EXPIRED_HANDOFF_TEXT: &str =
    "A browser handoff expired before it was confirmed. The file was not downloaded.";

/// Row-key separator. Transaction ids cannot contain control characters.
const NOTICE_SEP: char = '\u{1e}';

/// One snapshot reduced to the fields the list filter reads.
struct ViewTask<'a> {
    filename: &'a str,
    url: &'a str,
    state: TaskState,
}

/// Global badge totals from one snapshot walk. Search does not narrow them.
struct CategoryCounts {
    by_id: BTreeMap<String, i32>,
    total: i32,
    finished: i32,
    unfinished: i32,
}

/// Classify cache: filename plus a fingerprint of the effective rules.
struct CategoryCache {
    fingerprint: u64,
    by_filename: HashMap<String, String>,
    /// Classifications performed on a cache miss. Tests assert the one-pass bound.
    #[allow(dead_code)]
    misses: u64,
}

impl CategoryCache {
    fn new() -> Self {
        CategoryCache {
            // Not a real fingerprint, so the first ruleset always installs itself.
            fingerprint: u64::MAX,
            by_filename: HashMap::new(),
            misses: 0,
        }
    }
}

/// One expired handoff the All view may show. Not an engine task.
#[derive(Clone)]
struct ExpiredNotice {
    caller: String,
    tx: String,
    url: String,
    filename: String,
    size_hint: Option<u64>,
    at_ms: i64,
}

/// Everything the event loop needs.
pub struct Gui {
    pub engine: Arc<Engine>,
    pub tasks: Rc<VecModel<TaskRow>>,
    /// GUI session only. Never persisted to engine/redb.
    pub selected: RefCell<BTreeSet<u64>>,
    /// Last plain/Ctrl-clicked task, for Shift range selection.
    pub anchor: RefCell<Option<u64>>,
    /// Focused task for the bottom Task Detail panel (Commit 2).
    /// Separate from `selected`: changing the multi-selection never
    /// changes the focus, and refresh never moves the focus.
    pub focused: RefCell<Option<u64>>,
    /// System tray handle (Phase 2). `None` when tray creation failed or
    /// on platforms without a tray implementation yet.
    pub tray: RefCell<Option<crate::tray::Tray>>,
    pub endpoints: Rc<VecModel<EndpointRow>>,
    pub ui: Weak<AppWindow>,
    slow: RefCell<Instant>,
    toast_until: RefCell<Instant>,
    /// G2: the Add form's destination *intent*, tracked separately from the
    /// displayed string (contract section 2). `true` only when the user
    /// actually typed or browsed a folder; an untouched field or one the
    /// form pre-filled stays `false` and submits as `dest_dir: None`.
    pub add_dir_explicit: RefCell<bool>,
    /// G3 tray summary throttle: when the summary was last pushed to the
    /// tray, and the content key that was pushed (identical content is
    /// not re-sent).
    pub tray_summary_at: RefCell<Instant>,
    pub tray_summary_key: RefCell<String>,
    /// G3 minimise-to-tray watcher: true once the current minimised
    /// episode has been converted into a tray hide, so the poll does not
    /// repeat it until the window is restored and minimised again.
    pub minimized_seen: RefCell<bool>,
    /// Restore-repaint guard: the previous poll tick's minimised state,
    /// and how many full-repaint pulses are still owed after the window
    /// becomes visible again (see repaint_guard).
    pub was_minimized: RefCell<bool>,
    pub repaint_pulses: RefCell<u8>,
    /// G3 popup: the lazily created downloads popup window and whether it
    /// is currently open. Refreshed only while open, from tray_poll.
    pub popup: RefCell<Option<crate::PopupWindow>>,
    pub popup_open: RefCell<bool>,
    /// Work area (physical px) of the monitor the popup was opened
    /// from, captured at open time; placement clamps inside it so the
    /// popup can never straddle two monitors.
    pub popup_area: RefCell<Option<(i32, i32, i32, i32)>>,
    /// G5-closing: the live handoff coordinator, when one is serving.
    /// Recovery rows stay read-only. The handoff dialog commits and
    /// discards through this same instance, and it does not hold the
    /// coordinator lock while the user or a probe is waiting.
    pub handoff: RefCell<Option<Arc<Coordinator>>>,
    /// Dialog queue and the independent confirmation window.
    pub handoff_ui: RefCell<crate::handoff_dialog::DialogSession>,
    /// Per-download detail windows. Session only: closing one does not
    /// cancel the task, and a restart does not reopen them.
    pub detail: RefCell<crate::detail_window::DetailSession>,
    /// Browser integration (items 1-3): the data root the browser
    /// rows read (registry keys, journal, manifests); the browsers
    /// the startup repair could not repoint (their rows show the
    /// registration-broken fault); and the poll throttle for the
    /// row refresh.
    pub paths: usagi::Paths,
    pub browser_broken: RefCell<Vec<String>>,
    browser_refresh_at: RefCell<Instant>,
    /// Wiring W2: the install layout the updater surface acts on (None
    /// when the executable's directory is unknown), the single-flight
    /// guard for the background check thread, and the relaunch request
    /// the Apply flow raises before quitting the event loop.
    pub update_layout: Option<InstallLayout>,
    pub update_check_running: Arc<AtomicBool>,
    pub relaunch_after_apply: Arc<AtomicBool>,
    /// Autocheck window: the single 6-hour clock and the one-shot startup
    /// flag, plus the poll bookkeeping that recognises the first ready
    /// evaluation and the OFF->ON flip (see `UpdateSchedule`).
    pub update_schedule: RefCell<UpdateSchedule>,
    /// The previous poll's single-flight guard value, so the poll can spot
    /// a check completion (running true->false) and recompute the tray
    /// badge. The completion closure cannot touch the tray itself: the GUI
    /// handle is not `Send`.
    pub was_check_running: RefCell<bool>,
    /// The version a check found newer than this build this session,
    /// shared with the check/apply worker threads (the GUI handle
    /// itself is not `Send`). `None` until a check reports one.
    pub update_offered: Arc<std::sync::Mutex<Option<String>>>,
    /// Single-flight guard for the download+install worker.
    pub update_apply_running: Arc<AtomicBool>,
    /// CATEGORIES sidebar rows: All, each effective rule, Finished, Unfinished.
    pub category_nav: Rc<VecModel<CategoryNav>>,
    /// Filename → category id. Cleared when the effective rules change.
    category_cache: RefCell<CategoryCache>,
    /// Session dismissals of expired-handoff notices, keyed by (caller, tx).
    /// In memory only: a restart shows the notice again.
    notice_dismissed: RefCell<BTreeSet<(String, String)>>,
    notice_cache: RefCell<Vec<ExpiredNotice>>,
    notice_polled_at: RefCell<Instant>,
}

/// The automatic-check window. There is one clock instant (`clock_base`,
/// the start of the current 6-hour window) and one `startup_pending` flag;
/// the due decision is the pure [`update_check_due`] function. `armed` and
/// `last_on` are not a second clock: they let [`UpdateSchedule::observe`]
/// recognise the two events that rewrite `clock_base` without a check
/// starting — the first ready evaluation and an OFF->ON flip.
pub struct UpdateSchedule {
    pub clock_base: Instant,
    pub startup_pending: bool,
    armed: bool,
    last_on: bool,
}

/// The periodic check interval: every 6 hours while the app runs.
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Pure due decision. A check is due only when checks resolve ON, nothing
/// is in flight, and either the one-shot startup check is still owed or a
/// full 6-hour window has elapsed since `clock_base`.
pub(crate) fn update_check_due(
    now: Instant,
    clock_base: Instant,
    startup_pending: bool,
    running: bool,
    resolved_on: bool,
) -> bool {
    resolved_on && !running && (startup_pending || now.duration_since(clock_base) >= CHECK_INTERVAL)
}

impl UpdateSchedule {
    fn new(now: Instant) -> Self {
        UpdateSchedule {
            clock_base: now,
            startup_pending: false,
            armed: false,
            last_on: false,
        }
    }

    /// Poll observation. The first ready evaluation arms the clock and sets
    /// the startup flag to the resolved state at that moment. An OFF->ON
    /// flip rewrites the clock to now and leaves the startup flag alone:
    /// enabling starts the 6-hour window, it never fires a check by itself.
    fn observe(&mut self, now: Instant, resolved_on: bool) {
        if !self.armed {
            self.armed = true;
            self.clock_base = now;
            self.startup_pending = resolved_on;
        } else if !self.last_on && resolved_on {
            self.clock_base = now;
        }
        self.last_on = resolved_on;
    }

    /// Any check start (manual, startup or periodic) rewrites the clock to
    /// the start instant and consumes the one-shot startup flag.
    fn note_check_start(&mut self, now: Instant) {
        self.clock_base = now;
        self.startup_pending = false;
    }

    fn due(&self, now: Instant, running: bool, resolved_on: bool) -> bool {
        update_check_due(
            now,
            self.clock_base,
            self.startup_pending,
            running,
            resolved_on,
        )
    }
}

pub fn setup(
    ui: &AppWindow,
    engine: Arc<Engine>,
    handoff: Option<Arc<Coordinator>>,
    start_hidden: bool,
    startup_notice: Option<String>,
    update_layout: Option<InstallLayout>,
    relaunch_after_apply: Arc<AtomicBool>,
) {
    ui.global::<crate::I18n>().on_translate(|key, language| {
        if language == 1 {
            crate::i18n::vi(key.as_str()).into()
        } else {
            key
        }
    });
    let tasks = Rc::new(VecModel::<TaskRow>::default());
    let endpoints = Rc::new(VecModel::<EndpointRow>::default());
    let category_nav = Rc::new(VecModel::<CategoryNav>::default());
    ui.set_tasks(ModelRc::from(tasks.clone()));
    ui.set_endpoints(ModelRc::from(endpoints.clone()));
    ui.set_category_nav(ModelRc::from(category_nav.clone()));
    ui.set_version_text(SharedString::from(env!("CARGO_PKG_VERSION")));
    ui.set_http3_available(engine.net().http3_available());

    let gui = Rc::new(Gui {
        engine: engine.clone(),
        tasks: tasks.clone(),
        selected: RefCell::new(BTreeSet::new()),
        anchor: RefCell::new(None),
        focused: RefCell::new(None),
        tray: RefCell::new(crate::tray::Tray::new(engine.settings().language == "vi")),
        endpoints: endpoints.clone(),
        ui: ui.as_weak(),
        slow: RefCell::new(Instant::now() - Duration::from_millis(SLOW_REFRESH_MS)),
        toast_until: RefCell::new(Instant::now()),
        add_dir_explicit: RefCell::new(false),
        tray_summary_at: RefCell::new(Instant::now() - Duration::from_secs(2)),
        tray_summary_key: RefCell::new(String::new()),
        minimized_seen: RefCell::new(false),
        was_minimized: RefCell::new(false),
        repaint_pulses: RefCell::new(0),
        popup: RefCell::new(None),
        popup_open: RefCell::new(false),
        popup_area: RefCell::new(None),
        handoff: RefCell::new(handoff),
        handoff_ui: RefCell::new(crate::handoff_dialog::DialogSession::default()),
        detail: RefCell::new(crate::detail_window::DetailSession::default()),
        paths: engine.paths().clone(),
        browser_broken: RefCell::new(Vec::new()),
        browser_refresh_at: RefCell::new(Instant::now()),
        update_layout,
        update_check_running: Arc::new(AtomicBool::new(false)),
        relaunch_after_apply,
        update_schedule: RefCell::new(UpdateSchedule::new(Instant::now())),
        was_check_running: RefCell::new(false),
        update_offered: Arc::new(std::sync::Mutex::new(None)),
        update_apply_running: Arc::new(AtomicBool::new(false)),
        category_nav: category_nav.clone(),
        category_cache: RefCell::new(CategoryCache::new()),
        notice_dismissed: RefCell::new(BTreeSet::new()),
        notice_cache: RefCell::new(Vec::new()),
        notice_polled_at: RefCell::new(
            Instant::now()
                .checked_sub(NOTICE_POLL)
                .unwrap_or_else(Instant::now),
        ),
    });

    // Wiring W1: a rollback at startup reconciliation surfaces its
    // notice in the first UI (design section 2.3).
    if let Some(notice) = startup_notice {
        toast(&gui, notice, true);
    }

    push_settings_to_ui(ui, &engine.settings(), gui.update_layout.as_ref());
    refresh(&gui);

    // The update offer is session state in the Yeager model: a check
    // stores nothing and no candidate is held on disk between sessions,
    // so the offer starts empty and the startup automatic check
    // re-finds a newer build within seconds when one is published.
    ui.set_update_available(false);
    refresh_update_badge(&gui);

    // G3 lifecycle: the window close button hides to the tray when the
    // setting is on and a tray exists; Exit in the tray menu always quits
    // for real. Start-with-Windows launches pass --minimized, which (with
    // the start_minimized setting composed into the Run entry by the
    // startup module) starts the app hidden in the tray.
    {
        let g_close = gui.clone();
        ui.window().on_close_requested(move || {
            let settings = g_close.engine.settings();
            if settings.close_to_tray && g_close.tray.borrow().is_some() {
                if let Some(ui) = g_close.ui.upgrade() {
                    let _ = ui.window().hide();
                }
                if let Some(tray) = g_close.tray.borrow_mut().as_mut() {
                    tray.set_visible(false);
                }
                slint::CloseRequestResponse::HideWindow
            } else {
                // 0.2.3 U1: with close-to-tray off (or no tray), the X
                // button quits the app. The event loop no longer ends
                // when the window hides, so end it explicitly; vetoing
                // the close (the old KeepWindowShown) left the button
                // dead.
                let _ = slint::quit_event_loop();
                slint::CloseRequestResponse::HideWindow
            }
        });
    }
    if start_hidden {
        let _ = ui.window().hide();
        if let Some(tray) = gui.tray.borrow_mut().as_mut() {
            tray.set_visible(false);
        }
        tracing::info!("started hidden in the tray (--minimized)");
    }

    // G3 C2: re-sync the HKCU Run entry from the stored setting on every
    // start, so a portable copy moved to a new folder repairs its entry
    // on the next launch. Failure only logs; it never blocks startup.
    {
        let s = engine.settings();
        if let Err(e) = crate::startup::sync(s.start_with_windows, s.start_minimized) {
            tracing::warn!(error = %e, "start-with-windows sync failed at startup");
        }
    }

    // Browser integration item 3: portable relocation repair, from
    // the same startup call site as the Run-entry sync above, but
    // NOT gated on the autostart setting. A moved portable folder
    // repoints its HKCU host keys on the next launch; a browser the
    // repair cannot place keeps its key and shows registration-broken
    // until re-toggled.
    {
        let broken = crate::browser::repair_on_start(&gui.paths);
        if !broken.is_empty() {
            tracing::warn!(browsers = ?broken, "browser host repair could not place the exe or manifest");
        }
        *gui.browser_broken.borrow_mut() = broken;
    }
    refresh_browser_rows(&gui);

    // G3 C1: the sidebar popup choice.
    let g = gui.clone();
    ui.on_show_popup(move || toggle_popup(&g));

    let g = gui.clone();
    ui.on_appearance_changed(move || {
        let Some(ui) = g.ui.upgrade() else { return };
        ui.invoke_apply_native_theme();
        // The native tray menu follows the OS, not the app; keep it in
        // step with the effective theme (0.2.3, U9). Every theme funnel
        // in this file repeats this line after apply_native_theme.
        crate::tray::set_menu_theme(ui.get_theme_index() == 0);
        let mut settings = g.engine.settings();
        read_appearance(&ui, &mut settings);
        if let Err(error) = g.engine.update_settings(settings.clone()) {
            toast(&g, error.to_string(), true);
        }
        resolve_auto_theme(&ui, &settings);
        refresh(&g);
    });

    let g = gui.clone();
    ui.on_fit_column(move |column| {
        let Some(ui) = g.ui.upgrade() else {
            return 100.0;
        };
        let (title, font_size) = match column {
            0 => ("NAME", 13.0),
            1 => ("SIZE", 12.0),
            2 => ("STATUS", 11.0),
            3 => ("SPEED", 11.0),
            4 => ("ADDED", 11.0),
            _ => return 100.0,
        };
        ui.set_measure_font_size(font_size);
        ui.set_measure_text(ui.invoke_translate(title.into()));
        let mut width = ui.get_measured_text_width();
        // Fit every row in the current filtered model, including rows outside
        // the viewport. Work is performed on demand, never on progress ticks.
        for row in g.tasks.iter() {
            let texts: &[&str] = match column {
                0 => &[row.name.as_str()],
                1 => &[row.total_text.as_str(), row.received_text.as_str()],
                2 => &[row.state_label.as_str()],
                3 => &[row.speed_text.as_str(), row.eta_text.as_str()],
                4 => &[row.created_text.as_str()],
                _ => &[],
            };
            for text in texts {
                ui.set_measure_text(if column == 2 {
                    ui.invoke_translate((*text).into())
                } else {
                    (*text).into()
                });
                width = width.max(ui.get_measured_text_width());
            }
        }
        // Status includes the percentage beside its label, plus a gap.
        width + if column == 2 { 60.0 } else { 24.0 }
    });

    // ------------------------------------------------------------- callbacks
    let g = gui.clone();
    ui.on_pause(move |id| act_verb(&g, id as u64, TaskVerb::Pause));
    let g = gui.clone();
    ui.on_resume(move |id| act_verb(&g, id as u64, TaskVerb::Resume));
    let g = gui.clone();
    ui.on_cancel(move |id| act_verb(&g, id as u64, TaskVerb::Cancel));
    let g = gui.clone();
    ui.on_retry(move |id| act_verb(&g, id as u64, TaskVerb::Retry));
    let g = gui.clone();
    ui.on_remove(move |id| act(&g, || g.engine.remove(id as u64, false)));
    let g = gui.clone();
    ui.on_delete_file(move |id| act(&g, || g.engine.remove(id as u64, true)));
    let g = gui.clone();
    ui.on_copy_url(move |id| copy_task_url(&g, id as u64));
    let g = gui.clone();
    ui.on_open_file(move |id| open_task_file(&g, id as u64, false));
    let g = gui.clone();
    ui.on_open_folder(move |id| {
        let _ = open_task_folder(&g, id as u64);
    });

    // ------------------------------------------------------------- selection / bulk (Commit 1)
    // Selection lives in Rust session state, keyed by the u64 task id.
    // The Slint row only carries the decimal string key and a display flag,
    // so a 150 ms refresh cannot silently lose or invent a selection.
    let g = gui.clone();
    ui.on_toggle_selection(move |key| {
        if let Some(id) = parse_key(key.as_str()) {
            {
                let mut sel = g.selected.borrow_mut();
                if !sel.remove(&id) {
                    sel.insert(id);
                }
            }
            *g.anchor.borrow_mut() = Some(id);
        }
        refresh(&g);
    });
    let g = gui.clone();
    ui.on_row_clicked(move |key, ctrl, shift| {
        let Some(id) = parse_key(key.as_str()) else {
            return;
        };
        if shift {
            let anchor = *g.anchor.borrow();
            if let Some(anchor_id) = anchor {
                let order: Vec<u64> = g
                    .tasks
                    .iter()
                    .filter_map(|row| parse_key(row.key.as_str()))
                    .collect();
                if let (Some(a), Some(b)) = (
                    order.iter().position(|x| *x == anchor_id),
                    order.iter().position(|x| *x == id),
                ) {
                    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                    {
                        let mut sel = g.selected.borrow_mut();
                        for x in &order[lo..=hi] {
                            sel.insert(*x);
                        }
                    }
                    refresh(&g);
                    return;
                }
            }
            // No usable anchor: fall through to single select below.
            {
                let mut sel = g.selected.borrow_mut();
                sel.clear();
                sel.insert(id);
            }
            *g.anchor.borrow_mut() = Some(id);
        } else if ctrl {
            {
                let mut sel = g.selected.borrow_mut();
                if !sel.remove(&id) {
                    sel.insert(id);
                }
            }
            *g.anchor.borrow_mut() = Some(id);
        } else {
            {
                let mut sel = g.selected.borrow_mut();
                sel.clear();
                sel.insert(id);
            }
            *g.anchor.borrow_mut() = Some(id);
            // Plain click is the only selection gesture that moves the
            // Task Detail focus. Ctrl/Shift/checkbox change the selection
            // set only, so the panel never jumps while bulk-selecting.
            *g.focused.borrow_mut() = Some(id);
        }
        refresh(&g);
    });
    let g = gui.clone();
    ui.on_select_all(move || {
        let ids: Vec<u64> = g
            .tasks
            .iter()
            .filter_map(|row| parse_key(row.key.as_str()))
            .collect();
        g.selected.borrow_mut().extend(ids);
        refresh(&g);
    });
    let g = gui.clone();
    ui.on_clear_selection(move || {
        g.selected.borrow_mut().clear();
        *g.anchor.borrow_mut() = None;
        refresh(&g);
    });
    let g = gui.clone();
    ui.on_close_detail(move || {
        *g.focused.borrow_mut() = None;
        refresh(&g);
    });
    let g = gui.clone();
    ui.on_bulk_pause(move || bulk_act(&g, true));
    let g = gui.clone();
    ui.on_bulk_resume(move || bulk_act(&g, false));
    let g = gui.clone();
    ui.on_bulk_remove(move || {
        let ids: Vec<u64> = g.selected.borrow().iter().copied().collect();
        let mut handled = 0usize;
        let mut skipped = 0usize;
        for id in ids {
            // Bulk Remove never deletes the file in this commit; it only
            // removes the task from the list, reusing the existing safety
            // path. delete_part_on_remove is handled by engine.remove
            // according to settings, so no stronger promise is made here.
            match g.engine.remove(id, false) {
                Ok(()) => handled += 1,
                Err(_) => skipped += 1,
            }
        }
        g.selected.borrow_mut().clear();
        ui_set_bulk_remove_armed(&g, false);
        toast(&g, format!("removed {handled}, skipped {skipped}"), false);
        refresh(&g);
    });
    let g = gui.clone();
    ui.on_strip_pause(move || {
        if let Some(id) = sole_selected(&g) {
            act_verb(&g, id, TaskVerb::Pause);
        }
    });
    let g = gui.clone();
    ui.on_strip_resume(move || {
        let Some(id) = sole_selected(&g) else { return };
        // The Resume slot is Retry only when the shared predicate says so.
        let verb = if g
            .engine
            .snapshot()
            .iter()
            .any(|task| task.id == id && caps_for_state(task.state).can_retry)
        {
            TaskVerb::Retry
        } else {
            TaskVerb::Resume
        };
        act_verb(&g, id, verb);
    });
    let g = gui.clone();
    ui.on_strip_cancel(move || {
        if let Some(id) = sole_selected(&g) {
            act_verb(&g, id, TaskVerb::Cancel);
        }
    });
    let g = gui.clone();
    ui.on_strip_remove(move || {
        if let Some(id) = sole_selected(&g) {
            act(&g, || g.engine.remove(id, false));
        }
    });
    let g = gui.clone();
    ui.on_strip_open_file(move || {
        if let Some(id) = sole_selected(&g) {
            open_task_file(&g, id, false);
        }
    });
    let g = gui.clone();
    ui.on_strip_open_folder(move || {
        if let Some(id) = sole_selected(&g) {
            let _ = open_task_folder(&g, id);
        }
    });
    let g = gui.clone();
    ui.on_strip_copy_url(move || {
        if let Some(id) = sole_selected(&g) {
            copy_task_url(&g, id);
        }
    });

    let g = gui.clone();
    ui.on_add_dir_edited(move || {
        // G2: typing in the Add destination field is an explicit user
        // choice; clearing it back to empty resets to the default (None).
        let Some(ui) = g.ui.upgrade() else { return };
        *g.add_dir_explicit.borrow_mut() = !ui.get_add_dir().trim().is_empty();
        update_add_preview(&g);
    });
    let g = gui.clone();
    ui.on_add_urls_edited(move || update_add_preview(&g));
    let g = gui.clone();
    ui.on_add_pick_dir(move || pick_into(&g, true));
    let g = gui.clone();
    ui.on_settings_pick_dir(move || pick_into(&g, false));

    let g = gui.clone();
    ui.on_add_submitted(move || submit_add(&g));
    let g = gui.clone();
    ui.on_settings_saved(move || save_settings(&g));
    let g = gui.clone();
    ui.on_settings_reset(move || {
        // Defaults must not wipe the sidebar flags from a snapshot that
        // never contained them. The live engine settings are the source.
        let defaults = settings_for_reset(&g.engine.settings());
        if let Some(ui) = g.ui.upgrade() {
            push_settings_to_ui(&ui, &defaults, g.update_layout.as_ref());
        }
        act(&g, || g.engine.update_settings(defaults.clone()));
        toast(&g, "defaults restored".into(), false);
    });
    let g = gui.clone();
    ui.on_sidebar_section_toggled(move |section| {
        let which = if section == 0 {
            SidebarSection::Library
        } else {
            SidebarSection::Categories
        };
        apply_sidebar_toggle(&g, which);
    });

    // Wiring W2 (design section 2.2): the updater surface. The checkbox
    // writes the install-base flag file directly - never through
    // Settings/engine.update_settings - and the check button is the only
    // GUI path that can open an update socket. Apply is the only GUI
    // path that swaps anything.
    let g = gui.clone();
    ui.on_set_checks_toggled(move |enabled| set_update_checks(&g, enabled));

    // Browser integration (item 1): the toggles are immediate
    // callbacks like checks-toggled, never values waiting on Save.
    // The Install buttons only reveal the built-in guide, in .slint.
    let g = gui.clone();
    ui.on_set_browser_toggled(move |browser, enabled| {
        set_browser_toggle(&g, browser.as_str(), enabled)
    });
    let g = gui.clone();
    ui.on_set_browser_hint_dismissed(move || dismiss_browser_hint(&g));
    let g = gui.clone();
    ui.on_update_check_clicked(move || start_update_check(&g));
    let g = gui.clone();
    ui.on_update_apply_clicked(move || apply_update(&g));

    let g = gui.clone();
    ui.on_dismiss_notice(move |key| {
        if let Some((caller, tx)) = parse_notice_row_key(key.as_str()) {
            dismiss_expired_notice(&g, caller, tx);
        }
        refresh(&g);
    });

    let g = gui.clone();
    ui.on_clear_finished(move || {
        let done: Vec<u64> = g
            .engine
            .snapshot()
            .into_iter()
            .filter(|s| s.state == TaskState::Completed)
            .map(|s| s.id)
            .collect();
        for id in done {
            let _ = g.engine.remove(id, false);
        }
        refresh(&g);
    });

    let g = gui.clone();
    ui.on_open_data_dir(move || shell_open(&g.engine.paths().root, false));
    let g = gui.clone();
    ui.on_open_log(move || shell_open(&g.engine.paths().log_file, false));
    ui.on_open_github(move || shell_open_url("https://github.com/iamcino/udm"));
    ui.on_open_gitlab(move || shell_open_url("https://gitlab.com/cino-group/udm"));

    // ------------------------------------------------------------- poll timer
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(REFRESH_MS),
        move || {
            tray_poll(&gui);
            repaint_guard(&gui);
            poll_update_checks(&gui);
            poll_browser_rows(&gui);
            refresh(&gui);
        },
    );
    // The timer must outlive `main`; it stops when the event loop ends.
    std::mem::forget(timer);
}

// ------------------------------------------------------------------ helpers

/// Parse a Slint task-key back to the real u64 id. Anything that is not a
/// plain decimal u64 is rejected, never coerced from a float or an index.
fn parse_key(key: &str) -> Option<u64> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    key.parse::<u64>().ok()
}

fn sole_selected(g: &Gui) -> Option<u64> {
    let selected = g.selected.borrow();
    if selected.len() == 1 {
        selected.iter().next().copied()
    } else {
        None
    }
}

fn ui_set_bulk_remove_armed(g: &Rc<Gui>, armed: bool) {
    if let Some(ui) = g.ui.upgrade() {
        ui.set_bulk_remove_armed(armed);
    }
}

/// Bulk Pause (pause=true) or Resume (pause=false), eligible tasks only.
/// Retry is deliberately not treated as Resume: Failed tasks are skipped
/// here, because Retry resets the retry budget.
fn bulk_act(g: &Rc<Gui>, pause: bool) {
    let ids: Vec<u64> = g.selected.borrow().iter().copied().collect();
    let (handled, skipped) = pause_resume_ids(g, &ids, pause);
    ui_set_bulk_remove_armed(g, false);
    toast(
        g,
        if pause {
            format!("paused {handled}, skipped {skipped}")
        } else {
            format!("resumed {handled}, skipped {skipped}")
        },
        false,
    );
    refresh(g);
}

/// Tray "Pause all" / "Resume all": same eligibility rules as the bulk
/// actions, applied to every task in the current snapshot.
fn all_act(g: &Rc<Gui>, pause: bool) {
    let ids: Vec<u64> = g.engine.snapshot().iter().map(|s| s.id).collect();
    let (handled, skipped) = pause_resume_ids(g, &ids, pause);
    if handled + skipped > 0 {
        toast(
            g,
            if pause {
                format!("paused {handled}, skipped {skipped}")
            } else {
                format!("resumed {handled}, skipped {skipped}")
            },
            false,
        );
    }
    refresh(g);
}

/// Pause (or resume) the given task ids, honouring the same per-state
/// eligibility as the selection bulk actions. Returns (handled, skipped).
fn pause_resume_ids(g: &Rc<Gui>, ids: &[u64], pause: bool) -> (usize, usize) {
    let snapshots = g.engine.snapshot();
    let mut handled = 0usize;
    let mut skipped = 0usize;
    for &id in ids {
        let Some(s) = snapshots.iter().find(|s| s.id == id) else {
            skipped += 1;
            continue;
        };
        let eligible = if pause {
            matches!(
                s.state,
                TaskState::Queued
                    | TaskState::Probing
                    | TaskState::Downloading
                    | TaskState::Verifying
            )
        } else {
            matches!(s.state, TaskState::Paused | TaskState::Canceled)
        };
        if !eligible {
            skipped += 1;
            continue;
        }
        let result = if pause {
            g.engine.pause(id)
        } else {
            g.engine.resume(id)
        };
        match result {
            Ok(()) => handled += 1,
            Err(_) => skipped += 1,
        }
    }
    (handled, skipped)
}

/// Arm the restore-repaint guard (see repaint_guard): the next few
/// poll ticks owe the window full repaints.
fn arm_repaint(g: &Rc<Gui>) {
    *g.repaint_pulses.borrow_mut() = 3;
}

/// Restore-repaint guard, run on every 150 ms poll tick. The winit
/// software renderer paints partial frames into a buffer it believes
/// still holds the previous frame; Windows discards a window's
/// presented content across minimize, so the first partial frame after
/// a restore leaves most of the window unpainted - the window behind
/// shows through and only dirty fragments (caret field, scrollbar,
/// refreshed rows) appear - until a resize forces a full repaint.
/// Detect the transitions that can invalidate the presented frame
/// (un-minimise here; the tray show paths arm the counter directly)
/// and flip the root background pulse for a few ticks: the window
/// background spans the whole window, so each flip dirties the full
/// window and the renderer repaints everything, healing the buffer.
fn repaint_guard(g: &Rc<Gui>) {
    let Some(ui) = g.ui.upgrade() else {
        return;
    };
    let minimized = ui.window().is_minimized();
    let was = *g.was_minimized.borrow();
    if was && !minimized {
        arm_repaint(g);
    }
    *g.was_minimized.borrow_mut() = minimized;
    if minimized {
        return;
    }
    let owed = *g.repaint_pulses.borrow();
    if owed > 0 {
        *g.repaint_pulses.borrow_mut() = owed - 1;
        ui.set_repaint_pulse(!ui.get_repaint_pulse());
        ui.window().request_redraw();
    }
}

/// Drain tray events (Phase 2). Runs inside the existing 150 ms poll, on
/// the event-loop thread; the tray module itself never touches the UI.
fn tray_poll(g: &Rc<Gui>) {
    use crate::tray::TrayAction;
    let actions = match g.tray.borrow().as_ref() {
        Some(tray) => tray.drain(),
        None => return,
    };
    // G3 minimise-to-tray: convert a minimised episode into a tray hide,
    // once per episode (the watcher re-arms when the window is restored).
    if g.engine.settings().minimize_to_tray {
        if let Some(ui) = g.ui.upgrade() {
            if ui.window().is_minimized() {
                if !*g.minimized_seen.borrow() {
                    *g.minimized_seen.borrow_mut() = true;
                    // 0.2.3 U2: hide only. Un-minimising here (the old
                    // set_minimized(false)) restored the window straight
                    // back onto the taskbar; the flag stays set while the
                    // window is in the tray and the restore paths clear
                    // it when they show the window again.
                    let _ = ui.window().hide();
                    if let Some(tray) = g.tray.borrow_mut().as_mut() {
                        tray.set_visible(false);
                    }
                }
            } else {
                *g.minimized_seen.borrow_mut() = false;
            }
        }
    }
    for action in actions {
        tracing::info!(?action, "tray action handled");
        match action {
            TrayAction::Show => {
                close_popup(g);
                show_main_window(g);
            }
            TrayAction::ShowPopup => toggle_popup(g),
            TrayAction::ToggleShowHide => {
                let visible = g
                    .tray
                    .borrow()
                    .as_ref()
                    .map(|tray| tray.is_visible())
                    .unwrap_or(true);
                if let Some(ui) = g.ui.upgrade() {
                    if visible {
                        let _ = ui.window().hide();
                    } else {
                        let _ = ui.window().show();
                        ui.window().set_minimized(false);
                        focus_native_window(ui.window());
                    }
                }
                arm_repaint(g);
                if let Some(tray) = g.tray.borrow_mut().as_mut() {
                    tray.set_visible(!visible);
                }
            }
            TrayAction::PauseAll => all_act(g, true),
            TrayAction::ResumeAll => all_act(g, false),
            TrayAction::CheckForUpdates => {
                // 0.2.3 U5: a tray user got no feedback from a manual
                // check — its busy/result state renders in the main
                // window, invisible while the app sits in the tray.
                // Restore the window first (the Show sequence), then
                // run the check where its outcome can be seen.
                close_popup(g);
                show_main_window(g);
                start_update_check(g);
            }
            TrayAction::Exit => {
                let _ = slint::quit_event_loop();
            }
        }
    }
    update_tray_summary(g);
    if *g.popup_open.borrow() {
        refresh_popup(g);
    }
}

// ---------------------------------------------------------------- popup

/// The task rows the popup lists: the active states (queued, probing,
/// downloading, verifying) plus paused tasks, whose Resume button is the
/// popup's most useful per-row action. The tray summary's per-file lines
/// use only the downloading subset (see update_tray_summary).
fn active_task_rows(g: &Rc<Gui>) -> Vec<TaskRow> {
    g.tasks
        .iter()
        .filter(|r| {
            matches!(
                r.state.as_str(),
                "queued" | "probing" | "downloading" | "verifying" | "paused"
            )
        })
        .collect()
}

fn show_main_window(g: &Rc<Gui>) {
    if let Some(ui) = g.ui.upgrade() {
        let next = crate::tray::after_show(crate::tray::ShownWindow {
            visible: ui.window().is_visible(),
            minimized: ui.window().is_minimized(),
        });
        // after_show never hides and never asks for a second window.
        if next.visible {
            let _ = ui.window().show();
            ui.window().set_minimized(next.minimized);
            focus_native_window(ui.window());
        }
    }
    arm_repaint(g);
    if let Some(tray) = g.tray.borrow_mut().as_mut() {
        tray.set_visible(true);
    }
}

/// Native raise for a window this process already owns. winit's
/// `focus_window` is the same call the handoff dialog uses (Alt +
/// SetForegroundWindow inside winit). No second activation hack.
fn focus_native_window(window: &slint::Window) {
    use slint::winit_030::WinitWindowAccessor;
    if window
        .with_winit_window(|winit_window| {
            winit_window.focus_window();
        })
        .is_none()
    {
        tracing::debug!("native window was not realized; focus_window was not called");
    }
}

fn toggle_popup(g: &Rc<Gui>) {
    if *g.popup_open.borrow() {
        close_popup(g);
    } else {
        open_popup(g);
    }
}

fn open_popup(g: &Rc<Gui>) {
    if g.popup.borrow().is_none() {
        let Ok(pop) = crate::PopupWindow::new() else {
            return;
        };
        let gw = Rc::downgrade(g);
        pop.on_close_popup(move || {
            if let Some(g) = gw.upgrade() {
                close_popup(&g);
            }
        });
        let gw = Rc::downgrade(g);
        pop.on_open_main(move || {
            if let Some(g) = gw.upgrade() {
                close_popup(&g);
                show_main_window(&g);
            }
        });
        let gw = Rc::downgrade(g);
        pop.on_pause_row(move |id| {
            if let Some(g) = gw.upgrade() {
                act_verb(&g, id as u64, TaskVerb::Pause);
            }
        });
        let gw = Rc::downgrade(g);
        pop.on_resume_row(move |id| {
            if let Some(g) = gw.upgrade() {
                act_verb(&g, id as u64, TaskVerb::Resume);
            }
        });
        *g.popup.borrow_mut() = Some(pop);
    }
    *g.popup_area.borrow_mut() = Some(crate::tray::cursor_work_area());
    refresh_popup(g);
    if let Some(pop) = g.popup.borrow().as_ref() {
        // Show first: an unrealised window reports a zero size, which
        // would anchor the popup almost entirely off-screen.
        let _ = pop.show();
        place_popup(pop, g);
        pop.invoke_grab_focus();
    }
    *g.popup_open.borrow_mut() = true;
}

fn close_popup(g: &Rc<Gui>) {
    if let Some(pop) = g.popup.borrow().as_ref() {
        let _ = pop.hide();
    }
    *g.popup_open.borrow_mut() = false;
    *g.popup_area.borrow_mut() = None;
}

/// Rebuild the popup model from the shared snapshot. Runs only while the
/// popup is open, inside the existing 150 ms poll - no separate timer,
/// no extra engine queries beyond the totals the status bar already
/// reads.
fn refresh_popup(g: &Rc<Gui>) {
    let Some(ui) = g.ui.upgrade() else { return };
    let vi = ui.get_language_index() == 1;
    let totals = g.engine.totals();
    let rows = active_task_rows(g);
    let extra = rows.len().saturating_sub(5);
    let borrowed = g.popup.borrow();
    let Some(pop) = borrowed.as_ref() else {
        return;
    };
    pop.set_total_text(SharedString::from(totals_line(vi, &totals)));
    pop.set_open_label(SharedString::from(if vi { "Mở UDM" } else { "Open UDM" }));
    pop.set_empty_text(SharedString::from(if vi {
        "Không có tải đang chạy"
    } else {
        "No active downloads"
    }));
    pop.set_more_text(SharedString::from(if extra > 0 {
        if vi {
            format!("v\u{e0} {extra} n\u{1eefa}a")
        } else {
            format!("and {extra} more")
        }
    } else {
        String::new()
    }));
    let model = Rc::new(VecModel::from(rows.into_iter().take(5).collect::<Vec<_>>()));
    pop.set_rows(ModelRc::from(model));
    place_popup(pop, g);
}

/// Anchor the downloads popup at the bottom-right of the work area
/// captured when it was opened, clamped so the whole window stays
/// inside that one monitor. Re-run on every refresh while open: the
/// first show can report a size that has not settled yet, and the row
/// count can change the height - without the clamp either could push
/// the window across a monitor seam (field case 2026-11-16: the popup
/// opened straddling two monitors).
fn place_popup(pop: &crate::PopupWindow, g: &Rc<Gui>) {
    let Some((left, top, right, bottom)) = *g.popup_area.borrow() else {
        return;
    };
    let size = pop.window().size();
    let (w, h) = (size.width as i32, size.height as i32);
    if w <= 0 || h <= 0 {
        return;
    }
    let x = (right - w - 12).clamp(left, (right - w).max(left));
    let y = (bottom - h - 12).clamp(top, (bottom - h).max(top));
    let pos = slint::PhysicalPosition::new(x, y);
    if pop.window().position() != pos {
        pop.window().set_position(pos);
    }
}

/// G3 B1: compose the tray summary from the current totals and task rows
/// and push it to the tray, throttled to about one update per second and
/// skipped entirely when the content is unchanged.
fn update_tray_summary(g: &Rc<Gui>) {
    if g.tray.borrow().is_none() {
        return;
    }
    let Some(ui) = g.ui.upgrade() else { return };
    let vi = ui.get_language_index() == 1;
    let totals = g.engine.totals();
    let total_line = totals_line(vi, &totals);
    let mut files: Vec<String> = Vec::new();
    for row in g.tasks.iter() {
        if files.len() >= 3 {
            break;
        }
        if row.state.as_str() == "downloading" {
            files.push(format!(
                "{} \u{b7} {}",
                elide_middle(row.name.as_str(), 36),
                row.speed_text
            ));
        }
    }
    let mut key = total_line.clone();
    for f in &files {
        key.push('\n');
        key.push_str(f);
    }
    if key == *g.tray_summary_key.borrow() {
        return;
    }
    if g.tray_summary_at.borrow().elapsed() < Duration::from_millis(900) {
        return;
    }
    *g.tray_summary_key.borrow_mut() = key;
    *g.tray_summary_at.borrow_mut() = Instant::now();
    let tooltip = if totals.active > 0 {
        Some(total_line.as_str())
    } else {
        None
    };
    if let Some(tray) = g.tray.borrow_mut().as_mut() {
        tray.set_summary(&total_line, &files, tooltip);
    }
}

/// The one-line downloads summary shared by the tray menu, the tray
/// tooltip and the popup header.
fn totals_line(vi: bool, totals: &usagi::engine::Totals) -> String {
    let speed = util::human_speed(totals.active_speed);
    let speed = if vi { speed.replace('.', ",") } else { speed };
    if totals.active > 0 {
        if vi {
            format!("{} \u{0111}ang t\u{1ea3}i \u{b7} {}", totals.active, speed)
        } else {
            format!("{} downloading \u{b7} {}", totals.active, speed)
        }
    } else if vi {
        "Kh\u{f4}ng c\u{f3} t\u{1ea3}i \u{0111}ang ch\u{1ea1}y".to_string()
    } else {
        "No active downloads".to_string()
    }
}

/// Truncate the middle of a long file name for the tray menu, keeping the
/// start and the extension end readable.
fn elide_middle(name: &str, max_chars: usize) -> String {
    let count = name.chars().count();
    if count <= max_chars {
        return name.to_string();
    }
    let head = max_chars / 2;
    let tail = max_chars - head - 1;
    let start: String = name.chars().take(head).collect();
    let end: String = name.chars().skip(count - tail).collect();
    format!("{start}\u{2026}{end}")
}

pub(crate) fn refresh_ui(g: &Rc<Gui>) {
    refresh(g);
}

pub(crate) fn show_toast(g: &Rc<Gui>, message: String, is_error: bool) {
    toast(g, message, is_error);
}

fn act(g: &Rc<Gui>, f: impl FnOnce() -> usagi::error::Result<()>) {
    match f() {
        Ok(()) => {}
        Err(e) => toast(g, e.to_string(), true),
    }
    refresh(g);
}

/// Pause, resume, cancel, or retry for one task. The row, the toolbar,
/// the tray popup, and the detail window all call this. Bulk pause and
/// resume keep their own skip counts and do not come through here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskVerb {
    Pause,
    Resume,
    Cancel,
    Retry,
}

pub(crate) fn run_task_verb(g: &Rc<Gui>, id: u64, verb: TaskVerb) -> Result<(), String> {
    let result = match verb {
        TaskVerb::Pause => g.engine.pause(id),
        TaskVerb::Resume => g.engine.resume(id),
        TaskVerb::Cancel => g.engine.cancel(id),
        TaskVerb::Retry => g.engine.retry(id),
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) => {
            let message = error.to_string();
            toast(g, message.clone(), true);
            Err(message)
        }
    }
}

fn act_verb(g: &Rc<Gui>, id: u64, verb: TaskVerb) {
    let _ = run_task_verb(g, id, verb);
    refresh(g);
}

fn toast(g: &Rc<Gui>, message: String, is_error: bool) {
    let Some(ui) = g.ui.upgrade() else { return };
    ui.set_toast(ui.invoke_translate(message.into()));
    ui.set_toast_error(is_error);
    *g.toast_until.borrow_mut() = Instant::now() + Duration::from_secs(6);
}

fn rules_fingerprint(rules: &[CategoryRule]) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    rules.len().hash(&mut hasher);
    for rule in rules {
        rule.id.hash(&mut hasher);
        rule.folder.hash(&mut hasher);
        rule.suffixes.len().hash(&mut hasher);
        for suffix in &rule.suffixes {
            suffix.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// Empty names and a `None` classification both land in the virtual `other`
/// bucket, including when the effective set has no `other` rule.
fn category_bucket(filename: &str, rules: &[CategoryRule]) -> String {
    if filename.is_empty() {
        return OTHER_CATEGORY.to_string();
    }
    classify(filename, rules)
        .map(|rule| rule.id.clone())
        .unwrap_or_else(|| OTHER_CATEGORY.to_string())
}

fn classify_cached(cache: &mut CategoryCache, filename: &str, rules: &[CategoryRule]) -> String {
    let fingerprint = rules_fingerprint(rules);
    if cache.fingerprint != fingerprint {
        cache.by_filename.clear();
        cache.fingerprint = fingerprint;
    }
    if let Some(id) = cache.by_filename.get(filename) {
        return id.clone();
    }
    let id = category_bucket(filename, rules);
    cache.misses = cache.misses.saturating_add(1);
    cache.by_filename.insert(filename.to_string(), id.clone());
    id
}

fn status_matches(library: i32, state: TaskState) -> bool {
    match library {
        LIBRARY_ACTIVE => matches!(
            state,
            TaskState::Queued
                | TaskState::Probing
                | TaskState::Downloading
                | TaskState::Verifying
                | TaskState::Paused
        ),
        LIBRARY_FINISHED => state == TaskState::Completed,
        LIBRARY_FAILED => state == TaskState::Failed,
        LIBRARY_UNFINISHED => state != TaskState::Completed,
        _ => true,
    }
}

fn category_matches(selected: &str, task_category: &str) -> bool {
    selected.is_empty() || selected == task_category
}

fn search_matches(filter_lower: &str, filename: &str, url: &str) -> bool {
    filter_lower.is_empty()
        || filename.to_lowercase().contains(filter_lower)
        || url.to_lowercase().contains(filter_lower)
}

/// One walk: classify each filename at most once and tally every badge.
/// `visible` is status AND category AND search. Counts ignore all three.
struct ListPass {
    visible: Vec<usize>,
    counts: CategoryCounts,
}

fn pass_task_list(
    tasks: &[ViewTask<'_>],
    rules: &[CategoryRule],
    cache: &mut CategoryCache,
    library: i32,
    category: &str,
    filter_lower: &str,
) -> ListPass {
    let mut by_id: BTreeMap<String, i32> = BTreeMap::new();
    for rule in rules {
        by_id.insert(rule.id.clone(), 0);
    }
    by_id.entry(OTHER_CATEGORY.to_string()).or_insert(0);
    let mut finished = 0i32;
    let mut unfinished = 0i32;
    let mut visible = Vec::new();
    for (index, task) in tasks.iter().enumerate() {
        let id = classify_cached(cache, task.filename, rules);
        *by_id.entry(id.clone()).or_default() += 1;
        if task.state == TaskState::Completed {
            finished += 1;
        } else {
            unfinished += 1;
        }
        if status_matches(library, task.state)
            && category_matches(category, &id)
            && search_matches(filter_lower, task.filename, task.url)
        {
            visible.push(index);
        }
    }
    ListPass {
        visible,
        counts: CategoryCounts {
            by_id,
            total: i32::try_from(tasks.len()).unwrap_or(i32::MAX),
            finished,
            unfinished,
        },
    }
}

/// English source for a built-in category id. A custom id is not a
/// translation key: callers show the raw id instead.
fn builtin_category_label(id: &str) -> Option<&'static str> {
    match id {
        "documents" => Some("Documents"),
        "music" => Some("Music"),
        "videos" => Some("Videos"),
        "images" => Some("Images"),
        "archives" => Some("Archives"),
        "programs" => Some("Programs"),
        "disk-images" => Some("Disk images"),
        "other" => Some("Other"),
        _ => None,
    }
}

pub(crate) fn category_label_for(
    filename: &str,
    rules: &[CategoryRule],
    vietnamese: bool,
) -> String {
    category_item_label(&category_bucket(filename, rules), vietnamese)
}

pub(crate) fn category_item_label(id: &str, vietnamese: bool) -> String {
    match builtin_category_label(id) {
        Some(key) => {
            if vietnamese {
                crate::i18n::vi(key).to_string()
            } else {
                key.to_string()
            }
        }
        None => id.to_string(),
    }
}

fn badge_text(count: i32) -> String {
    if count > 0 {
        count.to_string()
    } else {
        String::new()
    }
}

fn category_glyph(id: &str) -> &'static str {
    match id {
        "documents" => "D",
        "music" => "M",
        "videos" => "V",
        "images" => "I",
        "archives" => "Z",
        "programs" => "P",
        "disk-images" => "O",
        _ => "\u{2022}",
    }
}

/// All categories, one row per effective rule, then Finished and Unfinished.
/// Built-in ids go through the translator. A custom id is shown as itself.
fn publish_category_nav(g: &Gui, ui: &AppWindow, rules: &[CategoryRule], counts: &CategoryCounts) {
    let vietnamese = ui.get_language_index() == 1;
    let mut rows = Vec::with_capacity(rules.len() + 3);
    rows.push(CategoryNav {
        id: SharedString::from(""),
        label: ui.invoke_translate("All categories".into()),
        badge: SharedString::from(badge_text(counts.total)),
        glyph: SharedString::from("*"),
        kind: 0,
    });
    for rule in rules {
        let count = counts.by_id.get(&rule.id).copied().unwrap_or(0);
        rows.push(CategoryNav {
            id: SharedString::from(rule.id.as_str()),
            label: SharedString::from(category_item_label(&rule.id, vietnamese)),
            badge: SharedString::from(badge_text(count)),
            glyph: SharedString::from(category_glyph(&rule.id)),
            kind: 0,
        });
    }
    rows.push(CategoryNav {
        id: SharedString::from(""),
        label: ui.invoke_translate("Finished".into()),
        badge: SharedString::from(badge_text(counts.finished)),
        glyph: SharedString::from("\u{2713}"),
        kind: 1,
    });
    rows.push(CategoryNav {
        id: SharedString::from(""),
        label: ui.invoke_translate("Unfinished".into()),
        badge: SharedString::from(badge_text(counts.unfinished)),
        glyph: SharedString::from("\u{2026}"),
        kind: 2,
    });
    if g.category_nav.row_count() == rows.len() {
        for (index, row) in rows.into_iter().enumerate() {
            g.category_nav.set_row_data(index, row);
        }
    } else {
        g.category_nav.set_vec(rows);
    }
}

/// Notices belong to All on both axes. A category or a status filter hides them.
fn notices_in_view(library: i32, category: &str) -> bool {
    library == LIBRARY_ALL && category.is_empty()
}

fn notice_row_key(caller: &str, tx: &str) -> String {
    format!("notice{NOTICE_SEP}{caller}{NOTICE_SEP}{tx}")
}

fn parse_notice_row_key(key: &str) -> Option<(&str, &str)> {
    let rest = key.strip_prefix("notice\u{1e}")?;
    let (caller, tx) = rest.split_once('\u{1e}')?;
    if caller.is_empty() || tx.is_empty() {
        None
    } else {
        Some((caller, tx))
    }
}

fn notice_from_discovery(entry: &DiscoveryEntry) -> ExpiredNotice {
    let url = entry
        .intent
        .as_ref()
        .map(|intent| intent.url.clone())
        .unwrap_or_default();
    let filename = entry
        .intent
        .as_ref()
        .and_then(|intent| intent.filename_hint.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_default();
    let size_hint = entry.intent.as_ref().and_then(|intent| intent.size_hint);
    ExpiredNotice {
        caller: entry.caller.clone(),
        tx: entry.tx.clone(),
        url,
        filename,
        size_hint,
        at_ms: entry.created_at_ms,
    }
}

fn collect_expired_notices(
    entries: &[DiscoveryEntry],
    dismissed: &BTreeSet<(String, String)>,
) -> Vec<ExpiredNotice> {
    entries
        .iter()
        .filter(|entry| entry.expired)
        .filter(|entry| !dismissed.contains(&(entry.caller.clone(), entry.tx.clone())))
        .map(notice_from_discovery)
        .collect()
}

/// Read discovery about once a second. The rendered rows come from the
/// cached result, so a 150 ms tick does not call the coordinator again.
fn poll_expired_notices(g: &Gui) {
    if g.notice_polled_at.borrow().elapsed() < NOTICE_POLL {
        return;
    }
    *g.notice_polled_at.borrow_mut() = Instant::now();
    let coordinator = g.handoff.borrow().clone();
    let Some(coordinator) = coordinator else {
        g.notice_cache.borrow_mut().clear();
        return;
    };
    let callers = usagi::handoff::read_callers(&g.paths);
    let dismissed = g.notice_dismissed.borrow().clone();
    let mut found = Vec::new();
    let mut all = Vec::new();
    let mut failed = BTreeSet::new();
    for caller in &callers {
        match coordinator.discovery(caller) {
            Ok(entries) => {
                found.extend(collect_expired_notices(&entries, &dismissed));
                all.extend(entries);
            }
            Err(error) => {
                failed.insert(caller.clone());
                tracing::debug!(%caller, %error, "handoff discovery skipped");
            }
        }
    }
    *g.notice_cache.borrow_mut() = found;
    crate::handoff_dialog::note_discovery(g, &all, &failed);
}

fn dismiss_expired_notice(g: &Gui, caller: &str, tx: &str) {
    g.notice_dismissed
        .borrow_mut()
        .insert((caller.to_string(), tx.to_string()));
    g.notice_cache
        .borrow_mut()
        .retain(|notice| notice.caller != caller || notice.tx != tx);
}

fn refresh(g: &Rc<Gui>) {
    let Some(ui) = g.ui.upgrade() else { return };
    // Slint 1.6 does not clamp ScrollView state when its viewport grows.
    // Reuse the existing refresh (no extra timer), writing only invalid offsets.
    if ui.get_table_needs_scroll_clamp() {
        ui.invoke_clamp_table_scroll();
    }

    // clear an expired toast
    if ui.get_toast() != "" && Instant::now() >= *g.toast_until.borrow() {
        ui.set_toast(SharedString::default());
    }

    // surface engine log events (failures) as toasts
    for event in g.engine.drain_events(32) {
        if let Event::TaskFailed { id, error } = event {
            toast(g, format!("task #{id}: {error}"), true);
        }
    }

    // --- rows: status axis AND category axis AND toolbar search.
    // One walk classifies each filename at most once and tallies every
    // category badge. Counts are global; the search text does not narrow them.
    let library = ui.get_library();
    let category = ui.get_category().to_string();
    let filter = ui.get_filter().to_lowercase();
    let rules = g.engine.settings().category_rules();
    let snapshots = g.engine.snapshot();
    let views: Vec<ViewTask<'_>> = snapshots
        .iter()
        .map(|s| ViewTask {
            filename: &s.filename,
            url: &s.url,
            state: s.state,
        })
        .collect();
    let listed = {
        let mut cache = g.category_cache.borrow_mut();
        pass_task_list(&views, &rules, &mut cache, library, &category, &filter)
    };
    // Badge totals from this same walk. Search does not narrow them.
    let counts = listed.counts;
    publish_category_nav(g, &ui, &rules, &counts);
    let visible: Vec<&TaskSnapshot> = listed
        .visible
        .iter()
        .map(|&index| &snapshots[index])
        .collect();
    // Prune selection to currently visible rows, so bulk actions never
    // silently affect a hidden row, and removed tasks drop out.
    let visible_ids: BTreeSet<u64> = visible.iter().map(|s| s.id).collect();
    g.selected
        .borrow_mut()
        .retain(|id| visible_ids.contains(id));
    let selected = g.selected.borrow().clone();
    ui.set_selected_count(selected.len() as i32);
    publish_strip(&ui, &snapshots, &selected);
    let mut rows: Vec<TaskRow> = visible
        .iter()
        .map(|s| row_from(s, ui.get_language_index() == 1, selected.contains(&s.id)))
        .collect();
    // G5-closing (owner decision 2026-11-09): durable handoff commitments
    // whose engine association never landed are rendered as read-only
    // recovery rows in the All view. They are coordinator records, not
    // engine tasks: no id, no actions - every row handler resolves keys
    // through parse_key, which rejects the recovery: keys, so clicks on
    // them are inert. The text filter applies to them like any row.
    if library == 0 {
        if let Some(coordinator) = g.handoff.borrow().as_ref() {
            let vietnamese = ui.get_language_index() == 1;
            for entry in coordinator.recovery_entries() {
                let row = recovery_row(&entry, vietnamese);
                if filter.is_empty()
                    || row.name.to_lowercase().contains(&filter)
                    || row.url.to_lowercase().contains(&filter)
                {
                    rows.push(row);
                }
            }
        }
    }
    // Expired handoffs from this GUI session (or still Prepared at startup
    // and now expired). Same family as recovery rows: not engine tasks, no
    // actions, never counted. They stay in All on both axes; a category or
    // a status selection hides them. Dismissals are in-memory for the session.
    poll_expired_notices(g);
    crate::handoff_dialog::tick(g);
    if notices_in_view(library, &category) {
        let vietnamese = ui.get_language_index() == 1;
        let cached = g.notice_cache.borrow().clone();
        for notice in cached {
            let row = notice_row(&notice, vietnamese);
            if filter.is_empty()
                || row.name.to_lowercase().contains(&filter)
                || row.url.to_lowercase().contains(&filter)
                || row.error_text.to_lowercase().contains(&filter)
            {
                rows.push(row);
            }
        }
    }
    if g.tasks.row_count() == rows.len() {
        for (i, row) in rows.into_iter().enumerate() {
            g.tasks.set_row_data(i, row);
        }
    } else {
        g.tasks.set_vec(rows);
    }

    // --- Task Detail (Commit 2): focus is separate from selection.
    // Use the full snapshot list, not the filtered rows, so a filter
    // change does not close the panel. Refresh never changes the focus;
    // it only clears it when the focused task no longer exists at all.
    {
        let focused_id = *g.focused.borrow();
        match focused_id {
            Some(fid) => match snapshots.iter().find(|s| s.id == fid) {
                Some(s) => {
                    ui.set_detail(detail_from(s, ui.get_language_index() == 1));
                    ui.set_detail_visible(true);
                }
                None => {
                    *g.focused.borrow_mut() = None;
                    ui.set_detail_visible(false);
                }
            },
            None => ui.set_detail_visible(false),
        }
    }

    // --- status (cheap)
    let totals = g.engine.totals();
    let paths = g.engine.paths();
    let prev_status = ui.get_status();
    ui.set_status(StatusInfo {
        active_count: totals.active as i32,
        queued_count: totals.queued as i32,
        paused_count: totals.paused as i32,
        completed_count: totals.completed as i32,
        failed_count: totals.failed as i32,
        speed_text: SharedString::from(util::human_speed(totals.active_speed)),
        db_text: prev_status.db_text,
        mode_text: SharedString::from(if paths.portable {
            "Runs without installation"
        } else {
            "Installed for this Windows account"
        }),
        data_dir_text: SharedString::from(util::display_path(&paths.root)),
        disk_text: prev_status.disk_text,
        disk_fraction: prev_status.disk_fraction,
    });

    // --- slow stuff: db stats + alt-svc cache
    if g.slow.borrow().elapsed() >= Duration::from_millis(SLOW_REFRESH_MS) {
        *g.slow.borrow_mut() = Instant::now();
        // G3: while the theme preference is Auto, re-resolving on the slow
        // tick makes the UI follow OS dark/light flips within seconds.
        resolve_auto_theme(&ui, &g.engine.settings());
        {
            let dl = g.engine.settings().download_dir();
            let (free, total) = disk_stats(&dl);
            if total > 0 {
                let mut status = ui.get_status();
                status.disk_text = SharedString::from(if ui.get_language_index() == 1 {
                    format!(
                        "Trống {} / {}",
                        util::human_bytes(free as f64),
                        util::human_bytes(total as f64)
                    )
                } else {
                    format!(
                        "{} free of {}",
                        util::human_bytes(free as f64),
                        util::human_bytes(total as f64)
                    )
                });
                status.disk_fraction = ((total - free) as f64 / total as f64) as f32;
                ui.set_status(status);
            }
        }
        if let Ok(stats) = g.engine.store().stats() {
            let mut status = ui.get_status();
            status.db_text = SharedString::from(format!(
                "db {} · {} task{}",
                util::human_bytes(stats.db_bytes as f64),
                stats.tasks,
                if stats.tasks == 1 { "" } else { "s" }
            ));
            ui.set_status(status);
        }
        if let Ok(entries) = g.engine.store().alt_svc_all() {
            let rows: Vec<EndpointRow> = entries
                .into_iter()
                .map(|e| EndpointRow {
                    origin: SharedString::from(e.origin),
                    learned_text: SharedString::from(util::format_timestamp(e.learned_at)),
                    failures_text: SharedString::from(format!(
                        "{} failure(s), ma={}s",
                        e.failures, e.max_age_secs
                    )),
                })
                .collect();
            g.endpoints.set_vec(rows);
        }
    }

    // Detail windows share this snapshot. A list filter does not hide them,
    // and this call does not move or focus them.
    crate::detail_window::sync(g, &snapshots);
}

/// Row title: the resolved filename once known, else the best guess from the
/// URL (last path segment, percent-decoded) so queued/paused/failed rows that
/// have not seen response headers yet are never blank. Final fallbacks: host,
/// then the raw URL.
fn display_name(s: &TaskSnapshot) -> String {
    if !s.filename.is_empty() {
        return s.filename.clone();
    }
    if let Ok(url) = url::Url::parse(&s.url) {
        if let Some(name) = util::filename_from_url(&url) {
            return name;
        }
        if let Some(host) = url.host_str() {
            return host.to_string();
        }
    }
    s.url.clone()
}

/// Hex digest of a stored hash spec, with any `algo:` prefix removed. The
/// engine's snapshot string is the *display* form (`algo:first12...`), so a
/// trailing ellipsis is stripped too; real hex never contains dots.
fn digest_of(h: &str) -> &str {
    let rest = match h.rsplit_once(':') {
        Some((_, rest)) if !rest.is_empty() => rest,
        _ => h,
    };
    rest.trim_end_matches('.')
}

/// `algo:first12hex…` - compact form for the row meta line.
fn short_hash(h: &str) -> String {
    let (alg, rest) = h.split_once(':').unwrap_or(("", h));
    format!("{alg}:{:.12}", rest)
}

/// Build the bottom-panel detail for the focused task, from snapshot data
/// only. No priority, referer, per-segment speed, or history is invented:
/// the snapshot does not carry those fields.
fn transport_labels(s: &TaskSnapshot, vietnamese: bool) -> (&'static str, String) {
    use usagi::store::records::{TransportKind, TransportSummary};
    if matches!(s.transport_summary, TransportSummary::Mixed(_)) {
        return ("Mixed", if vietnamese { "Hỗn hợp" } else { "Mixed" }.into());
    }
    let (short, label) = match s.transport {
        Some(TransportKind::Http3) => ("H3", "HTTP/3 (QUIC)"),
        Some(TransportKind::Http2) => ("H2", "HTTP/2"),
        Some(TransportKind::Http11) => ("H1", "HTTP/1.1"),
        _ => ("?", ""),
    };
    (short, label.into())
}

/// The row/detail state label, with the segment-retry note appended
/// while a transfer is recovering from a transport error (0.2.2): a
/// task working through a stall must read as retrying, not frozen.
fn state_label_with_retry(s: &TaskSnapshot, vietnamese: bool) -> String {
    match (&s.segment_retry, s.state == TaskState::Downloading) {
        (Some(retry), true) => format!(
            "{} \u{b7} {}",
            s.state_label,
            if vietnamese {
                format!(
                    "đang thử lại đoạn {} ({}/{})",
                    retry.segment + 1,
                    retry.attempt,
                    retry.max
                )
            } else {
                format!(
                    "retrying segment {} ({}/{})",
                    retry.segment + 1,
                    retry.attempt,
                    retry.max
                )
            }
        ),
        _ => s.state_label.clone(),
    }
}

fn detail_from(s: &TaskSnapshot, vietnamese: bool) -> TaskDetail {
    let (_, mut transport_label) = transport_labels(s, vietnamese);
    if !s.paths.is_empty() {
        let paths = s
            .paths
            .iter()
            .map(|path| {
                let family = match path.family {
                    usagi::net::http12::Family::V4 => "IPv4",
                    _ => "IPv6",
                };
                let protocols = path
                    .observed_protocols
                    .iter()
                    .map(|p| p.short())
                    .collect::<Vec<_>>()
                    .join("/");
                let state = if path.active_ranges > 0 {
                    if vietnamese {
                        "tải"
                    } else {
                        "active"
                    }
                } else if s.state.is_active() {
                    if vietnamese {
                        "chờ"
                    } else {
                        "ready"
                    }
                } else {
                    if vietnamese {
                        "dừng"
                    } else {
                        "retired"
                    }
                };
                format!("{family}: {protocols} ({state})")
            })
            .collect::<Vec<_>>()
            .join("\n");
        transport_label = paths;
    }
    let percent = if s.state == TaskState::Completed {
        1.0
    } else {
        s.fraction.map(|f| f as f32).unwrap_or(-1.0)
    };
    let progress_text = match s.total_size {
        Some(total) => format!(
            "{} / {}",
            util::human_bytes(s.received as f64),
            util::human_bytes(total as f64)
        ),
        None => format!(
            "{} / {}",
            util::human_bytes(s.received as f64),
            if vietnamese {
                "không rõ kích thước"
            } else {
                "unknown size"
            }
        ),
    };
    let speed = if s.verify_bps > 0.0 {
        format!(
            "{} {}",
            if vietnamese { "xác minh" } else { "verifying" },
            util::human_speed(s.verify_bps)
        )
    } else if s.state == TaskState::Downloading && s.speed_bps > 0.0 {
        util::human_speed(s.speed_bps)
    } else {
        String::new()
    };
    let eta = if s.state == TaskState::Downloading && s.speed_bps > 1.0 && s.eta_secs.is_finite() {
        if vietnamese {
            format!("còn {}", util::human_eta(s.eta_secs))
        } else {
            format!("{} left", util::human_eta(s.eta_secs))
        }
    } else {
        String::new()
    };
    // Checksum status. "Verified" is claimed only when the engine's own
    // finish path must already have run: in crates/core/src/engine/task.rs
    // finish(), a present expected hash with verify_checksums on goes
    // through Verifying, the full digest is compared with matches(), a
    // mismatch returns an error (the task fails and is never renamed), and
    // only a match logs "checksum verified" and proceeds to Completed.
    // So Completed + expected + computed present implies the engine
    // compared the full hashes. A GUI prefix comparison is never used as
    // proof, and both hash fields are previews, not full checksums.
    let (checksum_status, checksum_verified) = match (&s.expected_hash, &s.computed_hash) {
        (Some(_), Some(_)) if s.state == TaskState::Completed => (
            if vietnamese {
                "Đã được engine xác minh"
            } else {
                "Verified by engine"
            }
            .to_string(),
            true,
        ),
        (Some(_), Some(_)) => (
            if vietnamese {
                "Đã tính digest, chưa xác nhận xác minh"
            } else {
                "Digest computed, verification not confirmed"
            }
            .to_string(),
            false,
        ),
        (Some(_), None) => (
            if vietnamese {
                "Có checksum kỳ vọng, chưa xác minh"
            } else {
                "Expected checksum present, not verified"
            }
            .to_string(),
            false,
        ),
        (None, Some(_)) => (
            if vietnamese {
                "Đã tính digest, không có checksum kỳ vọng để đối chiếu"
            } else {
                "Digest computed, no expected checksum to verify against"
            }
            .to_string(),
            false,
        ),
        (None, None) => (
            if vietnamese {
                "Không có checksum"
            } else {
                "No checksum provided"
            }
            .to_string(),
            false,
        ),
    };
    TaskDetail {
        id: s.id as f32,
        key: SharedString::from(s.id.to_string()),
        name: SharedString::from(display_name(s)),
        state: SharedString::from(s.state.as_str()),
        state_label: SharedString::from(state_label_with_retry(s, vietnamese)),
        percent,
        progress_text: SharedString::from(progress_text),
        size_text: SharedString::from(match s.total_size {
            Some(t) => util::human_bytes(t as f64),
            None => "unknown size".to_string(),
        }),
        speed_text: SharedString::from(speed),
        eta_text: SharedString::from(eta),
        transport_label: SharedString::from(transport_label),
        segments_text: SharedString::from(format!(
            "{}/{} {} · {} {}",
            s.segments_done,
            s.segments_total,
            if vietnamese { "xong" } else { "done" },
            s.active_requests,
            if vietnamese { "yêu cầu" } else { "req" },
        )),
        url: SharedString::from(s.url.clone()),
        dest_dir: SharedString::from(s.dest_dir.clone()),
        final_path: SharedString::from(s.final_path.clone()),
        created_text: SharedString::from(util::format_timestamp(s.created_at)),
        started_text: SharedString::from(
            s.started_at
                .map(util::format_timestamp)
                .unwrap_or_else(|| "-".to_string()),
        ),
        finished_text: SharedString::from(
            s.finished_at
                .map(util::format_timestamp)
                .unwrap_or_else(|| "-".to_string()),
        ),
        attempts_text: SharedString::from(s.attempts.to_string()),
        error_text: SharedString::from(s.error.clone().unwrap_or_default()),
        expected_hash_text: SharedString::from(s.expected_hash.clone().unwrap_or_default()),
        computed_hash_text: SharedString::from(
            s.computed_hash
                .as_ref()
                .map(|h| format!("{:.12}...", digest_of(h)))
                .unwrap_or_default(),
        ),
        checksum_status: SharedString::from(checksum_status),
        checksum_verified,
        can_open_file: s.state == TaskState::Completed,
        can_open_folder: matches!(s.state, TaskState::Completed | TaskState::Paused),
        accept_ranges: s.accept_ranges,
        ranges_observed: s.ranges_observed,
    }
}

/// Toolbar strip after the selection rules. Empty selection disables every
/// task action. One real task uses that task's predicates, and the Resume
/// slot becomes Retry for a single Failed task. Several real tasks enable
/// Pause, Resume, or Remove only when every selected task accepts that
/// action; Cancel, Open, Folder, and Copy stay off.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StripEnablement {
    resume: bool,
    retry: bool,
    pause: bool,
    cancel: bool,
    remove: bool,
    open_file: bool,
    open_folder: bool,
    copy_url: bool,
}

fn strip_enablement(tasks: &[StripCaps]) -> StripEnablement {
    let off = StripEnablement {
        resume: false,
        retry: false,
        pause: false,
        cancel: false,
        remove: false,
        open_file: false,
        open_folder: false,
        copy_url: false,
    };
    match tasks {
        [] => off,
        [task] => StripEnablement {
            resume: task.can_resume || task.can_retry,
            retry: task.can_retry,
            pause: task.can_pause,
            cancel: task.can_cancel,
            remove: task.can_remove,
            open_file: task.can_open_file,
            open_folder: task.can_open_folder,
            copy_url: task.can_copy,
        },
        many => StripEnablement {
            resume: many.iter().all(|task| task.can_resume),
            retry: false,
            pause: many.iter().all(|task| task.can_pause),
            cancel: false,
            remove: many.iter().all(|task| task.can_remove),
            open_file: false,
            open_folder: false,
            copy_url: false,
        },
    }
}

fn publish_strip(ui: &AppWindow, snapshots: &[TaskSnapshot], selected: &BTreeSet<u64>) {
    let caps: Vec<StripCaps> = snapshots
        .iter()
        .filter(|task| selected.contains(&task.id))
        .map(|task| caps_for_state(task.state))
        .collect();
    let enable = strip_enablement(&caps);
    ui.set_can_strip_resume(enable.resume);
    ui.set_can_strip_pause(enable.pause);
    ui.set_can_strip_cancel(enable.cancel);
    ui.set_can_strip_remove(enable.remove);
    ui.set_can_strip_open(enable.open_file);
    ui.set_can_strip_folder(enable.open_folder);
    ui.set_can_strip_copy(enable.copy_url);
    ui.set_can_strip_retry(enable.retry);
}

fn row_from(s: &TaskSnapshot, vietnamese: bool, selected: bool) -> TaskRow {
    let downloading = s.state == TaskState::Downloading;
    let (transport, transport_label) = transport_labels(s, vietnamese);
    let speed = if s.verify_bps > 0.0 {
        format!(
            "{} {}",
            if vietnamese { "xác minh" } else { "verifying" },
            util::human_speed(s.verify_bps)
        )
    } else if downloading && s.speed_bps > 0.0 {
        util::human_speed(s.speed_bps)
    } else {
        String::new()
    };
    let eta = if downloading && s.speed_bps > 1.0 && s.eta_secs.is_finite() {
        if vietnamese {
            format!("còn {}", util::human_eta(s.eta_secs))
        } else {
            format!("{} left", util::human_eta(s.eta_secs))
        }
    } else {
        String::new()
    };
    // Row meta is narrow: show a short digest, with a tick when the computed
    // value matches, instead of the full "expected = computed" pair.
    let hash = match (&s.expected_hash, &s.computed_hash) {
        (Some(e), Some(c)) => {
            let algo = e.split_once(':').map(|(a, _)| a).unwrap_or("hash");
            // The snapshot only carries the 12-hex display digest, so a match
            // is a prefix match against the full computed hex.
            if digest_of(c)
                .to_ascii_lowercase()
                .starts_with(&digest_of(e).to_ascii_lowercase())
            {
                format!("{algo} ✓")
            } else {
                format!("{} ≠ {:.12}", short_hash(e), digest_of(c))
            }
        }
        (Some(e), None) => short_hash(e),
        _ => String::new(),
    };

    let name = display_name(s);
    let caps = caps_for_state(s.state);
    TaskRow {
        id: s.id as f32,
        key: SharedString::from(s.id.to_string()),
        selected,
        name: SharedString::from(name.clone()),
        url: SharedString::from(s.url.clone()),
        state: SharedString::from(s.state.as_str()),
        state_label: SharedString::from(state_label_with_retry(s, vietnamese)),
        received_text: SharedString::from(util::human_bytes(s.received as f64)),
        total_text: SharedString::from(match s.total_size {
            Some(t) => util::human_bytes(t as f64),
            None => "unknown size".to_string(),
        }),
        percent: if s.state == TaskState::Completed {
            1.0
        } else {
            s.fraction.map(|f| f as f32).unwrap_or(-1.0)
        },
        speed_text: SharedString::from(speed),
        eta_text: SharedString::from(eta),
        transport: SharedString::from(transport),
        transport_label: SharedString::from(transport_label),
        segments_text: SharedString::from(format!(
            "{} {}",
            s.active_requests,
            if vietnamese { "yêu cầu" } else { "req" }
        )),
        error_text: SharedString::from(s.error.clone().unwrap_or_default()),
        path_text: SharedString::from(s.final_path.clone()),
        created_text: SharedString::from(util::format_timestamp(s.created_at)),
        can_pause: caps.can_pause,
        can_resume: caps.can_resume,
        can_cancel: caps.can_cancel,
        can_retry: caps.can_retry,
        can_open_file: caps.can_open_file,
        can_open_folder: caps.can_open_folder,
        can_remove: caps.can_remove,
        can_dismiss: false,
        hash_text: SharedString::from(hash),
        glyph: SharedString::from(glyph_for(&name)),
    }
}

/// A read-only recovery row for one lost-commit handoff record (see
/// the refresh site). `state` uses the wire vocabulary
/// (recovery-required); the label goes through the I18n translator like
/// every state label. Size shows the browser's hint when one exists.
fn recovery_row(e: &RecoveryEntry, _vietnamese: bool) -> TaskRow {
    let url = e.url.clone().unwrap_or_default();
    let name = e
        .filename_hint
        .clone()
        .filter(|n| !n.is_empty())
        .or_else(|| {
            url::Url::parse(&url)
                .ok()
                .and_then(|u| util::filename_from_url(&u))
        })
        .unwrap_or_else(|| url.clone());
    let detail = match &e.outcome {
        Some(o) => format!("Browser handoff needs recovery ({o})"),
        None => "Browser handoff needs recovery".to_string(),
    };
    TaskRow {
        id: -1.0,
        key: SharedString::from(format!("recovery:{}", e.tx)),
        selected: false,
        name: SharedString::from(name.clone()),
        url: SharedString::from(url),
        state: SharedString::from("recovery-required"),
        state_label: SharedString::from("Needs recovery"),
        received_text: SharedString::from(util::human_bytes(0.0)),
        total_text: SharedString::from(match e.size_hint {
            Some(t) => util::human_bytes(t as f64),
            None => "unknown size".to_string(),
        }),
        percent: -1.0,
        speed_text: SharedString::from(""),
        eta_text: SharedString::from(""),
        transport: SharedString::from(""),
        transport_label: SharedString::from(""),
        segments_text: SharedString::from(""),
        error_text: SharedString::from(detail),
        path_text: SharedString::from(""),
        created_text: SharedString::from(util::format_timestamp(e.committed_at_ms)),
        can_pause: false,
        can_resume: false,
        can_cancel: false,
        can_retry: false,
        can_open_file: false,
        can_open_folder: false,
        can_remove: false,
        can_dismiss: false,
        hash_text: SharedString::from(""),
        glyph: SharedString::from(glyph_for(&name)),
    }
}

/// Read-only expired-handoff notice. The sentence is the English i18n key
/// in `state_label` (the row translates it) and already translated in the
/// meta line. No task actions: every `can_*` flag stays false.
fn notice_row(notice: &ExpiredNotice, vietnamese: bool) -> TaskRow {
    let message = if vietnamese {
        crate::i18n::vi(EXPIRED_HANDOFF_TEXT).to_string()
    } else {
        EXPIRED_HANDOFF_TEXT.to_string()
    };
    let name = if notice.filename.is_empty() {
        message.clone()
    } else {
        notice.filename.clone()
    };
    TaskRow {
        id: -1.0,
        key: SharedString::from(notice_row_key(&notice.caller, &notice.tx)),
        selected: false,
        name: SharedString::from(name.clone()),
        url: SharedString::from(notice.url.clone()),
        state: SharedString::from("expired-handoff"),
        state_label: SharedString::from(EXPIRED_HANDOFF_TEXT),
        received_text: SharedString::from(""),
        total_text: SharedString::from(match notice.size_hint {
            Some(size) => util::human_bytes(size as f64),
            None => String::new(),
        }),
        percent: -1.0,
        speed_text: SharedString::from(""),
        eta_text: SharedString::from(""),
        transport: SharedString::from("?"),
        transport_label: SharedString::from(""),
        segments_text: SharedString::from(""),
        error_text: SharedString::from(message),
        path_text: SharedString::from(""),
        created_text: SharedString::from(util::format_timestamp(notice.at_ms)),
        can_pause: false,
        can_resume: false,
        can_cancel: false,
        can_retry: false,
        can_open_file: false,
        can_open_folder: false,
        can_remove: false,
        can_dismiss: true,
        hash_text: SharedString::from(""),
        glyph: SharedString::from(if notice.filename.is_empty() {
            "FILE"
        } else {
            glyph_for(&name)
        }),
    }
}

/// Category badge for the downloads table, derived from the file extension.
fn glyph_for(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "iso" | "img" | "wim" | "esd" | "vhd" | "vhdx" => "ISO",
        "zip" | "7z" | "rar" | "tar" | "gz" | "xz" | "bz2" | "zst" | "tgz" => "ZIP",
        "exe" | "msi" | "appx" | "msix" => "EXE",
        "mp4" | "mkv" | "avi" | "mov" | "webm" | "m4v" | "wmv" | "flv" | "ts" => "VID",
        "mp3" | "flac" | "wav" | "m4a" | "ogg" | "opus" | "aac" | "wma" => "AUD",
        "pdf" => "PDF",
        "doc" | "docx" | "odt" | "rtf" | "txt" | "md" => "DOC",
        "xls" | "xlsx" | "csv" | "ods" => "XLS",
        "ppt" | "pptx" | "odp" => "PPT",
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" | "bmp" | "heic" => "IMG",
        "apk" | "deb" | "rpm" | "dmg" | "pkg" => "PKG",
        _ => "FILE",
    }
}

/// (free bytes, total bytes) of the volume that holds `path`; (0, 0) if the
/// OS call fails, which the UI renders as "measuring…".
#[cfg(windows)]
fn disk_stats(path: &std::path::Path) -> (u64, u64) {
    use std::os::windows::ffi::OsStrExt;
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let (mut free_to_caller, mut total, mut total_free) = (0u64, 0u64, 0u64);
    let ok = unsafe {
        windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_to_caller,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        (0, 0)
    } else {
        (total_free, total)
    }
}

#[cfg(unix)]
fn disk_stats(path: &std::path::Path) -> (u64, u64) {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return (0, 0);
    };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return (0, 0);
    }
    (
        st.f_bavail as u64 * st.f_frsize as u64,
        st.f_blocks as u64 * st.f_frsize as u64,
    )
}

#[cfg(not(any(windows, unix)))]
fn disk_stats(_path: &std::path::Path) -> (u64, u64) {
    (0, 0)
}

// ------------------------------------------------------------------ actions

fn submit_add(g: &Rc<Gui>) {
    // Manual Add Download does not open a detail window. The only opener
    // is the handoff dialog's Download action (`DetailSource::HandoffDownload`).
    let Some(ui) = g.ui.upgrade() else { return };
    let urls = ui.get_add_urls().to_string();
    if !urls
        .lines()
        .any(|line| !line.trim().is_empty() && !line.trim().starts_with('#'))
    {
        toast(g, "Enter at least one download URL".into(), true);
        return;
    }
    let dir = ui.get_add_dir().to_string();
    let hash = ui.get_add_hash().to_string();
    let segments = ui.get_add_segments();
    let transport = transport_from_index(ui.get_add_transport_index());
    let paused = ui.get_add_paused();

    let expected = if hash.trim().is_empty() {
        None
    } else {
        match ExpectedHash::parse(hash.trim()) {
            Ok(h) => Some(h),
            Err(e) => {
                toast(g, format!("bad checksum: {e}"), true);
                return;
            }
        }
    };

    let mut added = 0usize;
    let mut last_error = String::new();
    for line in urls.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut req = AddRequest::new(line);
        // G2: pass Some only for an explicit user choice (typed or browsed,
        // even when it equals the default). An untouched or cleared field
        // stays None so the engine may auto-route by category.
        if *g.add_dir_explicit.borrow() && !dir.trim().is_empty() {
            req = req.dest_dir(dir.trim());
        }
        if segments > 0 {
            req = req.segments(segments as u32);
        }
        if let Some(h) = expected.clone() {
            req = req.expected_hash(h);
        }
        req = req.transport_pref(transport).start_paused(paused);
        match g.engine.add(req) {
            Ok(_) => added += 1,
            Err(e) => last_error = e.to_string(),
        }
    }
    if added == 0 && !last_error.is_empty() {
        toast(g, last_error, true);
        return;
    }
    ui.set_add_urls(SharedString::default());
    ui.set_add_preview(SharedString::default());
    *g.add_dir_explicit.borrow_mut() = false;
    ui.set_view(0);
    toast(
        g,
        if added == 1 {
            "download added".to_string()
        } else {
            format!("{added} {}", ui.invoke_translate("downloads added".into()))
        },
        false,
    );
    refresh(g);
}

/// G2 Add-form preview: the provisional destination for the first URL,
/// computed with the engine's own classifier. Shown only while
/// auto-categorize is on and no explicit folder was chosen; the final name
/// is known only after the probe, so the text is marked provisional and
/// nothing is created on disk.
fn update_add_preview(g: &Rc<Gui>) {
    let Some(ui) = g.ui.upgrade() else { return };
    let settings = g.engine.settings();
    if !settings.auto_categorize || *g.add_dir_explicit.borrow() {
        ui.set_add_preview(SharedString::default());
        return;
    }
    let first = ui
        .get_add_urls()
        .to_string()
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("")
        .to_string();
    let preview = url::Url::parse(&first)
        .ok()
        .and_then(|u| util::filename_from_url(&u))
        .and_then(|name| {
            usagi::category::preview_dest(
                &settings.download_dir(),
                Some(&name),
                &settings.category_rules(),
            )
        })
        .map(|p| {
            format!(
                "{}: {}",
                ui.invoke_translate("Provisional destination".into()),
                util::display_path(&p)
            )
        })
        .unwrap_or_default();
    ui.set_add_preview(SharedString::from(preview));
}

/// Which sidebar heading the user toggled. General is not collapsible.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarSection {
    Library,
    Categories,
}

/// The two flags the sidebar renders. Kept as a pair so a failed save
/// restores both, not only the one the click changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SidebarFlags {
    library: bool,
    categories: bool,
}

fn sidebar_flags(settings: &Settings) -> SidebarFlags {
    SidebarFlags {
        library: settings.sidebar_library_collapsed,
        categories: settings.sidebar_categories_collapsed,
    }
}

fn copy_sidebar_flags(from: &Settings, to: &mut Settings) {
    to.sidebar_library_collapsed = from.sidebar_library_collapsed;
    to.sidebar_categories_collapsed = from.sidebar_categories_collapsed;
}

/// Restore-defaults payload. Every other field returns to `Settings::default`;
/// the collapse flags stay at whatever the engine currently has stored.
fn settings_for_reset(live: &Settings) -> Settings {
    let mut defaults = Settings::default();
    copy_sidebar_flags(live, &mut defaults);
    defaults
}

/// One heading click, applied to a copy of the live settings. Other fields,
/// including the other section's flag, are unchanged.
fn toggled_sidebar(current: &Settings, section: SidebarSection) -> Settings {
    let mut next = current.clone();
    match section {
        SidebarSection::Library => {
            next.sidebar_library_collapsed = !current.sidebar_library_collapsed;
        }
        SidebarSection::Categories => {
            next.sidebar_categories_collapsed = !current.sidebar_categories_collapsed;
        }
    }
    next
}

/// Flags the sidebar must show after an acknowledged save. `Err` means the
/// disk write failed, so the pre-click snapshot wins.
fn sidebar_after_save(before: SidebarFlags, saved: Result<SidebarFlags, ()>) -> SidebarFlags {
    match saved {
        Ok(flags) => flags,
        Err(()) => before,
    }
}

/// Persist one heading toggle through `Engine::save_settings` and paint the
/// sidebar from the result. A failed write toasts the error and puts the
/// previous flags back; the in-memory settings are left untouched by the
/// engine on that path.
fn apply_sidebar_toggle(g: &Rc<Gui>, section: SidebarSection) {
    let Some(ui) = g.ui.upgrade() else { return };
    let before_settings = g.engine.settings();
    let before = sidebar_flags(&before_settings);
    let next = toggled_sidebar(&before_settings, section);
    let shown = match g.engine.save_settings(next) {
        Ok(()) => sidebar_after_save(before, Ok(sidebar_flags(&g.engine.settings()))),
        Err(error) => {
            let shown = sidebar_after_save(before, Err(()));
            toast(g, error.to_string(), true);
            shown
        }
    };
    ui.set_sidebar_library_collapsed(shown.library);
    ui.set_sidebar_categories_collapsed(shown.categories);
}

fn save_settings(g: &Rc<Gui>) {
    let Some(ui) = g.ui.upgrade() else { return };
    let mut s = g.engine.settings();
    // A preceding immediate preference command may still be queued. Read the
    // current controls so Save cannot enqueue older appearance values after it.
    read_appearance(&ui, &mut s);
    s.max_concurrent_tasks = ui.get_set_max_tasks().max(1) as u32;
    s.segments_per_task = ui.get_set_segments().max(1) as u32;
    s.default_download_dir = ui.get_set_download_dir().to_string();
    s.transport_pref = transport_from_index(ui.get_set_transport_index());
    s.conflict_policy = match ui.get_set_conflict_index() {
        1 => ConflictPolicy::Overwrite,
        2 => ConflictPolicy::Fail,
        _ => ConflictPolicy::UniqueSuffix,
    };
    s.http3_enabled = ui.get_set_http3();
    s.use_system_proxy = ui.get_set_proxy();
    s.verify_checksums = ui.get_set_verify();
    s.auto_categorize = ui.get_set_auto_categorize();
    s.close_to_tray = ui.get_set_close_to_tray();
    s.minimize_to_tray = ui.get_set_minimize_to_tray();
    s.start_minimized = ui.get_set_start_minimized();
    s.start_with_windows = ui.get_set_start_with_windows();
    s.user_agent = ui.get_set_user_agent().to_string();
    s.chrome_extension_id = ui.get_set_chrome_extension_id().to_string();
    s.log_level = match ui.get_set_log_index() {
        0 => "off".into(),
        1 => "error".into(),
        2 => "warn".into(),
        4 => "debug".into(),
        5 => "trace".into(),
        _ => "info".into(),
    };
    // The form has no collapse controls. Re-read the live flags so a Save
    // cannot write an older snapshot over a toggle that already landed.
    copy_sidebar_flags(&g.engine.settings(), &mut s);
    s.sanitize();
    match g.engine.update_settings(s.clone()) {
        Ok(()) => {
            // G3 C2: the Run entry follows the saved toggle immediately.
            match crate::startup::sync(s.start_with_windows, s.start_minimized) {
                Ok(()) => toast(g, "settings saved".into(), false),
                Err(e) => toast(g, e, true),
            }
        }
        Err(e) => toast(g, e.to_string(), true),
    }
    push_settings_to_ui(&ui, &s, g.update_layout.as_ref());
    refresh_browser_rows(g);
    ui.set_http3_available(g.engine.net().http3_available());
}

// ------------------------------------------------- updater surface (W2)

/// Write the checks flag on the install base. Checks resolve on by default;
/// turning them off writes durable `off` content rather than deleting the
/// file (a deletion would resurrect the default). The scheduler reads the
/// resolved state in its poll: enabling starts the 6-hour clock but fires no
/// check by itself, and off stops new checks from starting while a check
/// already running finishes. On failure the checkbox reverts.
fn set_update_checks(g: &Rc<Gui>, enabled: bool) {
    let Some(layout) = g.update_layout.as_ref() else {
        if let Some(ui) = g.ui.upgrade() {
            ui.set_set_checks_enabled(!enabled);
        }
        toast(g, "cannot locate the install directory".into(), true);
        return;
    };
    match usagi::updater::set_checks_enabled(layout, enabled) {
        Ok(()) => {
            if let Some(ui) = g.ui.upgrade() {
                ui.set_set_checks_enabled(enabled);
            }
            toast(
                g,
                if enabled {
                    "update checks enabled"
                } else {
                    "update checks disabled"
                }
                .into(),
                false,
            );
        }
        Err(e) => {
            if let Some(ui) = g.ui.upgrade() {
                ui.set_set_checks_enabled(!enabled);
            }
            toast(g, e.to_string(), true);
        }
    }
}

// ------------------------------------------------- browser integration (items 1-3)

/// Push the browser rows' live state into the Settings view (item 2):
/// toggle from the HKCU host key, status tier from the journal, the
/// registration-broken fault from the startup repair. Called at setup,
/// after every toggle, after Save, and (throttled) from the poll while
/// the Settings view is open. The Chrome ID field shows the stored
/// setting, or the registered caller when the setting is empty (the
/// user registered from the host CLI, or an earlier session did).
fn refresh_browser_rows(g: &Rc<Gui>) {
    let Some(ui) = g.ui.upgrade() else { return };
    let settings = g.engine.settings();
    let chrome_id = if settings.chrome_extension_id.is_empty() {
        crate::browser::registered_caller_ext_id(&g.paths, "chrome").unwrap_or_default()
    } else {
        settings.chrome_extension_id.clone()
    };
    ui.set_set_chrome_extension_id(SharedString::from(chrome_id));
    ui.set_set_browser_hint_visible(!settings.browser_hint_dismissed);
    let broken = g.browser_broken.borrow();
    for browser in crate::browser::BROWSERS {
        let is_broken = broken.iter().any(|b| b == browser);
        let state = crate::browser::row_state(&g.paths, browser, is_broken);
        let enabled = !matches!(state.status, crate::browser::Status::NotRegistered);
        let (tier, last_ms) = match state.status {
            crate::browser::Status::NotRegistered => (1, 0),
            crate::browser::Status::Registered => (2, 0),
            crate::browser::Status::Connected { last_ms } => (3, last_ms),
        };
        let last_text = if last_ms > 0 {
            util::format_timestamp(last_ms)
        } else {
            String::new()
        };
        match browser {
            "chrome" => {
                ui.set_set_chrome_enabled(enabled);
                ui.set_set_chrome_status(tier);
                ui.set_set_chrome_last_connected(SharedString::from(last_text));
                ui.set_set_chrome_broken(state.broken);
            }
            _ => {
                ui.set_set_firefox_enabled(enabled);
                ui.set_set_firefox_status(tier);
                ui.set_set_firefox_last_connected(SharedString::from(last_text));
                ui.set_set_firefox_broken(state.broken);
            }
        }
    }
}

/// Throttled row refresh from the 150 ms poll, only while the
/// Settings view is open: an out-of-band register/unregister (the
/// host CLI, another UDM copy) shows up without a restart.
fn poll_browser_rows(g: &Rc<Gui>) {
    let Some(ui) = g.ui.upgrade() else { return };
    if ui.get_view() != 2 {
        return;
    }
    let mut at = g.browser_refresh_at.borrow_mut();
    if at.elapsed() < Duration::from_secs(2) {
        return;
    }
    *at = Instant::now();
    drop(at);
    refresh_browser_rows(g);
}

/// The toggle handler (item 1), an immediate callback like
/// checks-toggled: ON spawns the sibling udm-host.exe --register,
/// OFF spawns --unregister, never registering inside the GUI
/// process. Chrome needs a valid extension ID from the row's field;
/// an empty or illegal ID leaves the toggle off and writes nothing.
/// On a failed spawn the toggle reverts and the error is toasted.
fn set_browser_toggle(g: &Rc<Gui>, browser: &str, enabled: bool) {
    let Some(ui) = g.ui.upgrade() else { return };
    let revert = |g: &Rc<Gui>, browser: &str, enabled: bool| {
        if let Some(ui) = g.ui.upgrade() {
            match browser {
                "chrome" => ui.set_set_chrome_enabled(!enabled),
                _ => ui.set_set_firefox_enabled(!enabled),
            }
        }
    };
    let ext_id = if browser == "chrome" {
        let id = ui.get_set_chrome_extension_id().to_string();
        if enabled && !crate::browser::valid_extension_id(&id) {
            revert(g, browser, enabled);
            toast(
                g,
                "enter a Chrome extension ID first - copy it from the extension options page"
                    .into(),
                true,
            );
            return;
        }
        id
    } else {
        crate::browser::FIREFOX_EXTENSION_ID.to_string()
    };
    let result = if enabled {
        crate::browser::spawn_register(browser, &ext_id)
    } else {
        crate::browser::spawn_unregister(browser)
    };
    if let Err(e) = result {
        revert(g, browser, enabled);
        toast(g, e, true);
        return;
    }
    // Persist the Chrome ID that just registered, so the field
    // survives a restart even without a Save.
    if browser == "chrome" && enabled {
        let mut settings = g.engine.settings();
        if settings.chrome_extension_id != ext_id {
            settings.chrome_extension_id = ext_id;
            if let Err(e) = g.engine.update_settings(settings) {
                tracing::warn!(error = %e, "could not persist the Chrome extension ID");
            }
        }
    }
    apply_browser_session_effect(g);
    refresh_browser_rows(g);
    // Fixed strings, so the toast goes through the i18n catalog
    // (a format!() interpolation would miss it and stay English).
    toast(
        g,
        match (browser, enabled) {
            ("chrome", true) => "Chrome handoff registered".to_string(),
            ("chrome", false) => "Chrome handoff unregistered".to_string(),
            (_, true) => "Firefox handoff registered".to_string(),
            (_, false) => "Firefox handoff unregistered".to_string(),
        },
        false,
    );
}

/// Running-session effect (item 1), exactly this mechanism: keep both
/// boot conditions. After a successful register, if the lease is
/// Some(n>0) and no coordinator is up, start it the same way startup
/// does (never a second pipe, never rotate the pairing secret outside
/// start). If a coordinator is up, hand it the freshly read caller
/// list; pairing refuses a removed caller on the next connection. A
/// stored null lease stays fully inert: the key is written, the pipe
/// does not start, and the row promises no live connection.
fn apply_browser_session_effect(g: &Rc<Gui>) {
    let lease = match g.engine.settings().handoff_lease_secs {
        Some(n) if n > 0 => n,
        _ => return,
    };
    let _ = lease;
    if g.handoff.borrow().is_none() {
        if let Some(coordinator) = crate::handoff_boot::start(&g.paths, g.engine.clone()) {
            *g.handoff.borrow_mut() = Some(coordinator);
        }
    } else if let Some(coordinator) = g.handoff.borrow().as_ref() {
        let callers = usagi::handoff::read_callers(&g.paths);
        coordinator.set_allowed_callers(callers);
    }
}

/// The one-time first-run hint: dismissing it persists the dismissal
/// at once (it is not a Save-gated setting).
fn dismiss_browser_hint(g: &Rc<Gui>) {
    let mut settings = g.engine.settings();
    settings.browser_hint_dismissed = true;
    if let Err(e) = g.engine.update_settings(settings) {
        toast(g, e.to_string(), true);
        return;
    }
    refresh_browser_rows(g);
}

/// The manual check (the Check for Updates button): the same factored
/// check the CLI runs, on a background thread so the UI never blocks.
/// Unlike the automatic path it clears the offer and result at start, and
/// its completion sets the offer from the outcome kind. A click while any
/// check is in flight is refused by the shared single-flight guard. Both
/// paths rewrite the same window clock when they start.
fn start_update_check(g: &Rc<Gui>) {
    let Some(layout) = g.update_layout.clone() else {
        toast(g, "cannot locate the install directory".into(), true);
        return;
    };
    if g.update_check_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let Some(ui) = g.ui.upgrade() else {
        g.update_check_running.store(false, Ordering::SeqCst);
        return;
    };
    g.update_schedule
        .borrow_mut()
        .note_check_start(Instant::now());
    if let Ok(mut slot) = g.update_offered.lock() {
        *slot = None;
    }
    ui.set_update_check_busy(true);
    ui.set_update_available(false);
    ui.set_update_result(SharedString::default());
    let weak = g.ui.clone();
    let running = g.update_check_running.clone();
    let offered = g.update_offered.clone();
    std::thread::spawn(move || {
        let outcome = run_check_blocking(&layout);
        let _ = slint::invoke_from_event_loop(move || {
            if outcome.kind == CheckKind::Available {
                if let Ok(mut slot) = offered.lock() {
                    *slot = outcome.version.clone();
                }
            }
            let Some(ui) = weak.upgrade() else {
                running.store(false, Ordering::SeqCst);
                return;
            };
            ui.set_update_check_busy(false);
            ui.set_update_available(outcome.kind == CheckKind::Available);
            ui.set_update_result(ui.invoke_translate(SharedString::from(outcome.notice)));
            running.store(false, Ordering::SeqCst);
        });
    });
}

fn run_check_blocking(layout: &InstallLayout) -> usagi::updater::CheckOutcome {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    usagi::updater::run_check(layout, now_unix)
}

/// Recompute the tray update badge from the session offer: ON exactly
/// when a check has found a newer build this session, OFF otherwise.
/// Called at launch, on every check completion of any path, and after
/// apply. Display-only - no click behaviour, no toast, no settings
/// toggle.
fn refresh_update_badge(g: &Rc<Gui>) {
    let held = g
        .update_offered
        .lock()
        .map(|slot| slot.is_some())
        .unwrap_or(false);
    if let Some(tray) = g.tray.borrow_mut().as_mut() {
        tray.set_update_badge(held);
    }
}

/// The automatic-check scheduler. It runs in the 150 ms timer callback
/// itself, never inside `tray_poll` and never gated on the tray handle
/// existing, so a launch with no tray still performs its startup and
/// 6-hour checks. Each tick reads the resolved flag, observes the window
/// clock, recomputes the badge when a check has just completed, and fires
/// a check when the pure due rule says so.
fn poll_update_checks(g: &Rc<Gui>) {
    let resolved_on = g
        .update_layout
        .as_ref()
        .is_some_and(|layout| usagi::updater::checks_enabled(layout));
    let running = g.update_check_running.load(Ordering::SeqCst);

    // A check completion (running true->false) recomputes the badge from
    // the session offer. The completion closure cannot touch the tray
    // itself (the GUI handle is not `Send`), so the poll detects the
    // transition.
    let was_running = *g.was_check_running.borrow();
    if was_running && !running {
        refresh_update_badge(g);
    }
    *g.was_check_running.borrow_mut() = running;

    let now = Instant::now();
    let due = {
        let mut schedule = g.update_schedule.borrow_mut();
        schedule.observe(now, resolved_on);
        schedule.due(now, running, resolved_on)
    };
    if due {
        start_automatic_check(g);
    }
}

/// The automatic check: the same factored check and single-flight guard
/// as the manual path, but it does NOT clear an offer or result already
/// on screen. An Available outcome sets the offer; an AlreadyCurrent
/// outcome clears a stale one (the build was updated another way); any
/// other kind leaves the UI as it is. It never applies; apply stays a
/// manual act.
fn start_automatic_check(g: &Rc<Gui>) {
    let Some(layout) = g.update_layout.clone() else {
        return;
    };
    if g.update_check_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let Some(ui) = g.ui.upgrade() else {
        g.update_check_running.store(false, Ordering::SeqCst);
        return;
    };
    g.update_schedule
        .borrow_mut()
        .note_check_start(Instant::now());
    ui.set_update_check_busy(true);
    let weak = g.ui.clone();
    let running = g.update_check_running.clone();
    let offered = g.update_offered.clone();
    std::thread::spawn(move || {
        let outcome = run_check_blocking(&layout);
        let _ = slint::invoke_from_event_loop(move || {
            match outcome.kind {
                CheckKind::Available => {
                    if let Ok(mut slot) = offered.lock() {
                        *slot = outcome.version.clone();
                    }
                    if let Some(ui) = weak.upgrade() {
                        ui.set_update_available(true);
                        ui.set_update_result(
                            ui.invoke_translate(SharedString::from(outcome.notice.clone())),
                        );
                    }
                }
                CheckKind::AlreadyCurrent => {
                    if let Ok(mut slot) = offered.lock() {
                        *slot = None;
                    }
                    if let Some(ui) = weak.upgrade() {
                        ui.set_update_available(false);
                    }
                }
                _ => {}
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_update_check_busy(false);
            }
            running.store(false, Ordering::SeqCst);
        });
    });
}

/// The explicit Apply action: download the offered build, verify it
/// (SHA256SUMS hash, then the Authenticode + SPKI gate on every
/// executable), swap it into the install tree, then relaunch into the
/// new build. Runs on a worker thread; the outcome returns through the
/// event loop. A failure changes nothing outside the staging directory
/// (a failed swap rolls back) and is reported in the update result
/// text; the offer stands so the user can retry.
fn apply_update(g: &Rc<Gui>) {
    let Some(layout) = g.update_layout.clone() else {
        toast(g, "cannot locate the install directory".into(), true);
        return;
    };
    let version = g.update_offered.lock().ok().and_then(|slot| slot.clone());
    let Some(version) = version else {
        if let Some(ui) = g.ui.upgrade() {
            ui.set_update_available(false);
        }
        toast(g, "no update is available".into(), false);
        return;
    };
    if g.update_apply_running
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    if let Some(ui) = g.ui.upgrade() {
        ui.set_update_check_busy(true);
    }
    let weak = g.ui.clone();
    let running = g.update_apply_running.clone();
    let relaunch = g.relaunch_after_apply.clone();
    std::thread::spawn(move || {
        let result = usagi::updater::download_and_install(&layout, &version);
        let _ = slint::invoke_from_event_loop(move || {
            running.store(false, Ordering::SeqCst);
            let Some(ui) = weak.upgrade() else { return };
            ui.set_update_check_busy(false);
            match result {
                Ok(()) => {
                    // main.rs starts the new exe only after the engine
                    // has shut down, so both databases are released
                    // first.
                    relaunch.store(true, Ordering::SeqCst);
                    let _ = slint::quit_event_loop();
                }
                Err(f) => {
                    ui.set_update_result(ui.invoke_translate(SharedString::from(f.message())));
                }
            }
        });
    });
}

/// OS dark-mode preference for the Auto theme (Windows: the per-user
/// AppsUseLightTheme registry value; the app reads it, never writes it).
/// Falls back to dark, the product default, when it cannot be read.
#[cfg(windows)]
fn os_prefers_dark() -> bool {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, KEY_READ, REG_DWORD,
    };
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    unsafe {
        let mut key: HKEY = 0;
        let sub = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize");
        if RegOpenKeyExW(HKEY_CURRENT_USER, sub.as_ptr(), 0, KEY_READ, &mut key) != ERROR_SUCCESS {
            return true;
        }
        let name = wide("AppsUseLightTheme");
        let mut data: u32 = 0;
        let mut ty: u32 = 0;
        let mut len: u32 = std::mem::size_of::<u32>() as u32;
        let rc = RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut ty,
            &mut data as *mut u32 as *mut u8,
            &mut len,
        );
        RegCloseKey(key);
        if rc != ERROR_SUCCESS || ty != REG_DWORD {
            return true;
        }
        data == 0
    }
}

#[cfg(not(windows))]
fn os_prefers_dark() -> bool {
    true
}

/// Resolve the Auto theme preference into theme-index from the OS scheme
/// and re-apply. No-op for explicit dark/light preferences.
fn resolve_auto_theme(ui: &AppWindow, settings: &Settings) {
    if settings.theme == "auto" {
        ui.set_theme_index(if os_prefers_dark() { 0 } else { 1 });
        ui.invoke_apply_native_theme();
        crate::tray::set_menu_theme(ui.get_theme_index() == 0);
    }
}

fn read_appearance(ui: &AppWindow, settings: &mut Settings) {
    settings.theme = match ui.get_theme_pref() {
        1 => "light",
        2 => "auto",
        _ => "dark",
    }
    .into();
    settings.language = if ui.get_language_index() == 1 {
        "vi"
    } else {
        "en"
    }
    .into();
    settings.accent = ["blue", "green", "orange", "pink", "purple"]
        .get(ui.get_accent_index() as usize)
        .unwrap_or(&"blue")
        .to_string();
}

pub fn push_settings_to_ui(ui: &AppWindow, s: &Settings, update_layout: Option<&InstallLayout>) {
    ui.set_theme_pref(match s.theme.as_str() {
        "light" => 1,
        "auto" => 2,
        _ => 0,
    });
    ui.set_theme_index(if s.theme == "light" { 1 } else { 0 });
    if s.theme == "auto" {
        ui.set_theme_index(if os_prefers_dark() { 0 } else { 1 });
    }
    ui.set_language_index(if s.language == "vi" { 1 } else { 0 });
    ui.set_accent_index(
        ["blue", "green", "orange", "pink", "purple"]
            .iter()
            .position(|a| *a == s.accent)
            .unwrap_or(0) as i32,
    );
    ui.invoke_apply_native_theme();
    crate::tray::set_menu_theme(ui.get_theme_index() == 0);
    ui.set_set_max_tasks(s.max_concurrent_tasks as i32);
    ui.set_set_segments(s.segments_per_task as i32);
    ui.set_set_download_dir(SharedString::from(s.default_download_dir.clone()));
    ui.set_set_transport_index(match s.transport_pref {
        TransportPref::Auto => 0,
        TransportPref::PreferHttp3 => 1,
        TransportPref::Http3Only => 2,
        TransportPref::NoHttp3 => 3,
    });
    ui.set_set_conflict_index(match s.conflict_policy {
        ConflictPolicy::UniqueSuffix => 0,
        ConflictPolicy::Overwrite => 1,
        ConflictPolicy::Fail => 2,
    });
    ui.set_set_http3(s.http3_enabled);
    ui.set_set_proxy(s.use_system_proxy);
    ui.set_set_verify(s.verify_checksums);
    ui.set_set_auto_categorize(s.auto_categorize);
    ui.set_set_close_to_tray(s.close_to_tray);
    ui.set_set_minimize_to_tray(s.minimize_to_tray);
    ui.set_set_start_minimized(s.start_minimized);
    // G3 C2: the startup toggle mirrors the registry as it actually is,
    // and the location warning follows the running executable's folder.
    ui.set_set_start_with_windows(crate::startup::is_enabled());
    ui.set_set_startup_location_warn(crate::startup::exe_in_temp_or_downloads());
    // Wiring W2: the checks checkbox mirrors the flag file as it actually
    // is (like the startup toggle mirrors the registry). Save re-mirrors
    // it here but never writes it: the flag is not a Settings field.
    ui.set_set_checks_enabled(
        update_layout.is_some_and(|layout| usagi::updater::checks_enabled(layout)),
    );
    ui.set_set_user_agent(SharedString::from(s.user_agent.clone()));
    ui.set_set_log_index(match s.log_level.as_str() {
        "off" => 0,
        "error" => 1,
        "warn" => 2,
        "debug" => 4,
        "trace" => 5,
        _ => 3,
    });
    ui.set_sidebar_library_collapsed(s.sidebar_library_collapsed);
    ui.set_sidebar_categories_collapsed(s.sidebar_categories_collapsed);
}

fn transport_from_index(i: i32) -> TransportPref {
    match i {
        1 => TransportPref::PreferHttp3,
        2 => TransportPref::Http3Only,
        3 => TransportPref::NoHttp3,
        _ => TransportPref::Auto,
    }
}

fn pick_into(g: &Rc<Gui>, add_form: bool) {
    let Some(ui) = g.ui.upgrade() else { return };
    let dialog = rfd::FileDialog::new().set_title(
        ui.invoke_translate("Choose a download folder".into())
            .as_str(),
    );
    let Some(dir) = dialog.pick_folder() else {
        return;
    };
    let text = SharedString::from(dir.to_string_lossy().to_string());
    if add_form {
        // G2: browsing for a folder is an explicit destination choice, even
        // when the user picks the folder that equals the default.
        *g.add_dir_explicit.borrow_mut() = true;
        ui.set_add_dir(text);
        update_add_preview(g);
    } else {
        ui.set_set_download_dir(text);
    }
}

fn copy_task_url(g: &Rc<Gui>, id: u64) {
    let Some(s) = g.engine.task(id) else { return };
    copy_to_clipboard(&s.url);
    toast(g, "URL copied to clipboard".into(), false);
}

pub fn copy_to_clipboard(text: &str) {
    #[cfg(target_os = "windows")]
    {
        use std::io::Write;
        if let Ok(mut child) = std::process::Command::new("clip")
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        use std::io::Write;
        let cmd = if std::process::Command::new("which")
            .arg("wl-copy")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
        {
            "wl-copy"
        } else {
            "xclip"
        };
        let mut args = vec![];
        if cmd == "xclip" {
            args.push("-selection");
            args.push("clipboard");
        }
        if let Ok(mut child) = std::process::Command::new(cmd)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .spawn()
        {
            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
        }
    }
}

fn open_task_file(g: &Rc<Gui>, id: u64, folder: bool) {
    let _ = open_task_path(g, id, folder);
}

/// Open the task's folder. `Some` is the message already shown as a toast,
/// so the detail window can repeat it in its own action row.
pub(crate) fn open_task_folder(g: &Rc<Gui>, id: u64) -> Option<String> {
    open_task_path(g, id, true)
}

fn open_task_path(g: &Rc<Gui>, id: u64, folder: bool) -> Option<String> {
    let Some(s) = g.engine.task(id) else {
        return None;
    };
    if s.final_path.is_empty() {
        return None;
    }
    let path = std::path::PathBuf::from(&s.final_path);
    if folder {
        let dir = path.parent().unwrap_or(&path).to_path_buf();
        if dir.exists() {
            shell_open(&dir, false);
            None
        } else {
            let message = format!("folder no longer exists: {}", dir.display());
            toast(g, message.clone(), true);
            Some(message)
        }
    } else if path.exists() {
        shell_open(&path, false);
        None
    } else {
        // A deleted completed file must be reported, not silently
        // papered over by opening its parent folder.
        let message = format!("file no longer exists: {}", path.display());
        toast(g, message.clone(), true);
        Some(message)
    }
}

/// Open a web URL in the default browser.
pub fn shell_open_url(url: &str) {
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(url).spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    }
}

/// Open a path with the desktop shell (Explorer / xdg-open / open).
pub fn shell_open(path: &std::path::Path, select: bool) {
    #[cfg(target_os = "windows")]
    {
        let mut cmd = std::process::Command::new("explorer");
        if select {
            cmd.arg(format!("/select,{}", path.display()));
        } else {
            cmd.arg(path);
        }
        let _ = cmd.spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = select;
        let _ = std::process::Command::new("xdg-open").arg(path).spawn();
    }
}

/// Headless Slint platform for GUI tests. Each test thread installs its
/// own context: the context slot is thread-local. `set_platform` also
/// stores the proxy from that call in a process-wide cell, and a proxy
/// there makes every other thread fail component creation ("initialized
/// in another thread"). `spawn_local` asks for a proxy again later, so
/// the install call returns none and the thread-local flag still gates
/// the later ask. Each component gets its own software window.
#[cfg(test)]
pub(crate) mod test_platform {
    use std::cell::Cell;
    use std::cell::RefCell;
    use std::rc::Rc;

    use slint::platform::software_renderer::{MinimalSoftwareWindow, RepaintBufferType};
    use slint::platform::{EventLoopProxy, Platform, WindowAdapter};

    thread_local! {
        static LATEST: RefCell<Option<Rc<MinimalSoftwareWindow>>> = const { RefCell::new(None) };
        static PROXY: Cell<bool> = const { Cell::new(true) };
        static INSTALLING: Cell<bool> = const { Cell::new(false) };
    }

    struct Headless;

    impl Platform for Headless {
        fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
            let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
            LATEST.with(|slot| *slot.borrow_mut() = Some(window.clone()));
            Ok(window)
        }

        fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
            if INSTALLING.with(|flag| flag.get()) {
                return None;
            }
            if PROXY.with(|flag| flag.get()) {
                Some(Box::new(DropWake))
            } else {
                None
            }
        }
    }

    /// Accepts the wake `spawn_local` sends and does not run it here.
    /// Running it inline re-enters the future on this stack. The real
    /// winit backend queues the same wake onto the event loop.
    struct DropWake;

    impl EventLoopProxy for DropWake {
        fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
            Ok(())
        }

        fn invoke_from_event_loop(
            &self,
            _event: Box<dyn FnOnce() + Send>,
        ) -> Result<(), slint::EventLoopError> {
            Ok(())
        }
    }

    pub fn install() {
        INSTALLING.with(|flag| flag.set(true));
        let result = slint::platform::set_platform(Box::new(Headless));
        INSTALLING.with(|flag| flag.set(false));
        // Already set on this thread. Another thread's context does not
        // occupy this slot, so a second thread still installs its own.
        let _ = result;
    }

    pub fn latest_window() -> Rc<MinimalSoftwareWindow> {
        LATEST.with(|slot| {
            slot.borrow()
                .clone()
                .expect("a component created a window on this thread")
        })
    }

    /// `false` makes `slint::spawn_local` fail, which is what happens
    /// when no event-loop proxy exists yet.
    pub fn set_event_loop_proxy(enabled: bool) {
        PROXY.with(|flag| flag.set(enabled));
    }
}

/// A `Gui` with no main window, for handoff tests. The dialog builds
/// its own component. No tray and no registry calls.
#[cfg(test)]
pub(crate) fn gui_for_test(engine: Arc<Engine>, handoff: Option<Arc<Coordinator>>) -> Rc<Gui> {
    let tasks = Rc::new(VecModel::<TaskRow>::default());
    let endpoints = Rc::new(VecModel::<EndpointRow>::default());
    let category_nav = Rc::new(VecModel::<CategoryNav>::default());
    Rc::new(Gui {
        engine: engine.clone(),
        tasks,
        selected: RefCell::new(BTreeSet::new()),
        anchor: RefCell::new(None),
        focused: RefCell::new(None),
        tray: RefCell::new(None),
        endpoints,
        ui: Weak::default(),
        slow: RefCell::new(Instant::now() - Duration::from_millis(SLOW_REFRESH_MS)),
        toast_until: RefCell::new(Instant::now()),
        add_dir_explicit: RefCell::new(false),
        tray_summary_at: RefCell::new(Instant::now() - Duration::from_secs(2)),
        tray_summary_key: RefCell::new(String::new()),
        minimized_seen: RefCell::new(false),
        was_minimized: RefCell::new(false),
        repaint_pulses: RefCell::new(0),
        popup: RefCell::new(None),
        popup_open: RefCell::new(false),
        popup_area: RefCell::new(None),
        handoff: RefCell::new(handoff),
        handoff_ui: RefCell::new(crate::handoff_dialog::DialogSession::default()),
        detail: RefCell::new(crate::detail_window::DetailSession::default()),
        paths: engine.paths().clone(),
        browser_broken: RefCell::new(Vec::new()),
        browser_refresh_at: RefCell::new(Instant::now()),
        update_layout: None,
        update_check_running: Arc::new(AtomicBool::new(false)),
        relaunch_after_apply: Arc::new(AtomicBool::new(false)),
        update_schedule: RefCell::new(UpdateSchedule::new(Instant::now())),
        was_check_running: RefCell::new(false),
        update_offered: Arc::new(std::sync::Mutex::new(None)),
        update_apply_running: Arc::new(AtomicBool::new(false)),
        category_nav,
        category_cache: RefCell::new(CategoryCache::new()),
        notice_dismissed: RefCell::new(BTreeSet::new()),
        notice_cache: RefCell::new(Vec::new()),
        notice_polled_at: RefCell::new(
            Instant::now()
                .checked_sub(NOTICE_POLL)
                .unwrap_or_else(Instant::now),
        ),
    })
}

#[cfg(all(test, windows))]
mod s2_render_tests {
    use super::test_platform;
    use super::*;
    use usagi::store::records::{TransportKind, TransportSummary};

    // Exercise the real row/detail bindings and software renderer at the minimum
    // width: family history must not silently elide the second family.
    #[test]
    fn s2_mixed_family_detail_headless_render() {
        test_platform::install();
        let ui = AppWindow::new().unwrap();
        let window = test_platform::latest_window();
        ui.global::<crate::I18n>().on_translate(|key, language| {
            if language == 1 {
                crate::i18n::vi(key.as_str()).into()
            } else {
                key
            }
        });
        let mut task = TaskSnapshot {
            id: 1,
            url: "https://localhost/proof.bin".into(),
            filename: "Mixed transport proof.bin".into(),
            dest_dir: "C:/proof/downloads".into(),
            state: TaskState::Downloading,
            state_label: "Downloading".into(),
            total_size: Some(262281),
            received: 131140,
            fraction: Some(0.5),
            speed_bps: 65536.0,
            eta_secs: 2.0,
            segments_done: 2,
            segments_total: 4,
            active_requests: 2,
            error: None,
            transport: None,
            transport_summary: TransportSummary::Mixed(vec![
                TransportKind::Http3,
                TransportKind::Http2,
            ]),
            paths: vec![
                usagi::net::lane::PathSnapshot {
                    generation: 1,
                    family: usagi::net::http12::Family::V4,
                    allocation_epoch: 0,
                    active_ranges: 1,
                    observed_protocols: vec![TransportKind::Http3],
                    logical_url: "https://localhost/proof.bin".into(),
                    endpoint: Some("127.0.0.1:8443".parse().unwrap()),
                },
                usagi::net::lane::PathSnapshot {
                    generation: 1,
                    family: usagi::net::http12::Family::V6,
                    allocation_epoch: 0,
                    active_ranges: 1,
                    observed_protocols: vec![TransportKind::Http2],
                    logical_url: "https://localhost/proof.bin".into(),
                    endpoint: Some("[::1]:443".parse().unwrap()),
                },
            ],
            created_at: 0,
            started_at: Some(0),
            finished_at: None,
            final_path: "C:/proof/downloads/proof.bin".into(),
            expected_hash: None,
            computed_hash: None,
            attempts: 0,
            segment_retry: None,
            verify_bps: 0.0,
            accept_ranges: true,
            ranges_observed: true,
        };
        ui.show().unwrap();
        ui.set_category_nav(ModelRc::new(VecModel::from(vec![
            CategoryNav {
                id: "".into(),
                label: "All categories".into(),
                badge: "6".into(),
                glyph: "*".into(),
                kind: 0,
            },
            CategoryNav {
                id: "documents".into(),
                label: "Documents".into(),
                badge: "2".into(),
                glyph: "D".into(),
                kind: 0,
            },
            CategoryNav {
                id: "music".into(),
                label: "Music".into(),
                badge: "1".into(),
                glyph: "M".into(),
                kind: 0,
            },
            CategoryNav {
                id: "".into(),
                label: "Finished".into(),
                badge: "1".into(),
                glyph: "\u{2713}".into(),
                kind: 1,
            },
            CategoryNav {
                id: "".into(),
                label: "Unfinished".into(),
                badge: "5".into(),
                glyph: "\u{2026}".into(),
                kind: 2,
            },
        ])));
        let mut palette_samples = [None, None];
        for vietnamese in [false, true] {
            for theme in [0, 1] {
                for (width, height) in [(800u32, 640u32), (1040, 720), (1600, 900)] {
                    ui.set_language_index(if vietnamese { 1 } else { 0 });
                    ui.set_theme_index(theme);
                    ui.set_theme_pref(theme);
                    ui.invoke_apply_native_theme();
                    crate::tray::set_menu_theme(ui.get_theme_index() == 0);
                    ui.set_tasks(ModelRc::new(VecModel::from(vec![row_from(
                        &task, vietnamese, false,
                    )])));
                    let detail = detail_from(&task, vietnamese);
                    assert_eq!(detail.transport_label.lines().count(), 2);
                    assert!(detail.transport_label.contains("IPv4: H3"));
                    assert!(detail.transport_label.contains("IPv6: H2"));
                    ui.set_detail(detail);
                    ui.set_detail_visible(true);
                    window.set_size(slint::PhysicalSize::new(width, height));
                    window.request_redraw();
                    let mut pixels = vec![slint::Rgb8Pixel::default(); (width * height) as usize];
                    assert!(window.draw_if_needed(|renderer| {
                        renderer.render(&mut pixels, width as usize);
                    }));
                    if !vietnamese && width == 800 {
                        palette_samples[theme as usize] = Some(pixels[0]);
                    }
                    if let Some(root) = std::env::var_os("UDM_S2_RENDER_DIR") {
                        let root = std::path::PathBuf::from(root);
                        std::fs::create_dir_all(&root).unwrap();
                        let bytes: Vec<_> = pixels.iter().flat_map(|p| [p.r, p.g, p.b]).collect();
                        image::save_buffer(
                            root.join(format!(
                                "mixed-{}-{theme}-{width}.png",
                                if vietnamese { "vi" } else { "en" }
                            )),
                            &bytes,
                            width,
                            height,
                            image::ColorType::Rgb8,
                        )
                        .unwrap();
                    }
                }
            }
        }
        assert_ne!(
            palette_samples[0], palette_samples[1],
            "dark and light renders must differ"
        );
        task.paths[0].active_ranges = 0;
        task.paths[0].observed_protocols.push(TransportKind::Http2);
        let detail = detail_from(&task, false);
        assert!(detail.transport_label.contains("IPv4: H3/H2 (ready)"));
        assert!(detail.transport_label.contains("IPv6: H2 (active)"));
        ui.hide().unwrap();
    }
}

/// The automatic-check due rule and window clock, as pure logic. These run
/// on every platform (no GUI, no tray, no network): the scheduler's whole
/// decision is `update_check_due` plus the three clock-rewrite events.
#[cfg(test)]
mod update_schedule_tests {
    use super::*;

    const SIX_H: Duration = Duration::from_secs(6 * 60 * 60);
    const ONE_H: Duration = Duration::from_secs(60 * 60);
    const ONE_S: Duration = Duration::from_secs(1);

    #[test]
    fn off_is_never_due() {
        let t0 = Instant::now();
        // Even with the startup flag owed and a long window elapsed, OFF is
        // never due.
        assert!(!update_check_due(t0, t0, true, false, false));
        assert!(!update_check_due(t0 + SIX_H * 2, t0, true, false, false));
        assert!(!update_check_due(t0 + SIX_H * 2, t0, false, false, false));
        // Arming while OFF never owes a startup check.
        let mut s = UpdateSchedule::new(t0);
        s.observe(t0, false);
        assert!(!s.startup_pending);
        assert!(!s.due(t0 + SIX_H * 2, false, false));
    }

    #[test]
    fn on_at_launch_due_exactly_once_at_ready() {
        let t0 = Instant::now();
        let mut s = UpdateSchedule::new(t0);
        // The first ready evaluation with checks ON owes the startup check.
        s.observe(t0, true);
        assert!(s.startup_pending);
        assert!(s.due(t0, false, true));
        // The start consumes the flag and rewrites the clock.
        s.note_check_start(t0);
        assert!(!s.startup_pending);
        assert!(!s.due(t0, false, true));
        // No second startup firing inside the window.
        assert!(!s.due(t0 + ONE_H, false, true));
        assert!(!s.due(t0 + SIX_H - ONE_S, false, true));
    }

    #[test]
    fn enable_after_off_launch_not_due_until_enable_plus_six_hours() {
        let t0 = Instant::now();
        let mut s = UpdateSchedule::new(t0);
        s.observe(t0, false); // launch OFF
        assert!(!s.startup_pending);
        let t1 = t0 + ONE_H; // enable an hour later
        s.observe(t1, true); // OFF->ON flip rewrites the clock to t1
        assert!(!s.startup_pending); // enabling never fires a check by itself
        assert!(!s.due(t1, false, true));
        assert!(!s.due(t1 + SIX_H - ONE_S, false, true));
        assert!(s.due(t1 + SIX_H, false, true));
    }

    #[test]
    fn manual_start_resets_clock_and_clears_startup_pending() {
        let t0 = Instant::now();
        let mut s = UpdateSchedule::new(t0);
        s.observe(t0, true);
        assert!(s.startup_pending);
        // A manual start before the startup tick consumes the flag and
        // resets the window clock to the start instant.
        let tm = t0 + Duration::from_secs(120);
        s.note_check_start(tm);
        assert!(!s.startup_pending);
        assert_eq!(s.clock_base, tm);
        assert!(!s.due(tm + SIX_H - ONE_S, false, true));
        assert!(s.due(tm + SIX_H, false, true));
    }

    #[test]
    fn running_is_never_due() {
        let t0 = Instant::now();
        assert!(!update_check_due(t0, t0, true, true, true));
        assert!(!update_check_due(t0 + SIX_H * 2, t0, true, true, true));
        assert!(!update_check_due(t0 + SIX_H * 2, t0, false, true, true));
    }

    #[test]
    fn at_or_over_six_hours_since_clock_base_is_due() {
        let t0 = Instant::now();
        assert!(!update_check_due(
            t0 + SIX_H - ONE_S,
            t0,
            false,
            false,
            true
        ));
        assert!(update_check_due(t0 + SIX_H, t0, false, false, true));
        assert!(update_check_due(t0 + SIX_H + ONE_S, t0, false, false, true));
    }

    /// Rev.3 trace (i): start OFF at T0, flip ON at T1 at least 6h after T0.
    /// The flip rewrites the clock, so it is NOT due at T1 and due at T1+6h.
    #[test]
    fn rev3_trace_i_off_then_on_after_six_hours() {
        let t0 = Instant::now();
        let mut s = UpdateSchedule::new(t0);
        s.observe(t0, false);
        let t1 = t0 + SIX_H + ONE_H;
        s.observe(t1, true);
        assert!(!s.due(t1, false, true), "not due at the enable instant");
        assert!(
            s.due(t1 + SIX_H, false, true),
            "due a full window after enable"
        );
    }

    /// Rev.3 trace (ii): start OFF at T0, flip ON at T1 under 6h after T0.
    /// Due at T1+6h, NOT at T0+6h: the older window must not resurrect.
    #[test]
    fn rev3_trace_ii_off_then_on_before_six_hours() {
        let t0 = Instant::now();
        let mut s = UpdateSchedule::new(t0);
        s.observe(t0, false);
        let t1 = t0 + ONE_H;
        s.observe(t1, true);
        assert!(
            !s.due(t0 + SIX_H, false, true),
            "the old T0 window must not resurrect"
        );
        assert!(
            s.due(t1 + SIX_H, false, true),
            "due a full window after enable"
        );
    }
}

/// Sidebar category axis, Finished / Unfinished, one-pass counts, and
/// expired-handoff notice placement. Pure: no window and no engine.
#[cfg(test)]
mod category_view_tests {
    use std::collections::BTreeSet;

    use usagi::category::default_categories;
    use usagi::handoff::DiscoveryEntry;
    use usagi::store::records::TaskState;

    use super::{
        badge_text, category_bucket, category_item_label, collect_expired_notices, notice_row,
        notice_row_key, notices_in_view, parse_key, parse_notice_row_key, pass_task_list,
        status_matches, CategoryCache, CategoryRule, ExpiredNotice, ViewTask, EXPIRED_HANDOFF_TEXT,
        LIBRARY_ACTIVE, LIBRARY_ALL, LIBRARY_FAILED, LIBRARY_FINISHED, LIBRARY_UNFINISHED,
    };

    fn view<'a>(filename: &'a str, url: &'a str, state: TaskState) -> ViewTask<'a> {
        ViewTask {
            filename,
            url,
            state,
        }
    }

    fn rule(id: &str, folder: &str, suffixes: &[&str]) -> CategoryRule {
        CategoryRule {
            id: id.to_string(),
            folder: folder.to_string(),
            suffixes: suffixes
                .iter()
                .map(|suffix| (*suffix).to_string())
                .collect(),
        }
    }

    fn mixed() -> Vec<ViewTask<'static>> {
        vec![
            view("report.pdf", "https://ex/report.pdf", TaskState::Completed),
            view("song.mp3", "https://ex/song.mp3", TaskState::Failed),
            view("song.mp3", "https://ex/other", TaskState::Downloading),
            view("clip.mp4", "https://ex/clip.mp4", TaskState::Paused),
            view("", "https://ex/unknown", TaskState::Queued),
            view("notes.txt", "https://ex/notes.txt", TaskState::Canceled),
        ]
    }

    fn visible_names(tasks: &[ViewTask<'_>], indexes: &[usize]) -> Vec<String> {
        indexes
            .iter()
            .map(|&index| format!("{}|{}", tasks[index].filename, tasks[index].url))
            .collect()
    }

    fn discovery(caller: &str, tx: &str, expired: bool) -> DiscoveryEntry {
        DiscoveryEntry {
            caller: caller.to_string(),
            tx: tx.to_string(),
            intent: None,
            created_at_ms: 10,
            expires_at_ms: Some(20),
            expired,
        }
    }

    /// The same gate refresh uses: notices are listed only in All on both axes,
    /// and a dismissed (caller, tx) stays out.
    fn shown_notice_keys(
        library: i32,
        category: &str,
        entries: &[DiscoveryEntry],
        dismissed: &BTreeSet<(String, String)>,
    ) -> Vec<String> {
        if !notices_in_view(library, category) {
            return Vec::new();
        }
        collect_expired_notices(entries, dismissed)
            .iter()
            .map(|notice| notice_row_key(&notice.caller, &notice.tx))
            .collect()
    }

    #[test]
    fn empty_filename_and_none_classification_land_in_other() {
        let rules = default_categories();
        assert_eq!(category_bucket("", &rules), "other");
        assert_eq!(category_bucket("readme", &rules), "other");
        assert_eq!(category_bucket("a.pdf", &rules), "documents");
        let no_other: Vec<_> = rules
            .into_iter()
            .filter(|rule| rule.id != "other")
            .collect();
        assert!(usagi::category::classify("readme", &no_other).is_none());
        assert_eq!(category_bucket("readme", &no_other), "other");
        assert_eq!(category_bucket("", &no_other), "other");
        assert_eq!(category_bucket("a.pdf", &no_other), "documents");
    }

    #[test]
    fn unfinished_includes_failed_and_finished_is_completed() {
        let states = [
            TaskState::Queued,
            TaskState::Probing,
            TaskState::Downloading,
            TaskState::Verifying,
            TaskState::Paused,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
        ];
        for state in states {
            assert_eq!(
                status_matches(LIBRARY_FINISHED, state),
                state == TaskState::Completed,
                "finished must be the completed set for {state:?}"
            );
            assert_eq!(
                status_matches(LIBRARY_UNFINISHED, state),
                state != TaskState::Completed,
                "unfinished must be everything except completed for {state:?}"
            );
        }
        assert!(status_matches(LIBRARY_UNFINISHED, TaskState::Failed));
        assert!(!status_matches(LIBRARY_UNFINISHED, TaskState::Completed));
        assert!(!status_matches(LIBRARY_ACTIVE, TaskState::Failed));
        assert!(status_matches(LIBRARY_ACTIVE, TaskState::Paused));
    }

    #[test]
    fn category_status_and_search_combine_and_counts_stay_global() {
        let tasks = mixed();
        let rules = default_categories();
        let mut cache = CategoryCache::new();
        let all = pass_task_list(&tasks, &rules, &mut cache, LIBRARY_ALL, "", "");
        assert_eq!(all.counts.total, 6);
        assert_eq!(all.counts.finished, 1);
        assert_eq!(all.counts.unfinished, 5);
        assert_eq!(all.counts.by_id.get("documents").copied(), Some(2));
        assert_eq!(all.counts.by_id.get("music").copied(), Some(2));
        assert_eq!(all.counts.by_id.get("videos").copied(), Some(1));
        assert_eq!(all.counts.by_id.get("other").copied(), Some(1));
        assert_eq!(all.counts.by_id.get("images").copied(), Some(0));
        assert_eq!(all.visible.len(), 6);

        let narrowed = pass_task_list(
            &tasks,
            &rules,
            &mut cache,
            LIBRARY_UNFINISHED,
            "music",
            "song",
        );
        assert_eq!(
            visible_names(&tasks, &narrowed.visible),
            vec![
                "song.mp3|https://ex/song.mp3".to_string(),
                "song.mp3|https://ex/other".to_string(),
            ]
        );
        assert_eq!(narrowed.counts.total, all.counts.total);
        assert_eq!(narrowed.counts.by_id, all.counts.by_id);
        assert_eq!(narrowed.counts.finished, all.counts.finished);
        assert_eq!(narrowed.counts.unfinished, all.counts.unfinished);

        let by_url = pass_task_list(
            &tasks,
            &rules,
            &mut cache,
            LIBRARY_UNFINISHED,
            "music",
            "other",
        );
        assert_eq!(
            visible_names(&tasks, &by_url.visible),
            vec!["song.mp3|https://ex/other".to_string()]
        );

        let finished_docs = pass_task_list(
            &tasks,
            &rules,
            &mut cache,
            LIBRARY_FINISHED,
            "documents",
            "",
        );
        assert_eq!(
            visible_names(&tasks, &finished_docs.visible),
            vec!["report.pdf|https://ex/report.pdf".to_string()]
        );
        let finished_ids: Vec<_> = tasks
            .iter()
            .enumerate()
            .filter(|(_, task)| task.state == TaskState::Completed)
            .map(|(index, _)| index)
            .collect();
        let finished_view = pass_task_list(&tasks, &rules, &mut cache, LIBRARY_FINISHED, "", "");
        assert_eq!(finished_view.visible, finished_ids);

        let unknown = pass_task_list(
            &tasks,
            &rules,
            &mut cache,
            LIBRARY_UNFINISHED,
            "",
            "https://ex/unknown",
        );
        assert_eq!(
            visible_names(&tasks, &unknown.visible),
            vec!["|https://ex/unknown".to_string()]
        );
        assert_eq!(unknown.counts.by_id.get("other").copied(), Some(1));
    }

    #[test]
    fn one_pass_classifies_each_filename_once_until_rules_change() {
        let tasks = mixed();
        let rules = default_categories();
        let mut cache = CategoryCache::new();
        let _ = pass_task_list(&tasks, &rules, &mut cache, LIBRARY_ALL, "", "");
        assert_eq!(cache.misses, 5, "duplicate song.mp3 is classified once");
        let _ = pass_task_list(
            &tasks,
            &rules,
            &mut cache,
            LIBRARY_UNFINISHED,
            "music",
            "song",
        );
        assert_eq!(cache.misses, 5, "a later refresh reuses the cache");

        let renamed = vec![
            rule("tunes", "Tunes", &[".mp3"]),
            rule("other", "Other", &[]),
        ];
        let again = pass_task_list(&tasks, &renamed, &mut cache, LIBRARY_ALL, "", "");
        assert_eq!(cache.misses, 10, "a new rules fingerprint drops the cache");
        assert_eq!(again.counts.by_id.get("tunes").copied(), Some(2));
        assert_eq!(again.counts.by_id.get("other").copied(), Some(4));
        assert_eq!(again.counts.total, 6);
    }

    #[test]
    fn custom_category_id_is_not_a_translation_key() {
        assert_eq!(category_item_label("documents", false), "Documents");
        assert_eq!(category_item_label("documents", true), "Tài liệu");
        assert_eq!(category_item_label("disk-images", true), "Ảnh đĩa");
        assert_eq!(category_item_label("Invoices", true), "Invoices");
        assert_eq!(crate::i18n::vi("Invoices"), "Invoices");
        assert_ne!(crate::i18n::vi(EXPIRED_HANDOFF_TEXT), EXPIRED_HANDOFF_TEXT);
        assert_eq!(badge_text(0), "");
        assert_eq!(badge_text(4), "4");
    }

    #[test]
    fn expired_notices_show_in_all_and_hide_under_a_category() {
        let entries = vec![
            discovery("chrome:ext", "tx-live", false),
            discovery("chrome:ext", "tx-old", true),
            discovery("firefox:ext", "tx-gone", true),
        ];
        let dismissed = BTreeSet::new();
        let all = shown_notice_keys(LIBRARY_ALL, "", &entries, &dismissed);
        assert_eq!(
            all,
            vec![
                notice_row_key("chrome:ext", "tx-old"),
                notice_row_key("firefox:ext", "tx-gone"),
            ]
        );
        assert!(shown_notice_keys(LIBRARY_ALL, "documents", &entries, &dismissed).is_empty());
        assert!(shown_notice_keys(LIBRARY_ALL, "music", &entries, &dismissed).is_empty());
        assert!(shown_notice_keys(LIBRARY_UNFINISHED, "", &entries, &dismissed).is_empty());
        assert!(shown_notice_keys(LIBRARY_FINISHED, "", &entries, &dismissed).is_empty());
        assert!(shown_notice_keys(LIBRARY_FAILED, "", &entries, &dismissed).is_empty());
        assert!(shown_notice_keys(LIBRARY_ACTIVE, "", &entries, &dismissed).is_empty());

        let mut dismissed = dismissed;
        dismissed.insert(("chrome:ext".to_string(), "tx-old".to_string()));
        assert_eq!(
            shown_notice_keys(LIBRARY_ALL, "", &entries, &dismissed),
            vec![notice_row_key("firefox:ext", "tx-gone")]
        );

        let notice = ExpiredNotice {
            caller: "chrome:ext".to_string(),
            tx: "tx-old".to_string(),
            url: "https://ex/file".to_string(),
            filename: "file.pdf".to_string(),
            size_hint: None,
            at_ms: 10,
        };
        let row = notice_row(&notice, false);
        assert!(parse_key(row.key.as_str()).is_none());
        assert_eq!(
            parse_notice_row_key(row.key.as_str()),
            Some(("chrome:ext", "tx-old"))
        );
        assert_eq!(row.state.as_str(), "expired-handoff");
        assert!(!row.can_pause);
        assert!(!row.can_resume);
        assert!(!row.can_cancel);
        assert!(!row.can_retry);
        assert!(!row.can_remove);
        assert!(row.can_dismiss);
        assert!(!row.can_open_file);
        assert!(!row.can_open_folder);
        assert!(row.error_text.contains("was not downloaded"));
        let vietnamese = notice_row(&notice, true);
        assert!(vietnamese.error_text.contains("chưa được tải"));
    }
}

#[cfg(test)]
mod resume_support_detail {
    use super::{detail_from, TaskSnapshot};
    use usagi::store::records::{TaskState, TransportSummary};

    fn snap(accept_ranges: bool, ranges_observed: bool) -> TaskSnapshot {
        TaskSnapshot {
            id: 7,
            url: "https://example.com/a.bin".into(),
            filename: "a.bin".into(),
            dest_dir: "C:/dl".into(),
            state: TaskState::Paused,
            state_label: "Paused".into(),
            total_size: None,
            received: 0,
            fraction: None,
            speed_bps: 0.0,
            eta_secs: f64::INFINITY,
            segments_done: 0,
            segments_total: 1,
            active_requests: 0,
            error: None,
            transport: None,
            transport_summary: TransportSummary::Unknown,
            paths: Vec::new(),
            created_at: 0,
            started_at: None,
            finished_at: None,
            final_path: "C:/dl/a.bin".into(),
            expected_hash: None,
            computed_hash: None,
            attempts: 0,
            segment_retry: None,
            verify_bps: 0.0,
            accept_ranges,
            ranges_observed,
        }
    }

    #[test]
    fn detail_carries_the_three_range_states() {
        let unknown = detail_from(&snap(false, false), false);
        assert!(!unknown.ranges_observed);
        assert!(!unknown.accept_ranges);

        let unsupported = detail_from(&snap(false, true), false);
        assert!(unsupported.ranges_observed);
        assert!(!unsupported.accept_ranges);

        let supported = detail_from(&snap(true, true), true);
        assert!(supported.ranges_observed);
        assert!(supported.accept_ranges);
    }
}

/// Sidebar collapse: the heading click goes through `Engine::save_settings`,
/// a reload matches, and a failed write puts the previous flags back.
#[cfg(test)]
mod sidebar_collapse_tests {
    use super::{
        settings_for_reset, sidebar_after_save, sidebar_flags, toggled_sidebar, SidebarFlags,
        SidebarSection,
    };
    use usagi::config::Settings;

    struct EngineDir {
        root: std::path::PathBuf,
    }

    impl Drop for EngineDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn idle_engine(root: &std::path::Path) -> usagi::Engine {
        std::fs::create_dir_all(root.join("downloads")).unwrap();
        usagi::EngineBuilder::new()
            .paths(usagi::Paths {
                root: root.to_path_buf(),
                db_file: root.join("test.redb"),
                log_file: root.join("logs").join("test.log"),
                temp_dir: root.join("tmp"),
                default_download_dir: root.join("downloads"),
                portable: true,
                exe_dir: Some(root.to_path_buf()),
            })
            .idle(true)
            .worker_threads(2)
            .build()
            .expect("idle engine")
    }

    fn fresh_dir() -> EngineDir {
        let root = std::env::temp_dir().join(format!(
            "udm-sidebar-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or(0)
        ));
        EngineDir { root }
    }

    /// What the UI handler does, without a window: save, then choose the
    /// flags the sidebar is allowed to show.
    fn apply_toggle(
        engine: &usagi::Engine,
        section: SidebarSection,
    ) -> (SidebarFlags, Result<(), String>) {
        let before = sidebar_flags(&engine.settings());
        let next = toggled_sidebar(&engine.settings(), section);
        match engine.save_settings(next) {
            Ok(()) => (
                sidebar_after_save(before, Ok(sidebar_flags(&engine.settings()))),
                Ok(()),
            ),
            Err(error) => (sidebar_after_save(before, Err(())), Err(error.to_string())),
        }
    }

    #[test]
    fn toggle_round_trip_matches_the_reloaded_settings() {
        let dir = fresh_dir();
        let expected = {
            let engine = idle_engine(&dir.root);
            let (shown, saved) = apply_toggle(&engine, SidebarSection::Library);
            assert!(saved.is_ok());
            assert!(shown.library);
            assert!(!shown.categories);
            assert_eq!(shown, sidebar_flags(&engine.settings()));
            // The other section and an unrelated field stay put.
            let again = toggled_sidebar(&engine.settings(), SidebarSection::Categories);
            assert!(again.sidebar_library_collapsed);
            assert!(again.sidebar_categories_collapsed);
            assert_eq!(again.segments_per_task, engine.settings().segments_per_task);
            engine.save_settings(again).unwrap();
            let live = sidebar_flags(&engine.settings());
            assert!(live.library && live.categories);
            engine.shutdown();
            live
        };
        let engine = idle_engine(&dir.root);
        assert_eq!(sidebar_flags(&engine.settings()), expected);
        engine.shutdown();
    }

    #[test]
    fn failed_save_reverts_the_sidebar_and_leaves_disk_unchanged() {
        let dir = fresh_dir();
        let engine = idle_engine(&dir.root);
        let (shown, saved) = apply_toggle(&engine, SidebarSection::Library);
        assert!(saved.is_ok() && shown.library);
        engine.store().fail_next_settings_saves(1);
        let (reverted, failed) = apply_toggle(&engine, SidebarSection::Categories);
        let message = failed.expect_err("injected failure");
        assert!(
            message.contains("injected settings save failure"),
            "{message}"
        );
        assert!(reverted.library);
        assert!(!reverted.categories);
        assert_eq!(reverted, sidebar_flags(&engine.settings()));
        engine.shutdown();
    }

    #[test]
    fn reset_keeps_sidebar_flags_and_restores_other_fields() {
        let mut live = Settings::default();
        live.sidebar_library_collapsed = true;
        live.sidebar_categories_collapsed = false;
        live.segments_per_task = 20;
        let reset = settings_for_reset(&live);
        assert!(reset.sidebar_library_collapsed);
        assert!(!reset.sidebar_categories_collapsed);
        assert_eq!(
            reset.segments_per_task,
            Settings::default().segments_per_task
        );
        assert_ne!(reset.segments_per_task, 20);
    }
}

/// Settings field widths from U5. The same clamps are written in
/// `views/settings.slint` (`ua-w`, `id-w`); these lock the numbers.
#[cfg(test)]
mod settings_field_width_tests {
    fn user_agent_width(text_px: f32, form_px: f32) -> f32 {
        let sized = (text_px + 24.0).clamp(180.0, 420.0);
        sized.min(form_px)
    }

    fn extension_id_width(cell_px: f32, form_px: f32) -> f32 {
        (cell_px + 24.0).min(360.0).min(form_px)
    }

    #[test]
    fn user_agent_clamps_to_text_padding_then_the_form() {
        assert_eq!(user_agent_width(100.0, 580.0), 180.0);
        assert_eq!(user_agent_width(200.0, 580.0), 224.0);
        assert_eq!(user_agent_width(500.0, 580.0), 420.0);
        assert_eq!(user_agent_width(200.0, 150.0), 150.0);
    }

    #[test]
    fn extension_id_is_thirty_two_cells_plus_padding_capped_at_360() {
        assert_eq!(extension_id_width(200.0, 580.0), 224.0);
        assert_eq!(extension_id_width(400.0, 580.0), 360.0);
        assert_eq!(extension_id_width(200.0, 100.0), 100.0);
    }

    #[test]
    fn chrome_save_as_note_has_a_vietnamese_rendering() {
        let english = "If Chrome shows a Save As dialog before UDM receives the download, you can turn off 'Ask where to save each file before downloading' in Chrome's Downloads settings. UDM does not read or change this setting.";
        let vietnamese = crate::i18n::vi(english);
        assert_ne!(vietnamese, english);
        assert!(vietnamese.contains("Save As") || vietnamese.contains("UDM"));
        assert!(vietnamese.contains("không"));
    }
}

/// Toolbar widths and the selection matrix. Widths match the expressions
/// in `app.slint` (`url-w`, `filter-w`): content is the window minus the
/// 220px sidebar and 32px of padding.
#[cfg(test)]
mod toolbar_tests {
    use super::strip_enablement;
    use crate::task_actions::{caps_for_state, StripCaps};
    use usagi::store::records::TaskState;

    fn content(window_px: f32) -> f32 {
        window_px - 220.0 - 32.0
    }

    fn quick_url_width(content_px: f32, submit_px: f32) -> f32 {
        let ideal = (content_px * 0.75).min(600.0);
        let room = (content_px - submit_px - 8.0).max(0.0);
        ideal.min(room).max(240.0)
    }

    fn filter_width(content_px: f32) -> f32 {
        (content_px * 0.5).min(320.0).max(120.0)
    }

    #[test]
    fn field_widths_match_the_1040_and_800_references() {
        let wide = content(1040.0);
        let narrow = content(800.0);
        assert_eq!(wide, 788.0);
        assert_eq!(narrow, 548.0);
        assert_eq!(quick_url_width(wide, 40.0), 591.0);
        assert_eq!(filter_width(wide), 320.0);
        // 411 is the 75% figure; a normal submit button still fits.
        assert_eq!(quick_url_width(narrow, 60.0), 411.0);
        assert_eq!(filter_width(narrow), 274.0);
        // A huge submit button clamps the URL field to the 240px floor.
        assert_eq!(quick_url_width(narrow, 400.0), 240.0);
        assert_eq!(filter_width(100.0), 120.0);
        assert_eq!(quick_url_width(2000.0, 40.0), 600.0);
    }

    fn states(list: &[TaskState]) -> Vec<StripCaps> {
        list.iter().copied().map(caps_for_state).collect()
    }

    #[test]
    fn strip_follows_selection_for_each_state_and_for_a_mix() {
        let off = strip_enablement(&[]);
        assert!(!off.resume && !off.pause && !off.cancel && !off.remove);
        assert!(!off.open_file && !off.open_folder && !off.copy_url && !off.retry);

        // A notice is not a real task: every capability stays off.
        let notice = strip_enablement(&[StripCaps {
            can_pause: false,
            can_resume: false,
            can_cancel: false,
            can_retry: false,
            can_open_file: false,
            can_open_folder: false,
            can_remove: false,
            can_copy: false,
        }]);
        assert_eq!(notice, off);

        for state in [
            TaskState::Queued,
            TaskState::Probing,
            TaskState::Downloading,
            TaskState::Verifying,
        ] {
            let one = strip_enablement(&states(&[state]));
            assert!(
                one.pause && one.cancel && one.remove && one.copy_url,
                "{state:?}"
            );
            assert!(!one.resume && !one.retry && !one.open_file && !one.open_folder);
        }

        let paused = strip_enablement(&states(&[TaskState::Paused]));
        assert!(paused.resume && paused.open_folder && paused.copy_url && paused.remove);
        assert!(!paused.pause && !paused.cancel && !paused.open_file && !paused.retry);

        let canceled = strip_enablement(&states(&[TaskState::Canceled]));
        assert!(canceled.resume && canceled.copy_url && canceled.remove);
        assert!(!canceled.open_folder && !canceled.pause && !canceled.cancel);

        let failed = strip_enablement(&states(&[TaskState::Failed]));
        assert!(failed.resume && failed.retry && failed.remove && failed.copy_url);
        assert!(!failed.pause && !failed.cancel && !failed.open_file);

        let done = strip_enablement(&states(&[TaskState::Completed]));
        assert!(done.open_file && done.open_folder && done.copy_url && done.remove);
        assert!(!done.resume && !done.pause && !done.cancel && !done.retry);

        let both_live = strip_enablement(&states(&[TaskState::Downloading, TaskState::Queued]));
        assert!(both_live.pause && both_live.remove);
        assert!(!both_live.resume && !both_live.cancel && !both_live.copy_url);
        assert!(!both_live.open_file && !both_live.open_folder && !both_live.retry);

        let both_paused = strip_enablement(&states(&[TaskState::Paused, TaskState::Canceled]));
        assert!(both_paused.resume && both_paused.remove);
        assert!(!both_paused.pause && !both_paused.open_folder && !both_paused.copy_url);

        let mixed = strip_enablement(&states(&[TaskState::Downloading, TaskState::Paused]));
        assert!(mixed.remove);
        assert!(!mixed.pause && !mixed.resume && !mixed.cancel);
        assert!(!mixed.open_file && !mixed.open_folder && !mixed.copy_url && !mixed.retry);

        let both_failed = strip_enablement(&states(&[TaskState::Failed, TaskState::Failed]));
        assert!(both_failed.remove);
        assert!(!both_failed.resume && !both_failed.retry && !both_failed.copy_url);

        let both_done = strip_enablement(&states(&[TaskState::Completed, TaskState::Completed]));
        assert!(both_done.remove);
        assert!(!both_done.open_file && !both_done.open_folder && !both_done.copy_url);
    }
}

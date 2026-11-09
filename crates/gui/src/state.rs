//! Wires the engine to the Slint UI.
//!
//! The UI thread never blocks: it polls the engine on a 150 ms timer (the
//! engine's own snapshot API is lock-free and cheap) and only shows native
//! dialogs (folder pickers) modally, which is what users expect on Windows.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak};

use usagi::config::{ConflictPolicy, Settings, TransportPref};
use usagi::engine::{AddRequest, Engine, Event, TaskSnapshot};
use usagi::hash::ExpectedHash;
use usagi::store::records::TaskState;
use usagi::util;

use crate::{AppWindow, EndpointRow, StatusInfo, TaskDetail, TaskRow};

const REFRESH_MS: u64 = 150;
const SLOW_REFRESH_MS: u64 = 2000;

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
}

pub fn setup(ui: &AppWindow, engine: Arc<Engine>) {
    ui.global::<crate::I18n>().on_translate(|key, language| {
        if language == 1 {
            crate::i18n::vi(key.as_str()).into()
        } else {
            key
        }
    });
    let tasks = Rc::new(VecModel::<TaskRow>::default());
    let endpoints = Rc::new(VecModel::<EndpointRow>::default());
    ui.set_tasks(ModelRc::from(tasks.clone()));
    ui.set_endpoints(ModelRc::from(endpoints.clone()));
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
    });

    push_settings_to_ui(ui, &engine.settings());
    refresh(&gui);

    let g = gui.clone();
    ui.on_appearance_changed(move || {
        let Some(ui) = g.ui.upgrade() else { return };
        ui.invoke_apply_native_theme();
        let mut settings = g.engine.settings();
        read_appearance(&ui, &mut settings);
        if let Err(error) = g.engine.update_settings(settings) {
            toast(&g, error.to_string(), true);
        }
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
    ui.on_pause(move |id| act(&g, || g.engine.pause(id as u64)));
    let g = gui.clone();
    ui.on_resume(move |id| act(&g, || g.engine.resume(id as u64)));
    let g = gui.clone();
    ui.on_cancel(move |id| act(&g, || g.engine.cancel(id as u64)));
    let g = gui.clone();
    ui.on_retry(move |id| act(&g, || g.engine.retry(id as u64)));
    let g = gui.clone();
    ui.on_remove(move |id| act(&g, || g.engine.remove(id as u64, false)));
    let g = gui.clone();
    ui.on_delete_file(move |id| act(&g, || g.engine.remove(id as u64, true)));
    let g = gui.clone();
    ui.on_copy_url(move |id| copy_task_url(&g, id as u64));
    let g = gui.clone();
    ui.on_open_file(move |id| open_task_file(&g, id as u64, false));
    let g = gui.clone();
    ui.on_open_folder(move |id| open_task_file(&g, id as u64, true));

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
        toast(
            &g,
            format!("removed {handled}, skipped {skipped}"),
            false,
        );
        refresh(&g);
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
        let defaults = Settings::default();
        if let Some(ui) = g.ui.upgrade() {
            push_settings_to_ui(&ui, &defaults);
        }
        act(&g, || g.engine.update_settings(defaults.clone()));
        toast(&g, "defaults restored".into(), false);
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

    // ------------------------------------------------------------- poll timer
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        Duration::from_millis(REFRESH_MS),
        move || {
            tray_poll(&gui);
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

/// Drain tray events (Phase 2). Runs inside the existing 150 ms poll, on
/// the event-loop thread; the tray module itself never touches the UI.
fn tray_poll(g: &Rc<Gui>) {
    use crate::tray::TrayAction;
    let actions = match g.tray.borrow().as_ref() {
        Some(tray) => tray.drain(),
        None => return,
    };
    for action in actions {
        tracing::info!(?action, "tray action handled");
        match action {
            TrayAction::Show => {
                if let Some(ui) = g.ui.upgrade() {
                    let _ = ui.window().show();
                }
                if let Some(tray) = g.tray.borrow_mut().as_mut() {
                    tray.set_visible(true);
                }
            }
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
                    }
                }
                if let Some(tray) = g.tray.borrow_mut().as_mut() {
                    tray.set_visible(!visible);
                }
            }
            TrayAction::PauseAll => all_act(g, true),
            TrayAction::ResumeAll => all_act(g, false),
            TrayAction::Exit => {
                let _ = slint::quit_event_loop();
            }
        }
    }
}

fn act(g: &Rc<Gui>, f: impl FnOnce() -> usagi::error::Result<()>) {
    match f() {
        Ok(()) => {}
        Err(e) => toast(g, e.to_string(), true),
    }
    refresh(g);
}

fn toast(g: &Rc<Gui>, message: String, is_error: bool) {
    let Some(ui) = g.ui.upgrade() else { return };
    ui.set_toast(ui.invoke_translate(message.into()));
    ui.set_toast_error(is_error);
    *g.toast_until.borrow_mut() = Instant::now() + Duration::from_secs(6);
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

    // --- rows (sidebar "library" group + toolbar substring filter)
    let library = ui.get_library();
    let filter = ui.get_filter().to_lowercase();
    let snapshots = g.engine.snapshot();
    let visible: Vec<&TaskSnapshot> = snapshots
        .iter()
        .filter(|s| match library {
            1 => matches!(
                s.state,
                TaskState::Queued
                    | TaskState::Probing
                    | TaskState::Downloading
                    | TaskState::Verifying
                    | TaskState::Paused
            ),
            2 => s.state == TaskState::Completed,
            3 => s.state == TaskState::Failed,
            _ => true,
        })
        .filter(|s| {
            filter.is_empty()
                || s.filename.to_lowercase().contains(&filter)
                || s.url.to_lowercase().contains(&filter)
        })
        .collect();
    // Prune selection to currently visible rows, so bulk actions never
    // silently affect a hidden row, and removed tasks drop out.
    let visible_ids: BTreeSet<u64> = visible.iter().map(|s| s.id).collect();
    g.selected.borrow_mut().retain(|id| visible_ids.contains(id));
    let selected = g.selected.borrow().clone();
    ui.set_selected_count(selected.len() as i32);
    let rows: Vec<TaskRow> = visible
        .iter()
        .map(|s| row_from(s, ui.get_language_index() == 1, selected.contains(&s.id)))
        .collect();
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
fn detail_from(s: &TaskSnapshot, vietnamese: bool) -> TaskDetail {
    let (_, transport_label) = match s.transport {
        Some(usagi::store::records::TransportKind::Http3) => ("H3", "HTTP/3 (QUIC)"),
        Some(usagi::store::records::TransportKind::Http2) => ("H2", "HTTP/2"),
        Some(usagi::store::records::TransportKind::Http11) => ("H1", "HTTP/1.1"),
        _ => ("?", ""),
    };
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
            if vietnamese { "không rõ kích thước" } else { "unknown size" }
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
        state_label: SharedString::from(s.state_label.clone()),
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
            "{}/{} {} · {} {}", s.segments_done, s.segments_total,
            if vietnamese { "xong" } else { "done" }, s.active_requests,
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
    }
}

fn row_from(s: &TaskSnapshot, vietnamese: bool, selected: bool) -> TaskRow {
    let downloading = s.state == TaskState::Downloading;
    let (transport, transport_label) = match s.transport {
        Some(usagi::store::records::TransportKind::Http3) => ("H3", "HTTP/3 (QUIC)"),
        Some(usagi::store::records::TransportKind::Http2) => ("H2", "HTTP/2"),
        Some(usagi::store::records::TransportKind::Http11) => ("H1", "HTTP/1.1"),
        _ => ("?", ""),
    };
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
    TaskRow {
        id: s.id as f32,
        key: SharedString::from(s.id.to_string()),
        selected,
        name: SharedString::from(name.clone()),
        url: SharedString::from(s.url.clone()),
        state: SharedString::from(s.state.as_str()),
        state_label: SharedString::from(s.state_label.clone()),
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
        can_pause: matches!(
            s.state,
            TaskState::Queued | TaskState::Probing | TaskState::Downloading | TaskState::Verifying
        ),
        can_resume: matches!(s.state, TaskState::Paused | TaskState::Canceled),
        can_cancel: matches!(
            s.state,
            TaskState::Queued | TaskState::Probing | TaskState::Downloading | TaskState::Verifying
        ),
        can_retry: s.state == TaskState::Failed,
        can_open_file: s.state == TaskState::Completed,
        can_open_folder: matches!(s.state, TaskState::Completed | TaskState::Paused),
        can_remove: true,
        hash_text: SharedString::from(hash),
        glyph: SharedString::from(glyph_for(&name)),
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
    s.user_agent = ui.get_set_user_agent().to_string();
    s.log_level = match ui.get_set_log_index() {
        0 => "off".into(),
        1 => "error".into(),
        2 => "warn".into(),
        4 => "debug".into(),
        5 => "trace".into(),
        _ => "info".into(),
    };
    s.sanitize();
    match g.engine.update_settings(s.clone()) {
        Ok(()) => toast(g, "settings saved".into(), false),
        Err(e) => toast(g, e.to_string(), true),
    }
    push_settings_to_ui(&ui, &s);
    ui.set_http3_available(g.engine.net().http3_available());
}

fn read_appearance(ui: &AppWindow, settings: &mut Settings) {
    settings.theme = if ui.get_theme_index() == 1 {
        "light"
    } else {
        "dark"
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

pub fn push_settings_to_ui(ui: &AppWindow, s: &Settings) {
    ui.set_theme_index(if s.theme == "light" { 1 } else { 0 });
    ui.set_language_index(if s.language == "vi" { 1 } else { 0 });
    ui.set_accent_index(
        ["blue", "green", "orange", "pink", "purple"]
            .iter()
            .position(|a| *a == s.accent)
            .unwrap_or(0) as i32,
    );
    ui.invoke_apply_native_theme();
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
    ui.set_set_user_agent(SharedString::from(s.user_agent.clone()));
    ui.set_set_log_index(match s.log_level.as_str() {
        "off" => 0,
        "error" => 1,
        "warn" => 2,
        "debug" => 4,
        "trace" => 5,
        _ => 3,
    });
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
    let Some(s) = g.engine.task(id) else { return };
    if s.final_path.is_empty() {
        return;
    }
    let path = std::path::PathBuf::from(&s.final_path);
    if folder {
        let dir = path.parent().unwrap_or(&path).to_path_buf();
        if dir.exists() {
            shell_open(&dir, false);
        } else {
            toast(
                g,
                format!("folder no longer exists: {}", dir.display()),
                true,
            );
        }
    } else if path.exists() {
        shell_open(&path, false);
    } else {
        // A deleted completed file must be reported, not silently
        // papered over by opening its parent folder.
        toast(g, format!("file no longer exists: {}", path.display()), true);
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

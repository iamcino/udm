//! Per-download detail windows.
//!
//! The book in [`crate::detail_assoc`] decides what a window is bound to.
//! This module only creates the native window, paints the book, and forwards
//! Pause, Resume, Cancel, Retry, and Open Folder to the handlers the main
//! window already uses. Pause is not painted until the next snapshot.
//! Callbacks hold a weak handle to the GUI. The session holds the window
//! while it is open.

use std::rc::Rc;

use slint::{ComponentHandle, SharedString};

use usagi::engine::TaskSnapshot;
use usagi::handoff::HandoffReply;

use crate::detail_assoc::{
    decide_transport, view_model, AssocKey, Book, DetailModel, DetailSource, Phase, TransportCall,
    TransportRequest,
};
use crate::handoff_dialog::{
    appearance_pending, apply_placement, decide_continuation, decide_first_show,
    native_size_settled, FirstShow, FirstShowGate, MonitorSnap, NativeSize,
};
use crate::state::{
    category_label_for, open_task_folder, refresh_ui, run_task_verb, show_toast, Gui, TaskVerb,
};

pub struct DetailSession {
    pub book: Book,
    windows: Vec<LiveWindow>,
    /// Hidden windows the user already closed. Kept so a close callback
    /// does not drop the component it is running on. Not shown again.
    retired: Vec<Rc<crate::DetailWindow>>,
}

impl Default for DetailSession {
    fn default() -> Self {
        DetailSession {
            book: Book::default(),
            windows: Vec::new(),
            retired: Vec::new(),
        }
    }
}

struct LiveWindow {
    key: AssocKey,
    window: Rc<crate::DetailWindow>,
    gate: FirstShowGate,
    monitor: MonitorSnap,
    cascade_index: u32,
    allow_focus: bool,
    appearance_applied: bool,
}

/// Open the window for an accepted handoff Download. The book records the
/// waiting state before the native window exists, and revokes that record
/// if the window cannot be created. A second call for the same pair does
/// nothing: the book refuses a duplicate and a closed tombstone.
pub fn open_from_handoff_download(
    g: &Rc<Gui>,
    key: AssocKey,
    filename: String,
    url: String,
    materialize_error: String,
    monitor: Option<MonitorSnap>,
    allow_focus: bool,
) {
    let cascade_index = {
        let mut session = g.detail.borrow_mut();
        session.book.request_open(
            DetailSource::HandoffDownload,
            key.clone(),
            filename,
            url,
            materialize_error,
        )
    };
    let Some(cascade_index) = cascade_index else {
        return;
    };
    let monitor = monitor.unwrap_or_else(|| crate::handoff_dialog::presentation_target().1);
    let created = match crate::DetailWindow::new() {
        Ok(window) => window,
        Err(error) => {
            tracing::error!(%error, caller = %key.caller, tx = %key.tx, "detail window: cannot create the window");
            g.detail.borrow_mut().book.revoke_unshown(&key);
            return;
        }
    };
    created
        .global::<crate::I18n>()
        .on_translate(|text, language| {
            if language == 1 {
                crate::i18n::vi(text.as_str()).into()
            } else {
                text
            }
        });
    let weak = Rc::downgrade(g);
    let close_key = key.clone();
    created.window().on_close_requested(move || {
        if let Some(g) = weak.upgrade() {
            // HideWindow performs the hide. Do not hide again here.
            close_detail(&g, &close_key, false);
        }
        slint::CloseRequestResponse::HideWindow
    });
    let weak = Rc::downgrade(g);
    let close_key = key.clone();
    created.on_close_window(move || {
        if let Some(g) = weak.upgrade() {
            close_detail(&g, &close_key, true);
        }
    });
    wire_transport(&created, g, &key);
    let window = Rc::new(created);
    {
        let mut session = g.detail.borrow_mut();
        session.windows.push(LiveWindow {
            key: key.clone(),
            window: Rc::clone(&window),
            gate: FirstShowGate::default(),
            monitor,
            cascade_index,
            allow_focus,
            appearance_applied: false,
        });
    }
    let snapshots = g.engine.snapshot();
    paint_open(g, &snapshots);
    let _ = window.show();
    present_once(g, &key);
    if !allow_focus {
        crate::handoff_dialog::refocus_open_dialog(g);
    }
    tracing::info!(
        caller = %key.caller,
        tx = %key.tx,
        cascade = cascade_index,
        focus = allow_focus,
        "detail window waiting for recovery"
    );
}

/// Apply one query result for this window's own `(caller, tx)`.
/// Does not create a window and does not query anyone else.
pub fn apply_authoritative(g: &Rc<Gui>, key: &AssocKey, reply: &HandoffReply) {
    let effect = g.detail.borrow_mut().book.apply_reply(key, reply);
    if matches!(
        effect,
        crate::detail_assoc::ApplyEffect::Bound
            | crate::detail_assoc::ApplyEffect::HandoffClosed
            | crate::detail_assoc::ApplyEffect::StillWaiting
    ) {
        if let Some(task_id) = g.detail.borrow().book.bound_id(key) {
            tracing::info!(
                caller = %key.caller,
                tx = %key.tx,
                task_id,
                "detail window bound to the handoff task"
            );
        }
        let snapshots = g.engine.snapshot();
        paint_open(g, &snapshots);
    }
}

/// Poll path. Queries only pairs that are still waiting, then repaints
/// every open window from the shared snapshot. Does not move or focus.
pub fn sync(g: &Rc<Gui>, snapshots: &[TaskSnapshot]) {
    let keys = g.detail.borrow().book.waiting_keys();
    for key in keys {
        let Some(coordinator) = g.handoff.borrow().clone() else {
            break;
        };
        match coordinator.query(&key.caller, &key.tx) {
            Ok(reply) => {
                let effect = g.detail.borrow_mut().book.apply_reply(&key, &reply);
                if let Some(task_id) = g.detail.borrow().book.bound_id(&key) {
                    if effect == crate::detail_assoc::ApplyEffect::Bound {
                        tracing::info!(
                            caller = %key.caller,
                            tx = %key.tx,
                            task_id,
                            "detail window bound to the handoff task"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::debug!(
                    %error,
                    caller = %key.caller,
                    tx = %key.tx,
                    "detail window association query failed"
                );
            }
        }
    }
    let ids: Vec<u64> = snapshots.iter().map(|snapshot| snapshot.id).collect();
    g.detail.borrow_mut().book.observe_present(&ids);
    paint_open(g, snapshots);
}

fn wire_transport(window: &crate::DetailWindow, g: &Rc<Gui>, key: &AssocKey) {
    let weak = Rc::downgrade(g);
    let pause_key = key.clone();
    window.on_pause(move || {
        if let Some(g) = weak.upgrade() {
            transport_clicked(&g, &pause_key, TransportRequest::Pause);
        }
    });
    let weak = Rc::downgrade(g);
    let resume_key = key.clone();
    window.on_resume_or_retry(move || {
        if let Some(g) = weak.upgrade() {
            transport_clicked(&g, &resume_key, TransportRequest::ResumeOrRetry);
        }
    });
    let weak = Rc::downgrade(g);
    let cancel_key = key.clone();
    window.on_cancel(move || {
        if let Some(g) = weak.upgrade() {
            transport_clicked(&g, &cancel_key, TransportRequest::Cancel);
        }
    });
    let weak = Rc::downgrade(g);
    let folder_key = key.clone();
    window.on_open_folder(move || {
        if let Some(g) = weak.upgrade() {
            transport_clicked(&g, &folder_key, TransportRequest::OpenFolder);
        }
    });
}

/// Run one detail-window action through the toolbar's handler.
///
/// The button enablement was computed from an earlier snapshot. This reads
/// the snapshot again and uses the same predicate. A rejection is stored on
/// the window; it does not flip the painted state. A pause that the engine
/// only queues is painted as whatever `snapshot()` says on the refresh that
/// follows, which may still be the pre-pause state.
fn transport_clicked(g: &Rc<Gui>, key: &AssocKey, request: TransportRequest) {
    let slot = {
        let session = g.detail.borrow();
        let Some(slot) = session.book.slot(key) else {
            return;
        };
        if slot.closed_by_user {
            return;
        }
        slot
    };
    let task_id = match &slot.phase {
        Phase::Bound { task_id } => Some(*task_id),
        _ => None,
    };
    let snapshots = g.engine.snapshot();
    let snapshot = task_id.and_then(|id| snapshots.iter().find(|snapshot| snapshot.id == id));
    let decision = decide_transport(&slot, snapshot, request);
    if let Some(message) = decision.error {
        g.detail
            .borrow_mut()
            .book
            .set_action_error(key, message.to_string());
        show_toast(g, message.to_string(), true);
        refresh_ui(g);
        return;
    }
    let Some(call) = decision.call else {
        refresh_ui(g);
        return;
    };
    let Some(id) = task_id else {
        refresh_ui(g);
        return;
    };
    let outcome = match call {
        TransportCall::Pause => run_task_verb(g, id, TaskVerb::Pause),
        TransportCall::Resume => run_task_verb(g, id, TaskVerb::Resume),
        TransportCall::Cancel => run_task_verb(g, id, TaskVerb::Cancel),
        TransportCall::Retry => run_task_verb(g, id, TaskVerb::Retry),
        TransportCall::OpenFolder => folder_outcome(g, id),
    };
    match outcome {
        Ok(()) => g.detail.borrow_mut().book.clear_action_error(key),
        Err(message) => g.detail.borrow_mut().book.set_action_error(key, message),
    }
    refresh_ui(g);
}

/// Open Folder uses the toolbar helper. A task that disappeared between
/// paint and click never reaches the shell: the same NotFound text the
/// engine returns for pause/resume/cancel is shown in this window.
fn folder_outcome(g: &Rc<Gui>, id: u64) -> Result<(), String> {
    if g.engine.task(id).is_none() {
        let message = usagi::Error::NotFound(id).to_string();
        show_toast(g, message.clone(), true);
        return Err(message);
    }
    match open_task_folder(g, id) {
        Some(message) => Err(message),
        None => Ok(()),
    }
}

fn close_detail(g: &Gui, key: &AssocKey, hide: bool) {
    let removed = {
        let mut session = g.detail.borrow_mut();
        if !session.book.close(key) {
            return;
        }
        session
            .windows
            .iter()
            .position(|live| &live.key == key)
            .map(|index| session.windows.remove(index))
    };
    let Some(live) = removed else {
        return;
    };
    if hide {
        let _ = live.window.hide();
    }
    g.detail.borrow_mut().retired.push(live.window);
    tracing::info!(caller = %key.caller, tx = %key.tx, "detail window closed by the user");
}

fn present_once(g: &Rc<Gui>, key: &AssocKey) {
    let settled = {
        let session = g.detail.borrow();
        let Some(live) = session.windows.iter().find(|live| &live.key == key) else {
            return;
        };
        native_window_settled(&live.window)
    };
    let decision = {
        let mut session = g.detail.borrow_mut();
        let Some(live) = session.windows.iter_mut().find(|live| &live.key == key) else {
            return;
        };
        decide_first_show(&mut live.gate, &key.caller, &key.tx, settled)
    };
    match decision {
        FirstShow::Now => place_window(g, key),
        FirstShow::Later => defer_present(g, key.clone()),
        FirstShow::NotNow => {}
    }
}

fn defer_present(g: &Rc<Gui>, key: AssocKey) {
    let window = {
        let session = g.detail.borrow();
        let Some(live) = session.windows.iter().find(|live| live.key == key) else {
            return;
        };
        Rc::clone(&live.window)
    };
    let weak = window.as_weak();
    let gui = Rc::downgrade(g);
    let caller = key.caller.clone();
    let tx = key.tx.clone();
    if let Err(error) = slint::spawn_local(async move {
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
                "detail window: presentation continuation lost the native window"
            );
            return;
        }
        let Some(g) = gui.upgrade() else {
            return;
        };
        let decision = {
            let mut session = g.detail.borrow_mut();
            let Some(live) = session
                .windows
                .iter_mut()
                .find(|live| live.key.caller == caller && live.key.tx == tx)
            else {
                return;
            };
            decide_continuation(&mut live.gate, &caller, &tx)
        };
        if decision == FirstShow::Now {
            place_window(&g, &AssocKey { caller, tx });
        }
    }) {
        tracing::error!(%error, caller = %key.caller, tx = %key.tx, "detail window: the one presentation continuation was not scheduled");
    }
}

fn place_window(g: &Gui, key: &AssocKey) {
    let (window, monitor, cascade_index, allow_focus) = {
        let session = g.detail.borrow();
        let Some(live) = session.windows.iter().find(|live| &live.key == key) else {
            return;
        };
        (
            Rc::clone(&live.window),
            live.monitor,
            live.cascade_index,
            live.allow_focus,
        )
    };
    use slint::winit_030::WinitWindowAccessor;
    let placed = window.window().with_winit_window(|winit_window| {
        let inner = winit_window.inner_size();
        let outer = winit_window.outer_size();
        let native = if native_size_settled(inner.width, inner.height, outer.width, outer.height) {
            NativeSize {
                inner_width: inner.width,
                inner_height: inner.height,
                outer_width: outer.width,
                outer_height: outer.height,
            }
        } else {
            NativeSize {
                inner_width: 0,
                inner_height: 0,
                outer_width: 0,
                outer_height: 0,
            }
        };
        let placed = crate::detail_assoc::detail_placement(&monitor, native, cascade_index);
        apply_placement(winit_window, placed);
        if allow_focus {
            winit_window.focus_window();
        }
        placed
    });
    if let Some(placed) = placed {
        if allow_focus {
            window.invoke_grab_focus();
        }
        tracing::info!(
            caller = %key.caller,
            tx = %key.tx,
            x = placed.x,
            y = placed.y,
            focus = allow_focus,
            "detail window placed once"
        );
    }
}

fn native_window_settled(window: &crate::DetailWindow) -> bool {
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

fn paint_open(g: &Gui, snapshots: &[TaskSnapshot]) {
    let lang =
        g.ui.upgrade()
            .map(|ui| ui.get_language_index())
            .unwrap_or(0);
    let theme = g.ui.upgrade().map(|ui| ui.get_theme_index()).unwrap_or(0);
    let accent = g.ui.upgrade().map(|ui| ui.get_accent_index()).unwrap_or(0);
    let vietnamese = lang == 1;
    let rules = g.engine.settings().category_rules();
    let jobs: Vec<PaintJob> = {
        let session = g.detail.borrow();
        session
            .windows
            .iter()
            .filter_map(|live| {
                let slot = session.book.slot(&live.key)?;
                if slot.closed_by_user {
                    return None;
                }
                let snapshot = match &slot.phase {
                    Phase::Bound { task_id } => {
                        snapshots.iter().find(|snapshot| snapshot.id == *task_id)
                    }
                    _ => None,
                };
                let category = snapshot
                    .map(|snapshot| {
                        let name = if snapshot.filename.is_empty() {
                            slot.filename.as_str()
                        } else {
                            snapshot.filename.as_str()
                        };
                        category_label_for(name, &rules, vietnamese)
                    })
                    .unwrap_or_default();
                let model = view_model(&slot, snapshot, &category, vietnamese);
                let differs = live.window.get_language_index() != lang
                    || live.window.get_theme_index() != theme
                    || live.window.get_accent_index() != accent;
                Some(PaintJob {
                    key: live.key.clone(),
                    window: Rc::clone(&live.window),
                    model,
                    apply_appearance: appearance_pending(live.appearance_applied, differs),
                    lang,
                    theme,
                    accent,
                })
            })
            .collect()
    };
    for job in &jobs {
        if job.apply_appearance {
            job.window.set_language_index(job.lang);
            job.window.set_theme_index(job.theme);
            job.window.set_accent_index(job.accent);
            job.window.invoke_apply_appearance();
        }
        apply_model(&job.window, &job.model);
    }
    if jobs.iter().any(|job| job.apply_appearance) {
        let mut session = g.detail.borrow_mut();
        for job in &jobs {
            if !job.apply_appearance {
                continue;
            }
            if let Some(live) = session.windows.iter_mut().find(|live| live.key == job.key) {
                live.appearance_applied = true;
            }
        }
    }
}

struct PaintJob {
    key: AssocKey,
    window: Rc<crate::DetailWindow>,
    model: DetailModel,
    apply_appearance: bool,
    lang: i32,
    theme: i32,
    accent: i32,
}

fn apply_model(window: &crate::DetailWindow, model: &DetailModel) {
    window.set_filename_text(SharedString::from(model.title.as_str()));
    window.set_url_text(SharedString::from(model.url.as_str()));
    window.set_status_text(SharedString::from(model.status.as_str()));
    window.set_size_text(SharedString::from(model.size_text.as_str()));
    window.set_downloaded_text(SharedString::from(model.downloaded_text.as_str()));
    window.set_percent_text(SharedString::from(model.percent_text.as_str()));
    window.set_percent_value(model.percent);
    window.set_speed_text(SharedString::from(model.speed_text.as_str()));
    window.set_eta_text(SharedString::from(model.eta_text.as_str()));
    window.set_resume_text(SharedString::from(model.resume_text.as_str()));
    window.set_resume_help(SharedString::from(model.resume_help.as_str()));
    window.set_destination_text(SharedString::from(model.destination.as_str()));
    window.set_category_text(SharedString::from(model.category.as_str()));
    window.set_notice_text(SharedString::from(model.notice.as_str()));
    window.set_error_text(SharedString::from(model.error.as_str()));
    window.set_waiting(model.waiting);
    window.set_failed(model.failed);
    window.set_done(model.done);
    window.set_can_pause(model.can_pause);
    window.set_can_resume(model.can_resume);
    window.set_can_cancel(model.can_cancel);
    window.set_can_retry(model.can_retry);
    window.set_can_open_folder(model.can_open_folder);
}

//! # UDM: Universal Download Manager desktop app
//!
//! A native Rust/Slint front-end for the Usagi (`usagi`) download engine,
//! targeting Windows 11. The desktop app presents task/detail views,
//! category options, EN/VI strings, Dark/Light/Auto appearance, tray totals
//! and an active-downloads popup. The engine performs the transfers.
//!
//! Closing hides to the tray by default; minimize-to-tray, start-minimized
//! and start-with-Windows are separate opt-ins. Startup uses a disclosed
//! per-user HKCU Run entry. The optional browser-handoff coordinator serves
//! registered local callers and exposes pending associations as read-only
//! Needs recovery rows, distinct from engine tasks. A prepared handoff
//! opens its own confirmation window.
//!
//! No telemetry, administrator rights or services are required or
//! implemented. Startup removes the previous update's `.old` backups
//! before the engine and the window start. Update checks are on by
//! default and run at startup and every 6 hours while the app runs, as
//! well as from the manual Check for Updates button; a durable opt-out
//! turns them off. An explicit Apply downloads the new build, verifies
//! it, swaps it in and relaunches into it - apply is never automatic.
//! Application data
//! is per-user, or in `data/` beside the
//! executable in portable mode; downloads have their own destination.
//! Panic diagnostics stay in the local log. See `docs/PRIVACY.md` and
//! `docs/OPERATIONS.md` in the repository. `udm-cli` is the standalone
//! headless front-end, not a remote controller for this process.

// Hide the console window on release builds for Windows.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
// The slint 1.6 code generator still emits `core::usize::MAX`; silence that.
#![allow(deprecated)]

mod browser;
mod detail_assoc;
mod detail_window;
mod handoff_boot;
mod handoff_dialog;
mod i18n;
mod startup;
mod state;
mod task_actions;
mod tray;

use std::sync::Arc;

use usagi::engine::EngineBuilder;
use usagi::paths::Paths;
use usagi::updater::InstallLayout;

slint::include_modules!();

fn main() {
    let paths = match Paths::discover() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("udm: cannot work out where to store data: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = paths.ensure_dirs() {
        eprintln!(
            "udm: cannot create data directories at {}: {e}",
            paths.root.display()
        );
        std::process::exit(1);
    }
    init_logging(&paths);

    // The GUI has no console (windows_subsystem = "windows"): without this
    // hook a panic would vanish without a trace. Now it lands in udm.log.
    std::panic::set_hook(Box::new(|info| {
        let bt = std::backtrace::Backtrace::force_capture();
        tracing::error!(panic = %info, backtrace = %bt, "PANIC");
    }));

    tracing::info!(
        data_dir = %paths.root.display(),
        portable = paths.portable,
        "udm starting"
    );

    // Updater startup: a previous apply left `.old` backups, and may
    // have left stale Firefox archives. Remove them on a background
    // thread (120 tries, 500 ms apart). The previous process may still
    // be exiting. A check stores nothing, and there is no swap journal.
    let update_layout = paths.exe_dir.clone().map(InstallLayout::new);
    // The Apply action sets this flag and quits the event loop; the new
    // exe starts below, after the engine released its databases, so the
    // new build can open them.
    let relaunch_after_apply = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let notice: Option<String> = None;
    if let Some(layout) = &update_layout {
        usagi::updater::finish_previous_update(layout);
    }

    let handoff_paths = paths.clone();
    let engine = match EngineBuilder::new().paths(paths).build() {
        Ok(e) => Arc::new(e),
        Err(usagi::error::Error::DatabaseLocked(p)) => {
            tracing::error!(path = %p.display(), "another udm instance holds the database");
            eprintln!(
                "udm is already running (the database at {} is locked).",
                p.display()
            );
            std::process::exit(2);
        }
        Err(e) => {
            tracing::error!(error = %e, "engine failed to start");
            eprintln!("udm: {e}");
            std::process::exit(1);
        }
    };

    tracing::info!("engine ready");
    let handoff = handoff_boot::start(&handoff_paths, engine.clone());

    let ui = match AppWindow::new() {
        Ok(ui) => ui,
        Err(e) => {
            tracing::error!(error = %e, "cannot create the window");
            eprintln!("udm: cannot create the window: {e}");
            std::process::exit(1);
        }
    };
    tracing::info!("window created");

    // Without a window manager (headless CI, Xvfb, bare X11) winit ignores the
    // preferred/min size declared in app.slint and falls back to a small
    // default that clips the downloads table. Ask for the designed size
    // explicitly there; real desktops (Windows, macOS, composited X11/Wayland)
    // honour the .slint declaration on their own.
    #[cfg(all(unix, not(target_os = "macos")))]
    ui.window().set_size(slint::LogicalSize::new(1280.0, 760.0));

    // The winit backend materialises the native window only once the event
    // loop runs, so a pre-run set_size can be dropped on the floor. Re-assert
    // the designed size once from inside the loop (headless X11 only).
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(400));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.window().set_size(slint::LogicalSize::new(1280.0, 760.0));
                }
            });
        });
    }

    // G3: the Start-with-Windows entry appends --minimized when the
    // start_minimized setting is on; the app then starts hidden in the
    // tray (see state::setup). A normal launch shows the window as usual.
    let hidden = std::env::args().any(|a| a == "--minimized");
    let layout = update_layout.clone();
    let relaunch = relaunch_after_apply.clone();
    let eng = engine.clone();
    state::setup(&ui, eng, handoff, hidden, notice, layout, relaunch);

    tracing::info!("entering event loop");

    // 0.2.3 U1: the app must outlive its window. Both `ui.run()` and
    // `slint::run_event_loop()` quit when the last window hides: Slint
    // keeps a keepalive count of visible windows (plus its own visible
    // SystemTrayIcon components), and hiding the last one posts a quit.
    // UDM's tray comes from the tray_icon crate, which Slint cannot see,
    // so close-to-tray dropped the count to zero and killed the process
    // (and a --minimized start with it). The until-quit variant is the
    // daemon-style loop: it ends only via `slint::quit_event_loop()` —
    // tray Exit, a real close with close-to-tray off, or the
    // update-apply restart paths. A hidden launch stays hidden: setup
    // has already applied the start-hidden state, so only a normal
    // launch is shown here.
    if !hidden {
        let _ = ui.show();
    }
    let result = slint::run_event_loop_until_quit();
    let _ = ui.hide();
    // Graceful: stop the supervisor, flush redb, close QUIC connections.
    engine.shutdown();
    tracing::info!("udm stopped");

    if let Err(e) = result {
        eprintln!("udm: window error: {e}");
        std::process::exit(1);
    }

    // An explicit Apply succeeded, so start the new build now. The
    // swap target is the install base's udm.exe. Arguments carry over
    // (e.g. --minimized). A failed spawn is logged; the files are
    // already swapped, so the next manual start runs the new build.
    if relaunch_after_apply.load(std::sync::atomic::Ordering::SeqCst) {
        if let Some(layout) = &update_layout {
            let exe = layout.base().join("udm.exe");
            let mut command = std::process::Command::new(&exe);
            command.args(std::env::args().skip(1));
            match command.spawn() {
                Ok(_) => tracing::info!(path = %exe.display(), "relaunched the updated build"),
                Err(e) => tracing::error!(
                    path = %exe.display(),
                    error = %e,
                    "cannot relaunch the updated build"
                ),
            }
        }
    }
}

/// Append-only log file in the data directory, capped at ~8 MiB (truncated on
/// start when bigger). Nothing leaves the machine.
fn init_logging(paths: &Paths) {
    let level = match std::env::var("UDM_LOG").ok().as_deref() {
        Some("off") => tracing::Level::ERROR, // "off" still keeps fatal errors
        Some("error") => tracing::Level::ERROR,
        Some("warn") => tracing::Level::WARN,
        Some("debug") => tracing::Level::DEBUG,
        Some("trace") => tracing::Level::TRACE,
        _ => tracing::Level::INFO,
    };
    let file = match std::fs::metadata(&paths.log_file) {
        Ok(m) if m.len() > 8 * 1024 * 1024 => std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&paths.log_file),
        _ => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&paths.log_file),
    };
    match file {
        Ok(f) => {
            let writer = std::sync::Mutex::new(f);
            tracing_subscriber::fmt()
                .with_writer(writer)
                .with_ansi(false)
                .with_max_level(level)
                .with_target(true)
                .init();
        }
        Err(e) => {
            eprintln!(
                "udm: cannot open log file {}: {e}",
                paths.log_file.display()
            );
        }
    }
}

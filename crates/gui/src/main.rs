//! udm - a download manager with HTTP/3 (QUIC) support.
//!
//! * no administrator rights, ever (data lives under %LOCALAPPDATA%, or next
//!   to the executable in portable mode)
//! * no telemetry of any kind (see docs/PRIVACY.md)
//!
//! This binary is the Slint front-end; the headless workhorse is `udm-cli`.

// Hide the console window on release builds for Windows.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]
// The slint 1.6 code generator still emits `core::usize::MAX`; silence that.
#![allow(deprecated)]

mod i18n;
mod state;
mod tray;

use std::sync::Arc;

use udm_core::engine::EngineBuilder;
use udm_core::paths::Paths;

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

    let engine = match EngineBuilder::new().paths(paths).build() {
        Ok(e) => Arc::new(e),
        Err(udm_core::error::Error::DatabaseLocked(p)) => {
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

    state::setup(&ui, engine.clone());
    tracing::info!("entering event loop");

    let result = ui.run();
    // Graceful: stop the supervisor, flush redb, close QUIC connections.
    engine.shutdown();
    tracing::info!("udm stopped");

    if let Err(e) = result {
        eprintln!("udm: window error: {e}");
        std::process::exit(1);
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

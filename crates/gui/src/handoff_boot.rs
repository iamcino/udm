//! G5 slice 2: start the browser-handoff coordinator and its named pipe,
//! when the owner has opted in. Two conditions, both required:
//!
//! * a lease is configured (`Settings.handoff_lease_secs`) - the
//!   owner set the production value to 60 s on 2026-11-09 (freeze §5
//!   closed), so fresh configs default to it; an explicitly stored
//!   `None` means fully inert;
//! * at least one browser caller is registered
//!   (`<data>/handoff/callers.json`, written by `udm-host --register`).
//!
//! Any failure is logged and the app runs normally without handoff.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use usagi::engine::Engine;
use usagi::handoff::{
    generate_pairing_secret, read_callers, Coordinator, CoordinatorConfig, EngineAdapter,
    FileStore, SystemClock,
};
use usagi::paths::Paths;

/// Lease passed to the coordinator while the handoff dialog is on.
///
/// The stored `handoff_lease_secs` setting is not rewritten. The dialog
/// counts down to the record deadline, and that deadline is this value:
/// the setting, floored at 10 minutes. No protocol field changes.
pub(crate) fn coordinator_lease_secs(configured: u64) -> u64 {
    configured.max(600)
}

/// Starts the coordinator when opted in and returns it, so the GUI
/// can render recovery rows from the same authoritative instance
/// (owner decision 2026-11-09). `None` = handoff fully inert.
pub fn start(paths: &Paths, engine: Arc<Engine>) -> Option<Arc<Coordinator>> {
    let lease_secs = match engine.settings().handoff_lease_secs {
        Some(secs) if secs > 0 => coordinator_lease_secs(secs),
        _ => {
            tracing::debug!("handoff: no lease configured; coordinator not started");
            return None;
        }
    };
    let callers = read_callers(paths);
    if callers.is_empty() {
        tracing::debug!("handoff: no registered browser callers; coordinator not started");
        return None;
    }
    let secret = generate_pairing_secret();
    let dir = paths.root.join("handoff");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, "handoff: cannot create the handoff directory");
        return None;
    }
    // The pairing secret is replaced on every start; the host re-reads
    // the file per connection, so this never strands a registered host.
    if let Err(e) = std::fs::write(dir.join("pairing.key"), format!("{secret}\n")) {
        tracing::warn!(error = %e, "handoff: cannot write the pairing key");
        return None;
    }
    let coordinator = match Coordinator::open(
        Box::new(FileStore::new(FileStore::journal_path(paths))),
        Arc::new(EngineAdapter::new(engine)),
        Arc::new(SystemClock),
        CoordinatorConfig {
            lease: Duration::from_secs(lease_secs),
            allowed_callers: callers.clone(),
            // The handoff dialog is the only commit path in this build.
            // A pipe commit without a user decision stays Prepared and
            // returns a conflict, so an older host cannot skip the dialog.
            require_decision: true,
        },
    ) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!(error = %e, "handoff: coordinator failed to open");
            return None;
        }
    };
    let gui_coordinator = coordinator.clone();
    tracing::info!(
        callers = callers.len(),
        lease_secs,
        "handoff: coordinator serving the pipe"
    );
    std::thread::spawn(move || {
        let stop = Arc::new(AtomicBool::new(false));
        if let Err(e) = usagi::handoff_pipe::serve(stop, coordinator, secret) {
            tracing::error!(error = %e, "handoff: pipe server stopped");
        }
    });
    Some(gui_coordinator)
}

#[cfg(test)]
mod tests {
    use super::coordinator_lease_secs;

    #[test]
    fn dialog_lease_floors_at_ten_minutes() {
        assert_eq!(coordinator_lease_secs(60), 600);
        assert_eq!(coordinator_lease_secs(600), 600);
        assert_eq!(coordinator_lease_secs(900), 900);
    }
}

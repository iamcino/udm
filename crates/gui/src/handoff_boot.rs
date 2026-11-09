//! G5 slice 2: start the browser-handoff coordinator and its named pipe,
//! when the owner has opted in. Two conditions, both required:
//!
//! * a lease is configured (`Settings.handoff_lease_secs`) - its
//!   production value is the owner's open decision (freeze §5), so the
//!   library ships no default and `None` means fully inert;
//! * at least one browser caller is registered
//!   (`<data>/handoff/callers.json`, written by `udm-host --register`).
//!
//! Any failure is logged and the app runs normally without handoff.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use usagi::engine::Engine;
use usagi::handoff::{
    generate_pairing_secret, read_callers, Coordinator, CoordinatorConfig,
    EngineAdapter, FileStore, SystemClock,
};
use usagi::paths::Paths;

pub fn start(paths: &Paths, engine: Arc<Engine>) {
    let lease_secs = match engine.settings().handoff_lease_secs {
        Some(secs) if secs > 0 => secs,
        _ => {
            tracing::debug!("handoff: no lease configured; coordinator not started");
            return;
        }
    };
    let callers = read_callers(paths);
    if callers.is_empty() {
        tracing::debug!("handoff: no registered browser callers; coordinator not started");
        return;
    }
    let secret = generate_pairing_secret();
    let dir = paths.root.join("handoff");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        tracing::warn!(error = %e, "handoff: cannot create the handoff directory");
        return;
    }
    // The pairing secret is replaced on every start; the host re-reads
    // the file per connection, so this never strands a registered host.
    if let Err(e) = std::fs::write(dir.join("pairing.key"), format!("{secret}\n")) {
        tracing::warn!(error = %e, "handoff: cannot write the pairing key");
        return;
    }
    let coordinator = match Coordinator::open(
        Box::new(FileStore::new(FileStore::journal_path(paths))),
        Arc::new(EngineAdapter::new(engine)),
        Arc::new(SystemClock),
        CoordinatorConfig {
            lease: Duration::from_secs(lease_secs),
            allowed_callers: callers.clone(),
        },
    ) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!(error = %e, "handoff: coordinator failed to open");
            return;
        }
    };
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
}

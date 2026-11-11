//! # udm-host: UDM's local browser bridge
//!
//! Bridges Chrome/Firefox Native Messaging to the running UDM handoff
//! coordinator. It does not own a download engine or start the desktop app.
//!
//! What runs is decided by the invocation shape (see `launch`):
//!
//! * A browser launch — Chromium family: the `chrome-extension://<id>/`
//!   origin, optionally followed by `--parent-window=<handle>`; Firefox:
//!   the application-manifest path plus the add-on ID — runs the bridge:
//!   stdin/stdout carry length-prefixed JSON frames and stdout is
//!   reserved for framed replies. The calling browser and extension are
//!   taken from the launch arguments and validated against the
//!   registered allowed identity in that browser's generated host
//!   manifest (`<data>/handoff/host-manifest-<browser>.json`). A launch
//!   whose identity is not registered is answered `unavailable`; it is
//!   never paired under another browser's identity.
//! * A bare launch (no arguments; developer harnesses and manual stdio
//!   testing) runs the bridge with the legacy caller sidecar
//!   `udm-host.caller` beside this executable.
//! * `--register <chrome|firefox> <extension-id>` and
//!   `--unregister <browser>` are the explicit per-user registry CLI.
//! * Any other shape is rejected: usage on stderr, exit status 2,
//!   stdout untouched.
//!
//! Each bridge request reads the local pairing key, opens a fresh
//! per-user pipe connection, pairs under the validated caller identity
//! and forwards the request. Connection/pairing failure returns
//! `unavailable`; the host never invents a successful commitment or
//! guarantees browser ownership. Coarse diagnostics are appended to
//! `<data>/handoff/host.log`.
//!
//! Registration writes an HKCU Native Messaging key, a host manifest,
//! the coordinator caller list and the `udm-host.caller` sidecar. The
//! sidecar records only the most recent registration; since P1 it is
//! not the bridge's identity source for browser launches (relying on
//! it was the multi-browser mislabelling defect). App and host must
//! resolve the same data root. The current packaging scripts omit this
//! binary; see `extension/README.md` for build, registration and
//! recovery limits.

mod launch;
mod reg;

use launch::{classify, BrowserInvocation, Launch};
use serde_json::{json, Value};
use usagi::handoff_pipe::{pipe_name, read_frame, write_frame, PipeClient};
use usagi::Paths;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match classify(&args) {
        Launch::Maintenance => std::process::exit(reg::run(&args)),
        Launch::Reject => {
            eprintln!("usage: udm-host --register <chrome|firefox> <extension-id>");
            eprintln!("       udm-host --unregister <chrome|firefox>");
            eprintln!("       udm-host            (bridge mode, run by the browser)");
            std::process::exit(2);
        }
        Launch::Bridge(invocation) => bridge(invocation),
    }
}

/// Append one coarse diagnostic line to `<data>/handoff/host.log`.
/// Op and outcome only - no URLs, no transaction contents, no payloads.
fn log(paths: &Paths, line: &str) {
    let dir = paths.root.join("handoff");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let stamp = usagi::util::now_millis();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("host.log"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{stamp} {line}");
    }
}

/// This host's legacy caller identity, from the registration sidecar
/// next to the executable. Used only by bare launches; browser launches
/// validate their own invocation identity instead (see `launch`).
/// `None` means the host was never registered for a browser.
fn read_caller() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let sidecar = exe.with_file_name("udm-host.caller");
    let text = std::fs::read_to_string(sidecar).ok()?;
    let caller = text.trim().to_string();
    if caller.contains(':') && !caller.is_empty() {
        Some(caller)
    } else {
        None
    }
}

fn unavailable(op: &str, tx: &str) -> Value {
    json!({"v": 1, "op": op, "tx": tx, "status": "unavailable"})
}

/// Forward one browser request to the coordinator and return its reply,
/// or the transport-level `unavailable` answer. The caller identity is
/// resolved per request: a browser launch's validated invocation
/// identity, or the legacy sidecar for a bare launch.
fn handle(req: &Value, invocation: &Option<BrowserInvocation>) -> Value {
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");
    let tx = req.get("tx").and_then(Value::as_str).unwrap_or("");
    let paths = match Paths::discover() {
        Ok(p) => p,
        Err(_) => return unavailable(op, tx),
    };
    let caller = match invocation {
        Some(inv) => match launch::registered_caller(&paths.root, inv) {
            Some(c) => c,
            None => {
                log(
                    &paths,
                    "launch identity not registered for this browser; answering unavailable",
                );
                return unavailable(op, tx);
            }
        },
        None => match read_caller() {
            Some(c) => c,
            None => {
                log(&paths, "no registration sidecar; answering unavailable");
                return unavailable(op, tx);
            }
        },
    };
    let secret = match std::fs::read_to_string(paths.root.join("handoff").join("pairing.key")) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            log(
                &paths,
                "no pairing key (coordinator not running); unavailable",
            );
            return unavailable(op, tx);
        }
    };
    let name = match pipe_name() {
        Ok(n) => n,
        Err(_) => return unavailable(op, tx),
    };
    let mut client = match PipeClient::connect(&name) {
        Ok(c) => c,
        Err(_) => {
            log(&paths, "pipe connect failed; unavailable");
            return unavailable(op, tx);
        }
    };
    match client.pair(&secret, &caller) {
        Ok(true) => {}
        _ => {
            log(&paths, "pairing refused; unavailable");
            return unavailable(op, tx);
        }
    }
    match client.request(req) {
        Ok(resp) => resp,
        Err(_) => {
            log(&paths, "pipe request failed; unavailable");
            unavailable(op, tx)
        }
    }
}

fn bridge(invocation: Option<BrowserInvocation>) {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    loop {
        let req = match read_frame(&mut input) {
            Ok(Some(v)) => v,
            _ => return,
        };
        let resp = handle(&req, &invocation);
        if write_frame(&mut output, &resp).is_err() {
            return;
        }
    }
}

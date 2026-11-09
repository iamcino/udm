//! # udm-host: UDM's local browser bridge
//!
//! Bridges Chrome/Firefox Native Messaging to the running UDM handoff
//! coordinator. It does not own a download engine or start the desktop app.
//!
//! With no arguments, stdin/stdout carry length-prefixed JSON frames.
//! Stdout is reserved for framed replies. Each request reads the local
//! pairing key, opens a fresh per-user pipe connection, pairs and forwards
//! the request. Connection/pairing failure returns `unavailable`; the host
//! never invents a successful commitment or guarantees browser ownership.
//! Coarse diagnostics are appended to `<data>/handoff/host.log`.
//!
//! `--register <chrome|firefox> <extension-id>` is an explicit per-user
//! operation: it writes an HKCU Native Messaging key, a host manifest,
//! the coordinator caller list and `udm-host.caller` beside this executable.
//! `--unregister <browser>` removes that browser's key/callers and its
//! matching sidecar. No administrator rights or service are involved.
//!
//! Caller identity comes from the sidecar, not the request payload. There
//! is only one sidecar identity per host directory: a later registration
//! overwrites it. A single host location must not be described as isolating
//! simultaneous Chrome and Firefox identities. App and host must resolve
//! the same data root. The current packaging scripts omit this binary;
//! see `extension/README.md` for build, registration and recovery limits.

mod reg;

use serde_json::{json, Value};
use usagi::handoff_pipe::{pipe_name, read_frame, write_frame, PipeClient};
use usagi::Paths;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if !args.is_empty() {
        std::process::exit(reg::run(&args));
    }
    bridge();
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

/// This host's caller identity, from the registration sidecar next to the
/// executable. `None` means the host was never registered for a browser.
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
/// or the transport-level `unavailable` answer.
fn handle(req: &Value) -> Value {
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");
    let tx = req.get("tx").and_then(Value::as_str).unwrap_or("");
    let paths = match Paths::discover() {
        Ok(p) => p,
        Err(_) => return unavailable(op, tx),
    };
    let caller = match read_caller() {
        Some(c) => c,
        None => {
            log(&paths, "no registration sidecar; answering unavailable");
            return unavailable(op, tx);
        }
    };
    let secret = match std::fs::read_to_string(paths.root.join("handoff").join("pairing.key")) {
        Ok(s) => s.trim().to_string(),
        Err(_) => {
            log(&paths, "no pairing key (coordinator not running); unavailable");
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

fn bridge() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    loop {
        let req = match read_frame(&mut input) {
            Ok(Some(v)) => v,
            _ => return,
        };
        let resp = handle(&req);
        if write_frame(&mut output, &resp).is_err() {
            return;
        }
    }
}

//! `udm-host` - the Native Messaging bridge between a browser extension
//! and the UDM handoff coordinator (G5 slice 2).
//!
//! Two modes:
//!
//! * **Bridge (no arguments)** - what the browser runs. stdin/stdout carry
//!   length-prefixed JSON frames (Native Messaging framing); stdout
//!   carries framed replies ONLY - no banners, no logs, no panic text
//!   (freeze §6). Diagnostics go to a local log file in the data
//!   directory. Each request opens a fresh pipe connection, pairs, and
//!   forwards, so a coordinator restart heals on the next request. Any
//!   failure to reach or pair with the coordinator answers the wire
//!   status `unavailable` and the browser keeps its download - the host
//!   never invents a state.
//! * **Registration (`--register` / `--unregister`)** - run by the user or
//!   the installer, never by the app and never silently (AGENTS.md §7):
//!   writes the HKCU Native Messaging host key + manifest, the caller
//!   sidecar next to this executable, and the coordinator callers list in
//!   the data directory.
//!
//! The caller identity (`chrome:<extension-id>` /
//! `firefox:<extension-id>`) comes from the registration sidecar - it is
//! established by which manifest the browser used to launch this host,
//! never from a payload field (freeze §6).

mod reg;

use serde_json::{json, Value};
use udm_core::handoff_pipe::{pipe_name, read_frame, write_frame, PipeClient};
use udm_core::Paths;

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
    let stamp = udm_core::util::now_millis();
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

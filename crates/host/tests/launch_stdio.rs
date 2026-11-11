//! Process-level stdio tests for `udm-host` launch dispatch (extension
//! P1, blockers B1 and B2).
//!
//! These spawn the real binary with actual browser argument shapes and
//! speak the framed native-messaging protocol to it. No coordinator is
//! running in the test environment, so a bridge-mode launch answers
//! `unavailable` — what matters is *how* it gets there: a browser shape
//! must enter the bridge (framed reply, clean exit at stdin EOF), an
//! undocumented shape must be rejected (exit 2, stdout untouched), and
//! a launch identity must validate against its own browser's
//! registration only (checked via the host's diagnostic log).

use std::io::Write;
use std::process::{Child, Command, Stdio};

use serde_json::{json, Value};
use usagi::handoff_pipe::{read_frame, write_frame};

const BIN: &str = env!("CARGO_BIN_EXE_udm-host");

fn spawn(data_dir: &std::path::Path, args: &[&str]) -> Child {
    Command::new(BIN)
        .args(args)
        .env("UDM_DATA_DIR", data_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn udm-host")
}

/// Send one framed request, close stdin, and return the framed reply.
fn roundtrip(child: &mut Child) -> Value {
    let req = json!({"v": 1, "op": "ping", "tx": "t-1"});
    {
        let mut stdin = child.stdin.take().expect("child stdin");
        write_frame(&mut stdin, &req).expect("write request frame");
        stdin.flush().expect("flush stdin");
        // stdin drops here: the bridge sees EOF after this request and
        // exits cleanly once it has answered.
    }
    let mut stdout = child.stdout.take().expect("child stdout");
    let reply = read_frame(&mut stdout)
        .expect("read reply frame")
        .expect("a framed reply");
    assert_eq!(reply["op"], "ping");
    assert_eq!(reply["tx"], "t-1");
    reply
}

fn host_log(data_dir: &std::path::Path) -> String {
    std::fs::read_to_string(data_dir.join("handoff").join("host.log")).unwrap_or_default()
}

fn write_chrome_manifest(data_dir: &std::path::Path, ext_id: &str) {
    let dir = data_dir.join("handoff");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = json!({
        "name": "pw.cino.udm.handoff.chrome",
        "description": "UDM browser handoff host",
        "path": "udm-host",
        "type": "stdio",
        "allowed_origins": [format!("chrome-extension://{ext_id}/")],
    });
    std::fs::write(
        dir.join("host-manifest-chrome.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

fn write_firefox_manifest(data_dir: &std::path::Path, ext_id: &str) {
    let dir = data_dir.join("handoff");
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = json!({
        "name": "pw.cino.udm.handoff.firefox",
        "description": "UDM browser handoff host",
        "path": "udm-host",
        "type": "stdio",
        "allowed_extensions": [ext_id],
    });
    std::fs::write(
        dir.join("host-manifest-firefox.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
}

#[test]
fn bare_launch_enters_bridge_and_answers_a_frame() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn(dir.path(), &[]);
    let reply = roundtrip(&mut child);
    assert_eq!(reply["status"], "unavailable");
    let status = child.wait().expect("child exit");
    assert!(status.success(), "bridge exits cleanly at EOF: {status}");
}

#[test]
fn chrome_launch_shape_enters_bridge() {
    let dir = tempfile::tempdir().unwrap();
    // The documented Windows Chrome shape: origin + --parent-window.
    let mut child = spawn(
        dir.path(),
        &["chrome-extension://abc123/", "--parent-window=123456"],
    );
    let reply = roundtrip(&mut child);
    assert_eq!(reply["status"], "unavailable");
    // Nothing is registered in this data root, so the bridge must have
    // refused at identity validation — proof it took the bridge path
    // with the parsed invocation, not the usage path (exit 2).
    assert!(
        host_log(dir.path()).contains("launch identity not registered"),
        "log: {}",
        host_log(dir.path())
    );
    let status = child.wait().expect("child exit");
    assert!(status.success(), "bridge exits cleanly at EOF: {status}");
}

#[test]
fn firefox_launch_shape_enters_bridge() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = spawn(
        dir.path(),
        &[
            r"C:\udm\handoff\host-manifest-firefox.json",
            "udm-handoff@cino.pw",
        ],
    );
    let reply = roundtrip(&mut child);
    assert_eq!(reply["status"], "unavailable");
    assert!(
        host_log(dir.path()).contains("launch identity not registered"),
        "log: {}",
        host_log(dir.path())
    );
    let status = child.wait().expect("child exit");
    assert!(status.success(), "bridge exits cleanly at EOF: {status}");
}

#[test]
fn registered_chrome_identity_passes_validation() {
    let dir = tempfile::tempdir().unwrap();
    write_chrome_manifest(dir.path(), "abc123");
    let mut child = spawn(dir.path(), &["chrome-extension://abc123/"]);
    let reply = roundtrip(&mut child);
    assert_eq!(reply["status"], "unavailable");
    // Identity validated; the bridge stopped later, at the missing
    // pairing key (no coordinator running in the test environment).
    let log = host_log(dir.path());
    assert!(
        log.contains("no pairing key"),
        "log should show validation passed: {log}"
    );
    assert!(
        !log.contains("launch identity not registered"),
        "log must not show an identity refusal: {log}"
    );
    let status = child.wait().expect("child exit");
    assert!(status.success(), "bridge exits cleanly at EOF: {status}");
}

#[test]
fn chrome_launch_is_not_relabelled_by_a_firefox_registration() {
    let dir = tempfile::tempdir().unwrap();
    // Only Firefox is registered (the old single-sidecar world would
    // have paired this Chrome launch as firefox:udm-handoff@cino.pw).
    write_firefox_manifest(dir.path(), "udm-handoff@cino.pw");
    let mut child = spawn(dir.path(), &["chrome-extension://abc123/"]);
    let reply = roundtrip(&mut child);
    assert_eq!(reply["status"], "unavailable");
    // The Chrome claim is checked against the Chrome registration,
    // which does not exist: refused at identity validation.
    assert!(
        host_log(dir.path()).contains("launch identity not registered"),
        "log: {}",
        host_log(dir.path())
    );
    let status = child.wait().expect("child exit");
    assert!(status.success(), "bridge exits cleanly at EOF: {status}");
}

#[test]
fn undocumented_shape_is_rejected_with_stdout_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(["--frobnicate"])
        .env("UDM_DATA_DIR", dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run udm-host");
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        out.stdout.is_empty(),
        "stdout is reserved for framed replies and must stay empty"
    );
}

#[test]
fn malformed_maintenance_still_belongs_to_the_registry_cli() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args(["--register"])
        .env("UDM_DATA_DIR", dir.path())
        .stdin(Stdio::null())
        .output()
        .expect("run udm-host");
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--register"),
        "the registry CLI owns this usage text"
    );
}

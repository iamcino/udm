//! Windows named-pipe transport for the handoff coordinator (G5 slice 2).
//!
//! The coordinator (`crate::handoff`) owns no transport; this module is the
//! transport the design handoff settled (F-03): one pipe per user,
//! `\\.\pipe\udm.handoff.<user-SID>`, with a DACL granting only the current
//! user (and LocalSystem), remote clients rejected by the pipe mode, and a
//! pairing handshake as the first frame on every connection - the host
//! presents the per-instance pairing secret plus its caller identity
//! (`chrome:<extension-id>` / `firefox:<extension-id>`), and that caller is
//! fixed for the connection. A request payload never carries a caller
//! (freeze §6).
//!
//! Wire format: u32 little-endian length-prefixed JSON objects, the same
//! framing browsers use for Native Messaging stdio, capped at 1 MiB. The
//! request/reply shapes are the design handoff §2 contract (wire v1);
//! `status` strings are the kebab-case serde forms of
//! [`crate::handoff::HandoffStatus`] plus the transport-produced
//! `unavailable`, which only the host bridge emits (never this server).
//!
//! Threading: the accept loop creates one pipe instance per connection and
//! serves each connection on its own thread with blocking reads. A wedged
//! or slow client therefore occupies only its own thread; the coordinator
//! lock is held only for the duration of one dispatch. Deviation from the
//! design handoff, recorded deliberately: no per-frame read timeout in v1
//! (same-user local threat model; the coordinator and other connections
//! are unaffected by a stuck one).
//!
//! On non-Windows platforms only the framing and dispatch code compiles;
//! the server entry points return an error (UDM ships for Windows).
//!
//! Unsafe code: `usagi` denies `unsafe` crate-wide. The single scoped
//! exception is `windows_impl` below - the Win32 named-pipe and SID
//! surface has no safe std equivalent (std cannot create a named pipe).
//! All unsafe is confined to that module; raw handles are wrapped in
//! `std::fs::File` immediately and never escape.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::{json, Value};

use crate::handoff::{Coordinator, HandoffIntent};

/// Maximum framed message size, both directions (design handoff §2).
pub const MAX_FRAME_BYTES: usize = 1_048_576;

/// Write one length-prefixed JSON frame.
pub fn write_frame<W: Write>(w: &mut W, value: &Value) -> io::Result<()> {
    let bytes =
        serde_json::to_vec(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds the 1 MiB cap",
        ));
    }
    w.write_all(&(bytes.len() as u32).to_le_bytes())?;
    w.write_all(&bytes)?;
    w.flush()
}

/// Read one length-prefixed JSON frame. `Ok(None)` is a clean EOF at a
/// frame boundary; a torn frame is an error.
pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Option<Value>> {
    let mut len_buf = [0u8; 4];
    match r.read_exact(&mut len_buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds the 1 MiB cap",
        ));
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf)?;
    let value =
        serde_json::from_slice(&buf).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(value))
}

/// Turn one wire request into one wire reply against the coordinator.
/// Validation failures never mutate: they answer `conflict` with a coarse
/// detail (freeze §3 knows no generic error status; a rejected request is
/// a conflict without mutation).
fn dispatch(coordinator: &Coordinator, caller: &str, req: &Value) -> Value {
    let op = req.get("op").and_then(Value::as_str).unwrap_or("");
    let tx = req
        .get("tx")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let conflict = |detail: &str| -> Value {
        json!({"v": 1, "op": op, "tx": tx, "status": "conflict", "detail": detail})
    };
    if req.get("v").and_then(Value::as_u64) != Some(1) {
        return conflict("unsupported wire version");
    }
    let bool_field = |name: &str| -> bool {
        req.get("confirm")
            .and_then(|c| c.get(name))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let result = match op {
        "prepare" => {
            let intent = req
                .get("intent")
                .cloned()
                .and_then(|v| serde_json::from_value::<HandoffIntent>(v).ok());
            match intent {
                Some(intent) => coordinator.prepare(caller, &tx, intent),
                None => return conflict("invalid intent"),
            }
        }
        "commit" => coordinator.commit(caller, &tx, bool_field("browser_cancelled")),
        "abort" => coordinator.abort(caller, &tx, bool_field("browser_retains")),
        "query" => coordinator.query(caller, &tx),
        _ => return conflict("unknown op"),
    };
    match result {
        Ok(reply) => {
            let mut out = json!({
                "v": 1,
                "op": op,
                "tx": tx,
                "status": serde_json::to_value(&reply.status).unwrap_or(Value::Null),
            });
            if let Some(state) = &reply.state {
                out["state"] = serde_json::to_value(state).unwrap_or(Value::Null);
            }
            if let Some(task_ref) = &reply.task_ref {
                out["task_ref"] = json!(task_ref);
            }
            if let Some(expires_at) = reply.expires_at_ms {
                out["expires_at"] = json!(expires_at);
            }
            if let Some(detail) = &reply.detail {
                out["detail"] = json!(detail);
            }
            out
        }
        // Invalid transaction id or a caller race: reject without mutation
        // and without leaking internals.
        Err(_) => conflict("request rejected"),
    }
}

/// Constant-time equality for the pairing secret.
fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Serve one paired connection until EOF or a framing error.
fn handle_connection<S: Read + Write>(
    stream: &mut S,
    coordinator: &Coordinator,
    pairing_secret: &str,
) {
    let first = match read_frame(stream) {
        Ok(Some(v)) => v,
        _ => return,
    };
    let caller = first
        .get("caller")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let paired = first
        .get("pairing")
        .and_then(Value::as_str)
        .map(|s| secret_eq(s, pairing_secret))
        .unwrap_or(false)
        && coordinator.caller_allowed(&caller);
    if write_frame(stream, &json!({"ok": paired})).is_err() || !paired {
        return;
    }
    loop {
        match read_frame(stream) {
            Ok(Some(req)) => {
                let resp = dispatch(coordinator, &caller, &req);
                if write_frame(stream, &resp).is_err() {
                    return;
                }
            }
            _ => return,
        }
    }
}

/// A client for the handoff pipe: used by the `udm-host` bridge and by the
/// transport tests. One request in flight per connection, per the design.
#[derive(Debug)]
pub struct PipeClient {
    stream: std::fs::File,
}

impl PipeClient {
    /// Connect to the pipe at `pipe_name`. Retries briefly only while every
    /// instance is busy (ERROR_PIPE_BUSY, 231); a missing pipe fails at
    /// once so the host can answer `unavailable` without stalling.
    pub fn connect(pipe_name: &str) -> io::Result<PipeClient> {
        let mut attempts = 0;
        loop {
            match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(pipe_name)
            {
                Ok(stream) => return Ok(PipeClient { stream }),
                Err(e) => {
                    attempts += 1;
                    if e.raw_os_error() != Some(231) || attempts >= 40 {
                        return Err(e);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
    }

    /// Perform the pairing handshake; the caller is fixed for the
    /// connection from this point on.
    pub fn pair(&mut self, pairing_secret: &str, caller: &str) -> io::Result<bool> {
        write_frame(
            &mut self.stream,
            &json!({"pairing": pairing_secret, "caller": caller}),
        )?;
        match read_frame(&mut self.stream)? {
            Some(v) => Ok(v.get("ok").and_then(Value::as_bool).unwrap_or(false)),
            None => Ok(false),
        }
    }

    /// Send one request, read its one reply.
    pub fn request(&mut self, req: &Value) -> io::Result<Value> {
        write_frame(&mut self.stream, req)?;
        read_frame(&mut self.stream)?.ok_or_else(|| {
            io::Error::new(io::ErrorKind::UnexpectedEof, "coordinator closed the pipe")
        })
    }
}

#[cfg(windows)]
pub use windows_impl::{current_user_sid, pipe_name, serve};

#[cfg(not(windows))]
mod stub_impl {
    use super::*;

    pub fn current_user_sid() -> io::Result<String> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "handoff pipe exists only on Windows",
        ))
    }

    pub fn pipe_name() -> io::Result<String> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "handoff pipe exists only on Windows",
        ))
    }

    pub fn serve(
        _stop: Arc<AtomicBool>,
        _coordinator: Arc<Coordinator>,
        _pairing_secret: String,
    ) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "handoff pipe exists only on Windows",
        ))
    }
}

#[cfg(not(windows))]
pub use stub_impl::{current_user_sid, pipe_name, serve};

#[cfg(windows)]
#[allow(unsafe_code)]
mod windows_impl {
    use super::*;
    use std::ffi::c_void;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};

    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, LocalFree, ERROR_PIPE_CONNECTED, HANDLE, HLOCAL,
        INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, TokenUser, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows_sys::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
        PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    fn to_wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The current user's SID in string form (e.g. `S-1-5-21-...-1001`).
    pub fn current_user_sid() -> io::Result<String> {
        unsafe {
            let mut token: HANDLE = 0;
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len: u32 = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr() as *mut c_void,
                len,
                &mut len,
            );
            CloseHandle(token);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let token_user = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut sid_str: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(token_user.User.Sid, &mut sid_str) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut chars: Vec<u16> = Vec::new();
            let mut p = sid_str;
            while *p != 0 {
                chars.push(*p);
                p = p.add(1);
            }
            LocalFree(sid_str as HLOCAL);
            String::from_utf16(&chars).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
        }
    }

    /// The settled endpoint (design handoff F-03): deterministic per user,
    /// which is the discovery mechanism; the name is not a credential.
    pub fn pipe_name() -> io::Result<String> {
        Ok(format!(r"\\.\pipe\udm.handoff.{}", current_user_sid()?))
    }

    /// Create one pipe instance with a DACL for the current user and
    /// LocalSystem only.
    fn create_instance(name: &str, sid: &str) -> io::Result<HANDLE> {
        unsafe {
            let sddl = to_wide(&format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)"));
            let mut descriptor: *mut c_void = std::ptr::null_mut();
            // 1 == SDDL_REVISION_1.
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let wide_name = to_wide(name);
            let handle = CreateNamedPipeW(
                wide_name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                65_536,
                65_536,
                0,
                &attributes,
            );
            LocalFree(descriptor as HLOCAL);
            if handle == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            Ok(handle)
        }
    }

    /// Blocking accept loop: one instance per connection, one thread per
    /// connection. Runs until `stop` is set and one more connection
    /// attempt arrives; the GUI never stops it (process lifetime).
    pub fn serve(
        stop: Arc<AtomicBool>,
        coordinator: Arc<Coordinator>,
        pairing_secret: String,
    ) -> io::Result<()> {
        let sid = current_user_sid()?;
        let name = format!(r"\\.\pipe\udm.handoff.{sid}");
        while !stop.load(Ordering::Relaxed) {
            let handle = create_instance(&name, &sid)?;
            let connected = unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            if connected == 0 {
                let err = unsafe { GetLastError() };
                if err != ERROR_PIPE_CONNECTED {
                    unsafe {
                        CloseHandle(handle);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
            }
            if stop.load(Ordering::Relaxed) {
                unsafe {
                    DisconnectNamedPipe(handle);
                    CloseHandle(handle);
                }
                break;
            }
            let coordinator = coordinator.clone();
            let secret = pairing_secret.clone();
            std::thread::spawn(move || {
                let mut file = unsafe { std::fs::File::from_raw_handle(handle as _) };
                handle_connection(&mut file, &coordinator, &secret);
                unsafe {
                    // Push the final reply (notably a pairing refusal)
                    // to the client before disconnecting; disconnecting
                    // with bytes in flight discards them (the client
                    // would see ERROR_NO_DATA instead of the reply).
                    FlushFileBuffers(file.as_raw_handle() as HANDLE);
                    DisconnectNamedPipe(file.as_raw_handle() as HANDLE);
                }
                drop(file);
            });
        }
        Ok(())
    }
}

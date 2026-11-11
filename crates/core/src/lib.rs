//! # Usagi: the engine behind Universal Download Manager
//!
//! The `usagi` package provides HTTP/1.1, HTTP/2 and HTTP/3 (QUIC) downloads
//! for UDM's desktop and command-line front-ends. It owns queue admission,
//! segmented transfers, optional filename-based routing and checksum checks.
//! Synced progress checkpoints, representation binding, run-generation fences
//! and a durable finalization journal govern reuse and completion of files.
//!
//! ## Behaviour and boundaries
//!
//! - HTTP/3 uses quinn and h3; HTTP/2 and HTTP/1.1 use reqwest. Transport
//!   preference controls fallback: a failed H3-only attempt for an eligible
//!   HTTPS hostname is returned. Plain HTTP/IP-literal URLs and the global
//!   HTTP/3-off setting still take the HTTP/1.1/2 path in this implementation.
//! - Saved bytes need a valid local checkpoint and a matching representation
//!   binding for reuse. Weak ETags and Last-Modified alone are insufficient.
//! - [`Engine`] owns its task store. A GUI and CLI cannot independently open
//!   the same database while it is locked. Browser handoff uses the separate
//!   local coordinator/pipe boundary and persisted task transaction markers.
//! - No telemetry, remote crash reporting, or metric export is implemented.
//!   [`update_pin`] holds the compiled publisher trust root. [`update_manifest`]
//!   verifies a signed manifest. [`update_fetch`] downloads a manifest and
//!   payload only when the update flow calls it. There is no polling.
//!   Local instrumentation is not an engine-wide connection census;
//!   unavailable totals must not be displayed as zero.
//! - No administrator rights or services are required. [`Paths`] resolves
//!   user data to a per-user directory or `data/` beside the executable in
//!   portable mode; download destinations are selected independently.
//!
//! The application adds disclosed per-user startup and browser registration
//! outside the engine's transfer path. See the repository's `docs/PRIVACY.md`,
//! `docs/ARCHITECTURE.md` and `docs/ENGINE.md` for the complete product contract.
//!
//! ## Module map
//!
//! | Module | Responsibility |
//! |---|---|
//! | [`engine`] | Task API, supervision, queue admission and segment workers. |
//! | [`net`] | Transport selection, HTTP clients, TLS, Alt-Svc and local gauges. |
//! | [`store`] | redb records, settings and schema marker 2. |
//! | [`category`] | Validated filename-suffix routing shared with previews. |
//! | [`config`] | Persisted settings and supported key/value updates. |
//! | [`hash`] | SHA-256/SHA-512/SHA-1/MD5/BLAKE3 verification. |
//! | [`handoff`] | Durable browser ownership transactions and recovery entries. |
//! | [`handoff_pipe`] | Framing, pairing and Windows per-user named-pipe transport. |
//! | [`paths`] | Data and default download location discovery, including legacy fallbacks. |
//! | [`update_pin`] | U2 publisher pin: compiled Ed25519 trust root, no network. |
//! | [`update_manifest`] | U2 manifest parse and Ed25519 verification. No network. |
//! | [`update_fetch`] | U3 fetch: two configured origins, verified manifest, payload handoff. |
//! | [`error`] | Shared errors. |
//! | [`util`] | Filename, path, byte-count and duration helpers. |
//!
//! Internal durability and finalization modules enforce file-sync ordering
//! and reconcile rename/database boundaries. They are not a public updater
//! API. Timed scheduling, a global speed limit, torrents and media resolving
//! are not implemented here. Manifest verification ([`update_manifest`]) and
//! fetch ([`update_fetch`]) are implemented. Staging, rename swap, and
//! rollback are not. N2 dual-stack (IPv4/IPv6 selection) and N3 multi-family
//! transfers remain measurement-gated and are not built.

pub mod category;
pub mod config;
pub(crate) mod durability;
pub mod engine;
pub mod error;
pub(crate) mod finalize;
pub mod handoff;
pub mod handoff_pipe;
pub mod hash;
pub mod net;
pub mod paths;
pub mod store;
pub mod update_fetch;
pub mod update_manifest;
pub mod update_pin;
pub mod util;

pub use config::{ConflictPolicy, Settings, TransportPref};
pub use engine::{AddRequest, Engine, EngineBuilder, Event, LogLevel, TaskSnapshot};
pub use error::{Error, Result};
pub use hash::{ExpectedHash, HashAlgo};
pub use paths::Paths;
pub use store::{
    RouteState, SegmentState, Store, StoreStats, StoredTask, TaskRecord, TaskState, TransportKind,
};

/// Crate version, shown in the About view and sent in the default `User-Agent`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Default `User-Agent`. Honest and boring: it identifies the client and
/// nothing about the machine.
pub const DEFAULT_USER_AGENT: &str = concat!("udm/", env!("CARGO_PKG_VERSION"));

/// Telemetry endpoints this crate contacts. The list is empty.
/// `tests/no_telemetry.rs` checks dependency names, reporting-endpoint source
/// patterns and selected socket-construction strings; it does not assert
/// this constant.
pub const TELEMETRY_ENDPOINTS: &[&str] = &[];

/// Set to `true` if this build is a debug build (used by the About view).
pub const IS_DEBUG: bool = cfg!(debug_assertions);

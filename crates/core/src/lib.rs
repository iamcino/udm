//! # udm-core
//!
//! The download engine behind `udm`: HTTP/1.1, HTTP/2 and HTTP/3 (QUIC)
//! transports, segmented transfers with resume, checksum verification and
//! crash-safe persistence in a single [`redb`] file.
//!
//! ## Hard rules this crate is built around
//!
//! 1. **No telemetry.** There is no analytics, no crash reporting, no metric
//!    export, no "check for updates" beacon and no outbound connection that the
//!    user did not explicitly request. See `docs/PRIVACY.md` and the
//!    `tests/no_telemetry.rs` guard test.
//! 2. **No privileges.** Nothing here ever asks for elevation, writes to the
//!    registry, installs a service, or touches a directory outside the user
//!    profile (unless the user explicitly picks one). [`paths::Paths`] resolves
//!    everything under `%LOCALAPPDATA%` (or next to the executable in portable
//!    mode).
//! 3. **HTTP/3 first, with honest fallback.** QUIC/HTTP3 is attempted whenever
//!    the origin advertises `Alt-Svc: h3` (or the user forces it); failures
//!    degrade to HTTP/2 or HTTP/1.1 automatically and are remembered so we do
//!    not keep paying the handshake cost.
//!
//! ## Layout
//!
//! | module         | responsibility                                             |
//! |----------------|------------------------------------------------------------|
//! | [`category`]   | filename-based download routing (G2)                       |
//! | [`config`]     | user settings (serialised into redb)                       |
//! | [`durability`] | versioned progress proofs (data-sync receipts)             |
//! | [`engine`]     | task lifecycle, queueing, segment workers                  |
//! | [`error`]      | one error type for the whole engine                        |
//! | [`finalize`]   | durable rename transactions (finalize intent)              |
//! | [`hash`]       | MD5/SHA-1/SHA-256/SHA-512/BLAKE3 verification              |
//! | [`net`]        | HTTP/3 (quinn+h3) and HTTP/1.1-2 (reqwest) transports      |
//! | [`paths`]      | privilege-free filesystem locations                        |
//! | [`store`]      | redb schema, records, Alt-Svc cache                        |
//! | [`util`]       | filename/URL/size/duration helpers                         |

pub mod category;
pub mod config;
pub(crate) mod durability;
pub(crate) mod finalize;
pub mod engine;
pub mod error;
pub mod hash;
pub mod net;
pub mod paths;
pub mod store;
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

/// Telemetry endpoints this crate contacts. The list is empty and is asserted
/// to stay empty by `tests/no_telemetry.rs`.
pub const TELEMETRY_ENDPOINTS: &[&str] = &[];

/// Set to `true` if this build is a debug build (used by the About view).
pub const IS_DEBUG: bool = cfg!(debug_assertions);

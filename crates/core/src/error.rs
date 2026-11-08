//! The single error type used across the engine.

use std::path::PathBuf;

/// Convenience alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong while planning, running or persisting a
/// download.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("unsupported URL scheme `{0}` (only http:// and https:// are supported)")]
    UnsupportedScheme(String),

    #[error("server returned HTTP {status} for {url}")]
    HttpStatus { status: u16, url: String },

    #[error("server does not support partial downloads (Range requests); falling back to a single connection")]
    NoRangeSupport,

    #[error("the remote file changed while downloading (ETag/Last-Modified mismatch); the task must be restarted")]
    RemoteChanged,

    #[error("server ignored the Range request and sent the whole body again")]
    RangeIgnored,

    #[error("server reports the file is already fully downloaded (HTTP 416)")]
    RangeNotSatisfiable,

    /// The origin sent a content coding other than identity. Ranged bytes
    /// would be the decoded body, which this engine does not store. The
    /// failure is terminal: a restart would see the same coding.
    #[error("the server encoded the body; this engine will not store a ranged decoded body")]
    UnsupportedCoding,

    #[error("invalid download response: {0}")]
    InvalidResponse(String),

    #[error("connection failed: {0}")]
    Connect(String),

    #[error("DNS resolution failed for `{0}`")]
    Dns(String),

    #[error("TLS error: {0}")]
    Tls(String),

    #[error("QUIC/HTTP3 error: {0}")]
    Http3(String),

    #[error("no data received for {secs}s")]
    Stall { secs: u64 },

    #[error("operation timed out after {secs}s")]
    Timeout { secs: u64 },

    #[error("checksum mismatch ({algo}): expected {expected}, computed {actual}")]
    ChecksumMismatch {
        algo: String,
        expected: String,
        actual: String,
    },

    #[error("unknown checksum algorithm `{0}` (expected md5, sha1, sha256, sha512 or blake3)")]
    UnknownHashAlgo(String),

    #[error("invalid checksum `{0}` (use `algo:hex`, e.g. `sha256:9f86d0...`)")]
    InvalidHashSpec(String),

    #[error("I/O error while accessing {}: {source}", path.display())]
    IoPath {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("database error: {0}")]
    Storage(String),

    #[error("the udm database is locked by another running instance ({0})")]
    DatabaseLocked(PathBuf),

    #[error("task {0} not found")]
    NotFound(u64),

    #[error("task was cancelled")]
    Cancelled,

    /// A worker, checkpoint or finalize attempt belongs to a run of the task
    /// that has been superseded. It must not touch the task's current life.
    #[error("superseded task run (generation {stale}, current {current})")]
    StaleGeneration { stale: u64, current: u64 },

    #[error("destination already exists: {0}")]
    Conflict(PathBuf),

    #[error("category routing failed: {0}")]
    Route(String),

    #[error("finalization requires recovery: {0}")]
    Finalize(String),

    #[error("operation not allowed in state `{0}`")]
    BadState(String),

    #[error("not supported: {0}")]
    Unsupported(String),

    #[error("{0}")]
    Other(String),
}

impl Error {
    /// Errors that are worth retrying automatically (transient network woes).
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::Connect(_)
                | Error::Dns(_)
                | Error::Stall { .. }
                | Error::Timeout { .. }
                | Error::Http3(_)
                | Error::Io(_)
                | Error::IoPath { .. }
        ) || match self {
            Error::HttpStatus { status, .. } => {
                matches!(
                    status,
                    408 | 425 | 429 | 500 | 502 | 503 | 504 | 509 | 520..=527
                )
            }
            _ => false,
        }
    }

    /// Errors that mean "this URL is not going to work, stop retrying".
    pub fn is_fatal(&self) -> bool {
        matches!(
            self,
            Error::InvalidUrl(_)
                | Error::UnsupportedScheme(_)
                | Error::RemoteChanged
                | Error::RangeIgnored
                | Error::RangeNotSatisfiable
                | Error::UnsupportedCoding
                | Error::InvalidResponse(_)
                | Error::ChecksumMismatch { .. }
                | Error::Conflict(_)
                | Error::Route(_)
                  | Error::Finalize(_)
                | Error::Unsupported(_)
        ) || matches!(self, Error::HttpStatus { status, .. } if (400..500).contains(status) && !matches!(status, 408 | 425 | 429))
    }

    /// One-line description suitable for the UI's status column.
    pub fn short(&self) -> String {
        let s = self.to_string();
        s.chars().take(160).collect()
    }
}

impl From<url::ParseError> for Error {
    fn from(e: url::ParseError) -> Self {
        Error::InvalidUrl(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Storage(format!("settings are corrupt: {e}"))
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            return Error::Timeout { secs: 0 };
        }
        if e.is_connect() {
            return Error::Connect(e.to_string());
        }
        if let Some(status) = e.status() {
            return Error::HttpStatus {
                status: status.as_u16(),
                url: e.url().map(|u| u.to_string()).unwrap_or_default(),
            };
        }
        if e.is_decode() || e.is_body() {
            return Error::Http3(format!("http body: {e}"));
        }
        Error::Connect(e.to_string())
    }
}

impl From<rustls::Error> for Error {
    fn from(e: rustls::Error) -> Self {
        Error::Tls(e.to_string())
    }
}

impl From<quinn::ConnectError> for Error {
    fn from(e: quinn::ConnectError) -> Self {
        match e {
            quinn::ConnectError::EndpointStopping => Error::Http3(e.to_string()),
            quinn::ConnectError::InvalidRemoteAddress(addr) => {
                Error::Connect(format!("invalid remote address {addr}"))
            }
            other => Error::Http3(other.to_string()),
        }
    }
}

impl From<quinn::ConnectionError> for Error {
    fn from(e: quinn::ConnectionError) -> Self {
        Error::Http3(e.to_string())
    }
}

impl From<h3::error::ConnectionError> for Error {
    fn from(e: h3::error::ConnectionError) -> Self {
        Error::Http3(e.to_string())
    }
}

impl From<h3::error::StreamError> for Error {
    fn from(e: h3::error::StreamError) -> Self {
        Error::Http3(e.to_string())
    }
}

impl From<std::num::ParseIntError> for Error {
    fn from(e: std::num::ParseIntError) -> Self {
        Error::Other(e.to_string())
    }
}

impl Error {
    /// Wrap an `std::io::Error` with the path that caused it.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::IoPath {
            path: path.into(),
            source,
        }
    }

    /// Wrap any `Display`able storage error.
    pub fn storage(e: impl std::fmt::Display) -> Self {
        Error::Storage(e.to_string())
    }
}

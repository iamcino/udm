//! `Alt-Svc` learning: how udm discovers that an origin speaks HTTP/3.
//!
//! Servers advertise QUIC with `alt-svc: h3=":443"; ma=86400`. We remember the
//! advertisement (in memory and in redb) so the *next* request can start on
//! QUIC instead of paying for a TCP+TLS handshake first, and we count
//! consecutive failures so a broken QUIC path stops being retried.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::util;

/// After this many consecutive failures the entry is considered broken and
/// HTTP/3 is no longer attempted for that origin until the entry expires.
pub const FAILURE_LIMIT: u32 = 3;

/// A cached HTTP/3 endpoint for one `https` origin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AltSvcEntry {
    /// Cache key: `host:port` of the HTTPS origin, lower-cased.
    pub origin: String,
    /// Host name to dial over QUIC.
    pub host: String,
    /// UDP port to dial over QUIC.
    pub port: u16,
    /// When the advertisement was seen (unix ms).
    pub learned_at: i64,
    /// `ma=` parameter from the header (seconds).
    pub max_age_secs: u64,
    /// Consecutive QUIC failures for this origin.
    pub failures: u32,
}

impl AltSvcEntry {
    pub fn new(host: &str, port: u16, max_age_secs: u64) -> Self {
        AltSvcEntry {
            origin: origin_key(host, port),
            host: host.to_ascii_lowercase(),
            port,
            learned_at: util::now_millis(),
            max_age_secs,
            failures: 0,
        }
    }

    pub fn with_learned_at(mut self, now_millis: i64) -> Self {
        self.learned_at = now_millis;
        self
    }

    pub fn expires_at(&self, cap_secs: u64) -> i64 {
        let age = self.max_age_secs.min(cap_secs).saturating_mul(1000) as i64;
        self.learned_at.saturating_add(age)
    }

    /// Still fresh and not marked broken.
    pub fn is_valid(&self, now_millis: i64, cap_secs: u64) -> bool {
        !self.is_broken() && now_millis < self.expires_at(cap_secs)
    }

    pub fn is_broken(&self) -> bool {
        self.failures >= FAILURE_LIMIT
    }

    pub fn record_failure(&mut self) {
        self.failures = self.failures.saturating_add(1);
    }
}

/// Cache key for an origin.
pub fn origin_key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.to_ascii_lowercase())
}

/// Storage backend for [`AltSvcEntry`] values. Implemented by
/// [`crate::store::Store`] (persistent) and [`MemAltSvc`] (tests, `--no-db`).
pub trait AltSvcStore: Send + Sync {
    fn get(&self, origin: &str) -> Option<AltSvcEntry>;
    fn put(&self, entry: AltSvcEntry);
    fn record_failure(&self, origin: &str);
    fn record_success(&self, origin: &str);
}

/// Volatile implementation, also handy in unit tests.
#[derive(Debug, Default)]
pub struct MemAltSvc {
    map: Mutex<HashMap<String, AltSvcEntry>>,
}

impl MemAltSvc {
    pub fn new() -> Self {
        Self::default()
    }
}

impl AltSvcStore for MemAltSvc {
    fn get(&self, origin: &str) -> Option<AltSvcEntry> {
        self.map
            .lock()
            .ok()?
            .get(&origin.to_ascii_lowercase())
            .cloned()
    }

    fn put(&self, entry: AltSvcEntry) {
        if let Ok(mut map) = self.map.lock() {
            map.insert(entry.origin.to_ascii_lowercase(), entry);
        }
    }

    fn record_failure(&self, origin: &str) {
        if let Some(mut e) = self.get(origin) {
            e.record_failure();
            self.put(e);
        }
    }

    fn record_success(&self, origin: &str) {
        if let Some(mut e) = self.get(origin) {
            if e.failures != 0 {
                e.failures = 0;
                self.put(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validity_honours_max_age_and_cap() {
        let now = 1_000_000i64;
        let e = AltSvcEntry::new("cdn.example", 443, 60).with_learned_at(now);
        assert!(e.is_valid(now + 10_000, 86400));
        assert!(!e.is_valid(now + 61_000, 86400));
        // The user lowered the cap -> entry expires sooner.
        let e2 = AltSvcEntry::new("cdn.example", 443, 86400).with_learned_at(now);
        assert!(!e2.is_valid(now + 120_000, 60));
    }

    #[test]
    fn failures_break_an_entry() {
        let mut e = AltSvcEntry::new("cdn.example", 443, 3600);
        for _ in 0..FAILURE_LIMIT {
            e.record_failure();
        }
        assert!(e.is_broken());
        assert!(!e.is_valid(util::now_millis(), 86400));
    }

    #[test]
    fn origin_key_is_case_insensitive() {
        assert_eq!(origin_key("CDN.Example", 443), "cdn.example:443");
    }

    #[test]
    fn mem_store_round_trip() {
        let s = MemAltSvc::new();
        assert!(s.get("a.example:443").is_none());
        s.put(AltSvcEntry::new("a.example", 443, 60));
        assert!(s.get("a.example:443").is_some());
        s.record_failure("a.example:443");
        assert_eq!(s.get("a.example:443").unwrap().failures, 1);
        s.record_success("a.example:443");
        assert_eq!(s.get("a.example:443").unwrap().failures, 0);
    }
}

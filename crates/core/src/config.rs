//! User settings. Everything is stored locally in the redb database; nothing is
//! ever sent anywhere.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::paths::{display, system_download_dir};
use crate::DEFAULT_USER_AGENT;

/// Which transport the engine should prefer for a task.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum TransportPref {
    /// Use HTTP/3 when the origin advertises it, otherwise HTTP/2 or 1.1.
    #[default]
    Auto,
    /// Try HTTP/3 first even without a cached `Alt-Svc` entry; fall back on error.
    PreferHttp3,
    /// Only HTTP/3. The task fails if the origin has no QUIC endpoint.
    Http3Only,
    /// Never use HTTP/3 (HTTP/2 and HTTP/1.1 only).
    NoHttp3,
}

impl TransportPref {
    pub fn as_str(&self) -> &'static str {
        match self {
            TransportPref::Auto => "auto",
            TransportPref::PreferHttp3 => "prefer-http3",
            TransportPref::Http3Only => "http3-only",
            TransportPref::NoHttp3 => "no-http3",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" | "" => Ok(TransportPref::Auto),
            "prefer-http3" | "prefer-h3" | "h3" => Ok(TransportPref::PreferHttp3),
            "http3-only" | "h3-only" | "quic-only" => Ok(TransportPref::Http3Only),
            "no-http3" | "no-h3" | "http2" | "http1" => Ok(TransportPref::NoHttp3),
            other => Err(Error::Other(format!(
                "unknown transport preference `{other}`"
            ))),
        }
    }

    /// May this task use HTTP/3 at all?
    pub fn http3_allowed(&self) -> bool {
        !matches!(self, TransportPref::NoHttp3)
    }

    /// Should we dial QUIC before we have seen an `Alt-Svc` header?
    pub fn http3_eager(&self) -> bool {
        matches!(self, TransportPref::PreferHttp3 | TransportPref::Http3Only)
    }
}

/// What to do when the destination file already exists.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    /// `movie.mkv` -> `movie (1).mkv`
    #[default]
    UniqueSuffix,
    /// Replace the existing file.
    Overwrite,
    /// Refuse to start and report the conflict.
    Fail,
}

impl ConflictPolicy {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConflictPolicy::UniqueSuffix => "unique-suffix",
            ConflictPolicy::Overwrite => "overwrite",
            ConflictPolicy::Fail => "fail",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "unique-suffix" | "rename" | "unique" => Ok(ConflictPolicy::UniqueSuffix),
            "overwrite" | "replace" => Ok(ConflictPolicy::Overwrite),
            "fail" | "error" => Ok(ConflictPolicy::Fail),
            other => Err(Error::Other(format!("unknown conflict policy `{other}`"))),
        }
    }
}

/// All tunables. Serialised as JSON into the `settings` table.
///
/// There is deliberately **no** field for analytics, telemetry, update
/// endpoints, device identifiers or "improve the product" opt-ins.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    // --- queue ---------------------------------------------------------------
    /// How many tasks may transfer at the same time.
    pub max_concurrent_tasks: u32,
    /// Connections (segments) per task.
    pub segments_per_task: u32,
    /// Never create a segment smaller than this (bytes).
    pub min_segment_bytes: u64,

    // --- destinations --------------------------------------------------------
    /// Empty means "the system Downloads folder".
    pub default_download_dir: String,
    /// G2: route new downloads into per-category subfolders of the download
    /// root. Off by default, and existing configs load with it off.
    pub auto_categorize: bool,
    /// G2: category rules (id, folder, suffixes). `None` means the built-in
    /// defaults from `category::default_categories()`.
    pub categories: Option<Vec<crate::category::CategoryRule>>,

    // --- transport -----------------------------------------------------------
    pub transport_pref: TransportPref,
    /// Enable the QUIC/HTTP3 stack at all.
    pub http3_enabled: bool,
    /// Remember `Alt-Svc: h3` advertisements and reuse them.
    pub learn_alt_svc: bool,
    /// Seconds a cached `Alt-Svc` entry stays usable at most.
    pub alt_svc_max_age_secs: u64,
    /// Use the OS proxy settings (WinHTTP/IE proxy) for HTTP/1.1+2 requests.
    pub use_system_proxy: bool,
    pub user_agent: String,
    /// Maximum number of HTTP redirects to follow.
    pub max_redirects: u32,

    // --- robustness ----------------------------------------------------------
    pub connect_timeout_secs: u64,
    /// Abort a connection that delivers nothing for this long.
    pub stall_timeout_secs: u64,
    pub max_retries: u32,
    /// Bytes buffered per segment before hitting the disk.
    pub write_buffer_bytes: usize,

    // --- integrity -----------------------------------------------------------
    /// Verify a supplied checksum after the download completes and refuse to
    /// rename the `.part` file when it does not match.
    pub verify_checksums: bool,

    // --- housekeeping --------------------------------------------------------
    pub conflict_policy: ConflictPolicy,
    /// How often in-flight progress is flushed to redb (ms).
    pub persist_interval_ms: u64,
    /// Delete `.part` files when a task is removed from the list.
    pub delete_part_on_remove: bool,
    /// Keep completed tasks in the list forever (`false` prunes after N days).
    pub keep_history_days: u32,

    // --- ui ------------------------------------------------------------------
    /// "dark", "light" or "auto".
    pub theme: String,
    /// Desktop language: "en" or "vi". Protocol/error diagnostics stay verbatim.
    pub language: String,
    /// Curated desktop accent: blue, green, orange, pink, or purple.
    pub accent: String,
    /// tracing level written to the local log file.
    pub log_level: String,

    // --- browser handoff (G5) ----------------------------------------------
    /// Lease for a prepared browser handoff, in seconds. The owner set
    /// the production value to 60 s on 2026-11-09 (closing the freeze
    /// section 5 question), so fresh configs default to `Some(60)`. An
    /// explicitly stored `null` still loads as `None` — the coordinator
    /// is never started and the feature is fully inert — so installs
    /// that persisted `None` are never silently enabled. Serving the
    /// pipe additionally requires the disclosed registration (a callers
    /// list), which remains the user opt-in gate.
    pub handoff_lease_secs: Option<u64>,

    // --- window lifecycle (G3) ---------------------------------------------
    /// Closing the window hides it to the tray instead of exiting
    /// (default on, per the settled G3 defaults). The tray menu's Exit
    /// item always exits for real.
    pub close_to_tray: bool,
    /// Minimizing the window hides it to the tray (default off).
    pub minimize_to_tray: bool,
    /// Start with the main window hidden in the tray (default off).
    pub start_minimized: bool,
    /// Register the per-user HKCU Run entry so UDM starts at logon
    /// (default off; disclosed opt-in, see `gui::startup`).
    pub start_with_windows: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            max_concurrent_tasks: 3,
            segments_per_task: 8,
            min_segment_bytes: 2 * 1024 * 1024,
            default_download_dir: String::new(),
            auto_categorize: false,
            categories: None,
            transport_pref: TransportPref::Auto,
            http3_enabled: true,
            learn_alt_svc: true,
            alt_svc_max_age_secs: 24 * 3600,
            use_system_proxy: true,
            user_agent: DEFAULT_USER_AGENT.to_string(),
            max_redirects: 10,
            connect_timeout_secs: 20,
            stall_timeout_secs: 30,
            max_retries: 5,
            write_buffer_bytes: 256 * 1024,
            verify_checksums: true,
            conflict_policy: ConflictPolicy::UniqueSuffix,
            persist_interval_ms: 1000,
            delete_part_on_remove: true,
            keep_history_days: 0,
            theme: "dark".to_string(),
            language: "en".to_string(),
            accent: "blue".to_string(),
            log_level: "info".to_string(),
            handoff_lease_secs: Some(60),
            close_to_tray: true,
            minimize_to_tray: false,
            start_minimized: false,
            start_with_windows: false,
        }
    }
}

impl Settings {
    /// Clamp nonsense values into a sane range (also used after loading from
    /// disk, so a hand-edited database cannot make the engine misbehave).
    pub fn sanitize(&mut self) {
        if !matches!(self.language.as_str(), "en" | "vi") {
            self.language = "en".into();
        }
        if !matches!(
            self.accent.as_str(),
            "blue" | "green" | "orange" | "pink" | "purple"
        ) {
            self.accent = "blue".into();
        }
        self.max_concurrent_tasks = self.max_concurrent_tasks.clamp(1, 32);
        self.segments_per_task = self.segments_per_task.clamp(1, 64);
        self.min_segment_bytes = self.min_segment_bytes.clamp(64 * 1024, 1024 * 1024 * 1024);
        self.connect_timeout_secs = self.connect_timeout_secs.clamp(3, 300);
        self.stall_timeout_secs = self.stall_timeout_secs.clamp(5, 3600);
        self.max_retries = self.max_retries.clamp(0, 100);
        self.max_redirects = self.max_redirects.clamp(0, 50);
        self.write_buffer_bytes = self.write_buffer_bytes.clamp(16 * 1024, 64 * 1024 * 1024);
        self.persist_interval_ms = self.persist_interval_ms.clamp(200, 60_000);
        self.alt_svc_max_age_secs = self.alt_svc_max_age_secs.clamp(60, 30 * 86400);
        if self.user_agent.trim().is_empty() {
            self.user_agent = DEFAULT_USER_AGENT.to_string();
        }
        if !matches!(self.theme.as_str(), "dark" | "light" | "auto") {
            self.theme = "dark".to_string();
        }
        if !matches!(
            self.log_level.as_str(),
            "off" | "error" | "warn" | "info" | "debug" | "trace"
        ) {
            self.log_level = "info".to_string();
        }
        // G2: a stored rule set that no longer validates (hand-edited db,
        // a rule the contract now rejects) must not break every download:
        // drop it back to the built-in defaults rather than failing loads.
        if let Some(rules) = &self.categories {
            if crate::category::validate_categories(rules).is_err() {
                self.categories = None;
            }
        }
        if !self.http3_enabled {
            self.transport_pref = TransportPref::NoHttp3;
        }
        if self.transport_pref == TransportPref::NoHttp3 {
            self.learn_alt_svc = false;
        }
    }

    /// The effective G2 category rules: the user's stored set when present
    /// (already validated by `sanitize`), otherwise the built-in defaults.
    pub fn category_rules(&self) -> Vec<crate::category::CategoryRule> {
        match &self.categories {
            Some(rules) => rules.clone(),
            None => crate::category::default_categories(),
        }
    }

    /// Resolve [`Settings::default_download_dir`] to a concrete directory,
    /// falling back to the OS Downloads folder.
    pub fn download_dir(&self) -> PathBuf {
        if self.default_download_dir.trim().is_empty() {
            system_download_dir()
        } else {
            PathBuf::from(&self.default_download_dir)
        }
    }

    /// Key/value view used by `udm-cli settings` and the Settings view.
    pub fn to_pairs(&self) -> Vec<(String, String)> {
        let json = serde_json::to_value(self).unwrap_or_default();
        let mut out: Vec<(String, String)> = match json {
            serde_json::Value::Object(map) => map
                .into_iter()
                .map(|(k, v)| {
                    let s = match v {
                        serde_json::Value::String(s) => s,
                        other => other.to_string(),
                    };
                    (k, s)
                })
                .collect(),
            _ => Vec::new(),
        };
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Apply `key=value` (as typed in the CLI or the Settings view).
    pub fn apply_pair(&mut self, key: &str, value: &str) -> Result<()> {
        let key = key.trim();
        let value = value.trim();
        macro_rules! num {
            ($field:ident, $ty:ty) => {{
                let v: $ty = value.parse().map_err(|_| {
                    Error::Other(format!("`{value}` is not a valid number for {key}"))
                })?;
                self.$field = v;
            }};
        }
        match key {
            "max_concurrent_tasks" => num!(max_concurrent_tasks, u32),
            "segments_per_task" => num!(segments_per_task, u32),
            "min_segment_bytes" => num!(min_segment_bytes, u64),
            "connect_timeout_secs" => num!(connect_timeout_secs, u64),
            "stall_timeout_secs" => num!(stall_timeout_secs, u64),
            "max_retries" => num!(max_retries, u32),
            "max_redirects" => num!(max_redirects, u32),
            "write_buffer_bytes" => num!(write_buffer_bytes, usize),
            "persist_interval_ms" => num!(persist_interval_ms, u64),
            "alt_svc_max_age_secs" => num!(alt_svc_max_age_secs, u64),
            "keep_history_days" => num!(keep_history_days, u32),
            "default_download_dir" => {
                let p = if value.is_empty() {
                    String::new()
                } else {
                    display(&PathBuf::from(value))
                };
                self.default_download_dir = p;
            }
            "transport_pref" => self.transport_pref = TransportPref::parse(value)?,
            "conflict_policy" => self.conflict_policy = ConflictPolicy::parse(value)?,
            "http3_enabled"
            | "learn_alt_svc"
            | "use_system_proxy"
            | "verify_checksums"
            | "delete_part_on_remove"
            | "auto_categorize" => {
                let b = parse_bool(value)
                    .ok_or_else(|| Error::Other(format!("`{value}` is not true/false")))?;
                match key {
                    "http3_enabled" => self.http3_enabled = b,
                    "learn_alt_svc" => self.learn_alt_svc = b,
                    "use_system_proxy" => self.use_system_proxy = b,
                    "verify_checksums" => self.verify_checksums = b,
                    "delete_part_on_remove" => self.delete_part_on_remove = b,
                    "auto_categorize" => self.auto_categorize = b,
                    _ => unreachable!(),
                }
            }
            "categories" => {
                // The whole rule set as JSON (same shape as stored). It is
                // validated strictly: an invalid set is rejected, never
                // sanitised into different routing.
                let rules: Vec<crate::category::CategoryRule> = serde_json::from_str(value)
                    .map_err(|e| Error::Other(format!("`categories` is not valid JSON: {e}")))?;
                crate::category::validate_categories(&rules)?;
                self.categories = Some(rules);
            }
            "user_agent" => self.user_agent = value.to_string(),
            "theme" => self.theme = value.to_string(),
            "language" => self.language = value.to_string(),
            "accent" => self.accent = value.to_string(),
            "log_level" => self.log_level = value.to_string(),
            other => {
                return Err(Error::Other(format!(
                    "unknown setting `{other}` (try `udm-cli settings --list`)"
                )))
            }
        }
        self.sanitize();
        Ok(())
    }

    pub fn from_json(json: &[u8]) -> Result<Settings> {
        let mut s: Settings = serde_json::from_slice(json)?;
        s.sanitize();
        Ok(s)
    }

    pub fn to_json(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec_pretty(self)?)
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let s = Settings::default();
        let json = s.to_json().unwrap();
        let back = Settings::from_json(&json).unwrap();
        assert_eq!(back.max_concurrent_tasks, s.max_concurrent_tasks);
        assert_eq!(back.transport_pref, s.transport_pref);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // Guards against silently ignoring a renamed setting.
        let bad = br#"{"max_concurrent_tasks": 2, "telemetry_endpoint": "https://evil.example"}"#;
        assert!(Settings::from_json(bad).is_err());
    }

    #[test]
    fn legacy_settings_gain_local_appearance_defaults() {
        let settings = Settings::from_json(br#"{"theme":"light","segments_per_task":12}"#).unwrap();
        assert_eq!(settings.theme, "light");
        assert_eq!(settings.language, "en");
        assert_eq!(settings.accent, "blue");
        assert_eq!(settings.segments_per_task, 12);
    }

    #[test]
    fn appearance_round_trip_and_invalid_values() {
        let mut settings = Settings::default();
        settings.apply_pair("language", "vi").unwrap();
        settings.apply_pair("accent", "green").unwrap();
        settings.apply_pair("theme", "light").unwrap();
        let loaded = Settings::from_json(&settings.to_json().unwrap()).unwrap();
        assert_eq!(
            (
                loaded.theme.as_str(),
                loaded.language.as_str(),
                loaded.accent.as_str()
            ),
            ("light", "vi", "green")
        );
        settings.language = "xx".into();
        settings.accent = "invisible".into();
        settings.sanitize();
        assert_eq!(
            (settings.language.as_str(), settings.accent.as_str()),
            ("en", "blue")
        );
    }

    #[test]
    fn legacy_settings_default_auto_categorize_off() {
        // A pre-G2 settings JSON has neither G2 field.
        let s = Settings::from_json(br#"{"theme":"dark"}"#).unwrap();
        assert!(!s.auto_categorize);
        assert!(s.categories.is_none());
        assert_eq!(s.category_rules().len(), 8);
    }

    #[test]
    fn categories_apply_pair_validates() {
        let mut s = Settings::default();
        s.apply_pair("auto_categorize", "true").unwrap();
        assert!(s.auto_categorize);
        let good = r#"[{"id":"docs","folder":"Docs","suffixes":[".pdf"]}]"#;
        s.apply_pair("categories", good).unwrap();
        assert_eq!(s.category_rules()[0].folder, "Docs");
        // Traversal folder + duplicate suffix are rejected, not sanitised.
        let bad = r#"[{"id":"x","folder":"..","suffixes":[".pdf"]}]"#;
        assert!(s.apply_pair("categories", bad).is_err());
        let dup = r#"[{"id":"a","folder":"A","suffixes":[".pdf"]},{"id":"b","folder":"B","suffixes":[".PDF"]}]"#;
        assert!(s.apply_pair("categories", dup).is_err());
        // An invalid stored set falls back to defaults on sanitize.
        s.categories = Some(vec![crate::category::CategoryRule {
            id: "x".into(),
            folder: "..".into(),
            suffixes: vec![],
        }]);
        s.sanitize();
        assert!(s.categories.is_none());
    }

    #[test]
    fn sanitize_clamps() {
        let mut s = Settings {
            max_concurrent_tasks: 0,
            segments_per_task: 9999,
            stall_timeout_secs: 1,
            theme: "neon".into(),
            ..Default::default()
        };
        s.sanitize();
        assert_eq!(s.max_concurrent_tasks, 1);
        assert_eq!(s.segments_per_task, 64);
        assert_eq!(s.stall_timeout_secs, 5);
        assert_eq!(s.theme, "dark");
    }

    #[test]
    fn disabling_http3_forces_the_preference() {
        let mut s = Settings {
            http3_enabled: false,
            transport_pref: TransportPref::Http3Only,
            ..Default::default()
        };
        s.sanitize();
        assert_eq!(s.transport_pref, TransportPref::NoHttp3);
        assert!(!s.learn_alt_svc);
    }

    #[test]
    fn handoff_lease_default_is_the_owner_set_60s() {
        // Fresh configs carry the owner-set production lease (2026-11-09).
        assert_eq!(Settings::default().handoff_lease_secs, Some(60));
        // A config with no key at all takes the struct default too.
        let fresh: Settings = serde_json::from_str("{}").unwrap();
        assert_eq!(fresh.handoff_lease_secs, Some(60));
        // An explicitly stored null is preserved as None: upgrades never
        // silently enable the coordinator for existing installs.
        let stored: Settings = serde_json::from_str(r#"{"handoff_lease_secs": null}"#).unwrap();
        assert_eq!(stored.handoff_lease_secs, None);
        // And an explicit value round-trips.
        let set: Settings = serde_json::from_str(r#"{"handoff_lease_secs": 120}"#).unwrap();
        assert_eq!(set.handoff_lease_secs, Some(120));
    }

    #[test]
    fn apply_pair_works() {
        let mut s = Settings::default();
        s.apply_pair("segments_per_task", "16").unwrap();
        s.apply_pair("http3_enabled", "false").unwrap();
        s.apply_pair("transport_pref", "prefer-h3").unwrap();
        assert_eq!(s.segments_per_task, 16);
        assert!(!s.http3_enabled);
        assert!(s.apply_pair("nope", "1").is_err());
    }

    #[test]
    fn lifecycle_fields_round_trip_and_legacy_defaults() {
        // Settled G3 defaults on a fresh config.
        let d = Settings::default();
        assert!(d.close_to_tray);
        assert!(!d.minimize_to_tray);
        assert!(!d.start_minimized);
        assert!(!d.start_with_windows);
        // Non-default values survive a JSON round trip.
        let mut s = Settings::default();
        s.close_to_tray = false;
        s.minimize_to_tray = true;
        s.start_minimized = true;
        s.start_with_windows = true;
        let json = serde_json::to_string(&s).unwrap();
        let back: Settings = serde_json::from_str(&json).unwrap();
        assert!(!back.close_to_tray);
        assert!(back.minimize_to_tray);
        assert!(back.start_minimized);
        assert!(back.start_with_windows);
        // A legacy config without the lifecycle keys loads with defaults.
        let legacy: Settings = serde_json::from_str(r#"{"theme": "auto"}"#).unwrap();
        assert!(legacy.close_to_tray);
        assert!(!legacy.minimize_to_tray);
        assert!(!legacy.start_minimized);
        assert!(!legacy.start_with_windows);
        assert_eq!(legacy.theme, "auto");
    }
}

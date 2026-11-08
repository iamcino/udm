//! Build-time privacy guard.
//!
//! udm promises **zero telemetry**. This test makes that promise fail the
//! build instead of rotting silently: it parses the resolved dependency graph
//! (`Cargo.lock`) and rejects any crate whose name belongs to a known
//! analytics / crash-reporting / tracking family.
//!
//! It also greps the workspace sources for hardcoded reporting endpoints, so
//! an accidental `https://metrics.example.com` cannot ship either.

use std::path::PathBuf;

/// Crate-name substrings that must never appear in Cargo.lock.
/// ("segment-" rather than "segment" so the legit `unicode-segmentation`
/// text-processing crate does not trip the wire; the bare `segment` crate is
/// covered by FORBIDDEN_EXACT_NAMES.)
const FORBIDDEN_SUBSTRINGS: &[&str] = &[
    "telemetry",
    "analytics",
    "appinsights",
    "appcenter",
    "sentry",
    "bugsnag",
    "crashpad",
    "breakpad",
    "mixpanel",
    "amplitude",
    "segment-",
    "matomo",
    "plausible",
    "posthog",
    "hotjar",
    "fullstory",
    "countly",
    "firebase",
    "google-analytics",
    "ga4",
    "umami",
    "split.io",
    "launchdarkly",
    "statsig",
];

/// Crate names that are forbidden verbatim.
const FORBIDDEN_EXACT_NAMES: &[&str] = &["segment"];

/// Hostname prefixes that must never appear in our own sources. Assembled at
/// runtime so this guard file does not itself contain the forbidden strings.
const FORBIDDEN_SOURCE_PREFIXES: &[&str] = &["metrics", "telemetry", "crash", "stats", "beacon"];

fn forbidden_source_strings() -> Vec<String> {
    FORBIDDEN_SOURCE_PREFIXES
        .iter()
        .map(|p| format!("{p}.udm"))
        .collect()
}

fn workspace_root() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates
    p.pop(); // workspace root
    p
}

fn lock_crates() -> Vec<String> {
    let lock = workspace_root().join("Cargo.lock");
    let text = std::fs::read_to_string(&lock)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", lock.display()));
    let mut names = Vec::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            if let Some(n) = current.take() {
                names.push(n);
            }
        } else if let Some(rest) = line.strip_prefix("name = \"") {
            current = Some(rest.trim_end_matches('"').to_string());
        }
    }
    if let Some(n) = current {
        names.push(n);
    }
    names
}

#[test]
fn cargo_lock_contains_no_telemetry_crates() {
    let names = lock_crates();
    assert!(
        !names.is_empty(),
        "Cargo.lock parsed to zero packages - parser broken?"
    );
    let mut violations = Vec::new();
    for name in &names {
        let lower = name.to_ascii_lowercase();
        for bad in FORBIDDEN_SUBSTRINGS {
            if lower.contains(bad) {
                violations.push(format!("{name} (matches '{bad}')"));
            }
        }
        if FORBIDDEN_EXACT_NAMES.contains(&lower.as_str()) {
            violations.push(format!("{name} (exact match)"));
        }
    }
    assert!(
        violations.is_empty(),
        "telemetry/analytics dependency detected in Cargo.lock: {}",
        violations.join(", ")
    );
}

#[test]
fn first_party_sources_never_phone_home() {
    let root = workspace_root();
    let forbidden = forbidden_source_strings();
    let mut checked = 0usize;
    let mut violations = Vec::new();
    walk(&root.join("crates"), &mut |path| {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        checked += 1;
        for bad in &forbidden {
            if text.contains(bad.as_str()) {
                violations.push(format!("{}: {bad}", path.display()));
            }
        }
    });
    assert!(checked > 10, "walked only {checked} files - wrong root?");
    assert!(
        violations.is_empty(),
        "hardcoded reporting endpoint found:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_only_network_clients_are_the_download_stacks() {
    // Everything that opens a socket must live in crates/core/src/net or be a
    // direct user request. This is a weak tripwire (string based) but it
    // catches a stray reqwest/udp socket in UI or store code.
    let root = workspace_root();
    let mut violations = Vec::new();
    for dir in ["crates/gui/src", "crates/core/src/store"] {
        walk(&root.join(dir), &mut |path| {
            let Ok(text) = std::fs::read_to_string(path) else {
                return;
            };
            for bad in ["UdpSocket::bind", "TcpStream::connect", "reqwest::Client::new"] {
                if text.contains(bad) {
                    violations.push(format!("{}: {bad}", path.display()));
                }
            }
        });
    }
    assert!(
        violations.is_empty(),
        "unexpected socket use outside the network stack:\n{}",
        violations.join("\n")
    );
}

fn walk(dir: &std::path::Path, f: &mut impl FnMut(&std::path::Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk(&path, f);
        } else if matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("rs") | Some("slint") | Some("toml")
        ) {
            f(&path);
        }
    }
}

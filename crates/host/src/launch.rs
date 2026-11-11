//! Launch classification and invocation-identity validation for
//! `udm-host` (extension P1, blockers B1 and B2).
//!
//! B1: the process must route by invocation shape. Documented browser
//! native-messaging launches go to the bridge; the registry CLI keeps
//! its explicit subcommand surface; argument shapes that match neither
//! are rejected cleanly (usage on stderr, exit status 2, stdout
//! untouched — stdout is reserved for framed bridge replies).
//!
//! Documented launch shapes (Chrome and Firefox native-messaging
//! protocols, as recorded in the extension groundwork at bcdb55f):
//!
//! * Chromium family (Chrome, Edge, Chromium): the extension origin
//!   `chrome-extension://<extension-id>/` as the first argument,
//!   optionally followed by `--parent-window=<handle>` on Windows.
//! * Firefox: the application-manifest path (a `.json` file) as the
//!   first argument and the add-on ID as the second.
//! * No arguments: a bare launch (developer harness / manual stdio
//!   testing). The bridge runs with the legacy sidecar caller.
//!
//! B2: a browser launch carries its claimed identity in its arguments.
//! The bridge validates that claim against the registered allowed
//! identity for that browser — the generated host manifest at
//! `<data>/handoff/host-manifest-<browser>.json`, whose
//! `allowed_origins` / `allowed_extensions` are exactly what
//! registration recorded — and only then pairs with the coordinator
//! under the caller string `"<browser>:<extension-id>"`. The single
//! shared `udm-host.caller` sidecar is never consulted for a browser
//! launch, so registering one browser can no longer relabel another
//! browser's traffic (the single-sidecar conflict).

use serde_json::Value;

/// A browser native-messaging launch, parsed from the process
/// arguments. The identity is a *claim* until [`registered_caller`]
/// validates it against the registered allowed identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserInvocation {
    /// `"chrome"` for the Chromium family (Chrome, Edge, Chromium —
    /// same launch shape, same host manifest), `"firefox"` for Firefox.
    pub browser: &'static str,
    /// The extension / add-on ID the browser claims launched this host.
    pub extension_id: String,
}

/// What a process invocation means, decided from its argument shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Launch {
    /// Run the stdio bridge. `Some` carries the parsed browser
    /// invocation whose identity must be validated before use; `None`
    /// is a bare launch, which keeps the legacy sidecar caller.
    Bridge(Option<BrowserInvocation>),
    /// A registry CLI invocation (`--register` / `--unregister`). The
    /// arguments go to `reg::run`, which validates their exact shape.
    Maintenance,
    /// Matches no documented shape: reject cleanly.
    Reject,
}

/// Classify a process invocation by its argument shape. Pure: no I/O,
/// no environment access.
pub fn classify(args: &[String]) -> Launch {
    let Some(first) = args.first() else {
        return Launch::Bridge(None);
    };
    if first == "--register" || first == "--unregister" {
        return Launch::Maintenance;
    }
    if let Some(id) = parse_chrome_origin(first) {
        // Documented Chromium-family shape: the origin, optionally
        // followed by exactly one --parent-window=<handle> on Windows.
        let rest = &args[1..];
        if rest.is_empty() || (rest.len() == 1 && is_parent_window(&rest[0])) {
            return Launch::Bridge(Some(BrowserInvocation {
                browser: "chrome",
                extension_id: id,
            }));
        }
        return Launch::Reject;
    }
    if args.len() == 2 && is_manifest_path(first) {
        // Documented Firefox shape: application-manifest path + add-on ID.
        if let Some(id) = parse_addon_id(&args[1]) {
            return Launch::Bridge(Some(BrowserInvocation {
                browser: "firefox",
                extension_id: id,
            }));
        }
        return Launch::Reject;
    }
    Launch::Reject
}

/// Parse a Chromium-family origin argument,
/// `chrome-extension://<extension-id>/`. Returns the claimed extension
/// ID, or `None` when the argument is not a well-formed origin (in
/// which case it is not a Chrome launch at all).
fn parse_chrome_origin(arg: &str) -> Option<String> {
    let rest = arg.strip_prefix("chrome-extension://")?;
    let id = rest.strip_suffix('/')?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    Some(id.to_string())
}

/// True for Chrome's Windows companion argument,
/// `--parent-window=<handle>` with a numeric handle.
fn is_parent_window(arg: &str) -> bool {
    match arg.strip_prefix("--parent-window=") {
        Some(handle) => !handle.is_empty() && handle.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// True when the argument looks like the path of a native-messaging
/// application manifest (the Firefox launch's first argument): a path
/// ending in `.json`.
fn is_manifest_path(arg: &str) -> bool {
    arg.len() > 5 && arg.to_lowercase().ends_with(".json")
}

/// Validate a Firefox add-on ID claim (the launch's second argument):
/// non-empty, no whitespace or path separators. Add-on IDs are email-
/// shaped (`udm-handoff@cino.pw`) or GUID-shaped (`{...}`).
fn parse_addon_id(arg: &str) -> Option<String> {
    if arg.is_empty()
        || !arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "@._-{}".contains(c))
    {
        return None;
    }
    Some(arg.to_string())
}

/// Validate a parsed invocation against the registered allowed identity
/// for its browser, recorded in the generated host manifest under
/// `<data_root>/handoff/`. Returns the coordinator caller string
/// (`"<browser>:<extension-id>"`) on an exact match, `None` when the
/// browser was never registered here, the manifest is unreadable, or
/// the claimed identity is not the registered one. Never consults the
/// `udm-host.caller` sidecar: a browser launch stands or falls by its
/// own browser's registration.
pub fn registered_caller(data_root: &std::path::Path, inv: &BrowserInvocation) -> Option<String> {
    let manifest_path = data_root
        .join("handoff")
        .join(format!("host-manifest-{}.json", inv.browser));
    let text = std::fs::read_to_string(manifest_path).ok()?;
    let manifest: Value = serde_json::from_str(&text).ok()?;
    let key = if inv.browser == "chrome" {
        "allowed_origins"
    } else {
        "allowed_extensions"
    };
    let allowed = manifest[key].as_array()?;
    let claimed = if inv.browser == "chrome" {
        format!("chrome-extension://{}/", inv.extension_id)
    } else {
        inv.extension_id.clone()
    };
    if allowed.iter().any(|a| a.as_str() == Some(claimed.as_str())) {
        Some(format!("{}:{}", inv.browser, inv.extension_id))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_of(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn bare_launch_is_bridge_without_invocation() {
        assert_eq!(classify(&[]), Launch::Bridge(None));
    }

    #[test]
    fn maintenance_surface_routes_to_the_registry_cli() {
        assert_eq!(
            classify(&args_of(&["--register", "chrome", "abc"])),
            Launch::Maintenance
        );
        assert_eq!(
            classify(&args_of(&["--unregister", "firefox"])),
            Launch::Maintenance
        );
        // Malformed maintenance shapes still route to the CLI, which
        // owns their exact validation and usage text.
        assert_eq!(classify(&args_of(&["--register"])), Launch::Maintenance);
    }

    #[test]
    fn chrome_launch_shapes_are_bridge_invocations() {
        let plain = classify(&args_of(&["chrome-extension://abc123/"]));
        assert_eq!(
            plain,
            Launch::Bridge(Some(BrowserInvocation {
                browser: "chrome",
                extension_id: "abc123".to_string(),
            }))
        );
        // The documented Windows shape carries --parent-window.
        let with_window = classify(&args_of(&[
            "chrome-extension://abc123/",
            "--parent-window=123456",
        ]));
        assert_eq!(with_window, plain);
    }

    #[test]
    fn firefox_launch_shape_is_a_bridge_invocation() {
        let launch = classify(&args_of(&[
            r"C:\Users\x\AppData\Local\udm\handoff\host-manifest-firefox.json",
            "udm-handoff@cino.pw",
        ]));
        assert_eq!(
            launch,
            Launch::Bridge(Some(BrowserInvocation {
                browser: "firefox",
                extension_id: "udm-handoff@cino.pw".to_string(),
            }))
        );
    }

    #[test]
    fn undocumented_shapes_are_rejected() {
        for shape in [
            vec!["--frobnicate"],
            vec!["hello"],
            vec!["--parent-window=123"], // companion arg without an origin
            vec!["chrome-extension://abc123/", "--bogus"],
            vec!["chrome-extension://abc123/", "--parent-window="],
            vec!["chrome-extension://abc123/", "--parent-window=12x"],
            vec!["chrome-extension://abc123/", "extra", "args"],
            vec!["chrome-extension://"],                // no id, no slash
            vec!["chrome-extension:///"],               // empty id
            vec!["chrome-extension://a b/"],            // whitespace in id
            vec!["chrome-extension://a/b/"],            // path segment in id
            vec![r"C:\udm\host-manifest-firefox.json"], // firefox path without id
            vec![
                r"C:\udm\host-manifest-firefox.json",
                "udm-handoff@cino.pw",
                "x",
            ],
            vec![r"C:\udm\host-manifest-firefox.json", "bad id"],
            vec!["not-a-manifest.txt", "udm-handoff@cino.pw"],
        ] {
            assert_eq!(
                classify(&args_of(&shape)),
                Launch::Reject,
                "shape {shape:?} must be rejected"
            );
        }
    }

    fn write_manifest(root: &std::path::Path, browser: &str, ext_id: &str) {
        // Build the manifest exactly the way registration does, so the
        // validation reads the real registered format.
        let spec = crate::reg::describe(browser, ext_id).unwrap();
        let manifest = crate::reg::manifest_json(&spec, ext_id, r"C:\udm\udm-host.exe");
        let dir = root.join("handoff");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(format!("host-manifest-{browser}.json")),
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    fn inv(browser: &'static str, id: &str) -> BrowserInvocation {
        BrowserInvocation {
            browser,
            extension_id: id.to_string(),
        }
    }

    #[test]
    fn registered_identity_validates_per_browser() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "chrome", "abc123");
        write_manifest(dir.path(), "firefox", "udm-handoff@cino.pw");
        assert_eq!(
            registered_caller(dir.path(), &inv("chrome", "abc123")),
            Some("chrome:abc123".to_string())
        );
        assert_eq!(
            registered_caller(dir.path(), &inv("firefox", "udm-handoff@cino.pw")),
            Some("firefox:udm-handoff@cino.pw".to_string())
        );
    }

    #[test]
    fn identity_isolation_between_browsers() {
        // The single-sidecar defect: registering Firefox after Chrome
        // made Chrome traffic pair as Firefox. With validated launch
        // identity, each browser's claim is checked only against its
        // own browser's registration.
        let dir = tempfile::tempdir().unwrap();
        write_manifest(dir.path(), "chrome", "abc123");
        write_manifest(dir.path(), "firefox", "udm-handoff@cino.pw");
        // Chrome launch claiming the Firefox ID: not a Chrome identity.
        assert_eq!(
            registered_caller(dir.path(), &inv("chrome", "udm-handoff@cino.pw")),
            None
        );
        // Firefox launch claiming the Chrome ID: not a Firefox identity.
        assert_eq!(
            registered_caller(dir.path(), &inv("firefox", "abc123")),
            None
        );
        // Unregistered ID for a registered browser: refused.
        assert_eq!(
            registered_caller(dir.path(), &inv("chrome", "zzz999")),
            None
        );
    }

    #[test]
    fn unregistered_browser_or_broken_manifest_validates_to_none() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing registered at all.
        assert_eq!(
            registered_caller(dir.path(), &inv("chrome", "abc123")),
            None
        );
        // A manifest that is not JSON.
        let handoff = dir.path().join("handoff");
        std::fs::create_dir_all(&handoff).unwrap();
        std::fs::write(handoff.join("host-manifest-chrome.json"), "{oops").unwrap();
        assert_eq!(
            registered_caller(dir.path(), &inv("chrome", "abc123")),
            None
        );
    }
}

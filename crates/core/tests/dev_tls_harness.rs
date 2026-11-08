//! I1: production HTTP trust rejects the dev CA; the harness client accepts it.
//!
//! The CA type is compiled into this test binary only. The scan below fails
//! if `HarnessCa` shows up in the three production builders.

#[path = "common/mod.rs"]
mod common;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::ServerName;
use tokio::net::TcpStream;
use udm_core::net::http12::Http12Client;
use udm_core::net::stack::{NetSettings, RequestSpec};
use udm_core::Settings;

use common::dev_tls::{HarnessCa, HARNESS_MARKER};

#[tokio::test]
async fn production_client_rejects_harness_ca_and_harness_client_accepts_it() -> Result<(), String>
{
    let ca = HarnessCa::generate()?;
    let server = ca.spawn_http().await?;
    let path = "/harness";

    let accepted = ca.get(server.addr, path).await?;
    if !accepted.contains(HARNESS_MARKER) {
        return Err(format!(
            "harness client did not accept the server: {accepted}"
        ));
    }

    let settings = Settings {
        use_system_proxy: false,
        http3_enabled: false,
        connect_timeout_secs: 3,
        stall_timeout_secs: 5,
        ..Settings::default()
    };
    let client = Http12Client::new(&NetSettings::from_settings(&settings))
        .map_err(|e| format!("production client did not build: {e}"))?;
    let url = format!("https://{}{path}", server.addr);
    let result = tokio::time::timeout(
        Duration::from_secs(15),
        client.send(&RequestSpec::get(&url)),
    )
    .await
    .map_err(|_| {
        "production client timed out; a certificate rejection should be prompt".to_string()
    })?;
    let err = match result {
        Ok(response) => {
            return Err(format!(
                "production Http12Client accepted the harness server: HTTP {}",
                response.status()
            ));
        }
        Err(err) => err,
    };
    // `From<reqwest::Error>` keeps Display only. reqwest's Display omits the
    // rustls source, so the certificate wording is read from the same
    // `use_rustls_tls` stack and from `tls::client_config`.
    if !matches!(err, udm_core::Error::Connect(_)) {
        return Err(format!(
            "expected Http12Client to fail the handshake, got: {err}"
        ));
    }
    let stack = rustls_platform_failure(&url).await?;
    if !is_certificate_rejection(&stack) {
        return Err(format!(
            "Http12Client's rustls stack did not reject the harness certificate: {stack}"
        ));
    }
    let native = native_roots_failure(server.addr).await?;
    if !is_certificate_rejection(&native) {
        return Err(format!(
            "tls::client_config did not reject the harness certificate: {native}"
        ));
    }

    server.shutdown().await;
    Ok(())
}

/// The trust decision `Http12Client::new` makes: reqwest `use_rustls_tls`
/// with no added roots and no proxy.
async fn rustls_platform_failure(url: &str) -> Result<String, String> {
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(3))
        .read_timeout(Duration::from_secs(5))
        .use_rustls_tls()
        .no_proxy()
        .build()
        .map_err(|e| format!("rustls client: {e}"))?;
    match client.get(url).send().await {
        Ok(response) => Err(format!(
            "use_rustls_tls accepted the harness server: HTTP {}",
            response.status()
        )),
        Err(err) => Ok(error_chain(&err)),
    }
}

/// `tls::client_config` is the OS-root builder behind the HTTP/3 client.
async fn native_roots_failure(addr: SocketAddr) -> Result<String, String> {
    let config = udm_core::net::tls::client_config(&[b"http/1.1"])
        .map_err(|e| format!("client_config: {e}"))?;
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let tcp = TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect {addr}: {e}"))?;
    let name = ServerName::IpAddress(rustls::pki_types::IpAddr::from(addr.ip()));
    match connector.connect(name, tcp).await {
        Ok(_) => Err("tls::client_config accepted the harness certificate".to_string()),
        Err(err) => Ok(error_chain(&err)),
    }
}

fn is_certificate_rejection(message: &str) -> bool {
    let msg = message.to_ascii_lowercase();
    msg.contains("certificate") || msg.contains("unknownissuer") || msg.contains("not trusted")
}

fn error_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![err.to_string()];
    let mut current = err.source();
    for _ in 0..8 {
        let Some(source) = current else {
            break;
        };
        parts.push(source.to_string());
        current = source.source();
    }
    parts.join(" | ")
}

#[test]
fn harness_ca_type_is_absent_from_production_builders() -> Result<(), String> {
    let harness = read_rel("crates/core/tests/common/dev_tls.rs")?;
    if !harness.contains("struct HarnessCa") {
        return Err("the dev harness no longer defines HarnessCa".to_string());
    }
    for needle in [
        "accept_invalid_certs",
        "update_settings",
        "CertAddEncodedCertificateToStore",
        "certutil",
    ] {
        if harness.contains(needle) {
            return Err(format!("dev harness contains `{needle}`"));
        }
    }

    let builders = [
        (
            "crates/core/src/net/tls.rs",
            "pub fn client_config(alpn: &[&[u8]])",
        ),
        (
            "crates/core/src/net/http12.rs",
            "pub fn new(settings: &NetSettings)",
        ),
        (
            "crates/core/src/net/h3.rs",
            "pub fn new(connect_timeout: Duration, user_agent: &str)",
        ),
    ];
    for (rel, signature) in builders {
        let text = read_rel(rel)?;
        let body = function_body(&text, signature)?;
        if body.contains("HarnessCa") {
            return Err(format!("{rel} `{signature}` names HarnessCa"));
        }
        if text.contains("HarnessCa") {
            return Err(format!("{rel} names HarnessCa"));
        }
    }

    assert_tree_has_no_harness_ca(&workspace_root().join("crates/core/src"))?;
    Ok(())
}

fn workspace_root() -> std::path::PathBuf {
    let mut path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop();
    path.pop();
    path
}

fn read_rel(rel: &str) -> Result<String, String> {
    let path = workspace_root().join(rel);
    std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))
}

fn function_body<'a>(text: &'a str, signature: &str) -> Result<&'a str, String> {
    let start = text
        .find(signature)
        .ok_or_else(|| format!("missing `{signature}`"))?;
    let rest = &text[start..];
    let after_line = rest.find('\n').unwrap_or(rest.len());
    let tail = &rest[after_line..];
    let end = [
        "\n    pub fn ",
        "\n    pub async fn ",
        "\npub fn ",
        "\nimpl ",
    ]
    .iter()
    .filter_map(|marker| tail.find(marker))
    .min()
    .unwrap_or(tail.len());
    Ok(&rest[..after_line + end])
}

fn assert_tree_has_no_harness_ca(dir: &Path) -> Result<(), String> {
    let entries = std::fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            assert_tree_has_no_harness_ca(&path)?;
            continue;
        }
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        if text.contains("HarnessCa") {
            return Err(format!("{} names HarnessCa", path.display()));
        }
    }
    Ok(())
}

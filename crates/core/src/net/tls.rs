//! TLS configuration shared by the HTTP/3 (quinn) stack.
//!
//! * Trust anchors come from the **operating system trust store**
//!   (`rustls-native-certs`), so corporate CAs work and no root list has to be
//!   shipped or updated over the network.
//! * The crypto provider is [`ring`], installed process-wide exactly once.
//!   This keeps a single TLS stack in the binary: no aws-lc-rs (which would
//!   need CMake/NASM), no OpenSSL, no schannel.
//! * Certificate verification is always on. There is deliberately no
//!   `accept_invalid_certs` setting anywhere in udm.

use std::sync::{Arc, OnceLock};

use rustls::{ClientConfig, RootCertStore};

use crate::error::{Error, Result};

/// ALPN token for HTTP/3.
pub const ALPN_H3: &[u8] = b"h3";

static PROVIDER: OnceLock<bool> = OnceLock::new();

/// Install `ring` as the process-wide rustls crypto provider.
///
/// Idempotent; returns `true` if this call installed it.
pub fn install_default_provider() -> bool {
    *PROVIDER.get_or_init(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .is_ok()
    })
}

/// Load the OS trust store. Returns the root store and the number of entries
/// that could not be parsed (logged, never fatal on their own).
pub fn native_roots() -> Result<(RootCertStore, usize)> {
    install_default_provider();
    let result = rustls_native_certs::load_native_certs();
    let mut roots = RootCertStore::empty();
    let mut rejected = 0usize;
    for cert in result.certs {
        if roots.add(cert).is_err() {
            rejected += 1;
        }
    }
    for err in &result.errors {
        tracing::warn!("native certificate store: {err}");
    }
    if roots.is_empty() {
        return Err(Error::Tls(
            "the operating system trust store returned no usable root certificates".to_string(),
        ));
    }
    Ok((roots, rejected))
}

/// A rustls client config with `alpn` negotiated and no client certificate.
pub fn client_config(alpn: &[&[u8]]) -> Result<ClientConfig> {
    install_default_provider();
    let (roots, rejected) = native_roots()?;
    if rejected > 0 {
        tracing::debug!("{rejected} trust anchors were skipped");
    }
    let mut config = ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    Ok(config)
}

/// rustls config usable by quinn (ALPN is fixed to `h3`).
pub fn quic_client_config() -> Result<Arc<quinn::crypto::rustls::QuicClientConfig>> {
    let config = client_config(&[ALPN_H3])?;
    let quic = quinn::crypto::rustls::QuicClientConfig::try_from(config)
        .map_err(|e| Error::Tls(format!("QUIC TLS configuration rejected: {e}")))?;
    Ok(Arc::new(quic))
}

/// Transport tuning for bulk downloads: bigger windows help on high
/// bandwidth-delay-product links (the common case for a download manager).
pub fn tuned_client_config() -> Result<quinn::ClientConfig> {
    let mut config = quinn::ClientConfig::new(quic_client_config()?);
    let mut transport = quinn::TransportConfig::default();
    transport
        .stream_receive_window(quinn::VarInt::from_u32(16 * 1024 * 1024))
        .receive_window(quinn::VarInt::from_u32(48 * 1024 * 1024))
        .send_window(8 * 1024 * 1024)
        // Keep NAT bindings alive during long transfers.
        .keep_alive_interval(Some(std::time::Duration::from_secs(10)));
    config.transport_config(Arc::new(transport));
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_install_is_idempotent() {
        install_default_provider();
        install_default_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }
}

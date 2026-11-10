//! HTTP/1.1 + HTTP/2 transport (reqwest / hyper / rustls).
//!
//! This is the fallback path *and* the path that discovers HTTP/3: the
//! `Alt-Svc` header on these responses is what teaches the engine that an
//! origin can be reached over QUIC next time.

use std::time::Duration;

use http::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_ENCODING, IF_RANGE, RANGE, USER_AGENT,
};
use http::Method;
use reqwest::redirect::Policy;
use reqwest::{Client, RequestBuilder, Response};

use crate::error::{Error, Result};
use crate::net::stack::{NetSettings, RequestSpec};
use crate::net::ACCEPT_ENCODING_IDENTITY;

/// Thin wrapper around a configured [`reqwest::Client`].
#[derive(Clone, Debug)]
pub struct Http12Client {
    client: Client,
}

impl Http12Client {
    pub fn new(settings: &NetSettings) -> Result<Self> {
        Self::build(settings, &[])
    }

    /// Rebuild with new settings (called when the user edits them).
    ///
    /// A root installed by [`Self::install_extra_root_der`] is not kept.
    /// Settings changes go back to the operating-system trust store alone.
    pub fn rebuild(&mut self, settings: &NetSettings) -> Result<()> {
        *self = Http12Client::new(settings)?;
        Ok(())
    }

    /// Trust one extra DER root in addition to the operating-system store.
    ///
    /// Production never calls this. It does not write the OS trust store.
    /// The QUIC pool is a separate client and is not changed here.
    pub fn install_extra_root_der(&mut self, settings: &NetSettings, der: &[u8]) -> Result<()> {
        *self = Self::build(settings, &[der])?;
        Ok(())
    }

    fn build(settings: &NetSettings, extra_roots: &[&[u8]]) -> Result<Self> {
        crate::net::tls::install_default_provider();

        let mut headers = HeaderMap::new();
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
        // Identity encoding is mandatory: `Range` + `Content-Length` must
        // describe the *same* bytes we write to disk.
        headers.insert(
            ACCEPT_ENCODING,
            HeaderValue::from_static(ACCEPT_ENCODING_IDENTITY),
        );
        headers.insert(
            USER_AGENT,
            HeaderValue::from_str(&settings.user_agent)
                .unwrap_or_else(|_| HeaderValue::from_static(crate::DEFAULT_USER_AGENT)),
        );

        let mut builder = Client::builder()
            .default_headers(headers)
            .connect_timeout(settings.connect_timeout)
            // Per-read stall guard; the engine additionally wraps every chunk
            // read in a timeout so HTTP/3 behaves identically.
            .read_timeout(settings.stall_timeout)
            .redirect(Policy::limited(settings.max_redirects as usize))
            .tcp_nodelay(true)
            .pool_max_idle_per_host(8)
            .use_rustls_tls();

        if !settings.use_system_proxy {
            builder = builder.no_proxy();
        }
        for der in extra_roots {
            let cert = reqwest::Certificate::from_der(der)
                .map_err(|e| Error::Tls(format!("cannot load the extra trust anchor ({e})")))?;
            builder = builder.add_root_certificate(cert);
        }

        let client = builder.build().map_err(|e| {
            Error::Tls(format!(
                "cannot initialise the HTTP client ({e}); check the OS trust store"
            ))
        })?;
        Ok(Http12Client { client })
    }

    /// Issue a request. Redirects are followed by reqwest; `Response::url()`
    /// gives the final URL.
    pub async fn send(&self, spec: &RequestSpec) -> Result<Response> {
        let mut builder: RequestBuilder = match spec.method {
            Method::GET => self.client.get(&spec.url),
            Method::HEAD => self.client.head(&spec.url),
            ref other => self.client.request(other.clone(), &spec.url),
        };

        if let Some((start, end)) = spec.range {
            let value = match end {
                Some(end) => format!("bytes={start}-{end}"),
                None => format!("bytes={start}-"),
            };
            builder = builder.header(RANGE, value);
        }
        if let Some(if_range) = &spec.if_range {
            builder = builder.header(IF_RANGE, if_range.as_str());
        }
        for (name, value) in &spec.extra_headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| Error::Other(format!("bad header name `{name}`: {e}")))?;
            builder = builder.header(name, value.as_str());
        }

        let response = builder.send().await?;
        Ok(response)
    }

    /// Same as [`Self::send`] but with a hard wall-clock cap (used for probes,
    /// never for body transfers).
    pub async fn send_capped(&self, spec: &RequestSpec, cap: Duration) -> Result<Response> {
        tokio::time::timeout(cap, self.send(spec))
            .await
            .map_err(|_| Error::Timeout {
                secs: cap.as_secs(),
            })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> NetSettings {
        NetSettings::from_settings(&crate::config::Settings::default())
    }

    #[test]
    fn builds_a_client_with_the_os_trust_store() {
        let client = Http12Client::new(&settings()).expect("client should build");
        // Smoke test: the client object exists and can be cloned (pooled).
        let _clone = client.clone();
    }
}

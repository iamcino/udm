//! HTTP/1.1 + HTTP/2 transport (reqwest / hyper / rustls).
//!
//! This is the fallback path *and* the path that discovers HTTP/3: the
//! `Alt-Svc` header on these responses is what teaches the engine that an
//! origin can be reached over QUIC next time.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

/// Actual admitted peer family. A bind alone is never evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    V4,
    V6,
}

impl Family {
    pub fn accepts(self, peer: Option<SocketAddr>) -> bool {
        match (self, peer.map(|p| p.ip())) {
            (Self::V4, Some(IpAddr::V4(_))) => true,
            (Self::V6, Some(IpAddr::V6(ip))) => ip.to_ipv4_mapped().is_none(),
            _ => false,
        }
    }
}

struct FamilyResolver(Family);
impl reqwest::dns::Resolve for FamilyResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = name.as_str().to_owned();
        let family = self.0;
        Box::pin(async move {
            let addresses: Vec<_> = tokio::net::lookup_host((host.as_str(), 0))
                .await?
                .filter(|peer| family.accepts(Some(*peer)))
                .collect();
            if addresses.is_empty() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "requested IP family unavailable",
                )
                .into());
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

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
    family: Option<Family>,
    h2_only: bool,
    redirect_limit: u32,
    // Serial entry through headers also bounds cold pre-ALPN opens.
    // This is a request-entry bound, never a socket census.
    headers_gate: Arc<tokio::sync::Mutex<()>>,
}

impl Http12Client {
    pub fn new(settings: &NetSettings) -> Result<Self> {
        Self::build(settings, &[], None)
    }

    pub(crate) fn family(settings: &NetSettings, roots: &[&[u8]], family: Family) -> Result<Self> {
        if settings.use_system_proxy {
            return Err(Error::Unsupported(
                "family paths ineligible with proxy support enabled".into(),
            ));
        }
        Self::build(settings, roots, Some(family))
    }

    /// S3 local experiment: each call builds an independent H2-only pool.
    /// Never reinterpret an H1 fallback as evidence of a second H2 session.
    pub(crate) fn h2_experiment(
        settings: &NetSettings,
        roots: &[&[u8]],
        family: Family,
    ) -> Result<Self> {
        if settings.use_system_proxy {
            return Err(Error::Unsupported(
                "H2 experiment ineligible with proxy".into(),
            ));
        }
        Self::build_protocol(settings, roots, Some(family), true)
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
        *self = Self::build(settings, &[der], None)?;
        Ok(())
    }

    fn build(
        settings: &NetSettings,
        extra_roots: &[&[u8]],
        family: Option<Family>,
    ) -> Result<Self> {
        Self::build_protocol(settings, extra_roots, family, false)
    }

    fn build_protocol(
        settings: &NetSettings,
        extra_roots: &[&[u8]],
        family: Option<Family>,
        h2_only: bool,
    ) -> Result<Self> {
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

        if let Some(family) = family {
            builder = builder
                .dns_resolver(Arc::new(FamilyResolver(family)))
                .local_address(match family {
                    Family::V4 => IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
                    Family::V6 => IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED),
                })
                .redirect(Policy::none());
        }
        if !settings.use_system_proxy {
            builder = builder.no_proxy();
        }
        if h2_only {
            builder = builder.http2_prior_knowledge();
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
        Ok(Http12Client {
            client,
            family,
            h2_only,
            redirect_limit: settings.max_redirects,
            headers_gate: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    /// Issue a request. Redirects are followed by reqwest; `Response::url()`
    /// gives the final URL.
    pub async fn send(&self, spec: &RequestSpec) -> Result<Response> {
        let Some(family) = self.family else {
            return self.send_once(spec).await;
        };
        let _headers = self.headers_gate.lock().await;
        let mut current = spec.clone();
        for hop in 0..=self.redirect_limit {
            let response = self.send_once(&current).await?;
            if !family.accepts(response.remote_addr()) {
                return Err(Error::InvalidResponse(
                    "missing or wrong actual peer family".into(),
                ));
            }
            if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            let location = response
                .headers()
                .get(http::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| Error::InvalidResponse("invalid family redirect".into()))?;
            let next = response
                .url()
                .join(location)
                .map_err(|e| Error::InvalidUrl(e.to_string()))?;
            if !matches!(next.scheme(), "http" | "https") || hop == self.redirect_limit {
                return Err(Error::InvalidResponse(
                    "family redirect limit or scheme".into(),
                ));
            }
            if next.origin() != response.url().origin() {
                current.extra_headers.clear();
            }
            current.url = next.to_string();
        }
        Err(Error::InvalidResponse("family redirect limit".into()))
    }

    async fn send_once(&self, spec: &RequestSpec) -> Result<Response> {
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
        if self.h2_only && response.version() != http::Version::HTTP_2 {
            return Err(Error::InvalidResponse(
                "H2 experiment requires negotiated H2".into(),
            ));
        }
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

    #[test]
    fn a11_peer_evidence_and_proxy_fail_closed() {
        let v4 = "127.0.0.1:80".parse().unwrap();
        let v6 = "[::1]:80".parse().unwrap();
        let mapped = "[::ffff:127.0.0.1]:80".parse().unwrap();
        for family in [Family::V4, Family::V6] {
            assert!(!family.accepts(None));
            assert!(!family.accepts(Some(mapped)));
        }
        assert!(Family::V4.accepts(Some(v4)));
        assert!(Family::V6.accepts(Some(v6)));
        assert!(!Family::V6.accepts(Some(v4)));
        assert!(!Family::V4.accepts(Some(v6)));
        let mut settings = settings();
        settings.use_system_proxy = true;
        assert!(Http12Client::family(&settings, &[], Family::V4).is_err());
        assert!(Http12Client::family(&settings, &[], Family::V6).is_err());
    }

    #[tokio::test]
    async fn a11_redirect_hops_keep_the_family_pin() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for family in [Family::V4, Family::V6] {
            let host = if family == Family::V4 {
                "127.0.0.1"
            } else {
                "::1"
            };
            let listener = tokio::net::TcpListener::bind((host, 0)).await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let job = tokio::spawn(async move {
                for hop in 0..2 {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut request = vec![0; 4096];
                    let _ = stream.read(&mut request).await.unwrap();
                    let response = if hop == 0 {
                        format!("HTTP/1.1 302 Found\r\nLocation: http://localhost:{port}/final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    } else {
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok".into()
                    };
                    stream.write_all(response.as_bytes()).await.unwrap();
                }
            });
            let mut settings = settings();
            settings.use_system_proxy = false;
            let client = Http12Client::family(&settings, &[], family).unwrap();
            let response = client
                .send(&RequestSpec::get(&format!(
                    "http://localhost:{port}/redirect"
                )))
                .await
                .unwrap();
            assert!(family.accepts(response.remote_addr()));
            assert_eq!(response.url().path(), "/final");
            assert_eq!(response.text().await.unwrap(), "ok");
            job.await.unwrap();
        }
    }
}

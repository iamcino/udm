//! The transport façade: one API for "probe this URL" and "open this byte
//! range", with HTTP/3 attempted first when we know (or are told) that the
//! origin supports it, and automatic fallback to HTTP/2 or HTTP/1.1.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use http::header::{HeaderMap, HeaderName, LOCATION};
use http::Method;
use parking_lot::RwLock;
use tokio::sync::watch;
use url::Url;

use crate::config::{Settings, TransportPref};
use crate::error::{Error, Result};
use crate::net::altsvc::{origin_key, AltSvcEntry, AltSvcStore};
use crate::net::gauge::{EngineConnections, ProtocolGauge};
use crate::net::h3::{H3Pool, H3Response};
use crate::net::http12::{Family, Http12Client};
use crate::net::{self};
use crate::store::records::TransportKind;
use crate::util;

/// The subset of [`Settings`] the transports care about.
#[derive(Clone, Debug)]
pub struct NetSettings {
    pub user_agent: String,
    pub connect_timeout: Duration,
    pub stall_timeout: Duration,
    pub max_redirects: u32,
    pub use_system_proxy: bool,
    pub http3_enabled: bool,
    pub learn_alt_svc: bool,
    pub alt_svc_max_age: Duration,
    pub default_pref: TransportPref,
}

impl NetSettings {
    pub fn from_settings(s: &Settings) -> Self {
        NetSettings {
            user_agent: s.user_agent.clone(),
            connect_timeout: Duration::from_secs(s.connect_timeout_secs),
            stall_timeout: Duration::from_secs(s.stall_timeout_secs),
            max_redirects: s.max_redirects,
            use_system_proxy: s.use_system_proxy,
            http3_enabled: s.http3_enabled,
            learn_alt_svc: s.learn_alt_svc,
            alt_svc_max_age: Duration::from_secs(s.alt_svc_max_age_secs),
            default_pref: s.transport_pref,
        }
    }
}

/// One HTTP request description (used for both transports).
#[derive(Clone, Debug)]
pub struct RequestSpec {
    /// Frozen, independently built Http12 route. Dispatch precedes H3.
    pub family_route: Option<FamilyRoute>,
    pub url: String,
    pub method: Method,
    /// `(start, end_inclusive)`; `None` end means "to the end of the file".
    pub range: Option<(u64, Option<u64>)>,
    /// `If-Range` validator so a changed file cannot silently corrupt a resume.
    pub if_range: Option<String>,
    pub extra_headers: Vec<(String, String)>,
}

impl RequestSpec {
    pub fn get(url: &str) -> Self {
        RequestSpec {
            family_route: None,
            url: url.to_string(),
            method: Method::GET,
            range: None,
            if_range: None,
            extra_headers: Vec::new(),
        }
    }

    pub fn head(url: &str) -> Self {
        RequestSpec {
            method: Method::HEAD,
            ..RequestSpec::get(url)
        }
    }

    pub fn with_range(mut self, start: u64, end: Option<u64>) -> Self {
        self.range = Some((start, end));
        self
    }

    pub fn with_if_range(mut self, validator: Option<String>) -> Self {
        self.if_range = validator;
        self
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers
            .push((name.to_string(), value.to_string()));
        self
    }

    pub(crate) fn with_family_route(mut self, route: Option<FamilyRoute>) -> Self {
        self.family_route = route;
        self
    }
}

#[derive(Clone, Debug)]
pub struct FamilyRoute {
    pub family: Family,
    client: Http12Client,
}

#[derive(Clone, Debug)]
pub(crate) struct FamilyPaths(pub [FamilyRoute; 2]);

/// What a probe learned about a URL.
#[derive(Clone, Debug)]
pub struct ProbeInfo {
    /// The URL we were asked about.
    pub url: String,
    /// Where it ended up after redirects.
    pub final_url: String,
    pub status: u16,
    pub size: Option<u64>,
    pub accept_ranges: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub filename_hint: Option<String>,
    pub content_type: Option<String>,
    pub transport: TransportKind,
    /// `Alt-Svc: h3` seen on this response (already stored by [`NetStack`]).
    pub learned_alt_svc: Option<AltSvcEntry>,
    pub headers: HeaderMap,
}

impl ProbeInfo {
    pub fn headers_vec(&self) -> Vec<(String, String)> {
        net::headers_to_vec(&self.headers)
    }

    /// Strong ETag for `If-Range`, or nothing.
    ///
    /// A weak tag is not a validator this engine will send. `Last-Modified`
    /// is not either: the field stays on the probe as a hint and is never
    /// copied into this return value.
    pub fn validator(&self) -> Option<String> {
        self.etag
            .as_deref()
            .and_then(net::strong_etag)
            .map(str::to_string)
    }

    fn from_headers(
        headers: &HeaderMap,
        status: u16,
        url: &str,
        final_url: String,
        transport: TransportKind,
    ) -> ProbeInfo {
        let mut size = net::content_length(headers);
        let mut accept_ranges = net::accepts_ranges(headers);
        if status == 206 {
            accept_ranges = true;
            if let Some((_, _, total)) = net::content_range(headers) {
                if total.is_some() {
                    size = total;
                }
            }
        }
        ProbeInfo {
            url: url.to_string(),
            final_url,
            status,
            size,
            accept_ranges,
            etag: net::etag(headers),
            last_modified: net::last_modified(headers),
            filename_hint: net::filename_hint(headers),
            content_type: net::content_type(headers),
            transport,
            learned_alt_svc: None,
            headers: headers.clone(),
        }
    }
}

/// Response body reader for either transport.
pub enum Body {
    /// No body (HEAD, or a status we do not want to read).
    Empty,
    H12(BoxStream<'static, std::result::Result<Bytes, reqwest::Error>>),
    H3(H3Response),
}

impl Body {
    /// Next chunk, `None` at end of body.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        match self {
            Body::Empty => Ok(None),
            Body::H12(stream) => match stream.next().await {
                Some(Ok(bytes)) => Ok(Some(bytes)),
                Some(Err(e)) => Err(Error::from(e)),
                None => Ok(None),
            },
            Body::H3(response) => response.chunk().await,
        }
    }

    /// Abandon the transfer (frees the connection for reuse where possible).
    pub fn abort(&mut self) {
        self.end_network();
    }

    /// Drop the response body. This is the network terminal for the connection
    /// use-axis: HTTP/3 in-use ends here, before any local tail flush the
    /// caller still has to do. `ActiveRequest` is a different lifetime.
    pub fn end_network(&mut self) {
        match std::mem::replace(self, Body::Empty) {
            Body::Empty => {}
            Body::H12(_) => {
                // Dropping the stream cancels the hyper body.
            }
            Body::H3(mut response) => response.abort(),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Body::Empty)
    }
}

impl std::fmt::Debug for Body {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Body::Empty => write!(f, "Body::Empty"),
            Body::H12(_) => write!(f, "Body::H12(..)"),
            Body::H3(response) => write!(f, "Body::H3({response:?})"),
        }
    }
}

/// A successfully issued request (any status; check [`Opened::is_success`]).
#[derive(Debug)]
pub struct Opened {
    /// Peer of this response; observation only, never a family pin.
    pub remote_addr: Option<std::net::SocketAddr>,
    pub status: u16,
    pub headers: HeaderMap,
    pub transport: TransportKind,
    pub final_url: String,
    pub body: Body,
}

impl Opened {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// `206 Partial Content`.
    pub fn is_partial(&self) -> bool {
        self.status == 206
    }

    /// `416 Range Not Satisfiable`. This is not proof the local file is complete.
    pub fn is_range_not_satisfiable(&self) -> bool {
        self.status == 416
    }

    pub fn content_range(&self) -> Option<(u64, u64, Option<u64>)> {
        net::content_range(&self.headers)
    }

    pub fn content_length(&self) -> Option<u64> {
        net::content_length(&self.headers)
    }

    pub fn header(&self, name: &HeaderName) -> Option<String> {
        net::header_str(&self.headers, name)
    }
}

struct Inner {
    http: RwLock<Http12Client>,
    test_roots: RwLock<Vec<Vec<u8>>>,
    h3: RwLock<Option<Arc<H3Pool>>>,
    alt_svc: Arc<dyn AltSvcStore>,
    settings: RwLock<NetSettings>,
    /// Armed only by tests. `None` means the tail flush is not held.
    tail_flush: RwLock<Option<Arc<TailFlushGate>>>,
}

/// Holds the segment tail flush after the response body has been dropped.
///
/// The connection use-axis has already ended. `ActiveRequest` stays until the
/// flush is released. Production leaves this unset, so the wait does not run.
#[derive(Debug)]
pub struct TailFlushGate {
    entered: AtomicBool,
    release: watch::Sender<bool>,
    wait: watch::Receiver<bool>,
}

impl TailFlushGate {
    fn new() -> Arc<Self> {
        let (release, wait) = watch::channel(false);
        Arc::new(Self {
            entered: AtomicBool::new(false),
            release,
            wait,
        })
    }

    /// `true` once a worker has dropped the body and is waiting to flush.
    pub fn entered(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }

    /// Let the waiting worker flush.
    pub fn release(&self) {
        let _ = self.release.send(true);
    }

    pub(crate) async fn wait_for_release(&self) {
        self.entered.store(true, Ordering::SeqCst);
        let mut wait = self.wait.clone();
        while !*wait.borrow() {
            if wait.changed().await.is_err() {
                return;
            }
        }
    }
}

/// Owns the two transports and the HTTP/3 knowledge base.
pub struct NetStack {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for NetStack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NetStack")
            .field("http3", &self.inner.h3.read().is_some())
            .finish()
    }
}

impl Clone for NetStack {
    fn clone(&self) -> Self {
        NetStack {
            inner: self.inner.clone(),
        }
    }
}

impl NetStack {
    pub fn new(settings: &Settings, alt_svc: Arc<dyn AltSvcStore>) -> Result<Self> {
        let net_settings = NetSettings::from_settings(settings);
        let http = Http12Client::new(&net_settings)?;
        let h3 = if net_settings.http3_enabled {
            match H3Pool::new(net_settings.connect_timeout, &net_settings.user_agent) {
                Ok(pool) => Some(Arc::new(pool)),
                Err(e) => {
                    tracing::warn!("HTTP/3 disabled for this session: {e}");
                    None
                }
            }
        } else {
            None
        };
        Ok(NetStack {
            inner: Arc::new(Inner {
                http: RwLock::new(http),
                test_roots: RwLock::new(Vec::new()),
                h3: RwLock::new(h3),
                alt_svc,
                settings: RwLock::new(net_settings),
                tail_flush: RwLock::new(None),
            }),
        })
    }

    pub fn settings(&self) -> NetSettings {
        self.inner.settings.read().clone()
    }

    pub fn alt_svc_store(&self) -> Arc<dyn AltSvcStore> {
        self.inner.alt_svc.clone()
    }

    /// Is the QUIC stack usable in this process?
    pub fn http3_available(&self) -> bool {
        self.inner.h3.read().is_some()
    }

    /// Replace the HTTP/3 pool.
    ///
    /// [`NetStack::new`] still builds its pool with [`H3Pool::new`] (the
    /// operating-system trust store). A dev test installs a pool it already
    /// built. This function does not know that pool's trust anchors.
    pub fn install_h3_pool(&self, pool: Arc<H3Pool>) {
        *self.inner.h3.write() = Some(pool);
    }

    /// Trust one extra DER root on the HTTP/1.1 and HTTP/2 client.
    ///
    /// The operating-system roots stay. Production never calls this, and it
    /// does not install that root into the QUIC pool. A test that needs
    /// HTTP/3 trust still uses [`Self::install_h3_pool`].
    #[doc(hidden)]
    pub fn install_test_root_der(&self, der: &[u8]) -> Result<()> {
        let settings = self.settings();
        self.inner
            .http
            .write()
            .install_extra_root_der(&settings, der)?;
        *self.inner.test_roots.write() = vec![der.to_vec()];
        Ok(())
    }

    pub(crate) fn family_paths(&self) -> Result<FamilyPaths> {
        let settings = self.settings();
        let roots = self.inner.test_roots.read();
        let roots: Vec<_> = roots.iter().map(Vec::as_slice).collect();
        // Two builds, hence two connectors and pools. Clones below retain only
        // their own family's configuration throughout this task attempt.
        Ok(FamilyPaths([
            FamilyRoute {
                family: Family::V4,
                client: Http12Client::family(&settings, &roots, Family::V4)?,
            },
            FamilyRoute {
                family: Family::V6,
                client: Http12Client::family(&settings, &roots, Family::V6)?,
            },
        ]))
    }

    /// Hold the next tail flush until [`TailFlushGate::release`].
    ///
    /// The worker drops the response body before it waits, so a test can
    /// observe in-use `0` while `active_requests` is still positive.
    /// Production does not call this.
    pub fn arm_tail_flush_gate(&self) -> Arc<TailFlushGate> {
        let gate = TailFlushGate::new();
        *self.inner.tail_flush.write() = Some(Arc::clone(&gate));
        gate
    }

    pub(crate) fn tail_flush_gate(&self) -> Option<Arc<TailFlushGate>> {
        self.inner.tail_flush.read().clone()
    }

    /// Process-wide connection census.
    ///
    /// H1 and H2 have no pool events yet, so they are `Unknown`. The total
    /// therefore stays `Unavailable` whether or not the H3 gauge was read.
    /// `Unavailable` is not zero: [`EngineConnections::active`] is `None`.
    pub fn engine_connections(&self) -> EngineConnections {
        let http3_enabled = self.inner.settings.read().http3_enabled;
        let h3 = if !http3_enabled {
            ProtocolGauge::Disabled
        } else {
            let pool = self.inner.h3.read().clone();
            match pool {
                Some(pool) => pool.connection_gauge(),
                None => ProtocolGauge::Unknown,
            }
        };
        EngineConnections::from_protocols(ProtocolGauge::Unknown, ProtocolGauge::Unknown, h3)
    }

    /// Apply new settings, rebuilding the clients.
    pub fn update_settings(&self, settings: &Settings) -> Result<()> {
        let next = NetSettings::from_settings(settings);
        self.inner.http.write().rebuild(&next)?;
        self.inner.test_roots.write().clear();
        {
            let mut slot = self.inner.h3.write();
            if next.http3_enabled && slot.is_none() {
                *slot = H3Pool::new(next.connect_timeout, &next.user_agent)
                    .map(Arc::new)
                    .ok();
            } else if !next.http3_enabled {
                *slot = None;
            }
        }
        *self.inner.settings.write() = next;
        Ok(())
    }

    /// Close pooled QUIC connections.
    pub async fn shutdown(&self) {
        let pool = self.inner.h3.read().clone();
        if let Some(pool) = pool {
            pool.close_all("udm shutting down").await;
        }
    }

    // ---------------------------------------------------------------- probe

    /// Ask the server about size / range support without downloading.
    pub async fn probe(&self, url: &str, pref: TransportPref) -> Result<ProbeInfo> {
        let parsed = validate(url)?;
        let effective = self.effective_pref(pref);

        if let Some(entry) = self.h3_target(&parsed, effective) {
            match self.probe_h3(&parsed, &entry).await {
                Ok(info) => {
                    self.record_h3_success(&entry.origin);
                    return Ok(info);
                }
                Err(e) => {
                    self.record_h3_failure(&entry.origin);
                    if effective == TransportPref::Http3Only {
                        return Err(e);
                    }
                    tracing::info!(
                        origin = %entry.origin,
                        "HTTP/3 probe failed ({e}); falling back to HTTP/2 or HTTP/1.1"
                    );
                }
            }
        }

        self.probe_http12(&parsed).await
    }

    async fn probe_http12(&self, url: &Url) -> Result<ProbeInfo> {
        let client = self.inner.http.read().clone();
        let settings = self.settings();
        let cap = settings
            .connect_timeout
            .saturating_add(settings.stall_timeout);

        let mut info: Option<ProbeInfo> = None;

        // 1) HEAD is cheapest.
        match client
            .send_capped(&RequestSpec::head(url.as_str()), cap)
            .await
        {
            Ok(response) => {
                let status = response.status().as_u16();
                let headers = response.headers().clone();
                let final_url = response.url().to_string();
                let transport = net::transport_from_version(response.version());
                drop(response);
                if (200..300).contains(&status) {
                    let mut probe = ProbeInfo::from_headers(
                        &headers,
                        status,
                        url.as_str(),
                        final_url.clone(),
                        transport,
                    );
                    self.learn_alt_svc(&mut probe, &final_url);
                    info = Some(probe);
                } else {
                    tracing::debug!("HEAD {url} -> HTTP {status}");
                }
            }
            Err(e) => tracing::debug!("HEAD {url} failed: {e}"),
        }

        // 2) If HEAD was refused or uninformative, ask for one byte. This is
        //    also the only reliable way to learn whether Range works.
        let needs_range_probe = match &info {
            None => true,
            Some(i) => i.size.is_none() || !i.accept_ranges,
        };
        if needs_range_probe {
            let spec = RequestSpec::get(url.as_str()).with_range(0, Some(0));
            let response = client.send_capped(&spec, cap).await?;
            let status = response.status().as_u16();
            let headers = response.headers().clone();
            let final_url = response.url().to_string();
            let transport = net::transport_from_version(response.version());
            drop(response);

            if !(200..300).contains(&status) {
                return Err(Error::HttpStatus {
                    status,
                    url: final_url,
                });
            }
            let mut probe = ProbeInfo::from_headers(
                &headers,
                status,
                url.as_str(),
                final_url.clone(),
                transport,
            );
            self.learn_alt_svc(&mut probe, &final_url);
            info = Some(probe);
        }

        info.ok_or_else(|| Error::Connect(format!("no usable response from {url}")))
    }

    async fn probe_h3(&self, url: &Url, entry: &AltSvcEntry) -> Result<ProbeInfo> {
        let pool = self.h3_pool()?;

        // HEAD first, then a one-byte GET when HEAD is uninformative.
        let (response, final_url) = self
            .h3_exchange(&pool, RequestSpec::head(url.as_str()), entry)
            .await?;
        let status = response.status.as_u16();
        let headers = response.headers.clone();
        let mut response = response;
        response.abort();

        if !(200..300).contains(&status) {
            // Servers are allowed to refuse HEAD; try a one-byte GET.
            if !matches!(status, 405 | 501 | 403 | 400) {
                return Err(Error::HttpStatus {
                    status,
                    url: final_url,
                });
            }
            let (response, final_url) = self
                .h3_exchange(
                    &pool,
                    RequestSpec::get(url.as_str()).with_range(0, Some(0)),
                    entry,
                )
                .await?;
            let status = response.status.as_u16();
            let headers = response.headers.clone();
            let mut response = response;
            response.abort();
            if !(200..300).contains(&status) {
                return Err(Error::HttpStatus {
                    status,
                    url: final_url,
                });
            }
            let mut probe = ProbeInfo::from_headers(
                &headers,
                status,
                url.as_str(),
                final_url.clone(),
                TransportKind::Http3,
            );
            self.learn_alt_svc(&mut probe, &final_url);
            return Ok(probe);
        }

        let size = net::content_length(&headers);
        let accept_ranges = net::accepts_ranges(&headers);
        if size.is_none() || !accept_ranges {
            let (response, final_url) = self
                .h3_exchange(
                    &pool,
                    RequestSpec::get(url.as_str()).with_range(0, Some(0)),
                    entry,
                )
                .await?;
            let status = response.status.as_u16();
            let headers = response.headers.clone();
            let mut response = response;
            response.abort();
            if !(200..300).contains(&status) {
                return Err(Error::HttpStatus {
                    status,
                    url: final_url,
                });
            }
            let mut probe = ProbeInfo::from_headers(
                &headers,
                status,
                url.as_str(),
                final_url.clone(),
                TransportKind::Http3,
            );
            self.learn_alt_svc(&mut probe, &final_url);
            return Ok(probe);
        }

        let mut probe = ProbeInfo::from_headers(
            &headers,
            status,
            url.as_str(),
            final_url.clone(),
            TransportKind::Http3,
        );
        self.learn_alt_svc(&mut probe, &final_url);
        Ok(probe)
    }

    // ---------------------------------------------------------------- open

    /// Observe a segment open without changing transport selection or I5.
    pub(crate) async fn open_observed(
        &self,
        spec: &RequestSpec,
        pref: TransportPref,
        table: &Arc<crate::net::lane::LaneTable>,
        task: u64,
        generation: u64,
        segment: usize,
    ) -> Result<Opened> {
        use crate::net::lane::{OpenObservation, OpenOutcome};
        let mut attempt = table.begin(OpenObservation {
            task,
            generation,
            segment,
            range: spec.range,
            preference: pref,
            requested_family: spec.family_route.as_ref().map(|r| r.family),
            outcome: OpenOutcome::Pending,
            failure: None,
            open_to_headers: None,
            observed_family: None,
            protocol: None,
        });
        let result = self.open(spec, pref).await;
        match &result {
            Ok(opened) => attempt.headers(opened.transport, opened.remote_addr),
            Err(error) => attempt.failed(error),
        }
        result
    }
    /// Open a byte range for transfer.
    pub async fn open(&self, spec: &RequestSpec, pref: TransportPref) -> Result<Opened> {
        let parsed = validate(&spec.url)?;
        if let Some(route) = &spec.family_route {
            // In particular, IPv6 never enters generic H3-first selection.
            let response = route.client.send(spec).await?;
            return Ok(opened_from_http12(response));
        }
        let effective = self.effective_pref(pref);

        if let Some(entry) = self.h3_target(&parsed, effective) {
            let pool = self.h3_pool()?;
            match self.h3_exchange(&pool, spec.clone(), &entry).await {
                Ok((response, final_url)) => {
                    self.record_h3_success(&entry.origin);
                    self.learn_alt_svc_headers(&response.headers, &final_url);
                    return Ok(opened_from_h3(response, final_url));
                }
                Err(e) => {
                    self.record_h3_failure(&entry.origin);
                    if effective == TransportPref::Http3Only {
                        return Err(e);
                    }
                    tracing::info!(
                        origin = %entry.origin,
                        "HTTP/3 request failed ({e}); retrying over HTTP/2 or HTTP/1.1"
                    );
                }
            }
        }

        self.open_http12(spec).await
    }

    async fn open_http12(&self, spec: &RequestSpec) -> Result<Opened> {
        let client = self.inner.http.read().clone();
        let response = client.send(spec).await?;
        self.learn_alt_svc_headers(response.headers(), response.url().as_str());
        Ok(opened_from_http12(response))
    }

    // ---------------------------------------------------------------- h3 plumbing

    fn h3_pool(&self) -> Result<Arc<H3Pool>> {
        self.inner
            .h3
            .read()
            .clone()
            .ok_or_else(|| Error::Http3("the QUIC stack is disabled in settings".to_string()))
    }
    fn effective_pref(&self, pref: TransportPref) -> TransportPref {
        let settings = self.inner.settings.read();
        if !settings.http3_enabled {
            return TransportPref::NoHttp3;
        }
        if pref == TransportPref::Auto {
            settings.default_pref
        } else {
            pref
        }
    }

    /// Should this request go out over QUIC, and to which port?
    fn h3_target(&self, url: &Url, pref: TransportPref) -> Option<AltSvcEntry> {
        if !pref.http3_allowed() {
            return None;
        }
        if url.scheme() != "https" {
            return None;
        }
        let host = url.host_str()?.to_ascii_lowercase();
        if host.parse::<std::net::IpAddr>().is_ok() {
            return None;
        }
        let settings = self.inner.settings.read();
        let https_port = url.port_or_known_default().unwrap_or(443);
        let key = origin_key(&host, https_port);

        if let Some(entry) = self.inner.alt_svc.get(&key) {
            if entry.is_valid(util::now_millis(), settings.alt_svc_max_age.as_secs()) {
                return Some(entry);
            }
        }
        if pref.http3_eager() {
            // Speculative: assume QUIC on the same port. Failures are counted,
            // so we stop trying quickly when the origin has no QUIC listener.
            return Some(AltSvcEntry::new(&host, https_port, 0));
        }
        None
    }

    fn record_h3_failure(&self, origin: &str) {
        if self.inner.settings.read().learn_alt_svc {
            self.inner.alt_svc.record_failure(origin);
        }
    }

    fn record_h3_success(&self, origin: &str) {
        if self.inner.settings.read().learn_alt_svc {
            self.inner.alt_svc.record_success(origin);
        }
    }

    /// Send a request over HTTP/3, following redirects as long as they stay on
    /// origins that also speak HTTP/3.
    async fn h3_exchange(
        &self,
        pool: &H3Pool,
        spec: RequestSpec,
        entry: &AltSvcEntry,
    ) -> Result<(H3Response, String)> {
        let max_redirects = self.inner.settings.read().max_redirects;
        let mut spec = rewrite_for_h3(spec, entry);
        let mut current = spec.url.clone();

        for _ in 0..=max_redirects {
            let mut response = pool.request(&spec).await?;
            if !response.status.is_redirection() {
                return Ok((response, current));
            }
            let location = match net::header_str(&response.headers, &LOCATION) {
                Some(l) => l,
                None => return Ok((response, current)),
            };
            response.abort();
            let next = Url::parse(&current)?.join(&location)?;
            let next = validate(next.as_str())?;

            let host = next
                .host_str()
                .ok_or_else(|| Error::InvalidUrl("redirect target has no host".into()))?
                .to_ascii_lowercase();
            let port = next.port_or_known_default().unwrap_or(443);
            let key = origin_key(&host, port);
            let known = self
                .inner
                .alt_svc
                .get(&key)
                .map(|e| {
                    e.is_valid(
                        util::now_millis(),
                        self.inner.settings.read().alt_svc_max_age.as_secs(),
                    )
                })
                .unwrap_or(false);
            if !known && host != entry.host {
                return Err(Error::Http3(format!(
                    "redirect to `{host}` which has no known HTTP/3 endpoint"
                )));
            }
            spec.url = next.to_string();
            current = next.to_string();
        }
        Err(Error::Other(format!(
            "more than {max_redirects} redirects over HTTP/3"
        )))
    }

    // ---------------------------------------------------------------- alt-svc

    fn learn_alt_svc(&self, probe: &mut ProbeInfo, final_url: &str) {
        if let Some(entry) = self.learn_alt_svc_headers(&probe.headers, final_url) {
            probe.learned_alt_svc = Some(entry);
        }
    }

    /// Store an `Alt-Svc: h3` advertisement, if the settings allow it.
    fn learn_alt_svc_headers(&self, headers: &HeaderMap, final_url: &str) -> Option<AltSvcEntry> {
        if !self.inner.settings.read().learn_alt_svc {
            return None;
        }
        let url = Url::parse(final_url).ok()?;
        let host = url.host_str()?.to_ascii_lowercase();
        let port = url.port_or_known_default().unwrap_or(443);
        let entry = net::alt_svc_from(headers, &host, port)?;
        tracing::debug!(origin = %entry.origin, h3_port = entry.port, "learned HTTP/3 endpoint");
        self.inner.alt_svc.put(entry.clone());
        Some(entry)
    }
}

/// Rewrite a spec so it targets the QUIC port we learned about.
fn rewrite_for_h3(mut spec: RequestSpec, entry: &AltSvcEntry) -> RequestSpec {
    if let Ok(mut url) = Url::parse(&spec.url) {
        let current_port = url.port_or_known_default().unwrap_or(443);
        if entry.port != current_port {
            let _ = url.set_port(Some(entry.port));
            spec.url = url.to_string();
        }
    }
    spec
}

fn opened_from_http12(response: reqwest::Response) -> Opened {
    let remote_addr = response.remote_addr();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let final_url = response.url().to_string();
    let transport = net::transport_from_version(response.version());
    let body = if (200..300).contains(&status) {
        Body::H12(Box::pin(response.bytes_stream()))
    } else {
        Body::Empty
    };
    Opened {
        remote_addr,
        status,
        headers,
        transport,
        final_url,
        body,
    }
}

fn opened_from_h3(mut response: H3Response, final_url: String) -> Opened {
    let remote_addr = Some(response.remote_addr);
    let status = response.status.as_u16();
    let headers = response.headers.clone();
    let body = if (200..300).contains(&status) {
        Body::H3(response)
    } else {
        response.abort();
        Body::Empty
    };
    Opened {
        remote_addr,
        status,
        headers,
        transport: TransportKind::Http3,
        final_url,
        body,
    }
}

/// Parse and validate a user-supplied URL.
pub fn validate(url: &str) -> Result<Url> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(Error::InvalidUrl("URL is empty".into()));
    }
    let parsed = Url::parse(trimmed)?;
    match parsed.scheme() {
        "http" | "https" => Ok(parsed),
        other => Err(Error::UnsupportedScheme(other.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::MemAltSvc;

    fn stack() -> NetStack {
        let settings = Settings::default();
        NetStack::new(&settings, Arc::new(MemAltSvc::new())).unwrap()
    }

    #[test]
    fn if_range_validator_is_a_strong_etag_or_nothing() {
        let mut headers = HeaderMap::new();
        headers.insert(http::header::ETAG, "W/\"abc\"".parse().unwrap());
        headers.insert(
            http::header::LAST_MODIFIED,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        let weak = ProbeInfo::from_headers(
            &headers,
            200,
            "http://h/a",
            "http://h/a".into(),
            TransportKind::Http11,
        );
        assert!(weak.validator().is_none());
        assert!(weak.last_modified.is_some());

        let mut headers = HeaderMap::new();
        headers.insert(http::header::ETAG, "\"abc\"".parse().unwrap());
        headers.insert(
            http::header::LAST_MODIFIED,
            "Wed, 21 Oct 2015 07:28:00 GMT".parse().unwrap(),
        );
        let strong = ProbeInfo::from_headers(
            &headers,
            200,
            "http://h/a",
            "http://h/a".into(),
            TransportKind::Http11,
        );
        assert_eq!(strong.validator().as_deref(), Some("\"abc\""));
    }

    #[test]
    fn validates_urls() {
        assert!(validate("https://example.com/x").is_ok());
        assert!(validate("http://example.com/x").is_ok());
        assert!(matches!(
            validate("ftp://example.com/x"),
            Err(Error::UnsupportedScheme(_))
        ));
        assert!(validate("   ").is_err());
        assert!(validate("not a url").is_err());
    }

    #[test]
    fn h3_target_respects_preferences() {
        let stack = stack();
        let url = Url::parse("https://cdn.example/file.bin").unwrap();
        // No cached Alt-Svc and Auto -> no speculative QUIC.
        assert!(stack.h3_target(&url, TransportPref::Auto).is_none());
        // Eager preferences dial QUIC anyway.
        let eager = stack.h3_target(&url, TransportPref::PreferHttp3).unwrap();
        assert_eq!(eager.port, 443);
        // Disabled outright.
        assert!(stack.h3_target(&url, TransportPref::NoHttp3).is_none());
        // Plain http:// never uses QUIC.
        let plain = Url::parse("http://cdn.example/file.bin").unwrap();
        assert!(stack.h3_target(&plain, TransportPref::Http3Only).is_none());
    }

    #[test]
    fn cached_alt_svc_enables_auto_mode() {
        let stack = stack();
        stack
            .alt_svc_store()
            .put(AltSvcEntry::new("cdn.example", 443, 3600));
        let url = Url::parse("https://cdn.example/file.bin").unwrap();
        assert!(stack.h3_target(&url, TransportPref::Auto).is_some());
    }

    #[test]
    fn rewrites_the_quic_port() {
        let entry = AltSvcEntry::new("cdn.example", 8443, 60);
        let spec = rewrite_for_h3(RequestSpec::get("https://cdn.example/f.bin"), &entry);
        assert_eq!(spec.url, "https://cdn.example:8443/f.bin");
    }

    #[test]
    fn engine_connection_snapshot_stays_unavailable() {
        let stack = stack();
        assert!(stack.engine_connections().active().is_none());
        let settings = Settings {
            http3_enabled: false,
            ..Settings::default()
        };
        stack.update_settings(&settings).unwrap();
        assert!(stack.engine_connections().active().is_none());
    }

    #[test]
    fn request_spec_builders() {
        let spec = RequestSpec::get("https://x.example/f")
            .with_range(10, Some(20))
            .with_if_range(Some("\"etag\"".into()))
            .with_header("X-Test", "1");
        assert_eq!(spec.range, Some((10, Some(20))));
        assert_eq!(spec.if_range.as_deref(), Some("\"etag\""));
        assert_eq!(spec.extra_headers.len(), 1);
        assert_eq!(RequestSpec::head("u").method, Method::HEAD);
    }
}

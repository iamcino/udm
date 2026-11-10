//! HTTP/3 (QUIC) client built on [`quinn`] + [`h3`].
//!
//! One [`quinn::Endpoint`] is shared by the whole process. Connections are
//! pooled per origin and multiplexed. A pool miss starts one handshake for
//! that origin; later callers wait for it instead of dialing again. The
//! handshake is not held across the pool lock, so a different origin is not
//! stuck behind it. `stable_id` is stored only for the connection that is
//! actually inserted. That id is in use only while an accepted request is
//! still on the network. The count ends at body EOF, abort, error, or cancel,
//! which is earlier than the tail flush `active_requests` still covers.
//! GOAWAY marks the id draining: it takes no new request, and the draining
//! flag is not added into `active`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::{Buf, Bytes};
use http::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT, ACCEPT_ENCODING, IF_RANGE, RANGE, USER_AGENT,
};
use http::StatusCode;
use tokio::sync::{oneshot, Mutex as AsyncMutex};

use crate::error::{Error, Result};
use crate::net::gauge::ProtocolGauge;
use crate::net::stack::RequestSpec;
use crate::net::tls;
use crate::net::ACCEPT_ENCODING_IDENTITY;

type H3Sender = h3::client::SendRequest<h3_quinn::OpenStreams, Bytes>;
type H3Stream = h3::client::RequestStream<h3_quinn::BidiStream<Bytes>, Bytes>;

/// A finished handshake that is not yet a pool member.
///
/// `stable_id` is deliberately absent. It is copied onto [`PooledConn`] at
/// insert, so a handshake we close instead of inserting leaves no id behind.
struct Handshake {
    sender: H3Sender,
    quinn: quinn::Connection,
}

/// Use-axis for one pooled id. The gauge reads these atomics; they are not a
/// second connection count.
struct ConnUse {
    /// Accepted requests that have not reached a network terminal.
    in_use: AtomicU64,
    /// The peer will take no new request on this id (GOAWAY or equivalent).
    draining: AtomicBool,
}

/// One origin's pooled connection, with the id recorded at insert.
#[derive(Clone)]
struct PooledConn {
    sender: H3Sender,
    quinn: quinn::Connection,
    stable_id: usize,
    use_axis: Arc<ConnUse>,
}

/// One accepted request. Dropping it, or releasing it at EOF, ends that
/// request's contribution to [`ConnUse::in_use`].
struct RequestUse {
    axis: Arc<ConnUse>,
    released: AtomicBool,
}

impl RequestUse {
    fn begin(axis: Arc<ConnUse>) -> Self {
        axis.in_use.fetch_add(1, Ordering::AcqRel);
        Self {
            axis,
            released: AtomicBool::new(false),
        }
    }

    fn release(&self) {
        if self.released.swap(true, Ordering::AcqRel) {
            return;
        }
        self.axis.in_use.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for RequestUse {
    fn drop(&mut self) {
        self.release();
    }
}

/// Waiters blocked on the single in-flight handshake for one origin.
struct Flight {
    waiters: Vec<oneshot::Sender<std::result::Result<PooledConn, Arc<str>>>>,
}

struct PoolState {
    conns: HashMap<String, PooledConn>,
    flights: HashMap<String, Flight>,
}

/// Removes a flight if the leader is dropped before it publishes.
///
/// Cancellation and panic are failures: waiters wake, and the origin can
/// start a later handshake. Nothing is inserted, so no id is recorded.
struct LeadGuard {
    key: String,
    state: Arc<AsyncMutex<PoolState>>,
    armed: bool,
}

impl Drop for LeadGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let key = std::mem::take(&mut self.key);
        let state = Arc::clone(&self.state);
        if let Ok(mut guard) = state.try_lock() {
            fail_flight(&mut guard, &key);
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let mut guard = state.lock().await;
                fail_flight(&mut guard, &key);
            });
        }
    }
}

impl LeadGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

fn fail_flight(state: &mut PoolState, key: &str) {
    let Some(flight) = state.flights.remove(key) else {
        return;
    };
    for waiter in flight.waiters {
        let _ = waiter.send(Err(Arc::<str>::from(
            "HTTP/3 connect ended before the handshake finished",
        )));
    }
}

/// How the H3 pool answered a read of its recorded connection ids.
///
/// `Unknown` means the pool lock was busy. It is not an empty pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum H3StableIds {
    Unknown,
    Known(Vec<usize>),
}

/// Connection pool + QUIC endpoint.
pub struct H3Pool {
    endpoint: quinn::Endpoint,
    state: Arc<AsyncMutex<PoolState>>,
    /// Guards `Endpoint::connect` + the HTTP/3 handshake.
    connect_timeout: Duration,
    /// Guards a single `send_request`/`recv_response` round trip.
    request_timeout: Duration,
    user_agent: String,
}

impl std::fmt::Debug for H3Pool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H3Pool")
            .field("endpoint", &self.endpoint.local_addr().ok())
            .field("connect_timeout", &self.connect_timeout)
            .finish()
    }
}

impl H3Pool {
    /// Create the QUIC endpoint (binds an ephemeral UDP port; needs no
    /// privileges and opens no listening TCP port).
    ///
    /// Trust comes from [`tls::tuned_client_config`] (the operating-system
    /// roots). This function does not accept a caller-supplied certificate.
    pub fn new(connect_timeout: Duration, user_agent: &str) -> Result<Self> {
        tls::install_default_provider();
        Self::with_client_config(connect_timeout, user_agent, tls::tuned_client_config()?)
    }

    /// Pool that dials with `client` instead of the operating-system trust store.
    ///
    /// The engine calls [`H3Pool::new`]. A dev test passes a config it built
    /// itself; this function does not know that config's trust anchors.
    pub fn with_client_config(
        connect_timeout: Duration,
        user_agent: &str,
        client: quinn::ClientConfig,
    ) -> Result<Self> {
        tls::install_default_provider();
        let bind: SocketAddr = "0.0.0.0:0"
            .parse()
            .map_err(|e| Error::Http3(format!("cannot parse bind address: {e}")))?;
        let mut endpoint = quinn::Endpoint::client(bind)
            .map_err(|e| Error::Http3(format!("cannot create QUIC endpoint: {e}")))?;
        // Without a default client config `Endpoint::connect` always fails.
        endpoint.set_default_client_config(client);
        Ok(H3Pool {
            endpoint,
            state: Arc::new(AsyncMutex::new(PoolState {
                conns: HashMap::new(),
                flights: HashMap::new(),
            })),
            connect_timeout,
            request_timeout: connect_timeout.max(Duration::from_secs(10)),
            user_agent: user_agent.to_string(),
        })
    }

    /// Number of live pooled connections (non-blocking).
    ///
    /// A busy lock returns 0. That 0 is not evidence of an empty pool, so
    /// instrumentation must not call this. Use [`H3Pool::connection_gauge`].
    pub fn pooled_connections(&self) -> usize {
        match self.state.try_lock() {
            Ok(state) => state
                .conns
                .values()
                .filter(|conn| conn.quinn.close_reason().is_none())
                .count(),
            Err(_) => 0,
        }
    }

    /// H3 membership sample.
    ///
    /// A busy pool lock is [`ProtocolGauge::Unknown`], never a zero `Known`
    /// sample. `active` is the pooled ids whose in-use count is greater than
    /// zero, including a draining id that still has an accepted request on
    /// the network. `idle` is pooled, in-use zero, and not draining. `draining`
    /// is the flag count: it overlaps `active` and is not added into it.
    /// `active + idle` is at most `pooled`.
    pub fn connection_gauge(&self) -> ProtocolGauge {
        let Ok(state) = self.state.try_lock() else {
            return ProtocolGauge::Unknown;
        };
        let mut active = 0u64;
        let mut idle = 0u64;
        let mut draining = 0u64;
        for conn in state.conns.values() {
            let in_use = conn.use_axis.in_use.load(Ordering::Acquire);
            let is_draining = conn.use_axis.draining.load(Ordering::Acquire);
            if is_draining {
                draining = draining.saturating_add(1);
            }
            if in_use > 0 {
                active = active.saturating_add(1);
            } else if !is_draining {
                idle = idle.saturating_add(1);
            }
        }
        let pooled = state.conns.len() as u64;
        let connecting = state.flights.len() as u64;
        ProtocolGauge::Known {
            active,
            idle,
            connecting,
            draining,
            pooled,
        }
    }

    /// Accepted requests on pooled connections that have not reached a network
    /// terminal. `None` when the pool lock is busy. That is not zero.
    pub fn in_use_requests(&self) -> Option<u64> {
        let state = self.state.try_lock().ok()?;
        Some(
            state
                .conns
                .values()
                .map(|conn| conn.use_axis.in_use.load(Ordering::Acquire))
                .fold(0u64, u64::saturating_add),
        )
    }

    /// Ids recorded at insert. `Unknown` when the pool lock could not be read.
    pub fn recorded_stable_ids(&self) -> H3StableIds {
        let Ok(state) = self.state.try_lock() else {
            return H3StableIds::Unknown;
        };
        let mut ids: Vec<usize> = state.conns.values().map(|conn| conn.stable_id).collect();
        ids.sort_unstable();
        H3StableIds::Known(ids)
    }

    /// Send a request, following nothing (redirects are handled by the caller
    /// so it can decide to fall back to HTTP/2 for a different origin).
    pub async fn request(&self, spec: &RequestSpec) -> Result<H3Response> {
        let url = url::Url::parse(&spec.url)?;
        if url.scheme() != "https" {
            return Err(Error::UnsupportedScheme(format!(
                "{} (HTTP/3 requires TLS)",
                url.scheme()
            )));
        }
        let host = url
            .host_str()
            .ok_or_else(|| Error::InvalidUrl("URL has no host".into()))?
            .to_string();
        if host.parse::<std::net::IpAddr>().is_ok() {
            return Err(Error::Http3(format!(
                "HTTP/3 needs a DNS name for SNI, got the IP literal `{host}`"
            )));
        }
        let port = url.port_or_known_default().unwrap_or(443);

        let key = origin_key(&host, port);
        let conn = self.connection(&host, port).await?;
        let connection_id = conn.stable_id;
        let use_axis = Arc::clone(&conn.use_axis);
        let mut sender = conn.sender.clone();

        let request = build_request(spec, &self.user_agent)?;
        let send_result = tokio::time::timeout(self.request_timeout, sender.send_request(request))
            .await
            .map_err(|_| Error::Timeout {
                secs: self.request_timeout.as_secs(),
            })?;
        let mut stream = match send_result {
            Ok(stream) => stream,
            Err(err) if peer_refused_new_request(&err) => {
                // GOAWAY was observed before a new stream was accepted.
                self.mark_draining(&key).await;
                return Err(draining_refusal(connection_id));
            }
            Err(err) => {
                return Err(Error::Http3(format!("cannot open HTTP/3 stream: {err}")));
            }
        };

        // Accepted: send_request returned a stream. In-use starts here and
        // ends at the network terminal, not at the caller's tail flush.
        let use_guard = RequestUse::begin(use_axis);

        // No request body for GET/HEAD.
        if let Err(err) = stream.finish().await {
            drop(use_guard);
            if peer_refused_new_request(&err) {
                self.mark_draining(&key).await;
                return Err(draining_refusal(connection_id));
            }
            return Err(Error::Http3(format!("cannot finish HTTP/3 stream: {err}")));
        }

        let response =
            match tokio::time::timeout(self.request_timeout, stream.recv_response()).await {
                Err(_) => {
                    return Err(Error::Timeout {
                        secs: self.request_timeout.as_secs(),
                    });
                }
                Ok(Err(err)) if peer_refused_new_request(&err) => {
                    drop(use_guard);
                    self.mark_draining(&key).await;
                    return Err(draining_refusal(connection_id));
                }
                Ok(Err(err)) => {
                    return Err(Error::Http3(format!("no HTTP/3 response: {err}")));
                }
                Ok(Ok(response)) => response,
            };

        // The connection is only healthy if the driver is still running.
        if let Some(reason) = conn.quinn.close_reason() {
            drop(use_guard);
            self.state.lock().await.conns.remove(&key);
            return Err(Error::Http3(format!("QUIC connection closed: {reason}")));
        }

        Ok(H3Response {
            remote_addr: conn.quinn.remote_address(),
            status: response.status(),
            headers: response.headers().clone(),
            connection_id,
            stream,
            use_guard,
            network_done: false,
            body_read_pending: false,
        })
    }

    /// Reuse a pooled connection or share the one in-flight handshake.
    async fn connection(&self, host: &str, port: u16) -> Result<PooledConn> {
        let key = origin_key(host, port);
        let mut state = self.state.lock().await;
        if let Some(conn) = state.conns.get(&key) {
            if conn.quinn.close_reason().is_none() {
                if conn.use_axis.draining.load(Ordering::Acquire) {
                    // The id stays pooled so the gauge can report draining.
                    // A replacement handshake would be a different id; this
                    // request does not get that one either.
                    let stable_id = conn.stable_id;
                    return Err(draining_refusal(stable_id));
                }
                return Ok(conn.clone());
            }
        }
        // A dead member is no longer in the pool, so its id is retired
        // before a replacement handshake may record a new one.
        state.conns.remove(&key);
        if let Some(flight) = state.flights.get_mut(&key) {
            let (tx, rx) = oneshot::channel();
            flight.waiters.push(tx);
            drop(state);
            return await_shared(rx).await;
        }
        state.flights.insert(
            key.clone(),
            Flight {
                waiters: Vec::new(),
            },
        );
        drop(state);

        let mut lead = LeadGuard {
            key: key.clone(),
            state: Arc::clone(&self.state),
            armed: true,
        };
        // Dial outside the lock. Other origins take the lock and proceed.
        let connected = self.connect(host, port).await;
        self.finish_flight(&key, connected, &mut lead).await
    }

    async fn finish_flight(
        &self,
        key: &str,
        connected: Result<Handshake>,
        lead: &mut LeadGuard,
    ) -> Result<PooledConn> {
        let mut state = self.state.lock().await;
        let Some(flight) = state.flights.remove(key) else {
            drop(state);
            lead.disarm();
            return match connected {
                Ok(hs) => {
                    // Shutdown, or a dropped lead, already retired this flight.
                    // Closing here is what keeps the extra handshake out of the map.
                    hs.quinn.close(quinn::VarInt::from_u32(0), b"closed");
                    Err(Error::Http3(
                        "HTTP/3 connect ended before the handshake finished".into(),
                    ))
                }
                Err(err) => Err(err),
            };
        };

        let (leader, shared) = match connected {
            Ok(hs) => {
                state
                    .conns
                    .retain(|_, conn| conn.quinn.close_reason().is_none());
                let pooled = if let Some(existing) = state.conns.get(key).cloned() {
                    hs.quinn.close(quinn::VarInt::from_u32(0), b"duplicate");
                    existing
                } else {
                    // Recorded at insert, and only on the connection we keep.
                    let stable_id = hs.quinn.stable_id();
                    let pooled = PooledConn {
                        sender: hs.sender,
                        quinn: hs.quinn,
                        stable_id,
                        use_axis: Arc::new(ConnUse {
                            in_use: AtomicU64::new(0),
                            draining: AtomicBool::new(false),
                        }),
                    };
                    state.conns.insert(key.to_string(), pooled.clone());
                    pooled
                };
                (Ok(pooled.clone()), Ok(pooled))
            }
            Err(err) => {
                let msg = Arc::<str>::from(err.to_string());
                (Err(err), Err(msg))
            }
        };
        for waiter in flight.waiters {
            let _ = waiter.send(shared.clone());
        }
        drop(state);
        lead.disarm();
        leader
    }

    async fn connect(&self, host: &str, port: u16) -> Result<Handshake> {
        let addrs = resolve(host, port).await?;
        let mut last_err = Error::Http3(format!("no address found for {host}:{port}"));
        for addr in addrs {
            match self.connect_to(addr, host).await {
                Ok(conn) => {
                    tracing::debug!(host, %addr, "HTTP/3 connection established");
                    return Ok(conn);
                }
                Err(e) => {
                    tracing::debug!(host, %addr, "QUIC connect failed: {e}");
                    last_err = e;
                }
            }
        }
        Err(last_err)
    }

    async fn connect_to(&self, addr: SocketAddr, host: &str) -> Result<Handshake> {
        let quinn = tokio::time::timeout(self.connect_timeout, async {
            let connecting = self.endpoint.connect(addr, host).map_err(Error::from)?;
            connecting.await.map_err(Error::from)
        })
        .await
        .map_err(|_| Error::Timeout {
            secs: self.connect_timeout.as_secs(),
        })??;
        let (driver, sender) = tokio::time::timeout(
            self.connect_timeout,
            h3::client::new(h3_quinn::Connection::new(quinn.clone())),
        )
        .await
        .map_err(|_| Error::Timeout {
            secs: self.connect_timeout.as_secs(),
        })?
        .map_err(|e| Error::Http3(format!("HTTP/3 handshake failed: {e}")))?;

        // Drive the connection until it closes. Dropping every `SendRequest`
        // clone closes it cleanly with H3_NO_ERROR.
        let host_owned = host.to_string();
        tokio::spawn(async move {
            let mut driver = driver;
            let err = std::future::poll_fn(|cx| driver.poll_close(cx)).await;
            if !err.is_h3_no_error() {
                tracing::debug!(host = %host_owned, "HTTP/3 connection ended: {err:?}");
            }
        });

        Ok(Handshake { sender, quinn })
    }

    /// Close every pooled connection (used on shutdown).
    ///
    /// In-flight handshakes are failed rather than inserted afterwards, so
    /// shutdown does not record an id for a connection it just asked to close.
    pub async fn close_all(&self, reason: &str) {
        let mut state = self.state.lock().await;
        let reason_bytes = reason.as_bytes().get(..32).unwrap_or(b"closing");
        for (_, conn) in state.conns.drain() {
            conn.quinn.close(quinn::VarInt::from_u32(0), reason_bytes);
        }
        let flights = std::mem::take(&mut state.flights);
        drop(state);
        for (_, flight) in flights {
            for waiter in flight.waiters {
                let _ = waiter.send(Err(Arc::<str>::from("HTTP/3 pool closed during connect")));
            }
        }
    }

    async fn mark_draining(&self, key: &str) {
        let state = self.state.lock().await;
        if let Some(conn) = state.conns.get(key) {
            conn.use_axis.draining.store(true, Ordering::Release);
        }
    }
}

fn draining_refusal(stable_id: usize) -> Error {
    Error::Http3(format!(
        "HTTP/3 connection {stable_id} is draining and will not take a new request"
    ))
}

/// GOAWAY, or the server reset a stream it will not accept after GOAWAY.
///
/// A visible [`h3::error::Code`] is compared by value. h3 0.0.8 allows that
/// on `RemoteTerminate` and `StreamError`: the variants are `non_exhaustive`
/// struct variants, so the pattern ends in `..` and reads `code`. Their
/// `Display` text is not consulted.
///
/// Two shapes still have no typed read from this crate. `RemoteClosing` is a
/// unit variant marked `non_exhaustive` unless
/// `i-implement-a-third-party-backend-and-opt-into-breaking-changes` is
/// enabled, and it is not. `ConnectionError` (and `ConnectionError::Remote`)
/// are `non_exhaustive` tuple variants, which rustc treats as private, so the
/// application code inside a connection close cannot be read either.
/// `is_h3_no_error` does not name either case. Quinn `close_reason` stays
/// `None` across a GOAWAY: the QUIC connection is still open. What remains is
/// `Display` text. `RemoteClosing` is the whole sentence "Remote is closing
/// the connection". A connection close that carries the rejected code is
/// "Connection error: Remote error: ApplicationClose: H3_REQUEST_REJECTED",
/// and only the code name is visible from here.
/// `pinned_h3_goaway_refusal_display_strings` captures both full messages
/// from h3 0.0.8 and fails if either one changes.
fn peer_refused_new_request(err: &h3::error::StreamError) -> bool {
    if visible_request_rejected(err) {
        return true;
    }
    let text = err.to_string();
    text.contains("Remote is closing the connection") || text.contains("H3_REQUEST_REJECTED")
}

/// `H3_REQUEST_REJECTED` on a variant whose `code` field is public.
///
/// False when the variant cannot be named, or when it carries a different
/// code. The caller then uses the pinned `Display` text.
fn visible_request_rejected(err: &h3::error::StreamError) -> bool {
    const REJECTED: h3::error::Code = h3::error::Code::H3_REQUEST_REJECTED;
    match err {
        h3::error::StreamError::RemoteTerminate { code, .. }
        | h3::error::StreamError::StreamError { code, .. } => *code == REJECTED,
        _ => false,
    }
}

async fn await_shared(
    rx: oneshot::Receiver<std::result::Result<PooledConn, Arc<str>>>,
) -> Result<PooledConn> {
    match rx.await {
        Ok(Ok(conn)) => Ok(conn),
        Ok(Err(msg)) => Err(Error::Http3(msg.to_string())),
        Err(_canceled) => Err(Error::Http3(
            "HTTP/3 connect ended before the handshake finished".into(),
        )),
    }
}

/// An HTTP/3 response: headers plus a stream you can pull body chunks from.
pub struct H3Response {
    /// Peer observed on the QUIC connection serving this response.
    pub remote_addr: SocketAddr,
    pub status: StatusCode,
    pub headers: HeaderMap,
    /// `stable_id` stored when this connection was inserted into the pool.
    pub connection_id: usize,
    stream: H3Stream,
    use_guard: RequestUse,
    /// Set once the response has reached a network terminal, so a later drop
    /// does not send a second STOP_SENDING after a clean EOF.
    network_done: bool,
    /// `recv_data` has been polled and has not returned.
    ///
    /// Dropping that future does not clear this. h3-quinn 0.0.10 leaves the
    /// quinn recv half inside the read until the read finishes, so a later
    /// `stop_sending` would unwrap `None`.
    body_read_pending: bool,
}

impl std::fmt::Debug for H3Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("H3Response")
            .field("status", &self.status)
            .field("connection_id", &self.connection_id)
            .finish()
    }
}

impl H3Response {
    /// Next body chunk, or `None` at the end of the stream.
    ///
    /// `None` and a read error are network terminals: in-use ends here, even
    /// if the caller still holds this response across a later local flush.
    ///
    /// `body_read_pending` is cleared only after `recv_data` returns. Cancel
    /// drops this future at the await, and the flag stays set on purpose.
    pub async fn chunk(&mut self) -> Result<Option<Bytes>> {
        self.body_read_pending = true;
        let read = self.stream.recv_data().await;
        self.body_read_pending = false;
        match read {
            Ok(Some(mut buf)) => {
                let len = buf.remaining();
                if len == 0 {
                    Ok(Some(Bytes::new()))
                } else {
                    Ok(Some(buf.copy_to_bytes(len)))
                }
            }
            Ok(None) => {
                self.finish_network(false);
                Ok(None)
            }
            Err(e) => {
                // The read has returned, so the quinn recv half is back in
                // place and `stop_sending` is the H3_NO_ERROR path below.
                self.finish_network(true);
                Err(Error::Http3(format!("HTTP/3 body read failed: {e}")))
            }
        }
    }

    /// Stop the transfer (used when we only wanted the headers, or on cancel).
    ///
    /// This is a network terminal. It does not wait for a stall timer.
    pub fn abort(&mut self) {
        self.finish_network(true);
    }

    fn finish_network(&mut self, stop_sending: bool) {
        if !self.network_done {
            self.network_done = true;
            // h3-quinn 0.0.10 takes the quinn recv half out of its slot for the
            // whole pending read (`poll_data`) and `stop_sending` unwraps that
            // slot. `catch_unwind` would still run the panic hook and print
            // the unwrap. The half lives in this response: dropping `stream`
            // drops it, and `quinn::RecvStream`'s Drop sends STOP_SENDING(0).
            // Skip the explicit call only while the read owns the half. A half
            // that is still in place (no body read yet, or a read that already
            // returned) still gets STOP_SENDING(H3_NO_ERROR), 0x100, not 0.
            if stop_sending && !self.body_read_pending {
                self.stream.stop_sending(h3::error::Code::H3_NO_ERROR);
            }
        }
        self.use_guard.release();
    }
}

impl Drop for H3Response {
    fn drop(&mut self) {
        // Cancel and drop are network terminals too. A clean EOF has already
        // set `network_done`, so this does not reset a finished stream. A
        // pending body read skips the explicit STOP_SENDING inside
        // `finish_network`; dropping `stream` is what releases the quinn half.
        self.finish_network(true);
    }
}

/// Build the `http::Request` for a spec, including the headers every download
/// request needs.
pub(crate) fn build_request(spec: &RequestSpec, user_agent: &str) -> Result<http::Request<()>> {
    let mut builder = http::Request::builder()
        .method(spec.method.clone())
        .uri(spec.url.as_str())
        .header(USER_AGENT, header_value(user_agent)?)
        .header(ACCEPT, header_value("*/*")?)
        .header(ACCEPT_ENCODING, header_value(ACCEPT_ENCODING_IDENTITY)?);
    if let Some((start, end)) = spec.range {
        let value = match end {
            Some(end) => format!("bytes={start}-{end}"),
            None => format!("bytes={start}-"),
        };
        builder = builder.header(RANGE, header_value(&value)?);
    }
    if let Some(if_range) = &spec.if_range {
        builder = builder.header(IF_RANGE, header_value(if_range)?);
    }
    for (name, value) in &spec.extra_headers {
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| Error::Other(format!("bad header name `{name}`: {e}")))?;
        builder = builder.header(name, header_value(value)?);
    }
    builder
        .body(())
        .map_err(|e| Error::Other(format!("cannot build HTTP/3 request: {e}")))
}

fn header_value(value: &str) -> Result<HeaderValue> {
    HeaderValue::from_str(value).map_err(|e| Error::Other(format!("bad header value: {e}")))
}

/// Resolve `host:port`, preferring IPv4 (many home networks have broken IPv6
/// egress) but keeping IPv6 as a fallback.
async fn resolve(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let mut addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| Error::Dns(format!("{host}: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(Error::Dns(host.to_string()));
    }
    addrs.sort_by_key(|a| if a.is_ipv4() { 0 } else { 1 });
    Ok(addrs)
}

fn origin_key(host: &str, port: u16) -> String {
    format!("{}:{port}", host.to_ascii_lowercase())
}

/// Shared, thread-safe handle to a pool (the engine keeps one).
pub type SharedH3Pool = Arc<H3Pool>;

#[cfg(test)]
mod tests {
    use super::*;

    fn idle_client_config() -> quinn::ClientConfig {
        tls::install_default_provider();
        let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .expect("ring supports TLS 1.3")
        .with_root_certificates(rustls::RootCertStore::empty())
        .with_no_client_auth();
        config.alpn_protocols = vec![b"h3".to_vec()];
        config.enable_early_data = true;
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(config)
            .expect("empty-root config is still a QUIC client config");
        quinn::ClientConfig::new(Arc::new(crypto))
    }

    #[tokio::test]
    async fn gauge_is_unknown_while_the_pool_lock_is_held() {
        let pool =
            H3Pool::with_client_config(Duration::from_secs(2), "udm-test", idle_client_config())
                .expect("pool binds a UDP socket");

        let guard = pool.state.lock().await;
        assert!(
            matches!(pool.connection_gauge(), ProtocolGauge::Unknown),
            "a busy lock must not become a zero Known sample"
        );
        assert_eq!(pool.recorded_stable_ids(), H3StableIds::Unknown);
        // The old counter still collapses contention to 0. The gauge must not.
        assert_eq!(pool.pooled_connections(), 0);
        drop(guard);

        match pool.connection_gauge() {
            ProtocolGauge::Known {
                active: 0,
                idle: 0,
                connecting: 0,
                draining: 0,
                pooled: 0,
            } => {}
            other => panic!("a readable empty pool is Known zero, got {other:?}"),
        }
        assert_eq!(pool.recorded_stable_ids(), H3StableIds::Known(Vec::new()));
    }
}

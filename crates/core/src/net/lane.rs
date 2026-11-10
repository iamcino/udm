//! Bounded, memory-only segment-open evidence. These are requests, not connections.
//! S0 observes the existing unpinned route; S1 supplies family/epoch allocation.
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::config::TransportPref;
use crate::store::records::TransportKind;

const CAPACITY: usize = 64;

#[derive(Clone, Debug)]
pub struct OpenObservation {
    pub task: u64,
    pub generation: u64,
    pub segment: usize,
    pub range: Option<(u64, Option<u64>)>,
    pub preference: TransportPref,
    pub outcome: OpenOutcome,
    pub failure: Option<&'static str>,
    pub open_to_headers: Option<Duration>,
    /// No peer acquisition is inferred from the requested URL or protocol.
    pub observed_family: Option<&'static str>,
    pub protocol: Option<TransportKind>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOutcome {
    Pending,
    Headers,
    Failed,
    Canceled,
}

#[derive(Debug, Default)]
pub struct OpenEvidence {
    pub observations: Vec<OpenObservation>,
    pub pending: usize,
    pub dropped: u64,
}

#[derive(Debug, Default)]
struct State {
    pending: usize,
    dropped: u64,
    completed: VecDeque<OpenObservation>,
}

/// Owned by one task. Completed attempts are bounded; overflow invalidates proof.
#[derive(Debug, Default)]
pub struct LaneTable(Mutex<State>);

impl LaneTable {
    pub fn snapshot(&self) -> OpenEvidence {
        let state = self.0.lock();
        OpenEvidence {
            observations: state.completed.iter().cloned().collect(),
            pending: state.pending,
            dropped: state.dropped,
        }
    }

    pub(crate) fn begin(self: &Arc<Self>, row: OpenObservation) -> OpenAttempt {
        self.0.lock().pending += 1;
        OpenAttempt {
            table: self.clone(),
            row,
            start: Instant::now(),
        }
    }
}

pub(crate) struct OpenAttempt {
    table: Arc<LaneTable>,
    row: OpenObservation,
    start: Instant,
}

impl OpenAttempt {
    pub(crate) fn headers(&mut self, protocol: TransportKind, peer: Option<std::net::SocketAddr>) {
        self.row.outcome = OpenOutcome::Headers;
        self.row.protocol = Some(protocol);
        self.row.observed_family = peer.and_then(|p| match p.ip() {
            std::net::IpAddr::V4(_) => Some("v4"),
            std::net::IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => Some("v6"),
            _ => None,
        });
        self.row.open_to_headers = Some(self.start.elapsed());
    }

    pub(crate) fn failed(&mut self, error: &crate::error::Error) {
        self.row.outcome = OpenOutcome::Failed;
        use crate::error::Error;
        self.row.failure = Some(match error {
            Error::Tls(_) => "tls",
            Error::Dns(_) => "dns",
            Error::Connect(_) => "connect",
            Error::Timeout { .. } | Error::Stall { .. } => "timeout",
            Error::Cancelled => "canceled",
            Error::Http3(_) => "h3",
            Error::HttpStatus { .. } => "http-status",
            _ => "other",
        });
    }
}

impl Drop for OpenAttempt {
    fn drop(&mut self) {
        if self.row.outcome == OpenOutcome::Pending {
            self.row.outcome = OpenOutcome::Canceled;
            self.row.failure = Some("canceled");
        }
        let mut state = self.table.0.lock();
        state.pending -= 1;
        if state.completed.len() == CAPACITY {
            state.completed.pop_front();
            state.dropped += 1;
        }
        state.completed.push_back(self.row.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn row(segment: usize) -> OpenObservation {
        OpenObservation {
            task: 1,
            generation: 3,
            segment,
            range: Some((0, Some(7))),
            preference: TransportPref::Auto,
            outcome: OpenOutcome::Pending,
            failure: None,
            open_to_headers: None,
            observed_family: None,
            protocol: None,
        }
    }
    #[test]
    fn canceled_failed_and_successful_opens_are_bounded_and_never_connections() {
        let table = Arc::new(LaneTable::default());
        let attempt = table.begin(row(0));
        assert_eq!(table.snapshot().pending, 1);
        drop(attempt);
        assert_eq!(
            table.snapshot().observations[0].outcome,
            OpenOutcome::Canceled
        );
        let mut attempt = table.begin(row(1));
        attempt.failed(&crate::error::Error::Cancelled);
        drop(attempt);
        for segment in 2..67 {
            let mut attempt = table.begin(row(segment));
            attempt.headers(TransportKind::Http2, None);
            drop(attempt);
        }
        let evidence = table.snapshot();
        assert_eq!(evidence.pending, 0);
        assert_eq!(evidence.observations.len(), 64);
        assert_eq!(evidence.dropped, 3);
        assert!(evidence
            .observations
            .iter()
            .all(|r| r.observed_family.is_none() && r.generation == 3));
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use crate::net::stack::RequestSpec;
    use crate::net::{MemAltSvc, NetStack};

    #[tokio::test]
    async fn netstack_exit_records_invalid_url_and_dropped_pending_open() {
        let settings = crate::Settings {
            http3_enabled: false,
            use_system_proxy: false,
            ..crate::Settings::default()
        };
        let stack = NetStack::new(&settings, Arc::new(MemAltSvc::new())).unwrap();
        let table = Arc::new(LaneTable::default());
        let invalid = RequestSpec::get("not a URL");
        assert!(stack
            .open_observed(&invalid, TransportPref::NoHttp3, &table, 1, 4, 0)
            .await
            .is_err());
        assert_eq!(
            table.snapshot().observations[0].outcome,
            OpenOutcome::Failed
        );
        // A listening socket which never serves headers keeps the real open
        // future pending. Dropping it exercises the cancellation RAII path.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let pending = RequestSpec::get(&format!(
            "http://{}/pending",
            listener.local_addr().unwrap()
        ));
        assert!(tokio::time::timeout(
            Duration::from_millis(50),
            stack.open_observed(&pending, TransportPref::NoHttp3, &table, 1, 4, 1)
        )
        .await
        .is_err());
        let evidence = table.snapshot();
        assert_eq!(evidence.pending, 0);
        assert_eq!(evidence.observations.len(), 2);
        assert_eq!(evidence.observations[1].outcome, OpenOutcome::Canceled);
        assert_eq!(evidence.observations[1].failure, Some("canceled"));
        assert_eq!(evidence.observations[1].segment, 1);
        assert_eq!(evidence.observations[1].generation, 4);
        assert!(stack.engine_connections().active().is_none());
    }
}

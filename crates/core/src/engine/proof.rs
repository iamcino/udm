//! Explicit, opt-in proof barriers. Empty slots preserve the normal worker path.
use std::sync::{Arc, Condvar, Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProofPoint {
    BeforeWrite,
    AfterWriteBeforeSync,
    AfterSyncBeforeTransaction,
    TransactionCommittedBeforeAcknowledgment,
    AfterBodyBeforeFlush,
    BeforeReclaim,
}

#[derive(Debug)]
pub struct ProofBarrier {
    pub point: ProofPoint,
    pub(crate) segment: Option<usize>,
    state: Mutex<(bool, bool, u64)>,
    changed: Condvar,
    release: tokio::sync::watch::Sender<bool>,
}
impl ProofBarrier {
    pub fn new(point: ProofPoint) -> Arc<Self> {
        Arc::new(Self {
            point,
            segment: None,
            state: Mutex::new((false, false, 0)),
            changed: Condvar::new(),
            release: tokio::sync::watch::channel(false).0,
        })
    }
    pub fn for_segment(point: ProofPoint, segment: usize) -> Arc<Self> {
        let mut gate = Self::new(point);
        if let Some(inner) = Arc::get_mut(&mut gate) {
            inner.segment = Some(segment);
        }
        gate
    }
    pub fn entered(&self) -> bool {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).0
    }
    pub fn bytes_at_barrier(&self) -> u64 {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).2
    }
    pub fn release(&self) {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).1 = true;
        self.release.send_replace(true);
        self.changed.notify_all();
    }
    fn enter(&self, bytes: u64) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.0 = true;
        state.2 = bytes;
    }
    pub(crate) async fn wait_async(&self, bytes: u64) {
        self.enter(bytes);
        let mut release = self.release.subscribe();
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while !*release.borrow_and_update() {
                if release.changed().await.is_err() {
                    break;
                }
            }
        })
        .await
        .expect("proof barrier release timed out");
    }
    pub(crate) fn wait_sync(&self, bytes: u64) {
        self.enter(bytes);
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let (_released, timeout) = self
            .changed
            .wait_timeout_while(state, std::time::Duration::from_secs(30), |state| !state.1)
            .unwrap_or_else(|e| e.into_inner());
        assert!(!timeout.timed_out(), "proof barrier release timed out");
    }
}

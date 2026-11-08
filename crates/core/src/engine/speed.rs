//! Speed / ETA estimation from a rolling window of byte counters.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Samples `(instant, cumulative bytes)` and reports a smoothed rate.
#[derive(Clone, Debug)]
pub struct SpeedMeter {
    window: Duration,
    samples: VecDeque<(Instant, u64)>,
    smoothed: f64,
}

impl Default for SpeedMeter {
    fn default() -> Self {
        SpeedMeter::new(Duration::from_secs(6))
    }
}

impl SpeedMeter {
    pub fn new(window: Duration) -> Self {
        SpeedMeter {
            window,
            samples: VecDeque::new(),
            smoothed: 0.0,
        }
    }

    /// Record the cumulative byte count for a task.
    pub fn sample(&mut self, total_bytes: u64) {
        let now = Instant::now();
        if let Some((_, last)) = self.samples.back() {
            // A counter that went backwards means the task restarted.
            if *last > total_bytes {
                self.reset();
            }
        }
        self.samples.push_back((now, total_bytes));
        let cutoff = now - self.window;
        while let Some((t, _)) = self.samples.front() {
            if *t < cutoff && self.samples.len() > 2 {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        if let Some(instant) = self.instant_rate() {
            // Exponential smoothing keeps the UI readable without hiding stalls.
            self.smoothed = if self.smoothed <= 0.0 {
                instant
            } else {
                self.smoothed * 0.6 + instant * 0.4
            };
        }
    }

    fn instant_rate(&self) -> Option<f64> {
        let (t0, b0) = *self.samples.front()?;
        let (t1, b1) = *self.samples.back()?;
        let dt = t1.saturating_duration_since(t0).as_secs_f64();
        if dt < 0.05 {
            return None;
        }
        let db = b1.saturating_sub(b0) as f64;
        Some(db / dt)
    }

    /// Bytes per second (0.0 when idle).
    pub fn bps(&self) -> f64 {
        if self.smoothed.is_finite() && self.smoothed > 0.5 {
            self.smoothed
        } else {
            0.0
        }
    }

    /// Seconds until `remaining` bytes are done at the current rate.
    pub fn eta_seconds(&self, remaining: u64) -> f64 {
        let rate = self.bps();
        if rate < 1.0 {
            f64::INFINITY
        } else {
            remaining as f64 / rate
        }
    }

    pub fn reset(&mut self) {
        self.samples.clear();
        self.smoothed = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn reports_zero_when_idle() {
        let mut m = SpeedMeter::new(Duration::from_secs(2));
        m.sample(100);
        assert_eq!(m.bps(), 0.0);
        assert!(m.eta_seconds(100).is_infinite());
    }

    #[test]
    fn measures_a_rate() {
        let mut m = SpeedMeter::new(Duration::from_secs(5));
        m.sample(0);
        sleep(Duration::from_millis(120));
        m.sample(120_000);
        let bps = m.bps();
        // ~1 MB/s; allow a wide margin for a busy CI box.
        assert!(bps > 200_000.0 && bps < 20_000_000.0, "bps={bps}");
        assert!(m.eta_seconds(120_000) < 10.0);
    }

    #[test]
    fn a_restarted_counter_resets_the_meter() {
        let mut m = SpeedMeter::new(Duration::from_secs(5));
        m.sample(1_000_000);
        sleep(Duration::from_millis(60));
        m.sample(0); // task restarted from scratch
        assert_eq!(m.samples.len(), 1);
    }
}

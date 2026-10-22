//! Monotonic time for the audio path, injected so the reader's deadlines (start ramp, re-arm, the 12 ms
//! freshness window) can be driven at a known rate; with a wall clock those tests measure the harness.

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Instant;

pub trait Clock: Send + Sync {
    /// Monotonic nanoseconds from an arbitrary per-process epoch.
    fn now_ns(&self) -> u64;
}

pub fn ms(value: f64) -> u64 {
    (value * 1_000_000.0) as u64
}

fn epoch() -> &'static Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now)
}

#[derive(Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ns(&self) -> u64 {
        epoch().elapsed().as_nanos() as u64
    }
}

/// A clock the test controls: `advance` moves time without any thread waiting, so a five-minute drift run
/// becomes a loop of a few thousand simulated periods.
#[cfg(test)]
pub struct ManualClock {
    now: Arc<AtomicU64>,
}

#[cfg(test)]
impl ManualClock {
    pub fn new() -> Self {
        Self {
            now: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn advance(&self, ns: u64) {
        self.now.fetch_add(ns, Ordering::Relaxed);
    }

    pub fn set(&self, ns: u64) {
        self.now.store(ns, Ordering::Relaxed);
    }
}

#[cfg(test)]
impl Clock for ManualClock {
    fn now_ns(&self) -> u64 {
        self.now.load(Ordering::Relaxed)
    }
}

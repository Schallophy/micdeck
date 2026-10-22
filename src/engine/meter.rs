//! Peak meters with the shape the UI was built around: instant attack, fast fall, and a hold that drops
//! only after it has been visible.

use std::sync::{Arc, Mutex};

use super::clock::Clock;

pub struct MeterState {
    last_ns: u64,
    hold_until_ns: u64,
    level: f32,
    hold: f32,
}

impl Default for MeterState {
    fn default() -> Self {
        Self::new()
    }
}

impl MeterState {
    pub fn new() -> Self {
        Self {
            last_ns: 0,
            hold_until_ns: 0,
            level: 0.0,
            hold: 0.0,
        }
    }

    /// Instant attack, -60 dB/s fall. Peak hold stays 1.2 s, then falls at -20 dB/s.
    pub fn update(&mut self, sample: f32, now_ns: u64) {
        let dt = if self.last_ns == 0 {
            0.0
        } else {
            now_ns.saturating_sub(self.last_ns) as f64 / 1e9
        };
        self.last_ns = now_ns;
        self.level = sample.max(self.level * (10f64.powf(-3.0 * dt)) as f32);
        if sample >= self.hold {
            self.hold = sample;
            self.hold_until_ns = now_ns + 1_200_000_000;
        } else if now_ns > self.hold_until_ns {
            self.hold = (self.hold as f64 * 10f64.powf(-dt)) as f32;
        }
        if self.hold < self.level {
            self.hold = self.level;
        }
    }

    pub fn level(&self) -> f32 {
        self.level
    }

    pub fn hold(&self) -> f32 {
        self.hold
    }
}

/// A meter that can be handed to a capture callback and read from the status snapshot at the same time.
pub struct LiveMeter {
    state: Mutex<MeterState>,
    clock: Arc<dyn Clock>,
}

impl LiveMeter {
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MeterState::new()),
            clock,
        })
    }

    pub fn update(&self, sample: f32) {
        let now = self.clock.now_ns();
        self.state.lock().unwrap().update(sample, now);
    }

    pub fn level(&self) -> f32 {
        self.state.lock().unwrap().level()
    }

    pub fn hold(&self) -> f32 {
        self.state.lock().unwrap().hold()
    }
}

/// Loudest sample of an arrival, on the channel actually present: interleaved float, both channels feed
/// one meter so a mono source in a stereo stream cannot read quieter than it is.
pub fn peak_of(frames: &[f32]) -> f32 {
    let mut peak = 0.0f32;
    for sample in frames {
        let absolute = sample.abs();
        if absolute > peak {
            peak = absolute;
        }
    }
    peak
}

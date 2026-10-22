//! One output line's view of a [`SourceRing`]: its own cursor, level controller and glitch accounting.
//! Two endpoints on one source are two readers, each on a different hardware clock.
//!
//! Adaptive resampling in the PulseAudio `module-loopback` sense: a slow controller on the smoothed fill
//! level varies the read ratio continuously, so nothing is dropped, duplicated or slipped.

use std::sync::Arc;

use super::ring::{Inner, SourceRing};

#[derive(Clone, Copy, Debug, Default)]
pub struct BufferStats {
    pub buffered_ms: f64,
    pub starved_frames: u64,
    pub idle_frames: u64,
    pub dropped_frames: u64,
    /// Current read-ratio deviation of the adaptive resampler; 0 means reading 1:1.
    pub ratio_ppm: i32,
}

/// Fill level to hold. 16 ms leaves ~8.3 ms of stall tolerance once half a burst and one read block are
/// subtracted; 12 ms leaves ~4.3 ms.
const TARGET_MS: usize = 16;
/// ~130 ms at 375 blocks/s: kills the arrival sawtooth, still tracks real drift.
const LEVEL_SMOOTHING: f64 = 0.02;
const RATIO_KP: f64 = 1e-3;
const RATIO_KI: f64 = 1e-6;
/// ±2000 ppm, about 100x the drift it should ever cancel.
const MAX_RATIO_DEV: f64 = 2e-3;
/// Splice jumps below this are inaudible, so the gap counts as idle. A 3.3-6.0 ms gap at 0.05 full scale
/// measured 0.0065, so 0.02 reports only when the signal was louder than about -16 dBFS.
const STEP_THRESHOLD: f32 = 0.02;
/// 5 ms ramps at both ends of a gap: ~44 dB down at 10 kHz, far short of the ~20 ms that reads as a dip.
const FADE_FRAMES: usize = 240;
/// A new reader runs dry for its first packets; bounded by time, so a chronically starved pipeline still
/// reports its misses.
const RAMP_NS: u64 = 50_000_000;
/// A dry read within this window after an arrival is a missed deadline. 1.2 packets at 10 ms.
const STARVE_WINDOW_NS: u64 = 12_000_000;
/// A paused app stops sending; on resume, prebuffer again instead of counting the rebuild as misses.
const RE_ARM_NS: u64 = 50_000_000;

pub struct SourceReader {
    ring: Arc<SourceRing>,
    channels: usize,
    capacity_frames: usize,
    sample_rate: usize,
    target_frames: usize,
    level_control: bool,

    pos: u64,
    /// Fractional read position inside the current frame.
    frac: f64,
    ratio: f64,
    /// Integral term: accumulates the ppm needed to cancel clock drift.
    ratio_bias: f64,
    level_avg: f64,
    primed_ready: bool,
    started_ns: u64,
    last_read_ns: u64,

    gap_tail: Vec<f32>,
    gap_open: bool,
    gap_padded_frames: u64,
    /// Frames of real audio that still have to be ramped up after the reader started consuming.
    fade_in_left: usize,

    /// Frames this reader skipped forward because the ring recycled over it.
    dropped_frames: u64,
    /// Frames this line padded although a packet arrived moments ago: this endpoint missed its deadline.
    /// The only one of the two padding counters that means "glitch".
    starved_frames: u64,
    /// Frames padded because nothing had arrived — the source is silent, paused, or not started.
    idle_frames: u64,
}

impl SourceReader {
    pub(crate) fn new(
        ring: Arc<SourceRing>,
        level_control: bool,
        start_frame: u64,
    ) -> Self {
        let channels = ring.channels;
        Self {
            target_frames: ring.sample_rate * TARGET_MS / 1000,
            capacity_frames: ring.capacity_frames,
            sample_rate: ring.sample_rate,
            channels,
            ring,
            level_control,
            pos: start_frame,
            frac: 0.0,
            ratio: 1.0,
            ratio_bias: 0.0,
            level_avg: 0.0,
            primed_ready: false,
            started_ns: 0,
            last_read_ns: 0,
            gap_tail: vec![0.0f32; channels],
            gap_open: false,
            gap_padded_frames: 0,
            fade_in_left: 0,
            dropped_frames: 0,
            starved_frames: 0,
            idle_frames: 0,
        }
    }

    pub fn ring(&self) -> &Arc<SourceRing> {
        &self.ring
    }

    /// How far the read ratio currently deviates from 1:1.
    pub fn ratio_ppm(&self) -> i32 {
        ((self.ratio - 1.0) * 1e6).round() as i32
    }

    pub fn buffered_frames(&self) -> usize {
        let inner = self.ring.inner.lock().unwrap();
        Self::clamped(&inner, self.pos, self.capacity_frames)
    }

    pub fn buffered_ms(&self) -> f64 {
        self.buffered_frames() as f64 * 1000.0 / self.sample_rate as f64
    }

    fn clamped(inner: &Inner, pos: u64, capacity: usize) -> usize {
        inner.write_pos.saturating_sub(pos).min(capacity as u64) as usize
    }

    pub fn stats(&self) -> BufferStats {
        BufferStats {
            buffered_ms: self.buffered_frames() as f64 * 1000.0 / self.sample_rate as f64,
            starved_frames: self.starved_frames,
            idle_frames: self.idle_frames,
            dropped_frames: self.dropped_frames + self.ring.dropped_frames(),
            ratio_ppm: self.ratio_ppm(),
        }
    }

    /// Catmull-Rom: exact at integer positions, smooth in between, four taps.
    fn cubic(x0: f32, x1: f32, x2: f32, x3: f32, d: f64) -> f32 {
        let (x0, x1, x2, x3) = (x0 as f64, x1 as f64, x2 as f64, x3 as f64);
        let a = -0.5 * x0 + 1.5 * x1 - 1.5 * x2 + 0.5 * x3;
        let b = x0 - 2.5 * x1 + 2.0 * x2 - 0.5 * x3;
        let c = -0.5 * x0 + 0.5 * x2;
        (((a * d + b) * d + c) * d + x1) as f32
    }

    /// Produces `frames` frames into `destination` (interleaved, length at least `frames * channels`).
    /// Reads the ring and advances this cursor only; other lines on the same ring are untouched.
    pub fn read(&mut self, destination: &mut [f32], frames: usize) {
        let need = frames * self.channels;
        debug_assert!(
            destination.len() >= need,
            "block shorter than frames * channels"
        );
        let now = self.ring.clock.now_ns();
        let inner = self.ring.inner.lock().unwrap();

        if self.started_ns == 0 {
            self.started_ns = now;
        }

        // The ring recycles over a reader that falls behind: skip forward to the oldest frame that still
        // exists and own the loss, instead of letting one line drag the others.
        let write_pos = inner.write_pos;
        let mut buffered_before = Self::clamped(&inner, self.pos, self.capacity_frames);
        let behind = write_pos
            .saturating_sub(self.pos)
            .saturating_sub(self.capacity_frames as u64);
        if behind > 0 {
            self.pos += behind;
            self.dropped_frames += behind;
            buffered_before = self.capacity_frames;
        }

        if self.primed_ready
            && inner.last_write_ns != 0
            && now.saturating_sub(inner.last_write_ns) > RE_ARM_NS
        {
            if self.gap_open {
                self.idle_frames += self.gap_padded_frames;
                self.gap_open = false;
            }
            self.primed_ready = false;
            self.fade_in_left = 0;
            self.level_avg = 0.0;
        }

        // Prebuffer before consuming. A new reader starts at the write head with an empty window, and
        // synthesising a 16 ms level at the ±2000 ppm clamp would take seconds and starve the whole way.
        if !self.primed_ready && buffered_before < self.target_frames {
            destination[..need].fill(0.0);
            self.level_avg += (buffered_before as f64 - self.level_avg) * LEVEL_SMOOTHING;
            self.last_read_ns = now;
            self.idle_frames += frames as u64;
            return;
        }
        if !self.primed_ready {
            // The first real audio after a prebuffer must arrive on a ramp. The gain slot ramps for ~14 ms
            // and the prebuffer holds ~16 ms, so without this the gain reaches full exactly when the
            // samples start, and routing on a source that is already playing steps the waveform.
            self.fade_in_left = FADE_FRAMES;
        }
        self.primed_ready = true;

        self.level_avg += (buffered_before as f64 - self.level_avg) * LEVEL_SMOOTHING;
        if self.level_control {
            let err = (self.level_avg - self.target_frames as f64) / self.target_frames as f64;
            self.ratio_bias = clamp(self.ratio_bias + RATIO_KI * err, -MAX_RATIO_DEV, MAX_RATIO_DEV);
            self.ratio = clamp(
                1.0 + RATIO_KP * err + self.ratio_bias,
                1.0 - MAX_RATIO_DEV,
                1.0 + MAX_RATIO_DEV,
            );
        } else {
            // Open loop: used to measure the arrival rate without the controller correcting it.
            self.ratio = 1.0;
        }

        // Cubic interpolation reads one frame back and two ahead, so a sample can only be produced while
        // that whole window is inside the ring.
        let mut filled = 0i64;
        if buffered_before >= 4 {
            filled = (((buffered_before - 3) as f64 - self.frac) / self.ratio).floor() as i64 + 1;
            filled = filled.clamp(0, frames as i64);
        }
        let filled = filled as usize;

        for i in 0..filled {
            let p = self.frac + i as f64 * self.ratio;
            let adv = p.floor() as u64;
            let d = p - adv as f64;
            let n1 = self.pos + adv;
            // The frame before the head has already been consumed when we are at the very start.
            let back = if adv == 0 { 0 } else { 1 };
            for channel in 0..self.channels {
                let x0 = SourceRing::sample_at(&inner, self.capacity_frames, self.channels, n1 - back, channel);
                let x1 = SourceRing::sample_at(&inner, self.capacity_frames, self.channels, n1, channel);
                let x2 = SourceRing::sample_at(&inner, self.capacity_frames, self.channels, n1 + 1, channel);
                let x3 = SourceRing::sample_at(&inner, self.capacity_frames, self.channels, n1 + 2, channel);
                destination[i * self.channels + channel] = Self::cubic(x0, x1, x2, x3, d);
            }
        }
        if filled > 0 {
            let total = self.frac + filled as f64 * self.ratio;
            self.pos += total.floor() as u64;
            self.frac = total - total.floor();
        }

        if filled > 0 && self.fade_in_left > 0 {
            // The ramp is counted in frames remaining, so it continues across block boundaries: dividing
            // by what is left would restart it at every block and step the envelope instead of easing it.
            let n = self.fade_in_left.min(filled);
            let already = FADE_FRAMES - self.fade_in_left;
            for k in 0..n {
                let weight = (already + k + 1) as f32 / FADE_FRAMES as f32;
                for channel in 0..self.channels {
                    destination[k * self.channels + channel] *= weight;
                }
            }
            self.fade_in_left -= n;
        }

        if self.gap_open && filled > 0 {
            // Ramp back up out of the silence first: the return splice must not step either.
            let n = FADE_FRAMES.min(filled);
            for k in 0..n {
                let weight = (k + 1) as f32 / n as f32;
                for channel in 0..self.channels {
                    destination[k * self.channels + channel] *= weight;
                }
            }
            // The click is the jump from the last sample before the silence to the first one after it; the
            // silent stretch in between only says how long we had to bridge. The measured splice decides
            // the accounting: only a jump that could be heard counts as a miss.
            let mut step = 0.0f32;
            for channel in 0..self.channels {
                let diff = (destination[channel] - self.gap_tail[channel]).abs();
                if diff > step {
                    step = diff;
                }
            }
            if step >= STEP_THRESHOLD {
                self.starved_frames += self.gap_padded_frames;
            } else {
                self.idle_frames += self.gap_padded_frames;
            }
            self.gap_open = false;
        }

        for i in filled..frames {
            for channel in 0..self.channels {
                destination[i * self.channels + channel] = 0.0;
            }
        }
        self.last_read_ns = now;

        let padded = frames - filled;
        if padded > 0 {
            let past_ramp = now.saturating_sub(self.started_ns) > RAMP_NS;
            let fresh = inner.last_write_ns != 0
                && now.saturating_sub(inner.last_write_ns) < STARVE_WINDOW_NS;
            if past_ramp && fresh {
                if !self.gap_open {
                    self.gap_open = true;
                    self.gap_padded_frames = 0;
                }
                self.gap_padded_frames += padded as u64;
                if filled > 0 {
                    // Ramp the last real samples into the silence so the splice has no step.
                    let n = FADE_FRAMES.min(filled);
                    for k in 0..n {
                        let weight = (n - 1 - k) as f32 / n as f32;
                        let index = (filled - n + k) * self.channels;
                        for channel in 0..self.channels {
                            destination[index + channel] *= weight;
                        }
                    }
                    let last = (filled - 1) * self.channels;
                    for channel in 0..self.channels {
                        self.gap_tail[channel] = destination[last + channel];
                    }
                }
            } else {
                self.idle_frames += padded as u64;
            }
        }

        if need < destination.len() {
            destination[need..].fill(0.0);
        }
    }
}

impl Drop for SourceReader {
    fn drop(&mut self) {
        let mut inner = self.ring.inner.lock().unwrap();
        inner.readers = inner.readers.saturating_sub(1);
    }
}

fn clamp(value: f64, low: f64, high: f64) -> f64 {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

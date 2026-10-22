//! One output line's bus: the sources routed to it, each read through its own cursor. Slots come and go
//! while audio keeps flowing.
//!
//! Lock order is always bus -> ring.

use std::sync::Arc;

use super::clock::Clock;
use super::meter::MeterState;
use super::ring::SourceRing;

pub use super::reader::BufferStats;
use super::reader::SourceReader;

/// 0 -> 1 in ~14 ms: fast enough for a slider edit, slow enough that no edge becomes a step.
const GAIN_SLEW_PER_FRAME: f32 = 0.0015;
/// +12 dB. Above this the user is deliberately driving the limiter.
const MAX_GAIN: f32 = 4.0;

struct Slot {
    id: u32,
    #[allow(dead_code)]
    ring: Arc<SourceRing>,
    reader: SourceReader,
    block: Vec<f32>,
    removed: bool,
    gain: f32,
    current_gain: f32,
}

struct Bus {
    slots: Vec<Slot>,
    tone_phase: f64,
    test_tone_hz: f64,
    frames_consumed: u64,
    clipped_samples: u64,
    /// Blocks the sink had to throw away and hand over as silence. Should stay 0.
    backstop_errors: u64,
    meter: MeterState,
}

pub struct Mixer {
    bus: std::sync::Mutex<Bus>,
    name: String,
    channels: usize,
    sample_rate: usize,
    clock: Arc<dyn Clock>,
}

impl Mixer {
    pub fn new(sample_rate: usize, channels: usize, name: &str, clock: Arc<dyn Clock>) -> Self {
        Self {
            bus: std::sync::Mutex::new(Bus {
                slots: Vec::new(),
                tone_phase: 0.0,
                test_tone_hz: 0.0,
                frames_consumed: 0,
                clipped_samples: 0,
                backstop_errors: 0,
                meter: MeterState::new(),
            }),
            name: name.to_string(),
            channels,
            sample_rate,
            clock,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Routes a shared source onto this line. Re-attaching the same id onto a new ring swaps the reader
    /// without disturbing the gain, which is what a reconnect after a process restart looks like.
    pub fn attach_source(&self, ring: Arc<SourceRing>, id: u32, gain: f32) {
        let gain = gain.clamp(0.0, MAX_GAIN);
        let mut bus = self.bus.lock().unwrap();
        if let Some(existing) = bus.slots.iter_mut().find(|slot| slot.id == id) {
            if !Arc::ptr_eq(&existing.ring, &ring) {
                existing.reader = ring.create_reader(true);
                existing.ring = ring;
                existing.block = Vec::new();
            }
            existing.gain = gain;
            existing.removed = false;
            return;
        }
        let reader = ring.create_reader(true);
        bus.slots.push(Slot {
            id,
            ring,
            reader,
            block: Vec::new(),
            removed: false,
            gain,
            current_gain: 0.0,
        });
    }

    pub fn set_gain(&self, id: u32, gain: f32) {
        let gain = gain.clamp(0.0, MAX_GAIN);
        let mut bus = self.bus.lock().unwrap();
        if let Some(slot) = bus.slots.iter_mut().find(|slot| slot.id == id) {
            slot.gain = gain;
        }
    }

    /// Keeps the slot until `mix` has faded it to zero: dropping it here would step the output.
    pub fn detach_source(&self, id: u32) {
        let mut bus = self.bus.lock().unwrap();
        if let Some(slot) = bus.slots.iter_mut().find(|slot| slot.id == id) {
            slot.removed = true;
        }
    }

    /// Drops every slot at once. Used when the line itself is going away, so there is no waveform left to
    /// protect.
    pub fn clear_sources(&self) {
        let mut bus = self.bus.lock().unwrap();
        bus.slots.clear();
    }

    pub fn has_source(&self, id: u32) -> bool {
        self.bus
            .lock()
            .unwrap()
            .slots
            .iter()
            .any(|slot| slot.id == id && !slot.removed)
    }

    pub fn routed_ids(&self) -> Vec<u32> {
        self.bus
            .lock()
            .unwrap()
            .slots
            .iter()
            .filter(|slot| !slot.removed)
            .map(|slot| slot.id)
            .collect()
    }

    pub fn stats(&self, id: u32) -> Option<BufferStats> {
        let bus = self.bus.lock().unwrap();
        bus.slots
            .iter()
            .find(|slot| slot.id == id)
            .map(|slot| slot.reader.stats())
    }

    pub fn set_test_tone(&self, hz: f64) {
        self.bus.lock().unwrap().test_tone_hz = hz;
    }

    pub fn note_backstop(&self) {
        self.bus.lock().unwrap().backstop_errors += 1;
    }

    pub fn mix_peak(&self) -> f32 {
        self.bus.lock().unwrap().meter.level()
    }

    pub fn mix_hold(&self) -> f32 {
        self.bus.lock().unwrap().meter.hold()
    }

    pub fn clipped_samples(&self) -> u64 {
        self.bus.lock().unwrap().clipped_samples
    }

    pub fn backstop_errors(&self) -> u64 {
        self.bus.lock().unwrap().backstop_errors
    }

    pub fn frames_consumed(&self) -> u64 {
        self.bus.lock().unwrap().frames_consumed
    }

    /// Mixes one block into `destination` (interleaved float, length a multiple of the channel count).
    pub fn mix(&self, destination: &mut [f32]) {
        let frames = destination.len() / self.channels;
        let now = self.clock.now_ns();
        let mut bus = self.bus.lock().unwrap();

        for slot in bus.slots.iter_mut() {
            if slot.block.len() < destination.len() {
                slot.block.resize(destination.len(), 0.0);
            }
        }

        destination.fill(0.0);
        let mut any_removed = false;
        for slot in bus.slots.iter_mut() {
            slot.reader
                .read(&mut slot.block[..destination.len()], frames);
            let target = if slot.removed { 0.0 } else { slot.gain };
            let mut cur = slot.current_gain;
            if (target - cur).abs() <= GAIN_SLEW_PER_FRAME {
                cur = target;
                for (destination, &sample) in destination.iter_mut().zip(slot.block.iter()) {
                    *destination += sample * cur;
                }
            } else {
                let direction = if target > cur { 1.0 } else { -1.0 };
                for f in 0..frames {
                    cur += direction * GAIN_SLEW_PER_FRAME;
                    if (direction > 0.0 && cur > target) || (direction < 0.0 && cur < target) {
                        cur = target;
                    }
                    let start = f * self.channels;
                    for channel in 0..self.channels {
                        let index = start + channel;
                        destination[index] += slot.block[index] * cur;
                    }
                }
            }
            slot.current_gain = cur;
            if slot.removed && cur <= 0.0 {
                any_removed = true;
            }
        }
        if any_removed {
            bus.slots
                .retain(|slot| !(slot.removed && slot.current_gain <= 0.0));
        }

        bus.frames_consumed += frames as u64;

        if bus.test_tone_hz > 0.0 {
            let phase_step = 2.0 * std::f64::consts::PI * bus.test_tone_hz / self.sample_rate as f64;
            let mut phase = bus.tone_phase;
            for f in 0..frames {
                let tone = (0.05 * phase.sin()) as f32;
                phase += phase_step;
                if phase > 2.0 * std::f64::consts::PI {
                    phase -= 2.0 * std::f64::consts::PI;
                }
                let start = f * self.channels;
                for channel in 0..self.channels {
                    destination[start + channel] += tone;
                }
            }
            bus.tone_phase = phase;
        }

        let mut mix_peak = 0.0f32;
        let mut clipped = 0u64;
        for value in destination.iter_mut() {
            let mut sample = *value;
            if sample > 1.0 {
                sample = 1.0;
                clipped += 1;
            } else if sample < -1.0 {
                sample = -1.0;
                clipped += 1;
            }
            *value = sample;
            let absolute = sample.abs();
            if absolute > mix_peak {
                mix_peak = absolute;
            }
        }
        bus.clipped_samples += clipped;
        bus.meter.update(mix_peak, now);
    }
}

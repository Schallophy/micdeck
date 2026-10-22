//! Fixed-capacity float ring: one capture writes, every line reads through its own cursor. Producers never
//! wait: the ring recycles over a slow reader, and that line accounts the skip as its own drop.

use std::sync::{Arc, Mutex};

use super::clock::Clock;
use super::reader::SourceReader;

/// Half a second at 48 kHz: enough to survive a scheduling stall without hiding one.
pub const RING_CAPACITY_FRAMES: usize = 24_000;

pub(crate) struct Inner {
    /// Interleaved, `capacity_frames * channels` long.
    pub samples: Vec<f32>,
    pub write_pos: u64,
    /// When the last arrival was copied in, in the ring's clock. Freshness judgements depend on this, and
    /// a source that is merely silent still arrives — that is what separates "the app stopped" from "the
    /// app went quiet".
    pub last_write_ns: u64,
    pub readers: u32,
    pub dropped_frames: u64,
}

pub struct SourceRing {
    pub channels: usize,
    pub sample_rate: usize,
    pub capacity_frames: usize,
    pub(crate) inner: Mutex<Inner>,
    pub(crate) clock: Arc<dyn Clock>,
}

impl SourceRing {
    pub fn new(
        channels: usize,
        sample_rate: usize,
        capacity_frames: usize,
        clock: Arc<dyn Clock>,
    ) -> Arc<Self> {
        Arc::new(Self {
            channels,
            sample_rate,
            capacity_frames,
            inner: Mutex::new(Inner {
                samples: vec![0.0f32; capacity_frames * channels],
                write_pos: 0,
                last_write_ns: 0,
                readers: 0,
                dropped_frames: 0,
            }),
            clock,
        })
    }

    /// Called from the capture thread with one arrival's worth of interleaved samples.
    pub fn write(&self, interleaved: &[f32]) {
        let frames = interleaved.len() / self.channels;
        if frames == 0 {
            return;
        }
        let now = self.clock.now_ns();
        let mut inner = self.inner.lock().unwrap();
        inner.last_write_ns = now;
        if inner.readers == 0 {
            // Nobody is going to read this: record the arrival so freshness still works, skip the copy.
            return;
        }
        // Keep the newest frames when a single arrival is larger than the whole ring.
        let writable = frames.min(self.capacity_frames);
        let skip = (frames - writable) * self.channels;
        for i in 0..writable {
            let dst = ((inner.write_pos as usize + i) % self.capacity_frames) * self.channels;
            let src = skip + i * self.channels;
            for channel in 0..self.channels {
                inner.samples[dst + channel] = interleaved[src + channel];
            }
        }
        inner.write_pos += writable as u64;
        inner.dropped_frames += (frames - writable) as u64;
    }

    /// A reader starts at the write head: frames older than this are already recyclable.
    pub fn create_reader(self: &Arc<Self>, level_control: bool) -> SourceReader {
        let start = {
            let mut inner = self.inner.lock().unwrap();
            inner.readers += 1;
            inner.write_pos
        };
        SourceReader::new(self.clone(), level_control, start)
    }

    pub fn reader_count(&self) -> u32 {
        self.inner.lock().unwrap().readers
    }

    pub fn dropped_frames(&self) -> u64 {
        self.inner.lock().unwrap().dropped_frames
    }

    pub(crate) fn sample_at(
        inner: &Inner,
        capacity: usize,
        channels: usize,
        frame: u64,
        channel: usize,
    ) -> f32 {
        inner.samples[(frame as usize % capacity) * channels + channel]
    }
}

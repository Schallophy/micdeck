//! Offline proofs for drift and gap control, on an injected clock.
//!
//! Two measures, not one: the *splice jump* across padded silence (the reader's own 0.02 threshold), and
//! the *sample jump*, valid only on a near-DC tone — a 1 kHz signal slopes 0.065 per sample, three times
//! the threshold.

use std::sync::Arc;

use super::clock::{Clock, ManualClock};
use super::mixer::Mixer;
use super::reader::{BufferStats, SourceReader};
use super::ring::{SourceRing, RING_CAPACITY_FRAMES};

const RATE: usize = 48_000;
const CH: usize = 2;
/// One 10 ms capture packet.
const PACKET_FRAMES: usize = 480;
/// One read block at the engine period WASAPI grants on VB-CABLE (128 frames = 2.67 ms).
const BLOCK_FRAMES: usize = 128;
/// The reader's own audibility threshold.
const MAX_JUMP: f32 = 0.02;
/// Loud 1 kHz: a real gap shows a large splice jump.
const TONE_HZ: f64 = 1000.0;
/// Near-DC: the signal's own slope is negligible, so a step can only be an edit.
const SLOW_HZ: f64 = 10.0;

fn clock_and_ring() -> (Arc<ManualClock>, Arc<SourceRing>) {
    let clock = Arc::new(ManualClock::new());
    let ring = SourceRing::new(CH, RATE, RING_CAPACITY_FRAMES, clock.clone() as Arc<dyn Clock>);
    (clock, ring)
}

fn ring_of(clock: &Arc<ManualClock>) -> Arc<SourceRing> {
    SourceRing::new(CH, RATE, RING_CAPACITY_FRAMES, clock.clone() as Arc<dyn Clock>)
}

fn packet(phase: &mut f64, frames: usize, amplitude: f32, hz: f64) -> Vec<f32> {
    let step = 2.0 * std::f64::consts::PI * hz / RATE as f64;
    let mut out = Vec::with_capacity(frames * CH);
    for _ in 0..frames {
        let value = (phase.sin() * f64::from(amplitude)) as f32;
        *phase += step;
        out.push(value);
        out.push(value);
    }
    out
}

/// Virtual time for the given number of frames consumed at the nominal rate. Derived from frame counts
/// rather than accumulated nanoseconds, so the consumer's rate is exact to within 1 ns per block.
fn virtual_time(consumed_frames: u64) -> u64 {
    (consumed_frames as f64 * 1e9 / RATE as f64).round() as u64
}

fn concatenate(blocks: &[Vec<f32>]) -> Vec<f32> {
    let mut out = Vec::new();
    for block in blocks {
        out.extend_from_slice(block);
    }
    out
}

/// Biggest last-real-sample to first-real-sample difference across every run of padded silence.
fn splice_jump(blocks: &[Vec<f32>]) -> f32 {
    let stream = concatenate(blocks);
    let mut worst = 0.0f32;
    let mut index = 0;
    while index < stream.len() {
        if stream[index].abs() < 1e-9 {
            let start = index;
            while index < stream.len() && stream[index].abs() < 1e-9 {
                index += 1;
            }
            if start > 0 && index < stream.len() {
                worst = worst.max((stream[index] - stream[start - 1]).abs());
            }
        } else {
            index += 1;
        }
    }
    worst
}

/// Biggest sample-to-sample difference anywhere in the block sequence, ignoring an initial stretch.
fn sample_jump(blocks: &[Vec<f32>], from_block: usize) -> f32 {
    let mut worst = 0.0f32;
    let mut previous: Option<f32> = None;
    for (index, block) in blocks.iter().enumerate() {
        if index < from_block {
            previous = None;
            continue;
        }
        for &sample in block {
            if let Some(last) = previous {
                worst = worst.max((sample - last).abs());
            }
            previous = Some(sample);
        }
    }
    worst
}

struct Run {
    starved: u64,
    dropped: u64,
    min_buffered_ms: f64,
    max_buffered_ms: f64,
    ppm_min: i32,
    ppm_max: i32,
    ppm_last: i32,
}

/// Feeds one packet every `arrival_ns` and reads `BLOCK_FRAMES` per step until `seconds` of virtual time
/// has passed. An `arrival_ns` above 10 ms is a source that runs slow; below, fast.
fn drive(
    reader: &mut SourceReader,
    clock: &ManualClock,
    ring: &Arc<SourceRing>,
    arrival_ns: u64,
    seconds: f64,
) -> Run {
    let mut phase = 0.0f64;
    let mut consumed: u64 = 0;
    let mut next_arrival = 0u64;
    let mut block = vec![0.0f32; BLOCK_FRAMES * CH];
    let mut min_buffered_ms = f64::MAX;
    let mut max_buffered_ms = 0.0f64;
    let mut ppm: Vec<i32> = Vec::new();
    let end = virtual_time((RATE as f64 * seconds) as u64);

    while virtual_time(consumed) < end {
        let now = virtual_time(consumed);
        clock.set(now);
        while next_arrival <= now {
            ring.write(&packet(&mut phase, PACKET_FRAMES, 0.4, TONE_HZ));
            next_arrival += arrival_ns;
        }
        reader.read(&mut block, BLOCK_FRAMES);
        consumed += BLOCK_FRAMES as u64;
        if consumed > RATE as u64 {
            // Past the first second: the controller has established the level, so only steady state counts.
            let buffered = reader.buffered_ms();
            min_buffered_ms = min_buffered_ms.min(buffered);
            max_buffered_ms = max_buffered_ms.max(buffered);
            ppm.push(reader.ratio_ppm());
        }
    }
    let stats = reader.stats();
    let settled = &ppm[ppm.len() / 2..];
    Run {
        starved: stats.starved_frames,
        dropped: stats.dropped_frames,
        min_buffered_ms,
        max_buffered_ms,
        ppm_min: settled.iter().min().copied().unwrap_or(0),
        ppm_max: settled.iter().max().copied().unwrap_or(0),
        ppm_last: settled.last().copied().unwrap_or(0),
    }
}

#[test]
fn catmull_rom_is_exact_at_integer_positions() {
    let (clock, ring) = clock_and_ring();
    // The reader is created first, so it starts at the write head and the packets below are its input.
    let mut reader = ring.create_reader(false);
    let mut source_phase = 0.0f64;
    let mut expected: Vec<f32> = Vec::new();
    for _ in 0..8 {
        let data = packet(&mut source_phase, PACKET_FRAMES, 0.4, TONE_HZ);
        expected.extend_from_slice(&data);
        clock.advance(10_000_000);
        ring.write(&data);
    }
    // Controller off: ratio is exactly 1 and every read position lands on an integer frame, so the cubic
    // must reproduce the ring contents instead of interpolating away from them.
    let mut out = vec![0.0f32; BLOCK_FRAMES * CH];
    // The cubic needs two frames of lookahead, so the last three before the write head cannot be made,
    // and the entry ramp owns the first 240 frames: compare what is between those two edges.
    let ramped = 240 * CH;
    let producible = (expected.len() / CH - 3) * CH;
    let mut read_total = 0usize;
    while read_total < producible {
        clock.advance(1_000_000);
        reader.read(&mut out, BLOCK_FRAMES);
        let compared = (producible - read_total).min(BLOCK_FRAMES * CH);
        for i in 0..compared {
            if read_total + i < ramped {
                continue;
            }
            assert!(
                (out[i] - expected[read_total + i]).abs() < 1e-6,
                "sample {}: got {} expected {}",
                read_total + i,
                out[i],
                expected[read_total + i]
            );
        }
        read_total += BLOCK_FRAMES * CH;
    }
    assert_eq!(reader.ratio_ppm(), 0, "open loop must read exactly 1:1");
}

#[test]
fn a_fresh_reader_prebuffers_and_counts_idle_not_starved() {
    let (clock, ring) = clock_and_ring();
    let mut reader = ring.create_reader(true);
    let mut out = vec![0.0f32; BLOCK_FRAMES * CH];
    reader.read(&mut out, BLOCK_FRAMES);
    assert!(
        out.iter().all(|&s| s == 0.0),
        "no data yet, so the block must be silence"
    );
    let stats = reader.stats();
    assert_eq!(stats.starved_frames, 0);
    assert!(stats.idle_frames > 0);
    assert_eq!(
        reader.buffered_frames(),
        0,
        "prebuffering must not consume anything"
    );
    clock.set(0);
}

#[test]
fn a_matched_clock_holds_the_level_without_padding_anything() {
    let (clock, ring) = clock_and_ring();
    let mut reader = ring.create_reader(true);
    let run = drive(&mut reader, &clock, &ring, 10_000_000, 30.0);
    assert_eq!(run.starved, 0, "no deadline miss is possible on a matched clock");
    assert_eq!(run.dropped, 0, "the ring should never recycle over this reader");
    assert!(
        run.min_buffered_ms > 8.0,
        "water level fell to {:.1} ms",
        run.min_buffered_ms
    );
    assert!(
        run.max_buffered_ms < 24.0,
        "water level rose to {:.1} ms",
        run.max_buffered_ms
    );
    assert!(
        run.ppm_min > -60 && run.ppm_max < 60,
        "the ratio wandered to {}..{} ppm against a clock that is not drifting",
        run.ppm_min,
        run.ppm_max
    );
}

#[test]
fn a_slow_source_is_cancelled_instead_of_draining_the_ring() {
    // 480 frames every 10.001 ms is 100 ppm slow: an uncontrolled reader starves within about 16 s.
    let (clock, ring) = clock_and_ring();
    let mut reader = ring.create_reader(true);
    let run = drive(&mut reader, &clock, &ring, 10_001_000, 60.0);
    assert_eq!(run.starved, 0, "drift must not turn into a gap");
    assert!(
        run.ppm_last < -60 && run.ppm_last > -160,
        "expected the ratio to settle near -100 ppm, measured {}",
        run.ppm_last
    );
    assert!(
        run.min_buffered_ms > 8.0,
        "level drained to {:.1} ms",
        run.min_buffered_ms
    );
}

#[test]
fn a_fast_source_is_cancelled_instead_of_overflowing_the_ring() {
    let (clock, ring) = clock_and_ring();
    let mut reader = ring.create_reader(true);
    let run = drive(&mut reader, &clock, &ring, 9_999_000, 60.0);
    assert_eq!(run.starved, 0);
    assert_eq!(run.dropped, 0, "a fast source must not be allowed to wrap the ring");
    assert!(
        run.ppm_last > 60 && run.ppm_last < 160,
        "expected the ratio to settle near +100 ppm, measured {}",
        run.ppm_last
    );
    assert!(
        run.max_buffered_ms < 24.0,
        "level piled up to {:.1} ms",
        run.max_buffered_ms
    );
}

#[test]
fn a_producer_stall_is_absorbed_by_the_water_level() {
    // The source stops for 12 ms while the reader keeps pulling at the device rate. The level is the
    // buffer against exactly this, so most of the stall must be bridged and whatever padding happens has
    // to be ramped and attributed to the source, not to us.
    let (clock, ring) = clock_and_ring();
    let mut reader = ring.create_reader(true);
    let warm = drive(&mut reader, &clock, &ring, 10_000_000, 3.0);
    let before = reader.stats();

    let mut phase = 0.0f64;
    let base = clock.now_ns();
    let mut consumed: u64 = 0;
    let mut next_arrival = base + 10_000_000;
    let mut block = vec![0.0f32; BLOCK_FRAMES * CH];
    let mut blocks: Vec<Vec<f32>> = Vec::new();
    // The stall sits in the middle of the run, long enough to bite but shorter than the freshness window
    // multiplied by the level, so the ring can bridge most of it.
    let stall_from = base + virtual_time(RATE as u64);
    let stall_to = stall_from + 12_000_000;

    while virtual_time(consumed) < virtual_time(4 * RATE as u64) {
        let now = base + virtual_time(consumed);
        clock.set(now);
        let stalled = now >= stall_from && now <= stall_to;
        if !stalled {
            while next_arrival <= now {
                ring.write(&packet(&mut phase, PACKET_FRAMES, 0.5, TONE_HZ));
                next_arrival += 10_000_000;
            }
        }
        reader.read(&mut block, BLOCK_FRAMES);
        blocks.push(block.clone());
        consumed += BLOCK_FRAMES as u64;
    }
    let after = reader.stats();
    let padded = (after.idle_frames - before.idle_frames) + (after.starved_frames - before.starved_frames);
    assert_eq!(
        after.starved_frames - before.starved_frames,
        0,
        "a stall on the source side is not our deadline miss"
    );
    assert!(
        padded < 4000,
        "the 16 ms level should bridge most of a 12 ms stall, padded {} frames",
        padded
    );
    assert!(
        warm.min_buffered_ms > 8.0,
        "the level was never established before the stall"
    );
    assert!(
        splice_jump(&blocks) < MAX_JUMP,
        "the return splice after the stall was audible: {}",
        splice_jump(&blocks)
    );
}

#[test]
fn padding_is_counted_by_measured_audibility_and_the_splice_is_ramped() {
    // Force a real underrun on purpose: ask for a block far larger than the level while arrivals are
    // still fresh. The reader must ramp both edges, so the measured splice stays quiet and the frames land
    // in idle rather than in the glitch counter — the accounting follows the ear, not the clock.
    let (clock, ring) = clock_and_ring();
    let mut reader = ring.create_reader(true);
    // Long enough for the controller to settle on the target: the first tens of seconds are its transient.
    drive(&mut reader, &clock, &ring, 10_000_000, 30.0);
    let before: BufferStats = reader.stats();
    // buffered_ms is instantaneous and sawtooths by a whole arrival period, so the useful bound here is
    // only "far below the block we are about to ask for".
    assert!(
        before.buffered_ms < 35.0,
        "level {:.1} ms is too high for this to force an underrun",
        before.buffered_ms
    );

    let mut phase = 0.0f64;
    let mut blocks: Vec<Vec<f32>> = Vec::new();
    clock.advance(1_000_000);
    ring.write(&packet(&mut phase, PACKET_FRAMES, 0.5, TONE_HZ));
    let mut big = vec![0.0f32; 2000 * CH];
    clock.advance(1_000_000);
    reader.read(&mut big, 2000);
    blocks.push(big);
    let mut recovery = vec![0.0f32; BLOCK_FRAMES * CH];
    for _ in 0..20 {
        clock.advance(10_000_000);
        ring.write(&packet(&mut phase, PACKET_FRAMES, 0.5, TONE_HZ));
        reader.read(&mut recovery, BLOCK_FRAMES);
        blocks.push(recovery.clone());
    }

    let after = reader.stats();
    let padded = (after.idle_frames - before.idle_frames) + (after.starved_frames - before.starved_frames);
    assert!(
        padded > 500,
        "the underrun has to be accounted somewhere: idle +{ }, starved +{ }",
        after.idle_frames - before.idle_frames,
        after.starved_frames - before.starved_frames
    );
    assert_eq!(
        after.starved_frames - before.starved_frames,
        0,
        "a ramped splice must not be reported as a heard glitch"
    );
    let jump = splice_jump(&blocks);
    assert!(jump < MAX_JUMP, "biggest splice across the underrun was {jump}");
}

#[test]
fn route_and_gain_edits_never_step_the_waveform() {
    // Near-DC tone: the signal's own slope is ~0.0007 per sample, so a 0.02 step can only be an edit we
    // made with a hard edge.
    let (clock, ring_a) = clock_and_ring();
    let ring_b = ring_of(&clock);
    let mixer = Mixer::new(RATE, CH, "L1", clock.clone() as Arc<dyn Clock>);
    let mut phase = 0.0f64;
    let mut consumed: u64 = 0;
    let mut next_arrival = 0u64;
    let mut block = vec![0.0f32; BLOCK_FRAMES * CH];
    let mut blocks: Vec<Vec<f32>> = Vec::new();

    mixer.attach_source(ring_a.clone(), 1, 1.0);
    while virtual_time(consumed) < virtual_time(8 * RATE as u64) {
        let now = virtual_time(consumed);
        clock.set(now);
        while next_arrival <= now {
            let data = packet(&mut phase, PACKET_FRAMES, 0.5, SLOW_HZ);
            ring_a.write(&data);
            ring_b.write(&data);
            next_arrival += 10_000_000;
        }
        if consumed == (RATE as u64) * 3 {
            mixer.attach_source(ring_b.clone(), 2, 1.0);
        }
        if consumed == (RATE as u64) * 5 {
            mixer.detach_source(2);
        }
        if consumed == (RATE as u64) * 6 {
            mixer.set_gain(1, 0.25);
        }
        mixer.mix(&mut block);
        blocks.push(block.clone());
        consumed += BLOCK_FRAMES as u64;
    }

    let jump = sample_jump(&blocks, 200);
    assert!(
        jump < MAX_JUMP,
        "route and gain edits produced a {jump} step; every self-made edge is supposed to be slewed",
    );
    assert_eq!(mixer.routed_ids(), vec![1], "the faded-out slot must be gone");
    assert!(
        blocks
            .last()
            .map(|b| b.iter().any(|&s| s.abs() > 0.05))
            .unwrap_or(false),
        "the surviving source went silent along with it"
    );
}

#[test]
fn past_full_scale_is_clipped_and_counted_not_wrapped() {
    let (clock, source) = clock_and_ring();
    let mixer = Mixer::new(RATE, CH, "L1", clock.clone() as Arc<dyn Clock>);
    mixer.attach_source(source.clone(), 1, 2.0);
    let mut block = vec![0.0f32; BLOCK_FRAMES * CH];
    for _ in 0..400 {
        source.write(&vec![0.9f32; PACKET_FRAMES * CH]);
        clock.advance(10_000_000);
        mixer.mix(&mut block);
    }
    assert!(
        block.iter().all(|&s| s.abs() <= 1.0),
        "samples must never exceed full scale"
    );
    assert!(
        mixer.clipped_samples() > 0,
        "the clipping itself has to be counted, not silently fixed"
    );
}

#[test]
fn a_stalled_line_owns_its_own_drop_and_does_not_drag_the_other_one() {
    let (clock, shared) = clock_and_ring();
    let mut keeping_up = shared.create_reader(true);
    let mut stalled = shared.create_reader(true);
    let mut phase = 0.0f64;
    // Write and read at the same rate on the healthy line; the stalled line never reads.
    let mut out = vec![0.0f32; PACKET_FRAMES * CH];
    for _ in 0..(RATE / PACKET_FRAMES) * 4 {
        shared.write(&packet(&mut phase, PACKET_FRAMES, 0.3, TONE_HZ));
        clock.advance(10_000_000);
        keeping_up.read(&mut out, PACKET_FRAMES);
    }
    assert_eq!(
        keeping_up.stats().dropped_frames,
        0,
        "the line that keeps up must not pay for the other one"
    );
    let mut once = vec![0.0f32; BLOCK_FRAMES * CH];
    stalled.read(&mut once, BLOCK_FRAMES);
    let dropped = stalled.stats().dropped_frames;
    assert!(
        dropped >= RING_CAPACITY_FRAMES as u64,
        "the stalled line must account its own skip, saw {dropped}"
    );
}

//! The engine's whole view of the audio stack: an `Endpoint` the user can name, a `Capture` that pushes
//! interleaved float frames, and a `Sink` that pulls blocks at the pace of an output device. Each backend
//! absorbs its own API differences, so the engine never learns which one was compiled in.
//!
//! The canonical stream is 48 kHz stereo 32-bit float everywhere inside the program.

pub const SAMPLE_RATE: usize = 48_000;
pub const CHANNELS: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Capture,
    Render,
}

/// One endpoint the user can pick, named by the id the OS assigns.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

/// What a stream was actually granted, as opposed to what was asked for. The low-latency numbers here
/// are the ones the UI and the diagnostics quote; a request that was declined must say why.
#[derive(Clone, Debug)]
pub struct StreamFacts {
    pub endpoint: String,
    pub format: String,
    pub granted_period_ms: f64,
    pub buffer_frames: u32,
    pub low_latency_active: bool,
    pub low_latency_reason: String,
}

impl Default for StreamFacts {
    fn default() -> Self {
        Self {
            endpoint: String::new(),
            format: String::new(),
            granted_period_ms: 0.0,
            buffer_frames: 0,
            low_latency_active: false,
            low_latency_reason: String::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct StreamOptions {
    /// Extra queue the backend may keep, in milliseconds. Not a latency setting: the arrival period is
    /// decided by the device, and shrinking this only removes jitter protection.
    pub buffer_ms: u32,
    pub low_latency: bool,
    /// Run the thread under the "Pro Audio" multimedia schedule.
    pub mmcss: bool,
}

impl Default for StreamOptions {
    fn default() -> Self {
        Self {
            buffer_ms: 100,
            low_latency: false,
            mmcss: true,
        }
    }
}

/// Delivery bookkeeping for one stream: how big each transfer was and how far apart they landed.
/// This is what separates "the source is quiet" from "the source missed its deadline".
#[derive(Clone, Debug, Default)]
pub struct Delivery {
    pub packets: u64,
    pub frames: u64,
    pub min_packet_frames: u32,
    pub max_packet_frames: u32,
    /// Longest gap between consecutive transfers, in milliseconds.
    pub max_gap_ms: f64,
    pub silent_packets: u64,
    pub discontinuity: u64,
    /// Why the loop stopped. Empty while the stream is healthy.
    pub error: String,
    /// The HRESULT of the failure that reset the endpoint under this stream, when the stop was one of
    /// those. It is the difference between a line the engine can reopen by itself and a line the user has
    /// to look at.
    pub endpoint_reset: Option<u32>,
}

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub use win::capture::{Capture, OnFrames, Source};
#[cfg(windows)]
pub use win::brand;
#[cfg(windows)]
pub use win::candidates::{list_window_candidates, WindowRow};
#[cfg(windows)]
pub use win::icon::{for_pid as icon_for_pid, note_window as note_window_icon, Icon as ProcessIcon};
#[cfg(windows)]
pub use win::devices::{
    describe, find_endpoint, list_endpoints, render_sessions, default_endpoint as win_default,
    probe as win_probe,
};
#[cfg(windows)]
pub use win::render::{FillBlock, Sink};
#[cfg(windows)]
pub use win::process::{
    alive as process_alive, name as process_name, path as process_path, self_pid, session_pids,
    ProcessFacts, processes_named,
};

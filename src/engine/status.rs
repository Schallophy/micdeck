//! The wire shapes `ui.html` reads. Field names are the UI contract and must not change.

use serde::{Deserialize, Serialize};

/// What a source is, in a form that survives a reboot: the executable path when we can read it, the image
/// name when we cannot. A process id is deliberately not part of it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Identity {
    pub name: String,
    pub path: String,
}

impl Identity {
    /// Path wins; the name only breaks ties for processes whose module we cannot open. Separators are
    /// normalised so a hand-typed or round-tripped path still matches.
    pub fn key(&self) -> String {
        if self.path.is_empty() {
            format!("n:{}", self.name.trim().to_lowercase())
        } else {
            format!(
                "p:{}",
                self.path.replace('\\', "/").trim_end_matches('/').to_lowercase()
            )
        }
    }
}

/// One source on one line: the buffer and resampler state are per line, not per source.
#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteInfo {
    pub line_id: u32,
    pub buffered_ms: f64,
    /// Adaptive-resampling ratio deviation for this line's clock; 0 means reading 1:1.
    pub ratio_ppm: i32,
    pub starved_frames: u64,
    pub idle_frames: u64,
    pub dropped_frames: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceInfo {
    pub id: u32,
    /// The live process, or 0 while the source is only remembered and waiting for its app.
    pub pid: u32,
    pub name: String,
    pub title: String,
    pub path: String,
    pub gain: f32,
    pub level: f32,
    pub hold: f32,
    pub alive: bool,
    /// Lines the user wants this source on, connected or not — what gets persisted.
    pub lines: Vec<u32>,
    pub routes: Vec<RouteInfo>,
}

/// Meter for one app while the add-source list is open; only alive for that moment.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewInfo {
    pub pid: u32,
    pub level: f32,
    pub hold: f32,
    pub error: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LineInfo {
    pub id: u32,
    pub device_id: String,
    pub name: String,
    /// The user's own remark for this line; empty means "just the device name".
    pub label: String,
    pub state: String,
    pub error: String,
    pub mix_level: f32,
    pub mix_hold: f32,
    pub clipped_samples: u64,
    /// Blocks replaced with silence because this line's mix threw; should stay 0.
    pub mix_backstops: u64,
    /// Deadline misses on this line alone: silence this endpoint had to be given.
    pub starved_frames: u64,
    /// Frames sitting in this render endpoint's buffer right now.
    pub queue_ms: f64,
    /// Latency this endpoint actually granted at init time.
    pub render_latency_ms: f64,
    pub low_latency_active: bool,
    pub low_latency_reason: String,
    /// Feed ids routed here; the microphone's id means the microphone.
    pub source_ids: Vec<u32>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineStatus {
    pub running: bool,
    pub state: String,
    /// Only the aggregate: set when every line is down. Per-line faults stay on their line.
    pub error: String,
    pub mic_id: String,
    pub mic_name: String,
    pub mic_gain: f32,
    /// The master switch: the microphone is captured but its slot on every line is at zero gain.
    pub mic_muted: bool,
    pub mic_level: f32,
    pub mic_hold: f32,
    pub mic_routes: Vec<RouteInfo>,
    /// Sum of deadline misses across every source on every line.
    pub starved_frames: u64,
    pub lines: Vec<LineInfo>,
    pub sources: Vec<SourceInfo>,
    /// Non-empty only while the add-source list is open: one capture per candidate app.
    pub previews: Vec<PreviewInfo>,
}

//! The engine: one bus per output line. A capture makes one shared ring; every line that routes it reads
//! through its own cursor and resampler, because each endpoint runs on its own clock. Sources and routes
//! are edited live.
//!
//! A source exists from the moment the user adds it, whether or not a line carries it; a pid only lives
//! inside [`Feed`].
//!
//! Locking: the `world` mutex serialises topology edits and may be held across a device open, since no
//! audio thread takes it. Audio threads take only ring and bus locks, in the order bus → ring.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use super::clock::Clock;
use super::meter::{peak_of, LiveMeter};
use super::mixer::Mixer;
use super::ring::{SourceRing, RING_CAPACITY_FRAMES};
use super::status::{EngineStatus, Identity, LineInfo, PreviewInfo, RouteInfo, SourceInfo};
use crate::log;
use crate::platform::{
    self, Capture, Direction, Endpoint, FillBlock, OnFrames, Sink, Source, StreamOptions,
};

/// The microphone is a permanent feed with a fixed id; its routes are what the user edits.
pub const MIC_FEED_ID: u32 = 0;
/// +12 dB, the ceiling the slider maps to; above unity the limiter is doing the work.
pub const MAX_GAIN: f32 = 4.0;
/// The queue a render endpoint is asked for. Not a latency setting — what counts is what it grants.
const LINE_BUFFER_MS: u32 = 20;
/// Capture-side jitter allowance. The arrival period is decided by the device, so this only absorbs hiccups.
const CAPTURE_BUFFER_MS: u32 = 100;
/// A preview capture is dropped unless the add list renews its lease this often.
const PREVIEW_LEASE_NS: u64 = 15_000_000_000;
/// Watchdog period: fast enough that an app starting feels instant.
const MONITOR_PERIOD_MS: u64 = 500;
/// Reopen attempts a faulted line gets before the watchdog stops trying. An endpoint that is simply not
/// there is not counted against it — waiting for a plug is not a failure — so this bound only ever fires
/// on a device that is present and keeps refusing to start.
const MAX_RECONNECT_TRIES: u32 = 8;

/// The live half of a source: a process loopback writing into a shared ring.
struct Feed {
    pid: u32,
    ring: Arc<SourceRing>,
    capture: Capture,
}

/// The user's source: identity, gain and lines outlive any one process.
struct SourceWant {
    id: u32,
    identity: Identity,
    title: String,
    gain: f32,
    meter: Arc<LiveMeter>,
    live: Option<Feed>,
    lines: Vec<u32>,
}

/// A throwaway capture used only to paint meters while the add list is open.
struct Preview {
    pid: u32,
    name: String,
    meter: Arc<LiveMeter>,
    capture: Option<Capture>,
    error: String,
}

struct Line {
    id: u32,
    device_id: String,
    name: String,
    label: String,
    bus: Arc<Mixer>,
    sink: Option<Sink>,
    state: String,
    error: String,
    /// Set when the endpoint died under a running line: the watchdog replays the restart on this line
    /// until it comes back or refuses it a few times.
    reconnecting: bool,
    /// Failed reopen attempts so far, counted only while the endpoint is actually enumerated.
    reconnect_tries: u32,
    /// The "not enumerated, waiting" line is written once per fault, not once per half second.
    reconnect_notified: bool,
}

impl Line {
    fn stop_sink(&mut self) {
        if let Some(mut sink) = self.sink.take() {
            sink.stop();
        }
        self.bus.clear_sources();
    }
}

struct Mic {
    device_id: String,
    name: String,
    gain: f32,
    /// Silences every line's microphone slot without stopping the capture.
    muted: bool,
    ring: Arc<SourceRing>,
    capture: Option<Capture>,
    /// The selected device went away under the capture. The selection is kept and the watchdog looks again.
    lost: bool,
    lines: Vec<u32>,
    meter: Arc<LiveMeter>,
}

struct World {
    wants: Vec<SourceWant>,
    lines: Vec<Line>,
    previews: HashMap<u32, Preview>,
    preview_lease_ns: u64,
    mic: Mic,
    next_source_id: u32,
    next_line_id: u32,
    error: String,
}

pub struct Engine {
    world: Mutex<World>,
    clock: Arc<dyn Clock>,
    tick: AtomicU32,
    stop: Arc<AtomicBool>,
    monitor: Mutex<Option<std::thread::JoinHandle<()>>>,
}

fn capture_options() -> StreamOptions {
    StreamOptions {
        buffer_ms: CAPTURE_BUFFER_MS,
        // No `IAudioClient3` here: a microphone grants nothing below its own cadence and a process
        // loopback refuses it outright, and this is the packet size the level controller was tuned on.
        low_latency: false,
        mmcss: true,
    }
}

fn line_options() -> StreamOptions {
    StreamOptions {
        buffer_ms: LINE_BUFFER_MS,
        low_latency: true,
        mmcss: true,
    }
}

/// Hands each arrival to the ring and paints the meter. The callback must not reach back into the engine.
fn capture_callback(ring: Arc<SourceRing>, meter: Arc<LiveMeter>) -> OnFrames {
    Box::new(move |frames: &[f32]| {
        ring.write(frames);
        if !frames.is_empty() {
            meter.update(peak_of(frames));
        }
    })
}

fn meter_only_callback(meter: Arc<LiveMeter>) -> OnFrames {
    Box::new(move |frames: &[f32]| {
        if !frames.is_empty() {
            meter.update(peak_of(frames));
        }
    })
}

/// A line's render block: mix, and never hand the endpoint a half-written block. A panic here is a bug in
/// the mixer, so it is silenced for one block, counted, and reported instead of being repeated as garbage.
fn fill_callback(bus: Arc<Mixer>) -> FillBlock {
    Box::new(move |destination: &mut [f32]| {
        let mixed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| bus.mix(destination)));
        if mixed.is_err() {
            destination.fill(0.0);
            bus.note_backstop();
        }
    })
}

fn endpoint_of(id: &str, name: &str) -> Endpoint {
    Endpoint {
        id: id.to_string(),
        name: name.to_string(),
        is_default: false,
    }
}

impl Engine {
    /// Starts the watchdog with the engine: a source can exit while no line exists and still has to be
    /// noticed.
    pub fn start(clock: Arc<dyn Clock>) -> Arc<Self> {
        let ring = SourceRing::new(
            platform::CHANNELS,
            platform::SAMPLE_RATE,
            RING_CAPACITY_FRAMES,
            clock.clone(),
        );
        let engine = Arc::new(Self {
            world: Mutex::new(World {
                wants: Vec::new(),
                lines: Vec::new(),
                previews: HashMap::new(),
                preview_lease_ns: 0,
                mic: Mic {
                    device_id: String::new(),
                    name: String::new(),
                    gain: 1.0,
                    muted: false,
                    ring,
                    capture: None,
                    lost: false,
                    lines: Vec::new(),
                    meter: LiveMeter::new(clock.clone()),
                },
                next_source_id: 1,
                next_line_id: 1,
                error: String::new(),
            }),
            clock,
            tick: AtomicU32::new(0),
            stop: Arc::new(AtomicBool::new(false)),
            monitor: Mutex::new(None),
        });
        let weak = Arc::downgrade(&engine);
        let stop = engine.stop.clone();
        let handle = std::thread::Builder::new()
            .name("micdeck-monitor".to_string())
            .spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_millis(MONITOR_PERIOD_MS));
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    match weak.upgrade() {
                        Some(engine) => engine.monitor_tick(),
                        None => break,
                    }
                }
            });
        if let Ok(handle) = handle {
            *engine.monitor.lock().unwrap() = Some(handle);
        }
        engine
    }

    /// Poison is not fatal here: the table is plain data, and after a panic in an edit the worst outcome is
    /// continuing with whatever the last complete edit left.
    fn world(&self) -> MutexGuard<'_, World> {
        self.world
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // --- status -----------------------------------------------------------------------------

    pub fn snapshot(&self) -> EngineStatus {
        let mut routes: HashMap<u32, Vec<RouteInfo>> = HashMap::new();
        let mut starved_total = 0u64;
        let mut running = 0usize;
        let mut faulted = 0usize;
        let mut status = EngineStatus::default();

        {
            let world = self.world();
            status.mic_id = world.mic.device_id.clone();
            status.mic_name = world.mic.name.clone();
            status.mic_gain = world.mic.gain;
            status.mic_muted = world.mic.muted;
            status.mic_level = world.mic.meter.level();
            status.mic_hold = world.mic.meter.hold();
            status.previews = world
                .previews
                .values()
                .map(|preview| PreviewInfo {
                    pid: preview.pid,
                    level: preview.meter.level(),
                    hold: preview.meter.hold(),
                    error: preview.error.clone(),
                })
                .collect();

            // One pass over the buses builds the per-source route table; the UI keys rows by source id and
            // pools by line, so both views come from the same snapshot.
            for line in world.lines.iter() {
                let mut info = LineInfo {
                    id: line.id,
                    device_id: line.device_id.clone(),
                    name: line.name.clone(),
                    label: line.label.clone(),
                    state: line.state.clone(),
                    error: line.error.clone(),
                    mix_level: line.bus.mix_peak(),
                    mix_hold: line.bus.mix_hold(),
                    clipped_samples: line.bus.clipped_samples(),
                    mix_backstops: line.bus.backstop_errors(),
                    source_ids: line.bus.routed_ids(),
                    ..LineInfo::default()
                };
                if let Some(sink) = &line.sink {
                    let facts = sink.facts();
                    info.queue_ms = sink.queue_ms();
                    info.render_latency_ms = facts.granted_period_ms;
                    info.low_latency_active = facts.low_latency_active;
                    info.low_latency_reason = facts.low_latency_reason.clone();
                }
                for feed_id in info.source_ids.iter() {
                    if let Some(stats) = line.bus.stats(*feed_id) {
                        starved_total += stats.starved_frames;
                        info.starved_frames += stats.starved_frames;
                        routes.entry(*feed_id).or_default().push(RouteInfo {
                            line_id: line.id,
                            buffered_ms: stats.buffered_ms,
                            ratio_ppm: stats.ratio_ppm,
                            starved_frames: stats.starved_frames,
                            idle_frames: stats.idle_frames,
                            dropped_frames: stats.dropped_frames,
                        });
                    }
                }
                match line.state.as_str() {
                    "running" => running += 1,
                    "faulted" => faulted += 1,
                    _ => {}
                }
                status.lines.push(info);
            }

            status.sources = world
                .wants
                .iter()
                .map(|want| SourceInfo {
                    id: want.id,
                    pid: want.live.as_ref().map(|feed| feed.pid).unwrap_or(0),
                    name: want.identity.name.clone(),
                    title: want.title.clone(),
                    path: want.identity.path.clone(),
                    gain: want.gain,
                    level: want.meter.level(),
                    hold: want.meter.hold(),
                    alive: want.live.is_some(),
                    lines: want.lines.clone(),
                    routes: routes.remove(&want.id).unwrap_or_default(),
                })
                .collect();
            status.mic_routes = routes.remove(&MIC_FEED_ID).unwrap_or_default();
        }

        status.starved_frames = starved_total;
        status.running = running > 0;
        status.state = if status.lines.is_empty() {
            "stopped"
        } else if running > 0 {
            "running"
        } else if faulted > 0 {
            "faulted"
        } else {
            "stopped"
        }
        .to_string();

        // Only the aggregate is reported here: set when every line is down. Per-line faults stay on their
        // line, where the UI can show which one.
        let mut world = self.world();
        if running == 0 && faulted > 0 {
            world.error = status
                .lines
                .iter()
                .find(|line| line.state == "faulted")
                .map(|line| line.error.clone())
                .unwrap_or_default();
        } else if status.lines.is_empty() {
            world.error = String::new();
        }
        status.error = world.error.clone();
        status
    }

    // --- lines ------------------------------------------------------------------------------

    /// Adds an output line. Nothing is ever chosen for the user: the first call is the user's.
    pub fn add_line(&self, selector: &str) -> Result<u32, String> {
        let endpoint = platform::find_endpoint(Direction::Render, selector)?;
        let mut world = self.world();
        let id = world.next_line_id;
        world.next_line_id += 1;
        let bus = Arc::new(Mixer::new(
            platform::SAMPLE_RATE,
            platform::CHANNELS,
            &format!("L{id}"),
            self.clock.clone(),
        ));
        let mut line = Line {
            id,
            device_id: endpoint.id.clone(),
            name: endpoint.name.clone(),
            label: String::new(),
            bus,
            sink: None,
            state: "starting".to_string(),
            error: String::new(),
            reconnecting: false,
            reconnect_tries: 0,
            reconnect_notified: false,
        };
        self.open_sink(&mut line, &endpoint, &mut world)?;
        let name = line.name.clone();
        world.lines.push(line);
        drop(world);
        log::line(&format!("line {id} added -> '{name}'"));
        Ok(id)
    }

    /// Puts a running sink on this line and attaches everything the user wants on it. The feeds themselves
    /// are untouched: their rings keep being written by the capture callbacks, so re-attaching readers only
    /// re-primes this line's buffers.
    fn open_sink(
        &self,
        line: &mut Line,
        endpoint: &Endpoint,
        world: &mut World,
    ) -> Result<(), String> {
        let sink = Sink::start(
            endpoint,
            line_options(),
            fill_callback(line.bus.clone()),
        )
        .map_err(|err| format!("线路启动失败: {err}"))?;
        let facts = sink.facts();
        for want in world.wants.iter() {
            if want.lines.contains(&line.id) {
                if let Some(feed) = &want.live {
                    line.bus.attach_source(feed.ring.clone(), want.id, want.gain);
                }
            }
        }
        if world.mic.lines.contains(&line.id) {
            line.bus.attach_source(
                world.mic.ring.clone(),
                MIC_FEED_ID,
                mic_gain_for(&world.mic),
            );
        }
        line.sink = Some(sink);
        line.device_id = endpoint.id.clone();
        line.name = endpoint.name.clone();
        line.state = "running".to_string();
        line.error = String::new();
        log::line(&format!(
            "line {} '{}': granted={:.1}ms lowLatency={}{}",
            line.id,
            line.name,
            facts.granted_period_ms,
            facts.low_latency_active,
            if facts.low_latency_reason.is_empty() {
                String::new()
            } else {
                format!(" reason={}", facts.low_latency_reason)
            }
        ));
        Ok(())
    }

    pub fn remove_line(&self, line_id: u32) {
        let mut world = self.world();
        let Some(index) = world.lines.iter().position(|line| line.id == line_id) else {
            return;
        };
        let mut line = world.lines.remove(index);
        let name = line.name.clone();
        line.stop_sink();
        for want in world.wants.iter_mut() {
            want.lines.retain(|id| *id != line_id);
        }
        world.mic.lines.retain(|id| *id != line_id);
        drop(world);
        log::line(&format!("line {line_id} removed ('{name}')"));
    }

    /// Rebuilds one line: fresh readers, so this line's buffers re-prime. Every other line, every capture
    /// and every route definition survive untouched.
    pub fn restart_line(&self, line_id: u32) -> Result<(), String> {
        let mut world = self.world();
        let Some(index) = world.lines.iter().position(|line| line.id == line_id) else {
            return Err(format!("线路不存在: {line_id}"));
        };
        let device_id = world.lines[index].device_id.clone();
        let previous_error = world.lines[index].error.clone();
        let mut line = world.lines.remove(index);
        line.stop_sink();
        line.state = "starting".to_string();
        line.error = String::new();
        let endpoint = platform::find_endpoint(Direction::Render, &device_id)
            .map(|found| endpoint_of(&found.id, &found.name));
        match endpoint.and_then(|endpoint| self.open_sink(&mut line, &endpoint, &mut world)) {
            Ok(()) => {
                // A restart is a restart whether the user clicked it or the watchdog did: the fault it is
                // recovering from is over, and the retry budget starts clean for the next one.
                line.reconnecting = false;
                line.reconnect_tries = 0;
                line.reconnect_notified = false;
                world.lines.push(line);
                drop(world);
                log::line(&format!("line {line_id} restarted"));
                Ok(())
            }
            Err(err) => {
                line.state = "faulted".to_string();
                line.error = err.clone();
                world.lines.push(line);
                drop(world);
                log::line(&format!(
                    "line {line_id} restart failed: {err} (previous: {previous_error})"
                ));
                Err(err)
            }
        }
    }

    pub fn restart_all_lines(&self) {
        let ids = self
            .world()
            .lines
            .iter()
            .map(|line| line.id)
            .collect::<Vec<_>>();
        for id in ids {
            if let Err(err) = self.restart_line(id) {
                log::line(&format!("line {id} restart skipped: {err}"));
            }
        }
    }

    /// Replaces only this line's sink: the bus, its readers and every other line keep going, so a failed
    /// switch can be undone by attaching a sink to the previous endpoint.
    pub fn set_line_device(&self, line_id: u32, selector: &str) -> Result<(), String> {
        let mut world = self.world();
        let Some(index) = world.lines.iter().position(|line| line.id == line_id) else {
            return Err(format!("线路不存在: {line_id}"));
        };
        let previous_id = world.lines[index].device_id.clone();
        let previous_name = world.lines[index].name.clone();
        let mut line = world.lines.remove(index);
        let endpoint = match platform::find_endpoint(Direction::Render, selector) {
            Ok(endpoint) => endpoint,
            Err(err) => {
                world.lines.insert(index, line);
                return Err(err);
            }
        };
        // Stop the old sink but keep the bus: on a failure the routes have to be re-attachable to whatever
        // endpoint still answers.
        if let Some(mut sink) = line.sink.take() {
            sink.stop();
        }
        line.bus.clear_sources();
        let reason = match self.open_sink(&mut line, &endpoint, &mut world) {
            Ok(()) => {
                world.lines.push(line);
                drop(world);
                log::line(&format!("line {line_id} -> '{}'", endpoint.name));
                return Ok(());
            }
            Err(err) => err,
        };
        log::line(&format!("line {line_id} device switch failed: {reason}"));
        // The old sink is gone but the bus is intact: put a sink back on the previous endpoint instead of
        // tearing the line down and dropping every route on it.
        match platform::find_endpoint(Direction::Render, &previous_id)
            .map_err(|e| e.to_string())
            .and_then(|previous| self.open_sink(&mut line, &previous, &mut world))
        {
            Ok(()) => {
                world.lines.push(line);
                drop(world);
                log::line(&format!("line {line_id} rolled back -> '{previous_name}'"));
            }
            Err(rollback) => {
                log::line(&format!("line {line_id} rollback failed: {rollback}"));
                world.lines.push(Line {
                    id: line_id,
                    device_id: previous_id,
                    name: String::new(),
                    label: line.label.clone(),
                    bus: line.bus.clone(),
                    sink: None,
                    state: "faulted".to_string(),
                    error: format!("切换输出失败: {reason}; 回滚失败: {rollback}"),
                    reconnecting: false,
                    reconnect_tries: 0,
                    reconnect_notified: false,
                });
                drop(world);
            }
        }
        Err(reason)
    }

    /// A remark only: it never touches the device, the bus or the routes.
    pub fn set_line_label(&self, line_id: u32, label: &str) -> Result<(), String> {
        let mut world = self.world();
        let line = world
            .lines
            .iter_mut()
            .find(|line| line.id == line_id)
            .ok_or_else(|| format!("线路不存在: {line_id}"))?;
        let next: String = label.trim().chars().take(40).collect();
        if line.label == next {
            return Ok(());
        }
        line.label = next.clone();
        drop(world);
        log::line(&format!("line {line_id} label -> '{next}'"));
        Ok(())
    }

    // --- sources ----------------------------------------------------------------------------

    /// Adds an app as a source. It is captured from now on whether or not a line carries it, so its meter
    /// is live while the user decides where to route it.
    pub fn add_app_source(&self, pid: u32, title: &str) -> Result<u32, String> {
        if !platform::process_alive(pid) {
            return Err(format!("进程不存在: {pid}"));
        }
        let identity = Identity {
            name: platform::process_name(pid),
            path: platform::process_path(pid),
        };
        if identity.name.is_empty() {
            return Err(format!("无法读取进程 {pid} 的名称"));
        }
        let mut world = self.world();
        // Reuse a remembered-but-not-running source with the same identity; two live processes of one
        // executable are two rows on purpose.
        let existing = world
            .wants
            .iter()
            .position(|want| want.identity.key() == identity.key() && want.live.is_none())
            .or_else(|| {
                world
                    .wants
                    .iter()
                    .position(|want| want.live.as_ref().map(|feed| feed.pid) == Some(pid))
            });
        let index = match existing {
            Some(index) => index,
            None => {
                let id = world.next_source_id;
                world.next_source_id += 1;
                world.wants.push(SourceWant {
                    id,
                    identity: identity.clone(),
                    title: String::new(),
                    gain: 1.0,
                    meter: LiveMeter::new(self.clock.clone()),
                    live: None,
                    lines: Vec::new(),
                });
                world.wants.len() - 1
            }
        };
        let want = &mut world.wants[index];
        if !title.trim().is_empty() {
            want.title = title.trim().to_string();
        } else if want.title.is_empty() {
            want.title = identity.name.clone();
        }
        let want_id = want.id;
        // Already captured by this process: the user re-picked the same app, nothing to open.
        let already = want.live.as_ref().map(|feed| feed.pid) == Some(pid);
        self.stop_preview_locked(&mut world, pid);
        if !already {
            if let Err(err) = self.attach_feed(&mut world, want_id, pid) {
                return Err(err);
            }
        }
        let name = world.wants[index].identity.name.clone();
        drop(world);
        log::line(&format!("source added: id={want_id} pid={pid} ({name})"));
        Ok(want_id)
    }

    /// Remembers a source that was in the list before this launch. The feed is attached only when its app
    /// is already running; otherwise the source stays visible and waiting.
    pub fn restore_source(&self, identity: Identity, title: &str, gain: f32, lines: &[u32]) -> u32 {
        if identity.name.trim().is_empty() {
            return 0;
        }
        let mut world = self.world();
        let key = identity.key();
        let existing = world
            .wants
            .iter()
            .position(|want| want.identity.key() == key && want.live.is_none())
            .or_else(|| world.wants.iter().position(|want| want.identity.key() == key));
        let index = match existing {
            Some(index) => index,
            None => {
                let id = world.next_source_id;
                world.next_source_id += 1;
                world.wants.push(SourceWant {
                    id,
                    identity: identity.clone(),
                    title: String::new(),
                    gain: 1.0,
                    meter: LiveMeter::new(self.clock.clone()),
                    live: None,
                    lines: Vec::new(),
                });
                world.wants.len() - 1
            }
        };
        let want = &mut world.wants[index];
        if !title.trim().is_empty() {
            want.title = title.trim().to_string();
        }
        want.gain = gain.clamp(0.0, MAX_GAIN);
        for line_id in lines {
            if !want.lines.contains(line_id) {
                want.lines.push(*line_id);
            }
        }
        let want_id = want.id;
        let remembered_lines = want.lines.clone();
        let name = want.identity.name.clone();
        // Try to pick the app up right away; the watchdog keeps trying if it is not running yet.
        let candidates = platform::processes_named(name.trim());
        let pid = pick_candidate(&candidates, &identity, &platform::session_pids());
        if let Some(pid) = pid {
            let _ = self.attach_feed(&mut world, want_id, pid);
        }
        drop(world);
        log::line(&format!(
            "source restored: id={want_id} '{name}' lines=[{}]",
            remembered_lines
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",")
        ));
        want_id
    }

    /// Starts the loopback for a source and routes it onto the lines that already want it.
    fn attach_feed(&self, world: &mut World, want_id: u32, pid: u32) -> Result<(), String> {
        let (ring, meter, lines, gains) = {
            let want = world
                .wants
                .iter_mut()
                .find(|want| want.id == want_id)
                .ok_or_else(|| format!("音频源不存在: {want_id}"))?;
            if want.live.is_some() {
                return Ok(());
            }
            let ring = SourceRing::new(
                platform::CHANNELS,
                platform::SAMPLE_RATE,
                RING_CAPACITY_FRAMES,
                self.clock.clone(),
            );
            (
                ring.clone(),
                want.meter.clone(),
                want.lines.clone(),
                want.gain,
            )
        };
        let capture = Capture::start(
            Source::Process {
                pid,
                include_tree: true,
            },
            capture_options(),
            capture_callback(ring.clone(), meter),
        )
        .map_err(|err| format!("抓音启动失败: {err}"))?;
        world
            .wants
            .iter_mut()
            .find(|want| want.id == want_id)
            .ok_or_else(|| format!("音频源不存在: {want_id}"))?
            .live = Some(Feed {
            pid,
            ring: ring.clone(),
            capture,
        });
        // Readers are attached after the recorder so the ring is already being filled; a reader created now
        // starts at the write head and primes itself as data arrives.
        for line in world.lines.iter_mut() {
            if lines.contains(&line.id) && line.sink.is_some() {
                line.bus
                    .attach_source(ring.clone(), want_id, clamp_gain(gains));
            }
        }
        Ok(())
    }

    fn detach_feed(&self, world: &mut World, want_id: u32, reason: &str, name: &str) {
        if let Some(mut feed) = world
            .wants
            .iter_mut()
            .find(|want| want.id == want_id)
            .and_then(|want| want.live.take())
        {
            let pid = feed.pid;
            feed.capture.stop();
            if !reason.is_empty() {
                log::line(&format!(
                    "source gone: pid={pid} ({name}) — {reason}; waiting for it to come back"
                ));
            }
        }
    }

    /// Removes the source from the list: it stops being captured and every line drops it.
    pub fn remove_app_source(&self, want_id: u32) {
        let mut world = self.world();
        let Some(index) = world.wants.iter().position(|want| want.id == want_id) else {
            return;
        };
        let lines = world.wants[index].lines.clone();
        let name = world.wants[index].identity.name.clone();
        for line in world.lines.iter_mut() {
            if lines.contains(&line.id) {
                line.bus.detach_source(want_id);
            }
        }
        self.detach_feed(&mut world, want_id, "", &name);
        world.wants.remove(index);
        drop(world);
        log::line(&format!("source removed: id={want_id} ({name})"));
    }

    /// Connects or disconnects a source from one line. The source itself is untouched, so turning off the
    /// last route does not throw away the capture or the user's choice.
    pub fn set_app_route(&self, want_id: u32, line_id: u32, on: bool) -> Result<(), String> {
        let mut world = self.world();
        let line = world
            .lines
            .iter()
            .find(|line| line.id == line_id)
            .ok_or_else(|| format!("线路不存在: {line_id}"))?;
        let want = world
            .wants
            .iter()
            .find(|want| want.id == want_id)
            .ok_or_else(|| format!("音频源不存在: {want_id}"))?;
        let bus = line.bus.clone();
        let ring = want.live.as_ref().map(|feed| feed.ring.clone());
        let gain = want.gain;
        let name = want.identity.name.clone();
        let attached = want.lines.contains(&line_id);
        if on {
            let want = world
                .wants
                .iter_mut()
                .find(|want| want.id == want_id)
                .ok_or_else(|| format!("音频源不存在: {want_id}"))?;
            if !attached {
                want.lines.push(line_id);
            }
            drop(world);
            if attached {
                bus.set_gain(want_id, gain);
            } else if let Some(ring) = ring {
                bus.attach_source(ring, want_id, clamp_gain(gain));
                log::line(&format!("route on: {name} -> line {line_id}"));
            }
            // A source that is not running yet only records the intention; attach_feed routes it when the
            // process comes back.
            return Ok(());
        }
        let want = world
            .wants
            .iter_mut()
            .find(|want| want.id == want_id)
            .ok_or_else(|| format!("音频源不存在: {want_id}"))?;
        want.lines.retain(|id| *id != line_id);
        drop(world);
        if attached {
            bus.detach_source(want_id);
            log::line(&format!("route off: {name} -> line {line_id}"));
        }
        Ok(())
    }

    pub fn set_source_gain(&self, want_id: u32, gain: f32) {
        let gain = clamp_gain(gain);
        let mut world = self.world();
        let lines = match world.wants.iter_mut().find(|want| want.id == want_id) {
            Some(want) => {
                want.gain = gain;
                want.lines.clone()
            }
            None => return,
        };
        for line in world.lines.iter_mut() {
            if lines.contains(&line.id) {
                line.bus.set_gain(want_id, gain);
            }
        }
    }

    /// The gain belongs to the source; each line's reader is what has to be told about it.
    fn apply_mic_gain(&self, world: &mut World) {
        let gain = mic_gain_for(&world.mic);
        let lines = world.mic.lines.clone();
        for line in world.lines.iter_mut() {
            if lines.contains(&line.id) {
                line.bus.set_gain(MIC_FEED_ID, gain);
            }
        }
    }

    // --- microphone --------------------------------------------------------------------------

    /// The master switch: silence the microphone's slot on every line but keep the capture running.
    /// Unmuting is instant, and the input meter keeps reporting what the hardware actually hears.
    /// Returns whether the switch moved, because the caller's cue is only for a real change.
    pub fn set_mic_muted(&self, muted: bool) -> bool {
        let mut world = self.world();
        if !Self::apply_mic_muted(&mut world, muted) {
            return false;
        }
        drop(world);
        log::line(if muted { "mic muted" } else { "mic unmuted" });
        true
    }

    /// Returns the new state so the tray and the page can agree without a second read.
    pub fn toggle_mic_muted(&self) -> bool {
        let mut world = self.world();
        let next = !world.mic.muted;
        if Self::apply_mic_muted(&mut world, next) {
            drop(world);
            log::line(if next { "mic muted" } else { "mic unmuted" });
        }
        next
    }

    fn apply_mic_muted(world: &mut World, muted: bool) -> bool {
        if world.mic.muted == muted {
            return false;
        }
        world.mic.muted = muted;
        let gain = mic_gain_for(&world.mic);
        let lines = world.mic.lines.clone();
        for line in world.lines.iter_mut() {
            if lines.contains(&line.id) {
                // The existing gain slew makes this a ~14 ms fade, not a step.
                line.bus.set_gain(MIC_FEED_ID, gain);
            }
        }
        true
    }

    /// Selecting a microphone means capturing it: the meter moves whether or not a line carries it yet.
    pub fn set_mic(&self, selector: &str, gain: f32) -> Result<(), String> {
        let mut world = self.world();
        world.mic.gain = clamp_gain(gain);
        self.apply_mic_gain(&mut world);

        let selector = selector.trim_matches(|c| c == '"' || c == '\'').to_string();
        // A gain edit on the device that is already chosen must not tear the capture down: the slider is
        // dragged continuously, and restarting the recorder would gap the very signal being adjusted.
        let same = (world.mic.capture.is_some() || !world.mic.device_id.is_empty())
            && !selector.is_empty()
            && (selector.eq_ignore_ascii_case(&world.mic.device_id)
                || (!world.mic.name.is_empty() && selector.eq_ignore_ascii_case(&world.mic.name)));
        if same {
            let gain = world.mic.gain;
            drop(world);
            log::line(&format!("mic gain -> {gain:.2}"));
            return Ok(());
        }

        self.close_mic(&mut world);
        world.mic.lost = false;
        world.mic.name = String::new();
        world.mic.device_id = String::new();
        if selector.is_empty() {
            drop(world);
            log::line("mic off");
            return Ok(());
        }
        let endpoint = platform::find_endpoint(Direction::Capture, &selector)?;
        world.mic.name = endpoint.name.clone();
        world.mic.device_id = endpoint.id.clone();
        let gain = world.mic.gain;
        self.open_mic_capture(&mut world)?;
        drop(world);
        log::line(&format!(
            "mic capture on: '{}' gain={:.2}",
            endpoint.name, gain
        ));
        Ok(())
    }

    pub fn restore_mic(&self, selector: &str, gain: f32) -> Result<(), String> {
        self.set_mic(selector, gain)
    }

    fn open_mic_capture(&self, world: &mut World) -> Result<(), String> {
        if world.mic.capture.is_some() || world.mic.device_id.is_empty() {
            return Ok(());
        }
        let endpoint = endpoint_of(&world.mic.device_id, &world.mic.name);
        let capture = Capture::start(
            Source::Microphone(endpoint.clone()),
            capture_options(),
            capture_callback(world.mic.ring.clone(), world.mic.meter.clone()),
        )
        .map_err(|err| format!("麦克风启动失败: {err}"))?;
        world.mic.capture = Some(capture);
        world.mic.lost = false;
        Ok(())
    }

    fn close_mic(&self, world: &mut World) {
        if let Some(mut capture) = world.mic.capture.take() {
            capture.stop();
        }
    }

    /// Connects or disconnects the microphone from one line.
    pub fn set_mic_route(&self, line_id: u32, on: bool) -> Result<(), String> {
        let mut world = self.world();
        let bus = world
            .lines
            .iter()
            .find(|line| line.id == line_id)
            .ok_or_else(|| format!("线路不存在: {line_id}"))?
            .bus
            .clone();
        let attached = world.mic.lines.contains(&line_id);
        if on {
            if !attached {
                world.mic.lines.push(line_id);
            }
            let gain = mic_gain_for(&world.mic);
            let ring = world.mic.ring.clone();
            let reopen = world.mic.capture.is_none() && !world.mic.device_id.is_empty();
            drop(world);
            if attached {
                bus.set_gain(MIC_FEED_ID, gain);
            } else {
                bus.attach_source(ring, MIC_FEED_ID, gain);
                log::line(&format!("route on: 麦克风 -> line {line_id}"));
            }
            // Normally the recorder is already running from the selection itself; this covers a mic that
            // was selected while its capture could not open.
            if reopen {
                let mut world = self.world();
                self.open_mic_capture(&mut world)?;
            }
            return Ok(());
        }
        world.mic.lines.retain(|id| *id != line_id);
        drop(world);
        if attached {
            bus.detach_source(MIC_FEED_ID);
            log::line(&format!("route off: 麦克风 -> line {line_id}"));
        }
        Ok(())
    }

    /// A USB or wireless microphone can vanish without the user changing the selection. Keep its identity,
    /// look for it again, and reopen capture once it is enumerated; the buses still hold the mic ring, so
    /// nothing else has to be rewired.
    fn reopen_mic_if_lost(&self) {
        let suspect = {
            let world = self.world();
            (!world.mic.device_id.is_empty() || !world.mic.name.is_empty())
                && (world.mic.lost
                    || world.mic.capture.is_none()
                    || world
                        .mic
                        .capture
                        .as_ref()
                        .map(|capture| !capture.is_alive())
                        .unwrap_or(false))
        };
        if !suspect {
            return;
        }
        // The endpoint list is a COM walk of every capture device, so it runs outside the world lock:
        // while a microphone stays missing, the UI's 30 ms snapshot would queue up behind each lookup.
        let mut world = self.world();
        if world.mic.device_id.is_empty() && world.mic.name.is_empty() {
            return;
        }
        let wanted_id = world.mic.device_id.clone();
        let wanted_name = world.mic.name.clone();
        self.close_mic(&mut world);
        drop(world);
        let found = match platform::list_endpoints(Direction::Capture) {
            Ok(endpoints) => endpoints
                .iter()
                .find(|endpoint| endpoint.id == wanted_id)
                .or_else(|| endpoints.iter().find(|endpoint| endpoint.name == wanted_name))
                .cloned(),
            Err(_) => None,
        };
        let mut world = self.world();
        // The selection is only ours to restore while nobody else has moved it.
        let moved = world.mic.device_id != wanted_id || world.mic.name != wanted_name;
        if moved || world.mic.capture.is_some() {
            return;
        }
        let Some(endpoint) = found else {
            // Still gone; the watchdog looks again in two seconds.
            world.mic.lost = true;
            return;
        };
        world.mic.device_id = endpoint.id.clone();
        world.mic.name = endpoint.name.clone();
        match self.open_mic_capture(&mut world) {
            Ok(()) => {
                drop(world);
                log::line(&format!("mic reconnected: '{}'", endpoint.name));
            }
            Err(err) => {
                drop(world);
                log::line(&format!("mic reconnect failed: {err}"));
            }
        }
    }

    // --- add-source previews -----------------------------------------------------------------
    // While the add list is open, every candidate app gets its own process loopback so the meters are real
    // captured audio. They are throwaway: no ring, no routes, disposed when the list closes.

    pub fn start_previews(&self, candidates: &[(u32, String, String)]) {
        let mut world = self.world();
        let live_pids: Vec<u32> = world
            .wants
            .iter()
            .filter_map(|want| want.live.as_ref().map(|feed| feed.pid))
            .collect();
        world.preview_lease_ns = self.clock.now_ns() + PREVIEW_LEASE_NS;
        let wanted: Vec<u32> = candidates.iter().map(|(pid, _, _)| *pid).collect();
        let stale: Vec<u32> = world
            .previews
            .values()
            .filter(|preview| !wanted.contains(&preview.pid) || live_pids.contains(&preview.pid))
            .map(|preview| preview.pid)
            .collect();
        for pid in stale {
            if let Some(mut preview) = world.previews.remove(&pid) {
                if let Some(mut capture) = preview.capture.take() {
                    capture.stop();
                }
            }
        }
        let mut to_start: Vec<(u32, String, Arc<LiveMeter>)> = Vec::new();
        for (pid, name, _title) in candidates {
            if *pid == 0 || live_pids.contains(pid) {
                // Already-added apps are captured by their real source; reuse that meter instead of
                // opening a second loopback on the same process.
                continue;
            }
            match world.previews.get_mut(pid) {
                Some(existing) => {
                    existing.name = name.clone();
                    existing.error = String::new();
                }
                None => {
                    let meter = LiveMeter::new(self.clock.clone());
                    world.previews.insert(
                        *pid,
                        Preview {
                            pid: *pid,
                            name: name.clone(),
                            meter: meter.clone(),
                            capture: None,
                            error: String::new(),
                        },
                    );
                    to_start.push((*pid, name.clone(), meter));
                }
            }
        }
        drop(world);

        for (pid, name, meter) in to_start {
            let started = Capture::start(
                Source::Process {
                    pid,
                    include_tree: true,
                },
                capture_options(),
                meter_only_callback(meter),
            );
            let mut world = self.world();
            match started {
                Ok(capture) => {
                    if let Some(preview) = world.previews.get_mut(&pid) {
                        preview.capture = Some(capture);
                    }
                }
                Err(err) => {
                    if let Some(preview) = world.previews.get_mut(&pid) {
                        preview.error = err.clone();
                    }
                    drop(world);
                    log::line(&format!("preview failed: pid={pid} ({name}): {err}"));
                }
            }
        }
    }

    fn stop_preview_locked(&self, world: &mut World, pid: u32) {
        if let Some(mut preview) = world.previews.remove(&pid) {
            if let Some(mut capture) = preview.capture.take() {
                capture.stop();
            }
        }
    }

    pub fn stop_previews(&self) {
        let mut world = self.world();
        let count = world.previews.len();
        if count == 0 {
            return;
        }
        for (_pid, preview) in world.previews.iter_mut() {
            if let Some(mut capture) = preview.capture.take() {
                capture.stop();
            }
        }
        world.previews.clear();
        drop(world);
        log::line(&format!("previews stopped ({count})"));
    }

    // --- watchdog ----------------------------------------------------------------------------

    /// One pass. Everything here is a recovery path: a capture that died, a process that exited, an
    /// endpoint that was pulled, a preview lease that ran out.
    fn monitor_tick(&self) {
        {
            let mut world = self.world();
            let dead: Vec<(u32, String, &'static str)> = world
                .wants
                .iter()
                .filter_map(|want| {
                    let feed = want.live.as_ref()?;
                    if !feed.capture.is_alive() {
                        return Some((want.id, want.identity.name.clone(), "capture stopped"));
                    }
                    if !platform::process_alive(feed.pid) {
                        return Some((want.id, want.identity.name.clone(), "process exited"));
                    }
                    None
                })
                .collect();
            for (want_id, name, reason) in dead {
                self.detach_feed(&mut world, want_id, reason, &name);
            }

            // A sink that stopped on its own is an output device that was reset, removed or taken
            // exclusively. Handing that back to the user as a fault they have to notice is the wrong
            // default, so the watchdog below replays the restart on its own.
            for line in world.lines.iter_mut() {
                if let Some(sink) = &line.sink {
                    if line.state == "running" && !sink.is_alive() {
                        let delivery = sink.delivery();
                        let cause = match delivery.endpoint_reset {
                            Some(code) => {
                                format!("输出端点已被系统重置（HRESULT 0x{code:08X}）")
                            }
                            None if delivery.error.is_empty() => {
                                "输出已停止（设备被移除或被独占？）".to_string()
                            }
                            None => delivery.error.clone(),
                        };
                        line.state = "faulted".to_string();
                        line.error = format!("{cause}，正在自动重连");
                        line.reconnecting = true;
                        line.reconnect_tries = 0;
                        line.reconnect_notified = false;
                        log::line(&format!("line {} stopped: {}", line.id, line.error));
                    }
                }
            }

            let previews_expired =
                !world.previews.is_empty() && self.clock.now_ns() > world.preview_lease_ns;
            let mic_may_be_gone = !world.mic.device_id.is_empty() || !world.mic.name.is_empty();
            drop(world);

            let tick = self.tick.fetch_add(1, Ordering::Relaxed);
            // Lines are watched on every tick rather than on the two-second scan: a device the user just
            // plugged back in should not have to wait for the source scan to notice it.
            self.reconnect_lines();
            // Two seconds is fast enough to feel instant when an app starts, and the scan is free unless a
            // source is actually waiting for its process.
            if tick % 4 == 0 {
                self.reconnect_sources();
                if mic_may_be_gone {
                    self.reopen_mic_if_lost();
                }
            }
            if previews_expired {
                self.stop_previews();
            }
        }
    }

    /// Tries to attach every remembered source that is not running yet.
    ///
    /// The scan runs outside the world lock on purpose. `session_pids` walks every active render
    /// endpoint's audio sessions over COM and `processes_named` snapshots the whole process list; with one
    /// remembered source waiting for its app, that pair costs ~350 ms and it used to be paid holding the
    /// lock — so every 2 s the UI's 30 ms snapshot queued up behind it and the meters visibly froze.
    pub fn reconnect_sources(&self) {
        let pending: Vec<(u32, Identity)> = {
            let world = self.world();
            world
                .wants
                .iter()
                .filter(|want| want.live.is_none())
                .map(|want| (want.id, want.identity.clone()))
                .collect()
        };
        if pending.is_empty() {
            return;
        }
        let sessions = platform::session_pids();
        let found: Vec<(u32, Identity, u32)> = pending
            .into_iter()
            .filter_map(|(want_id, identity)| {
                let candidates = platform::processes_named(identity.name.trim());
                pick_candidate(&candidates, &identity, &sessions).map(|pid| (want_id, identity, pid))
            })
            .collect();
        if found.is_empty() {
            return;
        }
        let mut world = self.world();
        for (want_id, identity, pid) in found {
            // The user may have removed it, or another path may have attached it, while we were scanning.
            let still_pending = world
                .wants
                .iter()
                .any(|want| want.id == want_id && want.live.is_none());
            if !still_pending {
                continue;
            }
            match self.attach_feed(&mut world, want_id, pid) {
                Ok(()) => log::line(&format!(
                    "source reconnected: '{}' pid={pid}",
                    identity.name
                )),
                Err(err) => log::line(&format!("reconnect '{}' failed: {err}", identity.name)),
            }
        }
    }

    /// Reopens the lines whose endpoint died under them, the 重启 click on a timer. A missing endpoint is
    /// waited for indefinitely; one that is present but keeps refusing to start is dropped after
    /// `MAX_RECONNECT_TRIES`.
    fn reconnect_lines(&self) {
        let pending: Vec<(u32, String, String)> = {
            let world = self.world();
            world
                .lines
                .iter()
                .filter(|line| line.reconnecting)
                .map(|line| (line.id, line.device_id.clone(), line.name.clone()))
                .collect()
        };
        if pending.is_empty() {
            return;
        }
        let listed: Vec<(String, String)> = match platform::list_endpoints(Direction::Render) {
            Ok(endpoints) => endpoints
                .iter()
                .map(|endpoint| (endpoint.id.clone(), endpoint.name.clone()))
                .collect(),
            // A failed enumeration is not evidence the endpoint is gone. Leave the list empty and let the
            // reopen itself decide, so one hiccup cannot park a line until the next tick.
            Err(_) => Vec::new(),
        };
        for (line_id, device_id, name) in pending {
            let present = listed.is_empty()
                || listed.iter().any(|(id, listed_name)| *id == device_id || *listed_name == name);
            if !present {
                let first = self
                    .world()
                    .lines
                    .iter_mut()
                    .find(|line| line.id == line_id)
                    .map(|line| {
                        let first = !line.reconnect_notified;
                        line.reconnect_notified = true;
                        first
                    })
                    .unwrap_or(false);
                if first {
                    log::line(&format!(
                        "line {line_id} waiting: '{name}' is not enumerated; will reopen it when it is back"
                    ));
                }
                continue;
            }
            match self.restart_line(line_id) {
                Ok(()) => {
                    let facts = self
                        .world()
                        .lines
                        .iter()
                        .find(|line| line.id == line_id)
                        .and_then(|line| line.sink.as_ref())
                        .map(|sink| sink.facts());
                    match facts {
                        Some(facts) => log::line(&format!(
                            "line {line_id} reconnected -> '{name}' granted={:.1}ms lowLatency={}",
                            facts.granted_period_ms, facts.low_latency_active
                        )),
                        None => log::line(&format!("line {line_id} reconnected -> '{name}'")),
                    }
                }
                Err(err) => {
                    let mut world = self.world();
                    let Some(line) = world.lines.iter_mut().find(|line| line.id == line_id) else {
                        continue;
                    };
                    line.reconnect_tries += 1;
                    if line.reconnect_tries < MAX_RECONNECT_TRIES {
                        continue;
                    }
                    line.reconnecting = false;
                    line.error = format!("自动重连已放弃（{} 次）：{err}", line.reconnect_tries);
                    log::line(&format!("line {line_id} reconnect abandoned: {}", line.error));
                }
            }
        }
    }

    // --- shutdown ----------------------------------------------------------------------------

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
        let mut world = self.world();
        for (_pid, preview) in world.previews.iter_mut() {
            if let Some(mut capture) = preview.capture.take() {
                capture.stop();
            }
        }
        world.previews.clear();
        let ids: Vec<u32> = world.wants.iter().map(|want| want.id).collect();
        for id in ids {
            self.detach_feed(&mut world, id, "", "");
        }
        let line_ids: Vec<u32> = world.lines.iter().map(|line| line.id).collect();
        for id in line_ids {
            if let Some(index) = world.lines.iter().position(|line| line.id == id) {
                world.lines.remove(index).stop_sink();
            }
        }
        self.close_mic(&mut world);
        world.mic.lines.clear();
        world.error = String::new();
        drop(world);
        if let Some(handle) = self.monitor.lock().unwrap().take() {
            let _ = handle.join();
        }
        log::line("engine stopped");
    }

    /// Test-only: ids of the lines currently held.
    pub fn line_ids(&self) -> Vec<u32> {
        self.world().lines.iter().map(|line| line.id).collect()
    }
}

fn clamp_gain(gain: f32) -> f32 {
    gain.clamp(0.0, MAX_GAIN)
}

/// What every line's microphone slot should actually receive: the user's gain, or zero while muted.
fn mic_gain_for(mic: &Mic) -> f32 {
    if mic.muted {
        0.0
    } else {
        mic.gain
    }
}

/// Which of the same-named processes is the one the user routed: prefer the one holding an audio session,
/// then the earliest started; a stored path is a stronger claim than a name, but is only used when both
/// sides can read it.
fn pick_candidate(
    candidates: &[platform::ProcessFacts],
    identity: &Identity,
    sessions: &[u32],
) -> Option<u32> {
    let matched: Vec<u32> = candidates
        .iter()
        .filter(|candidate| {
            identity.path.is_empty()
                || candidate.path.is_empty()
                || candidate.path.eq_ignore_ascii_case(&identity.path)
        })
        .map(|candidate| candidate.pid)
        .collect();
    if matched.is_empty() {
        return None;
    }
    if matched.len() == 1 {
        return matched.first().copied();
    }
    let with_signal: Vec<u32> = matched
        .iter()
        .copied()
        .filter(|pid| sessions.contains(pid))
        .collect();
    if !with_signal.is_empty() {
        return with_signal.first().copied();
    }
    matched.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_master_switch_toggles_and_reaches_the_snapshot() {
        let engine = Engine::start(Arc::new(crate::engine::SystemClock));
        assert!(!engine.snapshot().mic_muted, "a new engine is open");
        assert!(engine.toggle_mic_muted(), "the first toggle mutes");
        assert!(engine.snapshot().mic_muted);
        assert!(!engine.toggle_mic_muted(), "the second toggle opens it again");
        assert!(!engine.snapshot().mic_muted);

        // The setter is idempotent and the snapshot gives the same answer.
        engine.set_mic_muted(true);
        engine.set_mic_muted(true);
        assert!(engine.snapshot().mic_muted);
        engine.stop();
    }
}

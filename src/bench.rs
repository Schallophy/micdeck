//! Local diagnostics: enumerate endpoints, capture a stream to WAV, render a test tone.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::cli::Command;
use crate::engine::status::{EngineStatus, LineInfo, RouteInfo};
use crate::engine::{Engine, SystemClock};
use crate::log::line;
use crate::platform::{self, Capture, Delivery, Direction, Endpoint, Source, StreamFacts, StreamOptions};
use crate::util::wav::WavWriter;

const EXIT_OK: i32 = 0;
const EXIT_BAD_ARGS: i32 = 1;
const EXIT_DEVICE_OR_PID: i32 = 3;

pub fn entry() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command = match crate::cli::parse(&args) {
        Ok(command) => command,
        Err(message) => {
            // Neither of these may panic: a tray build has no console attached, and a failed write to
            // stderr would take the process down instead of printing nothing.
            let mut err = std::io::stderr();
            if !message.is_empty() {
                let _ = writeln!(err, "ERROR: {message}");
            }
            let _ = writeln!(err, "{}", crate::cli::usage_text());
            return std::process::ExitCode::from(if message.is_empty() {
                EXIT_OK as u8
            } else {
                EXIT_BAD_ARGS as u8
            });
        }
    };
    line(&format!("self pid={}", std::process::id()));
    let code = run(command);
    if code != EXIT_OK {
        line(&format!("RESULT=FAIL code={code}"));
    }
    std::process::ExitCode::from(code as u8)
}

fn run(command: Command) -> i32 {
    match command {
        Command::ListDevices => list_devices(),
        Command::ListSessions { seconds } => list_sessions(seconds),
        Command::EngineSmoke {
            lines,
            pid,
            mic_device,
            seconds,
        } => engine_smoke(&lines, pid, mic_device.as_deref(), seconds),
        Command::DumpIcon { target, out } => dump_icon(&target, &out),
        Command::CapturePid {
            pid,
            out,
            seconds,
            include_tree,
        } => {
            line(&format!(
                "process loopback: pid={pid} tree={include_tree} seconds={seconds:.1} out={out}"
            ));
            capture(
                Source::Process {
                    pid,
                    include_tree,
                },
                StreamOptions::default(),
                &out,
                seconds,
            )
        }
        Command::CaptureMic {
            device,
            out,
            seconds,
            low_latency,
        } => match resolve(Direction::Capture, device.as_deref()) {
            Ok(endpoint) => {
                line(&format!("microphone: '{}' out={out}", endpoint.name));
                capture(
                    Source::Microphone(endpoint),
                    StreamOptions {
                        low_latency,
                        ..StreamOptions::default()
                    },
                    &out,
                    seconds,
                )
            }
            Err(err) => {
                line(&err);
                EXIT_DEVICE_OR_PID
            }
        },
        Command::CaptureLoopback {
            device,
            out,
            seconds,
        } => match resolve(Direction::Render, device.as_deref()) {
            Ok(endpoint) => {
                line(&format!("render loopback: '{}' out={out}", endpoint.name));
                capture(
                    Source::RenderLoopback(endpoint),
                    StreamOptions::default(),
                    &out,
                    seconds,
                )
            }
            Err(err) => {
                line(&err);
                EXIT_DEVICE_OR_PID
            }
        },
        Command::RenderTone {
            device,
            seconds,
            hz,
            level,
            low_latency,
            buffer_ms,
            capture_back,
        } => {
            let endpoint = match resolve(Direction::Render, device.as_deref()) {
                Ok(endpoint) => endpoint,
                Err(err) => {
                    line(&err);
                    return EXIT_DEVICE_OR_PID;
                }
            };
            render_tone(
                &endpoint,
                seconds,
                hz,
                level,
                low_latency,
                buffer_ms,
                capture_back,
            )
        }
    }
}

fn list_sessions(seconds: f64) -> i32 {
    match platform::render_sessions(seconds) {
        Ok(rows) => {
            line(&format!(
                "--- render sessions (peak over {seconds:.1} s) rows={} ---",
                rows.len()
            ));
            for row in &rows {
                line(&format!(
                    "  pid={:<7} name={:<26} peak={:.4} display={}",
                    row.pid,
                    if row.name.is_empty() {
                        "(unknown)"
                    } else {
                        &row.name
                    },
                    row.peak,
                    row.display
                ));
            }
            if rows.iter().all(|row| row.peak <= 0.0) {
                line("nothing is producing audio right now");
            }
            EXIT_OK
        }
        Err(err) => {
            line(&format!("session scan failed: {err}"));
            EXIT_DEVICE_OR_PID
        }
    }
}

fn resolve(dir: Direction, selector: Option<&str>) -> Result<Endpoint, String> {
    match selector {
        Some(text) => platform::find_endpoint(dir, text),
        None => platform::win_default(dir),
    }
}

fn list_devices() -> i32 {
    for dir in [Direction::Render, Direction::Capture] {
        match platform::list_endpoints(dir) {
            Ok(endpoints) => {
                line(&format!(
                    "--- {} endpoints ({} active) ---",
                    platform::describe(dir),
                    endpoints.len()
                ));
                for endpoint in &endpoints {
                    let probe = platform::win_probe(&endpoint.id);
                    line(&format!("  name={}", endpoint.name));
                    line(&format!("      id={}", endpoint.id));
                    line(&format!("      default={} mixFormat={}", endpoint.is_default, probe.mix_format));
                    match probe.engine_period_frames {
                        Some((default, fundamental, min, max)) => line(&format!(
                            "      enginePeriodFrames=default {default} ({:.2} ms) fundamental {fundamental} min {min} max {max}",
                            default as f64 * 1000.0 / 48000.0
                        )),
                        None => line(&format!(
                            "      enginePeriodFrames=IAudioClient3 unavailable{}",
                            if probe.error.is_empty() {
                                String::new()
                            } else {
                                format!(" ({})", probe.error)
                            }
                        )),
                    }
                    if let Some(nearest) = probe.nearest_float_format {
                        line(&format!(
                            "      48k/2ch/float=needs conversion, nearest {nearest}"
                        ));
                    } else {
                        line(&format!("      48k/2ch/float=exact"));
                    }
                }
            }
            Err(err) => {
                line(&format!("enumerate {} failed: {err}", platform::describe(dir)));
                return EXIT_DEVICE_OR_PID;
            }
        }
    }
    EXIT_OK
}

/// Level bookkeeping for one captured stream, kept beside the file so a log line and the WAV agree.
#[derive(Default)]
struct Tap {
    writer: Option<WavWriter>,
    peak: f32,
    sum: f64,
    squares: f64,
    frames: u64,
    packets: u64,
    first_packet_at: Option<Instant>,
    last_packet_at: Option<Instant>,
}

impl Tap {
    fn push(&mut self, frames: &[f32]) {
        if let Some(writer) = self.writer.as_mut() {
            let _ = writer.write_f32(frames);
        }
        let mut peak = self.peak;
        let mut sum = 0.0f64;
        let mut squares = 0.0f64;
        for sample in frames.iter().step_by(2) {
            let value = sample.abs();
            if value > peak {
                peak = value;
            }
            sum += f64::from(*sample);
            squares += f64::from(*sample) * f64::from(*sample);
        }
        self.peak = peak;
        self.sum += sum;
        self.squares += squares;
        self.frames += (frames.len() / 2) as u64;
        self.packets += 1;
        let now = Instant::now();
        if self.first_packet_at.is_none() {
            self.first_packet_at = Some(now);
        }
        self.last_packet_at = Some(now);
    }
}

fn capture(source: Source, options: StreamOptions, out: &str, seconds: f64) -> i32 {
    let tap = Arc::new(Mutex::new(Tap {
        writer: match WavWriter::create(out, 48000, 2, true) {
            Ok(writer) => Some(writer),
            Err(err) => {
                line(&format!("cannot open {out}: {err}"));
                return EXIT_BAD_ARGS;
            }
        },
        ..Tap::default()
    }));
    let sink = tap.clone();
    let on: platform::OnFrames = Box::new(move |frames: &[f32]| {
        sink.lock().unwrap().push(frames);
    });
    let mut capture = match Capture::start(source, options, on) {
        Ok(capture) => capture,
        Err(err) => {
            line(&format!("capture failed: {err}"));
            return EXIT_DEVICE_OR_PID;
        }
    };
    report_facts("capture", &capture.facts());
    std::thread::sleep(Duration::from_secs_f64(seconds));
    let delivery = capture.delivery();
    let mut tap = tap.lock().unwrap();
    let span = match (tap.first_packet_at, tap.last_packet_at) {
        (Some(start), Some(end)) => end.duration_since(start).as_secs_f64(),
        _ => 0.0,
    };
    let rms = if tap.frames > 0 {
        (tap.squares / tap.frames as f64).sqrt()
    } else {
        0.0
    };
    let written = tap
        .writer
        .as_ref()
        .map(|writer| writer.seconds())
        .unwrap_or(0.0);
    line(&format!(
        "captured: {:.2} s of arrivals, {:.2} s written, packets={} frames={} packetRange={}..{} maxGap={:.1} ms",
        span,
        written,
        tap.packets,
        tap.frames,
        delivery.min_packet_frames,
        delivery.max_packet_frames,
        delivery.max_gap_ms
    ));
    line(&format!(
        "level: peak={:.4} rms={:.4} silentPackets={} discontinuity={}",
        tap.peak, rms, delivery.silent_packets, delivery.discontinuity
    ));
    if !delivery.error.is_empty() {
        line(&format!("capture error: {}", delivery.error));
    }
    let alive = capture.is_alive();
    capture.stop();
    tap.writer = None;
    drop(tap);
    if delivery.frames == 0 {
        line("no frames arrived");
        return 9;
    }
    if !delivery.error.is_empty() || !alive {
        return 9;
    }
    line("RESULT=OK");
    EXIT_OK
}

fn report_facts(label: &str, facts: &StreamFacts) {
    line(&format!(
        "{label}: '{}' {} granted={} buffer={} frames lowLatency={}{}",
        facts.endpoint,
        facts.format,
        if facts.granted_period_ms > 0.0 {
            format!("{:.2} ms", facts.granted_period_ms)
        } else {
            "n/a".to_string()
        },
        facts.buffer_frames,
        facts.low_latency_active,
        if facts.low_latency_reason.is_empty() {
            String::new()
        } else {
            format!(" reason={}", facts.low_latency_reason)
        }
    ));
}

fn report_delivery(label: &str, delivery: &Delivery) {
    line(&format!(
        "{label} blocks={} frames={} blockRange={}..{} maxGap={:.2} ms",
        delivery.packets,
        delivery.frames,
        delivery.min_packet_frames,
        delivery.max_packet_frames,
        delivery.max_gap_ms
    ));
}

/// Renders a sine to an endpoint on the device clock, and optionally captures the same endpoint back so
/// the tone can be measured, not just assumed. This is the part of the product that has to work before
/// any mixing exists.
fn render_tone(
    endpoint: &Endpoint,
    seconds: f64,
    hz: f64,
    level: f64,
    low_latency: bool,
    buffer_ms: u32,
    capture_back: bool,
) -> i32 {
    let options = StreamOptions {
        buffer_ms,
        low_latency,
        mmcss: true,
    };
    line(&format!(
        "render tone: '{}' {hz:.0} Hz level={level:.2} seconds={seconds:.1} bufferMs={buffer_ms} lowLatency={low_latency}",
        endpoint.name
    ));

    let back = if capture_back {
        let tap = Arc::new(Mutex::new(Tap {
            writer: match WavWriter::create("micdeck-tone-back.wav", 48000, 2, true) {
                Ok(writer) => Some(writer),
                Err(err) => {
                    line(&format!("cannot write micdeck-tone-back.wav: {err}"));
                    return EXIT_BAD_ARGS;
                }
            },
            ..Tap::default()
        }));
        let sink = tap.clone();
        let on: platform::OnFrames = Box::new(move |frames: &[f32]| {
            sink.lock().unwrap().push(frames);
        });
        match Capture::start(
            Source::RenderLoopback(endpoint.clone()),
            StreamOptions::default(),
            on,
        ) {
            Ok(capture) => Some((tap, capture)),
            Err(err) => {
                line(&format!("loopback of the same endpoint failed: {err}"));
                None
            }
        }
    } else {
        None
    };

    let mut phase = 0.0f64;
    let step = 2.0 * std::f64::consts::PI * hz / 48000.0;
    let amplitude = level as f32;
    let fill: platform::FillBlock = Box::new(move |buf: &mut [f32]| {
        for frame in buf.chunks_exact_mut(2) {
            let value = (phase.sin() * f64::from(amplitude)) as f32;
            phase += step;
            if phase > 2.0 * std::f64::consts::PI {
                phase -= 2.0 * std::f64::consts::PI;
            }
            frame[0] = value;
            frame[1] = value;
        }
    });

    let mut sink = match platform::Sink::start(endpoint, options, fill) {
        Ok(sink) => sink,
        Err(err) => {
            line(&format!("render failed: {err}"));
            return EXIT_DEVICE_OR_PID;
        }
    };
    report_facts("sink", &sink.facts());

    // Poll the queue depth while the tone runs: the distribution is the evidence that the render path
    // is sitting at one engine period, not drifting or draining.
    let mut queue: Vec<f64> = Vec::new();
    let started = Instant::now();
    let deadline = started + Duration::from_secs_f64(seconds);
    while Instant::now() < deadline {
        queue.push(sink.queue_ms());
        std::thread::sleep(Duration::from_millis(10));
    }
    queue.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if !queue.is_empty() {
        let counts = quantize(&queue);
        line(&format!(
            "queue: min={:.2} ms median={:.2} ms max={:.2} ms samples={} values={}",
            queue[0],
            queue[queue.len() / 2],
            queue[queue.len() - 1],
            queue.len(),
            counts
        ));
    }
    let alive = sink.is_alive();
    report_delivery("sink", &sink.delivery());
    let delivery = sink.delivery();
    sink.stop();
    if !alive {
        line("sink thread stopped on its own");
        return 9;
    }

    if let Some((tap, mut capture)) = back {
        let capture_delivery = capture.delivery();
        report_delivery("loopback", &capture_delivery);
        let tap = tap.lock().unwrap();
        line(&format!(
            "loopback level: peak={:.4} frames={} (file micdeck-tone-back.wav)",
            tap.peak, tap.frames
        ));
        capture.stop();
        if tap.frames == 0 || capture_delivery.error != delivery.error {
            if !capture_delivery.error.is_empty() {
                line(&format!("loopback error: {}", capture_delivery.error));
            }
            return 9;
        }
    }
    if !delivery.error.is_empty() {
        line(&format!("sink error: {}", delivery.error));
        return 9;
    }
    line("RESULT=OK");
    EXIT_OK
}

/// The icon the UI would receive for one process — or the application's own mark — written to a file.
/// The candidate enumeration runs first because that walk is what teaches the cache which window belongs
/// to which pid: the same order the page reaches it in, and the only way the window fallback can be
/// exercised from the command line.
fn dump_icon(target: &str, out: &str) -> i32 {
    let (mime, bytes, path) = if target == "brand" {
        (
            "image/x-icon",
            crate::platform::brand::ico(),
            "brand.ico".to_string(),
        )
    } else {
        let pid = target.parse::<u32>().unwrap_or(0);
        let rows = platform::list_window_candidates();
        let listed = rows.iter().any(|row| row.pid == pid);
        let icon = platform::icon_for_pid(pid);
        line(&format!(
            "icon source: pid={pid} window={}",
            if listed { "listed" } else { "none" }
        ));
        (icon.mime, icon.bytes, format!("icon-{pid}.ico"))
    };
    let path = if out.is_empty() { path } else { out.to_string() };
    if let Err(err) = std::fs::write(&path, &bytes) {
        line(&format!("icon write failed for {path}: {err}"));
        return EXIT_BAD_ARGS;
    }
    line(&format!(
        "icon: target={target} mime={} bytes={} written={}",
        mime,
        bytes.len(),
        path
    ));
    EXIT_OK
}

/// How the queue depth reading was distributed, as fractions. WASAPI reports padding in whole engine
/// periods, so a healthy line shows up as one dominant value rather than a smear.
fn quantize(sorted: &[f64]) -> String {
    let mut out = String::new();
    let mut index = 0;
    while index < sorted.len() {
        let value = sorted[index];
        let mut run = 1;
        while index + run < sorted.len() && (sorted[index + run] - value).abs() < 0.01 {
            run += 1;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(&format!(
            "{:.2} ms x{} ({:.1}%)",
            value,
            run,
            run as f64 * 100.0 / sorted.len() as f64
        ));
        index += run;
    }
    out
}

/// Runs the real engine against one output line, with an optional process source and microphone, and
/// reports whether the loop the UI depends on holds: the ring level stays at its target, the resampler
/// cancels the clock difference instead of draining or overflowing, and nobody starves.
///
/// Counters are cumulative from engine start, and the first seconds of any line are legitimate
/// bookkeeping (prebuffer, prime, the watchdog seeing a cold capture), so the verdict is taken from the
/// difference across a steady window, not from the totals.
fn engine_smoke(
    selectors: &[String],
    pid: Option<u32>,
    mic_device: Option<&str>,
    seconds: f64,
) -> i32 {
    let engine = Engine::start(Arc::new(SystemClock));
    let mut faults: Vec<String> = Vec::new();
    let mut line_ids: Vec<u32> = Vec::new();
    for selector in selectors {
        match engine.add_line(selector) {
            Ok(id) => line_ids.push(id),
            Err(err) => faults.push(format!("add line '{selector}': {err}")),
        }
    }
    if line_ids.is_empty() {
        for fault in &faults {
            line(&format!("setup: {fault}"));
        }
        engine.stop();
        return EXIT_DEVICE_OR_PID;
    }
    // Everything the user asked to route goes on the first line; a second line with no feeds is how two
    // lines are proven not to drag each other down.
    let primary = line_ids[0];

    if let Some(pid) = pid {
        match engine.add_app_source(pid, "") {
            Ok(source_id) => {
                if let Err(err) = engine.set_app_route(source_id, primary, true) {
                    faults.push(format!("route source {source_id}: {err}"));
                }
            }
            Err(err) => faults.push(format!("add source pid={pid}: {err}")),
        }
    }
    if let Some(device) = mic_device {
        if let Err(err) = engine.set_mic(device, 1.0) {
            faults.push(format!("open mic: {err}"));
        } else if let Err(err) = engine.set_mic_route(primary, true) {
            faults.push(format!("route mic: {err}"));
        }
    }
    for fault in &faults {
        line(&format!("setup: {fault}"));
    }

    let warmup = (seconds / 3.0).min(5.0).max(1.0);
    line(&format!(
        "engine smoke: lines={line_ids:?} pid={} mic={} seconds={seconds:.1} warmup={warmup:.1}",
        pid.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string()),
        if mic_device.is_none() { "-" } else { "on" }
    ));
    std::thread::sleep(Duration::from_secs_f64(warmup));
    let base = engine.snapshot();
    report_status("warm", &base);
    let poll = 2.0f64;
    let mut elapsed = warmup;

    while elapsed < seconds {
        let step = poll.min(seconds - elapsed);
        std::thread::sleep(Duration::from_secs_f64(step));
        elapsed += step;
        report_status(&format!("t+{elapsed:.0}s"), &engine.snapshot());
    }
    let end = engine.snapshot();
    report_status("final", &end);

    let mut verdict: Vec<String> = Vec::new();
    if !faults.is_empty() {
        verdict.push(format!("setup errors={}", faults.len()));
    }
    if end.state != "running" {
        verdict.push(format!("state={}", end.state));
    }
    for info in &end.lines {
        if info.state != "running" {
            verdict.push(format!("line {} state={}", info.id, info.state));
        }
    }
    let base_line = |id: u32| -> LineInfo {
        base.lines
            .iter()
            .find(|l| l.id == id)
            .cloned()
            .unwrap_or_default()
    };
    let routes = end.all_routes();
    for info in &end.lines {
        let before = base_line(info.id);
        let starved = info.starved_frames.saturating_sub(before.starved_frames);
        let clipped = info.clipped_samples.saturating_sub(before.clipped_samples);
        let backstops = info.mix_backstops.saturating_sub(before.mix_backstops);
        if starved > 0 {
            verdict.push(format!("line {} starved+{starved}", info.id));
        }
        if backstops > 0 {
            verdict.push(format!("line {} backstops+{backstops}", info.id));
        }
        // Clipping depends on what the user played, so it is reported, not failed: a source at full
        // scale SHOULD clip and that is the mixer working, not breaking.
        if clipped > 0 {
            line(&format!(
                "note: line {} clipped+{clipped} (input above full scale)",
                info.id
            ));
        }
        for route in routes.iter().filter(|route| route.line_id == info.id) {
            let ppm = route.ratio_ppm.abs();
            if ppm > 300 {
                verdict.push(format!(
                    "line {} feed {} ratioPpm={} exceeds 300",
                    info.id, route.feed, route.ratio_ppm
                ));
            }
            if !(8.0..=24.0).contains(&route.buffered_ms) {
                verdict.push(format!(
                    "line {} feed {} bufferedMs={:.1} outside 8..24",
                    info.id, route.feed, route.buffered_ms
                ));
            }
        }
    }
    if pid.is_some() && end.sources.iter().any(|source| !source.alive) {
        verdict.push("source not attached".to_string());
    }
    if mic_device.is_some() && end.mic_routes.is_empty() {
        verdict.push("microphone not routed".to_string());
    }

    engine.stop();
    if verdict.is_empty() {
        line("RESULT=OK");
        return EXIT_OK;
    }
    for reason in &verdict {
        line(&format!("FAIL: {reason}"));
    }
    9
}

fn report_status(label: &str, status: &EngineStatus) {
    line(&format!(
        "{label}: state={} starvedTotal={} mic='{}' level={:.3} sources={} previews={}",
        status.state,
        status.starved_frames,
        status.mic_name,
        status.mic_level,
        status.sources.len(),
        status.previews.len()
    ));
    if !status.error.is_empty() {
        line(&format!("{label}: error={}", status.error));
    }
    for info in &status.lines {
        line(&format!(
            "{label}:   line {} '{}' state={} queue={:.2} ms grant={:.2} ms lowLat={} level={:.3} starved={} clipped={} backstops={} feeds={:?}",
            info.id,
            info.name,
            info.state,
            info.queue_ms,
            info.render_latency_ms,
            info.low_latency_active,
            info.mix_level,
            info.starved_frames,
            info.clipped_samples,
            info.mix_backstops,
            info.source_ids
        ));
        if !info.error.is_empty() {
            line(&format!("{label}:   line {} error={}", info.id, info.error));
        }
    }
    for route in status.all_routes() {
        line(&format!(
            "{label}:     feed {} line {} buffered={:.2} ms ppm={} starved={} dropped={}",
            route.feed, route.line_id, route.buffered_ms, route.ratio_ppm, route.starved_frames,
            route.dropped_frames
        ));
    }
}

impl EngineStatus {
    /// Every per-line buffer reading in one list, with the feed id it belongs to. The wire format keeps
    /// routes inside their source, which is what the UI wants; a diagnostic wants the flat view.
    fn all_routes(&self) -> Vec<FlatRoute> {
        let mut out = Vec::new();
        for route in &self.mic_routes {
            out.push(FlatRoute {
                feed: crate::engine::MIC_FEED_ID,
                line_id: route.line_id,
                info: *route,
            });
        }
        for source in &self.sources {
            for route in &source.routes {
                out.push(FlatRoute {
                    feed: source.id,
                    line_id: route.line_id,
                    info: *route,
                });
            }
        }
        out
    }
}

#[derive(Clone, Copy)]
struct FlatRoute {
    feed: u32,
    line_id: u32,
    info: RouteInfo,
}

impl std::ops::Deref for FlatRoute {
    type Target = RouteInfo;
    fn deref(&self) -> &RouteInfo {
        &self.info
    }
}

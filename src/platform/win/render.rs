use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;
use wasapi::{
    initialize_mta, AudioClient, AudioRenderClient, Direction as WasapiDirection, Handle, SampleType,
    StreamMode, TimingMode, WaveFormat,
};

use super::{devices, thread};
use crate::platform::{Delivery, Endpoint, StreamFacts, StreamOptions, CHANNELS, SAMPLE_RATE};

/// The engine fills one block of interleaved float frames. It is called with the number of frames the
/// endpoint can accept right now, capped at `MAX_BLOCK_FRAMES`, so the device clock is the only clock.
pub type FillBlock = Box<dyn FnMut(&mut [f32]) + Send + 'static>;

/// Same ceiling the mixing stage uses for one pass. A block larger than this would mean the endpoint
/// went quiet for a whole 10 ms, and handing it one huge block would hide that instead of showing it.
pub const MAX_BLOCK_FRAMES: usize = 480;

#[derive(Default, Clone)]
struct Inner {
    facts: StreamFacts,
    delivery: Delivery,
    queue_ms: f64,
}

pub struct Sink {
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    inner: Arc<Mutex<Inner>>,
    join: Option<JoinHandle<()>>,
}

impl Sink {
    /// Starts rendering to `endpoint`. Returns once the stream is running, or with the reason it is not:
    /// a line that could not be opened must never look like a line that is merely silent.
    pub fn start(endpoint: &Endpoint, options: StreamOptions, fill: FillBlock) -> Result<Sink, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let inner = Arc::new(Mutex::new(Inner {
            facts: StreamFacts {
                endpoint: endpoint.name.clone(),
                ..StreamFacts::default()
            },
            ..Inner::default()
        }));
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let (t_stop, t_alive, t_inner) = (stop.clone(), alive.clone(), inner.clone());
        let id = endpoint.id.clone();
        let name = endpoint.name.clone();
        let join = std::thread::Builder::new()
            .name("micdeck-sink".to_string())
            .spawn(move || {
                let _ = initialize_mta();
                if options.mmcss {
                    thread::enter_audio_thread();
                }
                run(id, name, options, &t_stop, &t_alive, &t_inner, tx, fill);
            })
            .map_err(|e| e.to_string())?;

        match rx.recv() {
            Ok(Ok(())) => Ok(Sink {
                stop,
                alive,
                inner,
                join: Some(join),
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err("sink thread ended before it could report".to_string()),
        }
    }

    pub fn facts(&self) -> StreamFacts {
        self.inner.lock().unwrap().facts.clone()
    }

    pub fn delivery(&self) -> Delivery {
        self.inner.lock().unwrap().delivery.clone()
    }

    /// What the endpoint still holds, read from the render thread each cycle. Sampled at the device's
    /// own granularity, so it steps by whole engine periods rather than moving smoothly.
    pub fn queue_ms(&self) -> f64 {
        self.inner.lock().unwrap().queue_ms
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.alive.store(false, Ordering::Relaxed);
        if let Some(join) = self.join.take() {
            let deadline = Instant::now() + std::time::Duration::from_millis(2000);
            while Instant::now() < deadline {
                if join.is_finished() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(2));
            }
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        self.stop();
    }
}

fn float_format() -> WaveFormat {
    WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE, CHANNELS, None)
}

struct Opened {
    client: AudioClient,
    event: Handle,
    render: AudioRenderClient,
    block_align: usize,
}

fn open(
    id: &str,
    name: &str,
    options: &StreamOptions,
) -> Result<(Opened, StreamFacts), String> {
    let format = float_format();
    let block_align = format.get_blockalign() as usize;
    let mut facts = StreamFacts {
        endpoint: name.to_string(),
        format: format!(
            "{} Hz, {} ch, 32 bit float",
            format.get_samplespersec(),
            format.get_nchannels()
        ),
        ..StreamFacts::default()
    };

    let device = devices::device_by_id(id)?;
    let mut client = device.get_iaudioclient().map_err(|e| e.to_string())?;

    let mut started = false;
    if options.low_latency {
        // The low-latency shared stream is an IAudioClient3 call that takes the engine's own period and
        // gives no format conversion, so it only works for a format the endpoint already mixes to.
        match client.get_shared_mode_engine_period(&format) {
            Ok(periods) => {
                // The engine's *minimum* shared-mode period is the low-latency choice: VB-CABLE answers 128
                // frames (2.67 ms) where the default is 480 (10 ms). A device that reports min == default
                // cannot be pushed below its own cadence, and that is what "declined" means here.
                let frames = super::lowest_latency_period(&periods);
                let granted = frames as f64 * 1000.0 / SAMPLE_RATE as f64;
                match client.initialize_shared_audio_stream(&format, TimingMode::Events, frames) {
                    Ok(()) => {
                        started = true;
                        facts.low_latency_active = true;
                        facts.granted_period_ms = granted;
                    }
                    Err(err) => facts.low_latency_reason = err.to_string(),
                }
            }
            Err(err) => facts.low_latency_reason = err.to_string(),
        }
    }

    if !started {
        if options.low_latency {
            // Retry on a fresh client: after a declined InitializeSharedAudioStream this one may not
            // accept Initialize any more.
            client = devices::device_by_id(id)?
                .get_iaudioclient()
                .map_err(|e| e.to_string())?;
        }
        let mode = StreamMode::EventsShared {
            autoconvert: true,
            buffer_duration_hns: options.buffer_ms as i64 * 10_000,
        };
        client.initialize_client(&format, &WasapiDirection::Render, &mode).map_err(|err| {
            format!(
                "render init failed: {err} (low latency: {})",
                if facts.low_latency_reason.is_empty() { "not requested" } else { &facts.low_latency_reason }
            )
        })?;
    }

    if facts.low_latency_active {
        if let Ok((_, frames)) = client.get_current_shared_mode_engine_period() {
            facts.granted_period_ms = frames as f64 * 1000.0 / SAMPLE_RATE as f64;
        }
    } else if let Ok((default_hns, _)) = client.get_device_period() {
        facts.granted_period_ms = default_hns as f64 / 10_000.0;
    }
    facts.buffer_frames = client.get_buffer_size().unwrap_or(0);
    super::grade(&client, &mut facts);

    let event = client.set_get_eventhandle().map_err(|e| e.to_string())?;
    let render = client.get_audiorenderclient().map_err(|e| e.to_string())?;
    Ok((
        Opened {
            client,
            event,
            render,
            block_align,
        },
        facts,
    ))
}

fn run(
    id: String,
    name: String,
    options: StreamOptions,
    stop: &AtomicBool,
    alive: &AtomicBool,
    inner: &Mutex<Inner>,
    ready: Sender<Result<(), String>>,
    mut fill: FillBlock,
) {
    let opened = match open(&id, &name, &options) {
        Ok((opened, facts)) => {
            inner.lock().unwrap().facts = facts;
            opened
        }
        Err(err) => {
            let mut guard = inner.lock().unwrap();
            guard.delivery.error = err.clone();
            alive.store(false, Ordering::Relaxed);
            let _ = ready.send(Err(err));
            return;
        }
    };
    let Opened {
        client,
        event,
        render,
        block_align,
    } = opened;

    // Start with a full buffer of silence. Filling it from the mixer here would consume real audio
    // before the endpoint is running, and the prefill is the only place the two are easy to tell apart.
    let prefill = client.get_available_space_in_frames().unwrap_or(0) as usize;
    if prefill > 0 {
        if let Err(err) = render.write_to_device(prefill, &vec![0u8; prefill * block_align], None) {
            let message = format!("prefill failed: {err}");
            inner.lock().unwrap().delivery.error = message.clone();
            alive.store(false, Ordering::Relaxed);
            let _ = ready.send(Err(message));
            return;
        }
    }
    if let Err(err) = client.start_stream() {
        let message = format!("start_stream failed: {err}");
        inner.lock().unwrap().delivery.error = message.clone();
        alive.store(false, Ordering::Relaxed);
        let _ = ready.send(Err(message));
        return;
    }
    let _ = ready.send(Ok(()));

    let mut frames: Vec<f32> = Vec::new();
    let mut bytes: Vec<u8> = Vec::new();
    let mut previous: Option<Instant> = None;
    let mut silence_runs: u32 = 0;
    let mut failure = String::new();
    let mut endpoint_reset: Option<u32> = None;

    while !stop.load(Ordering::Relaxed) {
        let woke = Instant::now();
        if let Err(err) = event.wait_for_event(500) {
            // A removed endpoint stops signalling the event and never reports an error code, so the only
            // way to see it is that the queue stopped draining.
            let padding = client.get_current_padding().unwrap_or(0);
            let full = padding >= client.get_buffer_size().unwrap_or(u32::MAX);
            silence_runs = if full { silence_runs + 1 } else { 0 };
            if silence_runs >= 4 {
                failure = format!("输出端点 {name} 无响应（已移除或被独占？）: {err}");
                break;
            }
            continue;
        }
        silence_runs = 0;

        let space = match client.get_available_space_in_frames() {
            Ok(space) => space as usize,
            Err(err) => {
                endpoint_reset = super::invalidated_endpoint(&err);
                failure = format!("space: {err}");
                break;
            }
        };
        let want = space.min(MAX_BLOCK_FRAMES);
        if want == 0 {
            continue;
        }
        frames.clear();
        frames.resize(want * CHANNELS, 0.0);
        fill(&mut frames);
        bytes.clear();
        bytes.resize(want * block_align, 0);
        for (sample, chunk) in frames.iter().zip(bytes.chunks_exact_mut(4)) {
            chunk.copy_from_slice(&sample.to_le_bytes());
        }
        if let Err(err) = render.write_to_device(want, &bytes, None) {
            endpoint_reset = super::invalidated_endpoint(&err);
            failure = format!("write: {err}");
            break;
        }
        let gap_ms = previous
            .map(|p| woke.duration_since(p).as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        previous = Some(woke);
        let padding = client.get_current_padding().unwrap_or(0);
        let mut guard = inner.lock().unwrap();
        guard.queue_ms = padding as f64 * 1000.0 / SAMPLE_RATE as f64;
        let delivery = &mut guard.delivery;
        if delivery.packets == 0 || (want as u32) < delivery.min_packet_frames {
            delivery.min_packet_frames = want as u32;
        }
        delivery.max_packet_frames = delivery.max_packet_frames.max(want as u32);
        delivery.packets += 1;
        delivery.frames += want as u64;
        delivery.max_gap_ms = delivery.max_gap_ms.max(gap_ms);
    }

    let _ = client.stop_stream();
    if !failure.is_empty() {
        let mut guard = inner.lock().unwrap();
        guard.delivery.error = failure;
        guard.delivery.endpoint_reset = endpoint_reset;
    }
    alive.store(false, Ordering::Relaxed);
}

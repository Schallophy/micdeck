use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;
use wasapi::{
    initialize_mta, AudioCaptureClient, AudioClient, Direction as WasapiDirection, Handle,
    SampleType, StreamMode, TimingMode, WaveFormat,
};

use super::{devices, thread};
use crate::platform::{Delivery, Endpoint, StreamFacts, StreamOptions, CHANNELS, SAMPLE_RATE};

/// Interleaved float frames as they arrive. The engine's ring and meter hang off this callback.
pub type OnFrames = Box<dyn FnMut(&[f32]) + Send + 'static>;

#[derive(Clone, Debug)]
pub enum Source {
    /// Process loopback: the target process tree's own render stream, captured without moving its
    /// output device.
    Process { pid: u32, include_tree: bool },
    /// A microphone endpoint.
    Microphone(Endpoint),
    /// Whole render endpoint as seen from the inside, the control path used by diagnostics.
    RenderLoopback(Endpoint),
}

impl Source {
    fn endpoint(&self) -> Endpoint {
        match self {
            Source::Process { pid, .. } => Endpoint {
                id: String::new(),
                name: format!("pid {pid}"),
                is_default: false,
            },
            Source::Microphone(ep) | Source::RenderLoopback(ep) => ep.clone(),
        }
    }

    /// `IAudioClient3` low latency exists for a stream on a real endpoint. A process loopback client
    /// is refused by the API, so asking for it must not be able to break the capture.
    fn can_ask_low_latency(&self) -> bool {
        !matches!(self, Source::Process { .. })
    }
}

#[derive(Default, Clone)]
struct Inner {
    facts: StreamFacts,
    delivery: Delivery,
}

pub struct Capture {
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    inner: Arc<Mutex<Inner>>,
    join: Option<JoinHandle<()>>,
}

struct Opened {
    client: AudioClient,
    event: Handle,
    capture: AudioCaptureClient,
    block_align: usize,
}

impl Capture {
    /// Starts the capture and does not return until the stream is running or has failed, so a caller
    /// gets the real reason instead of a thread that quietly never produced samples.
    pub fn start(
        source: Source,
        options: StreamOptions,
        on: OnFrames,
    ) -> Result<Capture, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let inner = Arc::new(Mutex::new(Inner {
            facts: StreamFacts {
                endpoint: source.endpoint().name,
                ..StreamFacts::default()
            },
            delivery: Delivery::default(),
        }));
        let (tx, rx) = std::sync::mpsc::channel::<Result<(), String>>();

        let (t_stop, t_alive, t_inner) = (stop.clone(), alive.clone(), inner.clone());
        let join = std::thread::Builder::new()
            .name("micdeck-capture".to_string())
            .spawn(move || {
                let _ = initialize_mta();
                if options.mmcss {
                    thread::enter_audio_thread();
                }
                run(source, options, &t_stop, &t_alive, &t_inner, tx, on);
            })
            .map_err(|e| e.to_string())?;

        match rx.recv() {
            Ok(Ok(())) => Ok(Capture {
                stop,
                alive,
                inner,
                join: Some(join),
            }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err("capture thread ended before it could report".to_string()),
        }
    }

    pub fn facts(&self) -> StreamFacts {
        self.inner.lock().unwrap().facts.clone()
    }

    pub fn delivery(&self) -> Delivery {
        self.inner.lock().unwrap().delivery.clone()
    }

    /// False once the device went away under the capture, or after `stop`. The engine's monitor turns
    /// this into "source stopped producing, look for its process again".
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

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop();
    }
}

fn float_format() -> WaveFormat {
    WaveFormat::new(32, 32, &SampleType::Float, SAMPLE_RATE, CHANNELS, None)
}

fn describe(err: impl std::fmt::Display) -> String {
    err.to_string()
}

fn open(source: &Source, options: &StreamOptions) -> Result<(Opened, StreamFacts), String> {
    let format = float_format();
    let block_align = format.get_blockalign() as usize;
    let endpoint = source.endpoint();
    let mut facts = StreamFacts {
        endpoint: endpoint.name.clone(),
        format: format!(
            "{} Hz, {} ch, 32 bit float",
            format.get_samplespersec(),
            format.get_nchannels()
        ),
        ..StreamFacts::default()
    };

    let mut client = match source {
        Source::Process { pid, include_tree } => {
            AudioClient::new_application_loopback_client(*pid, *include_tree).map_err(describe)?
        }
        Source::Microphone(ep) | Source::RenderLoopback(ep) => devices::device_by_id(&ep.id)?
            .get_iaudioclient()
            .map_err(describe)?,
    };

    let mut started = false;
    if options.low_latency {
        if !source.can_ask_low_latency() {
            facts.low_latency_reason =
                "IAudioClient3 low latency is not supported with process-loopback capture".to_string();
        } else {
            match client.get_shared_mode_engine_period(&format) {
                Ok(periods) => {
                    // The engine's *minimum* shared-mode period is the low-latency choice, raised to a
                    // whole multiple of its fundamental: a raw minimum that is not a multiple is declined,
                    // and a decline is indistinguishable from a device that cannot be pushed.
                    let frames = super::lowest_latency_period(&periods);
                    match client.initialize_shared_audio_stream(&format, TimingMode::Events, frames) {
                        Ok(()) => {
                            started = true;
                            facts.low_latency_active = true;
                        }
                        Err(err) => facts.low_latency_reason = describe(err),
                    }
                }
                Err(err) => facts.low_latency_reason = describe(err),
            }
        }
    }

    if !started {
        // A declined InitializeSharedAudioStream may leave this client unusable; only a fresh one is
        // guaranteed to still accept Initialize.
        if options.low_latency && source.can_ask_low_latency() {
            if let Source::Microphone(ep) | Source::RenderLoopback(ep) = source {
                client = devices::device_by_id(&ep.id)?
                    .get_iaudioclient()
                    .map_err(describe)?;
            }
        }
        let mode = StreamMode::EventsShared {
            // Always on, including for a process loopback. Measured here: a playing browser tab returned
            // peak 0.3521 with the converter and 0.0000 without it, at the same format, packet size and
            // cadence — the stream opens and delivers silence, with no error anywhere.
            autoconvert: true,
            buffer_duration_hns: options.buffer_ms as i64 * 10_000,
        };
        client
            .initialize_client(&format, &WasapiDirection::Capture, &mode)
            .map_err(|err| format!("capture init failed: {} (low latency: {})", err, if facts.low_latency_reason.is_empty() { "not requested" } else { &facts.low_latency_reason }))?;
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

    let event = client.set_get_eventhandle().map_err(describe)?;
    let capture = client.get_audiocaptureclient().map_err(describe)?;
    Ok((
        Opened {
            client,
            event,
            capture,
            block_align,
        },
        facts,
    ))
}

fn run(
    source: Source,
    options: StreamOptions,
    stop: &AtomicBool,
    alive: &AtomicBool,
    inner: &Mutex<Inner>,
    ready: Sender<Result<(), String>>,
    mut on: OnFrames,
) {
    let opened = match open(&source, &options) {
        Ok((opened, facts)) => {
            let mut guard = inner.lock().unwrap();
            guard.facts = facts;
            opened
        }
        Err(err) => {
            inner.lock().unwrap().delivery.error = err.clone();
            alive.store(false, Ordering::Relaxed);
            let _ = ready.send(Err(err));
            return;
        }
    };
    let Opened {
        client,
        event,
        capture,
        block_align,
    } = opened;

    if let Err(err) = client.start_stream() {
        let message = format!("start_stream failed: {err}");
        inner.lock().unwrap().delivery.error = message.clone();
        alive.store(false, Ordering::Relaxed);
        let _ = ready.send(Err(message));
        return;
    }
    let _ = ready.send(Ok(()));

    let mut bytes: Vec<u8> = Vec::new();
    let mut frames: Vec<f32> = Vec::new();
    let mut previous: Option<Instant> = None;
    let mut failure = String::new();

    while !stop.load(Ordering::Relaxed) {
        loop {
            let pending = match capture.get_next_packet_size() {
                Ok(Some(frames)) if frames > 0 => frames,
                Ok(_) => 0,
                Err(err) => {
                    failure = format!("packet size: {err}");
                    0
                }
            };
            if !failure.is_empty() {
                break;
            }
            if pending == 0 {
                break;
            }
            let needed = pending as usize * block_align;
            bytes.resize(needed, 0);
            let (got, info) = match capture.read_from_device(&mut bytes[..needed]) {
                Ok(read) => read,
                Err(err) => {
                    failure = format!("read: {err}");
                    break;
                }
            };
            let arrived = Instant::now();
            let frame_count = got as usize * block_align / 4;
            frames.clear();
            decode_packet(&bytes[..frame_count * 4], info.flags.silent, &mut frames);
            on(&frames);
            let gap_ms = previous.map(|p| arrived.duration_since(p).as_secs_f64() * 1000.0).unwrap_or(0.0);
            previous = Some(arrived);
            let mut guard = inner.lock().unwrap();
            let delivery = &mut guard.delivery;
            if delivery.packets == 0 || (got as u32) < delivery.min_packet_frames {
                delivery.min_packet_frames = got as u32;
            }
            delivery.max_packet_frames = delivery.max_packet_frames.max(got as u32);
            delivery.packets += 1;
            delivery.frames += got as u64;
            delivery.max_gap_ms = delivery.max_gap_ms.max(gap_ms);
            if info.flags.silent {
                delivery.silent_packets += 1;
            }
            if info.flags.data_discontinuity {
                delivery.discontinuity += 1;
            }
        }
        if !failure.is_empty() {
            break;
        }
        // The event only signals when a new packet is ready; a timeout means nothing arrived, which is
        // how a device that was pulled out under us shows up before the stream reports an error.
        if let Err(err) = event.wait_for_event(200) {
            if !matches!(err, wasapi::WasapiError::EventTimeout) {
                failure = format!("event: {err}");
                break;
            }
        }
    }

    let _ = client.stop_stream();
    if !failure.is_empty() {
        inner.lock().unwrap().delivery.error = failure;
    }
    alive.store(false, Ordering::Relaxed);
}

/// Decodes one packet. A silent packet carries undefined bytes — often stale audio from an earlier
/// stream — so it is handed over as real silence of the same length instead of a noise burst.
fn decode_packet(bytes: &[u8], silent: bool, out: &mut Vec<f32>) {
    if silent {
        out.resize(bytes.len() / 4, 0.0);
        return;
    }
    out.extend(
        bytes
            .chunks_exact(4)
            .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]])),
    );
}

#[cfg(test)]
mod tests {
    use super::decode_packet;

    #[test]
    fn a_silent_packet_becomes_silence_of_the_same_length() {
        // Two frames of whatever the device left in the buffer; 0,0,128,63 is 1.0.
        let stale = [0u8, 0, 128, 63, 0, 0, 128, 63];
        let mut frames = Vec::new();
        decode_packet(&stale, true, &mut frames);
        assert_eq!(frames.len(), 2);
        assert!(frames.iter().all(|sample| *sample == 0.0));
    }

    #[test]
    fn a_real_packet_keeps_its_samples() {
        let mut bytes = Vec::new();
        bytes.extend(1.5f32.to_le_bytes());
        bytes.extend((-0.25f32).to_le_bytes());
        let mut frames = Vec::new();
        decode_packet(&bytes, false, &mut frames);
        assert_eq!(frames, vec![1.5, -0.25]);
    }
}

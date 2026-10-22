use wasapi::{Device, DeviceEnumerator, DeviceState, Direction as WasapiDirection, Role};

use crate::platform::{Direction, Endpoint};

pub fn to_wasapi_direction(dir: Direction) -> WasapiDirection {
    match dir {
        Direction::Capture => WasapiDirection::Capture,
        Direction::Render => WasapiDirection::Render,
    }
}

pub fn describe(dir: Direction) -> &'static str {
    match dir {
        Direction::Capture => "capture",
        Direction::Render => "render",
    }
}

/// Opens an endpoint by its OS id. Every stream in the program gets its device this way: a friendly
/// name is only ever a selector typed by a human or stored by an older build.
pub fn device_by_id(id: &str) -> Result<Device, String> {
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    enumerator
        .get_device(id)
        .map_err(|e| format!("device not found: {id} ({e})"))
}

fn endpoint_of(device: &Device, default_id: &str) -> Result<Endpoint, String> {
    let id = device.get_id().map_err(|e| e.to_string())?;
    Ok(Endpoint {
        name: device.get_friendlyname().unwrap_or_default(),
        is_default: !default_id.is_empty() && id == default_id,
        id,
    })
}

/// Active endpoints of one direction, with the multimedia-role default marked. Never picks a device
/// for the user: an empty result means the user has to add one.
pub fn list_endpoints(dir: Direction) -> Result<Vec<Endpoint>, String> {
    let _ = wasapi::initialize_mta();
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let wdir = to_wasapi_direction(dir);
    let default_id = enumerator
        .get_default_device_for_role(&wdir, &Role::Multimedia)
        .ok()
        .and_then(|d| d.get_id().ok())
        .unwrap_or_default();
    let collection = enumerator.get_device_collection(&wdir).map_err(|e| e.to_string())?;
    let count = collection.get_nbr_devices().map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for i in 0..count {
        let device = collection.get_device_at_index(i).map_err(|e| e.to_string())?;
        if !matches!(device.get_state().map_err(|e| e.to_string())?, DeviceState::Active) {
            continue;
        }
        out.push(endpoint_of(&device, &default_id)?);
    }
    out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(out)
}

/// Resolves a selector: exact id first, then a case-insensitive substring of the friendly name.
/// A substring that matches more than one endpoint is refused rather than guessed at, because the
/// names people type ("CABLE Input") are also substrings of other devices ("Hi-Fi Cable Input").
pub fn find_endpoint(dir: Direction, selector: &str) -> Result<Endpoint, String> {
    let clean = selector.trim_matches(|c| c == '"' || c == '\'');
    if clean.is_empty() {
        return Err(format!("{} 设备未指定", describe(dir)));
    }
    let endpoints = list_endpoints(dir)?;
    let lower = clean.to_lowercase();
    if let Some(hit) = endpoints.iter().find(|e| e.id.to_lowercase() == lower) {
        return Ok(hit.clone());
    }
    let partial: Vec<&Endpoint> = endpoints
        .iter()
        .filter(|e| e.name.to_lowercase().contains(&lower))
        .collect();
    match partial.len() {
        1 => Ok(partial[0].clone()),
        0 => Err(format!(
            "{} 设备未找到: {}（可选：{}）",
            describe(dir),
            clean,
            endpoints
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        )),
        n => Err(format!(
            "{} 设备名匹配到 {} 个: {}（请给完整设备 id）",
            describe(dir),
            n,
            partial
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>()
                .join(" / ")
        )),
    }
}

/// Formats a device's mix format the way the diagnostics report it, so a user can compare it against
/// the 48 kHz stereo float the program runs on internally.
pub fn mix_format_text(device: &Device) -> String {
    match device.get_iaudioclient() {
        Ok(client) => match client.get_mixformat() {
            Ok(fmt) => {
                let sample = fmt
                    .get_subformat()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|_| "unknown".to_string());
                format!(
                    "{} Hz, {} ch, {} bit, {}",
                    fmt.get_samplespersec(),
                    fmt.get_nchannels(),
                    fmt.get_bitspersample(),
                    sample
                )
            }
            Err(e) => format!("mix format unavailable: {e}"),
        },
        Err(e) => format!("mix format unavailable: {e}"),
    }
}

/// The endpoint the OS would pick for this direction. Used by the diagnostics only: the product never
/// chooses a device on the user's behalf.
pub fn default_endpoint(dir: Direction) -> Result<Endpoint, String> {
    let _ = wasapi::initialize_mta();
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let device = enumerator
        .get_default_device_for_role(&to_wasapi_direction(dir), &Role::Multimedia)
        .map_err(|e| format!("没有默认{}设备: {e}", describe(dir)))?;
    let id = device.get_id().map_err(|e| e.to_string())?;
    endpoint_of(&device, &id)
}

/// What an endpoint can be asked for, without opening a stream on it. This is the answer to "why was
/// low latency declined here", so it is worth keeping next to the enumeration.
pub struct Probe {
    pub mix_format: String,
    /// `IAudioClient3` shared-mode engine period for 48 kHz stereo float, in frames:
    /// default, fundamental, minimum, maximum. `None` when the endpoint has no `IAudioClient3`.
    pub engine_period_frames: Option<(u32, u32, u32, u32)>,
    /// Some(format) means 48 kHz stereo float is not the mix format and would be converted.
    pub nearest_float_format: Option<String>,
    pub error: String,
}

pub fn probe(id: &str) -> Probe {
    let mut out = Probe {
        mix_format: String::new(),
        engine_period_frames: None,
        nearest_float_format: None,
        error: String::new(),
    };
    let format = wasapi::WaveFormat::new(
        32,
        32,
        &wasapi::SampleType::Float,
        crate::platform::SAMPLE_RATE,
        crate::platform::CHANNELS,
        None,
    );
    match device_by_id(id) {
        Ok(device) => {
            out.mix_format = mix_format_text(&device);
            match device.get_iaudioclient() {
                Ok(client) => {
                    match client
                        .is_supported(&format, &wasapi::ShareMode::Shared)
                        .map_err(|e| e.to_string())
                    {
                        Ok(None) => {}
                        Ok(Some(nearest)) => {
                            out.nearest_float_format = Some(format!(
                                "{} Hz, {} ch, {} bit",
                                nearest.get_samplespersec(),
                                nearest.get_nchannels(),
                                nearest.get_bitspersample()
                            ))
                        }
                        Err(err) => out.error = err,
                    }
                    // get_shared_mode_engine_period is the IAudioClient3 call; it fails on endpoints
                    // that only expose IAudioClient/2, which is what "no low latency here" means.
                    out.engine_period_frames =
                        client.get_shared_mode_engine_period(&format).ok().map(|p| {
                            (p.default, p.fundamental, p.min, p.max)
                        });
                }
                Err(err) => out.error = err.to_string(),
            }
        }
        Err(err) => out.error = err,
    }
    out
}

/// One render session and the loudest peak seen for it during a scan.
pub struct Session {
    pub pid: u32,
    pub name: String,
    pub display: String,
    pub peak: f32,
}

/// The peak each process reached on an active output. Keyed by pid because a session identifier is not
/// unique, and sampled over `sample_seconds` because one instantaneous peak is noise.
pub fn render_sessions(sample_seconds: f64) -> Result<Vec<Session>, String> {
    let _ = wasapi::initialize_mta();
    let enumerator = DeviceEnumerator::new().map_err(|e| e.to_string())?;
    let collection = enumerator
        .get_device_collection(&to_wasapi_direction(Direction::Render))
        .map_err(|e| e.to_string())?;
    let count = collection.get_nbr_devices().map_err(|e| e.to_string())?;
    let mut best: std::collections::HashMap<u32, Session> = std::collections::HashMap::new();
    let rounds = (sample_seconds / 0.1).max(1.0).round() as u32;
    for _ in 0..rounds {
        for index in 0..count {
            let device = match collection.get_device_at_index(index) {
                Ok(device) => device,
                Err(_) => continue,
            };
            let manager = match device.get_iaudiosessionmanager() {
                Ok(manager) => manager,
                Err(_) => continue,
            };
            let sessions = match manager.get_audiosessionenumerator() {
                Ok(sessions) => sessions,
                Err(_) => continue,
            };
            let total = sessions.get_count().unwrap_or(0);
            for i in 0..total {
                let session = match sessions.get_session(i) {
                    Ok(session) => session,
                    Err(_) => continue,
                };
                let pid = match session.get_process_id() {
                    Ok(pid) => pid,
                    Err(_) => continue,
                };
                let peak = session
                    .get_audiometerinformation()
                    .and_then(|meter| meter.get_peak_value())
                    .unwrap_or(0.0);
                let entry = best.entry(pid).or_insert_with(|| Session {
                    pid,
                    name: process_name(pid),
                    display: session.get_display_name().unwrap_or_default(),
                    peak: 0.0,
                });
                if peak > entry.peak {
                    entry.peak = peak;
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let mut rows: Vec<Session> = best.into_values().collect();
    rows.sort_by(|a, b| {
        b.peak
            .partial_cmp(&a.peak)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(rows)
}

/// Image file name of a process, read from a toolhelp snapshot. The snapshot is per call: it costs a few
/// milliseconds and avoids carrying a Windows handle that every error path would have to close.
pub fn process_name(pid: u32) -> String {
    if pid == std::process::id() {
        return "micdeck (self)".to_string();
    }
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let snapshot = match unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) } {
        Ok(handle) => handle,
        Err(_) => return String::new(),
    };
    let mut entry = PROCESSENTRY32W::default();
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut name = String::new();
    unsafe {
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if entry.th32ProcessID == pid {
                    let end = entry
                        .szExeFile
                        .iter()
                        .position(|c| *c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    name = String::from_utf16_lossy(&entry.szExeFile[..end]);
                    break;
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    name
}

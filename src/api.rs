//! The command table. Paths, methods and field names are the contract with `ui/index.html`; the shell
//! hands each request to [`handle`] and gets the same JSON body the HTTP server used to write to the
//! socket. Status codes are gone with the socket — every failure, refused or unknown, arrives as
//! `{"error": …}`, which is what the page already read out of the body.

use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde_json::{json, Value};

use crate::engine::status::Identity;
use crate::engine::Engine;
use crate::log;
use crate::platform::{self, Direction};
use crate::settings::Settings;

/// A missing key is a default, not an error: the page posts whole objects, and a hand-written or
/// half-filled body should not be able to take a route down.
macro_rules! request {
    ($name:ident { $($field:ident : $type:ty),* $(,)? }) => {
        #[derive(Debug, Default, Deserialize)]
        #[serde(default, rename_all = "camelCase")]
        struct $name { $( pub $field: $type ),* }
    };
}

request!(DeviceRequest { device_id: String });
request!(LineRequest { line_id: u32 });
request!(LineDeviceRequest { line_id: u32, device_id: String });
request!(LineLabelRequest { line_id: u32, label: String });
request!(AppRouteRequest { id: u32, line_id: u32, on: bool });
request!(MicRouteRequest { line_id: u32, on: bool });
request!(SourceAddRequest { pid: u32, title: String });
request!(SourceIdRequest { id: u32 });

/// `gain` defaults to unity, never to zero: a body that omits it must not silence a source.
#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GainRequest {
    id: u32,
    gain: f32,
}

impl Default for GainRequest {
    fn default() -> Self {
        Self { id: 0, gain: 1.0 }
    }
}

/// `muted` has no default, unlike every other request field: a body that omits the switch must be
/// refused, not read as "open the microphone".
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MicMuteRequest {
    muted: bool,
}

/// Likewise required: an empty string is the explicit "no shortcut", so a body that forgets the field
/// must not be read as "disable the shortcut".
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HotkeyRequest {
    hotkey: String,
}

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct MicRequest {
    device_id: String,
    gain: f32,
}

impl Default for MicRequest {
    fn default() -> Self {
        Self {
            device_id: String::new(),
            gain: 1.0,
        }
    }
}

fn ok() -> Value {
    json!({ "ok": true })
}

fn error(message: impl Into<String>) -> Value {
    json!({ "error": message.into() })
}

/// Lines, the microphone's routes and the added sources are the topology worth keeping; nothing here
/// stores a process id, because it means nothing after a reboot.
pub fn persist_topology(engine: &Arc<Engine>, settings: &Arc<Mutex<Settings>>) {
    let status = engine.snapshot();
    let lines: Vec<(u32, &str, &str)> = status
        .lines
        .iter()
        .map(|line| (line.id, line.device_id.as_str(), line.label.as_str()))
        .collect();
    let mic_routes: Vec<u32> = status.mic_routes.iter().map(|route| route.line_id).collect();
    let sources: Vec<(Identity, &str, f32, &[u32])> = status
        .sources
        .iter()
        .map(|source| {
            (
                Identity {
                    name: source.name.clone(),
                    path: source.path.clone(),
                },
                source.title.as_str(),
                source.gain,
                source.lines.as_slice(),
            )
        })
        .collect();
    let mut settings = settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    settings.set_lines(&lines);
    settings.set_mic_routes(&mic_routes);
    settings.set_sources(&sources);
}

/// One process icon as the page wants it: the mime type and the bytes. A process with nothing to show
/// answers with the 1×1 transparent PNG rather than an error, which is what keeps the row's placeholder
/// square on screen.
pub fn icon(pid: u32) -> Value {
    let icon = platform::icon_for_pid(pid);
    json!({ "mime": icon.mime, "bytes": icon.bytes })
}

fn from_json<T: for<'de> Deserialize<'de>>(body: &str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|err| format!("bad body: {err}"))
}

pub fn handle(
    engine: &Arc<Engine>,
    settings: &Arc<Mutex<Settings>>,
    method: &str,
    path: &str,
    body: &str,
) -> Value {
    match dispatch(engine, settings, method, path, body) {
        Ok(value) => value,
        Err(message) => error(message),
    }
}

fn dispatch(
    engine: &Arc<Engine>,
    settings: &Arc<Mutex<Settings>>,
    method: &str,
    path: &str,
    body: &str,
) -> Result<Value, String> {
    if method == "GET" && path == "/api/state" {
        return Ok(serde_json::to_value(engine.snapshot()).unwrap_or_else(|_| json!({})));
    }
    if method == "GET" && path == "/api/devices" {
        return Ok(list_devices());
    }

    if method == "POST" && path == "/api/lines/add" {
        let request: DeviceRequest = from_json(body)?;
        if request.device_id.trim().is_empty() {
            return Err("deviceId required".to_string());
        }
        let line_id = engine.add_line(&request.device_id)?;
        persist_topology(engine, settings);
        return Ok(json!({ "ok": true, "lineId": line_id }));
    }
    if method == "POST" && path == "/api/lines/remove" {
        let request: LineRequest = from_json(body)?;
        engine.remove_line(request.line_id);
        persist_topology(engine, settings);
        return Ok(ok());
    }
    if method == "POST" && path == "/api/lines/device" {
        let request: LineDeviceRequest = from_json(body)?;
        if request.device_id.trim().is_empty() {
            return Err("deviceId required".to_string());
        }
        engine.set_line_device(request.line_id, &request.device_id)?;
        persist_topology(engine, settings);
        return Ok(ok());
    }
    if method == "POST" && path == "/api/lines/restart" {
        let request: LineRequest = from_json(body)?;
        engine.restart_line(request.line_id)?;
        return Ok(ok());
    }
    if method == "POST" && path == "/api/lines/label" {
        let request: LineLabelRequest = from_json(body)?;
        engine.set_line_label(request.line_id, request.label.trim())?;
        persist_topology(engine, settings);
        return Ok(ok());
    }
    if method == "POST" && path == "/api/engine/restart" {
        engine.restart_all_lines();
        return Ok(json!({ "ok": true, "error": engine.snapshot().error }));
    }
    if method == "POST" && path == "/api/sources/add" {
        let request: SourceAddRequest = from_json(body)?;
        if request.pid == 0 {
            return Err("pid required".to_string());
        }
        let id = engine.add_app_source(request.pid, &request.title)?;
        persist_topology(engine, settings);
        return Ok(json!({ "ok": true, "id": id }));
    }
    if method == "POST" && path == "/api/sources/remove" {
        let request: SourceIdRequest = from_json(body)?;
        if request.id == 0 {
            return Err("id required".to_string());
        }
        engine.remove_app_source(request.id);
        persist_topology(engine, settings);
        return Ok(ok());
    }
    if method == "POST" && path == "/api/previews/start" {
        let rows = platform::list_window_candidates();
        let feeds: Vec<(u32, String, String)> = rows
            .iter()
            .map(|row| (row.pid, row.name.clone(), row.title.clone()))
            .collect();
        engine.start_previews(&feeds);
        let rows: Vec<Value> = rows
            .into_iter()
            .map(|row| json!({ "pid": row.pid, "name": row.name, "title": row.title }))
            .collect();
        return Ok(json!({ "rows": rows }));
    }
    if method == "POST" && path == "/api/previews/stop" {
        engine.stop_previews();
        return Ok(ok());
    }
    if method == "POST" && path == "/api/routes/app" {
        let request: AppRouteRequest = from_json(body)?;
        if request.id == 0 {
            return Err("id required".to_string());
        }
        engine.set_app_route(request.id, request.line_id, request.on)?;
        persist_topology(engine, settings);
        return Ok(ok());
    }
    if method == "POST" && path == "/api/routes/mic" {
        let request: MicRouteRequest = from_json(body)?;
        engine.set_mic_route(request.line_id, request.on)?;
        persist_topology(engine, settings);
        return Ok(ok());
    }
    if method == "POST" && path == "/api/sources/gain" {
        let request: GainRequest = from_json(body)?;
        if request.id == 0 {
            return Err("bad body".to_string());
        }
        engine.set_source_gain(request.id, request.gain);
        persist_topology(engine, settings);
        return Ok(ok());
    }
    if method == "POST" && path == "/api/mic" {
        let request: MicRequest = from_json(body)?;
        engine.set_mic(&request.device_id, request.gain)?;
        let mut settings = settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        settings.set_mic(&request.device_id, request.gain);
        return Ok(ok());
    }
    if method == "POST" && path == "/api/mic/mute" {
        let request: MicMuteRequest = from_json(body)?;
        // Silent on purpose: the page shows the state it just set, so the cue belongs to the global
        // shortcut alone (a click is already visible feedback, and a beep would double it).
        engine.set_mic_muted(request.muted);
        let mut settings = settings.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        settings.set_mic_muted(request.muted);
        return Ok(ok());
    }
    if method == "GET" && path == "/api/hotkey" {
        return Ok(json!({
            "hotkey": settings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .hotkey
        }));
    }
    if method == "POST" && path == "/api/hotkey" {
        let request: HotkeyRequest = from_json(body)?;
        let binding = crate::hotkey::parse(&request.hotkey)?;
        let canonical = binding.map(|binding| binding.label()).unwrap_or_default();
        // The running window is the only thing that can register a chord. A refused chord must leave
        // the store untouched, so persistence waits for the answer.
        match crate::tray::rebind(binding) {
            crate::tray::Rebind::Applied | crate::tray::Rebind::NoTray => {
                settings
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .set_hotkey(&canonical);
                return Ok(json!({ "hotkey": canonical }));
            }
            crate::tray::Rebind::Refused(message) => return Err(message),
        }
    }
    Err(format!("not found: {method} {path}"))
}

fn list_devices() -> Value {
    let rows = |direction: Direction| -> Vec<Value> {
        platform::list_endpoints(direction)
            .unwrap_or_else(|err| {
                log::line(&format!("enumerate {} failed: {err}", platform::describe(direction)));
                Vec::new()
            })
            .into_iter()
            .map(|endpoint| {
                json!({ "id": endpoint.id, "name": endpoint.name, "isDefault": endpoint.is_default })
            })
            .collect()
    };
    json!({ "mics": rows(Direction::Capture), "outputs": rows(Direction::Render) })
}

#[cfg(test)]
mod tests;

//! The route table. Paths, methods, field names and status codes are the contract with `ui.html`.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::engine::status::Identity;
use crate::engine::Engine;
use crate::log;
use crate::platform::{self, Direction};
use crate::settings::Settings;

/// The page this binary serves; it ships next to this module.
const UI_HTML: &str = include_str!("ui.html");

/// Identifies the UI this process serves. A tab compares it against the one it loaded and reloads itself
/// when a rebuilt binary starts, so a running page never keeps stale JS. A change detector, not a
/// digest: nothing outside this process compares the value, so no hashing dependency is warranted.
fn ui_build() -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in UI_HTML.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl Response {
    fn json(value: &impl Serialize) -> Response {
        Response {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body: serde_json::to_vec(value).unwrap_or_else(|_| b"{}".to_vec()),
        }
    }

    fn error(status: u16, message: impl Into<String>) -> Response {
        #[derive(Serialize)]
        struct Error {
            error: String,
        }
        Response::json(&Error {
            error: message.into(),
        })
        .with_status(status)
    }

    fn with_status(mut self, status: u16) -> Self {
        self.status = status;
        self
    }

    pub fn reason(&self) -> &'static str {
        match self.status {
            200 => "OK",
            400 => "Error",
            404 => "Not Found",
            _ => "Error",
        }
    }
}

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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Ack {
    ok: bool,
}

fn ok() -> Response {
    Response::json(&Ack { ok: true })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EndpointRow {
    id: String,
    name: String,
    is_default: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Devices {
    mics: Vec<EndpointRow>,
    outputs: Vec<EndpointRow>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WindowRowOut {
    pid: u32,
    name: String,
    title: String,
}

fn from_json<T: for<'de> Deserialize<'de>>(body: &str) -> Result<T, String> {
    serde_json::from_str(body).map_err(|err| format!("bad body: {err}"))
}

/// `pid=` anywhere in the query, up to the next `&`. An unparsable value is 0, which the icon route
/// answers with the empty icon rather than an error.
pub(super) fn icon_pid(target: &str) -> u32 {
    let Some(at) = target.find("pid=") else {
        return 0;
    };
    let rest = &target[at + "pid=".len()..];
    let value = rest.split('&').next().unwrap_or(rest);
    value.parse::<u32>().unwrap_or(0)
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

pub fn route(
    engine: &Arc<Engine>,
    settings: &Arc<Mutex<Settings>>,
    method: &str,
    target: &str,
    body: &str,
) -> Response {
    let path = target.split('?').next().unwrap_or(target);
    let matched = dispatch(engine, settings, method, target, path, body);
    match matched {
        Ok(response) => response,
        Err(message) => Response::error(400, message),
    }
}

fn dispatch(
    engine: &Arc<Engine>,
    settings: &Arc<Mutex<Settings>>,
    method: &str,
    target: &str,
    path: &str,
    body: &str,
) -> Result<Response, String> {
    if method == "GET" && path == "/" {
        return Ok(Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: UI_HTML.as_bytes().to_vec(),
        });
    }
    if method == "GET" && path == "/api/state" {
        return Ok(Response::json(&engine.snapshot()));
    }
    if method == "GET" && path == "/api/ping" {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Ping {
            ok: bool,
            build: String,
        }
        return Ok(Response::json(&Ping {
            ok: true,
            build: ui_build(),
        }));
    }
    if method == "GET" && path == "/api/devices" {
        return Ok(Response::json(&list_devices()));
    }
    if method == "GET" && path.starts_with("/api/icon") {
        // Always 200: the "no icon" answer is the 1×1 transparent PNG, which is what keeps the row's
        // placeholder square on screen. An error would hide the image instead.
        let icon = platform::icon_for_pid(icon_pid(target));
        return Ok(Response {
            status: 200,
            content_type: icon.mime,
            body: icon.bytes,
        });
    }

    if method == "POST" && path == "/api/lines/add" {
        let request: DeviceRequest = from_json(body)?;
        if request.device_id.trim().is_empty() {
            return Err("deviceId required".to_string());
        }
        let line_id = engine.add_line(&request.device_id)?;
        persist_topology(engine, settings);
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Added {
            ok: bool,
            line_id: u32,
        }
        return Ok(Response::json(&Added {
            ok: true,
            line_id,
        }));
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
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Restarted {
            ok: bool,
            error: String,
        }
        return Ok(Response::json(&Restarted {
            ok: true,
            error: engine.snapshot().error,
        }));
    }
    if method == "POST" && path == "/api/sources/add" {
        let request: SourceAddRequest = from_json(body)?;
        if request.pid == 0 {
            return Err("pid required".to_string());
        }
        let id = engine.add_app_source(request.pid, &request.title)?;
        persist_topology(engine, settings);
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct SourceAdded {
            ok: bool,
            id: u32,
        }
        return Ok(Response::json(&SourceAdded { ok: true, id }));
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
        let out: Vec<WindowRowOut> = rows
            .into_iter()
            .map(|row| WindowRowOut {
                pid: row.pid,
                name: row.name,
                title: row.title,
            })
            .collect();
        #[derive(Serialize)]
        struct Rows {
            rows: Vec<WindowRowOut>,
        }
        return Ok(Response::json(&Rows { rows: out }));
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
    Ok(Response::error(404, format!("not found: {method} {path}")))
}

fn list_devices() -> Devices {
    let rows = |direction: Direction| -> Vec<EndpointRow> {
        platform::list_endpoints(direction)
            .unwrap_or_else(|err| {
                log::line(&format!("enumerate {} failed: {err}", platform::describe(direction)));
                Vec::new()
            })
            .into_iter()
            .map(|endpoint| EndpointRow {
                id: endpoint.id,
                name: endpoint.name,
                is_default: endpoint.is_default,
            })
            .collect()
    };
    Devices {
        mics: rows(Direction::Capture),
        outputs: rows(Direction::Render),
    }
}

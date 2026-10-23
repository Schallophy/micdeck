//! Offline tests for the transport: the wire format the page parses, with no device and no browser. The
//! engine here has no lines and no microphone, so it only ever answers about an empty topology.

use std::io::{Cursor, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::web::api::Response;
use crate::engine::{Engine, SystemClock};
use crate::settings::Settings;

fn engine() -> Arc<Engine> {
    Engine::start(Arc::new(SystemClock))
}

/// A settings file under the temp directory, never the real store.
fn settings(name: &str) -> (Arc<Mutex<Settings>>, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "micdeck-web-{}-{name}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut loaded = Settings::load_at(&path);
    loaded.store = path.clone();
    (Arc::new(Mutex::new(loaded)), path)
}

fn json(response: &Response) -> serde_json::Value {
    serde_json::from_slice(&response.body).unwrap_or_else(|err| {
        panic!(
            "not json: {err}: {}",
            String::from_utf8_lossy(&response.body)
        )
    })
}

#[test]
fn a_request_is_split_into_method_target_and_body() {
    let text = "GET /api/state?ts=1727 HTTP/1.1\r\nHost: 127.0.0.1:8765\r\nAccept: */*\r\n\r\n";
    let request = read_request(&mut Cursor::new(text.as_bytes())).unwrap();
    assert_eq!(request.method, "GET");
    assert_eq!(request.target, "/api/state?ts=1727");
    assert!(request.body.is_empty());

    // A posted body: exactly content-length bytes are taken, whatever follows on the wire is not.
    let body = "{\"id\":3,\"lineId\":1}";
    let text = format!(
        "POST /api/routes/app HTTP/1.1\r\ncontent-length: {}\r\n\r\n{body}trailing",
        body.len()
    );
    let request = read_request(&mut Cursor::new(text.into_bytes())).unwrap();
    assert_eq!(request.method, "POST");
    assert_eq!(request.body, body);

    // Reads that come back in small pieces must not lose the header boundary.
    let text: &[u8] = b"POST /api/mic HTTP/1.1\r\nContent-Length: 12\r\n\r\n{\"gain\":1.0}";
    let mut reader = std::io::BufReader::with_capacity(7, Cursor::new(text.to_vec()));
    let request = read_request(&mut reader).unwrap();
    assert_eq!(request.body, "{\"gain\":1.0}");

    // Garbage that is not a request line, and a connection that closes before the headers end, both
    // refuse without panicking.
    assert!(read_request(&mut Cursor::new(b"abcdefgh\r\n\r\n".to_vec())).is_none());
    assert!(read_request(&mut Cursor::new(b"".to_vec())).is_none());
}

#[test]
fn the_ping_carries_the_build_the_page_loaded_against() {
    let engine = engine();
    let (settings, path) = settings("ping");
    let response = api::route(&engine, &settings, "GET", "/api/ping", "");
    assert_eq!(response.status, 200);
    let value = json(&response);
    assert_eq!(value["ok"], serde_json::json!(true));
    let build = value["build"].as_str().unwrap();
    assert_eq!(build.len(), 16, "a fixed-width token the page can remember");
    assert!(build.chars().all(|c| c.is_ascii_hexdigit()));
    let _ = engine;
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_state_envelope_carries_every_key_the_page_reads() {
    // This list is the contract with `ui.html`. Renaming a field on the engine side without renaming it
    // here is the failure this test exists to catch.
    let expected = [
        "running",
        "state",
        "error",
        "micId",
        "micName",
        "micGain",
        "micMuted",
        "micLevel",
        "micHold",
        "micRoutes",
        "starvedFrames",
        "lines",
        "sources",
        "previews",
    ];
    let engine = engine();
    let (settings, path) = settings("state");
    let response = api::route(&engine, &settings, "GET", "/api/state", "");
    let value = json(&response);
    for key in expected {
        assert!(value.get(key).is_some(), "missing {key}");
    }
    assert_eq!(value["state"], "stopped", "no lines yet");
    assert!(value["lines"].as_array().unwrap().is_empty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_page_is_embedded_and_served_whole() {
    let engine = engine();
    let (settings, path) = settings("page");
    let response = api::route(&engine, &settings, "GET", "/", "");
    assert_eq!(response.status, 200);
    assert_eq!(response.content_type, "text/html; charset=utf-8");
    let text = String::from_utf8_lossy(&response.body).into_owned();
    assert!(text.len() > 20_000, "the embedded page is the real one");
    for marker in [
        "/api/previews/start",
        "EventSource",
        "/api/mic/mute",
        "hotkeyBtn",
        "micToggle",
        "micDevice",
    ] {
        assert!(text.contains(marker), "the page should still use {marker}");
    }
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_unknown_path_is_404_and_a_broken_body_is_400() {
    let engine = engine();
    let (settings, path) = settings("errors");

    let response = api::route(&engine, &settings, "GET", "/api/nope", "");
    assert_eq!(response.status, 404);
    assert!(json(&response)["error"]
        .as_str()
        .unwrap()
        .contains("not found"));

    let response = api::route(&engine, &settings, "POST", "/api/lines/add", "{");
    assert_eq!(response.status, 400);
    assert!(json(&response)["error"].as_str().unwrap().contains("bad body"));

    // The device the page asked for does not exist, so no line appears and nothing is persisted.
    let response = api::route(
        &engine,
        &settings,
        "POST",
        "/api/lines/add",
        r#"{"deviceId":"no-such-endpoint-xyz"}"#,
    );
    assert_eq!(response.status, 400);
    assert!(json(&response)["error"].as_str().unwrap().len() > 1);
    assert!(engine.snapshot().lines.is_empty());
    assert!(!path.exists(), "a refused edit must not touch the store");
    let _ = engine;
}

#[test]
fn choosing_no_microphone_is_a_no_op_and_a_gain_change_is_not() {
    let engine = engine();
    let (settings, path) = settings("mic");
    let response = api::route(
        &engine,
        &settings,
        "POST",
        "/api/mic",
        r#"{"deviceId":"","gain":1.0}"#,
    );
    assert_eq!(response.status, 200);
    // A fresh store already says "no microphone at unity", so the setter's comparison holds and nothing
    // is written. The engine still has to accept the request.
    assert!(
        !path.exists(),
        "an edit that changes nothing must not touch the store"
    );

    let response = api::route(
        &engine,
        &settings,
        "POST",
        "/api/mic",
        r#"{"deviceId":"","gain":0.5}"#,
    );
    assert_eq!(response.status, 200);
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.contains("\"micId\": \"\""), "{stored}");
    assert!(stored.contains("\"micGain\": 0.5"), "{stored}");
    assert_eq!(engine.snapshot().mic_gain, 0.5);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_master_switch_is_reported_and_persisted() {
    let engine = engine();
    let (settings, path) = settings("mute");
    let response = api::route(
        &engine,
        &settings,
        "POST",
        "/api/mic/mute",
        r#"{"muted":true}"#,
    );
    assert_eq!(response.status, 200);
    assert!(engine.snapshot().mic_muted, "the engine is the live truth");
    let state = json(&api::route(&engine, &settings, "GET", "/api/state", ""));
    assert_eq!(state["micMuted"], serde_json::json!(true));
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.contains("\"micMuted\": true"), "{stored}");

    // An omitted switch must not be read as "unmute": the one field this route has is required.
    let response = api::route(&engine, &settings, "POST", "/api/mic/mute", "{}");
    assert_eq!(response.status, 400);
    assert!(engine.snapshot().mic_muted, "a refused body changed nothing");

    let response = api::route(
        &engine,
        &settings,
        "POST",
        "/api/mic/mute",
        r#"{"muted":false}"#,
    );
    assert_eq!(response.status, 200);
    assert!(!engine.snapshot().mic_muted);
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.contains("\"micMuted\": false"), "{stored}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_hotkey_route_keeps_one_canonical_spelling_and_refuses_nonsense() {
    let engine = engine();
    let (settings, path) = settings("hotkey");
    let value = json(&api::route(&engine, &settings, "GET", "/api/hotkey", ""));
    assert_eq!(value["hotkey"], serde_json::json!("Ctrl+Alt+M"));

    let response = api::route(
        &engine,
        &settings,
        "POST",
        "/api/hotkey",
        r#"{"hotkey":"ctrl+shift+f8"}"#,
    );
    assert_eq!(response.status, 200);
    assert_eq!(json(&response)["hotkey"], serde_json::json!("Ctrl+Shift+F8"));
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.contains("\"hotkey\": \"Ctrl+Shift+F8\""), "{stored}");

    // A chord the parser does not know is refused before anything is written.
    let response = api::route(
        &engine,
        &settings,
        "POST",
        "/api/hotkey",
        r#"{"hotkey":"Ctrl+Alt+Banana"}"#,
    );
    assert_eq!(response.status, 400);
    assert_eq!(
        json(&api::route(&engine, &settings, "GET", "/api/hotkey", ""))["hotkey"],
        serde_json::json!("Ctrl+Shift+F8")
    );

    // The empty string is the explicit "no shortcut"; a body that omits the field is not.
    let response = api::route(&engine, &settings, "POST", "/api/hotkey", r#"{"hotkey":""}"#);
    assert_eq!(response.status, 200);
    assert_eq!(json(&response)["hotkey"], serde_json::json!(""));
    let response = api::route(&engine, &settings, "POST", "/api/hotkey", "{}");
    assert_eq!(response.status, 400);
    let _ = std::fs::remove_file(&path);
}

/// One request over a real socket, which is what proves the framing a browser has to accept. The
/// listener lives on the server thread, so the caller gets the client, the join handle and the stop flag.
fn exchange(
    request: &[u8],
    engine: &Arc<Engine>,
    settings: &Arc<Mutex<Settings>>,
) -> (TcpStream, std::thread::JoinHandle<()>, Arc<AtomicBool>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let engine = engine.clone();
        let settings = settings.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            handle(
                stream,
                &engine,
                &settings,
                &stop,
                &Arc::new(AtomicBool::new(false)),
            )
        })
    };
    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client.write_all(request).unwrap();
    client.flush().unwrap();
    (client, server, stop)
}

#[test]
fn a_response_over_a_socket_is_framed_the_way_a_browser_expects() {
    let engine = engine();
    let (settings, path) = settings("socket");
    let (mut client, server, _stop) = exchange(b"GET /api/ping HTTP/1.1\r\n\r\n", &engine, &settings);
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    server.join().unwrap();

    let text = String::from_utf8_lossy(&response).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    assert!(head.starts_with("HTTP/1.1 200 OK\r\n"), "{head}");
    assert!(head.contains("Content-Type: application/json; charset=utf-8"));
    assert!(head.contains("Connection: close"));
    let length: usize = head
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("content-length:"))
        .unwrap()
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .parse()
        .unwrap();
    assert_eq!(length, body.as_bytes().len(), "the length must match the bytes");
    let value: serde_json::Value = serde_json::from_str(body).unwrap();
    assert_eq!(value["ok"], serde_json::json!(true));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_events_stream_pushes_until_the_server_is_stopped() {
    let engine = engine();
    let (settings, path) = settings("events");
    let (mut client, server, stop) =
        exchange(b"GET /api/events HTTP/1.1\r\n\r\n", &engine, &settings);

    // Two frames is enough: one proves the head and the first snapshot, two prove the cadence loop.
    client
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let mut seen = Vec::new();
    let mut byte = 0u8;
    let mut frames = 0;
    for _ in 0..400_000 {
        if client.read(std::slice::from_mut(&mut byte)).unwrap_or(0) == 0 {
            break;
        }
        seen.push(byte);
        if seen.ends_with(b"data: ") {
            frames += 1;
            if frames >= 2 {
                break;
            }
        }
    }
    assert!(frames >= 2, "the page must not have to poll");
    let text = String::from_utf8_lossy(&seen).into_owned();
    assert!(text.starts_with(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache"
    ));
    assert!(text.contains("\"state\""));

    // The stop flag is how a long-lived stream is ended without waiting for the client.
    stop.store(true, Ordering::Relaxed);
    server.join().unwrap();
    let _ = std::fs::remove_file(&path);
}

/// The icon route never fails: for a process with nothing to show it returns the 1×1 transparent PNG,
/// which keeps the row's placeholder square on screen instead of hiding the image.
#[test]
fn an_icon_request_answers_with_a_placeholder_rather_than_an_error() {
    let engine = engine();
    let (settings, _path) = settings("icon");
    for target in [
        "/api/icon?pid=0",
        "/api/icon?pid=not-a-pid",
        "/api/icon",
        "/api/icon?pid=",
    ] {
        let response = api::route(&engine, &settings, "GET", target, "");
        assert_eq!(response.status, 200, "{target}");
        assert_eq!(response.content_type, "image/png", "{target}");
        assert_eq!(&response.body[1..4], b"PNG", "{target}");
        assert_eq!(response.body.len(), 68, "{target}");
    }
    // A pid nobody can resolve is the same answer; an unreadable process is not an error either.
    let response = api::route(&engine, &settings, "GET", "/api/icon?pid=999999", "");
    assert_eq!(response.status, 200);
    assert!(!response.body.is_empty());
}

#[test]
fn the_icon_query_reads_the_pid_the_way_the_page_writes_it() {
    assert_eq!(api::icon_pid("/api/icon?pid=4321"), 4321);
    assert_eq!(api::icon_pid("/api/icon?pid=4321&x=2"), 4321);
    assert_eq!(api::icon_pid("/api/icon?pid=abc"), 0);
    assert_eq!(api::icon_pid("/api/icon"), 0);
    assert_eq!(api::icon_pid("/api/icon?pid=99999999999999"), 0);
}

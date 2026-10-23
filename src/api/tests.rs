//! Offline tests for the command table: the same bodies the page sends and reads back, with no device
//! and no window. The engine here has no lines and no microphone, so it only ever answers about an empty
//! topology.

use std::sync::{Arc, Mutex};

use super::*;
use crate::engine::{Engine, SystemClock};
use crate::settings::Settings;

fn engine() -> Arc<Engine> {
    Engine::start(Arc::new(SystemClock))
}

/// A settings file under the temp directory, never the real store.
fn settings(name: &str) -> (Arc<Mutex<Settings>>, std::path::PathBuf) {
    let path = std::env::temp_dir().join(format!(
        "micdeck-api-{}-{name}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&path);
    let mut loaded = Settings::load_at(&path);
    loaded.store = path.clone();
    (Arc::new(Mutex::new(loaded)), path)
}

fn get(engine: &Arc<Engine>, settings: &Arc<Mutex<Settings>>, path: &str) -> Value {
    handle(engine, settings, "GET", path, "")
}

fn post(engine: &Arc<Engine>, settings: &Arc<Mutex<Settings>>, path: &str, body: &str) -> Value {
    handle(engine, settings, "POST", path, body)
}

#[test]
fn the_state_envelope_carries_every_key_the_page_reads() {
    // This list is the contract with `ui/index.html`. Renaming a field on the engine side without
    // renaming it here is the failure this test exists to catch.
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
    let value = get(&engine, &settings, "/api/state");
    for key in expected {
        assert!(value.get(key).is_some(), "missing {key}");
    }
    assert_eq!(value["state"], "stopped", "no lines yet");
    assert!(value["lines"].as_array().unwrap().is_empty());
    let _ = std::fs::remove_file(&path);
}

#[test]
fn an_unknown_path_and_a_broken_body_both_answer_with_an_error() {
    let engine = engine();
    let (settings, path) = settings("errors");

    let value = get(&engine, &settings, "/api/nope");
    assert!(value["error"]
        .as_str()
        .unwrap()
        .contains("not found: GET /api/nope"));

    let value = post(&engine, &settings, "/api/lines/add", "{");
    assert!(value["error"].as_str().unwrap().contains("bad body"));

    // The device the page asked for does not exist, so no line appears and nothing is persisted.
    let value = post(
        &engine,
        &settings,
        "/api/lines/add",
        r#"{"deviceId":"no-such-endpoint-xyz"}"#,
    );
    assert!(value["error"].as_str().unwrap().len() > 1);
    assert!(engine.snapshot().lines.is_empty());
    assert!(!path.exists(), "a refused edit must not touch the store");
    let _ = engine;
}

#[test]
fn choosing_no_microphone_is_a_no_op_and_a_gain_change_is_not() {
    let engine = engine();
    let (settings, path) = settings("mic");
    let value = post(&engine, &settings, "/api/mic", r#"{"deviceId":"","gain":1.0}"#);
    assert_eq!(value["ok"], json!(true));
    // A fresh store already says "no microphone at unity", so the setter's comparison holds and nothing
    // is written. The engine still has to accept the request.
    assert!(
        !path.exists(),
        "an edit that changes nothing must not touch the store"
    );

    let value = post(&engine, &settings, "/api/mic", r#"{"deviceId":"","gain":0.5}"#);
    assert_eq!(value["ok"], json!(true));
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
    let value = post(&engine, &settings, "/api/mic/mute", r#"{"muted":true}"#);
    assert_eq!(value["ok"], json!(true));
    assert!(engine.snapshot().mic_muted, "the engine is the live truth");
    assert_eq!(
        get(&engine, &settings, "/api/state")["micMuted"],
        json!(true)
    );
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.contains("\"micMuted\": true"), "{stored}");

    // An omitted switch must not be read as "unmute": the one field this route has is required.
    let value = post(&engine, &settings, "/api/mic/mute", "{}");
    assert!(value["error"].is_string(), "a missing switch is refused");
    assert!(engine.snapshot().mic_muted, "a refused body changed nothing");

    let value = post(&engine, &settings, "/api/mic/mute", r#"{"muted":false}"#);
    assert_eq!(value["ok"], json!(true));
    assert!(!engine.snapshot().mic_muted);
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.contains("\"micMuted\": false"), "{stored}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_hotkey_route_keeps_one_canonical_spelling_and_refuses_nonsense() {
    let engine = engine();
    let (settings, path) = settings("hotkey");
    assert_eq!(get(&engine, &settings, "/api/hotkey")["hotkey"], json!("Ctrl+Alt+M"));

    let value = post(
        &engine,
        &settings,
        "/api/hotkey",
        r#"{"hotkey":"ctrl+shift+f8"}"#,
    );
    assert_eq!(value["hotkey"], json!("Ctrl+Shift+F8"));
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.contains("\"hotkey\": \"Ctrl+Shift+F8\""), "{stored}");

    // A lone side-specific modifier is a tap binding; the spelling it keeps names the side.
    let value = post(&engine, &settings, "/api/hotkey", r#"{"hotkey":"rctrl"}"#);
    assert_eq!(value["hotkey"], json!("RCtrl"));
    let stored = std::fs::read_to_string(&path).unwrap();
    assert!(stored.contains("\"hotkey\": \"RCtrl\""), "{stored}");

    // A chord the parser does not know is refused before anything is written.
    let value = post(
        &engine,
        &settings,
        "/api/hotkey",
        r#"{"hotkey":"Ctrl+Alt+Banana"}"#,
    );
    assert!(value["error"].is_string());
    assert_eq!(
        get(&engine, &settings, "/api/hotkey")["hotkey"],
        json!("RCtrl")
    );

    // The empty string is the explicit "no shortcut"; a body that omits the field is not.
    let value = post(&engine, &settings, "/api/hotkey", r#"{"hotkey":""}"#);
    assert_eq!(value["hotkey"], json!(""));
    let value = post(&engine, &settings, "/api/hotkey", "{}");
    assert!(value["error"].is_string());
    let _ = std::fs::remove_file(&path);
}

/// A process with nothing to show answers with the 1×1 transparent PNG rather than an error, which keeps
/// the row's placeholder square on screen instead of hiding the image.
#[test]
fn an_unresolvable_icon_is_the_placeholder_rather_than_an_error() {
    let value = icon(0);
    assert_eq!(value["mime"], json!("image/png"));
    let bytes = value["bytes"].as_array().unwrap();
    assert_eq!(bytes.len(), 68);
    assert_eq!(&bytes[1..4], &[b'P', b'N', b'G']);
    // A pid nobody can resolve is the same answer; an unreadable process is not an error either.
    let value = icon(999_999);
    assert!(!value["bytes"].as_array().unwrap().is_empty());
}

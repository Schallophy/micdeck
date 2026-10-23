//! The application bootstrap: engine, settings and the restore that puts a saved topology back.
//! Separate from `main` because the tray and the desktop shell own the same startup path.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::api;
use crate::engine::status::Identity;
use crate::engine::{Engine, SystemClock};
use crate::log;
use crate::settings::Settings;

pub struct App {
    pub engine: Arc<Engine>,
    pub settings: Arc<Mutex<Settings>>,
}

impl App {
    /// The tray's hotkey writes the switch back to the store; it owns no settings of its own.
    pub fn settings(&self) -> Arc<Mutex<Settings>> {
        self.settings.clone()
    }

    pub fn stop(&self) {
        self.engine.stop();
    }
}

/// `store` overrides the settings file. A test instance must not restore the live topology: that file
/// names the user's output line and their microphone, and opening the microphone is what puts the room
/// on the wire.
pub fn start(store: Option<&str>) -> Result<App, String> {
    let engine = Engine::start(Arc::new(SystemClock));
    let mut store = match store {
        Some(path) => Settings::load_at(std::path::Path::new(path)),
        None => Settings::load(),
    };
    if store.store.as_os_str().is_empty() {
        store.store = crate::settings::store_path();
    }
    log::line(&format!(
        "settings: store='{}' mic='{}' lines={} sources={}",
        store.store.display(),
        store.mic_id,
        store.lines.len(),
        store.sources.len()
    ));
    let settings = Arc::new(Mutex::new(store));
    restore(&engine, &settings);
    Ok(App { engine, settings })
}

/// Restore order is lines, then sources, then microphone routes, then the microphone; every stored line
/// id is translated through the line it names.
fn restore(engine: &Arc<Engine>, settings: &Arc<Mutex<Settings>>) {
    let store = settings
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let mut translated: HashMap<u32, u32> = HashMap::new();
    for line in &store.lines {
        match engine.add_line(&line.device_id) {
            Ok(id) => {
                if !line.label.is_empty() {
                    if let Err(err) = engine.set_line_label(id, &line.label) {
                        log::line(&format!("restore label for line {id} failed: {err}"));
                    }
                }
                if id != line.id {
                    log::line(&format!("line {} restored as {id}", line.id));
                }
                translated.insert(line.id, id);
            }
            Err(err) => log::line(&format!(
                "line {} could not come back: {err}",
                line.device_id
            )),
        }
    }

    for source in &store.sources {
        let lines: Vec<u32> = source
            .lines
            .iter()
            .filter_map(|id| translated.get(id).copied())
            .collect();
        let identity = Identity {
            name: source.name.clone(),
            path: source.path.clone(),
        };
        let id = engine.restore_source(identity, &source.title, source.gain, &lines);
        let name = &source.name;
        let gain = source.gain;
        log::line(&format!(
            "source restored: id={id} '{name}' gain={gain:.2} lines={lines:?}"
        ));
    }

    for stored in &store.mic_routes {
        if let Some(live) = translated.get(stored) {
            if let Err(err) = engine.set_mic_route(*live, true) {
                log::line(&format!("restore mic route to line {live} failed: {err}"));
            }
        }
    }

    // Before the capture opens, so a store that says "muted" never routes a live block first. A
    // restored switch is not a user action, so this one does not play the cue.
    let _ = engine.set_mic_muted(store.mic_muted);

    if !store.mic_id.is_empty() {
        if let Err(err) = engine.restore_mic(&store.mic_id, store.mic_gain) {
            log::line(&format!("microphone '{}' could not be opened: {err}", store.mic_id));
        }
    }

    // The live ids are the truth now; write them back so the next launch translates nothing.
    api::persist_topology(engine, settings);
}

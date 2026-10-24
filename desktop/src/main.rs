//! The desktop shell: the window, the tray, and the commands behind them.
//!
//! The engine, the settings store, the tray and its self-drawn menu all live in `micdeck_lib`; this
//! binary adds the webview window and replaces the HTTP socket with Tauri's IPC. The page still speaks
//! the route table verbatim — `api(method, path, body)` answers with exactly the JSON body the server
//! used to write to the socket — so the contract the UI reads is the one `src/api.rs` tests.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;
use tauri::{Emitter, Manager, RunEvent, WindowEvent};

use micdeck_lib::api;
use micdeck_lib::engine::Engine;
use micdeck_lib::log;
use micdeck_lib::settings::Settings;
use micdeck_lib::tray;

/// How often a state snapshot is pushed. The meters integrate a -60 dB/s decay, so a slower cadence
/// shows up as visible stepping rather than as a saving.
const PUSH_INTERVAL: Duration = Duration::from_millis(30);

#[derive(Clone)]
struct Hub {
    engine: Arc<Engine>,
    settings: Arc<Mutex<Settings>>,
}

/// One request against the route table. Errors are not exceptions here: a refused edit comes back as
/// `{"error": …}` in the body, which is what the page reads.
#[tauri::command]
async fn api_request(
    hub: tauri::State<'_, Hub>,
    method: String,
    path: String,
    body: String,
) -> Result<Value, String> {
    Ok(api::handle(&hub.engine, &hub.settings, &method, &path, &body))
}

/// One process icon as `{mime, bytes}`; the page turns the bytes into an object url and caches it per
/// pid, which is what the browser's image cache used to do.
#[tauri::command]
async fn icon(pid: u32) -> Value {
    api::icon(pid)
}


/// `--settings <file>` keeps a test instance off the live store, `--seconds <n>` bounds a smoke run and
/// `--log <file>` records it. Everything else is a usage error: a double-click has no arguments at all
/// and starts the product.
struct Options {
    settings: Option<String>,
    seconds: Option<f64>,
    log: Option<String>,
}

fn parse_options(args: &[String]) -> Result<Options, String> {
    let mut options = Options {
        settings: None,
        seconds: None,
        log: None,
    };
    let mut i = 0;
    while i < args.len() {
        let flag = args[i].clone();
        let mut value = |name: &str| -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("missing value for {name}"))
        };
        match flag.as_str() {
            "--settings" => options.settings = Some(value("--settings")?),
            "--log" => options.log = Some(value("--log")?),
            "--seconds" => {
                let text = value("--seconds")?;
                let seconds: f64 = text.parse().map_err(|_| format!("bad --seconds: {text}"))?;
                if seconds <= 0.0 {
                    return Err("--seconds must be > 0".to_string());
                }
                options.seconds = Some(seconds);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }
    Ok(options)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_options(&args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };
    if let Some(path) = &options.log {
        log::init(path);
    } else {
        // The window build has no console, so the record has to go somewhere the user can find.
        let path = std::env::temp_dir().join("MicDeck-desktop.log");
        log::init(&path.to_string_lossy());
    }

    // A second double-click should raise the window that is already there instead of starting a second
    // engine on the same endpoints. Test runs are exempt on purpose: `--settings`/`--seconds` exist to run
    // beside the product, and one of them is what CI would use.
    let _single = if options.settings.is_none() && options.seconds.is_none() {
        match micdeck_lib::platform::single_instance() {
            Ok(Some(guard)) => Some(guard),
            Ok(None) => {
                let focused = micdeck_lib::platform::focus_existing();
                log::line(&format!(
                    "another instance is running (window focused: {focused}); exiting"
                ));
                return;
            }
            Err(err) => {
                // Not a reason to refuse to start: a missing guard is worse than a lost one.
                log::line(&format!("single-instance guard unavailable: {err}"));
                None
            }
        }
    } else {
        None
    };

    // The restore talks to WASAPI, and `wasapi` initialises COM as multithreaded on whichever thread
    // asks it to. tao needs a pristine main thread to call `OleInitialize` on — with a saved topology
    // the restore alone poisoned it and the window could never be built (`RPC_E_CHANGED_MODE`). So the
    // whole startup happens on a worker and the main thread stays apartment-free.
    let store = options.settings.clone();
    let started = std::thread::spawn(move || micdeck_lib::app::start(store.as_deref())).join();
    let app = match started {
        Ok(Ok(app)) => app,
        Ok(Err(err)) => {
            log::line(&format!("startup failed: {err}"));
            std::process::exit(3);
        }
        Err(_) => {
            log::line("startup failed: the restore thread died");
            std::process::exit(3);
        }
    };

    let hub = Hub {
        engine: app.engine.clone(),
        settings: app.settings.clone(),
    };

    let built = tauri::Builder::default()
        .manage(hub)
        .invoke_handler(tauri::generate_handler![api_request, icon])
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api: guard, .. } = event {
                // Closing the window is not quitting: the tray and the engine stay up, and the page's
                // own loops stop while it is out of sight.
                guard.prevent_close();
                let _ = window.hide();
            }
        })
        .setup(move |app| {
            let handle = app.handle().clone();
            start_pushing(handle.clone());
            start_tray(handle, app.state::<Hub>().inner().clone(), options.seconds);
            Ok(())
        })
        .build(tauri::generate_context!());

    let app = match built {
        Ok(app) => app,
        Err(err) => {
            log::line(&format!("the window could not be built: {err}"));
            std::process::exit(4);
        }
    };

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            handle.state::<Hub>().engine.stop();
            log::line("desktop: stopped");
        }
    });
}

/// Pushes a snapshot every 30 ms while the window is on screen. Hidden is the same case the browser
/// tab used to be: nothing is shown, so nothing is worth serialising.
fn start_pushing(handle: tauri::AppHandle) {
    let hub = handle.state::<Hub>().inner().clone();
    std::thread::spawn(move || loop {
        let started = Instant::now();
        let visible = handle
            .get_webview_window("main")
            .is_some_and(|window| window.is_visible().unwrap_or(false));
        if visible {
            let value = serde_json::to_value(hub.engine.snapshot()).unwrap_or(Value::Null);
            let _ = handle.emit("state", value);
        }
        let elapsed = started.elapsed();
        if elapsed < PUSH_INTERVAL {
            std::thread::sleep(PUSH_INTERVAL - elapsed);
        }
    });
}

/// The tray owns the process's other half: its menu's 退出 ends the run, and its "打开 MicDeck" is what
/// brings the window back. It runs on its own thread with its own message loop, which is how
/// `micdeck_lib::tray` was written.
fn start_tray(handle: tauri::AppHandle, hub: Hub, seconds: Option<f64>) {
    let limit = seconds.filter(|seconds| *seconds > 0.0);
    let opener = handle.clone();
    std::thread::spawn(move || {
        let to_show = opener.clone();
        let shown = move || {
            let closer = to_show.clone();
            let on_thread = to_show.clone();
            let _ = closer.run_on_main_thread(move || {
                if let Some(window) = on_thread.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            });
        };
        if let Err(err) = tray::run(hub.engine, hub.settings, limit, shown) {
            log::line(&format!("tray failed: {err}"));
        }
        // The tray ended — by its menu, by a deadline, or by failing — and it is the only thing that
        // keeps this process alive besides the window the user may have closed.
        opener.exit(0);
    });
}

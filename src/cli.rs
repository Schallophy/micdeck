#[derive(Debug)]
pub enum Command {
    ListDevices,
    CapturePid {
        pid: u32,
        out: String,
        seconds: f64,
        include_tree: bool,
    },
    CaptureMic {
        device: Option<String>,
        out: String,
        seconds: f64,
        low_latency: bool,
    },
    CaptureLoopback {
        device: Option<String>,
        out: String,
        seconds: f64,
    },
    ListSessions {
        seconds: f64,
    },
    EngineSmoke {
        lines: Vec<String>,
        pid: Option<u32>,
        mic_device: Option<String>,
        seconds: f64,
    },
    /// Runs the real application: engine, restored settings, web UI. `limit` is `--seconds` when the
    /// caller gave it, which is how an automated run stops itself; none means run until killed.
    /// `tray` adds the notification-icon entry, which is what owns the process lifetime.
    Web {
        port: u16,
        browser: bool,
        settings: Option<String>,
        limit: Option<f64>,
        tray: bool,
    },
    /// Reads one process's icon, or the application's own mark with `--icon brand`, as the bytes the UI
    /// would receive — for checking the encoder against a decoder.
    DumpIcon {
        target: String,
        out: String,
    },
    RenderTone {
        device: Option<String>,
        seconds: f64,
        hz: f64,
        level: f64,
        low_latency: bool,
        buffer_ms: u32,
        capture_back: bool,
    },
}

/// One entry per line, joined on print. A `\` line continuation in a Rust string literal swallows the
/// leading whitespace of the next line, which is the indentation these entries need.
const USAGE_LINES: &[&str] = &[
    "MicDeck (Rust bench) - WASAPI plumbing proof, no mixing yet",
    "",
    "  --list-devices                 endpoints with mix format, shared-mode engine period and low-latency verdict",
    "  --list-audio-sessions          render sessions per process, with the peak reached while sampling",
    "  --capture-pid <n> --out <wav>  process loopback of one app tree (--pid is the same flag)",
    "  --capture-mic [--device s]     microphone",
    "  --capture-loopback [--device s] whole render endpoint (control path)",
    "  --engine --line <id|name>      run the real engine on one line: optional --pid source, --mic-device",
    "                                 --line may be repeated to run two lines at once",
    "  --web [--port n] [--no-browser]  run the product: engine, restored settings, UI on 127.0.0.1",
    "  --tray                           add the notification-icon entry; implies --web, and is what",
    "                                   running the program with no arguments at all means",
    "                                   with --seconds n the run stops itself after n seconds",
    "                                   --settings <file> reads and writes that file instead of %APPDATA%",
    "  --icon <pid|brand> [--out f.ico]  write that process's icon, or our own mark, as /api/icon serves it",
    "  --render-tone [--device s]     event-driven shared render, optional loopback of the same endpoint",
    "",
    "  --seconds <n>  --out <file.wav>  --low-latency  --buffer-ms <n>  --hz <f>  --level <f>",
    "  --exclude      with --capture-pid, capture everything except the target tree",
    "  --log <file>   also write the log to a file",
];

pub fn parse(args: &[String]) -> Result<Command, String> {
    // No arguments is how the product is started: web UI, tray entry, open the page, stay up.
    if args.is_empty() {
        return Ok(Command::Web {
            port: 8765,
            browser: true,
            settings: None,
            limit: None,
            tray: true,
        });
    }
    let mut pid: Option<u32> = None;
    let mut mic = false;
    let mut loopback = false;
    let mut render = false;
    let mut engine = false;
    let mut web = false;
    let mut tray = false;
    let mut port: u16 = 8765;
    let mut browser = true;
    let mut settings_path: Option<String> = None;
    let mut seconds_given = false;
    let mut icon_target: Option<String> = None;
    let mut line_selectors: Vec<String> = Vec::new();
    let mut mic_device: Option<String> = None;
    let mut list = false;
    let mut sessions = false;
    let mut capture_back = false;
    let mut exclude = false;
    let mut low_latency = false;
    let mut out = String::new();
    let mut device: Option<String> = None;
    let mut seconds = 10.0f64;
    let mut hz = 1000.0f64;
    let mut level = 0.2f64;
    let mut buffer_ms = 100u32;

    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut next = |name: &str| -> Result<String, String> {
            i += 1;
            args.get(i)
                .cloned()
                .ok_or_else(|| format!("missing value for {name}"))
        };
        match a {
            "--list-devices" => list = true,
            "--list-audio-sessions" => sessions = true,
            // `--pid` is the product's own flag name; `--engine` reads it as the source to attach.
            "--capture-pid" | "--pid" => {
                pid = Some(next(a)?.parse::<u32>().map_err(|e| format!("bad pid: {e}"))?);
            }
            "--capture-mic" => mic = true,
            "--capture-loopback" => loopback = true,
            "--render-tone" => render = true,
            "--engine" => engine = true,
            "--web" => web = true,
            // The tray owns the process lifetime, so asking for it means asking for the server too.
            "--tray" => {
                tray = true;
                web = true;
            }
            "--icon" => {
                let target = next(a)?;
                if target != "brand" && target.parse::<u32>().is_err() {
                    return Err(format!("--icon takes a pid or the word brand, got {target}"));
                }
                icon_target = Some(target);
            }
            "--settings" => settings_path = Some(next(a)?),
            "--no-browser" => browser = false,
            "--port" => {
                port = next(a)?
                    .parse::<u16>()
                    .map_err(|e| format!("bad --port: {e}"))?
            }
            "--line" => line_selectors.push(next(a)?),
            "--mic-device" => mic_device = Some(next(a)?),
            "--capture-back" => capture_back = true,
            "--exclude" => exclude = true,
            "--low-latency" => low_latency = true,
            "--device" => device = Some(next(a)?),
            "--out" => out = next(a)?,
            "--seconds" => {
                seconds = next(a)?
                    .parse::<f64>()
                    .map_err(|e| format!("bad --seconds: {e}"))?;
                seconds_given = true;
            }
            "--hz" => hz = next(a)?.parse::<f64>().map_err(|e| format!("bad --hz: {e}"))?,
            "--level" => {
                level = next(a)?
                    .parse::<f64>()
                    .map_err(|e| format!("bad --level: {e}"))?
            }
            "--buffer-ms" => {
                buffer_ms = next(a)?
                    .parse::<u32>()
                    .map_err(|e| format!("bad --buffer-ms: {e}"))?
            }
            "--log" => {
                crate::log::init(&next(a)?);
            }
            "--help" | "-h" => return Err(String::new()),
            other => return Err(format!("unknown argument: {other}")),
        }
        i += 1;
    }

    let capture_mode = pid.is_some() && !engine;
    let chosen = [
        capture_mode,
        mic,
        loopback,
        render,
        engine,
        web,
        icon_target.is_some(),
    ];
    let modes = chosen.iter().filter(|on| **on).count();
    if modes + [list, sessions].iter().filter(|on| **on).count() != 1 {
        return Err(
            "exactly one of --list-devices, --list-audio-sessions, --capture-pid, --capture-mic, --capture-loopback, --render-tone, --engine, --web, --icon is required"
                .to_string(),
        );
    }
    if seconds <= 0.0 {
        return Err("--seconds must be > 0".to_string());
    }
    if let Some(target) = icon_target {
        return Ok(Command::DumpIcon { target, out });
    }

    if engine {
        if line_selectors.is_empty() {
            return Err("--engine requires --line <device id or name>".to_string());
        }
        return Ok(Command::EngineSmoke {
            lines: line_selectors,
            pid,
            mic_device,
            seconds,
        });
    }
    if web {
        return Ok(Command::Web {
            port,
            browser,
            settings: settings_path,
            limit: if seconds_given { Some(seconds) } else { None },
            tray,
        });
    }
    if capture_mode || mic || loopback {
        if out.is_empty() {
            return Err("--out is required".to_string());
        }
        let out = out.clone();
        if let Some(pid) = pid {
            return Ok(Command::CapturePid {
                pid,
                out,
                seconds,
                include_tree: !exclude,
            });
        }
        if mic {
            return Ok(Command::CaptureMic {
                device,
                out,
                seconds,
                low_latency,
            });
        }
        return Ok(Command::CaptureLoopback {
            device,
            out,
            seconds,
        });
    }
    if render {
        return Ok(Command::RenderTone {
            device,
            seconds,
            hz,
            level,
            low_latency,
            buffer_ms,
            capture_back,
        });
    }
    if sessions {
        return Ok(Command::ListSessions { seconds });
    }
    Ok(Command::ListDevices)
}

pub fn usage_text() -> String {
    USAGE_LINES.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &[&str]) -> Result<Command, String> {
        super::parse(&text.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    /// Starting the program with nothing is how a double-click arrives, and it means the product: server
    /// plus tray, browser opened, no self-imposed deadline.
    #[test]
    fn no_arguments_means_the_product_with_a_tray_entry() {
        let command = parse(&[]).expect("no arguments must be a valid run");
        match command {
            Command::Web {
                port,
                browser,
                settings,
                limit,
                tray,
            } => {
                assert_eq!(port, 8765);
                assert!(browser, "a first run should show the page");
                assert!(tray);
                assert!(settings.is_none());
                assert_eq!(limit, None);
            }
            other => panic!("expected a web run, got {other:?}"),
        }
    }

    #[test]
    fn asking_for_the_tray_asks_for_the_server_too() {
        match parse(&["--tray"]).unwrap() {
            Command::Web { tray, browser, .. } => {
                assert!(tray);
                assert!(browser, "--tray alone still opens the page");
            }
            other => panic!("expected a web run, got {other:?}"),
        }
        // The port may be moved, and a smoke run may close the browser and bound itself.
        match parse(&["--tray", "--port", "9001", "--no-browser", "--seconds", "7"]).unwrap() {
            Command::Web {
                port,
                browser,
                limit,
                tray,
                ..
            } => {
                assert_eq!(port, 9001);
                assert!(!browser);
                assert_eq!(limit, Some(7.0));
                assert!(tray);
            }
            other => panic!("expected a web run, got {other:?}"),
        }
    }

    /// `--web` without `--seconds` runs until the tray or Ctrl+C ends it; a `--seconds` that was not
    /// passed must not silently bound a product run at the default ten.
    #[test]
    fn only_an_explicit_seconds_bounds_a_web_run() {
        match parse(&["--web"]).unwrap() {
            Command::Web { limit, tray, .. } => {
                assert_eq!(limit, None);
                assert!(!tray, "--web alone is the server without an icon");
            }
            other => panic!("expected a web run, got {other:?}"),
        }
    }

    #[test]
    fn the_icon_mode_is_a_diagnostic_and_not_a_second_product_run() {
        match parse(&["--icon", "4321", "--out", "C:\\tmp\\x.ico"]).unwrap() {
            Command::DumpIcon { target, out } => {
                assert_eq!(target, "4321");
                assert_eq!(out, "C:\\tmp\\x.ico");
            }
            other => panic!("expected an icon dump, got {other:?}"),
        }
        // The word `brand` is the application's own mark rather than a pid.
        match parse(&["--icon", "brand"]).unwrap() {
            Command::DumpIcon { target, out } => {
                assert_eq!(target, "brand");
                assert!(out.is_empty(), "an omitted --out names itself");
            }
            other => panic!("expected an icon dump, got {other:?}"),
        }
        assert!(parse(&["--icon", "CABLE"]).is_err());
        assert!(parse(&["--icon", "4321", "--web"]).is_err());
        assert!(parse(&["--icon", "-3"]).is_err());
    }

    #[test]
    fn one_mode_per_run_and_no_silent_default() {
        assert!(parse(&["--web", "--list-devices"]).is_err());
        assert!(parse(&["--capture-mic", "--capture-loopback"]).is_err());
        assert!(parse(&["--tray", "--seconds", "0"]).is_err());
        assert!(parse(&["--nonsense"]).is_err());
        // Nothing at all is the product, not an error — so the usage text must not be what a
        // double-click prints.
        assert!(parse(&[]).is_ok());
    }
}

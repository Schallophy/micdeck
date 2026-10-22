//! A tiny HTTP/1.1 server on a raw socket bound to 127.0.0.1. `HttpListener` would need a URL ACL on a
//! non-admin account; a `TcpListener` does not, and the loopback bind keeps the UI off the network.

pub mod api;

#[cfg(test)]
mod tests;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::engine::Engine;
use crate::log;
use crate::settings::Settings;

/// How often a state snapshot is pushed. The meters integrate a -60 dB/s decay, so a slower cadence
/// shows up as visible stepping rather than as a saving.
const SSE_INTERVAL: Duration = Duration::from_millis(30);
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const ACCEPT_POLL: Duration = Duration::from_millis(100);

pub struct WebUi {
    url: String,
    stop: Arc<AtomicBool>,
    /// Set by the accept loop when it ends, whether it was asked to stop or it fell over. The tray's
    /// tick watches this, because a dead server means a product with nothing left to do.
    stopped: Arc<AtomicBool>,
}

impl WebUi {
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn stopped_signal(&self) -> Arc<AtomicBool> {
        self.stopped.clone()
    }

    /// Stops accepting and lets the connection threads notice. Nothing waits for them: a live EventSource
    /// notices within one 30 ms tick, and a socket that nobody is talking to is closed by the process exit.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Binds `port`, or the next free one up to ten attempts, and serves until `stop`.
pub fn start(
    engine: Arc<Engine>,
    settings: Arc<Mutex<Settings>>,
    port: u16,
    open_browser: bool,
) -> Result<WebUi, String> {
    let mut bound: Option<TcpListener> = None;
    let mut used = port;
    for attempt in 0..10 {
        used = port + attempt;
        match TcpListener::bind(("127.0.0.1", used)) {
            Ok(listener) => {
                bound = Some(listener);
                break;
            }
            Err(err) => log::line(&format!("port {used} unavailable: {err}")),
        }
    }
    let listener = bound.ok_or_else(|| format!("no free port for the web UI from {port}"))?;
    // A nonblocking accept is how the loop notices the stop flag: there is no portable way to interrupt
    // a blocking accept, and one poll every 100 ms while no client is connecting costs nothing measurable.
    let _ = listener.set_nonblocking(true);
    let url = format!("http://127.0.0.1:{used}/");
    log::line(&format!("web ui: {url}"));

    let stop = Arc::new(AtomicBool::new(false));
    let stopped = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let loop_stopped = stopped.clone();
    let saw_client = Arc::new(AtomicBool::new(false));
    if open_browser {
        crate::web::open_browser_when_unclaimed(&url, saw_client.clone(), server_stop.clone());
    }

    std::thread::spawn(move || {
        let mut parked = false;
        while !server_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _peer)) => {
                    parked = false;
                    let engine = engine.clone();
                    let settings = settings.clone();
                    let stop = server_stop.clone();
                    let saw_client = saw_client.clone();
                    std::thread::spawn(move || {
                        handle(stream, &engine, &settings, &stop, &saw_client);
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    if !parked {
                        parked = true;
                    }
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(err) => {
                    log::line(&format!("accept failed: {err}"));
                    break;
                }
            }
        }
        loop_stopped.store(true, Ordering::Relaxed);
        log::line("web ui stopped");
    });

    Ok(WebUi {
        url,
        stop,
        stopped,
    })
}

struct Request {
    method: String,
    target: String,
    body: String,
}

/// Reads the request line, the `Content-Length` and exactly that many body bytes. The header block is
/// capped so a garbage connection cannot pin memory, and the body is capped for the same reason.
fn read_request(stream: &mut impl Read) -> Option<Request> {
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(found) = find_header_end(&buffer) {
            break found;
        }
        if buffer.len() > MAX_HEADER_BYTES {
            return None;
        }
    };

    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next()?.split_whitespace().collect::<Vec<_>>();
    if first.len() < 2 {
        return None;
    }
    let method = first[0].to_string();
    let target = first[1].to_string();
    let mut content_length = 0usize;
    for line in lines {
        // Lowercased as a whole line rather than sliced: a header is lossily decoded UTF-8, and indexing
        // it at a byte offset can land inside a character.
        let folded = line.to_ascii_lowercase();
        if let Some(rest) = folded.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }
    let content_length = content_length.min(MAX_BODY_BYTES);
    let body_start = header_end + 4;
    let mut body = buffer[body_start..].to_vec();
    while body.len() < content_length {
        let read = match stream.read(&mut chunk) {
            Ok(read) if read > 0 => read,
            _ => break,
        };
        let want = content_length - body.len();
        body.extend_from_slice(&chunk[..read.min(want)]);
    }
    body.truncate(content_length);
    Some(Request {
        method,
        target,
        body: String::from_utf8_lossy(&body).into_owned(),
    })
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn handle(
    mut stream: TcpStream,
    engine: &Arc<Engine>,
    settings: &Arc<Mutex<Settings>>,
    stop: &Arc<AtomicBool>,
    saw_client: &Arc<AtomicBool>,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let request = match read_request(&mut stream) {
        Some(request) => request,
        None => return,
    };
    // Only the page asks for these three; `/api/state` is left out so a script or a probe cannot make
    // the server think a tab is already open.
    if request.method == "GET"
        && (request.target == "/"
            || request.target.starts_with("/api/events")
            || request.target == "/api/ping")
    {
        let _ = saw_client.store(true, Ordering::Relaxed);
    }
    if request.method == "GET" && request.target.starts_with("/api/events") {
        if let Err(err) = stream.set_read_timeout(None) {
            let _ = err;
        }
        stream_events(&mut stream, engine, stop);
        return;
    }
    let response = api::route(engine, settings, &request.method, &request.target, &request.body);
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        response.status,
        response.reason(),
        response.content_type,
        response.body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&response.body);
    let _ = stream.flush();
}

/// Server-sent events: one snapshot per tick until the write fails, which is how a closed `EventSource`
/// shows up. There is no heartbeat: the payload itself is the traffic.
fn stream_events(stream: &mut TcpStream, engine: &Arc<Engine>, stop: &Arc<AtomicBool>) {
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\n\
                Connection: keep-alive\r\nX-Accel-Buffering: no\r\n\r\n";
    if stream.write_all(head.as_bytes()).is_err() || stream.flush().is_err() {
        return;
    }
    while !stop.load(Ordering::Relaxed) {
        let started = Instant::now();
        let json = serde_json::to_string(&engine.snapshot()).unwrap_or_else(|_| "{}".to_string());
        if stream
            .write_all(format!("data: {json}\n\n").as_bytes())
            .and_then(|_| stream.flush())
            .is_err()
        {
            return; // the client closed the EventSource
        }
        let elapsed = started.elapsed();
        if elapsed < SSE_INTERVAL {
            std::thread::sleep(SSE_INTERVAL - elapsed);
        }
    }
}

/// Opens the page only if no tab from a previous run has already polled us. A hidden tab stops all
/// traffic by design, so it cannot be detected.
fn open_browser_when_unclaimed(url: &str, saw_client: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    let url = url.to_string();
    std::thread::spawn(move || {
        for _ in 0..20 {
            if saw_client.load(Ordering::Relaxed) {
                log::line("browser tab already open; not launching another");
                return;
            }
            if stop.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        if saw_client.load(Ordering::Relaxed) {
            return;
        }
        open_page(&url);
    });
}

/// Opens a url in the default browser. The tray's "open MicDeck" and the startup launch both come here.
pub(crate) fn open_page(url: &str) {
    #[cfg(windows)]
    {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        let wide: Vec<u16> = OsStr::new(url).encode_wide().chain(Some(0)).collect();
        let verb: Vec<u16> = OsStr::new("open").encode_wide().chain(Some(0)).collect();
        let hit = unsafe {
            ShellExecuteW(
                None,
                PCWSTR(verb.as_ptr()),
                PCWSTR(wide.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            )
        };
        // ShellExecuteW reports failure as a small integer in the handle it returns, not as an error.
        let value = hit.0 as isize;
        if value <= 33 {
            log::line(&format!("open browser failed: shell code {value}"));
        } else {
            log::line("opened the web ui in the default browser");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = url;
    }
}

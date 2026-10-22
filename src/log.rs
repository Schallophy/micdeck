use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

static CLOCK: OnceLock<Instant> = OnceLock::new();
static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn init(path: &str) {
    CLOCK.get_or_init(Instant::now);
    let dir = std::path::Path::new(path).parent();
    if let Some(dir) = dir {
        if !dir.as_os_str().is_empty() {
            let _ = std::fs::create_dir_all(dir);
        }
    }
    let file = OpenOptions::new().create(true).write(true).truncate(true).open(path);
    *FILE.lock().unwrap() = file.ok();
}

/// Whether a log file is open. The product path uses this to decide whether to fall back to the temp
/// log, which matters when the process was started with no console to write to.
pub fn path_is_set() -> bool {
    FILE.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_some()
}

/// One line to the console and, when a log path was set, to the file. Shared by the CLI
/// diagnostics and the engine, so a timestamp in a log is the same clock everywhere.
pub fn line(message: &str) {
    let started = CLOCK.get_or_init(Instant::now);
    let elapsed = started.elapsed().as_secs_f64();
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let text = format!("{} [+{:.1}s] {}", clock_time(wall), elapsed, message);
    // The tray build starts with no console at all, and `println!` panics when the write fails. There
    // is nothing to recover from here — the log file is the record when stdout is not connected.
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{text}");
    let _ = out.flush();
    let mut guard = FILE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(file) = guard.as_mut() {
        let _ = file.write_all(text.as_bytes());
        let _ = file.write_all(b"\n");
        let _ = file.flush();
    }
}

/// Local `yyyy-MM-dd HH:mm:ss.mmm` without pulling in a date-time crate: break the epoch
/// millis into days and seconds-of-day, then convert the day count to a civil date.
fn clock_time(epoch_ms: i64) -> String {
    let millis = epoch_ms.rem_euclid(1000);
    let total_seconds = epoch_ms.div_euclid(1000);
    let days = total_seconds.div_euclid(86_400);
    let sod = total_seconds.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
        y,
        m,
        d,
        sod / 3600,
        (sod / 60) % 60,
        sod % 60,
        millis
    )
}

/// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

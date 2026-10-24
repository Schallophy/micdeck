//! One MicDeck per session.
//!
//! Two instances would fight over the same endpoints: the second one opens the same lines, registers the
//! same global hotkey and puts a second icon in the tray. A named mutex is the cheapest honest guard — the
//! name lives in the session's object directory, so a second user on the same machine is not affected.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, SetForegroundWindow, ShowWindow, SW_RESTORE};

const MUTEX: PCWSTR = w!("Local\\MicDeck-single");

/// Holds the kernel object open for as long as it lives; dropping it lets the next start in.
pub struct SingleInstance {
    handle: HANDLE,
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// `Ok(Some(_))` when this process is the only one, `Ok(None)` when another instance already owns the
/// name. The caller decides what a second start means — the product brings the running window forward.
pub fn acquire() -> Result<Option<SingleInstance>, String> {
    acquire_named(MUTEX)
}

fn acquire_named(name: PCWSTR) -> Result<Option<SingleInstance>, String> {
    let handle = unsafe { CreateMutexW(None, true, name) }
        .map_err(|err| format!("single-instance mutex: {err}"))?;
    let already_open = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
    if already_open {
        unsafe {
            let _ = CloseHandle(handle);
        }
        return Ok(None);
    }
    Ok(Some(SingleInstance { handle }))
}

/// Brings the already-running window to the front. The window's title is what the shell shows, so it is
/// also what a second start can find it by; `false` means there was no window to find (the tray may still
/// be up, in which case the second start simply ends).
pub fn focus_existing() -> bool {
    let title = w!("MicDeck");
    let found = unsafe { FindWindowW(PCWSTR::null(), title) };
    match found {
        Ok(hwnd) if !hwnd.0.is_null() => {
            unsafe {
                let _ = ShowWindow(hwnd, SW_RESTORE);
                let _ = SetForegroundWindow(hwnd);
            }
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The mechanism is "the second open of the same name sees ERROR_ALREADY_EXISTS", so it is pinned down
    /// on the OS rather than trusted. A name of its own keeps the test from colliding with a running
    /// product on the same session.
    #[test]
    fn a_second_acquire_says_someone_else_has_it_and_dropping_releases_it() {
        let name: Vec<u16> = format!("Local\\MicDeck-test-{}", std::process::id())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let first = acquire_named(PCWSTR(name.as_ptr())).expect("first");
        assert!(first.is_some(), "the first acquire should win");
        let second = acquire_named(PCWSTR(name.as_ptr())).expect("second");
        assert!(second.is_none(), "the second acquire should stand down");
        drop(first);
        let third = acquire_named(PCWSTR(name.as_ptr())).expect("third");
        assert!(third.is_some(), "the name should be free again after the guard drops");
    }
}

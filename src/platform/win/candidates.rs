//! Every window the user could pick, filtered the way Alt+Tab filters. This runs when the add list is
//! opened; no audio metric is read here — the levels in that list come from the engine's preview
//! captures, which are real process loopbacks.

use std::cell::RefCell;
use std::collections::HashMap;

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowLongW, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, GetWindowRect, IsWindowVisible, GW_OWNER, GWL_EXSTYLE,
    WS_EX_TOOLWINDOW,
};

/// One pickable window, collapsed to one row per process: the longest title wins.
#[derive(Clone, Debug, Default)]
pub struct WindowRow {
    pub pid: u32,
    pub name: String,
    pub title: String,
}

thread_local! {
    static ROWS: RefCell<HashMap<u32, WindowRow>> = RefCell::new(HashMap::new());
}

/// An IME host or a cloaked UWP window is "visible" to `IsWindowVisible` but is not a window the user
/// can choose, so the same four tests the shell applies are applied here.
unsafe fn is_user_facing(hwnd: HWND) -> bool {
    if (GetWindowLongW(hwnd, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0) != 0 {
        return false;
    }
    // An owned window is a dialog or a tool palette, not an alt-tab target.
    if let Ok(owner) = GetWindow(hwnd, GW_OWNER) {
        if !owner.is_invalid() {
            return false;
        }
    }
    let mut cloaked: i32 = 0;
    if dwm_cloaked(hwnd, &mut cloaked) && cloaked != 0 {
        return false;
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return false;
    }
    rect.right - rect.left > 0 && rect.bottom - rect.top > 0
}

/// `DwmGetWindowAttribute(DWMWA_CLOAKED)` — the only reliable way to see a virtual-desktop or
/// shell-minimised window that still reports itself visible.
unsafe fn dwm_cloaked(hwnd: HWND, out: &mut i32) -> bool {
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
    DwmGetWindowAttribute(
        hwnd,
        DWMWA_CLOAKED,
        out as *mut i32 as *mut core::ffi::c_void,
        std::mem::size_of::<i32>() as u32,
    )
    .is_ok()
}

unsafe extern "system" fn collect(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if !IsWindowVisible(hwnd).as_bool() {
            return;
        }
        if !is_user_facing(hwnd) {
            return;
        }
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        // pid 4 is the System process; our own windows would be rows the user cannot meaningfully pick.
        if pid == 0 || pid == 4 || pid == crate::platform::self_pid() {
            return;
        }
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return;
        }
        let mut buffer = vec![0u16; (len as usize) + 1];
        let written = GetWindowTextW(hwnd, &mut buffer);
        if written <= 0 {
            return;
        }
        let title = String::from_utf16_lossy(&buffer[..written as usize]);
        if title.trim().is_empty() {
            return;
        }
        ROWS.with(|rows| {
            let mut rows = rows.borrow_mut();
            let row = rows.entry(pid).or_insert_with(|| WindowRow {
                pid,
                name: crate::platform::process_name(pid),
                title: String::new(),
            });
            if title.chars().count() > row.title.chars().count() {
                row.title = title;
                // This enumeration is the only moment every pickable window is walked, so the row's
                // representative window is recorded here for the icon cache to fall back on.
                super::icon::note_window(pid, hwnd);
            }
        });
    }));
    // A panic here would unwind through a `system` call into user32's callback frame; swallow it and let
    // the enumeration continue with the next window.
    if outcome.is_err() {
        crate::log::line("window enumeration skipped a window after a panic");
    }
    BOOL::from(true)
}

pub fn list_window_candidates() -> Vec<WindowRow> {
    ROWS.with(|rows| rows.borrow_mut().clear());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(0));
    }));
    if outcome.is_err() {
        crate::log::line("window enumeration failed");
    }
    let mut rows: Vec<WindowRow> = ROWS.with(|rows| rows.borrow().values().cloned().collect());
    rows.sort_by_key(|row| {
        let label = if row.title.trim().is_empty() {
            row.name.as_str()
        } else {
            row.title.as_str()
        };
        label.to_lowercase()
    });
    rows
}

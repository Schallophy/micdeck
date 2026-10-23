//! The tray entry: the icon, the menu under it, and the reason the process stays alive.
//!
//! The owner is a real hidden window, not a message-only one: a popup menu needs an owner it can be
//! foregrounded against, or an outside click never dismisses it. Nothing may hold the tray lock across
//! `TrackPopupMenuEx` — it dispatches the measure and draw messages straight back into this file. The
//! menu is owner-drawn because Windows paints themed menus light whatever the app theme is.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, Error as WindowsError};
use windows::Win32::Foundation::{
    COLORREF, FALSE, HWND, HINSTANCE, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, GetDC,
    GetTextExtentPoint32W, GetTextMetricsW, HBRUSH, HDC, HFONT, HGDIOBJ, ReleaseDC, SelectObject,
    SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_SUBKEY_WOW6464KEY,
};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_SELECTED, ODT_MENU};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetProcessDpiAwarenessContext,
    SystemParametersInfoForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, GetMessageW, GetCursorPos, HICON, HMENU, IDC_ARROW, KillTimer,
    LoadCursorW, MSG, MENUINFO, MF_OWNERDRAW, MIM_APPLYTOSUBMENUS, MIM_BACKGROUND, MIM_STYLE,
    MNS_NOCHECK, NONCLIENTMETRICSW, PostMessageW, PostQuitMessage, RegisterClassExW,
    SetForegroundWindow, SetMenuInfo, SetTimer, SPI_GETNONCLIENTMETRICS, TPM_BOTTOMALIGN,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, WM_APP, WM_CONTEXTMENU,
    WM_DESTROY, WM_DRAWITEM, WM_HOTKEY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_MEASUREITEM, WM_MOUSEMOVE,
    WM_NULL, WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WNDCLASS_STYLES,
    WS_EX_TOOLWINDOW, WS_POPUP,
};

use crate::engine::Engine;
use crate::log;
use crate::platform::brand;
use crate::settings::Settings;
use crate::tap;
use crate::web;

/// The callback message the shell posts when the icon is touched.
const ICON_MESSAGE: u32 = WM_APP + 1;
/// A chord edit from the web thread, to be applied on the thread that owns the window.
const REBIND_MESSAGE: u32 = WM_APP + 2;
const TIMER: usize = 1;
const TICK_MS: u32 = 500;

/// `NOTIFYICONDATAW.uVersion` — the shell only routes its own click events to an icon that declares one.
const NOTIFY_ICON_VER: u32 = 4;
/// `shellapi.h`: `NIN_SELECT`, `NIN_KEYSELECT` and, new in Windows 11 24H2, `NIN_INNERRIGHTCLICK`, which
/// is how the notification-area flyout reports a right-click. Only `NIN_SELECT` is in the `windows`
/// crate's surface, so the other two are named here rather than guessed at in a match arm.
const NIN_SELECT: u32 = 1024;
const NIN_KEYSELECT: u32 = 1025;
const NIN_INNERRIGHTCLICK: u32 = 1032;

const ITEM_OPEN: usize = 1;
const ITEM_RESTART: usize = 2;
const ITEM_EXIT: usize = 3;
const ITEM_MUTE: usize = 4;

/// `RegisterHotKey` ids are per window, not global; one is all this process needs.
const HOTKEY_ID: i32 = 1;
/// While a modifier tap is bound, this fine timer watches the key; see [`crate::tap`].
const TAP_TIMER: usize = 2;
const TAP_TICK_MS: u32 = 15;

const CLASS: &str = "MicDeckTray";

/// The two palettes `TrayApp.SyncMenuTheme` painted, as blue-first colour references.
struct Palette {
    background: COLORREF,
    foreground: COLORREF,
    hover: COLORREF,
}

const fn rgb(r: u32, g: u32, b: u32) -> COLORREF {
    COLORREF((b << 16) | (g << 8) | r)
}

const DARK: Palette = Palette {
    background: rgb(36, 35, 32),
    foreground: rgb(243, 239, 230),
    hover: rgb(46, 44, 41),
};

const LIGHT: Palette = Palette {
    background: rgb(255, 255, 255),
    foreground: rgb(31, 30, 29),
    hover: rgb(246, 243, 236),
};

#[derive(Clone)]
struct Item {
    id: usize,
    label: Vec<u16>,
}

struct Tray {
    hwnd: isize,
    engine: Arc<Engine>,
    settings: Arc<Mutex<Settings>>,
    url: String,
    /// The accept loop's own "I stopped" flag: when it goes, the product has nothing left to serve.
    stopped: Arc<AtomicBool>,
    /// Set only by a bounded smoke run; the tick treats it as a shutdown request.
    deadline: Option<Instant>,
    icon: isize,
    font: isize,
    menu: isize,
    background: isize,
    items: Vec<Item>,
    dark: bool,
    running: bool,
    muted: bool,
    /// The chord Windows actually accepted, if any. `None` covers "no shortcut", a chord another
    /// program owns, and a store that could not be parsed.
    hotkey: Option<crate::hotkey::Binding>,
    /// The state behind a modifier-tap binding: inert unless the binding is a lone modifier.
    tap: tap::Machine,
    /// Event codes the shell has already been seen reporting, so an unknown one is diagnosed once rather
    /// than on every mouse move.
    seen_events: std::collections::HashSet<u32>,
    /// When the previous left click on the icon landed, while waiting for the one that would make it a
    /// double-click.
    last_click: Option<Instant>,
}

/// One tray per process, so the window procedure reads its state here instead of through a pointer kept
/// in `GWLP_USERDATA`. Handles are stored as integers: they are neither `Send` nor `Sync`, and a
/// window procedure has no way to capture anything else.
static TRAY: LazyLock<Mutex<Option<Tray>>> = LazyLock::new(|| Mutex::new(None));

type RebindJob = (
    Option<crate::hotkey::Binding>,
    std::sync::mpsc::Sender<Rebind>,
);

/// An edit waiting for the tray thread. `RegisterHotKey` is owned by the thread that created the
/// window — another thread gets `ERROR_INVALID_WINDOW_HANDLE` — so the web thread leaves the request
/// here, posts a message, and waits for the answer.
static REBIND_JOB: LazyLock<Mutex<Option<RebindJob>>> = LazyLock::new(|| Mutex::new(None));

fn with<T>(body: impl FnOnce(&mut Tray) -> T) -> Option<T> {
    let mut guard = TRAY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_mut().map(body)
}

fn hwnd_of(value: isize) -> HWND {
    HWND(value as *mut c_void)
}

/// Runs the tray until its menu says quit, the web server stops, or `limit_seconds` elapses.
pub fn run(
    engine: Arc<Engine>,
    settings: Arc<Mutex<Settings>>,
    url: &str,
    stopped: Arc<AtomicBool>,
    limit_seconds: Option<f64>,
) -> Result<(), String> {
    // Before any window is created or any metric is read, or the menu font and the icon size come back
    // for the wrong display. It fails harmlessly when a manifest already decided this.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let status = engine.snapshot();
    let running = status.running;
    let muted = status.mic_muted;
    let icon = brand::hicon().ok_or_else(|| "could not build the tray icon".to_string())?;
    let menu = unsafe { CreatePopupMenu() }.map_err(|err| format!("popup menu: {err}"))?;
    let items = vec![
        Item {
            id: ITEM_OPEN,
            label: wide("打开 MicDeck"),
        },
        Item {
            id: ITEM_MUTE,
            label: mute_label(muted),
        },
        Item {
            id: ITEM_RESTART,
            label: wide("重启引擎"),
        },
        Item {
            id: ITEM_EXIT,
            label: wide("退出"),
        },
    ];
    unsafe {
        for item in &items {
            // Owner-draw items are given an id and no text: the label is ours to measure and paint.
            let _ = AppendMenuW(menu, MF_OWNERDRAW, item.id, PCWSTR::null());
        }
    }

    let class = wide(CLASS);
    let instance = unsafe { GetModuleHandleW(PCWSTR::null()) }
        .map_err(|err| format!("module handle: {err}"))?;
    let window_class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: WNDCLASS_STYLES(0),
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: HINSTANCE(instance.0),
        hIcon: icon,
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class.as_ptr()),
        hIconSm: icon,
    };
    if unsafe { RegisterClassExW(&window_class) } == 0 {
        return Err(format!(
            "register tray window class: {}",
            WindowsError::from_thread()
        ));
    }

    let name = wide("MicDeck");
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            PCWSTR(name.as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(instance.0)),
            None,
        )
    }
    .map_err(|err| format!("create tray window: {err}"))?;

    let dark = is_dark_theme();
    let background = unsafe { CreateSolidBrush(palette_for(dark).background) };
    unsafe { apply_menu_background(menu, background, dark) };

    let mut notify = notify_for(hwnd, icon, NIF_ICON | NIF_MESSAGE | NIF_TIP);
    write_tip(&mut notify, running, url);
    if unsafe { Shell_NotifyIconW(NIM_ADD, &notify) } == FALSE {
        unsafe {
            let _ = DestroyMenu(menu);
            let _ = DeleteObject(background.into());
            let _ = DestroyIcon(icon);
            let _ = DestroyWindow(hwnd);
        }
        return Err("the shell refused the tray icon".to_string());
    }
    // The version is declared after the icon exists, because it changes which events the shell sends at
    // all: Windows 11 24H2 keeps the overflow flyout's right-click to itself unless the icon asks for
    // version 4, and a version-0 icon never hears about the click, so the menu cannot open.
    let mut versioned = notify;
    versioned.Anonymous.uVersion = NOTIFY_ICON_VER;
    let accepted = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &versioned) } != FALSE;
    log::line(&format!(
        "tray: icon version {NOTIFY_ICON_VER} {}",
        if accepted {
            "accepted"
        } else {
            "DECLINED — a right-click inside the overflow may not open the menu"
        }
    ));

    // A taken chord degrades the feature, not the product: the menu item and the page's switch still
    // toggle the same state, so a refusal is a log line, not a startup failure.
    let stored = settings
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .hotkey
        .clone();
    let hotkey = match crate::hotkey::parse(&stored) {
        Ok(Some(binding)) => match unsafe { install(hwnd, Some(binding)) } {
            Ok(()) => {
                log::line(&format!("tray: hotkey {} registered", binding.label()));
                Some(binding)
            }
            Err(err) => {
                log::line(&format!(
                    "tray: hotkey {} REFUSED — {err}",
                    binding.label()
                ));
                None
            }
        },
        Ok(None) => {
            log::line("tray: hotkey off");
            None
        }
        Err(err) => {
            log::line(&format!("tray: hotkey '{stored}' ignored: {err}"));
            None
        }
    };

    let font = menu_font(hwnd);
    let deadline = limit_seconds.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    *TRAY.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Tray {
        hwnd: hwnd.0 as isize,
        engine,
        settings,
        url: url.to_string(),
        stopped,
        deadline,
        icon: icon.0 as isize,
        font: font.map(|font| font.0 as isize).unwrap_or(0),
        menu: menu.0 as isize,
        background: background.0 as isize,
        items,
        dark,
        running,
        muted,
        hotkey,
        tap: tap::Machine::new(match hotkey {
            Some(crate::hotkey::Binding::Tap(key)) => Some(key),
            _ => None,
        }),
        seen_events: std::collections::HashSet::new(),
        last_click: None,
    });
    unsafe {
        let _ = SetTimer(Some(hwnd), TIMER, TICK_MS, None);
    }
    log::line(&format!(
        "tray: {} theme, {} ms tick",
        if dark { "dark" } else { "light" },
        TICK_MS
    ));

    let mut message = MSG::default();
    loop {
        let got = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        // Zero is WM_QUIT; a negative return is the error the docs tell us to treat as "stop".
        if got <= 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }

    // The window is already gone by here (WM_DESTROY ended the loop); what is left is the shell's copy
    // of the icon and the GDI objects this thread made for the menu.
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER);
        let _ = Shell_NotifyIconW(NIM_DELETE, &notify);
        let _ = DestroyMenu(menu);
        if let Some(font) = font {
            let _ = DeleteObject(font.into());
        }
        let _ = DeleteObject(background.into());
        let _ = DestroyIcon(icon);
    }
    *TRAY.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    log::line("tray: closed");
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A panic unwound through this frame would land inside user32's callback, so each message is
    // handled whole and a failure costs one ignored message.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
        ICON_MESSAGE => icon_message(hwnd, wparam, lparam),
        REBIND_MESSAGE => apply_rebind(),
        WM_MEASUREITEM => measure_item(lparam),
        WM_DRAWITEM => draw_item(lparam),
        WM_TIMER => match wparam.0 as usize {
            TIMER => tick(),
            TAP_TIMER => tap_tick(),
            _ => {}
        },
        WM_SETTINGCHANGE => refresh_theme(),
        // The id in `wParam` is what tells several hotkeys on one window apart; this window owns one.
        WM_HOTKEY if wparam.0 as i32 == HOTKEY_ID => toggle_mute(true),
        WM_DESTROY => unsafe {
            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
            let _ = KillTimer(Some(hwnd), TAP_TIMER);
            PostQuitMessage(0);
        },
        _ => {}
    }));
    if outcome.is_err() {
        log::line(&format!("tray: message {msg} failed"));
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn icon_message(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    // The event and the icon id share `lParam` in two 16-bit words, and which word is which depends on
    // the shell (24H2 sends event-low). Our id is 0, so the non-zero word is the event.
    let packed = lparam.0 as u32;
    let (low, high) = (packed & 0xffff, (packed >> 16) & 0xffff);
    let event = if high != 0 { high } else { low };
    match event {
        // The shell reports one event per click and no click count; the second click is timed here.
        NIN_SELECT | NIN_KEYSELECT if is_double_click() => {
            log::line(&format!("tray: icon event {event} → second click, open page"));
            open_page();
        }
        NIN_SELECT | NIN_KEYSELECT => {
            log::line(&format!("tray: icon event {event} → first click, waiting"));
        }
        // A shell reporting raw mouse messages has already decided the gesture itself. The lone
        // button-ups around it are ignored, so a click cannot open the page through this path either.
        WM_LBUTTONDBLCLK => {
            log::line(&format!("tray: icon event {event} → double click, open page"));
            open_page();
        }
        WM_LBUTTONUP => {}
        NIN_INNERRIGHTCLICK | WM_RBUTTONUP => {
            log::line(&format!("tray: icon event {event} → menu"));
            show_menu(hwnd);
        }
        WM_CONTEXTMENU => {
            // Some shells send this as a window message of its own after the button-up. Opening the menu
            // from both would pop it a second time as soon as the first was dismissed.
            log::line("tray: icon context-menu event ignored, the button-up already opened the menu");
        }
        WM_MOUSEMOVE => {}
        other => note_unknown_event(other, wparam, lparam),
    }
}

/// Whether the click we are holding now completes a double-click, judged against the interval the user
/// set for every other double-click on this machine.
fn is_double_click() -> bool {
    let now = Instant::now();
    let window = Duration::from_millis(unsafe { GetDoubleClickTime() } as u64);
    with(|tray| paired(&mut tray.last_click, now, window)).unwrap_or(false)
}

/// The two-click gesture as a state machine: the first click only leaves a time behind, the second spends
/// the pair so a third click starts a fresh gesture instead of reopening the page.
fn paired(previous: &mut Option<Instant>, now: Instant, window: Duration) -> bool {
    match *previous {
        Some(then) if now - then <= window => {
            *previous = None;
            true
        }
        _ => {
            *previous = Some(now);
            false
        }
    }
}

/// An event code we do not act on is logged the first time it appears, with the raw parameters, and then
/// kept quiet: a shell that reports hover would otherwise bury the log, and the first sighting is the
/// whole diagnosis.
fn note_unknown_event(event: u32, wparam: WPARAM, lparam: LPARAM) {
    let first = with(|tray| tray.seen_events.insert(event)).unwrap_or(true);
    if first {
        log::line(&format!(
            "tray: icon event {event} (0x{event:x}) unhandled, wParam={} lParam={}",
            wparam.0, lparam.0
        ));
    }
}

fn open_page() {
    if let Some(url) = with(|tray| tray.url.clone()) {
        web::open_page(&url);
    }
}

fn show_menu(hwnd: HWND) {
    // Everything the popup needs is copied out first: `TrackPopupMenuEx` blocks here while it dispatches
    // the measure and draw messages back into this module, and the state lock must not be held across it.
    let Some(menu) = with(|tray| tray.menu) else { return };
    let command = unsafe {
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        // Foregrounding the owner is what lets an outside click dismiss the menu instead of being eaten.
        let _ = SetForegroundWindow(hwnd);
        let command = TrackPopupMenuEx(
            HMENU(menu as *mut c_void),
            // BOTTOMALIGN puts the menu's lower edge at the point, so it grows upward from the cursor
            // instead of covering the icon and the taskbar the click came from.
            TPM_RIGHTBUTTON.0 | TPM_RETURNCMD.0 | TPM_BOTTOMALIGN.0,
            point.x,
            point.y,
            hwnd,
            None,
        );
        // Without this nudge the shell leaves the menu's modal loop marked active and the next click on
        // the desktop does nothing.
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        command.0.max(0) as usize
    };
    match command {
        ITEM_OPEN => open_page(),
        ITEM_MUTE => toggle_mute(false),
        ITEM_RESTART => restart_engine(),
        ITEM_EXIT => unsafe {
            let _ = DestroyWindow(hwnd);
        },
        _ => {}
    }
}

/// Restarting on this thread means a second click cannot run while the first is still going.
fn restart_engine() {
    let Some(engine) = with(|tray| tray.engine.clone()) else { return };
    engine.restart_all_lines();
    log::line("tray: engine restarted");
}

/// Puts a binding into force. Chords stay with `RegisterHotKey` — `MOD_NOREPEAT` makes holding the
/// chord one toggle, not a stream of them. A modifier tap cannot be a hot key (Windows accepts the
/// registration of a bare modifier and then never delivers it), so a fine timer watches the key.
unsafe fn install(hwnd: HWND, binding: Option<crate::hotkey::Binding>) -> Result<(), String> {
    match binding {
        Some(crate::hotkey::Binding::Chord { mods, vk }) => {
            let _ = KillTimer(Some(hwnd), TAP_TIMER);
            RegisterHotKey(
                Some(hwnd),
                HOTKEY_ID,
                HOT_KEY_MODIFIERS(mods | MOD_NOREPEAT.0),
                vk,
            )
            .map_err(|err| err.to_string())
        }
        Some(crate::hotkey::Binding::Tap(key)) => {
            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
            SetTimer(Some(hwnd), TAP_TIMER, TAP_TICK_MS, None);
            log::line(&format!(
                "tray: tap watch on {} ({} ms poll)",
                key.label(),
                TAP_TICK_MS
            ));
            Ok(())
        }
        None => {
            let _ = UnregisterHotKey(Some(hwnd), HOTKEY_ID);
            let _ = KillTimer(Some(hwnd), TAP_TIMER);
            Ok(())
        }
    }
}

/// What happened to an edit that arrived over HTTP.
pub enum Rebind {
    /// The window now holds the new chord, or holds none.
    Applied,
    /// No tray in this process: the store remembers the chord for the next tray start.
    NoTray,
    /// Windows refused it; the previous chord is still in force, and the message says why.
    Refused(String),
}

/// Asks the tray thread to swap the registration in place, and waits for its answer so a refused chord
/// never reaches the store. The page is the editor; edits are rare enough for a round trip.
pub fn rebind(binding: Option<crate::hotkey::Binding>) -> Rebind {
    let Some(hwnd) = with(|tray| tray.hwnd) else {
        return Rebind::NoTray;
    };
    let hwnd = hwnd_of(hwnd);
    let (reply, answer) = std::sync::mpsc::channel();
    *REBIND_JOB
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((binding, reply));
    if unsafe { PostMessageW(Some(hwnd), REBIND_MESSAGE, WPARAM(0), LPARAM(0)) }.is_err() {
        *REBIND_JOB
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        return Rebind::NoTray;
    }
    match answer.recv_timeout(Duration::from_secs(2)) {
        Ok(outcome) => outcome,
        Err(_) => {
            // Drop the job so a late message cannot apply an edit the caller was told failed.
            *REBIND_JOB
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            Rebind::Refused("托盘没有响应".to_string())
        }
    }
}

/// Runs on the tray thread: `WM_HOTKEY` and the registration live on the same thread, so a swap
/// cannot race the message that triggers it.
fn apply_rebind() {
    let Some((binding, reply)) = REBIND_JOB
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take()
    else {
        return;
    };
    let outcome = with(|tray| swap(tray, hwnd_of(tray.hwnd), binding)).unwrap_or(Rebind::NoTray);
    let _ = reply.send(outcome);
}

fn swap(tray: &mut Tray, hwnd: HWND, binding: Option<crate::hotkey::Binding>) -> Rebind {
    if tray.hotkey == binding {
        return Rebind::Applied;
    }
    let previous = tray.hotkey;
    match unsafe { install(hwnd, binding) } {
        Ok(()) => {
            tray.hotkey = binding;
            tray.tap.set_key(match binding {
                Some(crate::hotkey::Binding::Tap(key)) => Some(key),
                _ => None,
            });
            match binding {
                Some(binding) => {
                    log::line(&format!("tray: hotkey {} registered", binding.label()))
                }
                None => log::line("tray: hotkey off"),
            }
            Rebind::Applied
        }
        Err(err) => {
            // A refused edit must not silently cost the user the shortcut they already had.
            let restored = previous.filter(|previous| unsafe { install(hwnd, Some(*previous)) }.is_ok());
            tray.hotkey = restored;
            tray.tap.set_key(match restored {
                Some(crate::hotkey::Binding::Tap(key)) => Some(key),
                _ => None,
            });
            log::line(&format!(
                "tray: hotkey {} REFUSED — {err}",
                binding.map(|binding| binding.label()).unwrap_or_default()
            ));
            Rebind::Refused(err)
        }
    }
}

/// One poll of the tap key, on the thread that owns the window. A press only fires when no other
/// input was seen for the whole hold, so Right Ctrl keeps its day job as a modifier.
fn tap_tick() {
    let fired = with(|tray| {
        let Some(key) = tray.tap.key() else {
            return false;
        };
        let down = tap::key_down(key);
        let other = if down || tray.tap.holding() {
            tap::other_down(key)
        } else {
            false
        };
        tray.tap.poll(down, other)
    });
    if fired == Some(true) {
        toggle_mute(true);
    }
}

/// The hotkey, the menu row and the page's switch are one action. The engine is the live truth; the
/// store is written here so a hotkey toggle survives a restart exactly like the page's toggle does.
/// `cue` is true only for the global shortcut: the page and the menu show the new state themselves,
/// so the sound exists to confirm a toggle the user could not see.
fn toggle_mute(cue: bool) {
    let Some((engine, settings)) = with(|tray| (tray.engine.clone(), tray.settings.clone())) else {
        return;
    };
    let muted = engine.toggle_mic_muted();
    if cue {
        if muted {
            crate::cue::muted();
        } else {
            crate::cue::unmuted();
        }
    }
    settings
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .set_mic_muted(muted);
    with(|tray| {
        tray.muted = muted;
        if let Some(item) = tray.items.iter_mut().find(|item| item.id == ITEM_MUTE) {
            item.label = mute_label(muted);
        }
    });
    log::line(&format!(
        "tray: microphone {}",
        if muted { "muted" } else { "unmuted" }
    ));
}

/// The row states the action, not the state: a toggle labelled with what it is now leaves the user
/// guessing whether clicking changes it.
fn mute_label(muted: bool) -> Vec<u16> {
    wide(if muted {
        "取消麦克风静音"
    } else {
        "静音麦克风"
    })
}

fn measure_item(lparam: LPARAM) {
    let Some((hwnd, font)) = with(|tray| (tray.hwnd, tray.font)) else {
        return;
    };
    let owner = hwnd_of(hwnd);
    let dpi = unsafe { GetDpiForWindow(owner) }.max(96);
    let pad_x = (6 * dpi as i32 / 96).max(2);
    let pad_y = (3 * dpi as i32 / 96).max(1);
    unsafe {
        let hdc = GetDC(Some(owner));
        let previous = if font == 0 {
            HGDIOBJ::default()
        } else {
            SelectObject(hdc, HFONT(font as *mut c_void).into())
        };
        let mut metrics = TEXTMETRICW::default();
        let has_metrics = GetTextMetricsW(hdc, &mut metrics).as_bool();
        let mut widest = 0i32;
        if let Some(tray) = TRAY.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_mut() {
            for item in &tray.items {
                let mut size = SIZE::default();
                if GetTextExtentPoint32W(hdc, &item.label, &mut size).as_bool() {
                    widest = widest.max(size.cx);
                }
            }
        }
        if font != 0 {
            let _ = SelectObject(hdc, previous);
        }
        let _ = ReleaseDC(Some(owner), hdc);
        let height = if has_metrics {
            metrics.tmHeight
        } else {
            16 * dpi as i32 / 96
        };
        let item = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
        item.itemWidth = (widest + 2 * pad_x).max(1) as u32;
        item.itemHeight = (height + 2 * pad_y).max(1) as u32;
    }
}

fn draw_item(lparam: LPARAM) {
    let Some(state) = with(|tray| (tray.font, tray.dark, tray.items.clone())) else {
        return;
    };
    let (font, dark, items) = state;
    unsafe {
        let draw = &*(lparam.0 as *const DRAWITEMSTRUCT);
        if draw.CtlType != ODT_MENU {
            return;
        }
        let selected = (draw.itemState.0 & ODS_SELECTED.0) != 0;
        let label = items
            .iter()
            .find(|item| item.id == draw.itemID as usize)
            .map(|item| item.label.as_slice())
            .unwrap_or(&[]);
        paint_row(draw.hDC, &draw.rcItem, label, palette_for(dark), selected, font);
    }
}

/// One menu row: the theme's background or hover fill, then the label in the theme's ink. Split out from
/// the message handler so a test can drive it into an offscreen bitmap instead of trusting that the
/// palette constants were read the right way round.
unsafe fn paint_row(
    hdc: HDC,
    area: &RECT,
    label: &[u16],
    palette: &Palette,
    selected: bool,
    font: isize,
) {
    let brush = CreateSolidBrush(if selected {
        palette.hover
    } else {
        palette.background
    });
    let _ = FillRect(hdc, area, brush);
    let _ = DeleteObject(brush.into());
    let previous = if font == 0 {
        HGDIOBJ::default()
    } else {
        SelectObject(hdc, HFONT(font as *mut c_void).into())
    };
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = SetTextColor(hdc, palette.foreground);
    // 6 px of horizontal padding; the row height already carries the 3 px.
    let mut rect = *area;
    rect.left += 6;
    rect.right -= 6;
    let mut text = label.to_vec();
    let _ = DrawTextW(
        hdc,
        &mut text,
        &mut rect,
        DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
    if font != 0 {
        let _ = SelectObject(hdc, previous);
    }
}

fn tick() {
    // One snapshot per tick: the tooltip and the menu row change only on a transition, and both exits
    // are noticed here. The page can toggle the microphone between two ticks; the engine's answer is
    // what keeps the menu honest.
    let Some((hwnd, running, changed, stopped, expired)) = with(|tray| {
        let status = tray.engine.snapshot();
        let running = status.running;
        let changed = tray.running != running;
        if changed {
            tray.running = running;
        }
        if tray.muted != status.mic_muted {
            tray.muted = status.mic_muted;
            if let Some(item) = tray.items.iter_mut().find(|item| item.id == ITEM_MUTE) {
                item.label = mute_label(status.mic_muted);
            }
        }
        let expired = tray
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline);
        (
            tray.hwnd,
            running,
            changed,
            tray.stopped.load(Ordering::Relaxed),
            expired,
        )
    }) else {
        return;
    };
    if changed {
        update_tip(running);
    }
    if stopped || expired {
        unsafe {
            let _ = DestroyWindow(hwnd_of(hwnd));
        }
    }
}

fn update_tip(running: bool) {
    with(|tray| {
        tray.running = running;
        let url = tray.url.clone();
        let mut notify = notify_for(
            hwnd_of(tray.hwnd),
            HICON(tray.icon as *mut c_void),
            NIF_ICON | NIF_TIP,
        );
        write_tip(&mut notify, running, &url);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &notify);
        }
        log::line(&format!("tray: {}", one_line(&tooltip(running, &url))));
    });
}

/// The shell's record of our icon. It is rebuilt rather than stored, because the struct holds raw
/// handles and the tray's state has to be an integer-only `static` a window procedure can share.
fn notify_for(
    hwnd: HWND,
    icon: HICON,
    flags: NOTIFY_ICON_DATA_FLAGS,
) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        hIcon: icon,
        uFlags: flags,
        uCallbackMessage: ICON_MESSAGE,
        ..Default::default()
    }
}

fn refresh_theme() {
    let dark = is_dark_theme();
    let flipped = with(|tray| {
        if tray.dark == dark {
            return false;
        }
        tray.dark = dark;
        true
    })
    .unwrap_or(false);
    if !flipped {
        return;
    }
    let Some((menu, previous)) = with(|tray| (tray.menu, tray.background)) else {
        return;
    };
    unsafe {
        let brush = CreateSolidBrush(palette_for(dark).background);
        apply_menu_background(HMENU(menu as *mut c_void), brush, dark);
        // The menu has already been repainted with the new brush, so the one it held is now ours to free.
        let _ = DeleteObject(HBRUSH(previous as *mut c_void).into());
        with(|tray| tray.background = brush.0 as isize);
    }
    log::line(&format!(
        "tray: theme switched to {}",
        if dark { "dark" } else { "light" }
    ));
}

unsafe fn apply_menu_background(menu: HMENU, brush: HBRUSH, dark: bool) {
    let info = MENUINFO {
        cbSize: std::mem::size_of::<MENUINFO>() as u32,
        fMask: MIM_BACKGROUND | MIM_STYLE | MIM_APPLYTOSUBMENUS,
        // Owner-draw items take the whole row, and no check gutter is reserved for a menu that never
        // shows one.
        dwStyle: MNS_NOCHECK,
        cyMax: 0,
        hbrBack: brush,
        dwContextHelpID: 0,
        dwMenuData: 0,
    };
    if SetMenuInfo(menu, &info).is_err() {
        log::line(&format!(
            "tray: menu background could not be set for the {} theme",
            if dark { "dark" } else { "light" }
        ));
    }
}

/// The font the shell itself uses for menus, at the dpi of the display this window is on.
fn menu_font(hwnd: HWND) -> Option<HFONT> {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            metrics.cbSize,
            Some(&mut metrics as *mut NONCLIENTMETRICSW as *mut c_void),
            0,
            dpi,
        )
    };
    if ok.is_err() {
        return None;
    }
    let font = unsafe { CreateFontIndirectW(&metrics.lfMenuFont) };
    (!font.is_invalid()).then_some(font)
}

/// `HKCU\...\Themes\Personalize\AppsUseLightTheme == 0`, asked when the menu opens and on
/// `WM_SETTINGCHANGE`.
fn is_dark_theme() -> bool {
    let mut value = 0u32;
    let mut size = std::mem::size_of::<u32>() as u32;
    let error = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(wide(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize").as_ptr()),
            PCWSTR(wide("AppsUseLightTheme").as_ptr()),
            RRF_RT_REG_DWORD | RRF_SUBKEY_WOW6464KEY,
            None,
            Some(&mut value as *mut u32 as *mut c_void),
            Some(&mut size),
        )
    };
    error.0 == 0 && value == 0
}

fn palette_for(dark: bool) -> &'static Palette {
    if dark {
        &DARK
    } else {
        &LIGHT
    }
}

/// Word-for-word what `BuildTooltip` produced, line break included.
fn tooltip(running: bool, url: &str) -> String {
    if running {
        format!("MicDeck — 混音运行中\n{url}")
    } else {
        format!("MicDeck — 已停止\n{url}")
    }
}

fn one_line(text: &str) -> String {
    text.replace('\n', " / ")
}

/// `szTip` holds 128 characters with its terminator; a long url is cut, not dropped.
fn write_tip(notify: &mut NOTIFYICONDATAW, running: bool, url: &str) {
    let text = wide(&tooltip(running, url));
    let used = text.len().min(notify.szTip.len() - 1);
    notify.szTip[..used].copy_from_slice(&text[..used]);
    for slot in &mut notify.szTip[used..] {
        *slot = 0;
    }
}

fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text).encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
        DeleteDC,
    };

    /// Paints one row into a 32-bit buffer with the default font and hands the pixels back in the order
    /// they were drawn. `font: 0` is deliberate: it keeps the test independent of whatever menu font this
    /// machine happens to use, while still exercising the fill, the text colour and the padding.
    fn paint_probe(palette: &Palette, selected: bool, label: &str) -> Vec<(u8, u8, u8)> {
        let (width, height) = (200i32, 28i32);
        unsafe {
            let mut info = BITMAPINFO::default();
            info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            info.bmiHeader.biWidth = width;
            info.bmiHeader.biHeight = -height;
            info.bmiHeader.biPlanes = 1;
            info.bmiHeader.biBitCount = 32;
            info.bmiHeader.biCompression = BI_RGB.0;
            let mut bits: *mut c_void = std::ptr::null_mut();
            let Ok(dib) = CreateDIBSection(None, &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
                panic!("the probe bitmap could not be created");
            };
            let dc = CreateCompatibleDC(None);
            let previous = SelectObject(dc, dib.into());
            let area = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            };
            paint_row(dc, &area, &wide(label), palette, selected, 0);
            let pixels = std::slice::from_raw_parts(bits as *const u32, (width * height) as usize);
            let out = pixels
                .iter()
                .map(|p| {
                    (
                        // A 32-bit DIB pixel is 0xAARRGGBB; a COLORREF is 0x00BBGGRR. Reading the pixel
                        // as red-first is what makes the two comparable.
                        ((p >> 16) & 0xff) as u8,
                        ((p >> 8) & 0xff) as u8,
                        (p & 0xff) as u8,
                    )
                })
                .collect();
            let _ = SelectObject(dc, previous);
            let _ = DeleteDC(dc);
            let _ = DeleteObject(dib.into());
            out
        }
    }

    fn matches(pixel: (u8, u8, u8), colour: COLORREF) -> bool {
        pixel == (
            (colour.0 & 0xff) as u8,
            ((colour.0 >> 8) & 0xff) as u8,
            ((colour.0 >> 16) & 0xff) as u8,
        )
    }

    #[test]
    fn a_colourref_puts_blue_in_the_high_byte() {
        // `TrayApp.cs` wrote Color.FromArgb(36, 35, 32); COLORREF is (b << 16) | (g << 8) | r.
        assert_eq!(rgb(36, 35, 32).0, (32 << 16) | (35 << 8) | 36);
        assert_eq!(rgb(217, 119, 87).0, (87 << 16) | (119 << 8) | 217);
    }

    #[test]
    fn the_dark_row_is_filled_with_the_theme_and_inked_with_the_foreground() {
        let pixels = paint_probe(&DARK, false, "重启引擎");
        // The 6 px padding on the left and the far right edge are untouched fill.
        assert!(matches(pixels[0], DARK.background));
        assert!(matches(pixels[27 * 200 + 199], DARK.background));
        let painted = pixels.iter().filter(|p| !matches(**p, DARK.background)).count();
        assert!(painted > 30, "the label should cover more than {painted} pixels");
        let brightest = pixels
            .iter()
            .map(|p| p.0.max(p.1).max(p.2))
            .max()
            .unwrap_or(0);
        assert!(
            brightest > 200,
            "the ink is the light foreground, not a dark colour: {brightest}"
        );
    }

    #[test]
    fn a_selected_row_is_filled_with_the_hover_colour() {
        let pixels = paint_probe(&DARK, true, "退出");
        assert!(matches(pixels[0], DARK.hover));
        assert!(matches(pixels[27 * 200 + 199], DARK.hover));
        let painted = pixels.iter().filter(|p| !matches(**p, DARK.hover)).count();
        assert!(painted > 10, "the short label still drew {painted} pixels");
    }

    #[test]
    fn the_light_row_stays_dark_on_white() {
        let pixels = paint_probe(&LIGHT, false, "打开 MicDeck");
        assert!(matches(pixels[0], LIGHT.background));
        let darkest = pixels
            .iter()
            .map(|p| p.0.min(p.1).min(p.2))
            .min()
            .unwrap_or(255);
        assert!(
            darkest < 80,
            "the ink is the dark foreground, not white on white: {darkest}"
        );
    }

    #[test]
    fn the_tooltip_says_what_the_csharp_tray_said() {
        assert_eq!(
            tooltip(true, "http://127.0.0.1:8765/"),
            "MicDeck — 混音运行中\nhttp://127.0.0.1:8765/"
        );
        assert_eq!(tooltip(false, "http://x/"), "MicDeck — 已停止\nhttp://x/");
        assert_eq!(
            one_line(&tooltip(true, "http://x/")),
            "MicDeck — 混音运行中 / http://x/"
        );
    }

    /// The shell's tip buffer is 128 characters wide. A long url is cut, never overrun, and always
    /// terminated.
    #[test]
    fn a_long_tooltip_is_cut_at_the_buffer_rather_than_past_it() {
        let mut notify = NOTIFYICONDATAW::default();
        let url = "http://127.0.0.1:8765/".repeat(20);
        write_tip(&mut notify, true, &url);
        let text = String::from_utf16(&notify.szTip).unwrap();
        let text = text.trim_end_matches('\0');
        assert_eq!(text.chars().count(), 127);
        assert!(text.starts_with("MicDeck — 混音运行中\nhttp://127.0.0.1"));
        assert_eq!(notify.szTip[127], 0);
    }

    #[test]
    fn a_short_tooltip_keeps_its_line_break() {
        let mut notify = NOTIFYICONDATAW::default();
        write_tip(&mut notify, false, "http://127.0.0.1:8765/");
        let text = String::from_utf16(&notify.szTip)
            .unwrap()
            .trim_end_matches('\0')
            .to_string();
        assert_eq!(text, tooltip(false, "http://127.0.0.1:8765/"));
    }

    #[test]
    fn only_the_second_click_of_a_pair_opens() {
        let start = Instant::now();
        let at = |milliseconds: u64| start + Duration::from_millis(milliseconds);
        let window = Duration::from_millis(500);
        let mut last = None;
        assert!(!paired(&mut last, at(0), window), "one click is not a gesture");
        assert!(paired(&mut last, at(400), window), "400 ms apart is a gesture");
        assert!(last.is_none(), "the pair is spent");
        assert!(!paired(&mut last, at(450), window), "a third click starts over");
        assert!(
            !paired(&mut last, at(1100), window),
            "650 ms after the third click is too slow"
        );
        assert!(paired(&mut last, at(1500), window), "and the next click completes it");
    }
}

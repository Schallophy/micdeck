//! The tray entry: the icon, the menu under it, and the reason the process stays alive.
//!
//! The owner is a real hidden window, not a message-only one: [`crate::popup`] puts a foreground window
//! on screen when the menu opens, and a window that cannot be foregrounded leaves the menu unable to see
//! the click that should dismiss it. `show_menu` blocks the tray thread the way `TrackPopupMenuEx` did,
//! which is what keeps one click from starting a second menu or a second engine restart.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, Error as WindowsError};
use windows::Win32::Foundation::{FALSE, HWND, HINSTANCE, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{
    RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_SUBKEY_WOW6464KEY,
};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetDoubleClickTime, RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_NOREPEAT,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NIM_SETVERSION, NOTIFYICONDATAW, NOTIFY_ICON_DATA_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, DispatchMessageW, GetMessageW,
    GetCursorPos, HICON, IDC_ARROW, KillTimer, LoadCursorW, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, SetTimer, TranslateMessage, WM_APP,
    WM_CONTEXTMENU, WM_DESTROY, WM_HOTKEY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_RBUTTONUP, WM_SETTINGCHANGE, WM_TIMER, WNDCLASSEXW, WNDCLASS_STYLES, WS_EX_TOOLWINDOW,
    WS_POPUP,
};

use crate::engine::Engine;
use crate::log;
use crate::platform::brand;
use crate::popup;
use crate::settings::Settings;
use crate::tap;

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
const ITEM_SETTINGS: usize = 5;

/// `RegisterHotKey` ids are per window, not global; one is all this process needs.
const HOTKEY_ID: i32 = 1;
/// While a modifier tap is bound, this fine timer watches the key; see [`crate::tap`].
const TAP_TIMER: usize = 2;
const TAP_TICK_MS: u32 = 15;

const CLASS: &str = "MicDeckTray";

struct Tray {
    hwnd: isize,
    engine: Arc<Engine>,
    settings: Arc<Mutex<Settings>>,
    /// Shows the window. The shell owns it, so the tray only knows how to ask.
    on_open: Arc<dyn Fn() + Send + Sync>,
    /// Shows the settings window, same arrangement.
    on_settings: Arc<dyn Fn() + Send + Sync>,
    /// Set only by a bounded smoke run; the tick treats it as a shutdown request.
    deadline: Option<Instant>,
    icon: isize,
    items: Vec<popup::Row>,
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

/// Runs the tray until its menu says quit or `limit_seconds` elapses. `on_open` is what the "打开
/// MicDeck" row and the double-click do, `on_settings` what the "设置…" row does; the shell that owns
/// the windows supplies both.
pub fn run(
    engine: Arc<Engine>,
    settings: Arc<Mutex<Settings>>,
    limit_seconds: Option<f64>,
    on_open: impl Fn() + Send + Sync + 'static,
    on_settings: impl Fn() + Send + Sync + 'static,
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
    let items = vec![
        popup::Row {
            id: ITEM_OPEN,
            label: wide("打开 MicDeck"),
        },
        popup::Row {
            id: ITEM_SETTINGS,
            label: wide("设置…"),
        },
        popup::Row {
            id: ITEM_MUTE,
            label: mute_label(muted),
        },
        popup::Row {
            id: ITEM_RESTART,
            label: wide("重启引擎"),
        },
        popup::Row {
            id: ITEM_EXIT,
            label: wide("退出"),
        },
    ];

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

    // Not "MicDeck": the shell finds our main window by that title, and this one is never shown.
    let name = wide("MicDeckTray");
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

    let mut notify = notify_for(hwnd, icon, NIF_ICON | NIF_MESSAGE | NIF_TIP);
    write_tip(&mut notify, running);
    if unsafe { Shell_NotifyIconW(NIM_ADD, &notify) } == FALSE {
        unsafe {
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

    let deadline = limit_seconds.map(|seconds| Instant::now() + Duration::from_secs_f64(seconds));
    *TRAY.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Tray {
        hwnd: hwnd.0 as isize,
        engine,
        settings,
        on_open: Arc::new(on_open),
        on_settings: Arc::new(on_settings),
        deadline,
        icon: icon.0 as isize,
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
    // of the icon.
    unsafe {
        let _ = KillTimer(Some(hwnd), TIMER);
        let _ = Shell_NotifyIconW(NIM_DELETE, &notify);
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

/// The "打开 MicDeck" row and the double-click. The shell owns the window, so this hands off.
fn open_page() {
    with(|tray| (tray.on_open)());
}

/// The "设置…" row: same hand-off, to the other window.
fn open_settings_window() {
    with(|tray| (tray.on_settings)());
}

fn show_menu(owner: HWND) {
    // One right-click can reach us twice, and the click that dismissed a menu is reported again as a
    // fresh right-click on the icon, so a second sighting of either is the same gesture.
    if popup::is_open() || popup::reopened_too_soon() {
        return;
    }
    let mut point = POINT::default();
    unsafe {
        if GetCursorPos(&mut point).is_err() {
            return;
        }
    }
    // Everything the popup needs is copied out first: `popup::show` blocks here while it pumps this
    // thread's messages, and the state lock must not be held across it.
    let Some((rows, dark)) = with(|tray| (tray.items.clone(), tray.dark)) else {
        return;
    };
    let choice = popup::show(owner, point, rows, dark);
    log::line(&format!("menu: closed with {choice:?}"));
    match choice {
        Some(ITEM_OPEN) => open_page(),
        Some(ITEM_SETTINGS) => open_settings_window(),
        Some(ITEM_MUTE) => toggle_mute(false),
        Some(ITEM_RESTART) => restart_engine(),
        Some(ITEM_EXIT) => unsafe {
            let _ = DestroyWindow(owner);
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

fn tick() {
    // One snapshot per tick: the tooltip and the menu row change only on a transition, and both exits
    // are noticed here. The page can toggle the microphone between two ticks; the engine's answer is
    // what keeps the menu honest.
    let Some((hwnd, running, changed, expired)) = with(|tray| {
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
        (tray.hwnd, running, changed, expired)
    }) else {
        return;
    };
    if changed {
        update_tip(running);
    }
    if expired {
        unsafe {
            let _ = DestroyWindow(hwnd_of(hwnd));
        }
    }
}

fn update_tip(running: bool) {
    with(|tray| {
        tray.running = running;
        let mut notify = notify_for(
            hwnd_of(tray.hwnd),
            HICON(tray.icon as *mut c_void),
            NIF_ICON | NIF_TIP,
        );
        write_tip(&mut notify, running);
        unsafe {
            let _ = Shell_NotifyIconW(NIM_MODIFY, &notify);
        }
        log::line(&format!("tray: {}", tooltip(running)));
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

/// The theme is re-read when the shell announces it, and the next menu that opens is drawn with the
/// value stored here.
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
    if flipped {
        log::line(&format!(
            "tray: theme switched to {}",
            if dark { "dark" } else { "light" }
        ));
    }
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

/// The one line the shell shows over the icon. It used to carry the page's url on a second line; the
/// window has an address now, so the state alone is what is worth saying.
fn tooltip(running: bool) -> String {
    if running {
        "MicDeck — 混音运行中".to_string()
    } else {
        "MicDeck — 已停止".to_string()
    }
}

/// `szTip` holds 128 characters with its terminator; a long text is cut, not dropped.
fn write_tip(notify: &mut NOTIFYICONDATAW, running: bool) {
    let text = wide(&tooltip(running));
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

    #[test]
    fn the_tooltip_says_the_state_and_nothing_else() {
        assert_eq!(tooltip(true), "MicDeck — 混音运行中");
        assert_eq!(tooltip(false), "MicDeck — 已停止");
    }

    /// `szTip` holds 128 characters with its terminator. The text is a constant now, so what this
    /// guards is that it is written terminated and never reaches the last slot.
    #[test]
    fn the_tooltip_is_written_inside_the_shell_buffer() {
        for running in [true, false] {
            let mut notify = NOTIFYICONDATAW::default();
            write_tip(&mut notify, running);
            let text = String::from_utf16(&notify.szTip)
                .unwrap()
                .trim_end_matches('\0')
                .to_string();
            assert_eq!(text, tooltip(running));
            assert!(text.chars().count() < 127, "room to spare: {text}");
            assert_eq!(notify.szTip[text.encode_utf16().count()], 0);
        }
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

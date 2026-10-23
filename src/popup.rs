//! The tray's popup menu, painted by us.
//!
//! The system menu cannot be taken past its rows. Measured on Win11 24H2, its ring is `#A0A0A0` on all
//! 424 perimeter pixels of a 116×98 menu, square-cornered, and it survives
//! `DWMWA_BORDER_COLOR = DWMWA_COLOR_NONE`, `DWMWA_NCRENDERING_POLICY = DWMNCRP_DISABLED` and
//! `SetWindowTheme` alike — asked from our own thread at 30 ms and at 700 ms after the menu is on screen,
//! and from another process at 1 s, all returning S_OK with zero pixels changed. win32k draws that edge
//! itself, so a subclass of the menu window never sees a `WM_NCPAINT` either. Owning the edge means
//! owning the window.
//!
//! [`show`] keeps the one contract the tray was written against: it blocks on the thread that owns the
//! tray window and pumps that thread's messages while it does, so the meter tick, the modifier-tap poll
//! and a hot-key rebind from the web thread stay alive with the menu on screen.
//!
//! What the shell gives a menu that this file does not reproduce: the fade it animates in with, and the
//! accessibility tree of a real menu — the rows are pixels, so a screen reader sees a window titled
//! "MicDeck 菜单" rather than four menu items.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use windows::core::{Error as WindowsError, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUNDSMALL,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateCompatibleDC, CreateFontIndirectW, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, EndPaint, FillRect, GetDC, GetMonitorInfoW, GetTextExtentPoint32W, GetTextMetricsW,
    HBRUSH, HDC, HFONT, HGDIOBJ, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint, PAINTSTRUCT, ReleaseDC, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW,
    TRANSPARENT, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetCapture, ReleaseCapture, SetCapture, SetFocus, VK_DOWN, VK_ESCAPE,
    VK_LBUTTON, VK_RBUTTON, VK_RETURN, VK_SPACE, VK_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, HICON, IDC_ARROW,
    LoadCursorW, MSG, NONCLIENTMETRICSW, PostQuitMessage, RegisterClassExW, SetForegroundWindow,
    SetWindowPos, SPI_GETNONCLIENTMETRICS, TranslateMessage, CS_DROPSHADOW, HWND_TOPMOST,
    SWP_NOACTIVATE, SWP_SHOWWINDOW, WNDCLASSEXW, WS_EX_TOOLWINDOW, WS_POPUP, WM_DESTROY,
    WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT,
    WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN,
};

use crate::log;

/// The row padding the owner-drawn menu used, in the units the shell measures a menu in.
const PAD_X: i32 = 6;
const PAD_Y: i32 = 3;
/// Our own 1 px edge. The desktop window manager rounds the corners of the window, and that rounding
/// clips the edge at the corners the same way it clips a menu.
const BORDER: i32 = 1;
/// The outermost pixel of a top-level window is not shown: measured at 96 dpi, painting the edge at
/// window coordinate 0 left it invisible and the desktop visible in its place, with the fill starting
/// at 1. So the edge is drawn one pixel further in, and the rounded corners blend the edge colour into
/// the desktop rather than the fill.
const CLIPPED: i32 = 1;
/// Where the fill begins: past the pixel the manager clips, plus the edge we draw.
const INSET: i32 = CLIPPED + BORDER;
/// A second sighting of the right-click that dismissed a menu, within this much time, is the same
/// gesture rather than a request for a new menu.
const REOPEN_GRACE: Duration = Duration::from_millis(250);

const CLASS: &str = "MicDeckMenu";

/// The two palettes `TrayApp.SyncMenuTheme` painted, as blue-first colour references. `edge` replaces the
/// ring the system drew: one step off the background rather than a bright hairline.
pub struct Palette {
    background: COLORREF,
    foreground: COLORREF,
    hover: COLORREF,
    edge: COLORREF,
}

const fn rgb(r: u32, g: u32, b: u32) -> COLORREF {
    COLORREF((b << 16) | (g << 8) | r)
}

const DARK: Palette = Palette {
    background: rgb(36, 35, 32),
    foreground: rgb(243, 239, 230),
    hover: rgb(46, 44, 41),
    edge: rgb(70, 68, 64),
};

const LIGHT: Palette = Palette {
    background: rgb(255, 255, 255),
    foreground: rgb(31, 30, 29),
    hover: rgb(246, 243, 236),
    edge: rgb(217, 213, 206),
};

fn palette_for(dark: bool) -> &'static Palette {
    if dark {
        &DARK
    } else {
        &LIGHT
    }
}

/// One row of the menu: the tray's own command id, and the label we measure and paint.
#[derive(Clone)]
pub struct Row {
    pub id: usize,
    pub label: Vec<u16>,
}

/// The menu in flight. There is only ever one, on the thread that owns the tray window.
struct Open {
    hwnd: isize,
    rows: Vec<Row>,
    rects: Vec<RECT>,
    width: i32,
    height: i32,
    /// The lit row. Mouse and keyboard both move it.
    hot: Option<usize>,
    /// The row a press started on, so a release only chooses when it comes back to that one.
    pressed: Option<usize>,
    /// Which buttons were still held when the menu opened, as bit 1 for left and 2 for right. Releasing
    /// one of those outside the menu ends the gesture that opened it rather than dismissing a choice.
    held: u32,
    font: isize,
    dark: bool,
    result: Option<usize>,
    closing: bool,
}

static POPUP: LazyLock<Mutex<Option<Open>>> = LazyLock::new(|| Mutex::new(None));
static CLOSED_AT: LazyLock<Mutex<Option<Instant>>> = LazyLock::new(|| Mutex::new(None));
static REGISTERED: AtomicBool = AtomicBool::new(false);

fn with<T>(body: impl FnOnce(&mut Open) -> T) -> Option<T> {
    let mut guard = POPUP.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_mut().map(body)
}

/// Whether a menu is on screen right now.
pub fn is_open() -> bool {
    with(|_| ()).is_some()
}

/// A single right-click reaches us as both the shell's notification and a raw button message, and the
/// click that dismisses a menu is reported again as a fresh right-click on the icon. Without this, the
/// second sighting opens a menu a moment after the first one went away.
pub fn reopened_too_soon() -> bool {
    CLOSED_AT
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .is_some_and(|then| then.elapsed() < REOPEN_GRACE)
}

fn scale(dpi: i32, value: i32) -> i32 {
    (value * dpi / 96).max(1)
}

/// Rows fill the window below the top edge, one per item, each `height` pixels tall.
fn row_rects(count: usize, width: i32, height: i32) -> Vec<RECT> {
    (0..count)
        .map(|index| RECT {
            left: INSET,
            top: INSET + index as i32 * height,
            right: width - INSET,
            bottom: INSET + (index as i32 + 1) * height,
        })
        .collect()
}

/// Which row a point in window coordinates is over. `None` is the edge, the gap, or outside.
fn row_at(rects: &[RECT], point: POINT) -> Option<usize> {
    rects.iter().position(|rect| {
        point.x >= rect.left
            && point.x < rect.right
            && point.y >= rect.top
            && point.y < rect.bottom
    })
}

/// Up and down wrap, the way the shell's own menus do. With nothing lit yet, the first press lands on
/// the end nearest it.
fn step(hot: Option<usize>, delta: isize, count: usize) -> Option<usize> {
    if count == 0 {
        return None;
    }
    Some(match hot {
        Some(index) => (index as isize + delta).rem_euclid(count as isize) as usize,
        None if delta > 0 => 0,
        None => count - 1,
    })
}

/// The menu's rectangle: its bottom-left corner lands on the point the click came from, so it grows
/// upward off the taskbar the way the shell's tray menus do, then it is pulled back inside the work area
/// so it can never cover the taskbar or hang off the edge of the screen.
fn place(point: POINT, width: i32, height: i32, work: RECT) -> RECT {
    let mut left = point.x.max(work.left);
    let mut top = point.y - height;
    if left + width > work.right {
        left = work.right - width;
    }
    if top < work.top {
        top = work.top;
    }
    if top + height > work.bottom {
        top = work.bottom - height;
    }
    RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    }
}

/// Puts the menu on screen at `point` and does not return until it is gone. `None` is a dismissal;
/// `Some(id)` is the row that was chosen.
pub fn show(owner: HWND, point: POINT, rows: Vec<Row>, dark: bool) -> Option<usize> {
    let Ok(instance) = (unsafe { GetModuleHandleW(PCWSTR::null()) }) else {
        log::line("menu: no module handle");
        return None;
    };
    if !register(HINSTANCE(instance.0)) {
        return None;
    }

    let class = wide(CLASS);
    let name = wide("MicDeck 菜单");
    let Ok(hwnd) = (unsafe {
        CreateWindowExW(
            // A tool window: it belongs to the tray, not to the taskbar or Alt-Tab.
            WS_EX_TOOLWINDOW,
            PCWSTR(class.as_ptr()),
            PCWSTR(name.as_ptr()),
            WS_POPUP,
            0,
            0,
            1,
            1,
            // Owned by the tray window, so a shutdown that destroys the tray takes the menu with it.
            Some(owner),
            None,
            Some(HINSTANCE(instance.0)),
            None,
        )
    }) else {
        log::line(&format!(
            "menu: window could not be created ({})",
            WindowsError::from_thread()
        ));
        return None;
    };

    // The window is moved onto the click's monitor before anything is measured: under per-monitor v2
    // `GetDpiForWindow` answers for wherever the window currently sits, and the row height comes from that.
    unsafe {
        let _ = SetWindowPos(hwnd, None, point.x, point.y, 1, 1, SWP_NOACTIVATE);
        // A menu this small wants the shell's smaller corner radius, not the one it gives a document
        // window.
        let radius = DWMWCP_ROUNDSMALL.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &radius as *const i32 as *const c_void,
            std::mem::size_of::<i32>() as u32,
        );
    }
    let dpi = unsafe { GetDpiForWindow(hwnd) } as i32;
    // Built per opening: the dpi of whichever monitor the menu landed on is the one the labels have to
    // match, and a font kept from process start goes stale the moment the display scale changes.
    let font = menu_font(dpi.max(96));
    let Some((width, height, rects)) = measure(
        hwnd,
        &rows,
        dpi.max(96),
        font.map(|font| font.0 as isize).unwrap_or(0),
    ) else {
        unsafe {
            if let Some(font) = font {
                let _ = DeleteObject(font.into());
            }
            let _ = DestroyWindow(hwnd);
        }
        return None;
    };
    let rect = place(point, width, height, work_area(point));
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            // Above the taskbar for as long as it is open, which is what a menu is.
            Some(HWND_TOPMOST),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            SWP_SHOWWINDOW,
        );
    }

    *POPUP.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Open {
        hwnd: hwnd.0 as isize,
        width,
        height,
        rows,
        rects,
        hot: None,
        pressed: None,
        held: held_buttons(),
        font: font.map(|font| font.0 as isize).unwrap_or(0),
        dark,
        result: None,
        closing: false,
    });

    // Foreground and focus are what make Escape and the arrow keys arrive; the capture is what makes a
    // click anywhere else reach us instead of the window under the cursor.
    unsafe {
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(Some(hwnd));
        SetCapture(hwnd);
    }
    log::line(&format!(
        "menu: shown at ({},{}), {width}×{height}, dpi {dpi}",
        rect.left, rect.top
    ));

    let mut message = MSG::default();
    loop {
        let got = unsafe { GetMessageW(&mut message, None, 0, 0) }.0;
        if got <= 0 {
            // Zero is WM_QUIT. Reading it here removes it from the queue, and the loop that has to see it
            // is the one we are about to hand back to, so put it back. A negative return is the error the
            // docs tell us to treat as "stop".
            if got == 0 {
                unsafe {
                    PostQuitMessage(0);
                }
            }
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        // Every dismissal happens inside a message, so this check runs immediately after one.
        if with(|open| open.closing).unwrap_or(true) {
            break;
        }
    }

    let result = with(|open| open.result).flatten();
    unsafe {
        if GetCapture() == hwnd {
            let _ = ReleaseCapture();
        }
        let _ = DestroyWindow(hwnd);
    }
    *POPUP.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    *CLOSED_AT.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(Instant::now());
    if let Some(font) = font {
        unsafe {
            let _ = DeleteObject(font.into());
        }
    }
    result
}

/// Registers the class once per process. `CS_DROPSHADOW` is the shell's own menu shadow; the background
/// brush is `NULL` because every pixel is painted in `WM_PAINT`.
fn register(instance: HINSTANCE) -> bool {
    if REGISTERED.load(Ordering::Relaxed) {
        return true;
    }
    let name = wide(CLASS);
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_DROPSHADOW,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: HICON::default(),
        hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
        hbrBackground: HBRUSH::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(name.as_ptr()),
        hIconSm: HICON::default(),
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        log::line(&format!(
            "menu: class could not be registered ({})",
            WindowsError::from_thread()
        ));
        return false;
    }
    REGISTERED.store(true, Ordering::Relaxed);
    true
}

/// The font the shell itself uses for menus, at the dpi the menu was measured for.
fn menu_font(dpi: i32) -> Option<HFONT> {
    let mut metrics = NONCLIENTMETRICSW {
        cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            Some(&mut metrics as *mut NONCLIENTMETRICSW as *mut c_void),
            0,
            dpi as u32,
        )
    }
    .is_ok();
    if !ok {
        return None;
    }
    let font = unsafe { CreateFontIndirectW(&metrics.lfMenuFont) };
    (!font.is_invalid()).then_some(font)
}

/// The width the longest label needs and the height the rows fill, with the menu font at this dpi.
fn measure(hwnd: HWND, rows: &[Row], dpi: i32, font: isize) -> Option<(i32, i32, Vec<RECT>)> {
    if rows.is_empty() {
        return None;
    }
    unsafe {
        let screen = GetDC(Some(hwnd));
        let dc = CreateCompatibleDC(Some(screen));
        let previous = if font == 0 {
            HGDIOBJ::default()
        } else {
            SelectObject(dc, HFONT(font as *mut c_void).into())
        };
        let mut metrics = TEXTMETRICW::default();
        let text_height = if GetTextMetricsW(dc, &mut metrics).as_bool() {
            metrics.tmHeight
        } else {
            16 * dpi / 96
        };
        let mut widest = 0i32;
        for row in rows {
            let mut size = SIZE::default();
            if GetTextExtentPoint32W(dc, &row.label, &mut size).as_bool() {
                widest = widest.max(size.cx);
            }
        }
        if font != 0 {
            let _ = SelectObject(dc, previous);
        }
        let _ = DeleteDC(dc);
        let _ = ReleaseDC(Some(hwnd), screen);
        let row_height = text_height + 2 * scale(dpi, PAD_Y);
        let width = widest + 2 * scale(dpi, PAD_X) + 2 * INSET;
        let count = rows.len();
        Some((
            width,
            row_height * count as i32 + 2 * INSET,
            row_rects(count, width, row_height),
        ))
    }
}

fn work_area(point: POINT) -> RECT {
    unsafe {
        let monitor = MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST);
        let mut info = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(monitor, &mut info).as_bool() {
            info.rcWork
        } else {
            info.rcMonitor
        }
    }
}

/// Which mouse buttons were still down as the menu opened.
fn held_buttons() -> u32 {
    let down = |key: i32| (unsafe { GetAsyncKeyState(key) } as u16 & 0x8000) != 0;
    u32::from(down(VK_LBUTTON.0 as i32)) | (u32::from(down(VK_RBUTTON.0 as i32)) << 1)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A panic unwound through this frame would land inside user32's callback, so each message is handled
    // whole and a failure costs one ignored message.
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
        WM_PAINT => paint(hwnd),
        // `paint` covers the whole window, so there is nothing for a brush to erase first.
        WM_ERASEBKGND => 1,
        WM_MOUSEMOVE => hover(lparam),
        WM_LBUTTONDOWN => press(1, lparam, true),
        WM_RBUTTONDOWN => press(2, lparam, true),
        WM_LBUTTONUP => press(1, lparam, false),
        WM_RBUTTONUP => press(2, lparam, false),
        WM_KEYDOWN | WM_SYSKEYDOWN => key(wparam.0 as u32),
        // Alt-Tab, or another app taking the foreground: a menu that has lost the focus is a menu nobody
        // is using any more.
        WM_KILLFOCUS => close(),
        // The tray window going away ends the menu too, with nothing chosen.
        WM_DESTROY => close(),
        _ => 0,
    }));
    if outcome.is_err() {
        log::line(&format!("menu: message {msg} failed"));
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn point_of(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam.0 & 0xffff) as i16 as i32,
        y: ((lparam.0 >> 16) & 0xffff) as i16 as i32,
    }
}

fn paint(hwnd: HWND) -> isize {
    unsafe {
        let mut paint = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut paint);
        if let Some(state) = POPUP.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_mut() {
            draw(hdc, state);
        }
        let _ = EndPaint(hwnd, &paint);
    }
    0
}

/// The whole window at once: edge, fill, then every row. The menu is a few hundred pixels tall, and
/// repainting rows that did not change costs nothing next to a menu that shows a stale highlight.
fn draw(hdc: HDC, state: &Open) {
    unsafe {
        let palette = palette_for(state.dark);
        // The edge brush goes under the whole window, so the clipped pixel and the edge we mean are both
        // edge-coloured and the rounded corners blend that into the desktop rather than into the fill.
        let whole = RECT {
            left: 0,
            top: 0,
            right: state.width,
            bottom: state.height,
        };
        let edge = CreateSolidBrush(palette.edge);
        let _ = FillRect(hdc, &whole, edge);
        let _ = DeleteObject(edge.into());
        let fill = RECT {
            left: INSET,
            top: INSET,
            right: state.width - INSET,
            bottom: state.height - INSET,
        };
        let background = CreateSolidBrush(palette.background);
        let _ = FillRect(hdc, &fill, background);
        let _ = DeleteObject(background.into());
        for (index, row) in state.rows.iter().enumerate() {
            if let Some(rect) = state.rects.get(index) {
                paint_row(hdc, rect, &row.label, palette, state.hot == Some(index), state.font);
            }
        }
    }
}

/// One row: the theme's background or hover fill, then the label in the theme's ink. Split out from the
/// message handler so a test can drive it into an offscreen bitmap instead of trusting that the palette
/// constants were read the right way round.
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
    let mut text = label.to_vec();
    let mut rect = *area;
    rect.left += PAD_X;
    rect.right -= PAD_X;
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

/// Hovering a row lights it and nothing else; a point outside every row leaves the menu unlit, which is
/// how a menu behaves while the pointer wanders off it.
fn hover(lparam: LPARAM) -> isize {
    let point = point_of(lparam);
    let changed = with(|open| {
        let next = row_at(&open.rects, point);
        if open.closing || next == open.hot {
            return false;
        }
        open.hot = next;
        true
    })
    .unwrap_or(false);
    if changed {
        repaint();
    }
    0
}

/// Press and release, treated the way the shell treats them: a press outside dismisses, and the capture
/// means that click never reaches whatever was under it; a release chooses only the row its own press
/// started on.
fn press(button: u32, lparam: LPARAM, is_down: bool) -> isize {
    let point = point_of(lparam);
    let decision = with(|open| {
        if open.closing {
            return None;
        }
        let index = row_at(&open.rects, point);
        if is_down {
            open.pressed = index;
            open.hot = index;
            return Some(match index {
                None => Action::Close,
                Some(_) => Action::Repaint,
            });
        }
        let chosen = match (open.pressed.take(), index) {
            (Some(start), Some(end)) if start == end => Some(end),
            _ => None,
        };
        open.hot = index;
        if let Some(index) = chosen {
            return Some(match open.rows.get(index) {
                Some(row) => Action::Choose(row.id),
                None => Action::Close,
            });
        }
        if index.is_some() {
            // Released over a row its own press did not start on: nothing is chosen, the highlight stays.
            return Some(Action::Repaint);
        }
        // The release of the button that opened the menu, outside: the end of that gesture, not a
        // decision about this one.
        if open.held & button != 0 {
            open.held &= !button;
            return Some(Action::Repaint);
        }
        Some(Action::Close)
    })
    .flatten();
    match decision {
        Some(Action::Choose(id)) => {
            with(|open| open.result = Some(id));
            close()
        }
        Some(Action::Close) => close(),
        Some(Action::Repaint) => {
            repaint();
            0
        }
        None => 0,
    }
}

enum Action {
    Repaint,
    Choose(usize),
    Close,
}

fn key(code: u32) -> isize {
    if code == VK_ESCAPE.0 as u32 {
        return close();
    }
    if code == VK_RETURN.0 as u32 || code == VK_SPACE.0 as u32 {
        let chosen = with(|open| {
            open.hot
                .and_then(|index| open.rows.get(index))
                .map(|row| row.id)
        })
        .flatten();
        with(|open| open.result = chosen);
        return close();
    }
    let delta = match code {
        c if c == VK_UP.0 as u32 => -1,
        c if c == VK_DOWN.0 as u32 => 1,
        _ => return 0,
    };
    let changed = with(|open| {
        let next = step(open.hot, delta, open.rows.len());
        if open.closing || next == open.hot {
            return false;
        }
        open.hot = next;
        open.pressed = None;
        true
    })
    .unwrap_or(false);
    if changed {
        repaint();
    }
    0
}

/// Ends the menu. The choice, if there was one, is already in the state, and [`show`] notices `closing`
/// as soon as this message is done — so no wake-up message is needed.
fn close() -> isize {
    with(|open| {
        open.closing = true;
    });
    0
}

fn repaint() {
    let hwnd = with(|open| HWND(open.hwnd as *mut c_void)).unwrap_or(HWND::default());
    unsafe {
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn wide(text: &str) -> Vec<u16> {
    std::ffi::OsStr::new(text).encode_wide().chain(Some(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS,
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

    fn rows(count: usize) -> Vec<Row> {
        (0..count)
            .map(|index| Row {
                id: index + 1,
                label: vec![index as u16 + 65],
            })
            .collect()
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
        assert!(matches(pixels[0], DARK.background));
        assert!(matches(pixels[27 * 200 + 199], DARK.background));
        let painted = pixels.iter().filter(|p| !matches(**p, DARK.background)).count();
        assert!(painted > 200, "the label was not drawn ({painted} pixels)");
        let ink = pixels
            .iter()
            .filter(|p| !matches(**p, DARK.background))
            .all(|p| p.0 > 120 && p.1 > 120 && p.2 > 120);
        assert!(ink, "the label is not in the theme's ink");
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
    fn the_edge_is_a_step_off_the_background_rather_than_the_ring_we_replaced() {
        // The system ring measured #A0A0A0 on every perimeter pixel; ours sits next to its own fill.
        assert!(DARK.edge.0 > DARK.background.0, "dark edge should read lighter");
        assert!(
            LIGHT.edge.0 < LIGHT.background.0,
            "light edge should read darker"
        );
        assert!(DARK.edge.0 < rgb(160, 160, 160).0);
    }

    #[test]
    fn rows_stack_from_the_inner_edge_and_stop_at_the_bottom_one() {
        let rects = row_rects(3, 120, 24);
        assert_eq!(rects.len(), 3);
        assert_eq!(
            (rects[0].left, rects[0].top, rects[0].right, rects[0].bottom),
            (2, 2, 118, 26)
        );
        assert_eq!((rects[2].top, rects[2].bottom), (50, 74));
    }

    #[test]
    fn a_point_on_a_row_names_it_and_a_point_off_every_row_names_none() {
        let rects = row_rects(2, 120, 24);
        // The window is 52 tall: the clipped pixel and the edge make the first and last two rows of
        // pixels, and only the band between them belongs to an item.
        assert_eq!(row_at(&rects, POINT { x: 50, y: 1 }), None);
        assert_eq!(row_at(&rects, POINT { x: 50, y: 2 }), Some(0));
        assert_eq!(row_at(&rects, POINT { x: 2, y: 10 }), Some(0));
        assert_eq!(row_at(&rects, POINT { x: 50, y: 25 }), Some(0));
        assert_eq!(row_at(&rects, POINT { x: 50, y: 26 }), Some(1));
        assert_eq!(row_at(&rects, POINT { x: 50, y: 49 }), Some(1));
        assert_eq!(row_at(&rects, POINT { x: 50, y: 50 }), None);
        assert_eq!(row_at(&rects, POINT { x: 118, y: 10 }), None);
    }

    #[test]
    fn the_rows_and_their_ids_survive_the_layout_of_a_four_row_menu() {
        // The tray's own menu: four rows of 23 px, so the middle of the third band names the third item.
        let items = rows(4);
        let rects = row_rects(items.len(), 160, 23);
        let middle = rects[2].top + (rects[2].bottom - rects[2].top) / 2;
        let index = row_at(&rects, POINT { x: 80, y: middle }).expect("a point in the third row");
        assert_eq!(items[index].id, 3);
    }

    #[test]
    fn arrow_keys_wrap_and_start_at_the_end_the_key_points_at() {
        assert_eq!(step(None, 1, 4), Some(0));
        assert_eq!(step(None, -1, 4), Some(3));
        assert_eq!(step(Some(0), -1, 4), Some(3));
        assert_eq!(step(Some(3), 1, 4), Some(0));
        assert_eq!(step(Some(2), 1, 4), Some(3));
        assert_eq!(step(Some(0), 1, 0), None);
    }

    #[test]
    fn the_menu_grows_upward_from_the_point_and_is_kept_inside_the_work_area() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        // The usual case: a click on the icon at the bottom edge, with the menu rising from it.
        let raised = place(POINT { x: 900, y: 1040 }, 120, 100, work);
        assert_eq!((raised.left, raised.bottom), (900, 1040));
        assert_eq!(raised.top, 940);
        // A click near the right edge slides the menu left rather than off the screen.
        let edge = place(POINT { x: 1900, y: 1040 }, 120, 100, work);
        assert_eq!((edge.left, edge.right), (1800, 1920));
        // A menu taller than the free space is pushed down to the top of it, never above.
        let cramped = place(POINT { x: 900, y: 40 }, 120, 100, work);
        assert_eq!((cramped.top, cramped.bottom), (0, 100));
    }

    #[test]
    fn the_work_area_is_a_real_screen_and_not_a_zero_rectangle() {
        let area = work_area(POINT { x: 0, y: 0 });
        assert!(area.right - area.left >= 800, "{area:?}");
        assert!(area.bottom - area.top >= 480, "{area:?}");
    }
}

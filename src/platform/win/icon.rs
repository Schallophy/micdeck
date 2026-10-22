//! The picture beside a process name.
//!
//! One entry point, `for_pid`. Every failure — no icon in the image file, no image path, no window to
//! borrow from — ends at the same 1×1 transparent PNG, which keeps the row's placeholder square.
//!
//! Served as `.ico`, not PNG: an encoder would need deflate, while an `.ico` entry is a header, pixels
//! and a mask. Browsers decode both the same way.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::{Arc, LazyLock, Mutex};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, CreateDIBSection, DIB_RGB_COLORS,
    DeleteDC, DeleteObject, GetDC, HGDIOBJ, ReleaseDC, SelectObject,
};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, DrawIconEx, GCLP_HICON, GetClassLongPtrW, GetWindowThreadProcessId, DI_NORMAL,
    HICON, ICON_BIG, ICON_SMALL, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_GETICON,
};

/// The square the row chip is drawn at. The UI shows 18 CSS pixels, so 48 covers a 2× display without
/// upscaling and keeps the payload under 10 KB per application.
const SIZE: i32 = 48;

/// `None` means "tried, nothing there": failures are cached too, or a repaint would reopen the image file.
type Cache<T> = Mutex<HashMap<T, Option<Arc<Vec<u8>>>>>;

static BY_EXE: LazyLock<Cache<String>> = LazyLock::new(|| Mutex::new(HashMap::new()));
/// Keyed by `(handle as integer, pid)` because handles are not `Send` and both are recycled.
static BY_WINDOW: LazyLock<Cache<(usize, u32)>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static WINDOW_OF: LazyLock<Mutex<HashMap<u32, usize>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// What `/api/icon` sends: a media type and the bytes that go with it.
pub struct Icon {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

/// 1×1 transparent PNG for "no icon"; hand-assembled rather than adding a base64 decoder.
const EMPTY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5, 0x1c, 0x0c,
    0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64, 0x60, 0x00, 0x00,
    0x00, 0x06, 0x00, 0x02, 0x30, 0x81, 0xd0, 0x2f, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44,
    0xae, 0x42, 0x60, 0x82,
];

fn empty() -> Icon {
    Icon {
        mime: "image/png",
        bytes: EMPTY_PNG.to_vec(),
    }
}

fn cached<T: Clone + std::hash::Hash + Eq>(cache: &LazyLock<Cache<T>>, key: &T) -> Option<Option<Vec<u8>>> {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(key)
        .map(|hit| hit.as_ref().map(|bytes| (**bytes).clone()))
}

fn remember<T: Clone + std::hash::Hash + Eq>(
    cache: &LazyLock<Cache<T>>,
    key: T,
    bytes: Option<Vec<u8>>,
) -> Option<Vec<u8>> {
    let shared = bytes.map(Arc::new);
    let out = shared.as_ref().map(|bytes| (**bytes).clone());
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(key, shared);
    out
}

/// Records the window to borrow an icon from, made during the candidate enumeration — that is the only
/// moment every pickable window is walked. A protected process will not name its image file, so this is
/// the fallback that keeps its row from going blank.
pub fn note_window(pid: u32, hwnd: HWND) {
    if hwnd.is_invalid() {
        return;
    }
    WINDOW_OF
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(pid, hwnd.0 as usize);
}

pub fn for_pid(pid: u32) -> Icon {
    if pid == 0 {
        return empty();
    }
    let path = super::process::path(pid);
    if !path.is_empty() {
        if let Some(hit) = cached(&BY_EXE, &path) {
            if let Some(bytes) = hit {
                return Icon {
                    mime: "image/x-icon",
                    bytes,
                };
            }
        } else {
            let extracted = exe_icon(&path);
            let key = path.clone();
            if let Some(bytes) = remember(&BY_EXE, key, extracted) {
                return Icon {
                    mime: "image/x-icon",
                    bytes,
                };
            }
        }
    }
    let hwnd = WINDOW_OF
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .get(&pid)
        .copied();
    let Some(hwnd) = hwnd else { return empty() };
    let key = (hwnd, pid);
    if let Some(hit) = cached(&BY_WINDOW, &key) {
        return match hit {
            Some(bytes) => Icon {
                mime: "image/x-icon",
                bytes,
            },
            None => empty(),
        };
    }
    let extracted = window_icon(HWND(hwnd as *mut core::ffi::c_void), pid);
    if let Some(bytes) = remember(&BY_WINDOW, key, extracted) {
        return Icon {
            mime: "image/x-icon",
            bytes,
        };
    }
    empty()
}

/// The image file's own icon: `ExtractIconExW` index 0, straight from the path.
fn exe_icon(path: &str) -> Option<Vec<u8>> {
    let wide: Vec<u16> = OsStr::new(path).encode_wide().chain(Some(0)).collect();
    let mut icon = HICON::default();
    let found =
        unsafe { ExtractIconExW(PCWSTR(wide.as_ptr()), 0, Some(&mut icon), None, 1) };
    if found == 0 || icon.is_invalid() {
        return None;
    }
    let bytes = unsafe { render(icon) };
    unsafe {
        let _ = DestroyIcon(icon);
    }
    bytes
}

fn window_icon(hwnd: HWND, pid: u32) -> Option<Vec<u8>> {
    // The handle was recorded earlier, and window handles are recycled: if this one now belongs to a
    // different process, or to no process at all, it is not this pid's icon to give.
    let mut owner: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut owner)) };
    if owner != pid {
        return None;
    }
    // `WM_GETICON` is asked with a timeout: a target that is stuck in its own window procedure would
    // otherwise hold this connection thread open for as long as it takes to come back.
    let ask = |which: u32| {
        let mut result: usize = 0;
        unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_GETICON,
                WPARAM(which as usize),
                LPARAM(0),
                SMTO_ABORTIFHUNG,
                200,
                Some(&mut result),
            )
        };
        if result == 0 {
            None
        } else {
            Some(result)
        }
    };
    let handle = ask(ICON_BIG)
        .or_else(|| ask(ICON_SMALL))
        .or_else(|| {
            let class = unsafe { GetClassLongPtrW(hwnd, GCLP_HICON) };
            (class != 0).then_some(class)
        });
    let Some(handle) = handle else { return None };
    let icon = HICON(handle as *mut core::ffi::c_void);
    if icon.is_invalid() {
        return None;
    }
    // The handle belongs to the other process's window, so it is read but never destroyed.
    unsafe { render(icon) }
}

/// Draws an icon into a 32-bit buffer and serialises it. Every resource is released on the way out,
/// including when the draw itself fails.
unsafe fn render(icon: HICON) -> Option<Vec<u8>> {
    let screen = GetDC(None);
    let mut info = BITMAPINFO::default();
    info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    info.bmiHeader.biWidth = SIZE;
    // Negative height is what makes the bits top-down, which is the order the rest of this function
    // thinks in; `.ico` wants them bottom-up and the encoder flips them.
    info.bmiHeader.biHeight = -SIZE;
    info.bmiHeader.biPlanes = 1;
    info.bmiHeader.biBitCount = 32;
    info.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut core::ffi::c_void = ptr::null_mut();
    let Ok(dib) = CreateDIBSection(
        Some(screen),
        &info,
        DIB_RGB_COLORS,
        &mut bits,
        None,
        0,
    ) else {
        ReleaseDC(None, screen);
        return None;
    };
    let memory_dc = CreateCompatibleDC(Some(screen));
    ReleaseDC(None, screen);
    if memory_dc.is_invalid() || bits.is_null() {
        let _ = DeleteObject(dib.into());
        return None;
    }
    let previous: HGDIOBJ = SelectObject(memory_dc, dib.into());
    // The buffer CreateDIBSection hands back is zeroed, so anything the icon leaves untouched stays
    // transparent instead of picking up a black box behind the glyph.
    let drawn = DrawIconEx(
        memory_dc,
        0,
        0,
        icon,
        SIZE,
        SIZE,
        0,
        None,
        DI_NORMAL,
    );
    let pixels = std::slice::from_raw_parts_mut(bits as *mut u32, (SIZE * SIZE) as usize);
    let out = if drawn.is_ok() {
        restore_alpha(pixels);
        Some(encode_ico(SIZE, pixels))
    } else {
        None
    };
    let _ = SelectObject(memory_dc, previous);
    let _ = DeleteDC(memory_dc);
    let _ = DeleteObject(dib.into());
    out
}

/// Icons that predate alpha channels are drawn by `DrawIconEx` without destination alpha, so they come
/// back fully transparent and would render as an empty square. If nothing in the draw carries alpha,
/// treat every painted pixel as opaque — the same colour-key GDI+ applies when it converts an icon.
fn restore_alpha(pixels: &mut [u32]) {
    if pixels.iter().any(|&argb| (argb >> 24) != 0) {
        return;
    }
    for pixel in pixels {
        if *pixel & 0x00ff_ffff != 0 {
            *pixel |= 0xff00_0000;
        }
    }
}

/// A single-image `.ico`: the directory, one entry, a DIB header with the height doubled for the mask,
/// the pixels bottom-up and the 1-bit AND mask, where a set bit means transparent. Shared with the brand
/// mark, which needs the same container for its diagnostics dump.
pub(super) fn encode_ico(size: i32, pixels: &[u32]) -> Vec<u8> {
    let side = size as usize;
    let mask_row = (side as u32).div_ceil(32) as usize * 4;
    let mask_bytes = mask_row * side;
    let xor_bytes = side * side * 4;
    let image_bytes = 40 + xor_bytes + mask_bytes;

    let mut out: Vec<u8> = Vec::with_capacity(6 + 16 + image_bytes);
    push16(&mut out, 0); // reserved
    push16(&mut out, 1); // type: icon
    push16(&mut out, 1); // one image
    // 256 is written as 0 in both the width and height byte.
    out.push(if side >= 256 { 0 } else { side as u8 });
    out.push(if side >= 256 { 0 } else { side as u8 });
    out.push(0); // palette entries, 0 for a true-colour icon
    out.push(0); // reserved
    push16(&mut out, 1); // planes
    push16(&mut out, 32); // bits per pixel
    push32(&mut out, image_bytes as u32);
    push32(&mut out, 22); // directory (6) + entry (16)

    push32(&mut out, 40); // BITMAPINFOHEADER size
    push32(&mut out, side as u32);
    push32(&mut out, (side * 2) as u32); // XOR then AND
    push16(&mut out, 1);
    push16(&mut out, 32);
    push32(&mut out, 0); // BI_RGB
    push32(&mut out, (xor_bytes + mask_bytes) as u32);
    push32(&mut out, 0);
    push32(&mut out, 0);
    push32(&mut out, 0);
    push32(&mut out, 0);

    for row in (0..side).rev() {
        for column in 0..side {
            push32(&mut out, pixels[row * side + column]);
        }
    }
    for row in (0..side).rev() {
        let mut byte = 0u8;
        let mut emitted = 0usize;
        for column in 0..side {
            let argb = pixels[row * side + column];
            let transparent = (argb >> 24) == 0;
            byte = (byte << 1) | u8::from(transparent);
            if (column + 1) % 8 == 0 {
                out.push(byte);
                emitted += 1;
                byte = 0;
            }
        }
        if side % 8 != 0 {
            // Pad the final partial byte into the high bits, then the row to its 4-byte boundary.
            byte <<= 8 - (side % 8);
            out.push(byte);
            emitted += 1;
        }
        while emitted < mask_row {
            out.push(0);
            emitted += 1;
        }
    }
    out
}

fn push16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn push32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argb(a: u32, r: u32, g: u32, b: u32) -> u32 {
        (a << 24) | (r << 16) | (g << 8) | b
    }

    /// Reads the three numbers a decoder would trip over: the offsets the directory promises, the
    /// doubled height, and whether the rows came out bottom-up with the mask bit set on transparent
    /// pixels.
    #[test]
    fn the_directory_offsets_match_the_body_and_rows_flip_bottom_up() {
        // Top-down input: opaque white, transparent, opaque blue, transparent green.
        let pixels = [
            argb(255, 255, 255, 255),
            argb(0, 0, 0, 0),
            argb(255, 0, 0, 255),
            argb(0, 0, 255, 0),
        ];
        let ico = encode_ico(2, &pixels);
        let u16_at = |at: usize| u16::from_le_bytes([ico[at], ico[at + 1]]) as usize;
        let u32_at = |at: usize| u32::from_le_bytes(ico[at..at + 4].try_into().unwrap());

        assert_eq!((u16_at(0), u16_at(2), u16_at(4)), (0, 1, 1));
        assert_eq!((ico[6], ico[7]), (2, 2));
        assert_eq!((u16_at(10), u16_at(12)), (1, 32));
        let body = 40 + 4 * 4 + 2 * 4; // header, four pixels, two padded mask rows
        assert_eq!((u32_at(14) as usize, u32_at(18) as usize, ico.len()), (body, 22, 22 + body));

        assert_eq!(u32_at(22), 40);
        assert_eq!(u32_at(26), 2);
        assert_eq!(u32_at(30), 4, "the height counts the XOR and AND planes");
        assert_eq!(u32_at(42) as usize, 4 * 4 + 2 * 4);

        // The pixel body starts after the 40-byte DIB header, and the first stored row is the image's
        // bottom row.
        assert_eq!(u32_at(62), pixels[2]);
        assert_eq!(u32_at(66), pixels[3]);
        assert_eq!(u32_at(70), pixels[0]);
        assert_eq!(u32_at(74), pixels[1]);
        // Mask rows are bottom-up too, MSB first: column 1 of each row is the transparent one.
        assert_eq!((ico[78], ico[82]), (0x40, 0x40));
        assert_eq!((ico[79], ico[80], ico[81]), (0, 0, 0));
    }

    /// A 256-pixel side is written as 0 in the directory byte but stays 256 in the DIB header.
    #[test]
    fn a_large_icon_writes_zero_in_the_directory_bytes() {
        let pixels = vec![argb(255, 0, 0, 0); 256 * 256];
        let ico = encode_ico(256, &pixels);
        assert_eq!((ico[6], ico[7]), (0, 0));
        assert_eq!(u32::from_le_bytes(ico[26..30].try_into().unwrap()), 256);
    }

    #[test]
    fn a_draw_without_alpha_becomes_opaque_where_it_painted() {
        let mut pixels = [0u32; 4];
        pixels[0] = 0x0000_00ff; // blue with no alpha at all
        pixels[2] = 0x0000_0000; // untouched background
        restore_alpha(&mut pixels);
        assert_eq!(pixels[0], 0xff00_00ff);
        assert_eq!(pixels[2], 0);
    }

    #[test]
    fn a_draw_that_carries_alpha_is_left_alone() {
        let mut pixels = [argb(128, 10, 20, 30), 0, 0, 0];
        restore_alpha(&mut pixels);
        assert_eq!(pixels[0], argb(128, 10, 20, 30));
        assert_eq!(pixels[1], 0);
    }
}

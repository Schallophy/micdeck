//! The MicDeck mark, rasterised at the size the shell asks for.
//!
//! GDI does not antialias geometry, so each pixel's coverage comes from its distance to the nearest shape
//! edge: `0.5 - distance`, one device pixel of ramp. The shapes live in a 32-unit coordinate space.

use std::ffi::c_void;

use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateDIBSection, DIB_RGB_COLORS, DeleteObject,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, GetSystemMetrics, HICON, ICONINFO, SM_CXICON, SM_CXSMICON,
};

/// The brand orange, `Color.FromArgb(217, 119, 87)`.
const BACKGROUND: (u8, u8, u8) = (217, 119, 87);

/// A unit-space circle: `g.FillEllipse(brush, 1, 1, 30, 30)`.
const DISK: (f32, f32, f32) = (16.0, 16.0, 15.0);

/// The glyph is authored in a 32 unit square.
const SPACE: f32 = 32.0;

/// Straight-alpha BGRA, top-down, `size * size` pixels.
pub fn render(size: i32) -> Vec<u32> {
    let scale = size as f32 / SPACE;
    let mut out = Vec::with_capacity((size * size) as usize);
    for row in 0..size {
        for column in 0..size {
            // Pixel centres, so the ramp lands half a pixel inside the shape boundary rather than on it.
            let x = (column as f32 + 0.5) / scale;
            let y = (row as f32 + 0.5) / scale;
            out.push(argb(x, y, scale));
        }
    }
    out
}

/// The icon the tray shows, at the size the shell draws notification icons.
pub fn hicon() -> Option<HICON> {
    let size = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let pixels = render(size);
    unsafe { build_icon(size, &pixels) }
}

/// The mark as `.ico` bytes at the size the shell uses for a normal icon. Diagnostics write this to look
/// at the glyph, and a resource script could embed the same bytes as the executable's icon.
pub fn ico() -> Vec<u8> {
    let size = unsafe { GetSystemMetrics(SM_CXICON) }.max(32);
    super::icon::encode_ico(size, &render(size))
}

/// Replaces the icon on the tray. Kept separate from `hicon` because the buffers have to outlive the
/// call: `CreateIconIndirect` copies them, so both bitmaps are released before it returns.
unsafe fn build_icon(size: i32, pixels: &[u32]) -> Option<HICON> {
    let mut colour_info = BITMAPINFO::default();
    colour_info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    colour_info.bmiHeader.biWidth = size;
    colour_info.bmiHeader.biHeight = -size;
    colour_info.bmiHeader.biPlanes = 1;
    colour_info.bmiHeader.biBitCount = 32;
    colour_info.bmiHeader.biCompression = BI_RGB.0;
    let mut bits: *mut c_void = std::ptr::null_mut();
    let Ok(colour) = CreateDIBSection(None, &colour_info, DIB_RGB_COLORS, &mut bits, None, 0) else {
        return None;
    };
    if bits.is_null() {
        return None;
    }
    std::ptr::copy_nonoverlapping(
        pixels.as_ptr(),
        bits as *mut u32,
        (size * size) as usize,
    );

    // The AND mask stays all zero: the alpha channel is what carries the shape.
    let mut mask_info = BITMAPINFO::default();
    mask_info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
    mask_info.bmiHeader.biWidth = size;
    mask_info.bmiHeader.biHeight = -size;
    mask_info.bmiHeader.biPlanes = 1;
    mask_info.bmiHeader.biBitCount = 1;
    mask_info.bmiHeader.biCompression = BI_RGB.0;
    let mut mask_bits: *mut c_void = std::ptr::null_mut();
    let mask = match CreateDIBSection(None, &mask_info, DIB_RGB_COLORS, &mut mask_bits, None, 0) {
        Ok(bitmap) if !mask_bits.is_null() => bitmap,
        _ => {
            let _ = DeleteObject(colour.into());
            return None;
        }
    };

    let icon = CreateIconIndirect(&ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: colour,
    });

    let _ = DeleteObject(colour.into());
    let _ = DeleteObject(mask.into());
    icon.ok()
}

/// One pixel: blue, green, red, alpha packed the way a 32-bit DIB stores them. The brand colour is mixed
/// toward white by the mark's coverage, and the disk's coverage is what makes the alpha channel.
fn argb(x: f32, y: f32, scale: f32) -> u32 {
    let disk = cover(circle(x, y) * scale);
    if disk <= 0.0 {
        return 0;
    }
    let mark = cover(union(x, y) * scale).min(disk);
    let channel = |brand: f32| -> u32 {
        let value = brand + (255.0 - brand) * mark;
        value.round().clamp(0.0, 255.0) as u32
    };
    let (red, green, blue) = (BACKGROUND.0 as f32, BACKGROUND.1 as f32, BACKGROUND.2 as f32);
    ((disk * 255.0).round() as u32) << 24 | channel(red) << 16 | channel(green) << 8 | channel(blue)
}

fn circle(x: f32, y: f32) -> f32 {
    let (cx, cy, radius) = DISK;
    ((x - cx).powi(2) + (y - cy).powi(2)).sqrt() - radius
}

/// The white mark: the head is three overlapping shapes, the cradle is a stroked half-ellipse, and the
/// stand is two stroked segments. Overlaps take the larger coverage, never a sum.
fn union(x: f32, y: f32) -> f32 {
    let head = ellipse(x, y, 16.0, 6.5, 4.0, 2.5)
        .min(rect(x, y, 12.0, 6.0, 8.0, 8.0))
        .min(ellipse(x, y, 16.0, 14.5, 4.0, 2.5));
    let cradle = {
        let band = ellipse(x, y, 16.0, 18.5, 6.0, 5.5).abs() - 1.0;
        // `DrawArc(10, 13, 12, 11, 0, 180)` sweeps the lower half, so the upper half is cut away.
        band.max(18.5 - y)
    };
    let stem = segment(x, y, 16.0, 24.0, 16.0, 27.0);
    let base = segment(x, y, 10.0, 27.0, 22.0, 27.0);
    head.min(cradle).min(stem).min(base)
}

/// Distance to an axis-aligned ellipse, scaled by the smaller semi-axis. Exact on the axes and off by at
/// most a few percent elsewhere, which is under half a device pixel at tray sizes.
fn ellipse(x: f32, y: f32, cx: f32, cy: f32, a: f32, b: f32) -> f32 {
    let dx = (x - cx) / a;
    let dy = (y - cy) / b;
    (dx.hypot(dy) - 1.0) * a.min(b)
}

fn rect(x: f32, y: f32, left: f32, top: f32, width: f32, height: f32) -> f32 {
    let half = (width / 2.0, height / 2.0);
    let dx = (x - (left + half.0)).abs() - half.0;
    let dy = (y - (top + half.1)).abs() - half.1;
    dx.max(0.0).hypot(dy.max(0.0)) + dx.max(dy).min(0.0)
}

/// A 2-unit pen along a segment, which is a capsule of half-width 1 including its ends.
fn segment(x: f32, y: f32, from_x: f32, from_y: f32, to_x: f32, to_y: f32) -> f32 {
    let vx = to_x - from_x;
    let vy = to_y - from_y;
    let length = vx.hypot(vy);
    let t = if length == 0.0 {
        0.0
    } else {
        (((x - from_x) * vx + (y - from_y) * vy) / (length * length)).clamp(0.0, 1.0)
    };
    ((x - from_x - vx * t).powi(2) + (y - from_y - vy * t).powi(2)).sqrt() - 1.0
}

/// One device pixel of ramp centred on the boundary: fully solid half a pixel inside, fully clear half a
/// pixel outside.
fn cover(distance: f32) -> f32 {
    (0.5 - distance).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pixel(size: i32, x: i32, y: i32) -> (u8, u8, u8, u8) {
        let packed = render(size)[(y * size + x) as usize];
        (
            (packed & 0xff) as u8,
            ((packed >> 8) & 0xff) as u8,
            ((packed >> 16) & 0xff) as u8,
            (packed >> 24) as u8,
        )
    }

    fn is_brand(pixel: (u8, u8, u8, u8)) -> bool {
        let (blue, green, red, alpha) = pixel;
        alpha > 200 && red > 180 && green < 160 && blue < 130
    }

    fn is_white(pixel: (u8, u8, u8, u8)) -> bool {
        let (blue, green, red, alpha) = pixel;
        alpha > 200 && blue > 200 && green > 200 && red > 200
    }

    #[test]
    fn the_head_and_the_cradle_are_white_and_the_disk_is_brand_orange() {
        // The mic head (12..20 x 6..14), the lower lobe, the cradle's bottom and the stand.
        for (x, y) in [(16, 10), (16, 16), (16, 23), (16, 26), (12, 27)] {
            assert!(is_white(pixel(32, x, y)), "expected white at {x},{y}: {:?}", pixel(32, x, y));
        }
        // Inside the disk but off the mark: the top of the circle and the gap under the head.
        for (x, y) in [(16, 2), (4, 16), (16, 21), (27, 20)] {
            assert!(is_brand(pixel(32, x, y)), "expected orange at {x},{y}: {:?}", pixel(32, x, y));
        }
    }

    #[test]
    fn the_corners_are_transparent_and_the_edge_is_not_opaque() {
        assert_eq!(pixel(32, 0, 0).3, 0);
        assert_eq!(pixel(32, 31, 31).3, 0);
        // The disk boundary sits at radius 15 of 16, so the outermost row is not solid anywhere.
        let top_row: Vec<u8> = (0..32).map(|x| pixel(32, x, 0).3).collect();
        assert!(top_row.iter().all(|&a| a == 0));
    }

    #[test]
    fn the_disk_covers_the_area_of_its_radius() {
        let solid = render(32).iter().filter(|&&p| p >> 24 > 128).count();
        // pi * 15^2 = 707; the ramp accounts for a few pixels either way.
        assert!((660..=760).contains(&solid), "solid pixels = {solid}");
    }

    /// A 16-pixel glyph must not lose the mark: the same shape test at the smallest tray size.
    #[test]
    fn the_mark_survives_at_the_smallest_tray_size() {
        assert!(is_white(pixel(16, 8, 5)));
        assert!(is_white(pixel(16, 8, 13)));
        assert!(is_brand(pixel(16, 2, 8)));
        let glyph = render(16);
        let ramp = glyph.iter().filter(|&&p| {
            let alpha = p >> 24;
            alpha > 0 && alpha < 255
        });
        assert!(ramp.count() > 20, "the boundary should be antialiased, not stair-stepped");
    }
}

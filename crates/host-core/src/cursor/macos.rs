//! macOS cursor-shape capture through AppKit's `NSCursor`.
//!
//! `+[NSCursor currentSystemCursor]` gives the pointer the local user sees; its
//! `image` is rendered to a CGImage and read into straight-alpha RGBA. Every
//! class and selector is looked up the same defensive way as the virtual
//! display code, and the whole read is wrapped so a nil or an unexpected AppKit
//! state reports `Unavailable` rather than disturbing the session.

use super::{CursorImage, CursorSample};
use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::{msg_send, Encode, Encoding};
use std::ffi::c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct CGPoint {
    x: f64,
    y: f64,
}
// SAFETY: matches CoreGraphics' CGPoint layout and type encoding.
unsafe impl Encode for CGPoint {
    const ENCODING: Encoding = Encoding::Struct("CGPoint", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGSize {
    width: f64,
    height: f64,
}
// SAFETY: matches CoreGraphics' CGSize layout and type encoding.
unsafe impl Encode for CGSize {
    const ENCODING: Encoding = Encoding::Struct("CGSize", &[f64::ENCODING, f64::ENCODING]);
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}
// SAFETY: matches CoreGraphics' CGRect layout and type encoding.
unsafe impl Encode for CGRect {
    const ENCODING: Encoding = Encoding::Struct("CGRect", &[CGPoint::ENCODING, CGSize::ENCODING]);
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGCursorIsVisible() -> i32;
    fn CGImageGetWidth(image: *mut c_void) -> usize;
    fn CGImageGetHeight(image: *mut c_void) -> usize;
    fn CGColorSpaceCreateDeviceRGB() -> *mut c_void;
    fn CGColorSpaceRelease(space: *mut c_void);
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *mut c_void,
        bitmap_info: u32,
    ) -> *mut c_void;
    fn CGContextDrawImage(ctx: *mut c_void, rect: CGRect, image: *mut c_void);
    fn CGContextRelease(ctx: *mut c_void);
}

// kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big: bytes land as R,G,B,A.
const RGBA_BITMAP_INFO: u32 = 1 | (4 << 12);

pub fn sample() -> CursorSample {
    // AppKit is touched off the main thread here; keep the read read-only and
    // isolate any surprise so it can never take the session down.
    std::panic::catch_unwind(read_cursor).unwrap_or(CursorSample::Unavailable)
}

fn read_cursor() -> CursorSample {
    // SAFETY: every pointer is checked for null before use, the selectors are
    // the documented NSCursor/NSImage API, and the CoreGraphics buffer we hand
    // out lives for the whole draw. Objects come back autoreleased (+0), so the
    // pool below drains them; we only release what we create (space, context).
    autoreleasepool(|_| unsafe {
        if CGCursorIsVisible() == 0 {
            return CursorSample::Hidden;
        }
        let Some(class) = AnyClass::get(c"NSCursor") else { return CursorSample::Unavailable };
        let cursor: *mut AnyObject = msg_send![class, currentSystemCursor];
        if cursor.is_null() {
            return CursorSample::Unavailable;
        }
        let image: *mut AnyObject = msg_send![cursor, image];
        if image.is_null() {
            return CursorSample::Unavailable;
        }
        let hot_spot: CGPoint = msg_send![cursor, hotSpot];
        let point_size: CGSize = msg_send![image, size];

        let cg_image: *mut c_void = msg_send![
            image,
            CGImageForProposedRect: std::ptr::null_mut::<c_void>(),
            context: std::ptr::null_mut::<AnyObject>(),
            hints: std::ptr::null_mut::<AnyObject>()
        ];
        if cg_image.is_null() {
            return CursorSample::Unavailable;
        }
        let width = CGImageGetWidth(cg_image);
        let height = CGImageGetHeight(cg_image);
        let (Ok(w16), Ok(h16)) = (u16::try_from(width), u16::try_from(height)) else {
            return CursorSample::Unavailable;
        };
        if width == 0 || height == 0 {
            return CursorSample::Unavailable;
        }

        let space = CGColorSpaceCreateDeviceRGB();
        if space.is_null() {
            return CursorSample::Unavailable;
        }
        // Premultiplied RGBA, one row per `width * 4` bytes, no padding.
        let mut premul = vec![0u8; width * height * 4];
        let ctx = CGBitmapContextCreate(premul.as_mut_ptr().cast(), width, height, 8, width * 4, space, RGBA_BITMAP_INFO);
        CGColorSpaceRelease(space);
        if ctx.is_null() {
            return CursorSample::Unavailable;
        }
        let rect = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: width as f64, height: height as f64 } };
        CGContextDrawImage(ctx, rect, cg_image);
        CGContextRelease(ctx);

        // A bitmap context's buffer starts at the bottom row, so flip to put the
        // top row first, and un-premultiply to the straight alpha we promised.
        let mut rgba = vec![0u8; width * height * 4];
        for y in 0..height {
            let src_row = (height - 1 - y) * width * 4;
            let dst_row = y * width * 4;
            for x in 0..width {
                let s = src_row + x * 4;
                let d = dst_row + x * 4;
                let a = premul[s + 3];
                let straighten = |c: u8| if a == 0 { 0 } else { ((u16::from(c) * 255 + u16::from(a) / 2) / u16::from(a)).min(255) as u8 };
                rgba[d] = straighten(premul[s]);
                rgba[d + 1] = straighten(premul[s + 1]);
                rgba[d + 2] = straighten(premul[s + 2]);
                rgba[d + 3] = a;
            }
        }

        // The image is in points; scale the hotspot to pixels and clamp it in.
        let scale = if point_size.width > 0.0 { width as f64 / point_size.width } else { 1.0 };
        let hot_x = (hot_spot.x * scale).round().clamp(0.0, f64::from(w16 - 1)) as u16;
        let hot_y = (hot_spot.y * scale).round().clamp(0.0, f64::from(h16 - 1)) as u16;

        CursorSample::Visible(CursorImage { width: w16, height: h16, hot_x, hot_y, rgba })
    })
}

//! Windows cursor-shape capture. `GetCursorInfo` names the current cursor,
//! `GetIconInfo` hands back its bitmaps (which we own and must delete), and
//! `GetDIBits` reads them as top-down 32bpp so both colour and monochrome
//! cursors end up as straight-alpha RGBA.

use super::{CursorImage, CursorSample};
use std::ffi::c_void;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetDIBits, GetObjectW, ReleaseDC, BITMAP, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorInfo, GetIconInfo, CURSORINFO, CURSOR_SHOWING, HICON, ICONINFO};

pub fn sample() -> CursorSample {
    // SAFETY: plain Win32 calls with correctly sized, initialised structs.
    // Every bitmap and the DC acquired below is released on all paths.
    unsafe {
        let mut ci = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        if GetCursorInfo(&mut ci).is_err() {
            return CursorSample::Unavailable;
        }
        if ci.flags.0 & CURSOR_SHOWING.0 == 0 {
            return CursorSample::Hidden;
        }
        let mut info = ICONINFO::default();
        // HCURSOR and HICON are the same handle kind.
        if GetIconInfo(HICON(ci.hCursor.0), &mut info).is_err() {
            return CursorSample::Unavailable;
        }
        let dc = GetDC(None);
        let image = if dc.is_invalid() { None } else { build(dc, &info) };
        if !dc.is_invalid() {
            ReleaseDC(None::<HWND>, dc);
        }
        free(info.hbmColor);
        free(info.hbmMask);
        match image {
            Some(image) => CursorSample::Visible(image),
            None => CursorSample::Unavailable,
        }
    }
}

unsafe fn free(bitmap: HBITMAP) {
    if !bitmap.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
    }
}

/// Width and height of a bitmap in pixels.
unsafe fn dimensions(bitmap: HBITMAP) -> Option<(i32, i32)> {
    let mut bm = BITMAP::default();
    let got = GetObjectW(HGDIOBJ(bitmap.0), size_of::<BITMAP>() as i32, Some(&mut bm as *mut BITMAP as *mut c_void));
    (got != 0 && bm.bmWidth > 0 && bm.bmHeight > 0).then_some((bm.bmWidth, bm.bmHeight))
}

/// Reads `bitmap` as top-down 32bpp BGRA, `width * height * 4` bytes. A 1bpp
/// mask comes back as 0x000000 / 0xFFFFFF per pixel.
unsafe fn read_bgra(dc: HDC, bitmap: HBITMAP, width: i32, height: i32) -> Option<Vec<u8>> {
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            // Negative height asks for a top-down DIB.
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; width as usize * height as usize * 4];
    let lines = GetDIBits(dc, bitmap, 0, height as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bmi, DIB_RGB_COLORS);
    (lines == height).then_some(buf)
}

unsafe fn build(dc: HDC, info: &ICONINFO) -> Option<CursorImage> {
    let (width, height, rgba) = if !info.hbmColor.is_invalid() {
        let (w, h) = dimensions(info.hbmColor)?;
        let mut px = read_bgra(dc, info.hbmColor, w, h)?;
        let has_alpha = px.as_chunks::<4>().0.iter().any(|p| p[3] != 0);
        if !has_alpha {
            // Pre-alpha cursors rely on the AND mask: 0 is opaque, 1 is
            // transparent. The mask is the same size as the colour bitmap.
            let mask = if info.hbmMask.is_invalid() { None } else { read_bgra(dc, info.hbmMask, w, h) };
            for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                p[3] = match &mask {
                    Some(m) if m[i * 4] > 127 => 0,
                    _ => 255,
                };
            }
        }
        for p in px.as_chunks_mut::<4>().0 {
            p.swap(0, 2);
        }
        (w, h, px)
    } else {
        // Monochrome: one bitmap, AND mask on top and XOR mask below.
        if info.hbmMask.is_invalid() {
            return None;
        }
        let (w, full) = dimensions(info.hbmMask)?;
        let h = full / 2;
        if h == 0 {
            return None;
        }
        let bits = read_bgra(dc, info.hbmMask, w, full)?;
        let half = w as usize * h as usize;
        let mut px = vec![0u8; half * 4];
        for i in 0..half {
            let and = bits[i * 4] > 127;
            let xor = bits[(half + i) * 4] > 127;
            let out = &mut px[i * 4..i * 4 + 4];
            match (and, xor) {
                (true, false) => {}
                // Inversion (AND=1, XOR=1) can't be drawn, so it becomes black.
                (false, true) => out.copy_from_slice(&[255, 255, 255, 255]),
                _ => out.copy_from_slice(&[0, 0, 0, 255]),
            }
        }
        (w, h, px)
    };
    let width = u16::try_from(width).ok()?;
    let height = u16::try_from(height).ok()?;
    Some(CursorImage {
        width,
        height,
        hot_x: u16::try_from(info.xHotspot).unwrap_or(u16::MAX).min(width - 1),
        hot_y: u16::try_from(info.yHotspot).unwrap_or(u16::MAX).min(height - 1),
        rgba,
    })
}

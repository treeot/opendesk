//! The host's mouse cursor shape, read so the viewer can draw a matching
//! pointer locally instead of waiting for it in the video.
//!
//! Capture runs with the hardware cursor excluded from the frame, so the
//! viewer's own pointer stands in for it. [`sample`] reports the host's current
//! cursor image (or that it is hidden), and the session loop forwards changes
//! to the viewer as `CursorShape` / `CursorHidden`.

/// A cursor image in straight-alpha RGBA, top row first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    /// The click point within the image.
    pub hot_x: u16,
    pub hot_y: u16,
    pub rgba: Vec<u8>,
}

/// What the pointer looks like right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorSample {
    /// The platform could not report the cursor; leave the viewer's pointer be.
    Unavailable,
    /// The cursor is hidden (for example a game captured the mouse).
    Hidden,
    /// The cursor is visible with this image.
    Visible(CursorImage),
}

/// A cheap fingerprint so the session only resends the shape when it changes.
pub fn fingerprint(image: &CursorImage) -> u64 {
    // FNV-1a over the dimensions, hotspot and pixels.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |byte: u8| {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for value in [image.width, image.height, image.hot_x, image.hot_y] {
        mix(value as u8);
        mix((value >> 8) as u8);
    }
    for &byte in &image.rgba {
        mix(byte);
    }
    hash
}

/// The host's current cursor. Cheap enough to poll a few times a second.
pub fn sample() -> CursorSample {
    platform::sample()
}

#[cfg(target_os = "macos")]
#[path = "cursor/macos.rs"]
mod platform;

#[cfg(target_os = "windows")]
#[path = "cursor/windows.rs"]
mod platform;

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
mod platform {
    /// No cursor-shape source on this platform yet; the viewer keeps its own
    /// default pointer.
    pub(super) fn sample() -> super::CursorSample {
        super::CursorSample::Unavailable
    }
}

//! Stream sizing, rate and keyframe policy for the host video pipeline.
//!
//! Everything here is pure so the policy can be unit tested without a
//! capturer or encoder.

use super::Encoder;
use crate::transport::RateControl;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Frame rate the host captures and encodes at.
pub const STREAM_FPS: u32 = 60;
/// Longest side of the encoded stream. Larger captures (Retina, 4K) are
/// scaled down, keeping their aspect ratio.
pub const MAX_STREAM_LONG_SIDE: u16 = 1920;
/// Hard H.264 width limit for the levels browsers decode; never exceeded.
pub const MAX_STREAM_WIDTH: u16 = 4096;
/// Target bits per pixel, in thousandths: 1080p60 comes out near 12 Mbps.
pub const BITS_PER_PIXEL_MILLI: u64 = 100;
/// Bitrate floor, so small streams still look sharp.
pub const MIN_BITRATE: u32 = 2_000_000;
/// Bitrate ceiling.
pub const MAX_BITRATE: u32 = 20_000_000;

/// A keyframe is forced at least this often, so a viewer that lost data
/// without noticing still recovers.
pub const PERIODIC_KEYFRAME_INTERVAL: Duration = Duration::from_secs(10);
/// Viewer keyframe requests closer than this to the last keyframe are
/// ignored: the keyframe is most likely already on its way.
pub const MIN_REQUESTED_KEYFRAME_SPACING: Duration = Duration::from_millis(250);
/// On a static screen (no new captures) the last frame is re-encoded this
/// often, which lets the encoder refine the picture and keeps the stream
/// alive without spending bandwidth on identical frames at full rate.
pub const STATIC_REPEAT_INTERVAL: Duration = Duration::from_millis(250);

/// Size of the encoded stream for a capture of `width` x `height`.
///
/// Even dimensions (4:2:0), long side at most [`MAX_STREAM_LONG_SIDE`],
/// width at most [`MAX_STREAM_WIDTH`], aspect ratio kept. Returns `(0, 0)`
/// for a degenerate capture.
pub fn stream_dims(width: u16, height: u16) -> (u16, u16) {
    let (w, h) = (u32::from(width), u32::from(height));
    let long = w.max(h);
    if long < 2 || w.min(h) < 2 {
        return (0, 0);
    }
    let limit = u32::from(MAX_STREAM_LONG_SIDE.min(MAX_STREAM_WIDTH));
    let (w, h) = if long > limit {
        // Round to nearest so e.g. 2880x1800 maps to exactly 1920x1200.
        ((w * limit + long / 2) / long, (h * limit + long / 2) / long)
    } else {
        (w, h)
    };
    // Within u16: both sides are at most the limit here.
    let even = |v: u32| u16::try_from((v & !1).max(2)).unwrap_or(MAX_STREAM_LONG_SIDE);
    (even(w), even(h))
}

/// Target bitrate (bps) for a stream of the given size and frame rate.
pub fn stream_bitrate(width: u16, height: u16, fps: u32) -> u32 {
    let bits = u64::from(width) * u64::from(height) * u64::from(fps) * BITS_PER_PIXEL_MILLI / 1_000;
    u32::try_from(bits).unwrap_or(u32::MAX).clamp(MIN_BITRATE, MAX_BITRATE)
}

/// When the encoder must produce a keyframe: periodically, on accepted
/// viewer requests, and never more often than requests deserve.
#[derive(Debug)]
pub struct KeyframePolicy {
    last_keyframe: Instant,
    requested: bool,
}

impl KeyframePolicy {
    /// A policy for an encoder that starts with a keyframe at `now`.
    pub fn new(now: Instant) -> Self {
        Self { last_keyframe: now, requested: false }
    }

    /// A viewer asked for a keyframe. Returns whether it was accepted
    /// (not rate limited).
    pub fn request(&mut self, now: Instant) -> bool {
        if now.saturating_duration_since(self.last_keyframe) < MIN_REQUESTED_KEYFRAME_SPACING {
            return false;
        }
        self.requested = true;
        true
    }

    /// Whether the next encoded frame must be a keyframe.
    pub fn due(&self, now: Instant) -> bool {
        self.requested || now.saturating_duration_since(self.last_keyframe) >= PERIODIC_KEYFRAME_INTERVAL
    }

    /// A keyframe went out at `now` (forced or not).
    pub fn keyframe_sent(&mut self, now: Instant) {
        self.last_keyframe = now;
        self.requested = false;
    }
}

/// Bitrate changes smaller than this fraction of the current bitrate (in
/// percent) are not worth reconfiguring the encoder for.
const BITRATE_DEADBAND_PERCENT: u32 = 5;

/// Keeps an encoder on the bitrate the transport estimates for the path.
///
/// Publishes the stream's bitrate as the ceiling and applies the target
/// when it moved by more than a small dead band. A new encoder (stream size
/// change) starts at the ceiling, so the target is reapplied to it.
pub struct BitrateFollower {
    rate: Arc<RateControl>,
    /// The bitrate the encoder runs at; 0 for a new encoder.
    applied: u32,
    /// Viewer-chosen ceiling in bps; 0 means no cap (fully automatic).
    cap: u32,
}

impl BitrateFollower {
    pub fn new(rate: Arc<RateControl>) -> Self { Self { rate, applied: 0, cap: 0 } }

    /// The encoder was replaced by one created at the stream bitrate.
    pub fn encoder_replaced(&mut self) { self.applied = 0; }

    /// Cap the bitrate at `bps`, or `0` to go back to fully automatic.
    pub fn set_cap(&mut self, bps: u32) { self.cap = bps; }

    /// Call before each encode of a `width` x `height` stream.
    pub fn update<E: Encoder + ?Sized>(&mut self, encoder: &mut E, width: u16, height: u16) {
        let mut ceiling = stream_bitrate(width, height, STREAM_FPS);
        if self.cap > 0 {
            ceiling = ceiling.min(self.cap);
        }
        self.rate.set_ceiling(ceiling);
        if self.applied == 0 {
            self.applied = ceiling;
        }
        let target = self.rate.target();
        if target.abs_diff(self.applied) > self.applied / 100 * BITRATE_DEADBAND_PERCENT {
            log::debug!("encoder bitrate {} -> {target} bps", self.applied);
            encoder.set_bitrate(target);
            self.applied = target;
        }
    }
}

/// Whether the encode loop should encode now: on every fresh capture, when
/// a keyframe is due, or to repeat a static frame. `since_last_encode` is
/// `None` before the first encode.
pub fn encode_due(fresh: bool, keyframe_due: bool, since_last_encode: Option<Duration>) -> bool {
    fresh || keyframe_due || since_last_encode.is_none_or(|since| since >= STATIC_REPEAT_INTERVAL)
}

/// How long the encode loop may wait for a fresh capture before the static
/// repeat is due, capped at `max_wait` so it keeps serving commands.
pub fn capture_wait(since_last_encode: Option<Duration>, max_wait: Duration) -> Duration {
    since_last_encode
        .map_or(Duration::ZERO, |since| STATIC_REPEAT_INTERVAL.saturating_sub(since))
        .min(max_wait)
}

/// Scales tightly packed BGRA `src` (`sw` x `sh`) to `dw` x `dh`.
///
/// Bilinear sampling at destination pixel centres, which is an exact 2x2
/// box filter at a 2:1 ratio (4K to 1080p). Returns `None` on a size
/// mismatch or an upscale.
pub fn downscale_bgra(src: &[u8], sw: u16, sh: u16, dw: u16, dh: u16) -> Option<Vec<u8>> {
    let (sw, sh, dw, dh) = (usize::from(sw), usize::from(sh), usize::from(dw), usize::from(dh));
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 || dw > sw || dh > sh || src.len() != sw * sh * 4 {
        return None;
    }
    /// Source index pair and 8-bit weight of the second for each output.
    fn taps(src: usize, dst: usize) -> Vec<(usize, usize, u32)> {
        (0..dst)
            .map(|d| {
                // Centre of output pixel d in 16.16 source coordinates.
                let pos = (((2 * d + 1) * src) << 16) / (2 * dst);
                let pos = pos.saturating_sub(1 << 15);
                let i0 = (pos >> 16).min(src - 1);
                let i1 = (i0 + 1).min(src - 1);
                (i0, i1, ((pos >> 8) & 0xFF) as u32)
            })
            .collect()
    }
    let xs = taps(sw, dw);
    let ys = taps(sh, dh);
    let stride = sw * 4;
    let mut out = Vec::with_capacity(dw * dh * 4);
    for &(y0, y1, fy) in &ys {
        let r0 = &src[y0 * stride..][..stride];
        let r1 = &src[y1 * stride..][..stride];
        for &(x0, x1, fx) in &xs {
            for c in 0..4 {
                let top = u32::from(r0[x0 * 4 + c]) * (256 - fx) + u32::from(r0[x1 * 4 + c]) * fx;
                let bottom = u32::from(r1[x0 * 4 + c]) * (256 - fx) + u32::from(r1[x1 * 4 + c]) * fx;
                out.push(((top * (256 - fy) + bottom * fy + (1 << 15)) >> 16) as u8);
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct RecordingEncoder(Vec<u32>);

    impl Encoder for RecordingEncoder {
        fn encode(&mut self, _frame: &crate::encode::RawFrame) -> Option<crate::encode::EncodedFrame> { None }
        fn set_bitrate(&mut self, bps: u32) { self.0.push(bps); }
    }

    #[test]
    fn bitrate_follower_publishes_the_ceiling_and_applies_target_changes() {
        let rate = Arc::new(RateControl::default());
        let mut follower = BitrateFollower::new(Arc::clone(&rate));
        let mut encoder = RecordingEncoder::default();
        let ceiling = stream_bitrate(1920, 1080, STREAM_FPS);
        follower.update(&mut encoder, 1920, 1080);
        assert_eq!(rate.ceiling(), ceiling);
        assert!(encoder.0.is_empty(), "a new encoder already runs at the ceiling");

        // What a transport estimator does.
        rate.set_target(ceiling - ceiling / 50);
        follower.update(&mut encoder, 1920, 1080);
        assert!(encoder.0.is_empty(), "2% is inside the dead band");
        rate.set_target(4_000_000);
        follower.update(&mut encoder, 1920, 1080);
        follower.update(&mut encoder, 1920, 1080);
        assert_eq!(encoder.0, vec![4_000_000]);

        // A smaller stream gets a new encoder at its own ceiling (2 Mbps),
        // which the 4 Mbps target is capped to: nothing to change.
        follower.encoder_replaced();
        let mut replaced = RecordingEncoder::default();
        follower.update(&mut replaced, 640, 360);
        assert_eq!(rate.ceiling(), MIN_BITRATE);
        assert!(replaced.0.is_empty());
        // A lower target is applied to the new encoder.
        rate.set_target(1_500_000);
        follower.update(&mut replaced, 640, 360);
        assert_eq!(replaced.0, vec![1_500_000]);
    }

    #[test]
    fn bitrate_cap_lowers_the_ceiling_and_lifts_again() {
        let rate = Arc::new(RateControl::default());
        let mut follower = BitrateFollower::new(Arc::clone(&rate));
        let mut encoder = RecordingEncoder::default();
        let full = stream_bitrate(1920, 1080, STREAM_FPS);
        assert!(full > 3_000_000, "1080p runs well above the data-saver cap");

        // Warm up: the encoder is now known to run at the automatic ceiling.
        rate.set_target(full);
        follower.update(&mut encoder, 1920, 1080);
        assert!(encoder.0.is_empty());

        // A cap below the automatic ceiling becomes the ceiling, and the
        // plentiful target is clamped down to it.
        follower.set_cap(3_000_000);
        follower.update(&mut encoder, 1920, 1080);
        assert_eq!(rate.ceiling(), 3_000_000);
        assert_eq!(encoder.0, vec![3_000_000]);

        // Back to automatic: the ceiling returns to the stream's own bitrate.
        follower.set_cap(0);
        follower.update(&mut encoder, 1920, 1080);
        assert_eq!(rate.ceiling(), full);
        assert_eq!(encoder.0, vec![3_000_000, full]);
    }

    #[test]
    fn stream_dims_caps_the_long_side_and_keeps_aspect() {
        assert_eq!(stream_dims(2880, 1800), (1920, 1200));
        assert_eq!(stream_dims(3840, 2160), (1920, 1080));
        assert_eq!(stream_dims(5120, 2880), (1920, 1080));
        assert_eq!(stream_dims(5120, 1440), (1920, 540));
        // Portrait: the height is the long side.
        assert_eq!(stream_dims(1440, 2560), (1080, 1920));
    }

    #[test]
    fn stream_dims_leaves_small_captures_but_makes_them_even() {
        assert_eq!(stream_dims(1920, 1080), (1920, 1080));
        assert_eq!(stream_dims(1366, 768), (1366, 768));
        assert_eq!(stream_dims(97, 33), (96, 32));
        assert_eq!(stream_dims(1, 100), (0, 0));
        assert_eq!(stream_dims(0, 0), (0, 0));
    }

    #[test]
    fn stream_dims_never_exceed_codec_limits() {
        for (w, h) in [(65_535, 65_535), (65_535, 2), (2, 65_535), (8192, 4320), (4097, 1)] {
            let (sw, sh) = stream_dims(w, h);
            assert!(sw <= MAX_STREAM_WIDTH && sw.max(sh) <= MAX_STREAM_LONG_SIDE, "{w}x{h} -> {sw}x{sh}");
            assert!(sw % 2 == 0 && sh % 2 == 0);
        }
    }

    #[test]
    fn bitrate_scales_with_pixels_and_fps_within_bounds() {
        let full_hd = stream_bitrate(1920, 1080, 60);
        assert!((10_000_000..=15_000_000).contains(&full_hd), "{full_hd}");
        assert!(stream_bitrate(1920, 1200, 60) > full_hd);
        assert!(stream_bitrate(1920, 1080, 30) < full_hd);
        assert_eq!(stream_bitrate(64, 64, 60), MIN_BITRATE);
        assert_eq!(stream_bitrate(u16::MAX, u16::MAX, 240), MAX_BITRATE);
    }

    #[test]
    fn keyframe_policy_is_periodic() {
        let start = Instant::now();
        let policy = KeyframePolicy::new(start);
        assert!(!policy.due(start + Duration::from_secs(9)));
        assert!(policy.due(start + PERIODIC_KEYFRAME_INTERVAL));
    }

    #[test]
    fn keyframe_requests_are_rate_limited() {
        let start = Instant::now();
        let mut policy = KeyframePolicy::new(start);
        // Right after a keyframe a request is ignored.
        assert!(!policy.request(start + Duration::from_millis(100)));
        assert!(!policy.due(start + Duration::from_millis(100)));
        // Later it is honoured once.
        let later = start + MIN_REQUESTED_KEYFRAME_SPACING;
        assert!(policy.request(later));
        assert!(policy.due(later));
        policy.keyframe_sent(later);
        assert!(!policy.due(later));
        assert!(!policy.request(later + Duration::from_millis(10)));
        assert!(policy.request(later + MIN_REQUESTED_KEYFRAME_SPACING));
    }

    #[test]
    fn any_keyframe_restarts_the_periodic_timer() {
        let start = Instant::now();
        let mut policy = KeyframePolicy::new(start);
        policy.keyframe_sent(start + Duration::from_secs(8));
        assert!(!policy.due(start + PERIODIC_KEYFRAME_INTERVAL));
    }

    #[test]
    fn encoding_follows_captures_and_repeats_static_frames_slowly() {
        let short = Some(Duration::from_millis(5));
        assert!(encode_due(false, false, None), "the first frame encodes at once");
        assert!(encode_due(true, false, short), "fresh captures encode without waiting for a tick");
        assert!(encode_due(false, true, short), "a due keyframe encodes on a static screen");
        assert!(!encode_due(false, false, short));
        assert!(encode_due(false, false, Some(STATIC_REPEAT_INTERVAL)));
    }

    #[test]
    fn capture_wait_ends_at_the_static_repeat() {
        let max = Duration::from_millis(20);
        assert_eq!(capture_wait(None, max), Duration::ZERO);
        assert_eq!(capture_wait(Some(Duration::ZERO), max), max);
        assert_eq!(capture_wait(Some(STATIC_REPEAT_INTERVAL - Duration::from_millis(5)), max), Duration::from_millis(5));
        assert_eq!(capture_wait(Some(Duration::from_secs(5)), max), Duration::ZERO);
    }

    #[test]
    fn downscale_halves_with_a_box_filter() {
        // 4x2 -> 2x1: each output is the mean of a 2x2 block.
        let mut src = Vec::new();
        for value in [0u8, 100, 200, 40, 20, 120, 220, 60] {
            src.extend_from_slice(&[value, value, value, 255]);
        }
        let out = downscale_bgra(&src, 4, 2, 2, 1).expect("scaled");
        assert_eq!(out, [60, 60, 60, 255, 130, 130, 130, 255]);
    }

    #[test]
    fn downscale_keeps_solid_colour_and_size() {
        let src = [10u8, 20, 30, 255].repeat(2880 * 4);
        let out = downscale_bgra(&src, 2880, 4, 1920, 2).expect("scaled");
        assert_eq!(out.len(), 1920 * 2 * 4);
        assert!(out.as_chunks::<4>().0.iter().all(|p| *p == [10, 20, 30, 255]));
    }

    #[test]
    fn downscale_rejects_bad_input() {
        assert!(downscale_bgra(&[0; 16], 2, 2, 4, 4).is_none(), "no upscaling");
        assert!(downscale_bgra(&[0; 15], 2, 2, 1, 1).is_none(), "size mismatch");
        assert!(downscale_bgra(&[], 0, 0, 0, 0).is_none());
    }
}

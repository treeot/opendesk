use host_core::encode::{video_header, Codec, Encoder, H264Encoder, PixelFormat, RawFrame};
use protocol::{crypto::{Opener, Sealer}, Tag};
use protocol::kex::derive_keys;

#[test]
fn captured_frame_roundtrips_through_e2ee() {
    let (send, _) = derive_keys(&[9u8; 32], true);
    let (_, recv) = derive_keys(&[9u8; 32], false);
    let mut sealer = Sealer::new(send);
    let mut opener = Opener::new(recv);

    let mut encoder = H264Encoder::new(64, 64, 15, 1_000_000).expect("openh264");
    let raw = RawFrame {
        width: 64,
        height: 64,
        data: vec![0u8; 64 * 64 * 4],
        format: PixelFormat::Bgra,
        timestamp_us: 0,
    };
    let encoded = encoder.encode(&raw).expect("encode");

    let mut plaintext = video_header(0, 0, encoded.keyframe, 64, 64, Codec::H264);
    plaintext.extend_from_slice(&encoded.data);

    let sealed = sealer.seal(&[Tag::Data as u8], &plaintext);
    let opened = opener.open(&[Tag::Data as u8], &sealed).expect("decrypts with derived key");
    assert_eq!(opened, plaintext, "E2EE round-trip preserves the frame");
    assert_eq!(opened[12], 1);
    assert!(opened.len() > 18);
}

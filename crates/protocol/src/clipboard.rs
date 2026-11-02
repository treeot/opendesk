pub const MAX_CLIPBOARD: usize = 1_000_000;
pub const MAX_CLIPBOARD_IMAGE: usize = 5_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    Image {
        width: u32,
        height: u32,
        png: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardMsg {
    pub seq: u64,
    pub content: ClipboardContent,
}

impl ClipboardMsg {
    pub fn text(seq: u64, text: String) -> Self {
        Self {
            seq,
            content: ClipboardContent::Text(text),
        }
    }
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = self.seq.to_be_bytes().to_vec();
        match &self.content {
            ClipboardContent::Text(text) => {
                let data = text.as_bytes();
                bytes.extend_from_slice(
                    &u32::try_from(data.len())
                        .expect("clipboard text exceeds u32 length")
                        .to_be_bytes(),
                );
                bytes.extend_from_slice(data);
            }
            ClipboardContent::Image { width, height, png } => {
                bytes.extend_from_slice(&0x8000_0000u32.to_be_bytes());
                bytes.extend_from_slice(&width.to_be_bytes());
                bytes.extend_from_slice(&height.to_be_bytes());
                bytes.extend_from_slice(
                    &u32::try_from(png.len())
                        .expect("clipboard image exceeds u32 length")
                        .to_be_bytes(),
                );
                bytes.extend_from_slice(png);
            }
        }
        bytes
    }
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let seq = u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?);
        let marker = u32::from_be_bytes(bytes.get(8..12)?.try_into().ok()?);
        if marker & 0x8000_0000 != 0 {
            if marker != 0x8000_0000 {
                return None;
            }
            let width = u32::from_be_bytes(bytes.get(12..16)?.try_into().ok()?);
            let height = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
            let len =
                usize::try_from(u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?)).ok()?;
            if width == 0
                || height == 0
                || width > 16_384
                || height > 16_384
                || u64::from(width) * u64::from(height) > 100_000_000
                || len > MAX_CLIPBOARD_IMAGE
                || bytes.len() != 24usize.checked_add(len)?
            {
                return None;
            }
            Some(Self {
                seq,
                content: ClipboardContent::Image {
                    width,
                    height,
                    png: bytes.get(24..)?.to_vec(),
                },
            })
        } else {
            let len = usize::try_from(marker).ok()?;
            if len > MAX_CLIPBOARD || bytes.len() != 12usize.checked_add(len)? {
                return None;
            }
            Some(Self {
                seq,
                content: ClipboardContent::Text(String::from_utf8(bytes.get(12..)?.to_vec()).ok()?),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn text_wire_compatibility_and_round_trip() {
        let msg = ClipboardMsg::text(42, "hello".into());
        assert_eq!(
            msg.to_bytes(),
            [
                42u64.to_be_bytes().as_slice(),
                5u32.to_be_bytes().as_slice(),
                b"hello"
            ]
            .concat()
        );
        assert_eq!(ClipboardMsg::from_bytes(&msg.to_bytes()), Some(msg));
    }
    #[test]
    fn image_round_trip_bytes() {
        let msg = ClipboardMsg {
            seq: 7,
            content: ClipboardContent::Image {
                width: 2,
                height: 3,
                png: vec![137, 80, 78, 71],
            },
        };
        let expected = [
            7u64.to_be_bytes().as_slice(),
            0x8000_0000u32.to_be_bytes().as_slice(),
            2u32.to_be_bytes().as_slice(),
            3u32.to_be_bytes().as_slice(),
            4u32.to_be_bytes().as_slice(),
            &[137, 80, 78, 71],
        ]
        .concat();
        assert_eq!(msg.to_bytes(), expected);
        assert_eq!(ClipboardMsg::from_bytes(&expected), Some(msg));
    }
    #[test]
    fn rejects_oversize_and_bad_data() {
        let mut b = vec![];
        b.extend(0u64.to_be_bytes());
        b.extend(0x8000_0000u32.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(1u32.to_be_bytes());
        b.extend(
            u32::try_from(MAX_CLIPBOARD_IMAGE + 1)
                .unwrap()
                .to_be_bytes(),
        );
        assert!(ClipboardMsg::from_bytes(&b).is_none());
        assert!(ClipboardMsg::from_bytes(&[0xff, 0]).is_none());
    }
}

pub const MAX_CLIPBOARD: usize = 1_000_000;
pub const MAX_CLIPBOARD_IMAGE: usize = 5_000_000;
/// Most entries one file-list announcement may carry.
pub const MAX_CLIPBOARD_FILES: usize = 1_000;
/// Longest file name, in bytes, a file-list announcement may carry.
pub const MAX_CLIPBOARD_FILE_NAME: usize = 255;

const IMAGE_MARKER: u32 = 0x8000_0000;
const FILES_MARKER: u32 = 0x8000_0001;

/// One copied file or folder, announced by name only. The full local path
/// stays on the computer that copied it; the peer asks for an entry by its
/// index in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardFile {
    pub name: String,
    /// Size in bytes; 0 for a folder.
    pub size: u64,
    pub is_dir: bool,
}

/// True when `name` is one plain path component: no separators, not `.` or
/// `..`, no NUL, and within the length limit.
pub fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_CLIPBOARD_FILE_NAME
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\', '\0'])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    Text(String),
    Image {
        width: u32,
        height: u32,
        png: Vec<u8>,
    },
    /// Files or folders were copied. Only an announcement: content moves when
    /// the peer pastes, over the file transfer path. Peers that predate it
    /// fail to decode the message and drop it.
    Files(Vec<ClipboardFile>),
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
                bytes.extend_from_slice(&IMAGE_MARKER.to_be_bytes());
                bytes.extend_from_slice(&width.to_be_bytes());
                bytes.extend_from_slice(&height.to_be_bytes());
                bytes.extend_from_slice(
                    &u32::try_from(png.len())
                        .expect("clipboard image exceeds u32 length")
                        .to_be_bytes(),
                );
                bytes.extend_from_slice(png);
            }
            ClipboardContent::Files(files) => {
                bytes.extend_from_slice(&FILES_MARKER.to_be_bytes());
                bytes.extend_from_slice(
                    &u32::try_from(files.len())
                        .expect("clipboard file list exceeds u32 length")
                        .to_be_bytes(),
                );
                for file in files {
                    bytes.push(u8::from(file.is_dir));
                    bytes.extend_from_slice(&file.size.to_be_bytes());
                    let name = file.name.as_bytes();
                    bytes.extend_from_slice(
                        &u16::try_from(name.len())
                            .expect("clipboard file name exceeds u16 length")
                            .to_be_bytes(),
                    );
                    bytes.extend_from_slice(name);
                }
            }
        }
        bytes
    }
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let seq = u64::from_be_bytes(bytes.get(..8)?.try_into().ok()?);
        let marker = u32::from_be_bytes(bytes.get(8..12)?.try_into().ok()?);
        if marker == FILES_MARKER {
            return Some(Self {
                seq,
                content: ClipboardContent::Files(decode_files(bytes.get(12..)?)?),
            });
        }
        if marker & IMAGE_MARKER != 0 {
            if marker != IMAGE_MARKER {
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

fn decode_files(mut bytes: &[u8]) -> Option<Vec<ClipboardFile>> {
    let count = usize::try_from(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?)).ok()?;
    if count == 0 || count > MAX_CLIPBOARD_FILES {
        return None;
    }
    bytes = &bytes[4..];
    let mut files = Vec::with_capacity(count);
    for _ in 0..count {
        let is_dir = match *bytes.first()? {
            0 => false,
            1 => true,
            _ => return None,
        };
        let size = u64::from_be_bytes(bytes.get(1..9)?.try_into().ok()?);
        let len = usize::from(u16::from_be_bytes(bytes.get(9..11)?.try_into().ok()?));
        let name = std::str::from_utf8(bytes.get(11..11 + len)?).ok()?;
        if !valid_file_name(name) {
            return None;
        }
        files.push(ClipboardFile {
            name: name.to_owned(),
            size: if is_dir { 0 } else { size },
            is_dir,
        });
        bytes = &bytes[11 + len..];
    }
    bytes.is_empty().then_some(files)
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
    fn file(name: &str, size: u64, is_dir: bool) -> ClipboardFile {
        ClipboardFile {
            name: name.into(),
            size,
            is_dir,
        }
    }
    #[test]
    fn files_round_trip_bytes() {
        let msg = ClipboardMsg {
            seq: 9,
            content: ClipboardContent::Files(vec![
                file("report.pdf", 1234, false),
                file("photos", 0, true),
            ]),
        };
        let expected = [
            9u64.to_be_bytes().as_slice(),
            0x8000_0001u32.to_be_bytes().as_slice(),
            2u32.to_be_bytes().as_slice(),
            &[0],
            1234u64.to_be_bytes().as_slice(),
            10u16.to_be_bytes().as_slice(),
            b"report.pdf",
            &[1],
            0u64.to_be_bytes().as_slice(),
            6u16.to_be_bytes().as_slice(),
            b"photos",
        ]
        .concat();
        assert_eq!(msg.to_bytes(), expected);
        assert_eq!(ClipboardMsg::from_bytes(&expected), Some(msg));
    }
    #[test]
    fn rejects_unsafe_or_malformed_file_lists() {
        for name in ["", ".", "..", "a/b", "a\\b", "nul\0", &"x".repeat(256)] {
            let msg = ClipboardMsg {
                seq: 1,
                content: ClipboardContent::Files(vec![file(name, 1, false)]),
            };
            assert!(ClipboardMsg::from_bytes(&msg.to_bytes()).is_none(), "{name:?}");
        }
        let empty = ClipboardMsg {
            seq: 1,
            content: ClipboardContent::Files(vec![]),
        };
        assert!(ClipboardMsg::from_bytes(&empty.to_bytes()).is_none());
        let good = ClipboardMsg {
            seq: 1,
            content: ClipboardContent::Files(vec![file("a", 1, false)]),
        }
        .to_bytes();
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(ClipboardMsg::from_bytes(&trailing).is_none());
        assert!(ClipboardMsg::from_bytes(&good[..good.len() - 1]).is_none());
        let mut bad_flag = good;
        bad_flag[16] = 2;
        assert!(ClipboardMsg::from_bytes(&bad_flag).is_none());
    }
}

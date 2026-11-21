#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileMsg {
    Offer {
        id: u64,
        name: String,
        size: u64,
        hash: [u8; 32],
    },
    Accept {
        id: u64,
    },
    Reject {
        id: u64,
        reason: String,
    },
    Chunk {
        id: u64,
        offset: u64,
        data: Vec<u8>,
    },
    Done {
        id: u64,
    },
    Error {
        id: u64,
        message: String,
    },
    /// Receiver -> sender flow control: every byte below `offset` has been
    /// written. The sender keeps a bounded window un-acknowledged, and a
    /// final `Ack` at the full size confirms the verified file.
    Ack {
        id: u64,
        offset: u64,
    },
    /// Sender -> receiver: a whole folder, offered once. The receiver answers
    /// at once with `Ack { id, offset: 0 }` (seen, asking the user), then
    /// `Accept` or `Reject`. An older peer that does not know this message
    /// never acks, so the sender gives up quickly instead of hanging.
    FolderOffer {
        id: u64,
        name: String,
        files: u32,
        total: u64,
    },
    /// A directory inside folder `folder`, as a '/'-separated relative path.
    /// The receiver confirms every 32nd with `Ack { id: folder, offset: count }`.
    FolderDir {
        folder: u64,
        path: String,
    },
    /// One file of folder `folder`: an `Offer` with a '/'-separated relative
    /// path. Its chunks and replies use the file's own `id` as usual.
    FolderFile {
        folder: u64,
        id: u64,
        path: String,
        size: u64,
        hash: [u8; 32],
    },
    /// Every file of the folder has been sent. The receiver confirms with an
    /// `Ack` on the folder id.
    FolderDone {
        id: u64,
    },
}

impl FileMsg {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        match self {
            Self::Offer {
                id,
                name,
                size,
                hash,
            } => {
                bytes.push(1);
                put_u64(&mut bytes, *id);
                put_bytes(&mut bytes, name.as_bytes());
                put_u64(&mut bytes, *size);
                bytes.extend_from_slice(hash);
            }
            Self::Accept { id } => {
                bytes.push(2);
                put_u64(&mut bytes, *id);
            }
            Self::Reject { id, reason } => {
                bytes.push(3);
                put_u64(&mut bytes, *id);
                put_bytes(&mut bytes, reason.as_bytes());
            }
            Self::Chunk { id, offset, data } => {
                bytes.push(4);
                put_u64(&mut bytes, *id);
                put_u64(&mut bytes, *offset);
                put_bytes(&mut bytes, data);
            }
            Self::Done { id } => {
                bytes.push(5);
                put_u64(&mut bytes, *id);
            }
            Self::Error { id, message } => {
                bytes.push(6);
                put_u64(&mut bytes, *id);
                put_bytes(&mut bytes, message.as_bytes());
            }
            Self::Ack { id, offset } => {
                bytes.push(7);
                put_u64(&mut bytes, *id);
                put_u64(&mut bytes, *offset);
            }
            Self::FolderOffer {
                id,
                name,
                files,
                total,
            } => {
                bytes.push(8);
                put_u64(&mut bytes, *id);
                put_bytes(&mut bytes, name.as_bytes());
                bytes.extend_from_slice(&files.to_be_bytes());
                put_u64(&mut bytes, *total);
            }
            Self::FolderDir { folder, path } => {
                bytes.push(9);
                put_u64(&mut bytes, *folder);
                put_bytes(&mut bytes, path.as_bytes());
            }
            Self::FolderFile {
                folder,
                id,
                path,
                size,
                hash,
            } => {
                bytes.push(10);
                put_u64(&mut bytes, *folder);
                put_u64(&mut bytes, *id);
                put_bytes(&mut bytes, path.as_bytes());
                put_u64(&mut bytes, *size);
                bytes.extend_from_slice(hash);
            }
            Self::FolderDone { id } => {
                bytes.push(11);
                put_u64(&mut bytes, *id);
            }
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<FileMsg> {
        let mut reader = Reader::new(bytes);
        let variant = reader.take_u8()?;
        let message = match variant {
            1 => {
                let id = reader.take_u64()?;
                let name = reader.take_string()?;
                let size = reader.take_u64()?;
                let hash = reader.take_array::<32>()?;
                Self::Offer {
                    id,
                    name,
                    size,
                    hash,
                }
            }
            2 => Self::Accept {
                id: reader.take_u64()?,
            },
            3 => Self::Reject {
                id: reader.take_u64()?,
                reason: reader.take_string()?,
            },
            4 => Self::Chunk {
                id: reader.take_u64()?,
                offset: reader.take_u64()?,
                data: reader.take_bytes()?,
            },
            5 => Self::Done {
                id: reader.take_u64()?,
            },
            6 => Self::Error {
                id: reader.take_u64()?,
                message: reader.take_string()?,
            },
            7 => Self::Ack {
                id: reader.take_u64()?,
                offset: reader.take_u64()?,
            },
            8 => Self::FolderOffer {
                id: reader.take_u64()?,
                name: reader.take_string()?,
                files: reader.take_u32()?,
                total: reader.take_u64()?,
            },
            9 => Self::FolderDir {
                folder: reader.take_u64()?,
                path: reader.take_string()?,
            },
            10 => Self::FolderFile {
                folder: reader.take_u64()?,
                id: reader.take_u64()?,
                path: reader.take_string()?,
                size: reader.take_u64()?,
                hash: reader.take_array::<32>()?,
            },
            11 => Self::FolderDone {
                id: reader.take_u64()?,
            },
            _ => return None,
        };

        reader.is_empty().then_some(message)
    }
}

fn put_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_be_bytes());
}

fn put_bytes(bytes: &mut Vec<u8>, value: &[u8]) {
    let length = u32::try_from(value.len()).expect("file-transfer field exceeds u32 length");
    bytes.extend_from_slice(&length.to_be_bytes());
    bytes.extend_from_slice(value);
}

struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, position: 0 }
    }

    fn take(&mut self, length: usize) -> Option<&'a [u8]> {
        let end = self.position.checked_add(length)?;
        let value = self.bytes.get(self.position..end)?;
        self.position = end;
        Some(value)
    }

    fn take_u8(&mut self) -> Option<u8> {
        Some(*self.take(1)?.first()?)
    }

    fn take_u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn take_u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take(8)?.try_into().ok()?))
    }

    fn take_bytes(&mut self) -> Option<Vec<u8>> {
        let length = usize::try_from(self.take_u32()?).ok()?;
        Some(self.take(length)?.to_vec())
    }

    fn take_string(&mut self) -> Option<String> {
        String::from_utf8(self.take_bytes()?).ok()
    }

    fn take_array<const N: usize>(&mut self) -> Option<[u8; N]> {
        self.take(N)?.try_into().ok()
    }

    fn is_empty(&self) -> bool {
        self.position == self.bytes.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(message: FileMsg) {
        let bytes = message.to_bytes();
        assert_eq!(FileMsg::from_bytes(&bytes), Some(message));
    }

    #[test]
    fn offer_roundtrips_with_unicode_filename_and_hash() {
        round_trip(FileMsg::Offer {
            id: 7,
            name: "résumé 📄.pdf".into(),
            size: 123_456,
            hash: [0xAB; 32],
        });
    }

    #[test]
    fn accept_roundtrips() {
        round_trip(FileMsg::Accept { id: 8 });
    }

    #[test]
    fn reject_roundtrips() {
        round_trip(FileMsg::Reject {
            id: 9,
            reason: "not now".into(),
        });
    }

    #[test]
    fn chunk_roundtrips_with_binary_data_and_offset() {
        round_trip(FileMsg::Chunk {
            id: 10,
            offset: 4_096,
            data: vec![0, 255, 1, 254, 2, 253],
        });
    }

    #[test]
    fn done_roundtrips() {
        round_trip(FileMsg::Done { id: 11 });
    }

    #[test]
    fn error_roundtrips() {
        round_trip(FileMsg::Error {
            id: 12,
            message: "hash mismatch".into(),
        });
    }

    #[test]
    fn ack_roundtrips() {
        round_trip(FileMsg::Ack {
            id: 13,
            offset: 2 * 1024 * 1024,
        });
    }

    #[test]
    fn folder_messages_roundtrip() {
        round_trip(FileMsg::FolderOffer {
            id: 14,
            name: "Photos 2024".into(),
            files: 120,
            total: 9_000_000,
        });
        round_trip(FileMsg::FolderDir {
            folder: 14,
            path: "trip/day 1".into(),
        });
        round_trip(FileMsg::FolderFile {
            folder: 14,
            id: 15,
            path: "trip/day 1/beach.jpg".into(),
            size: 4_096,
            hash: [0x5A; 32],
        });
        round_trip(FileMsg::FolderDone { id: 14 });
    }

    #[test]
    fn rejects_empty_and_unknown_variant() {
        assert!(FileMsg::from_bytes(&[]).is_none());
        assert!(FileMsg::from_bytes(&[0xFF]).is_none());
    }

    #[test]
    fn rejects_truncated_chunk() {
        let mut bytes = vec![4];
        bytes.extend_from_slice(&1u64.to_be_bytes());
        bytes.extend_from_slice(&2u64.to_be_bytes());
        bytes.extend_from_slice(&5u32.to_be_bytes());
        bytes.extend_from_slice(&[1, 2, 3]);
        assert!(FileMsg::from_bytes(&bytes).is_none());
    }
}

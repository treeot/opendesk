use crate::{browse::BrowseMsg, chat::ChatMsg, clipboard::ClipboardMsg, filexfer::FileMsg, tunnel::TunnelMsg};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataMsg {
    FileTransfer(FileMsg),
    Clipboard(ClipboardMsg),
    Chat(ChatMsg),
    Browse(BrowseMsg),
    Tunnel(TunnelMsg),
}

impl DataMsg {
    pub fn to_bytes(&self) -> Vec<u8> {
        let (tag, inner) = match self {
            Self::FileTransfer(message) => (1, message.to_bytes()),
            Self::Clipboard(message) => (2, message.to_bytes()),
            Self::Chat(message) => (3, message.to_bytes()),
            Self::Browse(message) => (4, message.to_bytes()),
            Self::Tunnel(message) => (5, message.to_bytes()),
        };
        let mut bytes = Vec::with_capacity(1 + inner.len());
        bytes.push(tag);
        bytes.extend_from_slice(&inner);
        bytes
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (tag, inner) = bytes.split_first()?;
        match tag {
            1 => FileMsg::from_bytes(inner).map(Self::FileTransfer),
            2 => ClipboardMsg::from_bytes(inner).map(Self::Clipboard),
            3 => ChatMsg::from_bytes(inner).map(Self::Chat),
            4 => BrowseMsg::from_bytes(inner).map(Self::Browse),
            5 => TunnelMsg::from_bytes(inner).map(Self::Tunnel),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_file_transfer_clipboard_and_chat_variants() {
        let file = DataMsg::FileTransfer(FileMsg::Chunk {
            id: 11,
            offset: 128,
            data: vec![1, 2, 3, 255],
        });
        let clipboard = DataMsg::Clipboard(ClipboardMsg {
            seq: 12,
            text: "shared 📋".into(),
        });
        let chat = DataMsg::Chat(ChatMsg {
            text: "hello 💬".into(),
        });

        assert_eq!(DataMsg::from_bytes(&file.to_bytes()), Some(file));
        assert_eq!(DataMsg::from_bytes(&clipboard.to_bytes()), Some(clipboard));
        assert_eq!(DataMsg::from_bytes(&chat.to_bytes()), Some(chat));
    }

    #[test]
    fn rejects_empty_unknown_and_garbage_bytes() {
        assert!(DataMsg::from_bytes(&[]).is_none());
        assert!(DataMsg::from_bytes(&[0]).is_none());
        assert!(DataMsg::from_bytes(&[1, 0xff]).is_none());
    }
}

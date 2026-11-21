//! Sans-IO file transfer for the browser viewer's sealed Data channel.
//!
//! This mirrors `host_core::filexfer` but keeps whole files in memory (the
//! browser has no streaming filesystem here) and never does its own IO: the
//! page supplies outgoing bytes and takes delivered bytes. [`Transfers`] owns
//! the state of every concurrent transfer in both directions. It returns the
//! `FileMsg`s to seal and send plus [`FileEvent`]s for the page; the caller
//! (see `lib.rs`) seals and queues them. The receive flow is offer, consent,
//! chunks, SHA-256 verify, done; the send flow is hash, offer, windowed chunks
//! paced by acks.

use protocol::filexfer::FileMsg;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const CHUNK_SIZE: usize = 64 * 1024;
/// Acknowledge progress every this many chunks, like the desktop.
pub const ACK_EVERY_CHUNKS: u64 = 8;
/// Most bytes a sender keeps un-acknowledged (2 MiB), under the reliable
/// channel's pending Data budget so a slow receiver applies backpressure.
pub const SEND_WINDOW_BYTES: u64 = 32 * CHUNK_SIZE as u64;
/// Concurrent incoming offers accepted at once; further offers are refused.
pub const MAX_INCOMING_TRANSFERS: usize = 8;

/// Something the page should learn about a transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileEvent {
    /// An incoming offer awaiting the user's Accept or Reject.
    Offer { id: u64, name: String, size: u64 },
    /// Transfer progress. `sending` is true for our outgoing files.
    Progress { id: u64, transferred: u64, total: u64, sending: bool },
    /// A verified incoming file, ready for the page to save.
    Received { id: u64, name: String, data: Vec<u8> },
    /// An outgoing file the receiver has verified and stored.
    Sent { id: u64 },
    /// A transfer that failed or was refused, in either direction.
    Failed { id: u64, message: String },
}

struct Incoming {
    name: String,
    size: u64,
    hash: [u8; 32],
    accepted: bool,
    data: Vec<u8>,
    hasher: Sha256,
    chunks: u64,
}

struct Outgoing {
    data: Vec<u8>,
    size: u64,
    sent: u64,
    acked: u64,
    done: bool,
}

#[derive(Default)]
pub struct Transfers {
    incoming: HashMap<u64, Incoming>,
    outgoing: HashMap<u64, Outgoing>,
}

/// What a step produced: messages to seal and send, and events for the page.
type Step = (Vec<FileMsg>, Vec<FileEvent>);

impl Transfers {
    /// Begin sending `data` as `name` under the page-chosen `id`. Returns the
    /// Offer to seal, or `None` if the id is already in use.
    pub fn send_file(&mut self, id: u64, name: &str, data: Vec<u8>) -> Option<FileMsg> {
        if self.outgoing.contains_key(&id) {
            return None;
        }
        let name = sanitize_filename(name)?;
        let size = data.len() as u64;
        let hash: [u8; 32] = Sha256::digest(&data).into();
        self.outgoing.insert(id, Outgoing { data, size, sent: 0, acked: 0, done: false });
        Some(FileMsg::Offer { id, name, size, hash })
    }

    /// Accept an incoming offer. Returns the Accept to seal, or `None`.
    pub fn accept(&mut self, id: u64) -> Option<FileMsg> {
        let incoming = self.incoming.get_mut(&id)?;
        if incoming.accepted {
            return None;
        }
        incoming.accepted = true;
        Some(FileMsg::Accept { id })
    }

    /// Reject and drop an incoming offer. Returns the Reject to seal.
    pub fn reject(&mut self, id: u64) -> Option<FileMsg> {
        self.incoming.remove(&id)?;
        Some(FileMsg::Reject { id, reason: "declined".into() })
    }

    /// Handle one `FileMsg` from the peer.
    pub fn on_message(&mut self, message: FileMsg) -> Step {
        match message {
            FileMsg::Offer { id, name, size, hash } => self.on_offer(id, name, size, hash),
            FileMsg::Accept { id } => self.on_accept(id),
            FileMsg::Reject { id, reason } => self.on_reject(id, reason),
            FileMsg::Ack { id, offset } => self.on_ack(id, offset),
            FileMsg::Chunk { id, offset, data } => self.on_chunk(id, offset, data),
            FileMsg::Done { id } => self.on_done(id),
            FileMsg::Error { id, message } => self.on_error(id, message),
            // The browser viewer does not take folders. Refusing the offer at
            // once lets the desktop sender stop cleanly; nothing else of the
            // folder follows a refused offer.
            FileMsg::FolderOffer { id, .. } => {
                (vec![FileMsg::Reject { id, reason: "This viewer cannot receive folders.".into() }], Vec::new())
            }
            FileMsg::FolderDir { .. } | FileMsg::FolderFile { .. } | FileMsg::FolderDone { .. } => (Vec::new(), Vec::new()),
        }
    }

    fn on_offer(&mut self, id: u64, name: String, size: u64, hash: [u8; 32]) -> Step {
        let Some(name) = sanitize_filename(&name) else {
            return (vec![FileMsg::Reject { id, reason: "invalid file name".into() }], Vec::new());
        };
        if self.incoming.contains_key(&id) || self.incoming.len() >= MAX_INCOMING_TRANSFERS {
            return (vec![FileMsg::Reject { id, reason: "too many transfers".into() }], Vec::new());
        }
        self.incoming.insert(id, Incoming {
            name: name.clone(),
            size,
            hash,
            accepted: false,
            data: Vec::new(),
            hasher: Sha256::new(),
            chunks: 0,
        });
        (Vec::new(), vec![FileEvent::Offer { id, name, size }])
    }

    fn on_accept(&mut self, id: u64) -> Step {
        if !self.outgoing.contains_key(&id) {
            return (Vec::new(), Vec::new());
        }
        let messages = self.pump(id);
        let events = self.outgoing.get(&id).map(|out| vec![FileEvent::Progress {
            id,
            transferred: out.sent,
            total: out.size,
            sending: true,
        }]).unwrap_or_default();
        (messages, events)
    }

    fn on_reject(&mut self, id: u64, reason: String) -> Step {
        if self.outgoing.remove(&id).is_some() {
            let message = if reason.is_empty() { "the other side declined the file".to_owned() } else { reason };
            return (Vec::new(), vec![FileEvent::Failed { id, message }]);
        }
        (Vec::new(), Vec::new())
    }

    fn on_ack(&mut self, id: u64, offset: u64) -> Step {
        let Some(out) = self.outgoing.get_mut(&id) else { return (Vec::new(), Vec::new()) };
        out.acked = out.acked.max(offset);
        // The final full-size ack confirms the verified, stored file.
        if out.done && out.acked >= out.size {
            self.outgoing.remove(&id);
            return (Vec::new(), vec![FileEvent::Sent { id }]);
        }
        let messages = self.pump(id);
        let events = self.outgoing.get(&id).map(|out| vec![FileEvent::Progress {
            id,
            transferred: out.sent,
            total: out.size,
            sending: true,
        }]).unwrap_or_default();
        (messages, events)
    }

    /// Queue as many chunks as the un-acknowledged window allows, then Done.
    fn pump(&mut self, id: u64) -> Vec<FileMsg> {
        let Some(out) = self.outgoing.get_mut(&id) else { return Vec::new() };
        let mut messages = Vec::new();
        while out.sent < out.size && out.sent - out.acked < SEND_WINDOW_BYTES {
            let end = (out.sent + CHUNK_SIZE as u64).min(out.size);
            let offset = out.sent;
            messages.push(FileMsg::Chunk { id, offset, data: out.data[offset as usize..end as usize].to_vec() });
            out.sent = end;
        }
        if out.sent == out.size && !out.done {
            out.done = true;
            messages.push(FileMsg::Done { id });
        }
        messages
    }

    fn on_chunk(&mut self, id: u64, offset: u64, data: Vec<u8>) -> Step {
        let Some(incoming) = self.incoming.get_mut(&id) else { return (Vec::new(), Vec::new()) };
        if !incoming.accepted {
            return self.fail_incoming(id, "chunk before acceptance");
        }
        if offset != incoming.data.len() as u64
            || data.len() > CHUNK_SIZE
            || offset + data.len() as u64 > incoming.size
        {
            return self.fail_incoming(id, "invalid chunk ordering or size");
        }
        incoming.hasher.update(&data);
        incoming.data.extend_from_slice(&data);
        incoming.chunks += 1;
        let received = incoming.data.len() as u64;
        let mut messages = Vec::new();
        if incoming.chunks.is_multiple_of(ACK_EVERY_CHUNKS) && received < incoming.size {
            messages.push(FileMsg::Ack { id, offset: received });
        }
        let events = vec![FileEvent::Progress { id, transferred: received, total: incoming.size, sending: false }];
        (messages, events)
    }

    fn on_done(&mut self, id: u64) -> Step {
        let Some(incoming) = self.incoming.get(&id) else { return (Vec::new(), Vec::new()) };
        if incoming.data.len() as u64 != incoming.size {
            return self.fail_incoming(id, "transfer ended at the wrong size");
        }
        let incoming = self.incoming.remove(&id).expect("present");
        let actual: [u8; 32] = incoming.hasher.finalize().into();
        if actual != incoming.hash {
            return (
                vec![FileMsg::Error { id, message: "file transfer hash mismatch".into() }],
                vec![FileEvent::Failed { id, message: "file transfer hash mismatch".into() }],
            );
        }
        (
            vec![FileMsg::Ack { id, offset: incoming.size }],
            vec![FileEvent::Received { id, name: incoming.name, data: incoming.data }],
        )
    }

    fn on_error(&mut self, id: u64, message: String) -> Step {
        if self.outgoing.remove(&id).is_some() || self.incoming.remove(&id).is_some() {
            let message = if message.is_empty() { "the other side ended the transfer".to_owned() } else { message };
            return (Vec::new(), vec![FileEvent::Failed { id, message }]);
        }
        (Vec::new(), Vec::new())
    }

    fn fail_incoming(&mut self, id: u64, message: &str) -> Step {
        self.incoming.remove(&id);
        (
            vec![FileMsg::Error { id, message: message.to_owned() }],
            vec![FileEvent::Failed { id, message: message.to_owned() }],
        )
    }
}

/// A filename safe to offer to a browser download. Rejects path separators,
/// control and bidi/format characters, empty, hidden and trailing-dot/space
/// names. Mirrors the host's `sanitize_filename` closely enough for the viewer.
pub fn sanitize_filename(name: &str) -> Option<String> {
    if name.is_empty()
        || name.len() > 255
        || name.starts_with('.')
        || name.ends_with('.')
        || name.ends_with(' ')
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.contains('\0')
        || name.chars().any(|c| c.is_control() || is_format_char(c))
    {
        return None;
    }
    Some(name.to_owned())
}

/// Unicode general category Cf (format) characters that can disguise a name.
fn is_format_char(c: char) -> bool {
    matches!(
        c as u32,
        0x00AD
            | 0x0600..=0x0605
            | 0x061C
            | 0x06DD
            | 0x070F
            | 0x0890..=0x0891
            | 0x08E2
            | 0x180E
            | 0x200B..=0x200F
            | 0x202A..=0x202E
            | 0x2060..=0x2064
            | 0x2066..=0x206F
            | 0xFEFF
            | 0xFFF9..=0xFFFB
            | 0x110BD
            | 0x110CD
            | 0x13430..=0x1343F
            | 0x1BCA0..=0x1BCA3
            | 0x1D173..=0x1D17A
            | 0xE0001
            | 0xE0020..=0xE007F
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(data: &[u8]) -> [u8; 32] {
        Sha256::digest(data).into()
    }

    #[test]
    fn rejects_unsafe_names() {
        for bad in ["", "../x", "a/b", "a\\b", ".hidden", "file.", "c:evil", "a\u{202E}b"] {
            assert_eq!(sanitize_filename(bad), None, "{bad:?}");
        }
        assert_eq!(sanitize_filename("report.pdf"), Some("report.pdf".into()));
    }

    #[test]
    fn receive_flow_verifies_and_delivers() {
        let mut host = Transfers::default();
        let mut viewer = Transfers::default();
        let bytes: Vec<u8> = (0..CHUNK_SIZE * 2 + 9).map(|n| n as u8).collect();
        // Host offers; viewer sees an Offer event and nothing to send yet.
        let offer = host.send_file(7, "doc.bin", bytes.clone()).unwrap();
        let (sends, events) = viewer.on_message(offer);
        assert!(sends.is_empty());
        assert_eq!(events, vec![FileEvent::Offer { id: 7, name: "doc.bin".into(), size: bytes.len() as u64 }]);
        // Viewer accepts; feed the Accept to the host to start chunking.
        let accept = viewer.accept(7).unwrap();
        let (initial, _) = host.on_message(accept);
        // Drive the two state machines in order (the real channel is reliable).
        let mut pending: std::collections::VecDeque<FileMsg> = initial.into_iter().collect();
        let mut received_event = None;
        for _ in 0..10_000 {
            let Some(message) = pending.pop_front() else { break };
            let (replies, events) = viewer.on_message(message);
            for event in events {
                if let FileEvent::Received { .. } = event { received_event = Some(event); }
            }
            for reply in replies {
                let (more, _) = host.on_message(reply);
                pending.extend(more);
            }
        }
        match received_event {
            Some(FileEvent::Received { id, name, data }) => {
                assert_eq!(id, 7);
                assert_eq!(name, "doc.bin");
                assert_eq!(digest(&data), digest(&bytes));
            }
            other => panic!("expected a received file, got {other:?}"),
        }
    }

    #[test]
    fn hash_mismatch_is_reported() {
        let mut viewer = Transfers::default();
        let bytes = vec![1u8, 2, 3];
        viewer.on_message(FileMsg::Offer { id: 1, name: "x.bin".into(), size: 3, hash: [0; 32] });
        viewer.accept(1).unwrap();
        viewer.on_message(FileMsg::Chunk { id: 1, offset: 0, data: bytes });
        let (sends, events) = viewer.on_message(FileMsg::Done { id: 1 });
        assert!(matches!(sends.as_slice(), [FileMsg::Error { id: 1, .. }]));
        assert!(matches!(events.as_slice(), [FileEvent::Failed { id: 1, .. }]));
    }

    #[test]
    fn rejecting_an_offer_stops_the_sender() {
        let mut host = Transfers::default();
        let mut viewer = Transfers::default();
        let offer = host.send_file(3, "a.txt", vec![9; 10]).unwrap();
        viewer.on_message(offer);
        let reject = viewer.reject(3).unwrap();
        let (sends, events) = host.on_message(reject);
        assert!(sends.is_empty());
        assert!(matches!(events.as_slice(), [FileEvent::Failed { id: 3, .. }]));
    }

    #[test]
    fn folder_offers_are_refused_at_once() {
        let mut viewer = Transfers::default();
        let (sends, events) = viewer.on_message(FileMsg::FolderOffer { id: 4, name: "docs".into(), files: 2, total: 10 });
        assert!(matches!(sends.as_slice(), [FileMsg::Reject { id: 4, .. }]));
        assert!(events.is_empty());
        let (sends, events) = viewer.on_message(FileMsg::FolderDone { id: 4 });
        assert!(sends.is_empty() && events.is_empty());
    }
}

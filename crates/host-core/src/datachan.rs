//! Demultiplexing of a session's inbound, sealed Data channel.
//!
//! One Data channel carries clipboard updates and any number of file
//! transfers in both directions for the whole session. [`dispatch_data`]
//! opens and decodes every frame and never stops on a message it does not
//! expect: ending the dispatch would drop the reliable reader's Data queue and
//! with it the whole session. [`FileRouter`] hands each transfer its own
//! message stream: replies (Accept/Reject/Ack) to our outgoing transfers, and
//! offers plus their chunks for the peer's.

use crate::filexfer::FileSource;
use async_trait::async_trait;
use protocol::{chat::ChatMsg, clipboard::ClipboardMsg, datamsg::DataMsg, filexfer::FileMsg};
use std::{collections::HashMap, sync::{Arc, Mutex, MutexGuard}};
use tokio::sync::mpsc;

/// Concurrent incoming transfers; further offers are dropped (logged).
pub const MAX_INCOMING_TRANSFERS: usize = 8;
/// Per-transfer queue. The sender's window (32 chunks) plus Done fits.
const TRANSFER_QUEUE: usize = 64;

/// The message stream of one file transfer.
pub struct TransferSource(mpsc::Receiver<FileMsg>);

#[async_trait]
impl FileSource for TransferSource {
    async fn recv(&mut self) -> Option<FileMsg> { self.0.recv().await }
}

/// A new incoming transfer. Its source yields the Offer first, then the
/// sender's chunks, Done or Error.
pub struct IncomingTransfer {
    pub id: u64,
    pub source: TransferSource,
}

/// A reply channel for an outgoing transfer: register the sender half with
/// [`FileRouter::register_outgoing`] (before the Offer goes out) and pass the
/// source to `FileSender::send` as its replies.
pub fn reply_channel() -> (mpsc::Sender<FileMsg>, TransferSource) {
    let (tx, rx) = mpsc::channel(TRANSFER_QUEUE);
    (tx, TransferSource(rx))
}

pub struct FileRouter {
    outgoing: Mutex<HashMap<u64, mpsc::Sender<FileMsg>>>,
    incoming: Mutex<HashMap<u64, mpsc::Sender<FileMsg>>>,
    offers: mpsc::Sender<IncomingTransfer>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl FileRouter {
    /// A router and the stream of incoming transfers it announces.
    pub fn new() -> (Arc<Self>, mpsc::Receiver<IncomingTransfer>) {
        let (offers, offers_rx) = mpsc::channel(MAX_INCOMING_TRANSFERS);
        let router = Self { outgoing: Mutex::default(), incoming: Mutex::default(), offers };
        (Arc::new(router), offers_rx)
    }

    /// Route the peer's replies for outgoing transfer `id` to `replies`.
    pub fn register_outgoing(&self, id: u64, replies: mpsc::Sender<FileMsg>) {
        locked(&self.outgoing).insert(id, replies);
    }

    /// Stop routing replies for a finished outgoing transfer.
    pub fn finish_outgoing(&self, id: u64) {
        locked(&self.outgoing).remove(&id);
    }

    /// Deliver one file message. Never fails: messages for unknown or
    /// finished transfers are dropped.
    pub async fn route(&self, message: FileMsg) {
        match message {
            FileMsg::Accept { id } | FileMsg::Reject { id, .. } | FileMsg::Ack { id, .. } => {
                self.reply(id, message);
            }
            FileMsg::Offer { id, .. } => self.offer(id, message).await,
            FileMsg::Chunk { id, .. } => self.feed(id, message, false).await,
            FileMsg::Done { id } => self.feed(id, message, true).await,
            // Error travels both ways: the receiver failing our transfer, or
            // the sender abandoning theirs. Ids are random per peer, so an id
            // in both maps is practically impossible; replies win.
            FileMsg::Error { id, .. } => {
                if locked(&self.outgoing).contains_key(&id) {
                    self.reply(id, message);
                } else {
                    self.feed(id, message, true).await;
                }
            }
        }
    }

    fn reply(&self, id: u64, message: FileMsg) {
        let Some(replies) = locked(&self.outgoing).get(&id).cloned() else {
            log::debug!("dropping file reply for unknown transfer {id}");
            return;
        };
        // Replies are few and small (one ack per 8 chunks); never block the
        // shared Data dispatch on a sender that stopped reading.
        if replies.try_send(message).is_err() {
            log::debug!("dropping file reply for transfer {id}: sender not reading");
        }
    }

    async fn offer(&self, id: u64, offer: FileMsg) {
        let (tx, rx) = mpsc::channel(TRANSFER_QUEUE);
        {
            let mut incoming = locked(&self.incoming);
            incoming.retain(|_, transfer| !transfer.is_closed());
            if incoming.contains_key(&id) || incoming.len() >= MAX_INCOMING_TRANSFERS {
                log::warn!("dropping file offer {id}: duplicate or too many concurrent transfers");
                return;
            }
            if tx.try_send(offer).is_err() { return; }
            incoming.insert(id, tx);
        }
        if self.offers.send(IncomingTransfer { id, source: TransferSource(rx) }).await.is_err() {
            locked(&self.incoming).remove(&id);
        }
    }

    async fn feed(&self, id: u64, message: FileMsg, last: bool) {
        let Some(transfer) = locked(&self.incoming).get(&id).cloned() else {
            log::debug!("dropping file message for unknown transfer {id}");
            return;
        };
        // Bounded by the sender's window, so awaiting here is brief; a
        // receiver that gave up drops its source and the send fails.
        if transfer.send(message).await.is_err() || last {
            locked(&self.incoming).remove(&id);
        }
    }
}

/// Open, decode and dispatch every inbound Data frame until the channel
/// closes. Undecryptable, undecodable or unexpected frames are logged and
/// skipped; they never end the dispatch (and so never end the session).
pub async fn dispatch_data<O, C, H, B, T>(
    mut incoming: mpsc::Receiver<Vec<u8>>,
    mut open: O,
    router: Arc<FileRouter>,
    mut on_clipboard: C,
    mut on_chat: H,
    mut on_browse: B,
    mut on_tunnel: T,
) where
    O: FnMut(&[u8]) -> Option<Vec<u8>>,
    C: FnMut(ClipboardMsg),
    H: FnMut(ChatMsg),
    B: FnMut(protocol::browse::BrowseMsg),
    T: FnMut(protocol::tunnel::TunnelMsg),
{
    while let Some(sealed) = incoming.recv().await {
        let Some(plaintext) = open(&sealed) else {
            log::debug!("dropping a Data frame that failed to open");
            continue;
        };
        match DataMsg::from_bytes(&plaintext) {
            Some(DataMsg::FileTransfer(message)) => router.route(message).await,
            Some(DataMsg::Clipboard(message)) => on_clipboard(message),
            Some(DataMsg::Chat(message)) => on_chat(message),
            Some(DataMsg::Browse(message)) => on_browse(message),
            Some(DataMsg::Tunnel(message)) => on_tunnel(message),
            None => log::debug!("dropping an unrecognized Data message"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filexfer::{ConsentDecision, FileReceiver, FileSender, FileSink};
    use std::{fs, path::PathBuf, time::{Duration, SystemTime, UNIX_EPOCH}};
    use tokio::time::timeout;

    /// One peer's outbound Data channel (plaintext DataMsg bytes).
    struct Wire(mpsc::Sender<Vec<u8>>);

    #[async_trait]
    impl FileSink for Wire {
        async fn send(&mut self, message: FileMsg) {
            let _ = self.0.send(DataMsg::FileTransfer(message).to_bytes()).await;
        }
    }

    /// An outgoing-transfer sink that registers its reply route on Offer,
    /// as the app's sink does.
    struct RoutedWire {
        wire: Wire,
        router: Arc<FileRouter>,
        replies: Option<mpsc::Sender<FileMsg>>,
    }

    #[async_trait]
    impl FileSink for RoutedWire {
        async fn send(&mut self, message: FileMsg) {
            if let (FileMsg::Offer { id, .. }, Some(replies)) = (&message, self.replies.take()) {
                self.router.register_outgoing(*id, replies);
            }
            self.wire.send(message).await;
        }
    }

    struct Peer {
        router: Arc<FileRouter>,
        offers: mpsc::Receiver<IncomingTransfer>,
        out: mpsc::Sender<Vec<u8>>,
        clipboard: mpsc::UnboundedReceiver<ClipboardMsg>,
    }

    /// Two peers whose Data channels are wired to each other's dispatch.
    fn peers() -> (Peer, Peer, mpsc::Sender<Vec<u8>>) {
        let (a_out, b_in) = mpsc::channel(4);
        let (b_out, a_in) = mpsc::channel(4);
        let make = |incoming, out| {
            let (router, offers) = FileRouter::new();
            let (clip_tx, clipboard) = mpsc::unbounded_channel();
            tokio::spawn(dispatch_data(incoming, |bytes: &[u8]| Some(bytes.to_vec()), Arc::clone(&router), move |message| {
                let _ = clip_tx.send(message);
            }, |_chat| {}, |_browse| {}, |_tunnel| {}));
            Peer { router, offers, out, clipboard }
        };
        let a = make(a_in, a_out.clone());
        let b = make(b_in, b_out);
        // Raw access to a's outbound wire, for injecting junk into b.
        (a, b, a_out)
    }

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("opendesk-datachan-{label}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    async fn send_file(peer: &Peer, path: &std::path::Path) -> Result<(), crate::filexfer::FileError> {
        let (reply_tx, mut replies) = reply_channel();
        let mut sink = RoutedWire { wire: Wire(peer.out.clone()), router: Arc::clone(&peer.router), replies: Some(reply_tx) };
        FileSender::send(path, &mut sink, &mut replies, |_, _| {}).await
    }

    #[tokio::test]
    async fn unexpected_data_messages_do_not_end_the_dispatch() {
        let (_a, mut b, raw_to_b) = peers();
        for junk in [
            Vec::new(),
            vec![0xEE, 1, 2, 3],
            DataMsg::FileTransfer(FileMsg::Chunk { id: 99, offset: 0, data: vec![1] }).to_bytes(),
            DataMsg::FileTransfer(FileMsg::Ack { id: 98, offset: 5 }).to_bytes(),
            DataMsg::FileTransfer(FileMsg::Done { id: 97 }).to_bytes(),
            DataMsg::FileTransfer(FileMsg::Error { id: 96, message: "x".into() }).to_bytes(),
        ] {
            raw_to_b.send(junk).await.unwrap();
        }
        let clip = ClipboardMsg { seq: 1, text: "still alive".into() };
        raw_to_b.send(DataMsg::Clipboard(clip.clone()).to_bytes()).await.unwrap();
        assert_eq!(timeout(Duration::from_secs(1), b.clipboard.recv()).await.unwrap(), Some(clip));
    }

    #[tokio::test]
    async fn two_files_in_one_session_through_the_router() {
        let dir = temp_dir("two-files");
        let downloads = dir.join("downloads");
        let first = dir.join("first.txt");
        let second = dir.join("second.bin");
        fs::write(&first, b"one").unwrap();
        fs::write(&second, (0..300_000u32).map(|n| n as u8).collect::<Vec<_>>()).unwrap();
        let (a, mut b, _raw) = peers();

        // b serves every offer for the whole session, like the app does.
        let b_out = b.out.clone();
        let receive_dir = downloads.clone();
        let receiver = tokio::spawn(async move {
            let mut received = Vec::new();
            while let Some(IncomingTransfer { mut source, .. }) = b.offers.recv().await {
                let mut sink = Wire(b_out.clone());
                let path = FileReceiver::receive(&mut source, &mut sink, &receive_dir, |_| ConsentDecision::Accept, |_, _| {})
                    .await
                    .unwrap();
                received.push(path);
                if received.len() == 2 { break; }
            }
            received
        });
        timeout(Duration::from_secs(5), send_file(&a, &first)).await.unwrap().unwrap();
        timeout(Duration::from_secs(5), send_file(&a, &second)).await.unwrap().unwrap();
        let received = timeout(Duration::from_secs(5), receiver).await.unwrap().unwrap();
        assert_eq!(fs::read(&received[0]).unwrap(), fs::read(&first).unwrap());
        assert_eq!(fs::read(&received[1]).unwrap(), fs::read(&second).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn rejected_offer_through_the_router_stops_the_sender() {
        let dir = temp_dir("reject");
        let path = dir.join("nope.txt");
        fs::write(&path, b"no thanks").unwrap();
        let (a, mut b, _raw) = peers();
        let b_out = b.out.clone();
        let downloads = dir.join("downloads");
        tokio::spawn(async move {
            while let Some(IncomingTransfer { mut source, .. }) = b.offers.recv().await {
                let _ = FileReceiver::receive(&mut source, &mut Wire(b_out.clone()), &downloads, |_| ConsentDecision::Reject, |_, _| {}).await;
            }
        });
        let result = timeout(Duration::from_secs(5), send_file(&a, &path)).await.unwrap();
        assert!(matches!(result, Err(crate::filexfer::FileError::Rejected)), "{result:?}");
        fs::remove_dir_all(dir).unwrap();
    }
}

//! TCP port forwarding over the session's data channel.
//!
//! The viewer binds a local listener (`127.0.0.1:<local>`); each accepted
//! connection becomes a stream (`TunnelMsg::Open { port }`) that the host dials
//! against its own loopback (`127.0.0.1:<port>`, e.g. Ollama). Bytes are relayed
//! as `TunnelMsg::Data`, multiplexed by id, over the already sealed/authenticated
//! reliable data channel. The host only dials loopback and only when its
//! `forwarding` permission is on, so a viewer cannot reach the host's LAN.

use crate::lock_unpoisoned;
use host_core::transport::reliable::ReliableSender;
use protocol::crypto::Sealer;
use protocol::datamsg::DataMsg;
use protocol::tunnel::TunnelMsg;
use protocol::Tag;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::AbortHandle;

/// Bytes read from a socket per chunk before relaying.
const READ_CHUNK: usize = 16 * 1024;

/// The data-channel sender/sealer a live session forwards tunnel frames on.
#[derive(Clone)]
struct Channel {
    sender: ReliableSender,
    sealer: Arc<Mutex<Sealer>>,
}

/// One live stream: a queue feeding the socket's write half, plus the task pair
/// that moves bytes. Dropping it aborts both directions and closes the socket.
struct Stream {
    to_socket: mpsc::UnboundedSender<Vec<u8>>,
    reader: AbortHandle,
    writer: AbortHandle,
}

impl Drop for Stream {
    fn drop(&mut self) {
        self.reader.abort();
        self.writer.abort();
    }
}

/// A local forward the viewer started: its accept loop, aborted on stop.
struct Forward {
    local_port: u16,
    remote_port: u16,
    accept: AbortHandle,
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

#[derive(Default)]
pub(crate) struct TunnelManager {
    channel: Mutex<Option<Channel>>,
    streams: Mutex<HashMap<u64, Stream>>,
    forwards: Mutex<Vec<Forward>>,
    next_id: AtomicU64,
}

impl TunnelManager {
    /// Bind the channel for a new session and clear any leftover state.
    pub(crate) fn activate(&self, sender: ReliableSender, sealer: Arc<Mutex<Sealer>>) {
        *lock_unpoisoned(&self.channel) = Some(Channel { sender, sealer });
        lock_unpoisoned(&self.streams).clear();
        lock_unpoisoned(&self.forwards).clear();
    }

    /// Tear everything down when a session ends.
    pub(crate) fn deactivate(&self) {
        *lock_unpoisoned(&self.channel) = None;
        lock_unpoisoned(&self.streams).clear();
        lock_unpoisoned(&self.forwards).clear();
    }

    fn channel(&self) -> Option<Channel> {
        lock_unpoisoned(&self.channel).clone()
    }
}

/// Seal and send one tunnel frame on the data channel.
async fn send(channel: &Channel, message: TunnelMsg) -> bool {
    let sealed = lock_unpoisoned(&channel.sealer).seal(&[Tag::Data as u8], &DataMsg::Tunnel(message).to_bytes());
    channel.sender.send_frame(Tag::Data, &sealed).await.is_ok()
}

/// Relay a connected socket for stream `id`: pump the socket's reads out as
/// `Data`, and register a queue that writes inbound `Data` to the socket.
fn relay(manager: &Arc<TunnelManager>, channel: Channel, id: u64, socket: TcpStream) {
    let (mut read_half, mut write_half) = socket.into_split();
    let (to_socket, mut from_peer) = mpsc::unbounded_channel::<Vec<u8>>();

    let writer = tokio::spawn(async move {
        while let Some(bytes) = from_peer.recv().await {
            if write_half.write_all(&bytes).await.is_err() {
                break;
            }
        }
        let _ = write_half.shutdown().await;
    });

    let reader_manager = Arc::clone(manager);
    let reader_channel = channel.clone();
    let reader = tokio::spawn(async move {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match read_half.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if !send(&reader_channel, TunnelMsg::Data { id, bytes: buf[..n].to_vec() }).await {
                        break;
                    }
                }
            }
        }
        // Our side hit EOF: drop the stream and tell the peer.
        if lock_unpoisoned(&reader_manager.streams).remove(&id).is_some() {
            let _ = send(&reader_channel, TunnelMsg::Close { id }).await;
        }
    });

    lock_unpoisoned(&manager.streams).insert(
        id,
        Stream { to_socket, reader: reader.abort_handle(), writer: writer.abort_handle() },
    );
}

/// Handle one inbound tunnel frame. `is_host` and `forwarding` gate who may open
/// a stream and whether the host dials at all.
pub(crate) fn handle(manager: &Arc<TunnelManager>, is_host: bool, forwarding: &Arc<AtomicBool>, message: TunnelMsg) {
    let Some(channel) = manager.channel() else { return };
    match message {
        // Only the host opens streams, only to its own loopback, only when
        // forwarding is allowed.
        TunnelMsg::Open { id, port } if is_host => {
            if !forwarding.load(Ordering::Relaxed) {
                spawn_send(&channel, TunnelMsg::OpenErr { id, reason: "forwarding is off".into() });
                return;
            }
            let manager = Arc::clone(manager);
            tokio::spawn(async move {
                match TcpStream::connect(("127.0.0.1", port)).await {
                    Ok(socket) => {
                        relay(&manager, channel.clone(), id, socket);
                        let _ = send(&channel, TunnelMsg::OpenOk { id }).await;
                    }
                    Err(error) => {
                        let _ = send(&channel, TunnelMsg::OpenErr { id, reason: error.to_string() }).await;
                    }
                }
            });
        }
        // The viewer already set up its socket when it sent Open; a successful
        // connect needs nothing, a failure closes the local stream.
        TunnelMsg::OpenOk { .. } if !is_host => {}
        TunnelMsg::OpenErr { id, .. } if !is_host => {
            lock_unpoisoned(&manager.streams).remove(&id);
        }
        TunnelMsg::Data { id, bytes } => {
            if let Some(stream) = lock_unpoisoned(&manager.streams).get(&id) {
                let _ = stream.to_socket.send(bytes);
            }
        }
        TunnelMsg::Close { id } => {
            lock_unpoisoned(&manager.streams).remove(&id);
        }
        // Wrong-role frames are ignored (e.g. a host sending us Open).
        _ => {}
    }
}

/// Viewer: start forwarding `local_port` to the host's loopback `remote_port`.
/// Returns an error string if the local port cannot be bound.
pub(crate) async fn start_forward(manager: Arc<TunnelManager>, local_port: u16, remote_port: u16) -> Result<(), String> {
    let Some(channel) = manager.channel() else { return Err("not connected".into()) };
    if lock_unpoisoned(&manager.forwards).iter().any(|f| f.local_port == local_port) {
        return Err(format!("local port {local_port} is already forwarded"));
    }
    let listener = TcpListener::bind(("127.0.0.1", local_port)).await.map_err(|e| format!("could not bind 127.0.0.1:{local_port}: {e}"))?;

    let accept_manager = Arc::clone(&manager);
    let accept = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else { break };
            let id = accept_manager.next_id.fetch_add(1, Ordering::Relaxed);
            // Start relaying immediately; if the host refuses, it sends OpenErr
            // and the stream is dropped.
            relay(&accept_manager, channel.clone(), id, socket);
            if !send(&channel, TunnelMsg::Open { id, port: remote_port }).await {
                break;
            }
        }
    });

    lock_unpoisoned(&manager.forwards).push(Forward { local_port, remote_port, accept: accept.abort_handle() });
    Ok(())
}

/// Viewer: stop forwarding `local_port`.
pub(crate) fn stop_forward(manager: &Arc<TunnelManager>, local_port: u16) {
    lock_unpoisoned(&manager.forwards).retain(|f| f.local_port != local_port);
}

/// Viewer: the active forwards, as `(local_port, remote_port)` pairs.
pub(crate) fn list_forwards(manager: &Arc<TunnelManager>) -> Vec<(u16, u16)> {
    lock_unpoisoned(&manager.forwards).iter().map(|f| (f.local_port, f.remote_port)).collect()
}

fn spawn_send(channel: &Channel, message: TunnelMsg) {
    let channel = channel.clone();
    tokio::spawn(async move { send(&channel, message).await; });
}

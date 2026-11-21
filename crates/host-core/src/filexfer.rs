//! Streaming file transfer for the reliable, sealed data channel.
//!
//! The caller owns transport and crypto: it seals/opens `FileMsg` bytes and
//! implements [`FileSink`] and [`FileSource`]. The sender hashes the file in a
//! first pass so the receiver has the expected SHA-256 in the initial offer,
//! then reopens it for the bounded-memory chunking pass.

use async_trait::async_trait;
use protocol::filexfer::FileMsg;
use sha2::{Digest, Sha256};
use std::{
    error::Error,
    future::Future,
    fmt::{self, Display, Formatter},
    io,
    path::{Path, PathBuf},
    sync::{atomic::{AtomicBool, AtomicU64, Ordering}, Arc, OnceLock},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const CHUNK_SIZE: usize = 64 * 1024;
/// The receiver acknowledges progress every this many chunks (512 KiB).
pub const ACK_EVERY_CHUNKS: u64 = 8;
/// Most bytes a sender keeps un-acknowledged (2 MiB). Well under the reliable
/// channel's pending Data budget (~3.75 MiB), so a slow receiver applies
/// backpressure instead of overflowing the channel and dropping the session.
pub const SEND_WINDOW_BYTES: u64 = 32 * CHUNK_SIZE as u64;

/// How long a sender waits for its offer to be accepted or rejected (the
/// receiver's user is answering a prompt) and for each acknowledgement.
#[derive(Debug, Clone, Copy)]
pub struct FlowTimeouts {
    pub accept: Duration,
    pub ack: Duration,
}

impl Default for FlowTimeouts {
    fn default() -> Self {
        Self { accept: Duration::from_secs(120), ack: Duration::from_secs(30) }
    }
}

/// A fresh transfer id. Ids start at a random point so the two peers'
/// independent counters practically never collide, and stay below 2^53 so
/// they survive a round trip through JavaScript numbers.
pub fn next_transfer_id() -> u64 {
    static NEXT_ID: OnceLock<AtomicU64> = OnceLock::new();
    NEXT_ID
        .get_or_init(|| {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |duration| duration.as_nanos());
            let mut hasher = Sha256::new();
            hasher.update(nanos.to_le_bytes());
            hasher.update(std::process::id().to_le_bytes());
            let digest = hasher.finalize();
            let seed = u64::from_le_bytes(digest[..8].try_into().expect("8 bytes"));
            // 2^48 starting points leave 2^53 - 2^48 ids before overflow.
            AtomicU64::new((seed & ((1 << 48) - 1)) | 1)
        })
        .fetch_add(1, Ordering::Relaxed)
}

/// What the peer is told when a host-side permission stops a transfer.
pub const REVOKED_MESSAGE: &str = "The other computer turned off file access.";
/// How often a gated transfer re-checks its permission while it waits.
const GATE_POLL: Duration = Duration::from_millis(100);

/// A host-enforced permission checked for the whole life of a transfer, not
/// only when it is offered. Turning the flag off stops an in-flight transfer
/// within one chunk (or [`GATE_POLL`] while it waits), tells the peer with a
/// `FileMsg::Error` (or `Reject`), and discards a partially received file.
#[derive(Clone, Default)]
pub struct TransferGate(Option<Arc<AtomicBool>>);

impl TransferGate {
    /// No permission applies; the transfer is never stopped by a gate.
    pub fn open() -> Self {
        Self(None)
    }

    /// Gate the transfer on `flag`, which may be flipped at any time.
    pub fn new(flag: Arc<AtomicBool>) -> Self {
        Self(Some(flag))
    }

    pub fn allowed(&self) -> bool {
        self.0.as_ref().is_none_or(|flag| flag.load(Ordering::Relaxed))
    }

    fn check(&self) -> Result<(), FileError> {
        if self.allowed() { Ok(()) } else { Err(FileError::Revoked) }
    }

    /// Run `future` unless the permission is turned off first.
    async fn guard<F: Future>(&self, future: F) -> Result<F::Output, FileError> {
        if self.0.is_none() {
            return Ok(future.await);
        }
        let revoked = async {
            while self.allowed() {
                tokio::time::sleep(GATE_POLL).await;
            }
        };
        tokio::select! {
            biased;
            () = revoked => Err(FileError::Revoked),
            output = future => Ok(output),
        }
    }
}

#[async_trait]
pub trait FileSink {
    async fn send(&mut self, msg: FileMsg);
}

#[async_trait]
pub trait FileSource {
    async fn recv(&mut self) -> Option<FileMsg>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsentDecision {
    Accept,
    Reject,
}

#[derive(Debug)]
pub enum FileError {
    Io(io::Error),
    HashMismatch,
    Rejected,
    BadName,
    Protocol(String),
    /// The peer did not answer in time; the transfer was cancelled.
    TimedOut(String),
    /// The peer reported an error and abandoned the transfer.
    Peer(String),
    /// The host turned off the permission this transfer depends on.
    Revoked,
}

impl Display for FileError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "file transfer I/O error: {error}"),
            Self::HashMismatch => f.write_str("file transfer hash mismatch"),
            Self::Rejected => f.write_str("file transfer rejected"),
            Self::BadName => f.write_str("invalid file name"),
            Self::Protocol(message) => write!(f, "file transfer protocol error: {message}"),
            Self::TimedOut(message) => write!(f, "file transfer cancelled: {message}"),
            Self::Peer(message) => write!(f, "file transfer failed on the other side: {message}"),
            Self::Revoked => f.write_str("file transfer stopped: file access was turned off"),
        }
    }
}

impl Error for FileError {}

impl From<io::Error> for FileError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Return a filename safe to append to a trusted downloads directory.
///
/// Besides path traversal, this rejects names that could disguise what the
/// file is or where it lands: invisible/bidi format characters (e.g. U+202E
/// turning "invoice\u{202E}fdp.exe" into "invoiceexe.pdf"), hidden dotfiles,
/// trailing dots/spaces (silently stripped by Windows), ':' (Windows
/// drive-relative paths and NTFS alternate data streams) and Windows reserved
/// device names.
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
        || Path::new(name).is_absolute()
        || name.chars().any(|c| c.is_control() || is_format_char(c))
        || is_windows_reserved_name(name)
    {
        return None;
    }
    Some(name.to_owned())
}

/// Unicode general category Cf (format) characters: invisible marks, bidi
/// controls and tags that can make a displayed name differ from the real one.
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

/// Windows reserved device names (CON, PRN, AUX, NUL, COM1-9, LPT1-9),
/// case-insensitively and with any extension ("nul.txt", "Com1.tar.gz").
fn is_windows_reserved_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    match upper.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" => true,
        _ => {
            let bytes = upper.as_bytes();
            bytes.len() == 4
                && (upper.starts_with("COM") || upper.starts_with("LPT"))
                && (b'1'..=b'9').contains(&bytes[3])
        }
    }
}

/// Mark a received file as downloaded from an untrusted source so the OS
/// applies its usual first-open checks (Gatekeeper on macOS, SmartScreen /
/// Mark-of-the-Web on Windows). Failures are logged, never fatal.
fn quarantine(path: &Path) {
    if let Err(error) = apply_quarantine(path) {
        log::warn!("could not mark received file {} as quarantined: {error}", path.display());
    }
}

#[cfg(target_os = "macos")]
fn apply_quarantine(path: &Path) -> io::Result<()> {
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    // 0081 = downloaded, not yet user-approved; then time, agent, no UUID.
    let value = format!("0081;{now:x};OpenDesk;");
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let c_name = c"com.apple.quarantine";
    // SAFETY: `c_path` and `c_name` are valid NUL-terminated C strings and
    // `value` is a live buffer of `value.len()` bytes; all outlive the call.
    // setxattr does not retain any of the pointers.
    let result = unsafe {
        libc::setxattr(
            c_path.as_ptr(),
            c_name.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            0,
        )
    };
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(windows)]
fn apply_quarantine(path: &Path) -> io::Result<()> {
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    // ZoneId=3 is the Internet zone (Mark-of-the-Web).
    std::fs::write(stream, "[ZoneTransfer]\r\nZoneId=3\r\n")
}

#[cfg(not(any(target_os = "macos", windows)))]
fn apply_quarantine(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub struct FileSender;

impl FileSender {
    /// Offer `path` to the peer and stream it once accepted.
    ///
    /// `replies` yields the receiver's answers (Accept, Reject, Ack, Error);
    /// messages for other transfers are ignored. Only the offer is sent
    /// before the receiver accepts, at most [`SEND_WINDOW_BYTES`] are ever
    /// un-acknowledged, and this returns once the receiver confirms the
    /// verified file.
    pub async fn send<S, R, P>(
        path: P,
        sink: &mut S,
        replies: &mut R,
        progress: impl FnMut(u64, u64),
    ) -> Result<(), FileError>
    where
        S: FileSink + Send,
        R: FileSource + Send,
        P: AsRef<Path>,
    {
        Self::send_with_timeouts(path, sink, replies, progress, FlowTimeouts::default()).await
    }

    pub async fn send_with_timeouts<S, R, P>(
        path: P,
        sink: &mut S,
        replies: &mut R,
        progress: impl FnMut(u64, u64),
        timeouts: FlowTimeouts,
    ) -> Result<(), FileError>
    where
        S: FileSink + Send,
        R: FileSource + Send,
        P: AsRef<Path>,
    {
        Self::send_gated(path, sink, replies, progress, timeouts, &TransferGate::open()).await
    }

    /// Like [`Self::send_with_timeouts`], but stop as soon as `gate` closes:
    /// the receiver gets a `FileMsg::Error` and this returns
    /// [`FileError::Revoked`].
    pub async fn send_gated<S, R, P>(
        path: P,
        sink: &mut S,
        replies: &mut R,
        mut progress: impl FnMut(u64, u64),
        timeouts: FlowTimeouts,
        gate: &TransferGate,
    ) -> Result<(), FileError>
    where
        S: FileSink + Send,
        R: FileSource + Send,
        P: AsRef<Path>,
    {
        let path = path.as_ref();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(sanitize_filename)
            .ok_or(FileError::BadName)?;

        let mut file = tokio::fs::File::open(path).await?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; CHUNK_SIZE];
        let mut size = 0u64;
        loop {
            gate.check()?;
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            size = size
                .checked_add(read as u64)
                .ok_or_else(|| FileError::Protocol("file is too large".into()))?;
        }
        let hash: [u8; 32] = hasher.finalize().into();
        let id = next_transfer_id();
        sink.send(FileMsg::Offer {
            id,
            name,
            size,
            hash,
        })
        .await;

        let answer = match gate.guard(tokio::time::timeout(timeouts.accept, next_reply(replies, id))).await {
            Ok(answer) => answer,
            Err(error) => return Err(revoke(sink, id, error).await),
        };
        match answer {
            Err(_) => return Err(cancel(sink, id, "the receiver did not accept the file in time").await),
            Ok(Reply::Accepted) => {}
            Ok(Reply::Acked(_)) => return Err(FileError::Protocol("acknowledgement before acceptance".into())),
            Ok(Reply::Rejected) => return Err(FileError::Rejected),
            Ok(Reply::Failed(message)) => return Err(FileError::Peer(message)),
            Ok(Reply::Closed) => return Err(FileError::Protocol("session closed before the receiver answered".into())),
        }

        let mut file = tokio::fs::File::open(path).await?;
        let mut offset = 0u64;
        let mut acked = 0u64;
        progress(0, size);
        loop {
            if let Err(error) = gate.check() {
                return Err(revoke(sink, id, error).await);
            }
            let read = file.read(&mut buffer).await?;
            if read == 0 {
                break;
            }
            while offset + read as u64 - acked > SEND_WINDOW_BYTES {
                acked = acked.max(await_ack(sink, replies, id, timeouts.ack, gate).await?);
            }
            sink.send(FileMsg::Chunk {
                id,
                offset,
                data: buffer[..read].to_vec(),
            })
            .await;
            offset += read as u64;
            progress(offset, size);
        }
        sink.send(FileMsg::Done { id }).await;
        // Intermediate acks stay below `size`; the full-size ack comes only
        // after the receiver verified the hash and stored the file. Every
        // byte is already out, so the gate no longer applies.
        while await_ack(sink, replies, id, timeouts.ack, &TransferGate::open()).await? < size {}
        Ok(())
    }
}

enum Reply {
    Accepted,
    Acked(u64),
    Rejected,
    Failed(String),
    Closed,
}

/// The receiver's next answer for transfer `id`; anything else is skipped.
async fn next_reply<R: FileSource + Send>(replies: &mut R, id: u64) -> Reply {
    loop {
        match replies.recv().await {
            None => return Reply::Closed,
            Some(FileMsg::Accept { id: reply_id }) if reply_id == id => return Reply::Accepted,
            Some(FileMsg::Ack { id: reply_id, offset }) if reply_id == id => return Reply::Acked(offset),
            Some(FileMsg::Reject { id: reply_id, .. }) if reply_id == id => return Reply::Rejected,
            Some(FileMsg::Error { id: reply_id, message }) if reply_id == id => return Reply::Failed(message),
            Some(_) => continue,
        }
    }
}

async fn await_ack<S, R>(
    sink: &mut S,
    replies: &mut R,
    id: u64,
    ack_timeout: Duration,
    gate: &TransferGate,
) -> Result<u64, FileError>
where
    S: FileSink + Send,
    R: FileSource + Send,
{
    let answer = match gate.guard(tokio::time::timeout(ack_timeout, next_reply(replies, id))).await {
        Ok(answer) => answer,
        Err(error) => return Err(revoke(sink, id, error).await),
    };
    match answer {
        Err(_) => Err(cancel(sink, id, "the receiver stopped acknowledging data").await),
        Ok(Reply::Acked(offset)) => Ok(offset),
        Ok(Reply::Accepted) => Err(FileError::Protocol("duplicate acceptance".into())),
        Ok(Reply::Rejected) => Err(FileError::Rejected),
        Ok(Reply::Failed(message)) => Err(FileError::Peer(message)),
        Ok(Reply::Closed) => Err(FileError::Protocol("session closed mid-transfer".into())),
    }
}

/// Tell the receiver the transfer is abandoned and build the local error.
async fn cancel<S: FileSink + Send>(sink: &mut S, id: u64, reason: &str) -> FileError {
    sink.send(FileMsg::Error { id, message: reason.to_owned() }).await;
    FileError::TimedOut(reason.to_owned())
}

/// Tell the receiver the host withdrew permission and pass `error` through.
async fn revoke<S: FileSink + Send>(sink: &mut S, id: u64, error: FileError) -> FileError {
    sink.send(FileMsg::Error { id, message: REVOKED_MESSAGE.to_owned() }).await;
    error
}

pub struct FileReceiver;

impl FileReceiver {
    pub async fn receive<S, K>(
        source: &mut S,
        sink: &mut K,
        downloads_dir: &Path,
        consent: impl FnOnce(&FileMsg) -> ConsentDecision,
        progress: impl FnMut(u64, u64),
    ) -> Result<PathBuf, FileError>
    where
        S: FileSource + Send,
        K: FileSink + Send,
    {
        Self::receive_with_async_consent(source, sink, downloads_dir, |offer| async move {
            consent(&offer)
        }, progress)
        .await
    }

    /// Receive a file while obtaining consent without blocking a Tokio worker.
    ///
    /// This is the integration seam for UI consent: the offer callback may
    /// await a command or channel response before any file is created.
    ///
    /// `sink` carries the answers back to the sender: Accept or Reject, an
    /// `Ack` every [`ACK_EVERY_CHUNKS`] chunks, and a final full-size `Ack`
    /// once the file is verified and stored (or an `Error` if that failed).
    pub async fn receive_with_async_consent<S, K, C, ConsentFuture>(
        source: &mut S,
        sink: &mut K,
        downloads_dir: &Path,
        consent: C,
        progress: impl FnMut(u64, u64),
    ) -> Result<PathBuf, FileError>
    where
        S: FileSource + Send,
        K: FileSink + Send,
        C: FnOnce(FileMsg) -> ConsentFuture,
        ConsentFuture: Future<Output = ConsentDecision>,
    {
        Self::receive_gated(source, sink, downloads_dir, consent, progress, &TransferGate::open()).await
    }

    /// Like [`Self::receive_with_async_consent`], but stop as soon as `gate`
    /// closes: the partial file is deleted, the sender gets a
    /// `FileMsg::Error` (or a `Reject` if the gate closed during consent) and
    /// this returns [`FileError::Revoked`].
    pub async fn receive_gated<S, K, C, ConsentFuture>(
        source: &mut S,
        sink: &mut K,
        downloads_dir: &Path,
        consent: C,
        mut progress: impl FnMut(u64, u64),
        gate: &TransferGate,
    ) -> Result<PathBuf, FileError>
    where
        S: FileSource + Send,
        K: FileSink + Send,
        C: FnOnce(FileMsg) -> ConsentFuture,
        ConsentFuture: Future<Output = ConsentDecision>,
    {
        let offer = source
            .recv()
            .await
            .ok_or_else(|| FileError::Protocol("source closed before offer".into()))?;
        let FileMsg::Offer { id, ref name, size, hash: expected_hash } = offer else {
            return Err(FileError::Protocol("expected offer".into()));
        };
        let Some(name) = sanitize_filename(name) else {
            sink.send(FileMsg::Reject { id, reason: "invalid file name".into() }).await;
            return Err(FileError::BadName);
        };
        if consent(offer.clone()).await == ConsentDecision::Reject {
            sink.send(FileMsg::Reject { id, reason: "declined".into() }).await;
            return Err(FileError::Rejected);
        }
        // The permission may have been turned off while the user decided.
        if let Err(error) = gate.check() {
            sink.send(FileMsg::Reject { id, reason: REVOKED_MESSAGE.into() }).await;
            return Err(error);
        }
        sink.send(FileMsg::Accept { id }).await;
        let result = Self::store(source, sink, downloads_dir, &name, id, size, expected_hash, &mut progress, gate).await;
        match &result {
            Ok(_) => sink.send(FileMsg::Ack { id, offset: size }).await,
            Err(FileError::Revoked) => sink.send(FileMsg::Error { id, message: REVOKED_MESSAGE.into() }).await,
            Err(error) => sink.send(FileMsg::Error { id, message: error.to_string() }).await,
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn store<S, K>(
        source: &mut S,
        sink: &mut K,
        downloads_dir: &Path,
        name: &str,
        id: u64,
        size: u64,
        expected_hash: [u8; 32],
        mut progress: impl FnMut(u64, u64),
        gate: &TransferGate,
    ) -> Result<PathBuf, FileError>
    where
        S: FileSource + Send,
        K: FileSink + Send,
    {
        let name = name.to_owned();
        tokio::fs::create_dir_all(downloads_dir).await?;
        let (temp_path, temp_file) = create_temp_path(downloads_dir, &name).await?;
        let result = Self::receive_to_temp(
            source,
            sink,
            temp_file,
            id,
            size,
            expected_hash,
            &mut progress,
            gate,
        )
        .await;
        match result {
            Ok(()) => {
                let final_path = match unique_final_path(downloads_dir, &name).await {
                    Ok(path) => path,
                    Err(error) => {
                        let _ = tokio::fs::remove_file(&temp_path).await;
                        return Err(error);
                    }
                };
                // Defense in depth: the sanitized name must never escape the
                // downloads directory.
                if final_path.parent() != Some(downloads_dir) {
                    let _ = tokio::fs::remove_file(&final_path).await;
                    let _ = tokio::fs::remove_file(&temp_path).await;
                    return Err(FileError::BadName);
                }
                match tokio::fs::rename(&temp_path, &final_path).await {
                    Ok(()) => {
                        quarantine(&final_path);
                        Ok(final_path)
                    }
                    Err(error) => {
                        let _ = tokio::fs::remove_file(&final_path).await;
                        Err(error.into())
                    }
                }
            }
            Err(error) => {
                let _ = tokio::fs::remove_file(&temp_path).await;
                Err(error)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn receive_to_temp<S, K>(
        source: &mut S,
        sink: &mut K,
        mut file: tokio::fs::File,
        id: u64,
        size: u64,
        expected_hash: [u8; 32],
        progress: &mut impl FnMut(u64, u64),
        gate: &TransferGate,
    ) -> Result<(), FileError>
    where
        S: FileSource + Send,
        K: FileSink + Send,
    {
        let mut hasher = Sha256::new();
        let mut expected_offset = 0u64;
        let mut chunks = 0u64;
        progress(0, size);
        loop {
            let message = gate
                .guard(source.recv())
                .await?
                .ok_or_else(|| FileError::Protocol("source closed mid-transfer".into()))?;
            match message {
                FileMsg::Chunk {
                    id: chunk_id,
                    offset,
                    data,
                } if chunk_id == id => {
                    if offset != expected_offset || data.len() > CHUNK_SIZE {
                        return Err(FileError::Protocol("invalid chunk ordering or size".into()));
                    }
                    let new_offset = expected_offset
                        .checked_add(data.len() as u64)
                        .ok_or_else(|| FileError::Protocol("chunk offset overflow".into()))?;
                    if new_offset > size {
                        return Err(FileError::Protocol("received more bytes than offered".into()));
                    }
                    file.write_all(&data).await?;
                    hasher.update(&data);
                    expected_offset = new_offset;
                    progress(expected_offset, size);
                    chunks += 1;
                    // Intermediate acks stay below `size`: the full-size ack
                    // is reserved for the verified, stored file.
                    if chunks.is_multiple_of(ACK_EVERY_CHUNKS) && expected_offset < size {
                        sink.send(FileMsg::Ack { id, offset: expected_offset }).await;
                    }
                }
                FileMsg::Done { id: done_id } if done_id == id => {
                    if expected_offset != size {
                        return Err(FileError::Protocol("transfer ended at the wrong size".into()));
                    }
                    file.flush().await?;
                    let actual_hash: [u8; 32] = hasher.finalize().into();
                    if actual_hash != expected_hash {
                        return Err(FileError::HashMismatch);
                    }
                    return Ok(());
                }
                FileMsg::Error { id: error_id, message } if error_id == id => {
                    return Err(FileError::Peer(message));
                }
                _ => return Err(FileError::Protocol("unexpected file message".into())),
            }
        }
    }
}

async fn create_temp_path(dir: &Path, name: &str) -> Result<(PathBuf, tokio::fs::File), FileError> {
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);
    for _ in 0..100 {
        let suffix = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!(".{name}.{suffix}.part"));
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(file) => {
                return Ok((path, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(FileError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a temporary file",
    )))
}

async fn unique_final_path(dir: &Path, name: &str) -> Result<PathBuf, FileError> {
    let path = Path::new(name);
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or(name);
    let extension = path.extension().and_then(|s| s.to_str());
    for index in 0..u64::MAX {
        let candidate_name = match extension {
            Some(_extension) if index == 0 => name.to_owned(),
            Some(extension) => format!("{stem} ({index}).{extension}"),
            None if index == 0 => name.to_owned(),
            None => format!("{stem} ({index})"),
        };
        let candidate = dir.join(candidate_name);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
            .await
        {
            Ok(file) => {
                // The empty file reserves this name against concurrent
                // receivers. The caller replaces this reservation with the
                // verified temporary file.
                drop(file);
                return Ok(candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error.into()),
        }
    }
    Err(FileError::Protocol("could not choose a destination name".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::{SystemTime, UNIX_EPOCH}};
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    struct Sink(mpsc::Sender<FileMsg>);
    struct Source(mpsc::Receiver<FileMsg>);

    #[async_trait]
    impl FileSink for Sink {
        // A gone peer just drops messages, like a closed session.
        async fn send(&mut self, msg: FileMsg) { let _ = self.0.send(msg).await; }
    }

    /// Both directions of one session: (sender sink, sender replies,
    /// receiver source, receiver sink).
    fn link() -> (Sink, Source, Source, Sink) {
        let (data_tx, data_rx) = mpsc::channel(256);
        let (reply_tx, reply_rx) = mpsc::channel(256);
        (Sink(data_tx), Source(reply_rx), Source(data_rx), Sink(reply_tx))
    }

    fn payload(len: usize) -> Vec<u8> {
        (0..len).map(|n| (n % 251) as u8).collect()
    }

    #[async_trait]
    impl FileSource for Source {
        async fn recv(&mut self) -> Option<FileMsg> { self.0.recv().await }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("opendesk-filexfer-{label}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn sanitizes_filenames() {
        for name in ["../etc/passwd", "/abs/x", "..", "a/b", ""] {
            assert_eq!(sanitize_filename(name), None);
        }
        assert_eq!(sanitize_filename("report.pdf"), Some("report.pdf".into()));
        assert_eq!(sanitize_filename("my file.txt"), Some("my file.txt".into()));
        assert_eq!(sanitize_filename(&"a".repeat(256)), None);
        assert_eq!(sanitize_filename("line\nfeed.txt"), None);
        assert_eq!(sanitize_filename("tab\tfile.txt"), None);
    }

    #[test]
    fn rejects_spoofing_format_characters() {
        for name in [
            "invoice\u{202E}fdp.exe",
            "a\u{200E}.txt",
            "a\u{200F}.txt",
            "a\u{2066}b\u{2069}.txt",
            "\u{FEFF}report.pdf",
            "zero\u{200B}width.txt",
            "soft\u{00AD}hyphen.txt",
            "tag\u{E0041}.txt",
        ] {
            assert_eq!(sanitize_filename(name), None, "{name:?}");
        }
        assert_eq!(sanitize_filename("résumé 履歴書.pdf"), Some("résumé 履歴書.pdf".into()));
    }

    #[test]
    fn rejects_hidden_and_trailing_dot_or_space_names() {
        for name in [".", "..", ".bashrc", ".hidden.txt", "file.", "file ", "file.txt.", "file.txt "] {
            assert_eq!(sanitize_filename(name), None, "{name:?}");
        }
        assert_eq!(sanitize_filename("archive.tar.gz"), Some("archive.tar.gz".into()));
    }

    #[test]
    fn rejects_colons_and_streams() {
        for name in ["C:evil.exe", "a.txt:stream", "a.txt::$DATA", "c:"] {
            assert_eq!(sanitize_filename(name), None, "{name:?}");
        }
    }

    #[test]
    fn rejects_windows_reserved_device_names() {
        for name in [
            "CON", "con", "Prn", "aux.txt", "NUL.tar.gz", "com1", "COM9.log", "lpt1", "Lpt9.txt",
            "nul .txt",
        ] {
            assert_eq!(sanitize_filename(name), None, "{name:?}");
        }
        for name in ["console.txt", "com0.txt", "com10", "lpt", "auxiliary.doc", "nullable.rs", "my con.txt"] {
            assert_eq!(sanitize_filename(name), Some(name.to_owned()), "{name:?}");
        }
    }

    #[test]
    fn quarantine_is_best_effort() {
        let dir = temp_dir("quarantine");
        let file = dir.join("received.txt");
        fs::write(&file, b"x").unwrap();
        // Never panics, even for a missing file.
        quarantine(&file);
        quarantine(&dir.join("missing.txt"));
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("xattr")
                .args(["-p", "com.apple.quarantine"])
                .arg(&file)
                .output()
                .unwrap();
            let value = String::from_utf8_lossy(&output.stdout);
            assert!(value.starts_with("0081;") && value.trim_end().ends_with(";OpenDesk;"), "{value}");
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn final_path_reservation_is_unique() {
        let dir = temp_dir("unique-final");
        let first = unique_final_path(&dir, "report.pdf").await.unwrap();
        let second = unique_final_path(&dir, "report.pdf").await.unwrap();
        assert_eq!(first.file_name().unwrap(), "report.pdf");
        assert_eq!(second.file_name().unwrap(), "report (1).pdf");
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn final_path_reservation_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = temp_dir("symlink-final");
        let target = dir.join("outside.txt");
        fs::write(&target, b"keep me").unwrap();
        symlink(&target, dir.join("report.pdf")).unwrap();

        let reserved = unique_final_path(&dir, "report.pdf").await.unwrap();
        assert_eq!(reserved.file_name().unwrap(), "report (1).pdf");
        assert_eq!(fs::read(&target).unwrap(), b"keep me");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn round_trip_streams_and_verifies() {
        let dir = temp_dir("roundtrip");
        let source_path = dir.join("report.pdf");
        let bytes = payload(CHUNK_SIZE * 2 + 17);
        fs::write(&source_path, &bytes).unwrap();
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let sender = tokio::spawn(async move { FileSender::send(source_path, &mut out, &mut replies, |_, _| {}).await });
        let received = FileReceiver::receive(&mut incoming, &mut answers, &dir, |_| ConsentDecision::Accept, |_, _| {}).await.unwrap();
        sender.await.unwrap().unwrap();
        assert_eq!(fs::read(received).unwrap(), bytes);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn file_larger_than_the_window_streams_with_acks() {
        let dir = temp_dir("windowed");
        let source_path = dir.join("big.bin");
        let bytes = payload(3 * SEND_WINDOW_BYTES as usize + 1234);
        fs::write(&source_path, &bytes).unwrap();
        // Channels far smaller than the file: only acks let the sender go on.
        let (data_tx, data_rx) = mpsc::channel(4);
        let (reply_tx, reply_rx) = mpsc::channel(4);
        let sender = tokio::spawn(async move {
            FileSender::send(source_path, &mut Sink(data_tx), &mut Source(reply_rx), |_, _| {}).await
        });
        let received = FileReceiver::receive(&mut Source(data_rx), &mut Sink(reply_tx), &dir, |_| ConsentDecision::Accept, |_, _| {})
            .await
            .unwrap();
        sender.await.unwrap().unwrap();
        assert_eq!(fs::read(received).unwrap(), bytes);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn sender_waits_for_a_delayed_accept_before_streaming() {
        let dir = temp_dir("delayed-accept");
        let source_path = dir.join("later.bin");
        let bytes = payload(CHUNK_SIZE * 3);
        fs::write(&source_path, &bytes).unwrap();
        let (data_tx, mut data_rx) = mpsc::channel(256);
        let (reply_tx, reply_rx) = mpsc::channel(256);
        let sender = tokio::spawn(async move {
            FileSender::send(source_path, &mut Sink(data_tx), &mut Source(reply_rx), |_, _| {}).await
        });
        let offer = data_rx.recv().await.unwrap();
        assert!(matches!(offer, FileMsg::Offer { .. }), "expected an offer, got {offer:?}");
        // The user takes a while to answer: nothing but the offer may arrive.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(data_rx.try_recv().is_err(), "sender streamed before the offer was accepted");

        let (forward_tx, forward_rx) = mpsc::channel(256);
        forward_tx.send(offer).await.unwrap();
        tokio::spawn(async move {
            while let Some(message) = data_rx.recv().await {
                if forward_tx.send(message).await.is_err() { break; }
            }
        });
        let received = FileReceiver::receive(&mut Source(forward_rx), &mut Sink(reply_tx), &dir, |_| ConsentDecision::Accept, |_, _| {})
            .await
            .unwrap();
        sender.await.unwrap().unwrap();
        assert_eq!(fs::read(received).unwrap(), bytes);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn rejected_offer_stops_the_sender_cleanly() {
        let dir = temp_dir("reject-sender");
        let downloads = dir.join("downloads");
        let source_path = dir.join("nope.bin");
        fs::write(&source_path, payload(CHUNK_SIZE * 4)).unwrap();
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let sender = tokio::spawn(async move { FileSender::send(source_path, &mut out, &mut replies, |_, _| {}).await });
        let result = FileReceiver::receive(&mut incoming, &mut answers, &downloads, |_| ConsentDecision::Reject, |_, _| {}).await;
        assert!(matches!(result, Err(FileError::Rejected)));
        let sent = timeout(Duration::from_secs(2), sender).await.expect("sender must stop on Reject").unwrap();
        assert!(matches!(sent, Err(FileError::Rejected)), "{sent:?}");
        assert!(incoming.0.try_recv().is_err(), "no chunk may follow a rejected offer");
        assert!(!downloads.exists());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn unanswered_offer_times_out_and_cancels() {
        let dir = temp_dir("accept-timeout");
        let source_path = dir.join("ignored.bin");
        fs::write(&source_path, payload(100)).unwrap();
        let (mut out, mut replies, mut incoming, _answers) = link();
        let timeouts = FlowTimeouts { accept: Duration::from_millis(200), ack: Duration::from_secs(5) };
        let result = FileSender::send_with_timeouts(&source_path, &mut out, &mut replies, |_, _| {}, timeouts).await;
        assert!(matches!(result, Err(FileError::TimedOut(_))), "{result:?}");
        assert!(result.unwrap_err().to_string().contains("did not accept"));
        let Some(FileMsg::Offer { id, .. }) = incoming.recv().await else { panic!("expected the offer") };
        assert!(matches!(incoming.recv().await, Some(FileMsg::Error { id: error_id, .. }) if error_id == id));
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn sender_never_exceeds_the_window_without_acks() {
        let dir = temp_dir("window");
        let source_path = dir.join("big.bin");
        fs::write(&source_path, payload(2 * SEND_WINDOW_BYTES as usize)).unwrap();
        let (data_tx, mut data_rx) = mpsc::channel(1024);
        let (reply_tx, reply_rx) = mpsc::channel(16);
        let timeouts = FlowTimeouts { accept: Duration::from_secs(5), ack: Duration::from_millis(300) };
        let sender = tokio::spawn(async move {
            FileSender::send_with_timeouts(&source_path, &mut Sink(data_tx), &mut Source(reply_rx), |_, _| {}, timeouts).await
        });
        let Some(FileMsg::Offer { id, .. }) = data_rx.recv().await else { panic!("expected the offer") };
        reply_tx.send(FileMsg::Accept { id }).await.unwrap();
        let result = timeout(Duration::from_secs(5), sender).await.unwrap().unwrap();
        assert!(matches!(result, Err(FileError::TimedOut(_))), "{result:?}");
        let mut streamed = 0u64;
        let mut cancelled = false;
        while let Ok(message) = data_rx.try_recv() {
            match message {
                FileMsg::Chunk { data, .. } => streamed += data.len() as u64,
                FileMsg::Error { .. } => cancelled = true,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(streamed, SEND_WINDOW_BYTES, "exactly one window may be un-acknowledged");
        assert!(cancelled, "a stalled transfer is cancelled with an Error");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn two_sequential_files_share_one_session() {
        let dir = temp_dir("sequential");
        let first_path = dir.join("one.txt");
        let second_path = dir.join("two.txt");
        fs::write(&first_path, b"first file").unwrap();
        fs::write(&second_path, payload(CHUNK_SIZE * 9 + 5)).unwrap();
        let downloads = dir.join("downloads");
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let receive_dir = downloads.clone();
        let receiver = tokio::spawn(async move {
            let mut received = Vec::new();
            for _ in 0..2 {
                received.push(FileReceiver::receive(&mut incoming, &mut answers, &receive_dir, |_| ConsentDecision::Accept, |_, _| {}).await.unwrap());
            }
            received
        });
        FileSender::send(&first_path, &mut out, &mut replies, |_, _| {}).await.unwrap();
        FileSender::send(&second_path, &mut out, &mut replies, |_, _| {}).await.unwrap();
        let received = timeout(Duration::from_secs(5), receiver).await.unwrap().unwrap();
        assert_eq!(fs::read(&received[0]).unwrap(), fs::read(&first_path).unwrap());
        assert_eq!(fs::read(&received[1]).unwrap(), fs::read(&second_path).unwrap());
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn wrong_hash_discards_temp_file() {
        let dir = temp_dir("tamper");
        let (tx, rx) = mpsc::channel(4);
        let (reply_tx, mut reply_rx) = mpsc::channel(8);
        tx.send(FileMsg::Offer { id: 1, name: "x.bin".into(), size: 3, hash: [0; 32] }).await.unwrap();
        tx.send(FileMsg::Chunk { id: 1, offset: 0, data: vec![1, 2, 4] }).await.unwrap();
        tx.send(FileMsg::Done { id: 1 }).await.unwrap();
        let result = FileReceiver::receive(&mut Source(rx), &mut Sink(reply_tx), &dir, |_| ConsentDecision::Accept, |_, _| {}).await;
        assert!(matches!(result, Err(FileError::HashMismatch)));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert_eq!(reply_rx.recv().await, Some(FileMsg::Accept { id: 1 }));
        assert!(matches!(reply_rx.recv().await, Some(FileMsg::Error { id: 1, .. })), "the sender learns the file was bad");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn rejected_transfer_writes_nothing() {
        let dir = temp_dir("reject");
        let (tx, rx) = mpsc::channel(1);
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        tx.send(FileMsg::Offer { id: 1, name: "x.bin".into(), size: 3, hash: [0; 32] }).await.unwrap();
        let result = FileReceiver::receive(&mut Source(rx), &mut Sink(reply_tx), &dir, |_| ConsentDecision::Reject, |_, _| {}).await;
        assert!(matches!(result, Err(FileError::Rejected)));
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert!(matches!(reply_rx.recv().await, Some(FileMsg::Reject { id: 1, .. })));
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn revoked_gate_stops_an_upload_and_deletes_the_partial_file() {
        let dir = temp_dir("revoke-receive");
        let downloads = dir.join("downloads");
        let source_path = dir.join("upload.bin");
        fs::write(&source_path, payload(3 * SEND_WINDOW_BYTES as usize)).unwrap();
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let sender = tokio::spawn(async move { FileSender::send(source_path, &mut out, &mut replies, |_, _| {}).await });
        let files = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&files);
        // The host turns file access off once two chunks have landed.
        let result = FileReceiver::receive_gated(&mut incoming, &mut answers, &downloads, |_| async { ConsentDecision::Accept }, move |done, _| {
            if done >= 2 * CHUNK_SIZE as u64 { flag.store(false, Ordering::Relaxed); }
        }, &TransferGate::new(files))
        .await;
        assert!(matches!(result, Err(FileError::Revoked)), "{result:?}");
        assert_eq!(fs::read_dir(&downloads).unwrap().count(), 0, "the partial upload must be deleted");
        let sent = timeout(Duration::from_secs(5), sender).await.expect("sender must stop").unwrap();
        assert!(matches!(&sent, Err(FileError::Peer(message)) if message == REVOKED_MESSAGE), "{sent:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn revoked_gate_stops_a_receiver_waiting_for_data() {
        let dir = temp_dir("revoke-idle");
        let (tx, rx) = mpsc::channel(4);
        let (reply_tx, mut reply_rx) = mpsc::channel(8);
        tx.send(FileMsg::Offer { id: 1, name: "x.bin".into(), size: 10, hash: [0; 32] }).await.unwrap();
        tx.send(FileMsg::Chunk { id: 1, offset: 0, data: vec![1, 2, 3] }).await.unwrap();
        let files = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&files);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            flag.store(false, Ordering::Relaxed);
        });
        // The sender stalls after one chunk; the flip alone must end the wait.
        let (mut source, mut sink, gate) = (Source(rx), Sink(reply_tx), TransferGate::new(files));
        let receive = FileReceiver::receive_gated(&mut source, &mut sink, &dir, |_| async { ConsentDecision::Accept }, |_, _| {}, &gate);
        let result = timeout(Duration::from_secs(2), receive).await.expect("the gate must end the wait");
        assert!(matches!(result, Err(FileError::Revoked)), "{result:?}");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert_eq!(reply_rx.recv().await, Some(FileMsg::Accept { id: 1 }));
        assert_eq!(reply_rx.recv().await, Some(FileMsg::Error { id: 1, message: REVOKED_MESSAGE.into() }));
        drop(tx);
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn gate_closed_during_consent_rejects_the_offer() {
        let dir = temp_dir("revoke-consent");
        let (tx, rx) = mpsc::channel(1);
        let (reply_tx, mut reply_rx) = mpsc::channel(1);
        tx.send(FileMsg::Offer { id: 1, name: "x.bin".into(), size: 3, hash: [0; 32] }).await.unwrap();
        let files = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&files);
        let result = FileReceiver::receive_gated(&mut Source(rx), &mut Sink(reply_tx), &dir, move |_| async move {
            flag.store(false, Ordering::Relaxed);
            ConsentDecision::Accept
        }, |_, _| {}, &TransferGate::new(files))
        .await;
        assert!(matches!(result, Err(FileError::Revoked)), "{result:?}");
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        assert!(matches!(reply_rx.recv().await, Some(FileMsg::Reject { id: 1, .. })));
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn revoked_gate_stops_a_download_and_tells_the_receiver() {
        let dir = temp_dir("revoke-send");
        let downloads = dir.join("downloads");
        let source_path = dir.join("pulled.bin");
        fs::write(&source_path, payload(3 * SEND_WINDOW_BYTES as usize)).unwrap();
        let (mut out, mut replies, mut incoming, mut answers) = link();
        let files = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&files);
        let gate = TransferGate::new(files);
        let sender = tokio::spawn(async move {
            // The host turns file access off once two chunks are out.
            FileSender::send_gated(source_path, &mut out, &mut replies, move |done, _| {
                if done >= 2 * CHUNK_SIZE as u64 { flag.store(false, Ordering::Relaxed); }
            }, FlowTimeouts::default(), &gate)
            .await
        });
        let received = FileReceiver::receive(&mut incoming, &mut answers, &downloads, |_| ConsentDecision::Accept, |_, _| {}).await;
        assert!(matches!(&received, Err(FileError::Peer(message)) if message == REVOKED_MESSAGE), "{received:?}");
        assert_eq!(fs::read_dir(&downloads).unwrap().count(), 0, "the partial download must be deleted");
        let sent = timeout(Duration::from_secs(5), sender).await.expect("sender must stop").unwrap();
        assert!(matches!(sent, Err(FileError::Revoked)), "{sent:?}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transfer_ids_are_unique_and_javascript_safe() {
        let first = next_transfer_id();
        let second = next_transfer_id();
        assert_ne!(first, second);
        assert!(first < (1 << 53) && second < (1 << 53));
    }
}

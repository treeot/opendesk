//! Direct UDP transport over a nominated ICE connection.
//!
//! Sealed frames are split into datagrams (see [`super::recovery`]) and
//! handed to a writer task that paces them at 1.5x the target bitrate, with
//! audio/input and retransmissions ahead of video. A reader task reassembles
//! incoming messages and, when the session has feedback keys, exchanges
//! sealed feedback with the peer (see [`super::feedback`]): the media sender
//! sends reports, the receiver answers with loss counters and NACKs missing
//! fragments, the sender resends them from a ring of recent messages and
//! adapts the bitrate the encoder follows.

use super::feedback::{self, Feedback};
use super::pacer::Pacer;
use super::link::{LossMeter, RttMeter};
use super::rate::{AimdEstimator, PathSample, RateControl};
use super::recovery::{fragment_message, ReceiveTracker, RecvStats, RetransmitRing, FRAG_HEADER_LEN, MAX_MESSAGE_LEN};
use super::{Transport, TransportError, TransportReceiver, TransportSender};
use protocol::{frame, Opener, Sealer, Tag};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use webrtc_util::conn::Conn;

/// Video waiting to be sent beyond which `send` waits (the host normally
/// waits for [`TransportSender::ready_for_video`] instead).
const MAX_QUEUED_VIDEO_BYTES: usize = 2 * 1024 * 1024;
/// Reassembled messages waiting for the consumer; more are dropped.
const RECEIVE_QUEUE: usize = 512;
/// Feedback timer resolution (NACK timers, reports).
const TICK: Duration = Duration::from_millis(5);
/// How often the media sender sends a sender report.
const REPORT_INTERVAL: Duration = Duration::from_millis(100);
/// Retransmissions one NACK packet can queue.
const MAX_RESENDS_PER_NACK: usize = 512;
/// A peer with feedback sends a report every 100 ms (host) or answers each
/// one (viewer); this much silence means the path is gone.
const FEEDBACK_SILENCE: Duration = Duration::from_secs(8);
/// A host without feedback sends video even on a static screen (repeated
/// frames, a keyframe every 2 s), so this much media silence also means the
/// path is gone.
const MEDIA_SILENCE: Duration = Duration::from_secs(15);

/// Keys for the sealed feedback channel of a direct session.
#[derive(Clone)]
pub struct FeedbackKeys {
    send: [u8; 32],
    recv: [u8; 32],
    /// The host sends media, so it sends reports and adapts the bitrate;
    /// the viewer answers them.
    media_sender: bool,
}

impl FeedbackKeys {
    /// Feedback keys for this side of the session (`shared` is the PAKE
    /// secret).
    pub fn derive(shared: &[u8], is_host: bool) -> Self {
        let (send, recv) = protocol::kex::derive_transport_keys(shared, is_host);
        Self { send, recv, media_sender: is_host }
    }
}

/// Carries sealed protocol frames over application-level UDP fragmentation.
///
/// With feedback keys the receiver closes once the peer goes silent (see
/// [`PathWatch`]), so the session ends and the viewer reconnects instead of
/// showing a frozen picture. Keepalives go over the relay socket, so nothing
/// else notices a dead UDP path.
pub struct DirectTransport {
    pub(crate) conn: Arc<dyn Conn + Send + Sync>,
    feedback: Option<FeedbackKeys>,
    silence: SilenceLimits,
}

impl DirectTransport {
    /// A transport without feedback: paced sending and beta.4-compatible
    /// receiving, no retransmission or bitrate adaptation.
    pub(crate) fn new(conn: Arc<dyn Conn + Send + Sync>) -> Self {
        Self { conn, feedback: None, silence: SilenceLimits::default() }
    }

    pub(crate) fn with_feedback(conn: Arc<dyn Conn + Send + Sync>, keys: FeedbackKeys) -> Self {
        Self { conn, feedback: Some(keys), silence: SilenceLimits::default() }
    }

    #[cfg(test)]
    fn with_silence_limits(mut self, silence: SilenceLimits) -> Self {
        self.silence = silence;
        self
    }
}

/// How long a direct path may stay silent before it counts as dead.
#[derive(Debug, Clone, Copy)]
struct SilenceLimits {
    feedback: Duration,
    media: Duration,
}

impl Default for SilenceLimits {
    fn default() -> Self { Self { feedback: FEEDBACK_SILENCE, media: MEDIA_SILENCE } }
}

/// Decides when a silent direct path is dead. Only peers that talk all the
/// time are judged: one that sent feedback, or (on the viewer) a host
/// without feedback that sent media. A viewer without feedback only sends
/// input, so the host never judges it; a dead path then ends on the
/// viewer's side.
struct PathWatch {
    limits: SilenceLimits,
    /// This side receives media (the viewer).
    media_receiver: bool,
    last_heard: Instant,
    feedback: bool,
    media: bool,
}

impl PathWatch {
    fn new(now: Instant, limits: SilenceLimits, media_receiver: bool) -> Self {
        Self { limits, media_receiver, last_heard: now, feedback: false, media: false }
    }

    fn heard(&mut self, now: Instant, feedback: bool) {
        self.last_heard = now;
        if feedback { self.feedback = true } else { self.media = true }
    }

    fn dead(&self, now: Instant) -> bool {
        let limit = if self.feedback {
            self.limits.feedback
        } else if self.media_receiver && self.media {
            self.limits.media
        } else {
            return false;
        };
        now.saturating_duration_since(self.last_heard) >= limit
    }
}

pub struct DirectSender {
    shared: Arc<Shared>,
    _writer: AbortOnDrop,
}

pub struct DirectReceiver {
    messages: mpsc::Receiver<Vec<u8>>,
    _reader: AbortOnDrop,
}

#[async_trait::async_trait]
impl Transport for DirectTransport {
    fn into_split(self) -> (Box<dyn TransportSender>, Box<dyn TransportReceiver>) {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::new(Instant::now())),
            work: Notify::new(),
            drained: Notify::new(),
            rate: Arc::new(RateControl::default()),
            rtt: Arc::new(RttMeter::default()),
            loss: Arc::new(LossMeter::default()),
        });
        let (deliver, messages) = mpsc::channel(RECEIVE_QUEUE);
        let feedback = self.feedback.map(|keys| FeedbackState {
            sealer: Sealer::new(keys.send),
            opener: Opener::new(keys.recv),
            media_sender: keys.media_sender,
            start: Instant::now(),
            last_report: None,
            estimator: AimdEstimator::default(),
            last_counts: RecvStats::default(),
            last_received: RecvStats::default(),
        });
        let watch = feedback.as_ref().map(|state| PathWatch::new(Instant::now(), self.silence, !state.media_sender));
        let writer = tokio::spawn(run_writer(Arc::clone(&self.conn), Arc::clone(&shared)));
        let reader = tokio::spawn(run_reader(self.conn, Arc::clone(&shared), deliver, feedback, watch));
        (
            Box::new(DirectSender { shared, _writer: AbortOnDrop(writer) }),
            Box::new(DirectReceiver { messages, _reader: AbortOnDrop(reader) }),
        )
    }
}

#[async_trait::async_trait]
impl TransportSender for DirectSender {
    async fn send(&mut self, tag: Tag, sealed: &[u8]) -> Result<(), TransportError> {
        let message = frame::wrap(tag, sealed);
        if message.len() > MAX_MESSAGE_LEN {
            return Err(TransportError::Io("message has too many fragments".into()));
        }
        if tag == Tag::Video {
            self.shared.wait_until(|queue| queue.video_bytes < MAX_QUEUED_VIDEO_BYTES).await;
        }
        {
            let mut queue = self.shared.lock();
            if let Some(error) = &queue.error {
                return Err(error.clone());
            }
            if tag == Tag::Video {
                queue.video_bytes += message.len();
                queue.pending_video.push_back(message);
            } else {
                queue.pending_urgent.push_back(message);
            }
        }
        self.shared.work.notify_one();
        Ok(())
    }

    async fn ready_for_video(&mut self) {
        self.shared.wait_until(|queue| queue.video_bytes == 0).await;
    }

    fn rate_control(&self) -> Arc<RateControl> { Arc::clone(&self.shared.rate) }

    fn rtt_meter(&self) -> Arc<RttMeter> { Arc::clone(&self.shared.rtt) }

    fn loss_meter(&self) -> Arc<LossMeter> { Arc::clone(&self.shared.loss) }
}

#[async_trait::async_trait]
impl TransportReceiver for DirectReceiver {
    async fn recv(&mut self) -> Option<(Tag, Vec<u8>)> {
        loop {
            let message = self.messages.recv().await?;
            if let Some((tag, body)) = frame::unwrap(&message) {
                return Some((tag, body.to_vec()));
            }
        }
    }
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) { self.0.abort(); }
}

/// State shared by the sender handle, the writer and the reader.
struct Shared {
    queue: Mutex<Queue>,
    /// Wakes the writer: something was queued.
    work: Notify,
    /// Wakes senders: video drained, or the writer failed.
    drained: Notify,
    rate: Arc<RateControl>,
    /// Smoothed RTT: the host measures it from receiver reports, the viewer
    /// takes it from the host's sender reports.
    rtt: Arc<RttMeter>,
    /// Loss on the media we receive, measured once per sender report.
    loss: Arc<LossMeter>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Waits until `ready` holds or the writer failed. Cancel safe.
    async fn wait_until(&self, ready: impl Fn(&Queue) -> bool) {
        loop {
            let drained = self.drained.notified();
            tokio::pin!(drained);
            drained.as_mut().enable();
            {
                let queue = self.lock();
                if queue.error.is_some() || ready(&queue) {
                    return;
                }
            }
            drained.await;
        }
    }
}

struct Datagram {
    bytes: Arc<[u8]>,
    /// Message id and fragment index of a first transmission.
    origin: Option<(u32, u16)>,
}

/// Outgoing datagrams. Message ids are assigned when a message starts
/// going out, so ids on the wire follow send order even though urgent
/// messages overtake queued video.
struct Queue {
    next_msg_id: u32,
    /// Retransmissions and fragments of started audio/input/data messages.
    urgent: VecDeque<Datagram>,
    pending_urgent: VecDeque<Vec<u8>>,
    video: VecDeque<Datagram>,
    pending_video: VecDeque<Vec<u8>>,
    /// Unsent video payload bytes (queued and started messages).
    video_bytes: usize,
    ring: RetransmitRing,
    pacer: Pacer,
    error: Option<TransportError>,
}

impl Queue {
    fn new(now: Instant) -> Self {
        Self {
            next_msg_id: 0,
            urgent: VecDeque::new(),
            pending_urgent: VecDeque::new(),
            video: VecDeque::new(),
            pending_video: VecDeque::new(),
            video_bytes: 0,
            ring: RetransmitRing::default(),
            pacer: Pacer::new(now),
            error: None,
        }
    }

    /// Fragments a message under the next id and records it for resending.
    fn start(&mut self, message: &[u8]) -> VecDeque<Datagram> {
        let msg_id = self.next_msg_id;
        self.next_msg_id = msg_id.wrapping_add(1);
        // `send` rejected messages too large to fragment.
        let fragments = fragment_message(message, msg_id).unwrap_or_default();
        self.ring.insert(msg_id, fragments.clone());
        fragments
            .into_iter()
            .enumerate()
            .map(|(index, bytes)| Datagram { bytes, origin: Some((msg_id, index as u16)) })
            .collect()
    }

    /// The length of the datagram that goes next, starting a message if
    /// needed.
    fn peek_len(&mut self) -> Option<usize> {
        if self.urgent.is_empty() {
            if let Some(message) = self.pending_urgent.pop_front() {
                self.urgent = self.start(&message);
            }
        }
        if let Some(datagram) = self.urgent.front() {
            return Some(datagram.bytes.len());
        }
        if self.video.is_empty() {
            if let Some(message) = self.pending_video.pop_front() {
                self.video = self.start(&message);
            }
        }
        self.video.front().map(|datagram| datagram.bytes.len())
    }

    /// Takes the datagram [`Self::peek_len`] described. Returns whether
    /// that drained the video queue.
    fn pop(&mut self) -> Option<(Arc<[u8]>, bool)> {
        let (datagram, video) = match self.urgent.pop_front() {
            Some(datagram) => (datagram, false),
            None => (self.video.pop_front()?, true),
        };
        if let Some((msg_id, index)) = datagram.origin {
            self.ring.mark_sent(msg_id, index);
        }
        let mut drained = false;
        if video {
            self.video_bytes = self.video_bytes.saturating_sub(datagram.bytes.len() - FRAG_HEADER_LEN);
            if self.video.is_empty() && self.pending_video.is_empty() {
                self.video_bytes = 0;
                drained = true;
            }
        }
        Some((datagram.bytes, drained))
    }
}

enum Step {
    Send(Arc<[u8]>, bool),
    Wait(Duration),
    Idle,
}

/// Sends queued datagrams, paced at the rate the bitrate target implies.
async fn run_writer(conn: Arc<dyn Conn + Send + Sync>, shared: Arc<Shared>) {
    loop {
        let step = {
            let mut queue = shared.lock();
            let now = Instant::now();
            queue.pacer.set_rate(now, shared.rate.pacing_rate());
            match queue.peek_len() {
                None => Step::Idle,
                Some(len) => match queue.pacer.try_take(now, len) {
                    Ok(()) => queue.pop().map_or(Step::Idle, |(bytes, drained)| Step::Send(bytes, drained)),
                    Err(wait) => Step::Wait(wait),
                },
            }
        };
        match step {
            Step::Idle => shared.work.notified().await,
            Step::Wait(wait) => tokio::time::sleep(wait).await,
            Step::Send(bytes, drained) => {
                if drained {
                    shared.drained.notify_waiters();
                }
                let error = match conn.send(&bytes).await {
                    Ok(sent) if sent == bytes.len() => continue,
                    Ok(sent) => format!("UDP send was incomplete: sent {sent} of {} bytes", bytes.len()),
                    Err(error) => error.to_string(),
                };
                shared.lock().error = Some(TransportError::Io(error));
                shared.drained.notify_waiters();
                return;
            }
        }
    }
}

/// Feedback-channel state, owned by the reader task.
struct FeedbackState {
    sealer: Sealer,
    opener: Opener,
    media_sender: bool,
    start: Instant,
    last_report: Option<Instant>,
    estimator: AimdEstimator,
    last_counts: RecvStats,
    /// Our own receive counters at the previous sender report.
    last_received: RecvStats,
}

impl FeedbackState {
    fn micros(&self, now: Instant) -> u64 { now.duration_since(self.start).as_micros() as u64 }

    async fn send(&mut self, conn: &(dyn Conn + Send + Sync), feedback: &Feedback) {
        if let Err(error) = conn.send(&feedback.seal(&mut self.sealer)).await {
            log::debug!("direct transport: feedback not sent: {error}");
        }
    }

    /// Handles a feedback datagram; returns whether it was genuine.
    async fn on_datagram(&mut self, now: Instant, datagram: &[u8], tracker: &mut ReceiveTracker, conn: &(dyn Conn + Send + Sync), shared: &Shared) -> bool {
        let Some(feedback) = Feedback::open(&mut self.opener, datagram) else { return false };
        match feedback {
            Feedback::SenderReport { send_time_us, rtt_us } => {
                if !tracker.is_ordered() {
                    log::info!("direct transport: peer retransmits; ordered delivery with NACKs enabled");
                    tracker.enable_ordered();
                }
                if rtt_us > 0 {
                    let rtt = Duration::from_micros(u64::from(rtt_us));
                    tracker.set_rtt(rtt);
                    shared.rtt.record(rtt, now);
                }
                let stats = tracker.stats();
                shared.loss.record(loss_fraction(self.last_received, stats), now);
                self.last_received = stats;
                let report = Feedback::ReceiverReport {
                    echo_send_time_us: send_time_us,
                    hold_us: 0,
                    received: stats.received,
                    lost: stats.lost,
                    recovered: stats.recovered,
                    abandoned: stats.abandoned,
                };
                self.send(conn, &report).await;
            }
            Feedback::ReceiverReport { echo_send_time_us, hold_us, received, lost, recovered, abandoned } => {
                let rtt = self
                    .micros(now)
                    .checked_sub(echo_send_time_us)
                    .and_then(|elapsed| elapsed.checked_sub(u64::from(hold_us)))
                    .map(Duration::from_micros)
                    .filter(|rtt| *rtt < Duration::from_secs(10));
                let counts = RecvStats { received, lost, recovered, abandoned };
                let loss = loss_fraction(self.last_counts, counts);
                self.last_counts = counts;
                let ceiling = shared.rate.ceiling();
                if ceiling > 0 {
                    let before = shared.rate.target();
                    let target = self.estimator.on_report(now, ceiling, PathSample { rtt, loss });
                    shared.rate.set_target(target);
                    if target.abs_diff(before) > before / 10 {
                        log::debug!("direct transport: target bitrate {target} bps (loss {:.1}%, rtt {rtt:?})", loss * 100.0);
                    }
                }
                if let Some(srtt) = self.estimator.srtt() {
                    shared.rtt.record(srtt, now);
                }
            }
            Feedback::Nack(ranges) => {
                let mut resend = Vec::new();
                {
                    let mut queue = shared.lock();
                    for range in ranges {
                        queue.ring.resend(range, &mut resend, MAX_RESENDS_PER_NACK);
                    }
                    queue.urgent.extend(resend.iter().map(|bytes| Datagram { bytes: Arc::clone(bytes), origin: None }));
                }
                if !resend.is_empty() {
                    shared.work.notify_one();
                }
            }
        }
        true
    }

    async fn on_tick(&mut self, now: Instant, tracker: &mut ReceiveTracker, conn: &(dyn Conn + Send + Sync), shared: &Shared) {
        if self.media_sender && self.last_report.is_none_or(|at| now.duration_since(at) >= REPORT_INTERVAL) {
            self.last_report = Some(now);
            let rtt_us = self.estimator.srtt().map_or(0, |rtt| u32::try_from(rtt.as_micros()).unwrap_or(u32::MAX));
            let report = Feedback::SenderReport { send_time_us: self.micros(now), rtt_us };
            self.send(conn, &report).await;
            let ceiling = shared.rate.ceiling();
            if ceiling > 0 {
                if let Some(target) = self.estimator.on_tick(now, ceiling) {
                    log::debug!("direct transport: no receiver reports; target bitrate {target} bps");
                    shared.rate.set_target(target);
                }
            }
        }
        let ranges = tracker.poll(now);
        if !ranges.is_empty() {
            self.send(conn, &Feedback::Nack(ranges)).await;
        }
    }
}

/// Fraction of first transmissions lost between two receiver reports.
fn loss_fraction(before: RecvStats, after: RecvStats) -> f64 {
    let received = after.received.wrapping_sub(before.received);
    let recovered = after.recovered.wrapping_sub(before.recovered);
    let lost = after.lost.wrapping_sub(before.lost);
    let originals = received.saturating_sub(recovered);
    let total = f64::from(originals) + f64::from(lost);
    if total == 0.0 { 0.0 } else { f64::from(lost) / total }
}

/// Receives datagrams: media fragments go to reassembly, feedback to
/// [`FeedbackState`]; complete messages go to the receiver handle. Ends,
/// closing the receiver, when `watch` finds the path dead.
async fn run_reader(
    conn: Arc<dyn Conn + Send + Sync>,
    shared: Arc<Shared>,
    deliver: mpsc::Sender<Vec<u8>>,
    mut feedback: Option<FeedbackState>,
    mut watch: Option<PathWatch>,
) {
    let mut tracker = ReceiveTracker::new();
    let mut buf = vec![0u8; 65_535];
    let mut tick = tokio::time::interval(TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            received = conn.recv(&mut buf) => {
                let Ok(len) = received else { return };
                let now = Instant::now();
                let datagram = &buf[..len];
                if feedback::is_feedback(datagram) {
                    if let Some(state) = feedback.as_mut() {
                        if state.on_datagram(now, datagram, &mut tracker, conn.as_ref(), &shared).await {
                            if let Some(watch) = watch.as_mut() { watch.heard(now, true); }
                        }
                    }
                } else {
                    tracker.push(now, datagram);
                    if let Some(watch) = watch.as_mut() { watch.heard(now, false); }
                }
            }
            _ = tick.tick(), if feedback.is_some() => {
                let now = Instant::now();
                if watch.as_ref().is_some_and(|watch| watch.dead(now)) {
                    log::warn!("direct transport: nothing from the peer for too long; closing the path");
                    return;
                }
                if let Some(state) = feedback.as_mut() {
                    state.on_tick(now, &mut tracker, conn.as_ref(), &shared).await;
                }
            }
        }
        while let Some(message) = tracker.pop_ready() {
            // A consumer this far behind loses messages, as a full socket
            // buffer would; video then recovers on a keyframe.
            if let Err(mpsc::error::TrySendError::Closed(_)) = deliver.try_send(message) {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::timeout;
    use webrtc_util::conn::conn_pipe::pipe;

    #[tokio::test]
    async fn loopback_round_trip_preserves_tag_and_sealed_bytes() {
        let (left, right) = pipe();

        let (mut sender, _) = DirectTransport::new(Arc::new(left)).into_split();
        let (_, mut receiver) = DirectTransport::new(Arc::new(right)).into_split();
        let sealed = b"already sealed bytes";

        sender.send(Tag::Video, sealed).await.unwrap();
        assert_eq!(receiver.recv().await, Some((Tag::Video, sealed.to_vec())));
    }

    #[tokio::test]
    async fn loopback_round_trip_reassembles_large_message() {
        let (left, right) = pipe();
        let (mut sender, _) = DirectTransport::new(Arc::new(left)).into_split();
        let (_, mut receiver) = DirectTransport::new(Arc::new(right)).into_split();
        let sealed: Vec<u8> = (0..50_000).map(|index| (index % 251) as u8).collect();

        let (send_result, received) = tokio::join!(sender.send(Tag::Video, &sealed), receiver.recv());
        send_result.unwrap();
        assert_eq!(received, Some((Tag::Video, sealed)));
    }

    #[test]
    fn loss_fraction_counts_first_transmissions_only() {
        let before = RecvStats { received: 100, lost: 0, recovered: 0, abandoned: 0 };
        // 90 originals + 10 retransmissions arrived, 10 were found missing.
        let after = RecvStats { received: 200, lost: 10, recovered: 10, abandoned: 0 };
        assert!((loss_fraction(before, after) - 0.1).abs() < 1e-9);
        assert_eq!(loss_fraction(after, after), 0.0);
        let wrapped = RecvStats { received: u32::MAX, ..RecvStats::default() };
        let next = RecvStats { received: 9, lost: 0, ..RecvStats::default() };
        assert_eq!(loss_fraction(wrapped, next), 0.0);
    }

    #[tokio::test]
    async fn pacing_spreads_a_frame_over_time() {
        let (left, right) = pipe();
        let (mut sender, _) = DirectTransport::new(Arc::new(left)).into_split();
        let (_, mut receiver) = DirectTransport::new(Arc::new(right)).into_split();
        // 1 Mbps target -> paced at the 3 Mbps floor: 60 kB takes ~150 ms.
        sender.rate_control().set_ceiling(1_000_000);
        let frame = vec![7u8; 60_000];
        let started = Instant::now();
        sender.send(Tag::Video, &frame).await.unwrap();
        assert!(started.elapsed() < Duration::from_millis(20), "send only queues");
        assert_eq!(receiver.recv().await, Some((Tag::Video, frame)));
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(120), "not paced: {took:?}");
        assert!(took < Duration::from_millis(500), "paced too slowly: {took:?}");
    }

    #[tokio::test]
    async fn ready_for_video_waits_for_the_previous_frame_and_audio_overtakes_it() {
        let (left, right) = pipe();
        let (mut sender, _) = DirectTransport::new(Arc::new(left)).into_split();
        let (_, mut receiver) = DirectTransport::new(Arc::new(right)).into_split();
        sender.rate_control().set_ceiling(1_000_000);
        sender.send(Tag::Video, &[1u8; 30_000]).await.unwrap();
        sender.send(Tag::Audio, b"audio").await.unwrap();
        let ready = tokio::time::timeout(Duration::from_millis(20), sender.ready_for_video()).await;
        assert!(ready.is_err(), "video is still being paced out");
        assert_eq!(receiver.recv().await, Some((Tag::Audio, b"audio".to_vec())));
        assert_eq!(receiver.recv().await.map(|(tag, _)| tag), Some(Tag::Video));
        tokio::time::timeout(Duration::from_secs(1), sender.ready_for_video()).await.unwrap();
    }

    /// A UDP socket that silently drops a share of the datagrams it sends,
    /// or all of them once cut.
    struct LossyConn {
        socket: tokio::net::UdpSocket,
        drop_per_mille: u64,
        rng: std::sync::atomic::AtomicU64,
        cut: std::sync::atomic::AtomicBool,
    }

    impl LossyConn {
        fn drops(&self) -> bool {
            use std::sync::atomic::Ordering;
            if self.cut.load(Ordering::Relaxed) {
                return true;
            }
            // xorshift64: deterministic per seed.
            let mut x = self.rng.load(Ordering::Relaxed);
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.rng.store(x, Ordering::Relaxed);
            x % 1_000 < self.drop_per_mille
        }
    }

    #[async_trait::async_trait]
    impl Conn for LossyConn {
        async fn connect(&self, addr: std::net::SocketAddr) -> webrtc_util::Result<()> { Conn::connect(&self.socket, addr).await }
        async fn recv(&self, buf: &mut [u8]) -> webrtc_util::Result<usize> { Conn::recv(&self.socket, buf).await }
        async fn recv_from(&self, buf: &mut [u8]) -> webrtc_util::Result<(usize, std::net::SocketAddr)> { Conn::recv_from(&self.socket, buf).await }
        async fn send(&self, buf: &[u8]) -> webrtc_util::Result<usize> {
            if self.drops() { Ok(buf.len()) } else { Conn::send(&self.socket, buf).await }
        }
        async fn send_to(&self, buf: &[u8], target: std::net::SocketAddr) -> webrtc_util::Result<usize> { Conn::send_to(&self.socket, buf, target).await }
        fn local_addr(&self) -> webrtc_util::Result<std::net::SocketAddr> { Conn::local_addr(&self.socket) }
        fn remote_addr(&self) -> Option<std::net::SocketAddr> { Conn::remote_addr(&self.socket) }
        async fn close(&self) -> webrtc_util::Result<()> { Ok(()) }
        fn as_any(&self) -> &(dyn std::any::Any + Send + Sync) { self }
    }

    /// Two connected loopback UDP sockets that each drop `drop_per_mille`
    /// of what they send.
    async fn lossy_pair(drop_per_mille: u64) -> (Arc<LossyConn>, Arc<LossyConn>) {
        let host = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let viewer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        host.connect(viewer.local_addr().unwrap()).await.unwrap();
        viewer.connect(host.local_addr().unwrap()).await.unwrap();
        let wrap = |socket, seed| Arc::new(LossyConn { socket, drop_per_mille, rng: std::sync::atomic::AtomicU64::new(seed), cut: Default::default() });
        (wrap(host, 0x9E37_79B9_7F4A_7C15), wrap(viewer, 0xD1B5_4A32_D192_ED03))
    }

    fn transport(conn: Arc<LossyConn>, keys: Option<FeedbackKeys>) -> DirectTransport {
        match keys {
            Some(keys) => DirectTransport::with_feedback(conn, keys),
            None => DirectTransport::new(conn),
        }
    }

    struct LinkRun {
        /// Sequence numbers of the frames that arrived, in arrival order.
        received: Vec<u32>,
        target: u32,
        /// The loss the viewer last measured on what it received.
        viewer_loss: Option<f64>,
    }

    const CEILING: u32 = 8_000_000;

    /// Streams `frames` 20 kB video frames at 60 fps from host to viewer.
    async fn run_link(drop_per_mille: u64, host_feedback: bool, viewer_feedback: bool, frames: u32) -> LinkRun {
        let shared = [11u8; 32];
        let (host_conn, viewer_conn) = lossy_pair(drop_per_mille).await;
        let (mut host_tx, _host_rx) =
            transport(host_conn, host_feedback.then(|| FeedbackKeys::derive(&shared, true))).into_split();
        let (viewer_tx, mut viewer_rx) =
            transport(viewer_conn, viewer_feedback.then(|| FeedbackKeys::derive(&shared, false))).into_split();
        let viewer_loss = viewer_tx.loss_meter();
        let rate = host_tx.rate_control();
        rate.set_ceiling(CEILING);
        // Let a sender report switch the viewer to ordered delivery.
        tokio::time::sleep(Duration::from_millis(150)).await;

        let sending = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(16));
            for seq in 0..frames {
                interval.tick().await;
                host_tx.ready_for_video().await;
                let mut frame = vec![(seq % 251) as u8; 20_000];
                frame[..4].copy_from_slice(&seq.to_be_bytes());
                host_tx.send(Tag::Video, &frame).await.unwrap();
            }
            host_tx
        });
        let mut received = Vec::new();
        while let Ok(Some((tag, frame))) = tokio::time::timeout(Duration::from_millis(600), viewer_rx.recv()).await {
            assert_eq!(tag, Tag::Video);
            let seq = u32::from_be_bytes(frame[..4].try_into().unwrap());
            assert_eq!(frame.len(), 20_000);
            assert!(frame[4..].iter().all(|&byte| byte == (seq % 251) as u8), "frame {seq} corrupted");
            received.push(seq);
            if seq + 1 == frames {
                break;
            }
        }
        let _host_tx = sending.await.unwrap();
        LinkRun { received, target: rate.target(), viewer_loss: viewer_loss.latest(Instant::now()) }
    }

    fn assert_in_order(received: &[u32]) {
        assert!(received.windows(2).all(|pair| pair[0] < pair[1]), "out of order: {received:?}");
    }

    #[tokio::test]
    async fn lossy_link_recovers_frames_in_order() {
        // 5% loss in both directions: without retransmission only ~38% of
        // 19-fragment frames would arrive whole (about 42 of 120 here).
        let run = run_link(50, true, true, 120).await;
        assert_in_order(&run.received);
        assert!(run.received.len() >= 114, "only {} of 120 frames arrived", run.received.len());
        // Random loss this low is repaired, not a reason to drop quality
        // (pinned exactly in rate.rs). Real sockets on a busy CI runner add
        // queueing delay the estimator rightly reacts to, so only check it
        // didn't sink to the floor.
        assert!(run.target > crate::transport::rate::MIN_TARGET_BITRATE, "bitrate collapsed: {}", run.target);
    }

    #[tokio::test]
    async fn heavy_loss_lowers_the_bitrate_and_most_frames_still_arrive() {
        let run = run_link(150, true, true, 120).await;
        assert_in_order(&run.received);
        assert!(run.received.len() >= 100, "only {} of 120 frames arrived", run.received.len());
        assert!(run.target < CEILING / 2, "bitrate did not adapt: {}", run.target);
        // The viewer sees the loss too, for its stats panel.
        assert!(run.viewer_loss.is_some_and(|loss| loss > 0.02), "viewer loss: {:?}", run.viewer_loss);
    }

    #[tokio::test]
    async fn clean_link_keeps_the_ceiling() {
        let run = run_link(0, true, true, 60).await;
        assert_eq!(run.received, (0..60).collect::<Vec<_>>());
        assert_eq!(run.target, CEILING);
        assert_eq!(run.viewer_loss, Some(0.0));
    }

    #[tokio::test]
    async fn interoperates_with_peers_without_feedback() {
        // An old viewer drops the sender reports: frames still arrive and
        // the host, hearing no reports, keeps the ceiling.
        let run = run_link(0, true, false, 30).await;
        assert_eq!(run.received, (0..30).collect::<Vec<_>>());
        assert_eq!(run.target, CEILING);
        // A new viewer of an old host stays in legacy (unordered, no NACK)
        // mode and still gets every frame.
        let run = run_link(0, false, true, 30).await;
        assert_eq!(run.received, (0..30).collect::<Vec<_>>());
    }

    /// The viewer's RTT meter after a short idle session.
    async fn viewer_rtt(host_feedback: bool, viewer_feedback: bool) -> (Option<Duration>, Option<Duration>) {
        let shared = [12u8; 32];
        let (host_conn, viewer_conn) = lossy_pair(0).await;
        let (host_tx, _host_rx) = transport(host_conn, host_feedback.then(|| FeedbackKeys::derive(&shared, true))).into_split();
        let (viewer_tx, _viewer_rx) = transport(viewer_conn, viewer_feedback.then(|| FeedbackKeys::derive(&shared, false))).into_split();
        // The host estimates the path once its encoder publishes a ceiling.
        host_tx.rate_control().set_ceiling(CEILING);
        // A few report intervals: the host needs one receiver report before
        // its sender reports carry an RTT.
        tokio::time::sleep(Duration::from_millis(400)).await;
        let now = Instant::now();
        (host_tx.rtt_meter().latest(now), viewer_tx.rtt_meter().latest(now))
    }

    #[tokio::test]
    async fn viewer_learns_the_rtt_from_host_sender_reports() {
        let (host, viewer) = viewer_rtt(true, true).await;
        let viewer = viewer.expect("viewer has an RTT");
        assert!(viewer < Duration::from_millis(100), "{viewer:?}");
        assert!(host.is_some());
    }

    #[test]
    fn path_watch_judges_only_peers_that_keep_talking() {
        let limits = SilenceLimits { feedback: Duration::from_secs(8), media: Duration::from_secs(15) };
        let start = Instant::now();
        let later = |secs| start + Duration::from_secs(secs);

        // Nothing heard yet: an old viewer only sends input, so no verdict.
        let mut host = PathWatch::new(start, limits, false);
        assert!(!host.dead(later(60)));
        host.heard(later(1), false);
        assert!(!host.dead(later(60)), "input silence is not a dead path");
        // Feedback comes every 100 ms, so its silence is.
        host.heard(later(2), true);
        assert!(!host.dead(later(9)));
        assert!(host.dead(later(10)));

        // A viewer of a host without feedback waits out the media limit: those
        // hosts send frames even on a static screen.
        let mut viewer = PathWatch::new(start, limits, true);
        assert!(!viewer.dead(later(60)), "nothing arrived yet");
        viewer.heard(later(1), false);
        assert!(!viewer.dead(later(15)));
        assert!(viewer.dead(later(16)));
    }

    const SHORT: SilenceLimits = SilenceLimits { feedback: Duration::from_millis(300), media: Duration::from_millis(300) };

    #[tokio::test]
    async fn a_dead_direct_path_closes_both_receivers() {
        let shared = [13u8; 32];
        let (host_conn, viewer_conn) = lossy_pair(0).await;
        let (_host_tx, mut host_rx) =
            DirectTransport::with_feedback(Arc::clone(&host_conn) as _, FeedbackKeys::derive(&shared, true)).with_silence_limits(SHORT).into_split();
        let (_viewer_tx, mut viewer_rx) =
            DirectTransport::with_feedback(Arc::clone(&viewer_conn) as _, FeedbackKeys::derive(&shared, false)).with_silence_limits(SHORT).into_split();
        // Reports flow while the path is up, with no media at all.
        assert!(timeout(Duration::from_millis(700), host_rx.recv()).await.is_err(), "a quiet but healthy path stays open");

        for conn in [&host_conn, &viewer_conn] {
            conn.cut.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        let (host, viewer) = tokio::join!(
            timeout(Duration::from_secs(2), host_rx.recv()),
            timeout(Duration::from_secs(2), viewer_rx.recv()),
        );
        assert_eq!(host.expect("host still waits on a dead path"), None);
        assert_eq!(viewer.expect("viewer still waits on a dead path"), None);
    }

    #[tokio::test]
    async fn a_quiet_old_viewer_keeps_the_host_path_open() {
        let shared = [14u8; 32];
        let (host_conn, viewer_conn) = lossy_pair(0).await;
        let (_host_tx, mut host_rx) =
            DirectTransport::with_feedback(host_conn, FeedbackKeys::derive(&shared, true)).with_silence_limits(SHORT).into_split();
        let (_viewer_tx, _viewer_rx) = DirectTransport::new(viewer_conn).into_split();
        assert!(timeout(Duration::from_millis(900), host_rx.recv()).await.is_err());
    }

    #[tokio::test]
    async fn rtt_stays_unknown_with_peers_without_feedback() {
        // Old host: no sender reports, so the viewer has nothing to show.
        assert_eq!(viewer_rtt(false, true).await, (None, None));
        // Old viewer: no receiver reports, so the host never measures one.
        assert_eq!(viewer_rtt(true, false).await, (None, None));
    }
}

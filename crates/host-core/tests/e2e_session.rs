use host_core::{
    capture::FakeCapturer,
    encode::H264Encoder,
    filexfer::{ConsentDecision, FileReceiver, FileSink, FileSource, FileSender},
    inject::{InputInjector, NoopInjector},
    relay_client,
    roles::{run_host_session, run_host_session_with_data, run_viewer_session, run_viewer_session_with_data, DataHandle},
};
use async_trait::async_trait;
use protocol::{clipboard::ClipboardMsg, crypto::{Opener, Sealer}, datamsg::DataMsg, filexfer::FileMsg, input::InputEvent, Tag};
use sha2::{Digest, Sha256};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::path::PathBuf;
use tokio::time::{sleep, timeout, Duration};

const PEER_ID: &str = "111111111";
const PASSWORD: &str = "testpw";

struct RecordingInjector(Arc<AtomicUsize>);

struct DataFileSink {
    sender: host_core::transport::reliable::ReliableSender,
    sealer: Sealer,
}

#[async_trait]
impl FileSink for DataFileSink {
    async fn send(&mut self, message: FileMsg) {
        let sealed = self.sealer.seal(&[Tag::Data as u8], &DataMsg::FileTransfer(message).to_bytes());
        self.sender
            .send_frame(protocol::Tag::Data, &sealed)
            .await
            .expect("live Data channel remains available");
    }
}

struct DataFileSource {
    receiver: tokio::sync::mpsc::Receiver<Vec<u8>>,
    opener: Opener,
}

#[async_trait]
impl FileSource for DataFileSource {
    async fn recv(&mut self) -> Option<FileMsg> {
        while let Some(sealed) = self.receiver.recv().await {
            if let Some(plaintext) = self.opener.open(&[Tag::Data as u8], &sealed) {
                if let Some(DataMsg::FileTransfer(message)) = DataMsg::from_bytes(&plaintext) {
                    return Some(message);
                }
            }
        }
        None
    }
}

impl InputInjector for RecordingInjector {
    fn inject(&mut self, _event: &InputEvent, _screen_w: u32, _screen_h: u32) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn connect_to_peer(addr: &str, viewer: bool) -> relay_client::RelayWebSocket {
    let ws_url = format!("ws://{addr}/ws");
    let mut ws = relay_client::connect(&ws_url).await.expect("relay connect");
    if viewer {
        relay_client::connect_to(&mut ws, PEER_ID, if viewer { "222222222" } else { "333333333" })
            .await
            .expect("relay connect_to");
    } else {
        relay_client::register(&mut ws, PEER_ID, &"00".repeat(32))
            .await
            .expect("relay register");
    }
    ws
}

async fn wait_for_count(count: &AtomicUsize) {
    timeout(Duration::from_secs(5), async {
        while count.load(Ordering::SeqCst) == 0 {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("host did not inject the viewer input");
}

async fn run_positive_session() {
    let addr = relay::spawn_test_server().await;
    let injector_count = Arc::new(AtomicUsize::new(0));
    let host_injector_count = Arc::clone(&injector_count);

    let host_ws = connect_to_peer(&addr, false).await;
    let host = tokio::spawn(async move {
        run_host_session(
            host_ws,
            PASSWORD,
            "e2e-positive",
            FakeCapturer::new(64, 64),
            |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
            RecordingInjector(host_injector_count),
        )
        .await
    });

    let viewer_ws = connect_to_peer(&addr, true).await;
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(4);
    let (input_tx, input_rx) = tokio::sync::mpsc::channel(4);
    let viewer = tokio::spawn(run_viewer_session(
        viewer_ws,
        PASSWORD,
        "e2e-positive",
        frame_tx,
        input_rx,
    ));

    let frame = timeout(Duration::from_secs(15), frame_rx.recv())
        .await
        .expect("timed out waiting for a sealed video frame")
        .expect("viewer frame sink closed");
    assert!(frame.len() > 18, "opened frame must contain H.264 payload");
    assert_eq!(frame[12], 1, "opened frame must declare H.264");
    assert_eq!(&frame[14..16], &64u16.to_be_bytes());
    assert_eq!(&frame[16..18], &64u16.to_be_bytes());

    let input = InputEvent::Key {
        code: 65,
        down: true,
        modifiers: 0,
    }
    .to_bytes(42);
    input_tx.send(input).await.expect("send viewer input");
    wait_for_count(&injector_count).await;

    viewer.abort();
    host.abort();
}

async fn run_wrong_password_session() {
    let addr = relay::spawn_test_server().await;
    let host_ws = connect_to_peer(&addr, false).await;
    let host = tokio::spawn(async move {
        run_host_session(
            host_ws,
            PASSWORD,
            "e2e-negative",
            FakeCapturer::new(64, 64),
            |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
            RecordingInjector(Arc::new(AtomicUsize::new(0))),
        )
        .await
    });

    let viewer_ws = connect_to_peer(&addr, true).await;
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(4);
    let (_input_tx, input_rx) = tokio::sync::mpsc::channel(4);
    let viewer = tokio::spawn(run_viewer_session(
        viewer_ws,
        "wrong-password",
        "e2e-negative",
        frame_tx,
        input_rx,
    ));

    let result = timeout(Duration::from_secs(5), frame_rx.recv()).await;
    assert!(
        !matches!(result, Ok(Some(_))),
        "wrong password must not produce media"
    );
    // The host's confirmation must reach the viewer before the host hangs
    // up, or the viewer reads a dropped connection and keeps retrying.
    let error = timeout(Duration::from_secs(5), viewer).await.unwrap().unwrap().unwrap_err();
    assert!(error.to_string().contains("key confirmation failed"), "got: {error}");
    host.abort();
}

fn test_temp_dir(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after the epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("opendesk-e2e-{label}-{}-{nonce}", std::process::id()))
}

#[tokio::test(flavor = "multi_thread")]
async fn live_file_transfer_is_sealed_and_relayed_end_to_end() {
    run_live_file_transfer("filexfer", 200 * 1024, Duration::ZERO).await;
}

/// Larger than the reliable channel's Data buffer and accepted only after a
/// delay: the sender must wait for Accept and then respect the ack window, or
/// the receiver's Data queue overflows and the session drops.
#[tokio::test(flavor = "multi_thread")]
async fn large_file_with_delayed_accept_keeps_the_session_alive() {
    run_live_file_transfer("filexfer-large", 12 * 1024 * 1024, Duration::from_millis(1500)).await;
}

async fn run_live_file_transfer(label: &str, size: usize, consent_delay: Duration) {
    let addr = relay::spawn_test_server().await;
    let root = test_temp_dir(label);
    let source_dir = root.join("source");
    let downloads_dir = root.join("downloads");
    tokio::fs::create_dir_all(&source_dir).await.expect("create source dir");
    let source_path = source_dir.join("known-payload.bin");

    let mut expected = Vec::with_capacity(size);
    let mut state = 0x1234_5678u32;
    for _ in 0..size {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        expected.push((state >> 24) as u8);
    }
    tokio::fs::write(&source_path, &expected).await.expect("write source file");

    let host_ws = connect_to_peer(&addr, false).await;
    let (host_ready_tx, host_ready_rx) = tokio::sync::oneshot::channel();
    let host = tokio::spawn(async move {
        run_host_session_with_data(
            host_ws,
            PASSWORD,
            "e2e-filexfer",
            FakeCapturer::new(64, 64),
            |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
            NoopInjector::default(),
            Some(host_ready_tx),
        )
        .await
    });

    let viewer_ws = connect_to_peer(&addr, true).await;
    let (frame_tx, _frame_rx) = tokio::sync::mpsc::channel(4);
    let (_input_tx, input_rx) = tokio::sync::mpsc::channel(4);
    let (viewer_ready_tx, viewer_ready_rx) = tokio::sync::oneshot::channel();
    let viewer = tokio::spawn(run_viewer_session_with_data(
        viewer_ws,
        PASSWORD,
        "e2e-filexfer",
        frame_tx,
        input_rx,
        Some(viewer_ready_tx),
    ));

    let host_handle = timeout(Duration::from_secs(15), host_ready_rx)
        .await
        .expect("timed out waiting for host Data handle")
        .expect("host session closed before Data handle");
    let viewer_handle = timeout(Duration::from_secs(15), viewer_ready_rx)
        .await
        .expect("timed out waiting for viewer Data handle")
        .expect("viewer session closed before Data handle");
    let DataHandle { sender, sealer, receiver: host_receiver, opener: host_opener, .. } = host_handle;
    let DataHandle { receiver, opener, sender: viewer_sender, sealer: viewer_sealer, .. } = viewer_handle;

    let mut sink = DataFileSink { sender, sealer };
    let mut replies = DataFileSource { receiver: host_receiver, opener: host_opener };
    let mut source = DataFileSource { receiver, opener };
    let mut answers = DataFileSink { sender: viewer_sender, sealer: viewer_sealer };
    let receive_dir = downloads_dir.clone();
    let receive = tokio::spawn(async move {
        FileReceiver::receive_with_async_consent(&mut source, &mut answers, &receive_dir, |_| async move {
            tokio::time::sleep(consent_delay).await;
            ConsentDecision::Accept
        }, |_, _| {})
            .await
    });
    let send = FileSender::send(&source_path, &mut sink, &mut replies, |_, _| {});
    timeout(Duration::from_secs(30), async {
        let (send_result, receive_result) = tokio::join!(send, receive);
        send_result.expect("FileSender failed");
        receive_result
            .expect("receiver task panicked")
            .expect("FileReceiver failed");
    })
    .await
    .expect("file transfer exceeded timeout");

    let received_path = tokio::fs::read_dir(&downloads_dir)
        .await
        .expect("downloads directory was not created")
        .next_entry()
        .await
        .expect("read downloads directory")
        .expect("received file is missing")
        .path();
    let received = tokio::fs::read(&received_path).await.expect("read received file");
    assert_eq!(received, expected, "relay Data transfer changed file bytes");
    let expected_hash: [u8; 32] = Sha256::digest(&expected).into();
    let received_hash: [u8; 32] = Sha256::digest(&received).into();
    assert_eq!(received_hash, expected_hash, "received file SHA-256 differs");
    assert_eq!(received_path.file_name().and_then(|name| name.to_str()), Some("known-payload.bin"));
    assert!(!host.is_finished() && !viewer.is_finished(), "the transfer must not end the session");

    viewer.abort();
    host.abort();
    let _ = tokio::fs::remove_dir_all(root).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn live_clipboard_is_sealed_and_relayed_end_to_end() {
    let addr = relay::spawn_test_server().await;

    let host_ws = connect_to_peer(&addr, false).await;
    let (host_ready_tx, host_ready_rx) = tokio::sync::oneshot::channel();
    let host = tokio::spawn(async move {
        run_host_session_with_data(
            host_ws,
            PASSWORD,
            "e2e-clipboard",
            FakeCapturer::new(64, 64),
            |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
            NoopInjector::default(),
            Some(host_ready_tx),
        )
        .await
    });

    let viewer_ws = connect_to_peer(&addr, true).await;
    let (frame_tx, _frame_rx) = tokio::sync::mpsc::channel(4);
    let (_input_tx, input_rx) = tokio::sync::mpsc::channel(4);
    let (viewer_ready_tx, viewer_ready_rx) = tokio::sync::oneshot::channel();
    let viewer = tokio::spawn(run_viewer_session_with_data(
        viewer_ws,
        PASSWORD,
        "e2e-clipboard",
        frame_tx,
        input_rx,
        Some(viewer_ready_tx),
    ));

    let host_handle = timeout(Duration::from_secs(15), host_ready_rx)
        .await
        .expect("timed out waiting for host Data handle")
        .expect("host session closed before Data handle");
    let viewer_handle = timeout(Duration::from_secs(15), viewer_ready_rx)
        .await
        .expect("timed out waiting for viewer Data handle")
        .expect("viewer session closed before Data handle");
    let DataHandle { sender, mut sealer, .. } = host_handle;
    let DataHandle { mut receiver, mut opener, .. } = viewer_handle;

    let expected = ClipboardMsg {
        seq: 27,
        text: "clipboard round-trip: café 📋".into(),
    };
    let sealed = sealer.seal(&[Tag::Data as u8], &DataMsg::Clipboard(expected.clone()).to_bytes());
    sender
        .send_frame(Tag::Data, &sealed)
        .await
        .expect("live Data channel remains available");

    let received = timeout(Duration::from_secs(15), receiver.recv())
        .await
        .expect("timed out waiting for clipboard Data frame")
        .expect("viewer Data channel closed");
    let plaintext = opener.open(&[Tag::Data as u8], &received).expect("clipboard frame failed to open");
    assert_eq!(
        DataMsg::from_bytes(&plaintext),
        Some(DataMsg::Clipboard(expected)),
        "sealed relay Data channel changed clipboard message bytes"
    );

    viewer.abort();
    host.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn relay_host_viewer_session_seals_media_and_input() {
    run_positive_session().await;
    run_wrong_password_session().await;
}

/// Sleeps on every event to model a slow, blocking OS injection call.
struct SlowInjector {
    delay: Duration,
    injected: Arc<AtomicUsize>,
}

impl InputInjector for SlowInjector {
    fn inject(&mut self, _event: &InputEvent, _screen_w: u32, _screen_h: u32) {
        std::thread::sleep(self.delay);
        self.injected.fetch_add(1, Ordering::SeqCst);
    }
}

/// Flags when dropped, i.e. when the injection thread has exited.
struct DropFlagInjector(Arc<std::sync::atomic::AtomicBool>);

impl InputInjector for DropFlagInjector {
    fn inject(&mut self, _event: &InputEvent, _screen_w: u32, _screen_h: u32) {}
}

impl Drop for DropFlagInjector {
    fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
}

#[tokio::test(flavor = "multi_thread")]
async fn slow_injection_does_not_stall_video_frames() {
    let addr = relay::spawn_test_server().await;
    let injected = Arc::new(AtomicUsize::new(0));
    let host_injector = SlowInjector { delay: Duration::from_millis(1_000), injected: Arc::clone(&injected) };

    let host_ws = connect_to_peer(&addr, false).await;
    let host = tokio::spawn(async move {
        run_host_session(
            host_ws,
            PASSWORD,
            "e2e-slow-inject",
            FakeCapturer::new(64, 64),
            |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
            host_injector,
        )
        .await
    });

    let viewer_ws = connect_to_peer(&addr, true).await;
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(4);
    let (input_tx, input_rx) = tokio::sync::mpsc::channel(16);
    let viewer = tokio::spawn(run_viewer_session(viewer_ws, PASSWORD, "e2e-slow-inject", frame_tx, input_rx));

    timeout(Duration::from_secs(15), frame_rx.recv())
        .await
        .expect("timed out waiting for the first video frame")
        .expect("viewer frame sink closed");

    // Five events at 1 s each would stall an inline injector for ~5 s.
    for code in 65..70 {
        let input = InputEvent::Key { code, down: true, modifiers: 0 }.to_bytes(0);
        input_tx.send(input).await.expect("send viewer input");
    }
    let mut frames = 0usize;
    let window = tokio::time::Instant::now() + Duration::from_millis(2_000);
    while let Ok(Some(_)) = tokio::time::timeout_at(window, frame_rx.recv()).await {
        frames += 1;
    }
    // ~30 frames at 15 fps; an inline 1 s injector would yield about 2 (at most
    // 3) in this window. The bound is loose so slow, loaded CI runners pass.
    assert!(frames >= 4, "video stalled behind input injection: only {frames} frames in 2 s");
    wait_for_count(&injected).await;

    viewer.abort();
    host.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn host_session_ends_cleanly_on_viewer_bye() {
    let addr = relay::spawn_test_server().await;
    let injector_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let host_injector = DropFlagInjector(Arc::clone(&injector_dropped));

    let host_ws = connect_to_peer(&addr, false).await;
    let host = tokio::spawn(async move {
        run_host_session_with_data(
            host_ws,
            PASSWORD,
            "e2e-bye",
            FakeCapturer::new(64, 64),
            |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
            host_injector,
            None,
        )
        .await
    });

    let viewer_ws = connect_to_peer(&addr, true).await;
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(4);
    let (_input_tx, input_rx) = tokio::sync::mpsc::channel(4);
    let (viewer_ready_tx, viewer_ready_rx) = tokio::sync::oneshot::channel();
    let viewer = tokio::spawn(run_viewer_session_with_data(
        viewer_ws,
        PASSWORD,
        "e2e-bye",
        frame_tx,
        input_rx,
        Some(viewer_ready_tx),
    ));
    let DataHandle { control_sender, control_sealer, .. } = timeout(Duration::from_secs(15), viewer_ready_rx)
        .await
        .expect("timed out waiting for viewer Data handle")
        .expect("viewer session closed before Data handle");
    timeout(Duration::from_secs(15), frame_rx.recv())
        .await
        .expect("timed out waiting for the first video frame")
        .expect("viewer frame sink closed");

    let bye = protocol::control::Control::Bye { session: "e2e-bye".into() }.to_bytes();
    let sealed = control_sealer.lock().unwrap().seal(&[Tag::Control as u8], &bye);
    control_sender.send_frame(Tag::Control, &sealed).await.expect("send Bye");

    let result = timeout(Duration::from_secs(5), host)
        .await
        .expect("host session did not end after Bye")
        .expect("host task panicked");
    assert!(result.is_ok(), "Bye must end the host session cleanly: {result:?}");
    timeout(Duration::from_secs(2), async {
        while !injector_dropped.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("injection thread did not exit with the session");

    viewer.abort();
}

#[tokio::test(flavor = "multi_thread")]
async fn host_session_ends_when_viewer_disappears() {
    let addr = relay::spawn_test_server().await;
    let host_ws = connect_to_peer(&addr, false).await;
    let host = tokio::spawn(async move {
        run_host_session(
            host_ws,
            PASSWORD,
            "e2e-peer-gone",
            FakeCapturer::new(64, 64),
            |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
            NoopInjector::default(),
        )
        .await
    });

    let viewer_ws = connect_to_peer(&addr, true).await;
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(4);
    let (_input_tx, input_rx) = tokio::sync::mpsc::channel(4);
    let viewer = tokio::spawn(run_viewer_session(viewer_ws, PASSWORD, "e2e-peer-gone", frame_tx, input_rx));
    timeout(Duration::from_secs(15), frame_rx.recv())
        .await
        .expect("timed out waiting for the first video frame")
        .expect("viewer frame sink closed");

    viewer.abort();
    let ended = timeout(Duration::from_secs(10), host).await;
    assert!(ended.is_ok(), "host session kept running after the viewer disconnected");
}

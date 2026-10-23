//! The browser viewer's session code, run natively against a real host-core
//! host through an in-process relay.
#![cfg(not(target_arch = "wasm32"))]

use futures_util::{SinkExt, StreamExt};
use host_core::{
    capture::FakeCapturer,
    encode::H264Encoder,
    inject::InputInjector,
    relay_client,
    roles::{run_host_session, run_host_session_with_data_and_accept_and_2fa},
};
use protocol::input::InputEvent;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use web_core::{connect_frame, reconnect, Event, ViewerSession};

const HOST_ID: &str = "444444444";
const BROWSER_ID: &str = "555555555";
const PASSWORD: &str = "browser-pw";

struct CountingInjector(Arc<AtomicUsize>);

impl InputInjector for CountingInjector {
    fn inject(&mut self, _event: &InputEvent, _w: u32, _h: u32) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

async fn register_host(addr: &str) -> relay_client::RelayWebSocket {
    let mut ws = relay_client::connect(&format!("ws://{addr}/ws")).await.expect("host connects");
    relay_client::register(&mut ws, HOST_ID, &"ab".repeat(32)).await.expect("host registers");
    // The relay parks the registration before the browser's connect.
    tokio::time::sleep(Duration::from_millis(100)).await;
    ws
}

/// What a browser tab does: a plain WebSocket, the session state machine,
/// and a timer.
struct Browser {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    session: ViewerSession,
    started: Instant,
}

impl Browser {
    async fn connect(addr: &str, password: &str, auth_code: &str) -> Self {
        let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws")).await.expect("browser connects");
        let frame = connect_frame(HOST_ID, BROWSER_ID).unwrap();
        ws.send(Message::Binary(frame.into())).await.unwrap();
        Self { ws, session: ViewerSession::new(password, auth_code), started: Instant::now() }
    }

    fn now(&self) -> f64 { self.started.elapsed().as_secs_f64() * 1000.0 }

    async fn send(&mut self, bytes: Vec<u8>) {
        self.ws.send(Message::Binary(bytes.into())).await.expect("relay accepts the frame");
    }

    /// The next event that is not a frame to send; sends those on the way.
    async fn next(&mut self) -> Event {
        loop {
            let events = tokio::select! {
                message = self.ws.next() => match message {
                    Some(Ok(Message::Binary(bytes))) => self.session.receive(&bytes, self.now()),
                    Some(Ok(_)) => continue,
                    _ => self.session.closed(),
                },
                () = tokio::time::sleep(Duration::from_millis(250)) => self.session.tick(self.now()),
            };
            let mut found = None;
            for event in events {
                match event {
                    Event::Send(bytes) => self.send(bytes).await,
                    other if found.is_none() => found = Some(other),
                    _ => {}
                }
            }
            if let Some(event) = found { return event; }
        }
    }

    async fn next_within(&mut self, limit: Duration) -> Event {
        timeout(limit, self.next()).await.expect("timed out waiting for a session event")
    }

    async fn video(&mut self) -> Vec<u8> {
        loop {
            match self.next_within(Duration::from_secs(20)).await {
                Event::Video(frame) => return frame,
                Event::Ended(message) => panic!("session ended: {message}"),
                _ => {}
            }
        }
    }

    async fn ended(&mut self) -> String {
        loop {
            if let Event::Ended(message) = self.next_within(Duration::from_secs(20)).await { return message; }
        }
    }
}

fn fake_host_parts() -> (FakeCapturer, impl Fn(u16, u16, host_core::encode::Codec) -> Result<H264Encoder, host_core::encode::EncoderInitError> + Clone + Send + 'static) {
    (FakeCapturer::new(64, 64), |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000))
}

#[tokio::test(flavor = "multi_thread")]
async fn browser_viewer_streams_and_controls_a_real_host() {
    let addr = relay::spawn_test_server().await;
    let injected = Arc::new(AtomicUsize::new(0));
    let host_ws = register_host(&addr).await;
    let (capturer, encoder) = fake_host_parts();
    let injector = CountingInjector(Arc::clone(&injected));
    let host = tokio::spawn(async move {
        run_host_session(host_ws, PASSWORD, "s1", capturer, encoder, injector).await.map_err(|error| error.to_string())
    });

    let mut browser = Browser::connect(&addr, PASSWORD, "").await;
    assert_eq!(browser.next_within(Duration::from_secs(10)).await, Event::Authenticated);

    // The host gives up on ICE (the browser never answers) and streams over
    // the relay.
    let frame = browser.video().await;
    assert!(frame.len() > web_core::VIDEO_HEADER_LEN);
    assert_eq!(frame[12], 1, "H.264");
    assert_eq!(frame[13] & 1, 1, "the first decodable frame is a keyframe");
    assert_eq!(&frame[14..18], &[0, 64, 0, 64]);

    let key = InputEvent::Key { code: 65, down: true, modifiers: 0 }.to_bytes(1);
    let sealed = browser.session.input(&key).unwrap();
    browser.send(sealed).await;
    timeout(Duration::from_secs(10), async {
        while injected.load(Ordering::SeqCst) == 0 {
            let _ = browser.next().await;
        }
    })
    .await
    .expect("the host injected the browser's input");

    // A keyframe request is honoured.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let request = browser.session.request_keyframe().unwrap();
    browser.send(request).await;
    timeout(Duration::from_secs(10), async {
        while browser.video().await[13] & 1 == 0 {}
    })
    .await
    .expect("a keyframe after the request");

    // Disconnect: the host ends its session cleanly.
    let bye = browser.session.bye().unwrap();
    browser.send(bye).await;
    let ended = timeout(Duration::from_secs(10), host).await.expect("host ends after bye").unwrap();
    assert_eq!(ended, Ok(()));
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_gets_the_desktop_error() {
    let addr = relay::spawn_test_server().await;
    let host_ws = register_host(&addr).await;
    let (capturer, encoder) = fake_host_parts();
    let host = tokio::spawn(async move {
        run_host_session(host_ws, PASSWORD, "s1", capturer, encoder, CountingInjector(Arc::default())).await.is_err()
    });

    let mut browser = Browser::connect(&addr, "not-the-password", "").await;
    let message = browser.ended().await;
    assert_eq!(message, "key confirmation failed");
    let reconnect::NextStep::Stop { message } = reconnect::after_failure(&message, false, 0, Duration::ZERO) else { panic!("must not retry") };
    assert!(message.contains("check the password"), "{message}");
    assert!(timeout(Duration::from_secs(10), host).await.unwrap().unwrap(), "host rejects the viewer");
}

#[tokio::test(flavor = "multi_thread")]
async fn offline_host_is_reported_as_unavailable() {
    let addr = relay::spawn_test_server().await;
    let mut browser = Browser::connect(&addr, PASSWORD, "").await;
    assert_eq!(browser.ended().await, reconnect::PEER_UNAVAILABLE_MESSAGE);
}

async fn rejected_with(unattended: bool, twofa: bool, auth_code: &str) -> String {
    let addr = relay::spawn_test_server().await;
    let host_ws = register_host(&addr).await;
    let (capturer, encoder) = fake_host_parts();
    let _host = tokio::spawn(async move {
        run_host_session_with_data_and_accept_and_2fa(
            host_ws, PASSWORD, "s1", capturer, encoder, CountingInjector(Arc::default()),
            unattended, twofa, Some("JBSWY3DPEHPK3PXP"), || async { false }, None,
        )
        .await
        .is_err()
    });
    let mut browser = Browser::connect(&addr, PASSWORD, auth_code).await;
    let message = browser.ended().await;
    let reconnect::NextStep::Stop { message } = reconnect::after_failure(&message, false, 0, Duration::ZERO) else { panic!("must not retry: {message}") };
    message
}

#[tokio::test(flavor = "multi_thread")]
async fn host_rejections_reach_the_browser() {
    assert_eq!(rejected_with(false, false, "").await, "The host declined the connection request.");
    assert!(rejected_with(true, true, "").await.contains("requires a two-factor code"));
    assert!(rejected_with(true, true, "000000").await.contains("The 2FA code was rejected"));
}

/// The browser copy of the retry rules must match host-core's.
#[test]
fn reconnect_rules_match_host_core() {
    use host_core::reconnect as desktop;
    let messages = [
        "key confirmation failed",
        "invalid SPAKE2 message",
        "connection closed during KEX",
        "connection closed during KEX confirmation",
        "connection closed during session",
        desktop::RELAY_LIMITED_MESSAGE,
        desktop::RELAY_REFUSED_MESSAGE,
        "connection closed: host stopped responding for 45s",
        "relay closed while connecting",
        "invalid pairing message",
        "user ended session",
        desktop::PEER_UNAVAILABLE_MESSAGE,
        desktop::HOST_ENDED_MESSAGE,
        &desktop::rejection_error(desktop::reject_code::DENIED),
        &desktop::rejection_error(desktop::reject_code::TWOFA_REQUIRED),
        &desktop::rejection_error(desktop::reject_code::TWOFA_INVALID),
        &desktop::rejection_error(desktop::reject_code::CAPTURE_UNAVAILABLE),
        &desktop::rejection_error(desktop::reject_code::NO_ANSWER),
        &desktop::rejection_error(desktop::reject_code::NO_DISPLAY),
        &desktop::rejection_error(reconnect::reject_code::NO_DISPLAY),
        &desktop::rejection_error("future_code"),
    ];
    for message in messages {
        let reason = format!("{:?}", desktop::classify_viewer_error(message));
        assert_eq!(format!("{:?}", reconnect::classify_viewer_error(message)), reason, "{message}");
        for started in [false, true] {
            assert_eq!(
                reconnect::auth_failure_message(message, started),
                desktop::auth_failure_message(message, started),
                "{message}"
            );
            assert_eq!(
                reconnect::should_retry_viewer(reconnect::classify_viewer_error(message), started),
                desktop::should_retry_viewer(desktop::classify_viewer_error(message), started),
                "{message}"
            );
            assert_eq!(
                reconnect::stop_message(message, reconnect::classify_viewer_error(message), started),
                desktop::stop_message(message, desktop::classify_viewer_error(message), started),
                "{message}"
            );
        }
    }
    for code in [
        protocol::control::ERROR_PEER_UNAVAILABLE,
        protocol::control::ERROR_CONNECT_RATE,
        protocol::control::ERROR_RELAY_BUSY,
        protocol::control::ERROR_INVALID_CONNECT,
        protocol::control::ERROR_INVALID_CONTROL,
        "from_a_newer_relay",
    ] {
        assert_eq!(reconnect::connect_refusal_message(code), desktop::connect_refusal_message(code), "{code}");
    }
    assert_eq!(reconnect::rejection_error("x"), desktop::rejection_error("x"));
    for attempt in 0..12 {
        for elapsed in [0, 1, 7, 29, 30, 45].map(Duration::from_secs) {
            assert_eq!(reconnect::viewer_retry_delay(attempt, elapsed), desktop::viewer_retry_delay(attempt, elapsed));
        }
    }
}

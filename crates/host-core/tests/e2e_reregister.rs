//! A host whose session drops must be able to come back: it re-registers with
//! the relay under the same ID and token, and a later viewer connect succeeds.
//! This mirrors the desktop app's hosting loop (register -> await pairing ->
//! session -> re-register) using the same host-core helpers.

use futures_util::StreamExt;
use host_core::{
    capture::FakeCapturer,
    encode::H264Encoder,
    inject::NoopInjector,
    reconnect::{classify_viewer_error, reject_code, rejection_code, DisconnectReason},
    relay_client::{self, await_pairing, register_host, RelayWebSocket},
    roles::{
        run_host_session, run_host_session_with_data_and_accept_and_2fa, run_viewer_session,
        run_viewer_session_with_monitors_and_auth_code,
    },
};
use protocol::{control::Control, frame, Tag};
use tokio::task::JoinHandle;
use tokio::time::{timeout, Duration};
use tokio_tungstenite::tungstenite::Message;

const HOST_ID: &str = "444444444";
const PASSWORD: &str = "reconnect-pw";

/// Viewer side of the relay handshake, as the app does it: send `connect`,
/// then require a `paired` frame. A relay that has no registration for the
/// target closes the socket instead.
async fn viewer_pair(url: &str, viewer_id: &str) -> Result<RelayWebSocket, String> {
    let mut ws = relay_client::connect(url).await.map_err(|error| error.to_string())?;
    relay_client::connect_to(&mut ws, HOST_ID, viewer_id)
        .await
        .map_err(|error| error.to_string())?;
    let Some(Ok(Message::Binary(message))) = ws.next().await else {
        return Err("relay closed while connecting".into());
    };
    match frame::unwrap(&message).and_then(|(tag, body)| (tag == Tag::Control).then_some(body)).and_then(Control::from_bytes) {
        Some(Control::Paired { .. }) => Ok(ws),
        _ => Err("invalid pairing message".into()),
    }
}

/// Run one host+viewer session over already paired sockets and wait for the
/// first decrypted video frame. Returns the running tasks so the caller can
/// drop the session.
async fn run_session_until_first_frame(
    host_ws: RelayWebSocket,
    viewer_ws: RelayWebSocket,
    ice_session: &'static str,
) -> (JoinHandle<()>, JoinHandle<()>) {
    let host = tokio::spawn(async move {
        let _ = run_host_session(
            host_ws,
            PASSWORD,
            ice_session,
            FakeCapturer::new(64, 64),
            |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
            NoopInjector::default(),
        )
        .await;
    });
    let (frame_tx, mut frame_rx) = tokio::sync::mpsc::channel(4);
    let (input_tx, input_rx) = tokio::sync::mpsc::channel(4);
    let viewer = tokio::spawn(async move {
        let _ = run_viewer_session(viewer_ws, PASSWORD, ice_session, frame_tx, input_rx).await;
        // Keep the input channel open for the whole session.
        drop(input_tx);
    });
    let frame = timeout(Duration::from_secs(15), frame_rx.recv())
        .await
        .expect("timed out waiting for a video frame")
        .expect("viewer frame sink closed");
    assert!(frame.len() > 18, "opened frame must carry an H.264 payload");
    (host, viewer)
}

#[tokio::test(flavor = "multi_thread")]
async fn host_reregisters_with_same_token_after_session_drop() {
    let addr = relay::spawn_test_server().await;
    let url = format!("ws://{addr}/ws");
    let urls = vec![url.clone()];
    let token = "ab".repeat(32);

    // 1. Host registers; the first viewer pairs and a session streams.
    let (_, mut host_ws) = register_host(&urls, HOST_ID, &token).await.expect("first registration");
    let viewer_ws = viewer_pair(&url, "555555555").await.expect("first viewer pairs");
    let pairing = timeout(Duration::from_secs(5), await_pairing(&mut host_ws))
        .await
        .expect("host pairing timed out")
        .expect("host paired");
    assert_eq!(pairing.peer_id, "555555555");
    let (host, viewer) = run_session_until_first_frame(host_ws, viewer_ws, "reconnect-1").await;

    // 2. The session drops. The relay consumed the registration at pairing,
    //    so until the host re-registers a viewer connect is refused — which
    //    the viewer must classify as transient (retryable).
    host.abort();
    viewer.abort();
    let _ = (host.await, viewer.await);
    let refused = viewer_pair(&url, "555555555").await.expect_err("no registration to pair with");
    assert_eq!(classify_viewer_error(&refused), DisconnectReason::Transient);

    // 3. The host re-registers with the same ID and token (accepted under the
    //    relay's ownership lease); a different token still cannot take the ID.
    let (_, mut host_ws) = register_host(&urls, HOST_ID, &token).await.expect("re-registration");
    let (_, mut squatter) = register_host(&urls, HOST_ID, &"cd".repeat(32)).await.expect("squatter connects");
    let rejected = timeout(Duration::from_secs(5), await_pairing(&mut squatter))
        .await
        .expect("squatter closed")
        .expect_err("relay must reject a different token during the ownership lease");
    assert!(relay_client::is_id_taken(&rejected), "{rejected}");

    // 4. The (reconnecting) viewer now pairs and streams again.
    let viewer_ws = viewer_pair(&url, "555555555").await.expect("second viewer connect succeeds");
    let pairing = timeout(Duration::from_secs(5), await_pairing(&mut host_ws))
        .await
        .expect("host re-pairing timed out")
        .expect("host re-paired");
    assert_eq!(pairing.peer_id, "555555555");
    let (host, viewer) = run_session_until_first_frame(host_ws, viewer_ws, "reconnect-2").await;
    host.abort();
    viewer.abort();
}

/// The app only treats an ID as accepted once the relay has had time to
/// reject it, and tells a foreign ID from a copy of this device.
#[tokio::test(flavor = "multi_thread")]
async fn settling_tells_accepted_taken_and_in_use_apart() {
    let addr = relay::spawn_test_server().await;
    let urls = vec![format!("ws://{addr}/ws")];
    let settle = Duration::from_millis(300);
    let token = "ab".repeat(32);
    let (url, mut owner) = register_host(&urls, HOST_ID, &token).await.expect("register");
    assert_eq!(url, urls[0]);
    assert_eq!(relay_client::settle_registration(&mut owner, settle).await, Ok(None));

    let (_, mut squatter) = register_host(&urls, HOST_ID, &"cd".repeat(32)).await.expect("squatter connects");
    let taken = relay_client::settle_registration(&mut squatter, settle).await.expect_err("taken");
    assert!(relay_client::is_id_taken(&taken) && !relay_client::is_id_in_use(&taken), "{taken}");

    // A copy of this device keeps taking the registration over.
    let mut copies = vec![owner];
    let in_use = loop {
        let (_, mut copy) = register_host(&urls, HOST_ID, &token).await.expect("copy connects");
        match relay_client::settle_registration(&mut copy, settle).await {
            Ok(None) => copies.push(copy),
            other => break other.expect_err("refused"),
        }
        assert!(copies.len() < 10, "the relay never refused the copies");
    };
    assert!(relay_client::is_id_in_use(&in_use) && !relay_client::is_id_taken(&in_use), "{in_use}");
}

/// Pair a host that rejects after the key exchange with a viewer and return
/// the viewer's error, which must arrive promptly as an explicit rejection
/// rather than a generic close.
async fn viewer_rejection(
    unattended: bool,
    twofa: bool,
    viewer_code: Option<&str>,
) -> host_core::roles::ViewerSessionError {
    let addr = relay::spawn_test_server().await;
    let url = format!("ws://{addr}/ws");
    let (_, mut host_ws) = register_host(std::slice::from_ref(&url), HOST_ID, &"ab".repeat(32)).await.expect("register");
    let viewer_ws = viewer_pair(&url, "555555555").await.expect("viewer pairs");
    await_pairing(&mut host_ws).await.expect("host paired");
    let host = tokio::spawn(async move {
        run_host_session_with_data_and_accept_and_2fa(
            host_ws,
            PASSWORD,
            "reject",
            FakeCapturer::new(64, 64),
            |width, height, _codec| H264Encoder::new(width, height, 15, 1_000_000),
            NoopInjector::default(),
            unattended,
            twofa,
            Some("JBSWY3DPEHPK3PXP"),
            || async { false }, // the host user declines
            None,
        )
        .await
    });
    let (frame_tx, _frame_rx) = tokio::sync::mpsc::channel(4);
    let (_input_tx, input_rx) = tokio::sync::mpsc::channel(4);
    let viewer_code = viewer_code.map(str::to_owned);
    let result = timeout(
        Duration::from_secs(2),
        run_viewer_session_with_monitors_and_auth_code(
            viewer_ws, PASSWORD, "reject", frame_tx, input_rx, None, None, None, viewer_code,
        ),
    )
    .await
    .expect("viewer must learn of the rejection within 2s, not hang or time out");
    host.abort();
    result.expect_err("a rejected viewer session must fail")
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_denial_reaches_viewer_as_typed_rejection() {
    let error = viewer_rejection(false, false, None).await;
    assert_eq!(error.reason, DisconnectReason::AuthFailure, "{}", error.message);
    assert_eq!(rejection_code(&error.message), Some(reject_code::DENIED));
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_2fa_code_reaches_viewer_as_typed_rejection() {
    // Not six digits, so it can never be a valid TOTP code by chance.
    let error = viewer_rejection(true, true, Some("abcdef")).await;
    assert_eq!(error.reason, DisconnectReason::AuthFailure, "{}", error.message);
    assert_eq!(rejection_code(&error.message), Some(reject_code::TWOFA_INVALID));
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_2fa_code_is_rejected_promptly() {
    let error = viewer_rejection(true, true, None).await;
    assert_eq!(error.reason, DisconnectReason::AuthFailure, "{}", error.message);
    assert_eq!(rejection_code(&error.message), Some(reject_code::TWOFA_REQUIRED));
}

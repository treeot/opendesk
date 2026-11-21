// Shutdown and connection deadlines, run against the same accept loop the
// binary uses.
use futures_util::{SinkExt, StreamExt};
use protocol::{control::Control, frame, Tag};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;

type Client = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

fn settings(pairs: &[(&'static str, &'static str)]) -> relay::Settings {
    let pairs = pairs.to_vec();
    relay::Settings::from_lookup(move |name| pairs.iter().find(|(key, _)| *key == name).map(|(_, value)| value.to_string())).unwrap()
}

fn control(message: Control) -> Message { Message::Binary(frame::wrap(Tag::Control, &message.to_bytes()).into()) }

/// Reads until a close frame, skipping data, and returns its code.
async fn close_code(ws: &mut Client) -> Option<CloseCode> {
    tokio::time::timeout(Duration::from_secs(3), async {
        while let Some(Ok(message)) = ws.next().await {
            if let Message::Close(frame) = message { return frame.map(|frame| frame.code); }
        }
        None
    }).await.expect("socket was not closed")
}

#[tokio::test]
async fn shutdown_closes_every_socket_with_going_away() {
    let relay = relay::spawn_test_server_with(relay::Settings::default()).await;
    let url = format!("ws://{}/ws", relay.addr);

    // A host paired with a viewer, a host still waiting, and a socket that
    // has not sent its first message yet.
    let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    host.send(control(Control::Register { peer_id: "123456789".into(), token: "00".repeat(32), ver: 1 })).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (mut viewer, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    viewer.send(control(Control::Connect { target: "123456789".into(), peer_id: "987654321".into(), ver: 1 })).await.unwrap();
    let _paired = viewer.next().await.unwrap().unwrap();
    let (mut parked, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    parked.send(control(Control::Register { peer_id: "111111111".into(), token: "11".repeat(32), ver: 1 })).await.unwrap();
    let (mut silent, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(relay.handle.active_connections(), 4);

    relay.shutdown.send(true).unwrap();
    for ws in [&mut host, &mut viewer, &mut parked, &mut silent] {
        assert_eq!(close_code(ws).await, Some(CloseCode::Away));
    }
    assert!(relay.handle.wait_idle(Duration::from_secs(3)).await, "connections left open");
    // No longer accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(tokio_tungstenite::connect_async(&url).await.is_err());
}

#[tokio::test]
async fn an_unfinished_http_request_is_dropped() {
    let relay = relay::spawn_test_server_with(settings(&[("RELAY_HANDSHAKE_TIMEOUT_SECS", "1")])).await;
    let mut stream = tokio::net::TcpStream::connect(&relay.addr).await.unwrap();
    stream.write_all(b"GET /ws HTTP/1.1\r\nHost: localhost\r\n").await.unwrap();
    let mut rest = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(4), stream.read_to_end(&mut rest)).await;
    assert!(closed.is_ok(), "relay kept a half-sent request open");
}

#[tokio::test]
async fn an_idle_keep_alive_connection_is_dropped() {
    let relay = relay::spawn_test_server_with(settings(&[("RELAY_HANDSHAKE_TIMEOUT_SECS", "1")])).await;
    let mut stream = tokio::net::TcpStream::connect(&relay.addr).await.unwrap();
    stream.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
    let mut seen = Vec::new();
    let closed = tokio::time::timeout(Duration::from_secs(4), stream.read_to_end(&mut seen)).await;
    assert!(closed.is_ok(), "relay kept an idle connection open");
    assert!(seen.starts_with(b"HTTP/1.1 200"), "{}", String::from_utf8_lossy(&seen));
}

#[tokio::test]
async fn a_socket_that_never_sends_its_first_message_is_dropped() {
    let relay = relay::spawn_test_server_with(settings(&[("RELAY_HANDSHAKE_TIMEOUT_SECS", "1")])).await;
    let (mut ws, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", relay.addr)).await.unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(4), ws.next()).await;
    assert!(matches!(ended, Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(Message::Close(_))))), "{ended:?}");
    assert!(relay.handle.wait_idle(Duration::from_secs(2)).await);
}

#[tokio::test]
async fn a_host_nobody_connects_to_is_closed_cleanly() {
    let relay = relay::spawn_test_server_with(settings(&[("RELAY_PAIR_TIMEOUT_SECS", "1")])).await;
    let (mut host, _) = tokio_tungstenite::connect_async(format!("ws://{}/ws", relay.addr)).await.unwrap();
    host.send(control(Control::Register { peer_id: "123456789".into(), token: "00".repeat(32), ver: 1 })).await.unwrap();
    assert_eq!(close_code(&mut host).await, Some(CloseCode::Normal));
}

async fn http_get(addr: &str, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).await.unwrap();
    response
}

#[tokio::test]
async fn metrics_count_sessions_bytes_and_refusals() {
    let relay = relay::spawn_test_server_with(relay::Settings::default()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let metrics_addr = listener.local_addr().unwrap().to_string();
    let (_keep, stop) = tokio::sync::watch::channel(false);
    tokio::spawn(relay::serve(listener, relay::metrics_router(relay.handle.clone()), Duration::from_secs(5), stop));
    let url = format!("ws://{}/ws", relay.addr);

    let (mut stray, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    stray.send(control(Control::Connect { target: "555555555".into(), peer_id: "987654321".into(), ver: 1 })).await.unwrap();
    let _refused = stray.next().await;

    let (mut host, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    host.send(control(Control::Register { peer_id: "123456789".into(), token: "00".repeat(32), ver: 1 })).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (mut viewer, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    viewer.send(control(Control::Connect { target: "123456789".into(), peer_id: "987654321".into(), ver: 1 })).await.unwrap();
    let _paired = viewer.next().await.unwrap().unwrap();
    let payload = frame::wrap(Tag::Video, &[7; 100]);
    host.send(Message::Binary(payload.clone().into())).await.unwrap();
    assert_eq!(viewer.next().await.unwrap().unwrap().into_data(), payload);

    let response = http_get(&metrics_addr, "/metrics").await;
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("text/plain; version=0.0.4"), "{response}");
    for line in [
        "relay_sessions 1",
        "relay_pairings_total 1",
        &format!("relay_relayed_bytes_total {}", payload.len()),
        "relay_rejected_total{stage=\"pairing\",reason=\"peer_unavailable\"} 1",
        "relay_connections_accepted_total 3",
    ] {
        assert!(response.lines().any(|candidate| candidate == line), "missing {line:?} in:\n{response}");
    }
    // The relay port itself does not serve metrics.
    assert!(http_get(&relay.addr, "/metrics").await.starts_with("HTTP/1.1 404"));
}

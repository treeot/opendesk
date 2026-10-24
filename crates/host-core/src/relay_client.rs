//! Small helpers for the relay's existing register/connect control frames.

// These return tungstenite's own error type unchanged so callers can classify
// it (see reconnect.rs); boxing it would only add an allocation per failure.
#![allow(clippy::result_large_err)]

use futures_util::{SinkExt, StreamExt};
use protocol::{control::*, frame, Tag};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration, Instant};
use tokio_tungstenite::{connect_async_with_config, tungstenite::Message, MaybeTlsStream, WebSocketStream};

/// The WebSocket type returned by [`connect`].
pub type RelayWebSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Races every configured relay at once and keeps whichever finishes its
/// handshake first. The fastest handshake is the nearest, least-loaded relay,
/// so a session on a far primary no longer pays its latency when a closer
/// region is listed. Failed attempts drop out and the last error is returned
/// only if every relay fails.
pub async fn connect_best(urls: &[String]) -> Result<(String, RelayWebSocket), String> {
    let attempts = urls.iter().filter(|url| !url.trim().is_empty()).map(|url| {
        let url = url.clone();
        async move {
            match timeout(Duration::from_secs(3), connect(&url)).await {
                Ok(Ok(ws)) => Ok((url.clone(), ws)),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => Err(format!("relay connection timed out: {url}")),
            }
        }
    });
    first_ok(attempts, "no relay URLs configured").await
}

/// Drives `attempts` concurrently and returns the first to resolve to `Ok`,
/// cancelling the rest. If all fail, the last error is returned (or
/// `empty_error` when there were none).
async fn first_ok<T, Fut, I>(attempts: I, empty_error: &str) -> Result<T, String>
where
    I: IntoIterator<Item = Fut>,
    Fut: std::future::Future<Output = Result<T, String>>,
{
    use futures_util::stream::{FuturesUnordered, StreamExt};
    let mut racing: FuturesUnordered<Fut> = attempts.into_iter().collect();
    let mut last_error = empty_error.to_owned();
    while let Some(result) = racing.next().await {
        match result {
            Ok(value) => return Ok(value),
            Err(error) => last_error = error,
        }
    }
    Err(last_error)
}

pub async fn select_first_reachable<F, Fut>(urls: &[String], mut connector: F) -> Result<String, String>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    for url in urls {
        if connector(url.clone()).await.is_ok() {
            return Ok(url.clone());
        }
    }
    Err("no relay reachable".into())
}

/// Open a WebSocket connection to a relay endpoint (`ws://` or `wss://`).
pub async fn connect(url: &str) -> Result<RelayWebSocket, tokio_tungstenite::tungstenite::Error> {
    install_crypto_provider();
    // Nagle would hold small input frames back waiting for ACKs.
    Ok(connect_async_with_config(url, None, true).await?.0)
}

/// rustls needs a process-wide crypto provider before the first TLS handshake.
/// Errors only mean one is already installed, which is fine. Public for
/// the app's other TLS clients.
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Register this peer ID on an already connected relay WebSocket.
pub async fn register(
    ws: &mut RelayWebSocket,
    peer_id: &str,
    token: &str,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    ws.send(Message::Binary(frame::wrap(
        Tag::Control,
        &Control::Register {
            peer_id: peer_id.into(),
            token: token.into(),
            ver: 1,
        }
        .to_bytes(),
    ).into()))
    .await
}

/// Ask the first reachable relay which of `ids` have a host online right now.
/// Opens a short-lived socket, sends one `Presence` query, and returns the
/// online subset. A missing or malformed answer resolves to an empty list.
pub async fn query_presence(urls: &[String], ids: &[String]) -> Result<Vec<String>, String> {
    use futures_util::{SinkExt, StreamExt};
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let (_, mut ws) = connect_best(urls).await?;
    let query = Control::Presence { ids: ids.to_vec() };
    ws.send(Message::Binary(frame::wrap(Tag::Control, &query.to_bytes()).into()))
        .await
        .map_err(|error| error.to_string())?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await.map_err(|_| "presence query timed out".to_string())? {
            Some(Ok(Message::Binary(bytes))) => {
                return match frame::unwrap(&bytes).and_then(|(tag, body)| (tag == Tag::Control).then(|| Control::from_bytes(body)).flatten()) {
                    Some(Control::PresenceResult { online }) => Ok(online),
                    // Any other answer (e.g. an error frame) means none are known online.
                    _ => Ok(Vec::new()),
                };
            }
            Some(Ok(_)) => continue,
            Some(Err(error)) => return Err(error.to_string()),
            None => return Ok(Vec::new()),
        }
    }
}

/// A host registration that a viewer has paired with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    pub session: String,
    pub peer_id: String,
}

/// Connect to the first reachable relay and register `peer_id` with `token`.
/// Returns the relay's URL with the socket. The relay does not acknowledge
/// registration; a rejection surfaces as the socket closing in
/// [`await_pairing`] (see [`settle_registration`]).
pub async fn register_host(urls: &[String], peer_id: &str, token: &str) -> Result<(String, RelayWebSocket), String> {
    let (url, mut ws) = connect_best(urls).await?;
    register(&mut ws, peer_id, token).await.map_err(|error| error.to_string())?;
    Ok((url, ws))
}

/// How long a registration must stay open before it counts as accepted.
/// Relays reject a registration as soon as it arrives, so this only has to
/// cover a round trip.
pub const REGISTRATION_SETTLE: Duration = Duration::from_secs(2);

/// Wait up to `settle` for the relay to reject a fresh registration. `Ok(None)`
/// means it is still parked, so the relay accepted it; a viewer that pairs in
/// the meantime is returned as `Ok(Some(_))`.
pub async fn settle_registration(ws: &mut RelayWebSocket, settle: Duration) -> Result<Option<Pairing>, String> {
    match timeout(settle, await_pairing(ws)).await {
        Ok(result) => result.map(Some),
        Err(_) => Ok(None),
    }
}

/// The longest a parked registration is trusted. Relays drop a parked host
/// after their pairing timeout (60 s by default) and the host re-registers,
/// so this only runs out when the socket died without a close, as it does
/// when the computer sleeps or changes network.
pub const PARKED_LIMIT: Duration = Duration::from_secs(75);

/// How often a parked host pings the relay. A relay that answers (newer ones
/// do) must keep answering or the registration is renewed; older relays
/// don't read a parked socket, so for them only [`PARKED_LIMIT`] applies.
const PARKED_PING: Duration = Duration::from_secs(15);

/// Wait on a registered host socket until the relay pairs a viewer with it.
/// Errors when the relay closes the registration (rejected, pairing timeout,
/// relay restart), sends something other than a `paired` frame, or goes
/// quiet (see [`PARKED_LIMIT`]). Callers re-register in every case.
pub async fn await_pairing(ws: &mut RelayWebSocket) -> Result<Pairing, String> {
    await_pairing_within(ws, PARKED_LIMIT, PARKED_PING).await
}

async fn await_pairing_within(ws: &mut RelayWebSocket, limit: Duration, ping_every: Duration) -> Result<Pairing, String> {
    let limit = Instant::now() + limit;
    let mut ping = tokio::time::interval_at(Instant::now() + ping_every, ping_every);
    let mut answers_pings = false;
    let mut unanswered = false;
    loop {
        let message = tokio::select! {
            message = ws.next() => message,
            _ = tokio::time::sleep_until(limit) => return Err("relay went quiet while hosting".into()),
            _ = ping.tick() => {
                if answers_pings && unanswered {
                    return Err("relay stopped answering while hosting".into());
                }
                if !matches!(timeout(ping_every, ws.send(Message::Ping(Vec::new().into()))).await, Ok(Ok(()))) {
                    return Err("relay closed while hosting".into());
                }
                unanswered = true;
                continue;
            }
        };
        let message = match message {
            Some(Ok(Message::Binary(message))) => message,
            Some(Ok(Message::Pong(_))) => {
                answers_pings = true;
                unanswered = false;
                continue;
            }
            Some(Ok(Message::Ping(_))) => continue,
            _ => return Err("relay closed while hosting".into()),
        };
        let Some((Tag::Control, body)) = frame::unwrap(&message) else {
            return Err("invalid pairing message".into());
        };
        return match Control::from_bytes(body) {
            Some(Control::Paired { session, peer_id, .. }) => Ok(Pairing { session, peer_id }),
            Some(Control::Error { code }) => Err(match registration_refusal(&code) {
                Some(reason) => format!("relay rejected registration ({code}): {reason}"),
                None => format!("relay rejected registration ({code})"),
            }),
            _ => Err("invalid pairing message".into()),
        };
    }
}

/// What a relay's refusal code means, in words for the person hosting.
fn registration_refusal(code: &str) -> Option<&'static str> {
    Some(match code {
        ERROR_ID_TAKEN => "this ID is registered to another device",
        ERROR_ID_IN_USE => "another computer is online with this ID",
        ERROR_NEW_ID_RATE => "This relay is limiting new devices from your network. OpenDesk keeps trying, but it can take up to an hour. To skip the wait, use your own relay.",
        ERROR_REGISTRY_FULL => "This relay is full and can't take new devices. Add another relay or use your own.",
        ERROR_RELAY_BUSY => "This relay is busy or limiting connections from your network. OpenDesk keeps trying.",
        ERROR_INVALID_REGISTER => "This relay didn't accept this device's registration. Update OpenDesk and check the relay address.",
        _ => return None,
    })
}

fn refusal_code(error: &str) -> Option<&str> {
    let rest = error.strip_prefix("relay rejected registration (")?;
    Some(&rest[..rest.find(')')?])
}

/// The message to show for an [`await_pairing`] error when the relay refused
/// the registration for a reason that needs no other context. ID conflicts
/// are left out: callers explain those per relay.
pub fn refusal_message(error: &str) -> Option<&'static str> {
    refusal_code(error).filter(|code| ![ERROR_ID_TAKEN, ERROR_ID_IN_USE].contains(code)).and_then(registration_refusal)
}

/// How long to leave a relay alone after it refused a registration, when
/// trying again sooner can't help. A relay allows 20 new IDs per network an
/// hour, so one frees up every 3 minutes.
pub fn refusal_retry_after(error: &str) -> Option<Duration> {
    match refusal_code(error)? {
        ERROR_NEW_ID_RATE => Some(Duration::from_secs(180)),
        ERROR_REGISTRY_FULL | ERROR_INVALID_REGISTER => Some(Duration::from_secs(600)),
        _ => None,
    }
}

/// True when an [`await_pairing`] error means the relay refused the
/// registration because the ID belongs to a different token.
pub fn is_id_taken(error: &str) -> bool {
    refusal_code(error) == Some(ERROR_ID_TAKEN)
}

/// True when an [`await_pairing`] error means another live connection with
/// the same ID and token keeps taking the registration: a copy of this device.
pub fn is_id_in_use(error: &str) -> bool {
    refusal_code(error) == Some(ERROR_ID_IN_USE)
}

/// Ask the relay to connect this WebSocket to a registered peer.
pub async fn connect_to(
    ws: &mut RelayWebSocket,
    target: &str,
    peer_id: &str,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    ws.send(Message::Binary(frame::wrap(
        Tag::Control,
        &Control::Connect {
            target: target.into(),
            peer_id: peer_id.into(),
            ver: 1,
        }
        .to_bytes(),
    ).into()))
    .await
}

#[cfg(test)]
mod tests {
    use super::select_first_reachable;
    use std::time::Duration;

    /// Regression: release builds once shipped without TLS, so every wss://
    /// relay failed with "TLS support not compiled in". Against a listener
    /// that closes immediately, a TLS-capable client fails in the handshake;
    /// a client built without TLS fails earlier with a URL error.
    #[tokio::test]
    async fn wss_urls_are_supported() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await { drop(socket); }
        });
        let error = super::connect(&format!("wss://127.0.0.1:{port}/ws")).await.unwrap_err();
        let message = error.to_string();
        assert!(!message.contains("TLS support not compiled"), "{message}");
        assert!(!matches!(error, tokio_tungstenite::tungstenite::Error::Url(_)), "{message}");
    }

    /// A relay that accepted the WebSocket. `answer_pings` reads (and so
    /// answers pings) until that many pings arrived, then stops reading but
    /// keeps the socket open, like a path that died without a close.
    async fn quiet_relay(answer_pings: usize) -> super::RelayWebSocket {
        use futures_util::StreamExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            let mut pings = 0;
            while pings < answer_pings {
                match ws.next().await {
                    Some(Ok(super::Message::Ping(_))) => pings += 1,
                    Some(Ok(_)) => {}
                    _ => return,
                }
            }
            std::future::pending::<()>().await;
            drop(ws);
        });
        super::connect(&format!("ws://127.0.0.1:{port}")).await.unwrap()
    }

    /// Regression: a parked host waited on the socket forever, so after
    /// sleep or a network change it showed "online" while the relay had
    /// long dropped it.
    #[tokio::test(start_paused = true)]
    async fn a_parked_registration_that_goes_quiet_is_renewed() {
        let mut ws = quiet_relay(0).await;
        let started = tokio::time::Instant::now();
        let result = tokio::time::timeout(super::PARKED_LIMIT * 2, super::await_pairing(&mut ws)).await;
        assert!(result.expect("await_pairing never gave up").is_err());
        assert!(started.elapsed() <= super::PARKED_LIMIT + Duration::from_secs(1));
    }

    #[tokio::test]
    async fn a_relay_that_stops_answering_pings_is_left_early() {
        let mut ws = quiet_relay(2).await;
        let started = std::time::Instant::now();
        let result = super::await_pairing_within(&mut ws, Duration::from_secs(10), Duration::from_millis(200)).await;
        assert_eq!(result, Err("relay stopped answering while hosting".into()));
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    #[test]
    fn refusals_explain_themselves_and_rate_limits_back_off() {
        use protocol::control::*;
        let refused = |code: &str| format!("relay rejected registration ({code}): ...");
        assert!(super::refusal_message(&refused(ERROR_NEW_ID_RATE)).unwrap().contains("limiting new devices from your network"));
        assert_eq!(super::refusal_retry_after(&refused(ERROR_NEW_ID_RATE)), Some(Duration::from_secs(180)));
        assert!(super::refusal_message(&refused(ERROR_REGISTRY_FULL)).is_some());
        assert_eq!(super::refusal_retry_after(&refused(ERROR_RELAY_BUSY)), None);
        // ID conflicts are explained per relay by the caller.
        assert_eq!(super::refusal_message(&refused(ERROR_ID_TAKEN)), None);
        assert!(super::is_id_taken(&refused(ERROR_ID_TAKEN)) && !super::is_id_in_use(&refused(ERROR_ID_TAKEN)));
        // Codes from newer relays stay a generic rejection.
        assert_eq!(super::refusal_message("relay rejected registration (something_new)"), None);
        assert_eq!(super::refusal_retry_after("relay closed while hosting"), None);
    }

    #[tokio::test(start_paused = true)]
    async fn connect_best_keeps_the_fastest_relay() {
        use std::future::Future;
        use std::pin::Pin;
        let after = |ms: u64, result: Result<&'static str, String>| -> Pin<Box<dyn Future<Output = Result<&'static str, String>>>> {
            Box::pin(async move { tokio::time::sleep(Duration::from_millis(ms)).await; result })
        };
        // A far relay (500 ms) is listed first and a broken one fails fast, but
        // the closest relay's handshake (10 ms) is the one that is kept.
        let winner = super::first_ok(
            vec![after(500, Ok("far")), after(1, Err("down".into())), after(10, Ok("near"))],
            "none",
        ).await.unwrap();
        assert_eq!(winner, "near");
    }

    #[tokio::test(start_paused = true)]
    async fn connect_best_reports_the_last_error_when_all_fail() {
        use std::future::Future;
        use std::pin::Pin;
        let fail = |ms: u64, msg: &'static str| -> Pin<Box<dyn Future<Output = Result<&'static str, String>>>> {
            Box::pin(async move { tokio::time::sleep(Duration::from_millis(ms)).await; Err(msg.into()) })
        };
        let error = super::first_ok(vec![fail(1, "first"), fail(5, "last")], "none").await.unwrap_err();
        assert_eq!(error, "last");
    }

    #[tokio::test]
    async fn selection_returns_first_reachable_url_in_order() {
        let urls = vec!["wss://eu".into(), "wss://us".into(), "wss://apac".into()];
        let selected = select_first_reachable(&urls, |url| async move {
            if url == "wss://us" { Ok(()) } else { Err("offline".into()) }
        }).await.unwrap();
        assert_eq!(selected, "wss://us");
    }
}

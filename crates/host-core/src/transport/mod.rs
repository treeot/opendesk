//! Transport abstractions for carrying sealed protocol frames.

pub mod direct;
mod feedback;
pub mod ice;
pub mod link;
mod pacer;
pub mod rate;
mod recovery;
pub mod relay;
pub mod reliable;

pub use direct::{DirectTransport, FeedbackKeys};
pub use rate::RateControl;
pub use link::{LinkStatus, LossMeter, MediaPath, RttMeter};
pub use ice::RelayIceSignaling;
pub use relay::RelayTransport;
pub use reliable::{ReliableChannel, ReliableSender};

use ice::{establish_ice, IceOutcome, IceSignaling};
use std::sync::Arc;
use std::time::Duration;
use webrtc_ice::agent::agent_config::AgentConfig;

/// Default ICE configuration. Google Public STUN provides server-reflexive
/// candidates; deployments may replace `urls` with a self-hosted STUN server.
pub fn default_agent_config() -> AgentConfig {
    let mut config = AgentConfig::default();
    if let Ok(url) = webrtc_ice::url::Url::parse_url("stun:stun.l.google.com:19302") {
        config.urls.push(url);
    }
    config
}

/// Errors encountered while sending a sealed frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportError {
    /// The peer or transport has closed the connection.
    Closed,
    /// An I/O or WebSocket error, represented without exposing a concrete
    /// transport implementation in the trait API.
    Io(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed => f.write_str("transport is closed"),
            Self::Io(message) => write!(f, "transport I/O error: {message}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// A transport for sealed, tagged protocol frames.
#[async_trait::async_trait]
pub trait Transport {
    fn into_split(self) -> (Box<dyn TransportSender>, Box<dyn TransportReceiver>);
}

/// The independently owned sending half of a transport.
#[async_trait::async_trait]
pub trait TransportSender: Send {
    async fn send(&mut self, tag: protocol::Tag, sealed: &[u8]) -> Result<(), TransportError>;

    /// Resolves once a video frame can be sent without queueing behind
    /// unsent video. The host waits on this before taking a new encoded
    /// frame, so a slow link makes it skip captures instead of building
    /// latency. Cancel safe; the default (UDP) is always ready.
    async fn ready_for_video(&mut self) {}

    /// Bitrate state the host encoder follows: the encoder publishes its
    /// ceiling and applies the target this transport estimates for the path.
    /// The default is a detached instance that always yields the ceiling.
    fn rate_control(&self) -> Arc<RateControl> { Arc::new(RateControl::default()) }

    /// Round trips measured on this path (see [`link`]). The default is a
    /// detached meter that never has a sample.
    fn rtt_meter(&self) -> Arc<RttMeter> { Arc::new(RttMeter::default()) }

    /// Packet loss on the media this side receives. The default is a
    /// detached meter that never has a sample (the relay runs over TCP).
    fn loss_meter(&self) -> Arc<LossMeter> { Arc::new(LossMeter::default()) }
}

/// The independently owned receiving half of a transport.
#[async_trait::async_trait]
pub trait TransportReceiver: Send {
    async fn recv(&mut self) -> Option<(protocol::Tag, Vec<u8>)>;
}

/// Transport implementations selected for a session.
pub enum AnyTransport<S = tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    Relay(Box<relay::RelayTransport<S>>),
    Direct(direct::DirectTransport),
}

impl<S> AnyTransport<S> {
    pub fn path(&self) -> MediaPath {
        match self {
            Self::Relay(_) => MediaPath::Relay,
            Self::Direct(_) => MediaPath::Direct,
        }
    }
}

#[async_trait::async_trait]
impl<S> Transport for AnyTransport<S>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    fn into_split(self) -> (Box<dyn TransportSender>, Box<dyn TransportReceiver>) {
        match self {
            Self::Relay(transport) => transport.into_split(),
            Self::Direct(transport) => transport.into_split(),
        }
    }
}

/// Establish the direct path first and retain the relay as the fallback.
///
/// Callers should pair over the relay, complete SPAKE2 and key confirmation,
/// call this function, and only then start media: capture -> encode -> seal ->
/// transport, and transport -> open -> decode/inject.
pub async fn establish<S, R>(
    controlling: bool,
    signaling: S,
    relay_fallback: RelayTransport<R>,
    feedback: Option<FeedbackKeys>,
    agent_config: AgentConfig,
    deadline: Duration,
) -> AnyTransport<R>
where
    S: IceSignaling,
    R: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    match establish_ice(agent_config, controlling, signaling, deadline).await {
        IceOutcome::Nominated(conn) => {
            log::info!("transport selected: direct");
            AnyTransport::Direct(match feedback {
                Some(keys) => DirectTransport::with_feedback(conn, keys),
                None => DirectTransport::new(conn),
            })
        }
        IceOutcome::TimedOut => {
            log::info!("transport selected: relay (ICE timeout)");
            AnyTransport::Relay(Box::new(relay_fallback))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_tungstenite::{tungstenite::protocol::Role, WebSocketStream};

    struct NoRemote;

    #[async_trait::async_trait]
    impl IceSignaling for NoRemote {
        async fn send_local(&mut self, _signal: ice::IceSignal) {}

        async fn recv_remote(&mut self) -> Option<ice::IceSignal> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn establish_falls_back_to_relay_when_ice_times_out() {
        let (client, _server) = tokio::io::duplex(1024);
        let ws = WebSocketStream::from_raw_socket(client, Role::Client, None).await;
        let selected = establish(
            true,
            NoRemote,
            {
                let mut channel = ReliableChannel::spawn(ws);
                RelayTransport::<tokio::io::DuplexStream>::new(channel.sender(), channel.take_media_rx().unwrap())
            },
            None,
            AgentConfig::default(),
            Duration::from_millis(20),
        )
        .await;
        assert!(matches!(selected, AnyTransport::Relay(_)));
    }
}

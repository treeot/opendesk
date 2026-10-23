use crate::registry::{CloseOutbound, Outbound, Registration, Registry};
use protocol::{control::Control, frame, Tag};
use tokio::sync::oneshot;

pub const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, PartialEq, Eq)]
pub enum ClassifyError {
    InvalidControl,
    InvalidRegistration,
    /// The ID is leased to a different token.
    IdTaken,
    /// Two live connections with the same token keep displacing each other.
    IdInUse,
    RegistryFull,
    NewIdRateLimited,
    InvalidConnect,
    /// The target ID is not registered right now.
    PeerUnavailable,
    ConnectRateLimited,
}

pub fn valid_peer_id(value: &str) -> bool {
    value.len() == 9 && value.bytes().all(|byte| byte.is_ascii_digit())
}

pub fn decode_token(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) { return None; }
    let mut token = [0u8; 32];
    for (index, [high, low]) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        token[index] = (hex_value(*high)? << 4) | hex_value(*low)?;
    }
    Some(token)
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

/// Result of classifying the first message received on a fresh connection.
/// Pairing rule: a `register` parks the sender in the registry (this
/// connection is a host waiting to be taken); a matching `connect` takes it
/// and bridges both directions immediately.
pub enum Bridge {
    /// This connection is a host that just parked its sender. Resolves to
    /// the viewer's sender once a matching `connect` takes it.
    HostRegistered { peer_id: String, notify_rx: oneshot::Receiver<(Outbound, CloseOutbound, String)> },
    /// This connection is a viewer bridged to the given host sender.
    ViewerPaired { host_tx: Outbound, host_close_tx: CloseOutbound, requester_id: String, target_id: String },
    /// This connection asked which IDs are online. The relay answers and closes
    /// it; no pairing happens.
    Presence { online: Vec<String> },
}

/// Reads the classification of the first frame. Returns `None` when the
/// frame is not a valid Control frame, or a `connect` targets an unknown
/// peer_id — either way the connection should be dropped.
pub fn classify(msg: &[u8], me: Outbound, me_close_tx: CloseOutbound, reg: &Registry, source: std::net::IpAddr) -> Result<Bridge, ClassifyError> {
    let (tag, body) = frame::unwrap(msg).ok_or(ClassifyError::InvalidControl)?;
    if tag != Tag::Control { return Err(ClassifyError::InvalidControl); }
    match Control::from_bytes(body).ok_or(ClassifyError::InvalidControl)? {
        Control::Register { peer_id, token, ver } => {
            let token = decode_token(&token).ok_or(ClassifyError::InvalidRegistration)?;
            if !valid_peer_id(&peer_id) || ver != SUPPORTED_VERSION { return Err(ClassifyError::InvalidRegistration); }
            let (notify_tx, notify_rx) = oneshot::channel();
            match reg.register(peer_id.clone(), token, source, me, me_close_tx, notify_tx) {
                Registration::Accepted => {}
                Registration::IdTaken => return Err(ClassifyError::IdTaken),
                Registration::InUse => return Err(ClassifyError::IdInUse),
                Registration::Full => return Err(ClassifyError::RegistryFull),
                Registration::RateLimited => return Err(ClassifyError::NewIdRateLimited),
            }
            Ok(Bridge::HostRegistered { peer_id, notify_rx })
        }
        Control::Connect { target, peer_id, ver } => {
            if !valid_peer_id(&target) || !valid_peer_id(&peer_id) || ver != SUPPORTED_VERSION { return Err(ClassifyError::InvalidConnect); }
            if !reg.allow_connect(source, &target) { return Err(ClassifyError::ConnectRateLimited); }
            let (host_tx, host_close_tx, notify) = reg.take(&target).ok_or(ClassifyError::PeerUnavailable)?;
            let _ = notify.send((me, me_close_tx, peer_id.clone()));
            Ok(Bridge::ViewerPaired { host_tx, host_close_tx, requester_id: peer_id, target_id: target })
        }
        Control::Presence { ids } => {
            // Bounded and valid-only, so this cannot scan the ID space: each
            // query costs one rate-limited connection and asks about at most
            // MAX_PRESENCE_IDS well-formed IDs.
            let online = ids
                .into_iter()
                .take(protocol::control::MAX_PRESENCE_IDS)
                .filter(|id| valid_peer_id(id) && reg.is_online(id))
                .collect();
            Ok(Bridge::Presence { online })
        }
        _ => Err(ClassifyError::InvalidControl),
    }
}

/// The `PresenceResult` frame sent in answer to a `Presence` query.
pub fn presence_frame(online: Vec<String>) -> Vec<u8> {
    frame::wrap(Tag::Control, &Control::PresenceResult { online }.to_bytes())
}

/// Builds the `error` Control frame sent before closing a rejected socket.
pub fn error_frame(code: &str) -> Vec<u8> {
    frame::wrap(Tag::Control, &Control::Error { code: code.into() }.to_bytes())
}

/// Builds the `paired` Control frame sent to each side once bridged.
pub fn paired_frame(peer_role: &str, peer_id: &str) -> Vec<u8> {
    frame::wrap(
        Tag::Control,
        &Control::Paired {
            session: "s1".into(),
            peer_role: peer_role.into(),
            peer_id: peer_id.into(),
        }
        .to_bytes(),
    )
}

#[cfg(test)]
mod tests {
    use super::{decode_token, paired_frame, valid_peer_id};
    use protocol::{control::Control, frame, Tag};

    #[test]
    fn pairing_frame_carries_requester_id() {
        let bytes = paired_frame("host", "viewer-1");
        let (tag, body) = frame::unwrap(&bytes).unwrap();
        assert_eq!(tag, Tag::Control);
        assert_eq!(Control::from_bytes(body), Some(Control::Paired {
            session: "s1".into(), peer_role: "host".into(), peer_id: "viewer-1".into(),
        }));
    }

    #[test]
    fn register_validation_accepts_only_bounded_wire_values() {
        assert!(valid_peer_id("123456789"));
        assert!(!valid_peer_id("123-456-789"));
        assert!(!valid_peer_id("12345678"));
        assert!(decode_token(&"ab".repeat(32)).is_some());
        assert!(decode_token(&"AB".repeat(32)).is_none());
        assert!(decode_token(&"gg".repeat(32)).is_none());
        assert!(decode_token("00").is_none());
    }

    #[test]
    fn presence_frame_carries_the_online_ids() {
        let bytes = super::presence_frame(vec!["123456789".into()]);
        let (tag, body) = frame::unwrap(&bytes).unwrap();
        assert_eq!(tag, Tag::Control);
        assert_eq!(Control::from_bytes(body), Some(Control::PresenceResult { online: vec!["123456789".into()] }));
    }

    #[test]
    fn presence_reports_only_registered_online_ids() {
        use crate::registry::{Registration, Registry, RegistryConfig};
        use std::net::{IpAddr, Ipv4Addr};
        use tokio::sync::{mpsc, oneshot, watch};

        let source = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));
        let reg = Registry::new(RegistryConfig::default());
        let (htx, _hrx) = mpsc::channel(1);
        let htx = crate::registry::Outbound::for_test(htx);
        let (hclose, _hclose_rx) = watch::channel(None);
        let (hnotify, _hwait) = oneshot::channel();
        assert!(matches!(reg.register("123456789".into(), [1u8; 32], source, htx, hclose, hnotify), Registration::Accepted));

        // Asks about the online id, an offline id, and a malformed one.
        let query = frame::wrap(Tag::Control, &Control::Presence {
            ids: vec!["123456789".into(), "987654321".into(), "bad".into()],
        }.to_bytes());
        let (me, _me_rx) = mpsc::channel(1);
        let me = crate::registry::Outbound::for_test(me);
        let (me_close, _me_close_rx) = watch::channel(None);
        match super::classify(&query, me, me_close, &reg, source).expect("presence classifies") {
            super::Bridge::Presence { online } => assert_eq!(online, vec!["123456789".to_string()]),
            _ => panic!("expected a presence bridge"),
        }
    }
}

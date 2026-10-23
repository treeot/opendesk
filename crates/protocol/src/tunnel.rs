//! TCP port forwarding over the data channel.
//!
//! The viewer opens a local TCP listener; each accepted connection becomes a
//! tunnel stream (`Open`), and the host dials the loopback port on its own
//! machine and relays bytes. Many streams are multiplexed by a `u64` id. A
//! compact binary codec keeps the per-chunk overhead tiny, since `Data` carries
//! raw TCP bytes.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TunnelMsg {
    /// Viewer -> host: open a stream to `127.0.0.1:port` on the host.
    Open { id: u64, port: u16 },
    /// Host -> viewer: the stream connected.
    OpenOk { id: u64 },
    /// Host -> viewer: the stream could not connect; `reason` is for logs.
    OpenErr { id: u64, reason: String },
    /// Either side: payload bytes for `id`.
    Data { id: u64, bytes: Vec<u8> },
    /// Either side: `id` is closed (EOF or error).
    Close { id: u64 },
}

impl TunnelMsg {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Self::Open { id, port } => {
                out.push(0);
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(&port.to_be_bytes());
            }
            Self::OpenOk { id } => {
                out.push(1);
                out.extend_from_slice(&id.to_be_bytes());
            }
            Self::OpenErr { id, reason } => {
                out.push(2);
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(reason.as_bytes());
            }
            Self::Data { id, bytes } => {
                out.push(3);
                out.extend_from_slice(&id.to_be_bytes());
                out.extend_from_slice(bytes);
            }
            Self::Close { id } => {
                out.push(4);
                out.extend_from_slice(&id.to_be_bytes());
            }
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let (tag, rest) = bytes.split_first()?;
        let id = |b: &[u8]| -> Option<u64> { Some(u64::from_be_bytes(b.get(..8)?.try_into().ok()?)) };
        match tag {
            0 => {
                let id = id(rest)?;
                let port = u16::from_be_bytes(rest.get(8..10)?.try_into().ok()?);
                Some(Self::Open { id, port })
            }
            1 => Some(Self::OpenOk { id: id(rest)? }),
            2 => {
                let id = id(rest)?;
                let reason = String::from_utf8(rest.get(8..)?.to_vec()).ok()?;
                Some(Self::OpenErr { id, reason })
            }
            3 => Some(Self::Data { id: id(rest)?, bytes: rest.get(8..)?.to_vec() }),
            4 => Some(Self::Close { id: id(rest)? }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_variant() {
        for message in [
            TunnelMsg::Open { id: 7, port: 11434 },
            TunnelMsg::OpenOk { id: 7 },
            TunnelMsg::OpenErr { id: 7, reason: "refused".into() },
            TunnelMsg::Data { id: 7, bytes: vec![0, 1, 2, 255] },
            TunnelMsg::Close { id: 7 },
        ] {
            assert_eq!(TunnelMsg::from_bytes(&message.to_bytes()), Some(message.clone()), "{message:?}");
        }
    }

    #[test]
    fn rejects_truncated_input() {
        assert!(TunnelMsg::from_bytes(&[]).is_none());
        assert!(TunnelMsg::from_bytes(&[0, 1, 2]).is_none()); // open without full id/port
        assert!(TunnelMsg::from_bytes(&[9]).is_none()); // unknown tag
    }
}

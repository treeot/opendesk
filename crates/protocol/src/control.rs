use serde::{Deserialize, Serialize};

/// `Control::Error` code the relay sends when a `register` names an ID that is
/// leased to a different token.
pub const ERROR_ID_TAKEN: &str = "id_taken";
/// `Control::Error` code the relay sends when a `register` with the right
/// token keeps displacing another live connection with the same token, as
/// two copies of one device do. Older apps show it as a generic rejection.
pub const ERROR_ID_IN_USE: &str = "id_in_use";
/// `Control::Error` code the relay sends when a `connect` names an ID that is
/// not waiting for a viewer: offline, or already in a session.
pub const ERROR_PEER_UNAVAILABLE: &str = "peer_unavailable";
/// The relay has no room for another ID.
pub const ERROR_REGISTRY_FULL: &str = "registry_full";
/// Too many new IDs registered from this network (20 an hour by default).
pub const ERROR_NEW_ID_RATE: &str = "new_id_rate";
/// Too many `connect`s from this network to one ID in a short time.
pub const ERROR_CONNECT_RATE: &str = "connect_rate";
/// The relay is at its connection limit, or this network opened too many.
pub const ERROR_RELAY_BUSY: &str = "relay_busy";
/// A `register` the relay can't accept (bad ID, token or version).
pub const ERROR_INVALID_REGISTER: &str = "invalid_register";
/// A `connect` the relay can't accept (bad ID or version).
pub const ERROR_INVALID_CONNECT: &str = "invalid_connect";
/// A first message that is neither `register` nor `connect`.
pub const ERROR_INVALID_CONTROL: &str = "invalid_control";
// Relays older than these codes just close the socket. Apps that don't know
// a code treat it like that close: a generic rejection they retry.

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Control {
    Register { peer_id: String, token: String, ver: u32 },
    Connect { target: String, peer_id: String, ver: u32 },
    Paired { session: String, peer_role: String, peer_id: String },
    Ice { session: String, cand: String },
    Error { code: String },
    Bye { session: String },
    MonitorList { monitors: Vec<MonitorInfo> },
    SelectMonitor { id: u32 },
    AuthCode { code: String },
    /// Sealed liveness probe both peers send periodically on the relay socket
    /// after authentication. It keeps idle relay sockets open (a direct
    /// session carries no relay traffic otherwise) and lets each side notice
    /// a vanished peer when keepalives stop arriving.
    KeepAlive,
    /// Viewer → host: the decoder lost its reference (a gap or decode error),
    /// so the host should send a keyframe as soon as it can.
    RequestKeyframe,
    /// Viewer → host on the relay socket, to measure the relay round trip.
    /// `sent_us` is on the viewer's clock; the host echoes it in a `Pong`.
    /// beta.5 and older peers cannot parse it and ignore it, so the viewer
    /// just never gets an answer from them.
    Ping { sent_us: u64 },
    /// Host → viewer: the answer to a `Ping`, echoing its `sent_us`.
    Pong { sent_us: u64 },
    /// Host → viewer, after the first `MonitorList`: this host lets the
    /// viewer add up to `max` virtual displays. Hosts that can't, or don't
    /// allow it, never send it, so viewers hide the option by default.
    VirtualDisplaySupport { max: u32 },
    /// Viewer → host: add a virtual display of `width` x `height` pixels.
    /// With `hidpi` the display runs at 2x, so it is half that size in
    /// points. Only sent to hosts that sent `VirtualDisplaySupport`.
    AddVirtualDisplay {
        width: u32,
        height: u32,
        #[serde(default)]
        hidpi: bool,
    },
    /// Viewer → host: remove a virtual display this session added.
    RemoveVirtualDisplay { id: u32 },
    /// Host → viewer: the display from `AddVirtualDisplay` is up and being
    /// shared. A `MonitorList` that includes it comes first.
    VirtualDisplayAdded { id: u32 },
    /// Host → viewer: an add or remove request failed; `code` is one of
    /// [`VIRTUAL_DISPLAY_LIMIT`] and friends, or a newer code.
    VirtualDisplayRefused { code: String },
    /// Viewer → host: deliver the secure attention sequence (Ctrl+Alt+Del) on
    /// the host, so the viewer can reach the Windows security screen. Hosts
    /// that can't honor it ignore it; beta.5 peers can't parse it.
    RequestSecureAttention,
    /// Host → viewer, after the first `MonitorList`: this host can deliver the
    /// secure attention sequence, so the viewer may show a Ctrl+Alt+Del
    /// control. Hosts that can't never send it, so viewers hide the control.
    SecureAttentionSupport,
    /// Host → viewer, after the first `MonitorList`: this host can stream extra
    /// monitors as separate aux streams, up to `max` at once, so the viewer
    /// can open each monitor in its own window. Hosts that can't never send
    /// it; beta.8 peers can't parse it and ignore it.
    MultiMonitorSupport { max: u32 },
    /// Viewer → host: start streaming monitor `id` as a separate aux stream,
    /// alongside the main view. Only sent to hosts that sent
    /// `MultiMonitorSupport`.
    OpenMonitorStream { id: u32 },
    /// Viewer → host: stop the aux stream for monitor `id`.
    CloseMonitorStream { id: u32 },
    /// Host → viewer: the aux stream for monitor `id` is up; its frames arrive
    /// on the aux video channel tagged for that monitor.
    MonitorStreamOpened { id: u32 },
    /// Host → viewer: the aux stream for monitor `id` has stopped.
    MonitorStreamClosed { id: u32 },
    /// Host → viewer: an `OpenMonitorStream` failed; `code` is one of the
    /// `MONITOR_STREAM_*` codes, or a newer one.
    MonitorStreamRefused { id: u32, code: String },
    /// Viewer → host: point input at monitor `id` without changing which
    /// monitor the main view captures. A per-monitor window sends it so its
    /// clicks and keys land on its own monitor; the main view sends it to take
    /// input back. Unlike `SelectMonitor`, it never switches capture.
    SetInputMonitor { id: u32 },
    /// Viewer → relay as a first message: which of these IDs have a host
    /// online right now. The relay answers `PresenceResult` and closes. It is
    /// rate-limited like any connection and the list is capped, so it cannot be
    /// used to scan the ID space cheaply.
    Presence { ids: Vec<String> },
    /// Relay → viewer: the subset of a `Presence` query whose hosts are online.
    PresenceResult { online: Vec<String> },
    /// Host → viewer, after the first `MonitorList`: this host can restart the
    /// computer and come back hosting (unattended access on, and the platform
    /// can reboot without a prompt). Hosts that can't never send it, so viewers
    /// hide the restart control.
    RestartSupport,
    /// Viewer → host: restart the host computer, then let me reconnect. Only
    /// sent to hosts that sent `RestartSupport`.
    RequestRestart,
    /// Host → viewer: I am about to restart; expect a longer gap before I come
    /// back. The viewer keeps retrying for the restart window instead of the
    /// short transient one.
    Restarting,
    /// Host → viewer, after the first `MonitorList`: this host can turn on
    /// privacy mode (blank its physical screen and ignore its local keyboard
    /// and mouse during the session). Hosts that can't never send it, so
    /// viewers hide the control.
    PrivacyModeSupport,
    /// Viewer → host: turn privacy mode on or off. Only sent to hosts that sent
    /// `PrivacyModeSupport`. The host always restores normal input and display
    /// when the session ends.
    SetPrivacyMode { on: bool },
    /// Host → viewer: privacy mode is now on or off (the real state after the
    /// host tried to apply it).
    PrivacyModeChanged { on: bool },
    /// Viewer → host: cap the stream bitrate at `bps`, or `0` for no cap (the
    /// host's automatic, bandwidth-following behavior). The host still adapts
    /// below the cap. Older peers can't parse it and ignore it, so the stream
    /// stays automatic.
    SetMaxBitrate { bps: u32 },
    /// Host → viewer: the host's mouse cursor image changed. `rgba` is
    /// `width * height * 4` bytes of straight-alpha RGBA, top row first. The
    /// viewer draws it as its own pointer over the remote screen, so the shape
    /// (arrow, text beam, resize, hand) matches the host without waiting on the
    /// video frame. `hot_x`/`hot_y` are the click point within the image. Sent
    /// only when the shape changes; older peers ignore it.
    CursorShape { width: u16, height: u16, hot_x: u16, hot_y: u16, rgba: Vec<u8> },
    /// Host → viewer: the host hid its cursor (for example a game captured the
    /// mouse). The viewer hides its pointer over the remote screen until the
    /// next `CursorShape`.
    CursorHidden,
    /// Viewer → host: which codecs this viewer can decode beyond the baseline
    /// H.264. `hevc` lets the host switch the stream to HEVC where its hardware
    /// can encode it; the host falls back to H.264 otherwise and the frame
    /// header always says which codec a frame actually uses. Older hosts ignore
    /// it and stay on H.264.
    DecoderCodecs { hevc: bool },
}

/// Most IDs one `Presence` query may ask about.
pub const MAX_PRESENCE_IDS: usize = 50;

/// `MonitorStreamRefused` codes.
pub const MONITOR_STREAM_LIMIT: &str = "limit";
pub const MONITOR_STREAM_UNKNOWN: &str = "unknown_display";
pub const MONITOR_STREAM_FAILED: &str = "failed";

/// `VirtualDisplayRefused` codes.
pub const VIRTUAL_DISPLAY_LIMIT: &str = "limit";
pub const VIRTUAL_DISPLAY_NOT_ALLOWED: &str = "not_allowed";
pub const VIRTUAL_DISPLAY_FAILED: &str = "failed";
pub const VIRTUAL_DISPLAY_UNKNOWN: &str = "unknown_display";
/// The host can only remove its newest virtual display (Windows).
pub const VIRTUAL_DISPLAY_NOT_NEWEST: &str = "not_newest";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorInfo {
    pub id: u32,
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub is_primary: bool,
    /// A virtual display this session added, which the viewer may remove.
    /// beta.5 peers neither send nor read it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_virtual: bool,
}

impl Control {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("control serializes")
    }
    pub fn from_bytes(b: &[u8]) -> Option<Control> {
        serde_json::from_slice(b).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_roundtrips() {
        let c = Control::Register { peer_id: "123-456-789".into(), token: "00".repeat(32), ver: 1 };
        let bytes = c.to_bytes();
        assert_eq!(Control::from_bytes(&bytes), Some(c));
    }

    #[test]
    fn wire_uses_t_discriminator() {
        let c = Control::Connect { target: "1".into(), peer_id: "2".into(), ver: 1 };
        let json = String::from_utf8(c.to_bytes()).unwrap();
        assert!(json.contains("\"t\":\"connect\""));
    }

    #[test]
    fn paired_with_peer_id_roundtrips() {
        let c = Control::Paired { session: "s1".into(), peer_role: "host".into(), peer_id: "987654321".into() };
        assert_eq!(Control::from_bytes(&c.to_bytes()), Some(c));
    }

    #[test]
    fn multi_monitor_messages_roundtrip() {
        for c in [
            Control::MultiMonitorSupport { max: 3 },
            Control::OpenMonitorStream { id: 7 },
            Control::CloseMonitorStream { id: 7 },
            Control::MonitorStreamOpened { id: 7 },
            Control::MonitorStreamClosed { id: 7 },
            Control::MonitorStreamRefused { id: 7, code: MONITOR_STREAM_LIMIT.into() },
            Control::SetInputMonitor { id: 7 },
            Control::Presence { ids: vec!["123456789".into(), "987654321".into()] },
            Control::PresenceResult { online: vec!["123456789".into()] },
            Control::RestartSupport,
            Control::RequestRestart,
            Control::Restarting,
            Control::PrivacyModeSupport,
            Control::SetPrivacyMode { on: true },
            Control::PrivacyModeChanged { on: false },
        ] {
            assert_eq!(Control::from_bytes(&c.to_bytes()), Some(c));
        }
    }

    #[test]
    fn ice_roundtrips() {
        let c = Control::Ice {
            session: "s1".into(),
            cand: "4257992714 1 udp 2130706431 10.2.0.2 57012 typ host".into(),
        };
        let bytes = c.to_bytes();
        assert_eq!(Control::from_bytes(&bytes), Some(c));
    }

    #[test]
    fn ice_uses_t_discriminator() {
        let c = Control::Ice {
            session: "s1".into(),
            cand: "candidate".into(),
        };
        let json = String::from_utf8(c.to_bytes()).unwrap();
        assert!(json.contains("\"t\":\"ice\""));
    }

    #[test]
    fn monitor_list_roundtrips() {
        let c = Control::MonitorList {
            monitors: vec![MonitorInfo {
                id: 42,
                name: "Built-in Display".into(),
                width: 2560,
                height: 1600,
                is_primary: true,
                is_virtual: false,
            }],
        };
        let json = String::from_utf8(c.to_bytes()).unwrap();
        assert!(!json.contains("is_virtual"), "beta.5 wire shape for real displays: {json}");
        assert_eq!(Control::from_bytes(&c.to_bytes()), Some(c));
    }

    #[test]
    fn virtual_display_messages_roundtrip() {
        for c in [
            Control::VirtualDisplaySupport { max: 2 },
            Control::AddVirtualDisplay { width: 2560, height: 1440, hidpi: true },
            Control::RemoveVirtualDisplay { id: 9 },
            Control::VirtualDisplayAdded { id: 9 },
            Control::VirtualDisplayRefused { code: VIRTUAL_DISPLAY_LIMIT.into() },
        ] {
            assert_eq!(Control::from_bytes(&c.to_bytes()), Some(c.clone()), "{c:?}");
        }
        assert_eq!(
            String::from_utf8(Control::AddVirtualDisplay { width: 1280, height: 720, hidpi: false }.to_bytes()).unwrap(),
            r#"{"t":"add_virtual_display","width":1280,"height":720,"hidpi":false}"#
        );
        assert_eq!(
            Control::from_bytes(br#"{"t":"add_virtual_display","width":1280,"height":720}"#),
            Some(Control::AddVirtualDisplay { width: 1280, height: 720, hidpi: false })
        );
    }

    #[test]
    fn virtual_monitors_are_flagged_and_old_lists_read_as_real() {
        let virt = MonitorInfo { id: 5, name: "OpenDesk".into(), width: 0, height: 0, is_primary: false, is_virtual: true };
        let c = Control::MonitorList { monitors: vec![virt] };
        assert_eq!(Control::from_bytes(&c.to_bytes()), Some(c));
        // A beta.5 host's list has no flag.
        let old = br#"{"t":"monitor_list","monitors":[{"id":1,"name":"A","width":0,"height":0,"is_primary":true}]}"#;
        let Some(Control::MonitorList { monitors }) = Control::from_bytes(old) else { panic!("old list unreadable") };
        assert!(!monitors[0].is_virtual);
    }

    #[test]
    fn select_monitor_roundtrips() {
        let c = Control::SelectMonitor { id: 7 };
        assert_eq!(Control::from_bytes(&c.to_bytes()), Some(c));
    }

    #[test]
    fn set_max_bitrate_roundtrips_and_old_peers_ignore_it() {
        let c = Control::SetMaxBitrate { bps: 8_000_000 };
        assert_eq!(String::from_utf8(c.to_bytes()).unwrap(), r#"{"t":"set_max_bitrate","bps":8000000}"#);
        assert_eq!(Control::from_bytes(&c.to_bytes()), Some(c));
        assert_eq!(Control::from_bytes(&Control::SetMaxBitrate { bps: 0 }.to_bytes()), Some(Control::SetMaxBitrate { bps: 0 }));
    }

    #[test]
    fn cursor_messages_roundtrip() {
        let shape = Control::CursorShape { width: 2, height: 1, hot_x: 0, hot_y: 0, rgba: vec![1, 2, 3, 4, 5, 6, 7, 8] };
        assert_eq!(Control::from_bytes(&shape.to_bytes()), Some(shape));
        assert_eq!(Control::from_bytes(&Control::CursorHidden.to_bytes()), Some(Control::CursorHidden));
    }

    #[test]
    fn decoder_codecs_roundtrips() {
        let c = Control::DecoderCodecs { hevc: true };
        assert_eq!(String::from_utf8(c.to_bytes()).unwrap(), r#"{"t":"decoder_codecs","hevc":true}"#);
        assert_eq!(Control::from_bytes(&c.to_bytes()), Some(c));
    }

    #[test]
    fn auth_code_roundtrips() {
        let c = Control::AuthCode { code: "123456".into() };
        assert_eq!(Control::from_bytes(&c.to_bytes()), Some(c));
    }

    #[test]
    fn request_keyframe_roundtrips() {
        let bytes = Control::RequestKeyframe.to_bytes();
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), r#"{"t":"request_keyframe"}"#);
        assert_eq!(Control::from_bytes(&bytes), Some(Control::RequestKeyframe));
    }

    #[test]
    fn keepalive_roundtrips_and_uses_t_discriminator() {
        let bytes = Control::KeepAlive.to_bytes();
        assert_eq!(String::from_utf8(bytes.clone()).unwrap(), r#"{"t":"keep_alive"}"#);
        assert_eq!(Control::from_bytes(&bytes), Some(Control::KeepAlive));
    }

    #[test]
    fn ping_and_pong_roundtrip() {
        let ping = Control::Ping { sent_us: 1_234_567 };
        assert_eq!(String::from_utf8(ping.to_bytes()).unwrap(), r#"{"t":"ping","sent_us":1234567}"#);
        assert_eq!(Control::from_bytes(&ping.to_bytes()), Some(ping));
        let pong = Control::Pong { sent_us: 7 };
        assert_eq!(String::from_utf8(pong.to_bytes()).unwrap(), r#"{"t":"pong","sent_us":7}"#);
        assert_eq!(Control::from_bytes(&pong.to_bytes()), Some(pong));
    }

    /// The control enum as v0.1.0-beta.5 shipped it. Old peers parse sealed
    /// control with this, and anything that fails to parse falls through to
    /// a `_` arm in their session loops.
    mod beta5 {
        use serde::Deserialize;

        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        pub struct MonitorInfo { pub id: u32, pub name: String, pub width: u32, pub height: u32, pub is_primary: bool }

        #[derive(Debug, Deserialize)]
        #[serde(tag = "t", rename_all = "snake_case")]
        #[allow(dead_code)]
        pub enum Control {
            Register { peer_id: String, token: String, ver: u32 },
            Connect { target: String, peer_id: String, ver: u32 },
            Paired { session: String, peer_role: String, peer_id: String },
            Ice { session: String, cand: String },
            Error { code: String },
            Bye { session: String },
            MonitorList { monitors: Vec<MonitorInfo> },
            SelectMonitor { id: u32 },
            AuthCode { code: String },
            KeepAlive,
            RequestKeyframe,
        }

        pub fn from_bytes(b: &[u8]) -> Option<Control> { serde_json::from_slice(b).ok() }
    }

    #[test]
    fn beta5_peers_skip_ping_and_pong_but_read_everything_else() {
        assert!(beta5::from_bytes(&Control::Ping { sent_us: 1 }.to_bytes()).is_none());
        assert!(beta5::from_bytes(&Control::Pong { sent_us: 1 }.to_bytes()).is_none());
        for known in [Control::KeepAlive, Control::RequestKeyframe, Control::Bye { session: String::new() }, Control::SelectMonitor { id: 2 }] {
            assert!(beta5::from_bytes(&known.to_bytes()).is_some(), "{known:?}");
        }
    }

    #[test]
    fn beta5_peers_skip_virtual_display_messages_but_read_flagged_lists() {
        for newer in [
            Control::VirtualDisplaySupport { max: 2 },
            Control::AddVirtualDisplay { width: 1920, height: 1080, hidpi: false },
            Control::RemoveVirtualDisplay { id: 3 },
            Control::VirtualDisplayAdded { id: 3 },
            Control::VirtualDisplayRefused { code: VIRTUAL_DISPLAY_FAILED.into() },
            Control::RequestSecureAttention,
            Control::SecureAttentionSupport,
        ] {
            assert!(beta5::from_bytes(&newer.to_bytes()).is_none(), "{newer:?}");
        }
        let list = Control::MonitorList {
            monitors: vec![MonitorInfo { id: 3, name: "OpenDesk".into(), width: 0, height: 0, is_primary: false, is_virtual: true }],
        };
        assert!(matches!(beta5::from_bytes(&list.to_bytes()), Some(beta5::Control::MonitorList { .. })));
    }

    #[test]
    fn unknown_kinds_from_newer_peers_are_ignored() {
        assert_eq!(Control::from_bytes(br#"{"t":"something_newer","x":1}"#), None);
    }

    #[test]
    fn bye_roundtrips_and_uses_t_discriminator() {
        let c = Control::Bye { session: "s1".into() };
        let bytes = c.to_bytes();
        assert!(String::from_utf8(bytes.clone()).unwrap().contains("\"t\":\"bye\""));
        assert_eq!(Control::from_bytes(&bytes), Some(c));
    }
}

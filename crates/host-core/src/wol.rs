//! Wake-on-LAN: build and send a magic packet so a sleeping device on the
//! local network powers on before you connect to it.
//!
//! A magic packet is 6 bytes of `0xFF` followed by the target's 6-byte MAC
//! address repeated 16 times (102 bytes total), sent as a UDP broadcast. The
//! target's network card watches for it while the machine is asleep. Wake-on-LAN
//! only reaches devices on the same broadcast domain (same LAN/subnet), so this
//! runs from the viewer's own network, not through the relay.

use std::io;
use std::net::{SocketAddr, UdpSocket};

/// The discard port magic packets are conventionally sent to. Port 7 and 9 are
/// both common; the packet is matched by its contents, not the port.
const WOL_PORT: u16 = 9;

/// Parse a MAC address written with `:`, `-`, or `.` separators, or as 12 bare
/// hex digits, into its six bytes. Case-insensitive.
pub fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let hex: String = mac.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 12 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// Build the 102-byte magic packet for a MAC address.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xFFu8; 102];
    for chunk in packet[6..].chunks_mut(6) {
        chunk.copy_from_slice(&mac);
    }
    packet
}

/// Send a magic packet to wake the device with this MAC on the local network.
/// Broadcasts to `255.255.255.255`, which reaches the whole local subnet.
pub fn wake(mac: &str) -> io::Result<()> {
    let mac = parse_mac(mac)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid MAC address"))?;
    let packet = magic_packet(mac);
    // Bind to any local address/port; the OS picks one.
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_broadcast(true)?;
    let target = SocketAddr::from(([255, 255, 255, 255], WOL_PORT));
    socket.send_to(&packet, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_colon_separated_mac() {
        assert_eq!(parse_mac("01:23:45:67:89:AB"), Some([0x01, 0x23, 0x45, 0x67, 0x89, 0xAB]));
    }

    #[test]
    fn parses_dash_and_bare_and_lowercase() {
        let want = Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]);
        assert_eq!(parse_mac("aa-bb-cc-dd-ee-ff"), want);
        assert_eq!(parse_mac("aabbccddeeff"), want);
        assert_eq!(parse_mac("AABB.CCDD.EEFF"), want);
    }

    #[test]
    fn rejects_wrong_length_and_non_hex() {
        assert_eq!(parse_mac("01:23:45:67:89"), None);
        assert_eq!(parse_mac("zz:23:45:67:89:ab"), None);
        assert_eq!(parse_mac(""), None);
    }

    #[test]
    fn magic_packet_has_header_and_sixteen_repeats() {
        let mac = [0x01, 0x23, 0x45, 0x67, 0x89, 0xAB];
        let packet = magic_packet(mac);
        assert_eq!(&packet[..6], &[0xFF; 6]);
        for i in 0..16 {
            let start = 6 + i * 6;
            assert_eq!(&packet[start..start + 6], &mac);
        }
    }
}

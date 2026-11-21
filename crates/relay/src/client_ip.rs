//! Client address resolution behind trusted reverse proxies.
//!
//! The relay normally runs on a private Docker network behind Caddy, Traefik
//! or similar, so the TCP peer is the proxy. When (and only when) the peer is
//! a trusted proxy, the client address is the rightmost untrusted entry of
//! `X-Forwarded-For` (falling back to `X-Real-IP`). Entries to the left of
//! that one were supplied by the client and are ignored, so a client cannot
//! pick its own rate-limit bucket.

use axum::http::HeaderMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Default trusted ranges: loopback plus RFC 1918 and IPv6 ULA private ranges.
pub const DEFAULT_TRUSTED_PROXIES: &str = "127.0.0.0/8,::1/128,10.0.0.0/8,172.16.0.0/12,192.168.0.0/16,fc00::/7";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cidr { network: IpAddr, prefix: u8 }

impl Cidr {
    /// Parses `addr/prefix` or a bare address (a single-host range).
    pub fn parse(value: &str) -> Option<Self> {
        let value = value.trim();
        let (addr, prefix) = match value.split_once('/') {
            Some((addr, prefix)) => (addr.trim().parse::<IpAddr>().ok()?, Some(prefix.trim().parse::<u8>().ok()?)),
            None => (value.parse::<IpAddr>().ok()?, None),
        };
        let addr = canonical(addr);
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        if prefix > max { return None; }
        Some(Self { network: mask(addr, prefix), prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        ip.is_ipv4() == self.network.is_ipv4() && mask(ip, self.prefix) == self.network
    }
}

fn canonical(ip: IpAddr) -> IpAddr {
    match ip { IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4), v4 => v4 }
}

fn mask(ip: IpAddr, prefix: u8) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let bits = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) };
            IpAddr::V4(Ipv4Addr::from(u32::from(v4) & bits))
        }
        IpAddr::V6(v6) => {
            let bits = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix as u32) };
            IpAddr::V6(Ipv6Addr::from(u128::from(v6) & bits))
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TrustedProxies { ranges: Vec<Cidr> }

impl TrustedProxies {
    /// Parses a comma-separated CIDR list. `none` (or an empty list) trusts
    /// nothing. Invalid entries are returned separately so the caller can
    /// log them.
    pub fn parse(list: &str) -> (Self, Vec<String>) {
        let mut ranges = Vec::new();
        let mut invalid = Vec::new();
        for entry in list.split(',').map(str::trim).filter(|entry| !entry.is_empty()) {
            if entry.eq_ignore_ascii_case("none") { continue; }
            match Cidr::parse(entry) { Some(cidr) => ranges.push(cidr), None => invalid.push(entry.to_owned()) }
        }
        (Self { ranges }, invalid)
    }

    pub fn is_trusted(&self, ip: IpAddr) -> bool { self.ranges.iter().any(|range| range.contains(ip)) }

    /// The address used for per-IP limits and logs.
    pub fn client_ip(&self, peer: IpAddr, headers: &HeaderMap) -> IpAddr {
        let peer = canonical(peer);
        if !self.is_trusted(peer) { return peer; }
        let forwarded: Vec<&str> = headers.get_all("x-forwarded-for").iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .collect();
        if !forwarded.is_empty() {
            // Walk right to left through the proxy chain; the first address
            // not belonging to a trusted proxy is the client. If every hop is
            // trusted the client itself is on a private network, so the
            // leftmost valid hop is the best answer. An unparsable entry ends
            // the walk: nothing to its left can be trusted.
            let mut client = peer;
            for entry in forwarded.iter().rev() {
                let Some(ip) = parse_forwarded_ip(entry) else { break };
                client = ip;
                if !self.is_trusted(ip) { break; }
            }
            return client;
        }
        headers.get("x-real-ip").and_then(|value| value.to_str().ok()).and_then(parse_forwarded_ip).unwrap_or(peer)
    }
}

fn parse_forwarded_ip(value: &str) -> Option<IpAddr> {
    let value = value.trim();
    value.parse::<IpAddr>().ok()
        .or_else(|| value.parse::<SocketAddr>().ok().map(|addr| addr.ip()))
        .map(canonical)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn ip(value: &str) -> IpAddr { value.parse().unwrap() }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs { map.append(*name, HeaderValue::from_str(value).unwrap()); }
        map
    }

    fn defaults() -> TrustedProxies { TrustedProxies::parse(DEFAULT_TRUSTED_PROXIES).0 }

    #[test]
    fn cidr_parsing_and_matching() {
        let net = Cidr::parse("172.16.0.0/12").unwrap();
        assert!(net.contains(ip("172.31.255.1")));
        assert!(!net.contains(ip("172.32.0.1")));
        assert!(Cidr::parse("10.1.2.3").unwrap().contains(ip("10.1.2.3")));
        assert!(!Cidr::parse("10.1.2.3").unwrap().contains(ip("10.1.2.4")));
        assert!(Cidr::parse("fc00::/7").unwrap().contains(ip("fd12::1")));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains(ip("8.8.8.8")));
        assert!(!Cidr::parse("0.0.0.0/0").unwrap().contains(ip("2001:db8::1")));
        assert!(Cidr::parse("10.0.0.0/8").unwrap().contains(ip("::ffff:10.0.0.1")));
        assert!(Cidr::parse("10.0.0.0/33").is_none());
        assert!(Cidr::parse("::/129").is_none());
        assert!(Cidr::parse("nope").is_none());
    }

    #[test]
    fn list_parsing_reports_invalid_entries() {
        let (proxies, invalid) = TrustedProxies::parse(" 10.0.0.0/8 , bad, ::1 ,");
        assert_eq!(invalid, vec!["bad".to_owned()]);
        assert!(proxies.is_trusted(ip("10.9.9.9")));
        assert!(proxies.is_trusted(ip("::1")));
        assert!(!proxies.is_trusted(ip("192.168.1.1")));
        let (none, invalid) = TrustedProxies::parse("none");
        assert!(invalid.is_empty());
        assert!(!none.is_trusted(ip("127.0.0.1")));
    }

    #[test]
    fn untrusted_peer_headers_are_ignored() {
        let map = headers(&[("x-forwarded-for", "1.2.3.4"), ("x-real-ip", "5.6.7.8")]);
        assert_eq!(defaults().client_ip(ip("203.0.113.9"), &map), ip("203.0.113.9"));
    }

    #[test]
    fn trusted_peer_uses_rightmost_untrusted_forwarded_address() {
        // The client prepended a spoofed address; the proxy appended the real one.
        let map = headers(&[("x-forwarded-for", "1.2.3.4, 198.51.100.7")]);
        assert_eq!(defaults().client_ip(ip("172.18.0.2"), &map), ip("198.51.100.7"));
        // Two proxy hops: skip the trusted inner proxy.
        let map = headers(&[("x-forwarded-for", "9.9.9.9, 198.51.100.7, 10.0.0.5")]);
        assert_eq!(defaults().client_ip(ip("10.0.0.6"), &map), ip("198.51.100.7"));
        // Repeated headers are read in order.
        let map = headers(&[("x-forwarded-for", "9.9.9.9"), ("x-forwarded-for", "198.51.100.7")]);
        assert_eq!(defaults().client_ip(ip("127.0.0.1"), &map), ip("198.51.100.7"));
    }

    #[test]
    fn garbage_in_forwarded_chain_stops_the_walk() {
        let map = headers(&[("x-forwarded-for", "1.2.3.4, junk, 10.0.0.9")]);
        assert_eq!(defaults().client_ip(ip("10.0.0.1"), &map), ip("10.0.0.9"));
        let map = headers(&[("x-forwarded-for", "junk")]);
        assert_eq!(defaults().client_ip(ip("10.0.0.1"), &map), ip("10.0.0.1"));
    }

    #[test]
    fn all_private_chain_returns_leftmost_hop() {
        let map = headers(&[("x-forwarded-for", "192.168.1.20, 10.0.0.5")]);
        assert_eq!(defaults().client_ip(ip("10.0.0.6"), &map), ip("192.168.1.20"));
    }

    #[test]
    fn real_ip_fallback_and_ports() {
        let map = headers(&[("x-real-ip", "198.51.100.7")]);
        assert_eq!(defaults().client_ip(ip("127.0.0.1"), &map), ip("198.51.100.7"));
        let map = headers(&[("x-forwarded-for", "[2001:db8::1]:443")]);
        assert_eq!(defaults().client_ip(ip("::1"), &map), ip("2001:db8::1"));
        assert_eq!(defaults().client_ip(ip("127.0.0.1"), &HeaderMap::new()), ip("127.0.0.1"));
    }
}

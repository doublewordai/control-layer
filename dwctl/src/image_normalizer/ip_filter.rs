//! IP address deny-list for the hardened image fetcher.
//!
//! The fetcher resolves a hostname to an IP, validates it against this
//! deny-list, then connects to that exact IP (DNS pinning). This blocks
//! outbound requests to private / loopback / link-local / cloud-metadata
//! ranges at the application layer, in addition to any NetworkPolicy-level
//! egress restrictions that apply at the pod boundary.
//!
//! Ranges denied:
//!
//! - RFC1918: `10/8`, `172.16/12`, `192.168/16`
//! - Loopback: `127/8`, `::1`
//! - Link-local: `169.254/16` (covers GCE metadata at `169.254.169.254`),
//!   IPv6 link-local `fe80::/10`
//! - CGNAT: `100.64/10`
//! - IPv6 unique-local: `fc00::/7`
//! - Multicast (v4 and v6)
//! - Unspecified (`0.0.0.0`, `::`)
//! - IPv4-mapped IPv6 addresses are unmapped before checking (so `::ffff:10.0.0.1`
//!   is recognised as RFC1918).
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Returns true if `ip` should be refused as a fetch target.
pub fn is_denied(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_denied_v4(v4),
        IpAddr::V6(v6) => {
            // Unmap IPv4-mapped IPv6 (e.g. ::ffff:10.0.0.1) so the v4
            // deny-list applies.
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_denied_v4(mapped);
            }
            is_denied_v6(v6)
        }
    }
}

fn is_denied_v4(ip: Ipv4Addr) -> bool {
    if ip.is_loopback() || ip.is_link_local() || ip.is_broadcast() || ip.is_multicast() || ip.is_unspecified() || ip.is_documentation() {
        return true;
    }
    // NOTE: std's `Ipv4Addr::is_private()` covers ONLY the three RFC1918
    // ranges (10/8, 172.16/12, 192.168/16). It deliberately does NOT
    // include CGNAT 100.64/10 (RFC 6598) — that range is denied
    // explicitly below.
    if ip.is_private() {
        return true; // covers 10/8, 172.16/12, 192.168/16
    }
    // CGNAT 100.64.0.0/10 (RFC 6598)
    let octets = ip.octets();
    if octets[0] == 100 && (octets[1] & 0b1100_0000) == 0b0100_0000 {
        return true;
    }
    // 0.0.0.0/8 — current network, treat as unspecified-ish
    if octets[0] == 0 {
        return true;
    }
    // 240.0.0.0/4 — reserved for future use
    if octets[0] >= 240 {
        return true;
    }
    false
}

fn is_denied_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    let segments = ip.segments();
    // fe80::/10 — link-local
    if (segments[0] & 0xffc0) == 0xfe80 {
        return true;
    }
    // fc00::/7 — unique-local
    if (segments[0] & 0xfe00) == 0xfc00 {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> IpAddr {
        IpAddr::V4(s.parse().unwrap())
    }
    fn v6(s: &str) -> IpAddr {
        IpAddr::V6(s.parse().unwrap())
    }

    #[test]
    fn allows_typical_public_v4() {
        assert!(!is_denied(v4("8.8.8.8")));
        assert!(!is_denied(v4("1.1.1.1")));
        assert!(!is_denied(v4("142.250.190.78")));
    }

    #[test]
    fn denies_loopback_v4() {
        assert!(is_denied(v4("127.0.0.1")));
        assert!(is_denied(v4("127.1.2.3")));
    }

    #[test]
    fn denies_rfc1918() {
        assert!(is_denied(v4("10.0.0.1")));
        assert!(is_denied(v4("10.255.255.255")));
        assert!(is_denied(v4("172.16.0.1")));
        assert!(is_denied(v4("172.31.255.255")));
        assert!(is_denied(v4("192.168.1.1")));
    }

    #[test]
    fn rfc1918_172_boundaries_correct() {
        // 172.15.x.x and 172.32.x.x are public
        assert!(!is_denied(v4("172.15.0.1")));
        assert!(!is_denied(v4("172.32.0.1")));
    }

    #[test]
    fn denies_link_local_including_metadata() {
        assert!(is_denied(v4("169.254.0.1")));
        assert!(is_denied(v4("169.254.169.254"))); // GCE / AWS metadata
        assert!(is_denied(v4("169.254.255.255")));
    }

    #[test]
    fn denies_cgnat() {
        // 100.64.0.0/10 → 100.64.0.0 .. 100.127.255.255
        assert!(is_denied(v4("100.64.0.1")));
        assert!(is_denied(v4("100.127.255.255")));
        // boundaries
        assert!(!is_denied(v4("100.63.255.255")));
        assert!(!is_denied(v4("100.128.0.0")));
    }

    #[test]
    fn denies_broadcast_multicast_unspecified() {
        assert!(is_denied(v4("0.0.0.0")));
        assert!(is_denied(v4("255.255.255.255")));
        assert!(is_denied(v4("224.0.0.1")));
    }

    #[test]
    fn denies_reserved_future() {
        assert!(is_denied(v4("240.0.0.1")));
        assert!(is_denied(v4("250.10.20.30")));
    }

    #[test]
    fn denies_v6_loopback() {
        assert!(is_denied(v6("::1")));
    }

    #[test]
    fn denies_v6_unspecified() {
        assert!(is_denied(v6("::")));
    }

    #[test]
    fn denies_v6_link_local() {
        assert!(is_denied(v6("fe80::1")));
        assert!(is_denied(v6("fe80::1234:5678:9abc:def0")));
    }

    #[test]
    fn denies_v6_unique_local() {
        assert!(is_denied(v6("fc00::1")));
        assert!(is_denied(v6("fd12:3456::1")));
    }

    #[test]
    fn denies_v6_multicast() {
        assert!(is_denied(v6("ff02::1")));
    }

    #[test]
    fn allows_typical_public_v6() {
        assert!(!is_denied(v6("2001:4860:4860::8888"))); // Google DNS
        assert!(!is_denied(v6("2606:4700:4700::1111"))); // Cloudflare DNS
    }

    #[test]
    fn unmaps_v4_mapped_v6_to_apply_v4_rules() {
        // ::ffff:10.0.0.1 — IPv4-mapped form of an RFC1918 address
        assert!(is_denied(v6("::ffff:10.0.0.1")));
        assert!(is_denied(v6("::ffff:169.254.169.254")));
        // ::ffff:8.8.8.8 — mapped form of public v4, should be allowed
        assert!(!is_denied(v6("::ffff:8.8.8.8")));
    }
}

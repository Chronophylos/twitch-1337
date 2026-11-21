//! Public-vs-special-purpose IP classification, shared by the web fetcher's
//! SSRF guard, the RDAP redirect policy and the `lookup_network` tool's
//! input gate.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// True for every address that is not globally routable unicast: private,
/// loopback, link-local, CGNAT, multicast, broadcast, unspecified,
/// documentation, benchmarking and reserved space, v4 and v6.
///
/// IPv6 forms that embed an IPv4 address (v4-mapped `::ffff:0:0/96`, NAT64
/// `64:ff9b::/96`, 6to4 `2002::/16`) take the embedded address's verdict.
pub fn is_non_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_non_public_v4(v4),
        IpAddr::V6(v6) => is_non_public_v6(v6),
    }
}

fn is_non_public_v4(v4: Ipv4Addr) -> bool {
    let [a, b, c, _] = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_broadcast()
        || v4.is_unspecified()
        || v4.is_documentation()
        || v4.is_multicast()
        || is_cgnat(v4)
        // 0.0.0.0/8 "this network"
        || a == 0
        // 192.0.0.0/24 IETF protocol assignments
        || (a == 192 && b == 0 && c == 0)
        // 192.88.99.0/24 deprecated 6to4 relay anycast
        || (a == 192 && b == 88 && c == 99)
        // 198.18.0.0/15 benchmarking
        || (a == 198 && (b & 0xFE) == 18)
        // 240.0.0.0/4 reserved (incl. broadcast)
        || a >= 240
}

fn is_non_public_v6(v6: Ipv6Addr) -> bool {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_non_public_v4(v4);
    }
    let seg = v6.segments();
    let embedded = |hi: u16, lo: u16| {
        let [h1, h2] = hi.to_be_bytes();
        let [l1, l2] = lo.to_be_bytes();
        is_non_public_v4(Ipv4Addr::new(h1, h2, l1, l2))
    };
    // NAT64 well-known prefix 64:ff9b::/96, v4 in the low 32 bits.
    if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
        return embedded(seg[6], seg[7]);
    }
    // Global unicast is 2000::/3. Everything else is special or unassigned:
    // `::`, `::1`, ULA fc00::/7, link-local fe80::/10, multicast ff00::/8,
    // discard 100::/64, local-use NAT64 64:ff9b:1::/48, reserved space.
    if seg[0] & 0xE000 != 0x2000 {
        return true;
    }
    // 2001:db8::/32 documentation
    (seg[0] == 0x2001 && seg[1] == 0x0db8)
        // 3fff::/20 documentation (RFC 9637)
        || (seg[0] == 0x3fff && seg[1] & 0xF000 == 0)
        // 2001:2::/48 benchmarking
        || (seg[0] == 0x2001 && seg[1] == 0x0002 && seg[2] == 0)
        // 6to4 2002::/16, v4 in bits 16..48
        || (seg[0] == 0x2002 && embedded(seg[1], seg[2]))
}

/// True when the URL's host is missing, a non-public IP literal, or
/// `localhost`/`*.localhost`. Domain names are not resolved.
///
/// Matches on the parsed `Host`, not `host_str()`: the latter brackets IPv6
/// (`"[::1]"`), which never parses as an `IpAddr`, and hyper skips custom
/// resolvers for IP literals, so this is the only guard they get.
pub fn is_blocked_host_literal(url: &Url) -> bool {
    match url.host() {
        None => true,
        Some(Host::Ipv4(v4)) => is_non_public_ip(v4.into()),
        Some(Host::Ipv6(v6)) => is_non_public_ip(v6.into()),
        Some(Host::Domain(host)) => {
            let host = host.to_ascii_lowercase();
            host == "localhost" || host.ends_with(".localhost")
        }
    }
}

/// 100.64.0.0/10: carrier-grade NAT, also Tailscale's tailnet range.
fn is_cgnat(v4: Ipv4Addr) -> bool {
    let [a, b, ..] = v4.octets();
    a == 100 && (b & 0xC0) == 64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    #[test]
    fn classifies_common_v4_ranges() {
        for blocked in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "192.0.2.1",
            "198.51.100.7",
            "203.0.113.9",
            "192.0.0.8",
            "192.88.99.1",
        ] {
            assert!(is_non_public_ip(ip(blocked)), "{blocked}");
        }
        for public in ["1.1.1.1", "8.8.8.8", "193.99.144.80"] {
            assert!(!is_non_public_ip(ip(public)), "{public}");
        }
    }

    #[test]
    fn classifies_extra_special_v4_ranges() {
        for (blocked, outside) in [
            ("0.1.2.3", "1.0.0.1"),
            ("224.0.0.1", "223.255.255.255"),
            ("239.255.255.255", "223.255.255.254"),
            ("198.18.0.1", "198.17.255.255"),
            ("198.19.255.255", "198.20.0.0"),
            ("240.0.0.1", "8.8.8.8"),
            ("255.255.255.255", "9.9.9.9"),
        ] {
            assert!(is_non_public_ip(ip(blocked)), "{blocked}");
            assert!(!is_non_public_ip(ip(outside)), "{outside}");
        }
    }

    #[test]
    fn classifies_cgnat_range() {
        assert!(is_non_public_ip(ip("100.64.0.1")));
        assert!(is_non_public_ip(ip("100.127.255.254")));
        assert!(!is_non_public_ip(ip("100.128.0.1")));
        assert!(!is_non_public_ip(ip("100.63.255.255")));
        assert!(is_non_public_ip(ip("::ffff:100.64.0.1")));
    }

    #[test]
    fn classifies_special_v6_ranges() {
        for blocked in [
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fe00::1",
            "ff02::1",
            "100::1",
            "64:ff9b:1::1",
            "64:ff9a::7f00:1",
            "2001:db8::1",
            "3fff::1",
            "3fff:fff::1",
            "2001:2::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_non_public_ip(ip(blocked)), "{blocked}");
        }
        for public in [
            "2606:4700:4700::1111",
            "2a00:1450:4001::1",
            "2001:2:1::1",
            "3fff:1000::1",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_non_public_ip(ip(public)), "{public}");
        }
    }

    #[test]
    fn embedded_v4_takes_the_v4_verdict() {
        // NAT64 well-known prefix with embedded v4.
        assert!(is_non_public_ip(ip("64:ff9b::7f00:1")));
        assert!(is_non_public_ip(ip("64:ff9b::a00:1")));
        assert!(!is_non_public_ip(ip("64:ff9b::808:808")));
        // 6to4 with embedded v4 in bits 16..48.
        assert!(is_non_public_ip(ip("2002:7f00:1::1")));
        assert!(is_non_public_ip(ip("2002:6440:1::1")));
        assert!(!is_non_public_ip(ip("2002:808:808::1")));
        assert!(!is_non_public_ip(ip("2003:7f00:1::1")));
    }
}

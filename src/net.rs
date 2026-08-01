//! Best-effort local address discovery for reachability reporting.
//!
//! No packets are sent: "connecting" a UDP socket only makes the kernel pick the
//! local source address it would use to reach a given public host, which we then
//! read back with `local_addr()`. This is the standard dependency-free way to
//! learn "which of my addresses faces the internet" without enumerating NICs.

use std::net::{Ipv6Addr, SocketAddr, UdpSocket};

/// The globally-routable IPv6 address this host would use to reach the internet,
/// or `None` on an IPv4-only host (or one with only link-local / ULA v6).
///
/// With native IPv6 there is no NAT — this address is directly reachable from the
/// public internet (subject only to the router's IPv6 firewall), which is what
/// lets a home node be a first-class peer without any port-forwarding.
pub fn global_ipv6() -> Option<Ipv6Addr> {
    let sock = UdpSocket::bind("[::]:0").ok()?;
    // Google public DNS over IPv6. connect() on a UDP socket sends nothing.
    sock.connect("[2001:4860:4860::8888]:53").ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V6(a) => {
            let ip = *a.ip();
            if is_global_unicast_v6(&ip) {
                Some(ip)
            } else {
                None
            }
        }
        _ => None,
    }
}

/// True for a 2000::/3 global-unicast address, excluding link-local (fe80::/10),
/// unique-local (fc00::/7), loopback (::1) and the unspecified address (::).
fn is_global_unicast_v6(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return false;
    }
    let seg0 = ip.segments()[0];
    if (seg0 & 0xffc0) == 0xfe80 {
        return false; // link-local fe80::/10
    }
    if (seg0 & 0xfe00) == 0xfc00 {
        return false; // unique local fc00::/7
    }
    (seg0 & 0xe000) == 0x2000 // global unicast 2000::/3
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_v6_scopes() {
        // global unicast
        assert!(is_global_unicast_v6(&"2001:569:5ab9:df00::1".parse().unwrap()));
        assert!(is_global_unicast_v6(&"2606:4700:4700::1111".parse().unwrap()));
        // link-local, ULA, loopback, unspecified — not reportable
        assert!(!is_global_unicast_v6(&"fe80::1".parse().unwrap()));
        assert!(!is_global_unicast_v6(&"fc00::1".parse().unwrap()));
        assert!(!is_global_unicast_v6(&"fd12:3456::1".parse().unwrap()));
        assert!(!is_global_unicast_v6(&"::1".parse().unwrap()));
        assert!(!is_global_unicast_v6(&"::".parse().unwrap()));
    }
}

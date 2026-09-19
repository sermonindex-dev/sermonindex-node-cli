//! Peer-assisted reachability checking.
//!
//! THE PROBLEM
//!
//! Our active probe runs on a Bunny edge script that has no outbound IPv6 at
//! all. So for the households we most want running nodes — Starlink, T-Mobile
//! Home Internet, mobile broadband, where inbound IPv4 is impossible for ever
//! and IPv6 is the only route in — we cannot verify reachability. We can only
//! wait for an organic peer to arrive, which can take hours. That first hour is
//! exactly when someone decides whether setting this up was worth it.
//!
//! Buying an IPv6-capable probe would fix it. We are not buying anything.
//!
//! THE OBSERVATION
//!
//! Any node with a global IPv6 address can make an OUTBOUND IPv6 connection,
//! whether or not it is itself reachable. That is nearly every node on a modern
//! connection. So the network already contains, for free, hundreds of machines
//! that can do precisely what the edge cannot — they just have to be asked.
//!
//! THE MECHANISM
//!
//! No new service and no new connection: the heartbeat already runs every five
//! minutes and its response is already parsed.
//!
//!   1. A node that wants confirming sets `wants_check: true` on its heartbeat.
//!   2. The server picks a recently-seen node that has a global IPv6 address and
//!      attaches `check_peer: { ip, port, token }` to ITS next heartbeat reply.
//!   3. That node opens a TCP connection, waits at most 5s, closes it, and
//!      reports `check_result: { token, open }` on its own next beat.
//!   4. The server records the answer against the first node.
//!
//! Worst case about ten minutes; typically five. Cost: nothing.
//!
//! THE GUARD RAILS — NOT OPTIONAL
//!
//! This asks one machine to connect to another on request, which is a port
//! scanner if built carelessly. The narrowness is the design:
//!
//!   * The address comes from the SERVER, taken from the target's own
//!     heartbeat. A client can never name someone else's address.
//!   * One port — the target's own declared listening port. No ranges.
//!   * We connect and immediately close. Nothing is sent, nothing is read.
//!     A banner grab is not possible because we never look.
//!   * Anything not a global-unicast address is refused here as well as at the
//!     server, because two checks in different places is the only way this
//!     stays true after someone edits one of them.
//!   * One check per beat, so a node cannot be turned into a traffic source.
//!
//! The prober learns nothing about whose address it dialled, and the subject
//! learns nothing about who checked it.

use serde_json::{json, Value};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

/// How long to wait for the connection. Long enough for a real intercontinental
/// handshake, short enough that a firewalled address does not hold the task.
const DIAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Would we be willing to dial this address?
///
/// Global unicast only. Loopback, link-local, unique-local, multicast and the
/// RFC1918 ranges are all refused: dialling those would mean reaching into the
/// prober's OWN house, which is precisely the abuse this must not enable.
fn is_dialable(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V6(v6) => {
            if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
                return false;
            }
            if v6.to_ipv4_mapped().is_some() {
                return false; // judge it as IPv4, below, not as IPv6
            }
            let seg0 = v6.segments()[0];
            (seg0 & 0xe000) == 0x2000 // 2000::/3 global unicast
        }
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_multicast()
                || v4.is_unspecified()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 0x40) // 100.64/10 CGNAT
        }
    }
}

/// Perform one check. `Some(true)` means the connection was established.
///
/// A refusal to dial returns None rather than false: "we would not try" and
/// "we tried and nobody answered" are different answers, and reporting the
/// first as the second would mark a reachable node unreachable.
pub async fn perform(ip: &str, port: u16) -> Option<bool> {
    let parsed: IpAddr = ip.trim().parse().ok()?;
    if !is_dialable(&parsed) || port == 0 {
        eprintln!("[peercheck] refusing to dial {ip} — not a public address");
        return None;
    }
    let addr = SocketAddr::new(parsed, port);
    match tokio::time::timeout(DIAL_TIMEOUT, tokio::net::TcpStream::connect(addr)).await {
        // Connected. Drop it immediately — we asked a yes/no question and we
        // have the answer; reading anything would make this something else.
        Ok(Ok(stream)) => {
            drop(stream);
            Some(true)
        }
        Ok(Err(_)) => Some(false), // refused / unreachable — a real answer
        Err(_) => Some(false),     // timed out — indistinguishable from closed
    }
}

/// Pull a `check_peer` request out of a heartbeat response, if there is one.
pub fn request_from(body: &Value) -> Option<(String, u16, String)> {
    let c = body.get("check_peer")?;
    let ip = c.get("ip").and_then(|v| v.as_str())?.to_string();
    let port = c.get("port").and_then(|v| v.as_u64())? as u16;
    let token = c.get("token").and_then(|v| v.as_str())?.to_string();
    Some((ip, port, token))
}

/// The field to add to the next heartbeat body reporting what we found.
pub fn result_field(token: &str, open: bool) -> Value {
    json!({ "token": token, "open": open })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// The security property this module exists to hold. If any of these start
    /// passing, the node has become something that can be pointed at a private
    /// network by whoever controls the server.
    #[test]
    fn refuses_to_dial_anything_private() {
        for bad in [
            "127.0.0.1", "10.0.0.1", "192.168.1.1", "172.16.0.1",
            "169.254.1.1", "0.0.0.0", "224.0.0.1",
            "100.64.0.1",          // carrier NAT — someone else's household
            "::1", "fe80::1", "fc00::1", "fd12:3456::1",
            "::", "ff02::1", "::ffff:192.168.1.1",
        ] {
            assert!(!is_dialable(&ip(bad)), "must refuse {bad}");
        }
    }

    #[test]
    fn accepts_real_public_addresses() {
        for good in ["2001:4860:4860::8888", "2606:4700:4700::1111", "8.8.8.8", "1.1.1.1"] {
            assert!(is_dialable(&ip(good)), "must accept {good}");
        }
    }

    #[test]
    fn parses_a_request_and_ignores_a_malformed_one() {
        let ok = json!({ "check_peer": { "ip": "2001:db8::1", "port": 42800, "token": "t1" } });
        assert_eq!(
            request_from(&ok),
            Some(("2001:db8::1".into(), 42800, "t1".into()))
        );
        assert!(request_from(&json!({})).is_none());
        assert!(request_from(&json!({ "check_peer": { "ip": "2001:db8::1" } })).is_none());
        assert!(request_from(&json!({ "check_peer": { "port": 1, "token": "t" } })).is_none());
    }
}

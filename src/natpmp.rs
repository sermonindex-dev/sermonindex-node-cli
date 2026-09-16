//! NAT-PMP / PCP port mapping — the CLI's half of the GUI's `natpmp.rs`.
//!
//! WHY THIS IS HERE AT ALL
//!
//! librqbit already attempts UPnP (`enable_upnp_port_forwarding: true`). UPnP is
//! off by default on a great many routers, and Apple, Ubiquiti and a lot of ISP
//! gear speak NAT-PMP or PCP instead. For those households the difference
//! between running this and not running it is the difference between a green
//! node and a yellow peer — between being a place other people can come to and
//! only ever being able to dial out.
//!
//! WHY IT IS SAFE ON A 4 GB PI
//!
//! Worth stating plainly, because it was left out of the CLI on the belief that
//! it was expensive, and it is not. The component that was genuinely expensive
//! is the DHT — `seed.rs` documents that: it is the only thing whose memory
//! grows with torrent count and uptime. This is the opposite shape. One task,
//! one UDP round-trip with a 3-second cap, then an hour asleep. No per-torrent
//! state, no cache, nothing that grows with the size of the library. The
//! resident cost is a task stack and a String — kilobytes, and constant.
//!
//! WHAT CHANGED IN THE PORT
//!
//! The desktop version detects the gateway with `route -n get default`, which is
//! macOS/BSD. The CLI's primary target is a Raspberry Pi, where that command
//! does not exist at all — so on Linux it would have silently fallen through to
//! the hardcoded guesses and failed on any network not using one of them.
//! `ip route show default` is tried first on Linux, and `log::` is replaced with
//! `eprintln!` since this crate has no logging framework.

use std::net::{IpAddr, Ipv4Addr};

/// A successful mapping. `udp_external_port` is None when only TCP took.
#[derive(Debug, Clone)]
pub struct MappingResult {
    pub gateway: String,
    pub tcp_external_port: u16,
    #[allow(dead_code)] // kept for future QUIC/uTP use, as in the desktop app
    pub udp_external_port: Option<u16>,
}

/// Gateway addresses to try, best guess first.
///
/// Asking the routing table is the only way to be right on a network that does
/// not use a common prefix; the hardcoded list is the fallback for when the
/// command is missing or its output is not what we expect. Both platforms are
/// covered because this binary runs on the Pi, on a NAS, on an old laptop and
/// on macOS.
fn gateway_candidates() -> Vec<Ipv4Addr> {
    let mut candidates: Vec<Ipv4Addr> = Vec::new();

    // Linux — `ip route show default` → "default via 192.168.1.1 dev eth0 ..."
    #[cfg(target_os = "linux")]
    if let Ok(out) = std::process::Command::new("ip")
        .args(["route", "show", "default"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            let mut parts = line.split_whitespace();
            while let Some(tok) = parts.next() {
                if tok == "via" {
                    if let Some(ip) = parts.next().and_then(|s| s.parse::<Ipv4Addr>().ok()) {
                        if !candidates.contains(&ip) {
                            candidates.push(ip);
                        }
                    }
                    break;
                }
            }
        }
    }

    // macOS / BSD — `route -n get default` → a "gateway: 192.168.1.1" line.
    #[cfg(any(target_os = "macos", target_os = "freebsd", target_os = "openbsd"))]
    if let Ok(out) = std::process::Command::new("route")
        .args(["-n", "get", "default"])
        .output()
    {
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix("gateway:") {
                if let Ok(ip) = rest.trim().parse::<Ipv4Addr>() {
                    if !candidates.contains(&ip) {
                        candidates.push(ip);
                    }
                }
            }
        }
    }

    // Fallbacks, for when the above found nothing usable.
    for gw in [
        Ipv4Addr::new(192, 168, 1, 1),
        Ipv4Addr::new(192, 168, 0, 1),
        Ipv4Addr::new(192, 168, 2, 1), // Telus fibre
        Ipv4Addr::new(10, 0, 0, 1),
        Ipv4Addr::new(10, 0, 0, 138), // some Telus fibre gateways
        Ipv4Addr::new(172, 16, 0, 1),
    ] {
        if !candidates.contains(&gw) {
            candidates.push(gw);
        }
    }
    candidates
}

async fn try_gateway(gw: Ipv4Addr, tcp_port: u16, udp_port: u16) -> Option<MappingResult> {
    let local = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

    // TCP is the one that matters — BitTorrent peers connect over it, and the
    // session is started with ListenerMode::TcpOnly.
    let tcp = crab_nat::try_port_mapping(
        IpAddr::V4(gw),
        local,
        crab_nat::InternetProtocol::Tcp,
        tcp_port,
        Some(tcp_port),
        Some(7200), // 2-hour lifetime; the caller renews hourly
    )
    .await
    .ok()?;

    let tcp_ext = tcp.external_port;
    let gateway = format!("{}", tcp.gateway);
    // crab_nat's mapping handle sends a DELETE on drop. We want the mapping to
    // OUTLIVE this scope — the renewal loop owns its lifetime, not the RAII
    // guard — so the handle is deliberately leaked. This mirrors the desktop
    // app, and the mapping expires on its own after 7200s if we die.
    std::mem::forget(tcp);
    eprintln!("[natpmp] TCP {tcp_port} -> external {tcp_ext} via {gateway}");

    // UDP is a bonus (future QUIC/uTP). Its failure is not this function's.
    let udp_ext = match crab_nat::try_port_mapping(
        IpAddr::V4(gw),
        local,
        crab_nat::InternetProtocol::Udp,
        udp_port,
        Some(udp_port),
        Some(7200),
    )
    .await
    {
        Ok(m) => {
            let ext = m.external_port;
            std::mem::forget(m);
            Some(ext)
        }
        Err(_) => None,
    };

    Some(MappingResult {
        gateway,
        tcp_external_port: tcp_ext,
        udp_external_port: udp_ext,
    })
}

/// Try every candidate gateway, 3 seconds each. `None` means NAT-PMP/PCP is not
/// available here — which is common and not an error.
pub async fn try_mapping(tcp_port: u16, udp_port: u16) -> Option<MappingResult> {
    for gw in gateway_candidates() {
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            try_gateway(gw, tcp_port, udp_port),
        )
        .await
        {
            Ok(Some(r)) => return Some(r),
            Ok(None) => {}  // responded, refused
            Err(_) => {}    // timed out — almost certainly nothing there
        }
    }
    None
}

/// Background renewal. Returns a handle the caller stores so `/stats` and the
/// heartbeat can report what happened.
///
/// The mapping lifetime we ask for is 7200s and we renew at 3600s, so a missed
/// renewal has a full hour of slack before the port closes. On failure it waits
/// 30 minutes rather than giving up for good: a router can appear later (the
/// node booted before the router did, or the network changed).
pub fn spawn(port: u16, status: std::sync::Arc<std::sync::Mutex<String>>) {
    tokio::spawn(async move {
        loop {
            match try_mapping(port, port).await {
                Some(m) => {
                    *status.lock().unwrap_or_else(|e| e.into_inner()) =
                        format!("mapped via {}", m.gateway);
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
                None => {
                    *status.lock().unwrap_or_else(|e| e.into_inner()) = "unavailable".to_string();
                    tokio::time::sleep(std::time::Duration::from_secs(1800)).await;
                }
            }
        }
    });
}

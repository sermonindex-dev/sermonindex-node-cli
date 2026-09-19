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
    /// The lifetime the gateway actually GRANTED, which is frequently not the
    /// one we asked for.
    ///
    /// This matters far more than it looks. We request 7200s, and a home router
    /// generally agrees. **Proton VPN grants 60 seconds.** Renewing hourly
    /// against a gateway that grants a minute means the mapping is alive for
    /// one minute in every sixty, and for the other fifty-nine the node is
    /// unreachable while believing it is fine. So the renewal loop schedules
    /// itself from this, never from the value we asked for.
    pub lifetime_secs: u64,
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

    // ── VPN tunnel gateways ──────────────────────────────────────────────
    //
    // A node behind CGNAT (Starlink, T-Mobile Home Internet, most mobile
    // broadband) can never be dialled on IPv4 no matter what it does to its own
    // router — the carrier shares one address between many homes. The way out
    // is a VPN that offers port forwarding: the tunnel gives the node a real
    // public address and one real public port.
    //
    // Proton VPN — the common case — hands that port out over NAT-PMP on
    // 10.2.0.1 INSIDE the WireGuard tunnel. Which means the node can ask for it
    // itself: no wrapper script, no VPN CLI, no hand-copied port number. These
    // go FIRST because when a tunnel is up it is the interface that matters,
    // and the LAN router below it cannot help anyway.
    for gw in [
        Ipv4Addr::new(10, 2, 0, 1),   // Proton VPN (WireGuard tunnel)
        Ipv4Addr::new(10, 8, 0, 1),   // common OpenVPN tunnel
        Ipv4Addr::new(10, 64, 0, 1),  // Mullvad
    ] {
        if !candidates.contains(&gw) {
            candidates.push(gw);
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

async fn try_gateway(
    gw: Ipv4Addr,
    tcp_port: u16,
    udp_port: u16,
    want_external: Option<u16>,
) -> Option<MappingResult> {
    let local = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

    // TCP is the one that matters — BitTorrent peers connect over it, and the
    // session is started with ListenerMode::TcpOnly.
    //
    // `want_external` is the crux of working with a VPN. On the FIRST call it is
    // None, which tells the gateway "give me whatever port you like" — and a VPN
    // will hand back a random one. On every renewal afterwards we ask for the
    // port we were given, because the alternative is a new random port every
    // time the mapping is renewed, and the announce port would then have to
    // change every 30 seconds. A gateway honours the request while the port is
    // still free, which it is, because we are the one holding it.
    let tcp = crab_nat::try_port_mapping(
        IpAddr::V4(gw),
        local,
        crab_nat::InternetProtocol::Tcp,
        tcp_port,
        want_external.or(Some(tcp_port)),
        Some(7200), // what we ASK for; the gateway decides, see lifetime below
    )
    .await
    .ok()?;

    let tcp_ext = tcp.external_port;
    let gateway = format!("{}", tcp.gateway);
    let lifetime_secs = tcp.lifetime.as_secs().max(1);
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
        lifetime_secs,
    })
}

/// Try every candidate gateway, 3 seconds each. `None` means NAT-PMP/PCP is not
/// available here — which is common and not an error.
pub async fn try_mapping(
    tcp_port: u16,
    udp_port: u16,
    want_external: Option<u16>,
) -> Option<MappingResult> {
    for gw in gateway_candidates() {
        match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            try_gateway(gw, tcp_port, udp_port, want_external),
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

/// Map a port ONCE, before the torrent session starts.
///
/// Why before: librqbit takes `announce_port` when the session is constructed
/// and does not let it change afterwards — it is the number written into every
/// tracker announce and every DHT `announce_peer`. So the public port has to be
/// known before the session exists, not discovered later.
///
/// Returns the external port the gateway assigned, which the caller passes
/// straight into `Seeder::start`.
pub async fn map_once(port: u16) -> Option<MappingResult> {
    try_mapping(port, port, None).await
}

/// Background renewal. Returns a handle the caller stores so `/stats` and the
/// heartbeat can report what happened.
///
/// The mapping lifetime we ask for is 7200s and we renew at 3600s, so a missed
/// renewal has a full hour of slack before the port closes. On failure it waits
/// 30 minutes rather than giving up for good: a router can appear later (the
/// node booted before the router did, or the network changed).
/// Keep a mapping alive.
///
/// `hold_external` is the port a previous `map_once` obtained — the one already
/// baked into this session's announces. Every renewal asks for that same port,
/// so the number other peers were told to dial stays true.
///
/// The renewal interval comes from the lifetime the GATEWAY granted, halved,
/// and never trusted to be long: a home router grants the 7200s we ask for and
/// gets renewed hourly; **Proton VPN grants 60 seconds**, and gets renewed every
/// 30. Renewing hourly against Proton — which is what the previous version did
/// — left the mapping dead for 59 minutes out of every 60 while the node
/// cheerfully reported "mapped".
pub fn spawn(
    port: u16,
    hold_external: Option<u16>,
    status: std::sync::Arc<std::sync::Mutex<String>>,
) {
    tokio::spawn(async move {
        let mut want = hold_external;
        loop {
            match try_mapping(port, port, want).await {
                Some(m) => {
                    // If the gateway gave us a different port than we asked for,
                    // say so loudly: every announce already out there names the
                    // old one, and only a restart can correct that.
                    if let Some(w) = want {
                        if m.tcp_external_port != w {
                            eprintln!(
                                "[natpmp] gateway moved our port {} -> {} — peers were told {w}. \
                                 Restart the node to announce the new one.",
                                w, m.tcp_external_port
                            );
                        }
                    }
                    want = Some(m.tcp_external_port);
                    *status.lock().unwrap_or_else(|e| e.into_inner()) = format!(
                        "port {} via {} (renews every {}s)",
                        m.tcp_external_port,
                        m.gateway,
                        m.lifetime_secs / 2
                    );
                    // Half the granted lifetime, clamped: never hammer the
                    // gateway faster than every 15s, never leave more than an
                    // hour between renewals.
                    let every = (m.lifetime_secs / 2).clamp(15, 3600);
                    tokio::time::sleep(std::time::Duration::from_secs(every)).await;
                }
                None => {
                    *status.lock().unwrap_or_else(|e| e.into_inner()) = "unavailable".to_string();
                    // A tunnel that dropped may come back; retry sooner than the
                    // old 30 minutes, since on a VPN the mapping is the whole
                    // reason this node is reachable at all.
                    tokio::time::sleep(std::time::Duration::from_secs(120)).await;
                }
            }
        }
    });
}

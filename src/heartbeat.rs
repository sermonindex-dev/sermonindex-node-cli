//! Network presence: geo lookup (cached), the 5-minute heartbeat that puts this
//! node on the live map, and the 180-second liveness ping. Mirrors the app's
//! /api/node/heartbeat + /api/node/ping contract so the CLI node is a first-class
//! member of the same network view.

use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::config::{API_BASE, APP_VERSION, LISTEN_PORT_START, PROBE_API};
use crate::state::Shared;

/// Has the node admin granted THIS node seed status? Mirrors the desktop app's
/// `checkSeedAccess()`: GET /api/seed/access?node_id=X → { ok, enabled }. Only an
/// admin flipping `seed_access.enabled = 1` makes this true, so the node shows
/// blue on the map ONLY when the admin has set it as a seed (and it is reachable).
pub async fn check_seed_access(client: &reqwest::Client, node_id: &str) -> bool {
    let url = format!("{API_BASE}/api/seed/access?node_id={node_id}");
    let resp = match client
        .get(&url)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => r,
        _ => return false,
    };
    let data: Value = match resp.json().await {
        Ok(v) => v,
        Err(_) => return false,
    };
    data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false)
        && data.get("enabled").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Ask the probe edge to TCP-connect back to us over IPv4 and (if we have one)
/// IPv6, and report whether either succeeded. This is the exact mechanism the
/// desktop app uses: reachable = `open || open_v6`. Returns:
///   Some(true)  — a peer really can dial in on v4 or v6 → map shows "node"/green
///                 (or "seed"/blue if the admin has granted this node seed status)
///   Some(false) — the probe ran but no inbound worked → "peer"/yellow
///   None        — probe service unavailable → leave reachability unknown
/// A client pinned to IPv4 egress.
///
/// Binding the local address to 0.0.0.0 forces the connection over IPv4. This
/// is the whole fix for a class of node that reported itself unreachable while
/// being perfectly reachable: the edge tests `open` against the address the
/// request ARRIVES from, and only tests `open_v6` against addresses we put in
/// the body. A dual-stack host sends this over IPv6 by default, so the edge
/// would test our (usually firewalled) IPv6 address as `open` and never learn
/// about a forwarded IPv4 port at all. Measured on a real node: IPv4 42800 open
/// and confirmed by hand, yet the node kept reporting reachable=false.
fn ipv4_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("sermonindex-node/{}", env!("CARGO_PKG_VERSION")))
        .local_address(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
        .connect_timeout(std::time::Duration::from_secs(15))
        .build()
        .ok()
}

async fn post_probe(client: &reqwest::Client, body: &Value) -> Option<(bool, bool)> {
    let resp = client
        .post(format!("{PROBE_API}/probe"))
        .json(body)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let data: Value = resp.json().await.ok()?;
    if !data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        return None;
    }
    Some((
        data.get("open").and_then(|v| v.as_bool()).unwrap_or(false),
        data.get("open_v6").and_then(|v| v.as_bool()).unwrap_or(false),
    ))
}

pub async fn probe_reachability(client: &reqwest::Client, port: u16) -> Option<bool> {
    // Offer our global IPv6 (if any) so the edge can attempt the v6 dial — the
    // address a BitTorrent peer would actually use for global egress. The v6
    // list travels in the BODY, so a single request sent over IPv4 still gets
    // both answers: `open` for our public IPv4, `open_v6` for the addresses here.
    let mut body = match crate::net::global_ipv6() {
        Some(v6) => json!({ "port": port, "ipv6": [v6.to_string()] }),
        None => json!({ "port": port }),
    };
    // An explicitly configured public address, for the split-tunnel case: the
    // torrent traffic goes through a VPN but this request does not, so the edge
    // would otherwise test the home address, find it closed, and file a
    // perfectly reachable node as a yellow peer. Harmless when unset, and the
    // edge is free to ignore it.
    if let Some(ip) = crate::config::public_ip(&crate::config::load_settings()) {
        body["public_ip"] = json!(ip);
    }

    // Prefer the IPv4-pinned client so `open` is a real IPv4 measurement. Fall
    // back to the shared client on an IPv6-only host, where forcing IPv4 would
    // fail outright — there, `open` is judged against the v6 address as before.
    let (open, open_v6) = match ipv4_client() {
        Some(c4) => match post_probe(&c4, &body).await {
            Some(r) => r,
            None => post_probe(client, &body).await?,
        },
        None => post_probe(client, &body).await?,
    };
    Some(open || open_v6)
}

/// Best-effort IP geolocation (city/region/country/lat/lon). Cached by caller.
pub async fn geo(client: &reqwest::Client) -> Option<Value> {
    // 1) ipapi.co  2) ipwho.is  3) our own /api/geo
    if let Some(g) = try_geo(client, "https://ipapi.co/json/", |j| {
        Some(json!({
            "city": j.get("city"), "region": j.get("region"), "country": j.get("country_name"),
            "lat": j.get("latitude"), "lon": j.get("longitude"),
        }))
    })
    .await
    {
        return Some(g);
    }
    if let Some(g) = try_geo(client, "https://ipwho.is/", |j| {
        Some(json!({
            "city": j.get("city"), "region": j.get("region"), "country": j.get("country"),
            "lat": j.get("latitude"), "lon": j.get("longitude"),
        }))
    })
    .await
    {
        return Some(g);
    }
    try_geo(client, &format!("{API_BASE}/api/geo"), |j| Some(j.clone())).await
}

async fn try_geo(
    client: &reqwest::Client,
    url: &str,
    map: impl Fn(&Value) -> Option<Value>,
) -> Option<Value> {
    let resp = client
        .get(url)
        .timeout(std::time::Duration::from_secs(6))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let j: Value = resp.json().await.ok()?;
    let g = map(&j)?;
    // Require at least a latitude to count it as usable.
    if g.get("lat").map(|v| !v.is_null()).unwrap_or(false) {
        Some(g)
    } else {
        None
    }
}

/// Post one heartbeat. Returns the parsed response (may carry config/commands).
pub async fn beat(
    client: &reqwest::Client,
    shared: &Arc<Shared>,
    geo: &Option<Value>,
    reachable: Option<bool>,
    seed_granted: bool,
) -> Option<Value> {
    let cov = shared.coverage_pct();
    let g = geo.clone().unwrap_or_else(|| json!({}));
    // node_type reflects the ADMIN grant only — never self-declared. The server
    // upsert overwrites node_type from this field each beat, so we must keep
    // sending "seed" for as long as the admin grant stands, else it reverts.
    let node_type = if seed_granted { "seed" } else { "user" };
    // The port the session really bound (0 = not seeding → report the default,
    // which is the only port anyone could have forwarded anyway).
    let listen_port = match shared.listen_port.load(Ordering::Relaxed) {
        0 => LISTEN_PORT_START,
        p => p as u16,
    };
    // Global IPv6 (if any) so the map server can probe inbound reachability and
    // light this node green/blue without any router port-forward. Cheap to
    // recompute each beat (no packets are sent) and it tracks address changes.
    let public_ipv6 = crate::net::global_ipv6().map(|a| a.to_string());
    // The server reads `reachable` straight from the body (true/false/null); it
    // does NOT probe. Some(true)→1, Some(false)→0, None→null (server keeps prior).
    let reachable_json = match reachable {
        Some(b) => Value::Bool(b),
        None => Value::Null,
    };
    let mut body = json!({
        "node_id": shared.node_id,
        "protocol": "bittorrent",
        "app_version": APP_VERSION,
        "node_type": node_type,
        "content_mode": "cdn",
        "seed_scope": shared.scope,
        "seed_progress": cov,
        "seed_verified": cov >= 95.0,
        "library_coverage": cov,
        "files_stored": shared.held.load(Ordering::Relaxed),
        "storage_used_bytes": shared.storage_bytes.load(Ordering::Relaxed),
        "uploaded_bytes": shared.uploaded_bytes.load(Ordering::Relaxed),
        "peers_connected": shared.peers.load(Ordering::Relaxed),
        // Direction. `peers_connected` is a bare total and cannot distinguish
        // "people are taking sermons from me" from "I am taking from them" —
        // which is the whole question a node operator has. `peers_in` counts
        // peers that DIALLED US (proof we are serving); `peers_in_peak` is the
        // high-water mark for this run, because the instantaneous figure drops
        // to 0 every time the live window rotates away and a momentary 0 was
        // being read as "you have helped nobody".
        "peers_in": shared.peers_in.load(Ordering::Relaxed),
        "peers_out": shared.peers_out.load(Ordering::Relaxed),
        "peers_in_peak": shared.peers_in_peak.load(Ordering::Relaxed),
        "uptime_seconds": shared.started.elapsed().as_secs(),
        // Scheduled quiet hours in effect right now (church livestream etc.) —
        // the node is online and heartbeating but intentionally not serving.
        // Lets the admin dashboard show "quiet" instead of a worrying idle.
        "quiet": shared.quiet.load(Ordering::Relaxed),
        "tcp_listen_port": listen_port,
        "public_ipv6": public_ipv6,
        "reachable": reachable_json,
        "lat": g.get("lat"), "lon": g.get("lon"),
        "city": g.get("city"), "region": g.get("region"), "country": g.get("country"),
        "p2p_status": {
            "tcp_listen_port": listen_port,
            "torrent_count": shared.torrents.load(Ordering::Relaxed),
            "peer_count": shared.peers.load(Ordering::Relaxed),
            "uptime": shared.started.elapsed().as_secs(),
        }
    });
    // Ask to be checked when we have a global IPv6 address and nothing has
    // confirmed us recently. A node that is already confirmed does not need the
    // favour, and one without IPv6 cannot be helped this way.
    if public_ipv6.is_some() && !shared.v6_confirmed_recently() {
        body["wants_check"] = json!(true);
    }
    // Report a check we performed for someone else on the previous beat.
    if let Some(r) = shared.pending_check.lock().unwrap().take() {
        body["check_result"] = r;
    }

    let resp = client
        .post(format!("{API_BASE}/api/node/heartbeat"))
        .json(&body)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .ok()?;
    resp.json::<Value>().await.ok()
}

/// Lightweight liveness ping (keeps last_seen fresh between heartbeats).
pub async fn ping(client: &reqwest::Client, node_id: &str) {
    let _ = client
        .post(format!("{API_BASE}/api/node/ping"))
        .json(&json!({ "node_id": node_id }))
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;
}

/// Fetch the live map + stats (for the dashboard's network view).
pub async fn fetch_network(client: &reqwest::Client) -> (Value, Value) {
    let map = client
        .get(format!("{API_BASE}/api/node/map"))
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .await
        .ok();
    let map = match map {
        Some(r) => r.json::<Value>().await.unwrap_or_else(|_| json!({})),
        None => json!({}),
    };
    let stats = client
        .get(format!("{API_BASE}/api/node/stats"))
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .await
        .ok();
    let stats = match stats {
        Some(r) => r.json::<Value>().await.unwrap_or_else(|_| json!({})),
        None => json!({}),
    };
    (map, stats)
}

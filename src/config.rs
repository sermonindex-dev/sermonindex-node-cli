//! Configuration, the ~/.sermonindex data directory, and node identity.
//! Layout and formats are byte-compatible with the desktop app so the two can
//! share a data dir (settings.json, catalog.json, download-state.json, downloads/).

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const LISTEN_PORT_START: u16 = 42800;
pub const LISTEN_PORT_END: u16 = 42839; // inclusive, matches the app
pub const DASHBOARD_PORT: u16 = 8137;
pub const API_BASE: &str = "https://app.sermonindex.net";
/// Reachability probe edge (server/network-edge-script.js) — TCP-dials this node
/// back over IPv4 and IPv6 so it can self-report `reachable`, exactly like the
/// desktop app's `probeReachability()`.
pub const PROBE_API: &str = "https://app-endpoints-gkb5p.bunny.run";
pub const MASTER_LIST_URL: &str = "https://sermonindex1.b-cdn.net/torrents/master-list.json";
pub const APP_VERSION: &str = concat!("cli-", env!("CARGO_PKG_VERSION"));

/// Release index for the headless node.
///
/// This is the file `publish-node-cli.sh` ALREADY writes and already purges on
/// every release, and the same one `/node-software/` renders its download list
/// from. Pointing the update check at it means publishing a release is exactly
/// the one command it has always been — no second manifest to remember, and no
/// way for the two to disagree about what the newest version is.
///
/// Shape: { "releases": [ { "version": "v0.2.3", "date": …, "url": …, … }, … ] }
/// newest first, versions carrying a leading "v".
pub const MANIFEST_URL: &str = "https://sermonindex4.b-cdn.net/node-cli/releases/releases.json";

/// How often to re-read the signed master list looking for newly published
/// sermons. Before 0.2.2 it was read exactly once, at startup — so a node that
/// had been up for three weeks was serving the library as it stood three weeks
/// ago, and only a restart ever fixed that.
pub const MASTERLIST_REFRESH_SECS: u64 = 3600;

/// How often to append a row to history.jsonl.
pub const HISTORY_INTERVAL_SECS: u64 = 3600;

/// How often to re-check for a newer release. Notify-only; nothing installs.
pub const UPDATE_CHECK_SECS: u64 = 6 * 3600;

/// ~/.sermonindex (same dir the desktop app uses).
pub fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".sermonindex")
}

/// The downloads root — settings.json "storage_dir" if set, else <data>/downloads.
pub fn downloads_dir(settings: &Value) -> PathBuf {
    if let Some(s) = settings.get("storage_dir").and_then(|v| v.as_str()) {
        if !s.trim().is_empty() {
            return PathBuf::from(s);
        }
    }
    data_dir().join("downloads")
}

pub fn torrents_dir() -> PathBuf {
    data_dir().join("torrents")
}

/// Local 2-char shard for a file — copied verbatim from the app's `shard_for`
/// so files land in the same place and stay reusable across app and CLI.
pub fn shard_for(filename: &str) -> String {
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    let mut chars = stem.chars().filter(|c| c.is_ascii_alphanumeric());
    let a = chars.next().unwrap_or('0').to_ascii_lowercase();
    let b = chars.next().unwrap_or('0').to_ascii_lowercase();
    format!("{a}{b}")
}

/// Absolute on-disk path for a canonical file name like "gGl7g6A_0WI-IeZJ.mp3".
pub fn file_path(settings: &Value, name: &str) -> PathBuf {
    downloads_dir(settings).join(shard_for(name)).join(name)
}

fn settings_path() -> PathBuf {
    data_dir().join("settings.json")
}

pub fn load_settings() -> Value {
    match std::fs::read_to_string(settings_path()) {
        Ok(s) => serde_json::from_str(&s).unwrap_or_else(|_| json!({})),
        Err(_) => json!({}),
    }
}

/// Atomic settings write (temp file + rename), preserving existing keys.
pub fn save_settings(settings: &Value) -> Result<()> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).ok();
    let tmp = dir.join("settings.json.part");
    std::fs::write(&tmp, serde_json::to_vec_pretty(settings)?)
        .context("write settings.json.part")?;
    std::fs::rename(&tmp, settings_path()).context("rename settings.json")?;
    Ok(())
}

/// Return the persisted node_id, generating + saving a fresh one if absent.
/// Format matches the app: "si-" + 16 random bytes hex.
pub fn node_id(settings: &mut Value) -> String {
    if let Some(id) = settings.get("node_id").and_then(|v| v.as_str()) {
        if !id.is_empty() {
            return id.to_string();
        }
    }
    let mut buf = [0u8; 16];
    getrandom(&mut buf);
    let id = format!("si-{}", hex::encode(buf));
    settings["node_id"] = json!(id);
    let _ = save_settings(settings);
    id
}

/// The seed scope: "audio" (~400 GB) or "full" (~2.4 TB). Defaults to audio.
pub fn seed_scope(settings: &Value) -> String {
    settings
        .get("seed_scope")
        .and_then(|v| v.as_str())
        .unwrap_or("audio")
        .to_string()
}

/// Whether to join the BitTorrent DHT. Defaults to TRUE.
///
/// History worth keeping: DHT was briefly defaulted OFF because resident memory
/// on a Pi holding ~25,500 torrents climbed ~0.5 GB/hour until the machine
/// froze. Measured with src/bin/memtest.rs:
///
///   trackers only : 15.0 KB/torrent, flat over the sample window
///   DHT only      : 11.7 KB/torrent and climbing continuously
///
/// The cause was never DHT itself — it was that peers discovered per torrent
/// accumulated in a map librqbit never capped or pruned. Turning DHT off just
/// slowed the fill while costing all peer discovery (the node dropped to zero
/// peers). vendor/librqbit now bounds that map, so DHT is safe again and is
/// back on by default. See max_peers_per_torrent().
pub fn dht_enabled(settings: &Value) -> bool {
    settings
        .get("dht_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// How many DISCOVERED peer addresses each torrent will remember (default 64).
///
/// This is the knob that keeps memory bounded, and it is NOT the same as the
/// number of peers connected — that is governed by librqbit's own per-torrent
/// connection semaphore. This caps the map of *known candidate addresses*,
/// which is what grew without limit and exhausted the Pi.
///
/// Set 0 to restore upstream behaviour (unbounded) — useful for A/B testing
/// with memtest, not recommended in production on a small machine.
pub fn max_peers_per_torrent(settings: &Value) -> usize {
    settings
        .get("max_peers_per_torrent")
        .and_then(|v| v.as_u64())
        .unwrap_or(64) as usize
}

/// How many torrents are LIVE (announcing + serving) at once. Default 2000.
///
/// Why this exists: bringing all ~25,500 torrents live at once costs over 2 GB
/// of resident memory before the node even reaches steady state — on the 4 GB
/// Pi that meant a kernel OOM kill at the service's memory ceiling every ~10
/// minutes, forever. The discovered-peer cap (max_peers_per_torrent) bounds
/// growth over TIME but not that baseline. So the node keeps the whole library
/// registered but PAUSED (a paused torrent keeps its verified pieces and costs
/// ~6 KB) and rotates a window of this many live, advancing every
/// rotate_minutes. A torrent with connected peers is never paused mid-transfer.
///
/// Set 0 to disable rotation and bring everything live — fine on machines with
/// real RAM, exactly what melted the Pi. Nodes holding fewer files than the
/// window are unaffected: everything stays live and rotation is a no-op.
pub fn active_torrents(settings: &Value) -> usize {
    match settings.get("active_torrents").and_then(|v| v.as_u64()) {
        Some(n) => n as usize,
        // Unset now means "pick for this machine" rather than a flat 2000 that
        // was chosen for the smallest one. See active_torrents_default().
        None => active_torrents_default(),
    }
}

/// Minutes each rotation window stays live before advancing (default 15,
/// minimum 1). Time to cycle the full library ≈ ceil(total/active) × this —
/// at the defaults on the full audio scope, ~13 windows ≈ 3¼ hours.
pub fn rotate_minutes(settings: &Value) -> u64 {
    settings
        .get("rotate_minutes")
        .and_then(|v| v.as_u64())
        .unwrap_or(15)
        .max(1)
}

// ── quiet hours ──────────────────────────────────────────────────────────────
// Windows in LOCAL time when the node must not touch the network — e.g. Sunday
// morning services and Wednesday-night meetings, so a church's livestream never
// competes with the seed for upstream bandwidth. During a quiet window the node
// pauses every torrent, refuses incoming handshakes (including wake-on-demand),
// and holds off downloading; heartbeats and the dashboard stay up.
//
// settings.json (shared with the desktop app):
//   "quiet_hours": [
//     { "days": ["sun"], "start": "08:00", "end": "13:00" },
//     { "days": ["wed"], "start": "18:00", "end": "21:30" }
//   ]
//
// The GUI's simpler daily schedule (seed_schedule_enabled + seed_start/seed_end
// — the hours seeding IS allowed, same every day) is honored too: outside that
// window the node is quiet. Both can be used together.

pub const DAY_NAMES: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

#[derive(Clone, Debug)]
pub struct QuietWindow {
    pub days: u8,   // bitmask, bit 0 = Sunday … bit 6 = Saturday
    pub start: u16, // minutes since local midnight
    pub end: u16,   // end < start means the window crosses midnight
}

pub fn parse_hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u16, u16) = (h.parse().ok()?, m.parse().ok()?);
    if h > 23 || m > 59 {
        return None;
    }
    Some(h * 60 + m)
}

/// "sun,wed" | "all" | "weekdays" | "weekend" → day bitmask.
pub fn parse_days(spec: &str) -> Option<u8> {
    match spec.trim().to_lowercase().as_str() {
        "all" | "daily" | "everyday" => return Some(0x7f),
        "weekdays" => return Some(0b0111110),
        "weekend" | "weekends" => return Some(0b1000001),
        _ => {}
    }
    let mut mask = 0u8;
    for part in spec.split(',') {
        let p = part.trim().to_lowercase();
        // "sun", "sunday", "Sun" all match — compare on the first 3 ASCII chars.
        let key = p.get(..3)?;
        let i = DAY_NAMES.iter().position(|d| *d == key)?;
        mask |= 1 << i;
    }
    if mask == 0 { None } else { Some(mask) }
}

pub fn days_label(mask: u8) -> String {
    if mask == 0x7f {
        return "every day".into();
    }
    DAY_NAMES
        .iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, d)| *d)
        .collect::<Vec<_>>()
        .join(",")
}

fn fmt_min(m: u16) -> String {
    format!("{:02}:{:02}", m / 60, m % 60)
}

pub fn window_label(w: &QuietWindow) -> String {
    format!("{} {}–{}", days_label(w.days), fmt_min(w.start), fmt_min(w.end))
}

/// Parse settings "quiet_hours" — malformed entries are skipped, not fatal.
pub fn quiet_windows(settings: &Value) -> Vec<QuietWindow> {
    let mut out = Vec::new();
    if let Some(arr) = settings.get("quiet_hours").and_then(|v| v.as_array()) {
        for e in arr {
            let days = match e.get("days") {
                Some(Value::Array(ds)) => {
                    let spec = ds
                        .iter()
                        .filter_map(|d| d.as_str())
                        .collect::<Vec<_>>()
                        .join(",");
                    parse_days(&spec)
                }
                Some(Value::String(s)) => parse_days(s),
                _ => Some(0x7f), // no days field = every day
            };
            let (days, start, end) = match (
                days,
                e.get("start").and_then(|v| v.as_str()).and_then(parse_hhmm),
                e.get("end").and_then(|v| v.as_str()).and_then(parse_hhmm),
            ) {
                (Some(d), Some(s), Some(en)) if s != en => (d, s, en),
                _ => continue,
            };
            out.push(QuietWindow { days, start, end });
        }
    }
    out
}

/// The GUI's daily ACTIVE window, if enabled and sane.
fn gui_active_window(settings: &Value) -> Option<(u16, u16)> {
    let on = settings
        .get("seed_schedule_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !on {
        return None;
    }
    let s = settings.get("seed_start").and_then(|v| v.as_str()).and_then(parse_hhmm)?;
    let e = settings.get("seed_end").and_then(|v| v.as_str()).and_then(parse_hhmm)?;
    if s == e { None } else { Some((s, e)) }
}

fn in_quiet_window(w: &QuietWindow, dow: u8, cur: u16) -> bool {
    if w.start < w.end {
        w.days & (1 << dow) != 0 && cur >= w.start && cur < w.end
    } else {
        // Crosses midnight: tonight's part, or the spill-over from yesterday.
        (w.days & (1 << dow) != 0 && cur >= w.start)
            || (w.days & (1 << ((dow + 6) % 7)) != 0 && cur < w.end)
    }
}

/// Is the node scheduled quiet right now (local time)? Returns the matching
/// rule as a human label for logs, None when it should be running.
pub fn quiet_now(settings: &Value) -> Option<String> {
    use chrono::{Datelike, Local, Timelike};
    let now = Local::now();
    let dow = now.weekday().num_days_from_sunday() as u8;
    let cur = (now.hour() * 60 + now.minute()) as u16;
    for w in quiet_windows(settings) {
        if in_quiet_window(&w, dow, cur) {
            return Some(format!("quiet hours ({})", window_label(&w)));
        }
    }
    if let Some((s, e)) = gui_active_window(settings) {
        let active = if s < e { cur >= s && cur < e } else { cur >= s || cur < e };
        if !active {
            return Some(format!(
                "outside daily seeding hours ({}–{}, GUI schedule)",
                fmt_min(s),
                fmt_min(e)
            ));
        }
    }
    None
}

/// One-line schedule summary for startup/status output.
pub fn describe_quiet(settings: &Value) -> String {
    let mut parts: Vec<String> = quiet_windows(settings).iter().map(window_label).collect();
    if let Some((s, e)) = gui_active_window(settings) {
        parts.push(format!("outside {}–{} daily (GUI)", fmt_min(s), fmt_min(e)));
    }
    if parts.is_empty() {
        "none scheduled — running around the clock".into()
    } else {
        format!("quiet {}  (local time)", parts.join(", "))
    }
}

/// Optional upload cap in bytes/sec (0/absent = unlimited).
pub fn upload_limit_bps(settings: &Value) -> Option<u32> {
    let enabled = settings
        .get("upload_limit_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !enabled {
        return None;
    }
    let kbps = settings
        .get("upload_limit_kbps")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    if kbps <= 0.0 {
        None
    } else {
        Some((kbps * 1024.0) as u32)
    }
}

/// Minimal OS randomness without pulling the `rand` crate.
fn getrandom(buf: &mut [u8]) {
    #[cfg(unix)]
    {
        use std::io::Read;
        // Read EXACTLY buf.len() bytes — /dev/urandom is an infinite stream, so
        // fs::read() would never stop (and OOM). Open + read_exact instead.
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(buf).is_ok() {
                return;
            }
        }
    }
    // Fallback: mix a few entropy sources (only hit if /dev/urandom is unavailable).
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id() as u128;
    let mut x = seed ^ (pid << 64) ^ 0x9E3779B97F4A7C15;
    for b in buf.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = (x & 0xff) as u8;
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// 0.2.3 — settings the desktop app has always written and the CLI never read
//
// The two programs share ~/.sermonindex/settings.json. Until now the CLI read
// most of it but silently ignored these three, so a person who set them in the
// GUI and then ran a headless node got a node that quietly did something else.
// ─────────────────────────────────────────────────────────────────────────────

/// Monthly upload ceiling in BYTES, or None for unlimited.
///
/// The GUI has had this under Settings → Seeding Schedule & Limits since
/// forever; the CLI ignored it completely. That is the wrong way round: the
/// desktop app runs on a machine with somebody sitting at it who would notice a
/// data bill, and the headless node is the one on a church's metered connection
/// with nobody watching it at all.
pub fn monthly_cap_bytes(settings: &Value) -> Option<u64> {
    if !settings.get("monthly_cap_enabled").and_then(|v| v.as_bool()).unwrap_or(false) {
        return None;
    }
    settings
        .get("monthly_cap_gb")
        .and_then(|v| v.as_f64())
        .filter(|g| *g > 0.0)
        .map(|g| (g * 1024.0 * 1024.0 * 1024.0) as u64)
}

/// Is BitTorrent enabled at all? Default TRUE.
///
/// False = download and hold, never serve. Previously only reachable by
/// recompiling with `--no-default-features`, which is not a thing you can ask a
/// volunteer to do over the phone.
pub fn p2p_enabled(settings: &Value) -> bool {
    settings.get("p2p_enabled").and_then(|v| v.as_bool()).unwrap_or(true)
}

/// Where HTTP fallbacks are fetched from: "cdn" (default) or "archive".
///
/// The master list carries both for nearly every file, Archive first. This only
/// changes which one the node PREFERS; the other stays as the fallback, so a
/// wrong answer here costs a little speed and never availability.
pub fn content_mode(settings: &Value) -> String {
    match settings.get("content_mode").and_then(|v| v.as_str()) {
        Some("archive") => "archive".to_string(),
        _ => "cdn".to_string(),
    }
}

/// The port the outside world can reach this node on, when it differs from the
/// port the node listens on locally.
///
/// THE PROBLEM THIS SOLVES
///
/// A node behind carrier-grade NAT — Starlink, T-Mobile Home Internet, most
/// mobile broadband — can never be dialled on IPv4, however perfectly its owner
/// configures their router, because the carrier shares one public address
/// between many homes. There is no port to forward. Buying a static public IP
/// is increasingly expensive where it is offered at all.
///
/// A VPN with port forwarding is the way through: the tunnel gives the node a
/// real public address and one real public port. The catch is that the provider
/// picks the port, it is effectively random, and it can change.
///
/// WHY A SEPARATE SETTING IS NEEDED
///
/// BitTorrent already has the mechanism for telling other peers where to find
/// you — it is the `port=` field in a tracker announce and the `port` in a DHT
/// `announce_peer`. What it does NOT assume is that this equals the port you
/// bound locally. With a VPN they are different: librqbit listens on 42800 on
/// the machine, while the world must dial 51413 on the VPN's exit address.
/// librqbit models this as `ListenerOptions::announce_port`, and this setting
/// is what fills it.
///
/// Set it to 0 or leave it unset for the normal case, where the two are the
/// same. NAT-PMP (including Proton VPN's, inside the tunnel) fills it in
/// automatically when it succeeds, so most people will never touch this.
pub fn public_port(settings: &Value) -> Option<u16> {
    // The environment wins, so a wrapper script that pulls the port from a VPN
    // CLI can hand it over without rewriting anyone's settings file.
    if let Ok(v) = std::env::var("SI_PUBLIC_PORT") {
        if let Ok(p) = v.trim().parse::<u16>() {
            if p > 0 {
                return Some(p);
            }
        }
    }
    settings
        .get("public_port")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0 && *p <= 65535)
        .map(|p| p as u16)
}

/// The port this node LISTENS on, when the user has pinned one.
///
/// Distinct from `public_port` above, and the distinction matters:
///   • `listen_port` is the local socket this process binds.
///   • `public_port` is the number peers are told to dial, which differs only
///     when something upstream (a VPN, a carrier NAT) forwards a different one.
///
/// Why pin the listening port at all. By default `Seeder::start` walks
/// LISTEN_PORT_START..=LISTEN_PORT_END and takes the first free one, which is
/// fine when UPnP or NAT-PMP opens it automatically. When nothing does, the way
/// in is a rule the operator writes by hand — an IPv4 port forward, or an IPv6
/// firewall pinhole, which on most consumer routers is the ONLY way inbound
/// IPv6 ever works. Such a rule names one port. A node that may land on any of
/// forty matches it by luck and stops matching the first time something else
/// holds that port at startup, leaving the operator unreachable with nothing in
/// the logs that looks like a cause.
///
/// SI_LISTEN_PORT overrides settings.json, so a systemd unit or container can
/// set it without owning the settings file.
///
/// Range: 1–65535. Below 1024 is privileged on Linux and macOS — a node run as
/// a normal user cannot bind it and will fall back to the default range, which
/// `Seeder::start` reports. Run as root (or grant CAP_NET_BIND_SERVICE) if you
/// genuinely need a low port. There is no limit on which ports are "allowed";
/// a node uses exactly one TCP port, and the only real constraints are the
/// privileged range and whatever else already holds the port.
pub fn listen_port(settings: &Value) -> Option<u16> {
    if let Ok(v) = std::env::var("SI_LISTEN_PORT") {
        if let Ok(p) = v.trim().parse::<u16>() {
            if p > 0 {
                return Some(p);
            }
        }
    }
    settings
        .get("listen_port")
        .and_then(|v| v.as_u64())
        .filter(|p| *p > 0 && *p <= 65535)
        .map(|p| p as u16)
}

/// The public address peers reach this node at, when we cannot infer it.
///
/// Almost always unnecessary: trackers and the DHT record the address a packet
/// ARRIVED from, so a node whose traffic egresses through a tunnel is already
/// advertised at the tunnel's address without telling anyone anything.
///
/// It exists for the reachability PROBE, which is a different question. The
/// probe edge dials the address our request came from — correct when the
/// heartbeat shares the tunnel, wrong when someone split-tunnels the torrent
/// traffic and leaves everything else on the home connection. Then the probe
/// tests the home address, finds it closed, and the node is drawn as a yellow
/// peer while being perfectly reachable.
pub fn public_ip(settings: &Value) -> Option<String> {
    if let Ok(v) = std::env::var("SI_PUBLIC_IP") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    settings
        .get("public_ip")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Ask NAT-PMP/PCP for a public port at startup (default TRUE).
///
/// This is what makes a Proton VPN setup work with no wrapper at all: Proton
/// hands its forwarded port out over NAT-PMP on 10.2.0.1 inside the tunnel, so
/// the node can simply ask for it.
pub fn natpmp_enabled(settings: &Value) -> bool {
    settings.get("natpmp_enabled").and_then(|v| v.as_bool()).unwrap_or(true)
}

/// How recently an inbound connection must have arrived for this node to still
/// count as reachable. Default 14 days.
///
/// WHY THIS EXISTS
///
/// Before 0.2.5, one inbound IPv6 connection made a node "reachable" for ever.
/// That was the right fix for the previous problem — a badge that flickered
/// between green and yellow every time the last peer disconnected was worse
/// than one that lied — but it overshot badly. A node could show a confident
/// green "full node" on the strength of a single connection a month earlier,
/// with nothing since. That is not a claim about the present tense, and it was
/// being read as one.
///
/// WHY A DATE AND NOT A RATE
///
/// The obvious alternative is a quota: so many inbound connections per day.
/// That would be wrong. Inbound RATE measures swarm demand, not reachability —
/// a perfectly reachable node in a quiet fortnight can legitimately receive
/// nothing at all, and demoting it would punish exactly the households we most
/// want to keep. A recency window absorbs a quiet stretch while still making
/// "confirmed a month ago" unable to claim anything about today.
///
/// 14 days is deliberately generous as a starting point. Once history.jsonl has
/// real data from real homes, this can be tightened with a settings change and
/// no release.
pub fn reach_confirm_days(settings: &Value) -> u64 {
    settings
        .get("reach_confirm_days")
        .and_then(|v| v.as_u64())
        .unwrap_or(14)
        .clamp(1, 365)
}

/// Total installed RAM in GB, for the tuning defaults below.
pub fn total_ram_gb() -> f64 {
    use sysinfo::System;
    let mut s = System::new();
    s.refresh_memory();
    s.total_memory() as f64 / 1024.0 / 1024.0 / 1024.0
}

/// Per-torrent CONNECTION cap, scaled to the machine.
///
/// Not to be confused with `max_peers_per_torrent`, which caps the map of
/// DISCOVERED addresses inside vendored librqbit. This is the number of live
/// connections a single torrent may hold, and librqbit applies it per torrent
/// with no session-wide ceiling — so at ~25,500 torrents it is the only thing
/// bounding total peer state.
///
/// 8 was measured on a 4 GB Pi, where 24 walked the resident set from 1.1 GB at
/// 11 peers to 3.1 GB at 79 and then out of memory. That number has since been
/// applied to every machine, including a Mac mini M4 with 16 GB doing nothing
/// else — which is simply leaving capacity on the floor. The Pi keeps its 8.
pub fn peer_limit_per_torrent(settings: &Value) -> usize {
    if let Some(n) = settings.get("peer_limit_per_torrent").and_then(|v| v.as_u64()) {
        return (n as usize).clamp(2, 200);
    }
    let gb = total_ram_gb();
    if gb >= 15.0 {
        32
    } else if gb >= 7.0 {
        16
    } else {
        8
    }
}

/// Default live-window size, scaled the same way. Explicit `active_torrents`
/// always wins; this only changes what "unset" means.
pub fn active_torrents_default() -> usize {
    let gb = total_ram_gb();
    if gb >= 15.0 {
        8000
    } else if gb >= 7.0 {
        4000
    } else {
        2000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `shard_for` decides the on-disk folder for every single file in the
    /// library. One wrong character and 30,000 files silently relocate: the
    /// node then reports 0% coverage, re-downloads 412 GB it already has, and
    /// nothing anywhere logs an error — it just looks like a node that never
    /// finished. Worth a test.
    #[test]
    fn shard_is_stable_and_two_chars() {
        for name in [
            "gGl7g6A_0WI-IeZJ.mp3",
            "xuD_ySQ8N_bIgByN.mp4",
            "A.mp3",
            "",
            "_-.mp3",
            "ÉÀ-accented.mp3",
        ] {
            let a = shard_for(name);
            assert_eq!(a, shard_for(name), "shard must be deterministic: {name:?}");
            assert_eq!(a.chars().count(), 2, "shard must be 2 chars: {name:?} -> {a:?}");
        }
        // Different names with the same prefix share a shard; that is the point.
        assert_eq!(shard_for("abZZ.mp3"), shard_for("abYY.mp3"));
        assert_ne!(shard_for("abZZ.mp3"), shard_for("zzYY.mp3"));
    }

    #[test]
    fn parses_times_and_days() {
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("23:59"), Some(23 * 60 + 59));
        assert_eq!(parse_hhmm("08:30"), Some(510));
        assert_eq!(parse_hhmm("24:00"), None);
        assert_eq!(parse_hhmm("8:30am"), None);
        assert_eq!(parse_hhmm(""), None);
        assert!(parse_days("sun").is_some());
        assert!(parse_days("sun,wed").is_some());
        assert!(parse_days("everyday").is_some());
        assert!(parse_days("blursday").is_none());
    }

    /// A quiet window that crosses midnight is exactly where this kind of logic
    /// breaks, and getting it wrong is not cosmetic: the node either seeds
    /// straight through a church's livestream, or goes silent for 22 hours a day
    /// and nobody can work out why its coverage never moves.
    #[test]
    fn quiet_window_crossing_midnight_is_handled() {
        let s = json!({
            "quiet_hours": [{ "days": ["sun"], "start": "22:00", "end": "02:00" }]
        });
        let w = quiet_windows(&s);
        assert_eq!(w.len(), 1, "one window should parse");
        let w = &w[0];
        assert!(
            w.start > w.end,
            "22:00-02:00 must be stored as a wrapping window (start {} > end {}), \
             not silently normalised into an empty one",
            w.start, w.end
        );
        // An empty schedule is never quiet, whatever the clock says.
        assert!(quiet_now(&json!({})).is_none());
        assert!(quiet_now(&json!({ "quiet_hours": [] })).is_none());
    }

    #[test]
    fn upload_cap_needs_both_switch_and_value() {
        assert!(upload_limit_bps(&json!({})).is_none());
        assert!(upload_limit_bps(&json!({ "upload_limit_kbps": 500 })).is_none(),
                "a rate with the switch off must stay unlimited");
        assert!(upload_limit_bps(&json!({ "upload_limit_enabled": true, "upload_limit_kbps": 0 })).is_none());
        assert_eq!(
            upload_limit_bps(&json!({ "upload_limit_enabled": true, "upload_limit_kbps": 500 })),
            Some(512_000)
        );
    }

    #[test]
    fn monthly_cap_needs_both_too() {
        assert!(monthly_cap_bytes(&json!({ "monthly_cap_gb": 500 })).is_none());
        assert_eq!(
            monthly_cap_bytes(&json!({ "monthly_cap_enabled": true, "monthly_cap_gb": 1 })),
            Some(1_073_741_824)
        );
    }

    #[test]
    fn peer_limit_respects_an_explicit_setting_and_clamps_it() {
        assert_eq!(peer_limit_per_torrent(&json!({ "peer_limit_per_torrent": 24 })), 24);
        assert_eq!(peer_limit_per_torrent(&json!({ "peer_limit_per_torrent": 0 })), 2);
        assert_eq!(peer_limit_per_torrent(&json!({ "peer_limit_per_torrent": 9999 })), 200);
        // Unset scales to the machine; whatever it picks must be sane.
        let auto = peer_limit_per_torrent(&json!({}));
        assert!((8..=32).contains(&auto), "auto peer limit out of range: {auto}");
    }
}

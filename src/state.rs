//! Shared runtime state + the /stats snapshot the dashboard reads, plus the
//! download-state.json ledger and the cached network view (/api/node/map + stats).

use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use crate::config;

const TREND: usize = 90; // ~3 min of 2s samples

pub struct Shared {
    pub node_id: String,
    pub started: Instant,
    pub scope: String,
    pub held: AtomicU64,
    pub total: AtomicU64,
    /// Real bytes on disk of everything this node currently holds — the sum of
    /// master-list entry sizes for files verified present at their signed size.
    /// This is what the heartbeat reports as `storage_used_bytes`; the admin
    /// dashboard's Storage figures are a straight SUM of that column across
    /// nodes, so if this stays 0 every Storage card on the network reads 0.
    pub storage_bytes: AtomicU64,
    /// Best-effort cumulative bytes uploaded to peers, integrated from the seed
    /// session's instantaneous upload rate (librqbit exposes no lifetime total
    /// we can dig out reliably, so the stats poller accumulates up_bps × the
    /// poll interval each tick). Reported as `uploaded_bytes` — the admin's
    /// "Data Transferred" / "Uploaded across granted seeds".
    pub uploaded_bytes: AtomicU64,
    pub peers: AtomicU64,
    /// Peers that DIALLED US, summed across live torrents (all address
    /// families). This is the only honest answer to "am I actually serving the
    /// internet, or just taking from it?" — an outbound connection proves our
    /// egress works and nothing more. Feeds the dashboard's "peers served".
    pub peers_in: AtomicU64,
    /// Peers this node dialled out to.
    pub peers_out: AtomicU64,
    /// High-water mark of `peers_in` for this run. `peers_in` is a snapshot and
    /// falls back to 0 whenever the live window rotates away from the torrents
    /// those peers were on; the previous UI read that momentary 0 as "you have
    /// helped nobody", which was simply wrong. This never decreases.
    pub peers_in_peak: AtomicU64,
    /// STICKY: a peer at a global-unicast IPv6 address has dialled this node.
    ///
    /// Sticky on purpose, and persisted across restarts (see load/save_v6_proof).
    /// Reachability is a "has this ever been demonstrated" fact, not a momentary
    /// one — a probe can fail for reasons that have nothing to do with the
    /// operator, a real inbound connection cannot. A flag that flickered off
    /// every time the last peer disconnected would be worse than no flag.
    pub v6_inbound_seen: AtomicBool,
    /// When we first saw it (unix seconds), so the dashboard can date the claim
    /// instead of stating a months-old fact in the present tense.
    pub v6_inbound_at: AtomicU64,
    pub torrents: AtomicU64,
    /// The TCP port the seed session really bound (0 = not seeding). Set once
    /// at startup from `Seeder::port()`; everything that reports a port — the
    /// reachability probe, the heartbeat, /stats — must read THIS, never the
    /// LISTEN_PORT_START constant.
    pub listen_port: AtomicU64,
    /// The port bound on this machine. Usually identical to `listen_port`; it
    /// differs when a VPN or upstream NAT gives the node a different public
    /// port. Kept separately so the dashboard can show both — "listening on
    /// 42800, reachable as 51413" is a sentence somebody debugging a tunnel
    /// needs, and collapsing the two loses it.
    pub local_port: AtomicU64,
    pub seed_up_bps: AtomicU64, // from the torrent session (0 if seeding off)
    pub reachable: AtomicU8,    // 0 = unknown, 1 = reachable (v4 or v6), 2 = closed
    pub seed_granted: AtomicBool, // admin has flipped seed_access.enabled for us
    pub downloading: AtomicBool,
    /// The library drive is full. Downloading stops; seeding does NOT — a full
    /// disk is a reason to stop growing, never a reason to stop serving what is
    /// already there. Cleared automatically once space reappears.
    pub disk_full: AtomicBool,
    /// Scheduled quiet hours are in effect right now — seeding, serving and
    /// downloading are suspended so the connection is free (church livestream).
    pub quiet: AtomicBool,
    pub network: Mutex<Value>,      // merged /api/node/map + /api/node/stats
    pub stats_json: Mutex<String>,  // the rendered /stats payload the server serves
    pub trends: Mutex<Trends>,
    /// Newer released version, when one exists. Notify-only by design: a
    /// headless seed box that restarts itself mid-serve is worse than one that
    /// is a version behind, so nothing here ever installs anything.
    pub update_available: Mutex<Option<String>>,
    /// Master-list version currently loaded, so the refresh loop and the
    /// dashboard agree on what the node actually knows about.
    pub masterlist_version: AtomicU64,
    /// NAT-PMP / PCP mapping state: "trying" | "mapped via <gw>" | "unavailable"
    /// | "off". Reported so a node owner can tell the difference between "my
    /// router refused" and "we never tried".
    /// `Arc` rather than a bare `Mutex` because the renewal task in natpmp.rs
    /// owns a handle to it for the life of the process and writes the result of
    /// each attempt; `Shared` only ever reads it.
    pub natpmp: std::sync::Arc<Mutex<String>>,
    /// A peer-check result waiting to ride along on the next heartbeat. Held
    /// rather than posted immediately so the whole exchange costs no extra
    /// requests at all — the beat is happening anyway.
    pub pending_check: Mutex<Option<Value>>,
    /// Last 7 days of hourly history, newest last — the rendered form of
    /// history.jsonl, served at /stats for the dashboard's charts.
    pub history: Mutex<Value>,
}

#[derive(Default)]
pub struct Trends {
    pub up_bps: VecDeque<f64>,
    pub down_bps: VecDeque<f64>,
    pub cpu: VecDeque<f64>,
    pub temp: VecDeque<f64>,
    pub peers: VecDeque<f64>,
    pub peers_in: VecDeque<f64>,
    pub peers_out: VecDeque<f64>,
    pub net_up: VecDeque<f64>,
}

impl Trends {
    fn push(dq: &mut VecDeque<f64>, v: f64) {
        dq.push_back((v * 100.0).round() / 100.0);
        while dq.len() > TREND {
            dq.pop_front();
        }
    }
    fn to_json(&self) -> Value {
        let f = |dq: &VecDeque<f64>| dq.iter().copied().collect::<Vec<_>>();
        json!({
            "up_bps": f(&self.up_bps), "down_bps": f(&self.down_bps),
            "cpu": f(&self.cpu), "temp": f(&self.temp),
            "peers": f(&self.peers), "net_up": f(&self.net_up),
            "peers_in": f(&self.peers_in), "peers_out": f(&self.peers_out),
        })
    }
}

impl Shared {
    pub fn new(node_id: String, scope: String) -> Self {
        Shared {
            node_id,
            started: Instant::now(),
            scope,
            held: AtomicU64::new(0),
            total: AtomicU64::new(0),
            storage_bytes: AtomicU64::new(0),
            uploaded_bytes: AtomicU64::new(0),
            peers: AtomicU64::new(0),
            peers_in: AtomicU64::new(0),
            peers_out: AtomicU64::new(0),
            peers_in_peak: AtomicU64::new(0),
            v6_inbound_seen: AtomicBool::new(false),
            v6_inbound_at: AtomicU64::new(0),
            torrents: AtomicU64::new(0),
            listen_port: AtomicU64::new(0),
            local_port: AtomicU64::new(0),
            seed_up_bps: AtomicU64::new(0),
            reachable: AtomicU8::new(0),
            seed_granted: AtomicBool::new(false),
            downloading: AtomicBool::new(true),
            disk_full: AtomicBool::new(false),
            quiet: AtomicBool::new(false),
            network: Mutex::new(json!({})),
            stats_json: Mutex::new("{}".to_string()),
            trends: Mutex::new(Trends::default()),
            update_available: Mutex::new(None),
            natpmp: std::sync::Arc::new(Mutex::new("off".to_string())),
            pending_check: Mutex::new(None),
            masterlist_version: AtomicU64::new(0),
            history: Mutex::new(json!([])),
        }
    }

    /// Record a direction scan. `peers_in_peak` only ever rises.
    pub fn set_directions(&self, incoming: u64, outgoing: u64) {
        self.peers_in.store(incoming, Ordering::Relaxed);
        self.peers_out.store(outgoing, Ordering::Relaxed);
        self.peers_in_peak.fetch_max(incoming, Ordering::Relaxed);
    }

    /// Record that a global-IPv6 peer dialled in. Monotonic: this can only ever
    /// turn ON. Persists immediately, because the whole value of the fact is
    /// that it survives the restart that follows.
    pub fn note_v6_inbound(&self) {
        let now = now_secs();
        let first = !self.v6_inbound_seen.swap(true, Ordering::Relaxed);
        if !first {
            // Already known — but REFRESH the date. This is the whole point of
            // 0.2.5: the flag is not the claim, the date is. Only rewrite the
            // file when the day has actually changed, so a busy node is not
            // writing to an SD card every minute.
            let prev = self.v6_inbound_at.load(Ordering::Relaxed);
            self.v6_inbound_at.store(now, Ordering::Relaxed);
            if now / 86_400 != prev / 86_400 {
                save_v6_proof(now);
            }
            return;
        }
        self.v6_inbound_at.store(now, Ordering::Relaxed);
        save_v6_proof(now);
        println!(
            "[reach] a peer connected to you over IPv6 — your node is reachable.\n\
             [reach] Nothing to forward, nothing to change. This is the normal good\n\
             [reach] result on Starlink, T-Mobile Home Internet and mobile broadband."
        );
    }

    /// The honest reachability answer, combining both kinds of evidence.
    ///
    /// ACTIVE  — the probe edge dialled us and got through (IPv4 only, in
    ///           practice: the edge has no outbound IPv6).
    /// PASSIVE — a real peer dialled us over global IPv6.
    ///
    /// Before this, the CLI reported `open || open_v6`, and `open_v6` can never
    /// be true for anybody. So it collapsed to the IPv4 answer alone, and every
    /// node behind CGNAT — which is most households on Starlink or mobile
    /// broadband, exactly the people we are asking to run one — was filed as an
    /// unreachable "peer" however reachable it actually was.
    pub fn reachable_verdict(&self, probe: Option<bool>) -> Option<bool> {
        if self.v6_confirmed_recently() {
            return Some(true);
        }
        probe
    }

    /// Has a peer dialled in over IPv6 within the confirmation window?
    ///
    /// The FACT that it once happened is kept for ever — it is true, and the UI
    /// still shows the date. What expires is the licence to state it in the
    /// present tense. See config::reach_confirm_days for why this is a recency
    /// window and not a rate.
    pub fn v6_confirmed_recently(&self) -> bool {
        if !self.v6_inbound_seen.load(Ordering::Relaxed) {
            return false;
        }
        let at = self.v6_inbound_at.load(Ordering::Relaxed);
        if at == 0 {
            // Seen, but we never recorded when — an old state file. Treat it as
            // current rather than silently demoting somebody on an upgrade;
            // the next scan will stamp a real date on it.
            return true;
        }
        let window = config::reach_confirm_days(&config::load_settings()) * 86_400;
        now_secs().saturating_sub(at) <= window
    }

    /// Days since the last inbound IPv6 connection, or None if there never was
    /// one. For wording like "last confirmed 34 days ago".
    pub fn v6_age_days(&self) -> Option<u64> {
        let at = self.v6_inbound_at.load(Ordering::Relaxed);
        if !self.v6_inbound_seen.load(Ordering::Relaxed) || at == 0 {
            return None;
        }
        Some(now_secs().saturating_sub(at) / 86_400)
    }

    /// Record the latest reachability self-test (None = unknown/probe failed).
    pub fn set_reachable(&self, r: Option<bool>) {
        let code = match r {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        };
        self.reachable.store(code, Ordering::Relaxed);
    }

    /// "reachable" | "closed" | "unknown" for the dashboard.
    pub fn reachable_label(&self) -> &'static str {
        match self.reachable.load(Ordering::Relaxed) {
            1 => "reachable",
            2 => "closed",
            _ => "unknown",
        }
    }

    /// Record whether the admin has granted this node seed status.
    pub fn set_granted(&self, granted: bool) {
        self.seed_granted.store(granted, Ordering::Relaxed);
    }

    /// The node's own category, matching the server: seed (granted + reachable),
    /// node (reachable), peer (otherwise).
    pub fn category(&self) -> &'static str {
        let reachable = self.reachable.load(Ordering::Relaxed) == 1;
        if self.seed_granted.load(Ordering::Relaxed) && reachable {
            "seed"
        } else if reachable {
            "node"
        } else {
            "peer"
        }
    }

    pub fn coverage_pct(&self) -> f64 {
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        let held = self.held.load(Ordering::Relaxed) as f64;
        ((held / total as f64) * 1000.0).round() / 10.0
    }

    /// Rebuild the /stats JSON from a fresh system snapshot + counters + network.
    pub fn render(&self, sys: &crate::system::Snapshot) {
        let held = self.held.load(Ordering::Relaxed);
        let peers = self.peers.load(Ordering::Relaxed);
        // Seeding upload from the torrent session if present, else the NIC's upload.
        let seed_up = self.seed_up_bps.load(Ordering::Relaxed) as f64;
        let up_bps = if seed_up > 0.0 { seed_up } else { sys.up_bps };

        {
            let mut t = self.trends.lock().unwrap();
            Trends::push(&mut t.up_bps, up_bps);
            Trends::push(&mut t.down_bps, sys.down_bps);
            Trends::push(&mut t.cpu, sys.cpu_pct as f64);
            Trends::push(&mut t.temp, sys.temp_c.unwrap_or(0.0) as f64);
            Trends::push(&mut t.peers, peers as f64);
            Trends::push(&mut t.peers_in, self.peers_in.load(Ordering::Relaxed) as f64);
            Trends::push(&mut t.peers_out, self.peers_out.load(Ordering::Relaxed) as f64);
            Trends::push(&mut t.net_up, up_bps);
        }
        let trends = self.trends.lock().unwrap().to_json();
        let network = self.network.lock().unwrap().clone();

        let node = json!({
            "available": true,
            // The port actually bound, not the constant (see Shared::listen_port).
            "port": match self.listen_port.load(Ordering::Relaxed) {
                0 => config::LISTEN_PORT_START as u64,
                p => p,
            },
            "peers": peers,
            "uptime_s": self.started.elapsed().as_secs(),
            "coverage_pct": self.coverage_pct(),
            "held": held,
            "catalog": self.total.load(Ordering::Relaxed),
            // Same figures the heartbeat sends, so the local dashboard and the
            // central admin can be compared directly when they disagree.
            "storage_bytes": self.storage_bytes.load(Ordering::Relaxed),
            "uploaded_bytes": self.uploaded_bytes.load(Ordering::Relaxed),
            "reachable": self.reachable_label(),
            "quiet": self.quiet.load(Ordering::Relaxed),
            "disk_full": self.disk_full.load(Ordering::Relaxed),
            "seed_granted": self.seed_granted.load(Ordering::Relaxed),
            "category": self.category(),
            // Direction. `peers` above is a bare total and cannot answer the
            // question that matters; these can.
            "peers_in": self.peers_in.load(Ordering::Relaxed),
            "peers_out": self.peers_out.load(Ordering::Relaxed),
            "peers_in_peak": self.peers_in_peak.load(Ordering::Relaxed),
                "v6_inbound_seen": self.v6_inbound_seen.load(Ordering::Relaxed),
            "v6_inbound_at": self.v6_inbound_at.load(Ordering::Relaxed),
            "v6_confirmed": self.v6_confirmed_recently(),
            "v6_age_days": self.v6_age_days(),
            "masterlist_version": self.masterlist_version.load(Ordering::Relaxed),
            "update_available": self.update_available.lock().unwrap().clone(),
            "natpmp": self.natpmp.lock().unwrap().clone(),
            "local_port": self.local_port.load(Ordering::Relaxed),
        });
        let system = json!({
            "cpu_pct": (sys.cpu_pct as f64 * 10.0).round() / 10.0,
            // Round in f64 space so it serialises cleanly as e.g. 58.4 rather than
            // the f32-widened 58.400001525878906.
            "temp_c": sys.temp_c.map(|t| (t as f64 * 10.0).round() / 10.0),
            "mem_used": sys.mem_used, "mem_total": sys.mem_total,
            "disk_used": sys.disk_used, "disk_total": sys.disk_total,
            "nic": sys.nic, "up_bps": up_bps, "down_bps": sys.down_bps,
        });
        let history = self.history.lock().unwrap().clone();
        let out = json!({
            "node": node, "system": system, "network": network,
            "trends": trends, "history": history, "at": now_secs(),
        });
        *self.stats_json.lock().unwrap() = out.to_string();
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// download-state.json — { "<id>": { "downloaded": true, "diskSize": N } }.
pub fn load_download_state() -> Value {
    let p = config::data_dir().join("download-state.json");
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

pub fn save_download_state(v: &Value) {
    let dir = config::data_dir();
    std::fs::create_dir_all(&dir).ok();
    let tmp = dir.join("download-state.json.part");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, dir.join("download-state.json"));
    }
}

/// Merge the live /api/node/map + /api/node/stats into the shape the dashboard
/// expects (nodes[], online_count, total_sermons, seeds, storage, countries...).
pub fn build_network_view(map: &Value, stats: &Value, my_node_id: &str) -> Value {
    let empty = vec![];
    let raw = map.get("nodes").and_then(|v| v.as_array()).unwrap_or(&empty);
    let mut nodes = Vec::new();
    for n in raw {
        let cat = n
            .get("category")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if n.get("type").and_then(|v| v.as_str()) == Some("seed") {
                    "seed".into()
                } else if n.get("reachable").and_then(|v| v.as_i64()).unwrap_or(0) != 0 {
                    "node".into()
                } else {
                    "peer".into()
                }
            });
        let cov = n.get("coverage").and_then(|v| v.as_f64()).unwrap_or(0.0);
        nodes.push(json!({
            "name": n.get("city").and_then(|v| v.as_str()).unwrap_or(""),
            "self": n.get("id").and_then(|v| v.as_str()) == Some(my_node_id),
            "online": true,
            "category": cat,
            "lat": n.get("lat"), "lon": n.get("lon"),
            "city": n.get("city").and_then(|v| v.as_str()).unwrap_or(""),
            "country": n.get("country").and_then(|v| v.as_str()).unwrap_or(""),
            "sermons": n.get("files").and_then(|v| v.as_u64()).unwrap_or(0),
            "coverage": cov.clamp(0.0, 100.0),
        }));
    }
    json!({
        "nodes": nodes,
        "online_count": stats.get("totalNodes").and_then(|v| v.as_u64()).unwrap_or(nodes_len(map)),
        "total_count": stats.get("totalNodesEver").and_then(|v| v.as_u64()).unwrap_or(nodes_len(map)),
        "seeds": stats.get("seedNodes").and_then(|v| v.as_u64()).unwrap_or(0),
        "total_sermons": stats.get("totalFiles").and_then(|v| v.as_u64()).unwrap_or(0),
        "storage": stats.get("totalStorage").and_then(|v| v.as_u64()).unwrap_or(0),
        "countries": stats.get("countries").and_then(|v| v.as_u64()).unwrap_or(0),
        "net_peers": stats.get("peers").and_then(|v| v.as_u64()).unwrap_or(0),
        "up_bps": 0,
    })
}

fn nodes_len(map: &Value) -> u64 {
    map.get("nodes").and_then(|v| v.as_array()).map(|a| a.len() as u64).unwrap_or(0)
}

// ─────────────────────────────────────────────────────────────────────────────
// history.jsonl — the long view
//
// `Trends` above holds 90 samples at 2 s: three minutes, in RAM, gone on
// restart. That is a live needle, not a record, and it cannot answer "over a
// week, how much did I give versus take?" — which is the question a person
// running a seed node actually has.
//
// So: one JSON object per line, appended hourly, in the data dir. Append-only
// is the right shape here. It is crash-safe by construction (a torn final line
// is skipped on read, not fatal), it costs one open/write/close an hour, and
// on a Pi's SD card that is nothing. Trimmed to RETAIN_DAYS on write so the
// file cannot grow without bound.
// ─────────────────────────────────────────────────────────────────────────────

const RETAIN_DAYS: u64 = 90;
const HISTORY_WINDOW_DAYS: u64 = 7;

fn history_path() -> std::path::PathBuf {
    config::data_dir().join("history.jsonl")
}

/// One hourly sample. `up`/`down` are cumulative counters, not rates, so a
/// reader can difference two rows and get real bytes for that hour even if the
/// node was asleep in between.
pub fn append_history(shared: &Shared, up_total: u64, down_total: u64) {
    let row = json!({
        "ts": now_secs(),
        "peers_in": shared.peers_in.load(Ordering::Relaxed),
        "peers_out": shared.peers_out.load(Ordering::Relaxed),
        "peers_in_peak": shared.peers_in_peak.load(Ordering::Relaxed),
        "peers": shared.peers.load(Ordering::Relaxed),
        "up": up_total,
        "down": down_total,
        "held": shared.held.load(Ordering::Relaxed),
        "torrents": shared.torrents.load(Ordering::Relaxed),
        "reachable": shared.reachable_label(),
    });

    let dir = config::data_dir();
    std::fs::create_dir_all(&dir).ok();
    let path = history_path();

    // Read, trim, rewrite when the file has aged past RETAIN_DAYS; otherwise
    // just append. The trim is rare (once a day at most, and only after three
    // months of uptime), so the common path stays a bare append.
    let cutoff = now_secs().saturating_sub(RETAIN_DAYS * 86_400);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let needs_trim = existing
        .lines()
        .next()
        .and_then(|l| serde_json::from_str::<Value>(l).ok())
        .and_then(|v| v.get("ts").and_then(|t| t.as_u64()))
        .map(|ts| ts < cutoff)
        .unwrap_or(false);

    if needs_trim {
        let kept: String = existing
            .lines()
            .filter(|l| {
                serde_json::from_str::<Value>(l)
                    .ok()
                    .and_then(|v| v.get("ts").and_then(|t| t.as_u64()))
                    .map(|ts| ts >= cutoff)
                    .unwrap_or(false)
            })
            .map(|l| format!("{l}\n"))
            .collect();
        let tmp = dir.join("history.jsonl.part");
        if std::fs::write(&tmp, kept + &row.to_string() + "\n").is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
        return;
    }

    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{row}");
    }
}

/// The last `HISTORY_WINDOW_DAYS` of samples, oldest first, for /stats.
///
/// A malformed line (a half-written row from a power cut) is skipped rather
/// than failing the read — the whole point of the format.
pub fn load_history() -> Value {
    let cutoff = now_secs().saturating_sub(HISTORY_WINDOW_DAYS * 86_400);
    let text = match std::fs::read_to_string(history_path()) {
        Ok(t) => t,
        Err(_) => return json!([]),
    };
    let rows: Vec<Value> = text
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v.get("ts").and_then(|t| t.as_u64()).unwrap_or(0) >= cutoff)
        .collect();
    json!(rows)
}

/// The IPv6 reachability proof, kept in its own tiny file.
///
/// Not in settings.json on purpose: that file is edited by hand, by the desktop
/// app, and by `config`, and a measurement has no business sharing a file with
/// preferences. Losing this costs a node its green status until the next peer
/// arrives, which is a recoverable inconvenience rather than a problem.
pub fn load_v6_proof() -> Option<u64> {
    std::fs::read_to_string(config::data_dir().join("v6-inbound"))
        .ok()
        .and_then(|t| t.trim().parse::<u64>().ok())
        .filter(|ts| *ts > 0)
}

pub fn save_v6_proof(ts: u64) {
    let dir = config::data_dir();
    std::fs::create_dir_all(&dir).ok();
    let _ = std::fs::write(dir.join("v6-inbound"), ts.to_string());
}

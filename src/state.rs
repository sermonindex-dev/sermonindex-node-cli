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
    pub torrents: AtomicU64,
    pub seed_up_bps: AtomicU64, // from the torrent session (0 if seeding off)
    pub reachable: AtomicU8,    // 0 = unknown, 1 = reachable (v4 or v6), 2 = closed
    pub seed_granted: AtomicBool, // admin has flipped seed_access.enabled for us
    pub downloading: AtomicBool,
    /// Scheduled quiet hours are in effect right now — seeding, serving and
    /// downloading are suspended so the connection is free (church livestream).
    pub quiet: AtomicBool,
    pub network: Mutex<Value>,      // merged /api/node/map + /api/node/stats
    pub stats_json: Mutex<String>,  // the rendered /stats payload the server serves
    pub trends: Mutex<Trends>,
}

#[derive(Default)]
pub struct Trends {
    pub up_bps: VecDeque<f64>,
    pub down_bps: VecDeque<f64>,
    pub cpu: VecDeque<f64>,
    pub temp: VecDeque<f64>,
    pub peers: VecDeque<f64>,
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
            torrents: AtomicU64::new(0),
            seed_up_bps: AtomicU64::new(0),
            reachable: AtomicU8::new(0),
            seed_granted: AtomicBool::new(false),
            downloading: AtomicBool::new(true),
            quiet: AtomicBool::new(false),
            network: Mutex::new(json!({})),
            stats_json: Mutex::new("{}".to_string()),
            trends: Mutex::new(Trends::default()),
        }
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
            Trends::push(&mut t.net_up, up_bps);
        }
        let trends = self.trends.lock().unwrap().to_json();
        let network = self.network.lock().unwrap().clone();

        let node = json!({
            "available": true,
            "port": config::LISTEN_PORT_START,
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
            "seed_granted": self.seed_granted.load(Ordering::Relaxed),
            "category": self.category(),
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
        let out = json!({
            "node": node, "system": system, "network": network,
            "trends": trends, "at": now_secs(),
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

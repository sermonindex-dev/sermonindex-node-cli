//! BitTorrent seeding via librqbit — a faithful port of the desktop app's
//! `torrent_node.rs` seed-in-place path, minus all Tauri coupling. Compiled only
//! with the `seed` feature. Each downloaded file is hashed into its canonical
//! torrent (2 MiB pieces, name = "<id>.mp3") which reproduces the master-list
//! info_hash, then added with overwrite:true so librqbit hash-checks it, sees
//! 100%, and seeds it to the same swarm.
#![cfg(feature = "seed")]

use anyhow::{anyhow, Result};
use std::num::NonZeroU32;
use std::path::Path;
use std::sync::Arc;

use librqbit::limits::LimitsConfig;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    create_torrent, AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerMode,
    ListenerOptions, Session, SessionOptions,
};

use crate::config::{LISTEN_PORT_END, LISTEN_PORT_START};

const BLOCKING_THREADS: usize = 2;
/// Max peers per torrent.
///
/// IMPORTANT: librqbit has NO session-wide connection cap — `peer_limit` is
/// applied PER TORRENT (see SessionOptions: "Default peer limit per torrent").
/// A seed holding the full audio scope runs ~25,500 torrents at once, so this
/// number is the only thing bounding total peer state, and the ceiling it
/// implies is the whole session's worst case. At 24 the resident set on a
/// 4 GB Pi climbed steadily with peer count (1.1 GB @ 11 peers -> 3.1 GB @ 79)
/// until the machine ran out of memory and the display locked up. 8 still
/// leaves ample slots to serve any realistic swarm — a seed rarely needs more
/// than a couple of simultaneous leechers for one sermon — while cutting the
/// per-torrent worst case by two thirds.
const PEER_LIMIT_PER_TORRENT: usize = 8;
/// How many torrents hash-check/initialise at once on startup. Low = no memory
/// or CPU spike when a full library comes up (the old freeze window).
const CONCURRENT_INIT_LIMIT: usize = 4;

fn trackers() -> Vec<String> {
    vec![
        "udp://tracker.opentrackr.org:1337/announce".into(),
        "udp://open.demonii.com:1337/announce".into(),
        "udp://tracker.torrent.eu.org:451/announce".into(),
        "udp://exodus.desync.com:6969/announce".into(),
    ]
}

fn session_options(
    port: u16,
    ipv6: bool,
    enable_dht: bool,
    upload_bps: Option<NonZeroU32>,
) -> SessionOptions {
    let listen_addr: std::net::SocketAddr = if ipv6 {
        (std::net::Ipv6Addr::UNSPECIFIED, port).into()
    } else {
        (std::net::Ipv4Addr::UNSPECIFIED, port).into()
    };
    SessionOptions {
        fastresume: true,
        ratelimits: LimitsConfig {
            upload_bps,
            download_bps: None,
        },
        persistence: None,
        // Default enables DHT (Some(default)); disable it only as a last-resort
        // fallback on hosts where the DHT UDP socket can't bind. Trackers still work.
        dht: if enable_dht { Some(Default::default()) } else { None },
        listen: Some(ListenerOptions {
            mode: ListenerMode::TcpOnly,
            listen_addr,
            enable_upnp_port_forwarding: true,
            ipv4_only: !ipv6,
            ..Default::default()
        }),
        // ── Low-footprint tuning (matters a LOT at 25k+ torrents on a Pi) ──
        // Cap peers per torrent so connection buffers/state can't balloon; a seed
        // only needs a handful of leechers per swarm at a time.
        peer_limit: Some(PEER_LIMIT_PER_TORRENT),
        // Multicast LSD across tens of thousands of torrents is pure noise on a
        // home LAN — turn it off. DHT + trackers still find peers.
        disable_local_service_discovery: true,
        // Hash-check/initialise only a few torrents at a time on startup so the
        // scan can't spike memory/CPU (the old OOM-freeze window).
        concurrent_init_limit: Some(CONCURRENT_INIT_LIMIT),
        ..Default::default()
    }
}

fn port_unavailable(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}").to_lowercase();
    s.contains("address already in use")
        || s.contains("eaddrinuse")
        || s.contains("only one usage of each socket address")
}

/// The host lacks this address family (e.g. no IPv6) — try the other one.
fn af_unsupported(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}").to_lowercase();
    s.contains("address family not supported")
        || s.contains("os error 97")
        || s.contains("af_not_supported")
}

pub struct Seeder {
    session: Arc<Session>,
    spawner: BlockingSpawner,
}

impl Seeder {
    /// Start a session, walking the listen-port range like the app does.
    pub async fn start(download_dir: &Path, upload_bps: Option<u32>, dht: bool) -> Result<Seeder> {
        std::fs::create_dir_all(download_dir).ok();
        let cap = upload_bps.and_then(NonZeroU32::new);
        // DHT is OFF by default — see config::dht_enabled() for the measurements.
        // It is the only component whose memory keeps growing with torrent count
        // and uptime; trackers cost a fixed amount and then stay flat.
        let combos = if dht {
            [(true, true), (false, true), (false, false)]
        } else {
            [(true, false), (false, false), (false, false)]
        };
        let mut last = String::new();
        for port in LISTEN_PORT_START..=LISTEN_PORT_END {
            for (ipv6, dht) in combos {
                match Session::new_with_opts(
                    download_dir.to_path_buf(),
                    session_options(port, ipv6, dht, cap),
                )
                .await
                {
                    Ok(s) => {
                        eprintln!(
                            "[seed] session listening on TCP {port} (ipv6={ipv6}, dht={dht})"
                        );
                        return Ok(Seeder {
                            session: s,
                            spawner: BlockingSpawner::new(BLOCKING_THREADS),
                        });
                    }
                    Err(e) => {
                        last = format!("{e:#}");
                        if port_unavailable(&e) {
                            break; // this port is taken — move to the next port
                        }
                        // af_unsupported or a DHT socket error → try the next combo
                        let _ = af_unsupported(&e);
                    }
                }
            }
        }
        Err(anyhow!(
            "could not start torrent session in {LISTEN_PORT_START}..{LISTEN_PORT_END} (last: {last})"
        ))
    }

    /// Seed one completed file in place.
    ///
    /// `known_info_hash` — the master-list info_hash for this file, if the
    /// caller has it. When a `.torrent` this node wrote on a previous run is
    /// cached under that hash, the expensive `create_torrent()` re-hash of the
    /// whole file is skipped (at 25k files that re-hash was ~300 GB of disk
    /// reads on EVERY startup). `add_torrent` still hash-checks the data itself
    /// (`overwrite: true`), so a corrupted file can never silently seed.
    ///
    /// `start_paused` — register without going live. A paused torrent keeps its
    /// verified pieces but holds no peer/announce state; the rotation task
    /// brings it live when its window comes up.
    pub async fn seed_file(
        &self,
        file_path: &Path,
        torrents_dir: &Path,
        known_info_hash: Option<&str>,
        start_paused: bool,
    ) -> Result<()> {
        if !file_path.is_file() {
            return Err(anyhow!("not a file: {}", file_path.display()));
        }

        // Fast path: reuse the cached .torrent from a previous run.
        let cached: Option<Vec<u8>> = known_info_hash.and_then(|h| {
            std::fs::read(torrents_dir.join(format!("{}.torrent", h.to_lowercase()))).ok()
        });

        let bytes = match cached {
            Some(b) => b,
            None => {
                let display_name = file_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .ok_or_else(|| anyhow!("bad file name"))?
                    .to_string();

                let created = create_torrent(
                    file_path,
                    CreateTorrentOptions {
                        name: Some(&display_name),
                        ..Default::default()
                    },
                    &self.spawner,
                )
                .await
                .map_err(|e| anyhow!("create_torrent: {e:#}"))?;

                let info_hash = created.info_hash().as_string();
                let bytes = created
                    .as_bytes()
                    .map_err(|e| anyhow!("serialize torrent: {e:#}"))?;

                std::fs::create_dir_all(torrents_dir).ok();
                let _ =
                    std::fs::write(torrents_dir.join(format!("{info_hash}.torrent")), &bytes);
                bytes.to_vec()
            }
        };

        let output_folder = file_path
            .parent()
            .ok_or_else(|| anyhow!("no parent dir"))?
            .to_string_lossy()
            .to_string();

        self.session
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    overwrite: true,
                    paused: start_paused,
                    output_folder: Some(output_folder),
                    // Stored on the torrent even when adding paused, so the
                    // announce happens when the rotation unpauses it.
                    trackers: Some(trackers()),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| anyhow!("add_torrent (seed): {e:#}"))?;
        Ok(())
    }

    /// One rotation step: bring the window starting at `cursor` live and pause
    /// every live torrent outside it that has no connected peers.
    ///
    /// Ordering is by librqbit's TorrentId, which is assigned in add order and
    /// never reused — so the window walks the library in a stable sequence and
    /// every torrent gets its turn. Torrents still hash-checking (Initializing)
    /// are left alone; pause/unpause on them errors upstream.
    ///
    /// Returns (live_after, kept_busy, total).
    pub async fn rotate_tick(&self, window: usize, cursor: usize) -> (usize, usize, usize) {
        use librqbit::TorrentStatsState as S;
        let mut handles = self
            .session
            .with_torrents(|it| it.map(|(id, h)| (id, h.clone())).collect::<Vec<_>>());
        handles.sort_by_key(|(id, _)| *id);
        let total = handles.len();
        if total == 0 {
            return (0, 0, 0);
        }
        let window = window.min(total);
        let start = cursor % total;
        let end = start + window; // may wrap past total
        let in_window = |i: usize| {
            if end <= total {
                i >= start && i < end
            } else {
                i >= start || i < end - total
            }
        };

        let (mut live_after, mut kept_busy) = (0usize, 0usize);
        for (i, (_, h)) in handles.iter().enumerate() {
            let st = h.stats();
            match st.state {
                S::Initializing | S::Error => {}
                S::Live => {
                    if in_window(i) {
                        live_after += 1;
                    } else {
                        let connected = st
                            .live
                            .as_ref()
                            .map(|l| l.snapshot.peer_stats.live > 0)
                            .unwrap_or(false);
                        if connected {
                            // Someone is mid-download from us — let it finish.
                            // The next tick re-checks; it only stays live while
                            // peers stay connected.
                            kept_busy += 1;
                            live_after += 1;
                        } else if self.session.pause(h).await.is_ok() {
                            // paused — dropped its live peer/announce state
                        } else {
                            live_after += 1; // pause failed; still live
                        }
                    }
                }
                S::Paused => {
                    if in_window(i) && self.session.unpause(h).await.is_ok() {
                        live_after += 1;
                    }
                }
            }
        }
        (live_after, kept_busy, total)
    }

    /// Pause EVERY live torrent — quiet hours. Connected peers are dropped on
    /// purpose: the whole point is handing the building's bandwidth back to
    /// the livestream. Returns how many were paused.
    pub async fn pause_all(&self) -> usize {
        use librqbit::TorrentStatsState as S;
        let handles = self
            .session
            .with_torrents(|it| it.map(|(_, h)| h.clone()).collect::<Vec<_>>());
        let mut n = 0usize;
        for h in handles {
            if matches!(h.stats().state, S::Live) && self.session.pause(&h).await.is_ok() {
                n += 1;
            }
        }
        n
    }

    /// Number of torrents currently in the session.
    pub fn torrent_count(&self) -> usize {
        self.session.with_torrents(|iter| iter.count())
    }

    /// (upload_bytes_per_sec, peers, torrent_count) — best-effort from the
    /// session snapshot; torrent count is exact.
    pub fn stats(&self) -> (u64, u64, u64) {
        let torrents = self.session.with_torrents(|iter| iter.count()) as u64;
        let snap = serde_json::to_value(self.session.stats_snapshot())
            .unwrap_or(serde_json::Value::Null);
        let up = dig_number(&snap, &["upload", "bps"])
            .or_else(|| dig_number(&snap, &["upload_speed", "bytes_per_second"]))
            .or_else(|| dig_number(&snap, &["upload_speed", "mbps"]).map(|m| m * 125_000.0))
            .unwrap_or(0.0);
        let peers = dig_number(&snap, &["peers"]).unwrap_or(0.0);
        (up as u64, peers as u64, torrents)
    }
}

/// Find the first number under any key whose name contains all `needles`.
fn dig_number(v: &serde_json::Value, needles: &[&str]) -> Option<f64> {
    fn walk(v: &serde_json::Value, needles: &[&str], path_hit: bool) -> Option<f64> {
        match v {
            serde_json::Value::Object(m) => {
                for (k, val) in m {
                    let kl = k.to_lowercase();
                    let hit = path_hit || needles.iter().all(|n| kl.contains(n));
                    if hit {
                        if let Some(n) = val.as_f64() {
                            return Some(n);
                        }
                    }
                    if let Some(n) = walk(val, needles, hit) {
                        return Some(n);
                    }
                }
                None
            }
            _ => None,
        }
    }
    walk(v, needles, false)
}

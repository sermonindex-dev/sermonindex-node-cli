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
#[allow(dead_code)]
const PEER_LIMIT_PER_TORRENT: usize = 8; // the measured-safe floor; see config::peer_limit_per_torrent
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
    peer_limit: usize,
    announce_port: Option<u16>,
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
            // The port we TELL the world to dial, which is not always the port
            // we bound. librqbit writes this into every tracker announce
            // (`TrackerComms::start(.., announce_port, ..)`) and every DHT
            // `announce_peer` — so this one field is the entire answer to "how
            // does a node behind a VPN tell other nodes where to find it?".
            // It does not need a gossip layer of our own; BitTorrent has had
            // this since the beginning. None falls back to the listen port.
            announce_port,
            ..Default::default()
        }),
        // ── Low-footprint tuning (matters a LOT at 25k+ torrents on a Pi) ──
        // Cap peers per torrent so connection buffers/state can't balloon; a seed
        // only needs a handful of leechers per swarm at a time.
        peer_limit: Some(peer_limit),
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
    /// The TCP port the session ACTUALLY bound.
    ///
    /// `start()` walks LISTEN_PORT_START..=LISTEN_PORT_END looking for a free
    /// one, so this is frequently NOT LISTEN_PORT_START. Before 0.2.2 the bound
    /// port was printed and thrown away, and the reachability probe, the
    /// heartbeat's `tcp_listen_port` and /stats all reported the constant — so
    /// a node whose session landed on 42801 had its CLOSED 42800 tested
    /// forever, reported `reachable: false` forever, and was drawn as a yellow
    /// peer on the map no matter how correctly its owner had forwarded the
    /// port that was really listening.
    port: u16,
    /// The session bound `[::]` rather than `0.0.0.0`. Recorded so the
    /// dashboard can say which family is actually being served: on a host where
    /// the v6 socket does not accept v4-mapped connections this node is
    /// IPv6-only inbound, which no amount of port forwarding will change.
    ipv6: bool,
    /// The public port stamped into every announce, when it differs from `port`.
    announce_port: Option<u16>,
}

/// Inbound vs outbound peer connections, summed across every live torrent.
///
/// The distinction this exists to draw: `incoming` peers DIALLED US, which is
/// the only proof that we are reachable and actually serving; `outgoing` peers
/// we dialled, which proves nothing except that our egress works. A node whose
/// `incoming` stays 0 for a week is taking from the swarm and giving nothing
/// back to anyone who cannot already reach it.
#[derive(Debug, Clone, Copy, Default)]
pub struct Directions {
    /// Peers that opened a connection TO this node (all address families).
    pub incoming: u64,
    /// Peers this node dialled out to.
    pub outgoing: u64,
    /// Live torrents that had a peer table to inspect.
    pub torrents_checked: u64,
    /// PROOF OF IPv6 REACHABILITY: a peer at a global-unicast IPv6 address
    /// opened a connection TO US.
    ///
    /// This is the single most important flag for a home node. Carrier-grade
    /// NAT — Starlink, T-Mobile Home Internet, most mobile broadband — makes
    /// inbound IPv4 impossible for ever, and those same carriers hand out real
    /// routable IPv6. So an ordinary household running a node is very often
    /// perfectly reachable, over IPv6, and our active probe can NEVER show it:
    /// the probe runs on a Bunny edge script with no outbound IPv6 at all.
    ///
    /// A real peer dialling in is better evidence than any probe anyway. Nobody
    /// arranged it on our behalf; a stranger on the internet reached this
    /// machine unaided.
    pub inbound_ipv6: bool,
    /// How many distinct global-IPv6 peers dialled in (context, not a verdict).
    pub inbound_ipv6_peers: u64,
}

impl Seeder {
    /// Start a session, walking the listen-port range like the app does.
    ///
    /// `peer_limit` is the per-torrent CONNECTION cap, now passed in rather
    /// than baked in as a constant. The 8 that used to live here was measured
    /// on a 4 GB Pi and then applied to every machine including a 16 GB Mac
    /// mini — see config::peer_limit_per_torrent() for what replaced it.
    ///
    /// `announce_port` is the port peers are told to dial. Leave it None when
    /// the node is reachable at the port it bound; set it when a VPN or an
    /// upstream NAT has given the node a different public port. It CANNOT be
    /// changed after the session starts — it is stamped into every announce —
    /// so it must be known before we get here.
    ///
    /// `preferred_port` is the LOCAL socket to try first (config::listen_port).
    /// The default range stays as the fallback rather than this being a hard
    /// requirement: refusing to seed at all because something transient held
    /// the port for a moment is worse than seeding on a different one. The
    /// caller compares the two and says so when they differ, so an operator
    /// whose hand-written router rule has stopped matching finds out from the
    /// log instead of from a reachability test weeks later.
    pub async fn start(
        download_dir: &Path,
        upload_bps: Option<u32>,
        dht: bool,
        peer_limit: usize,
        announce_port: Option<u16>,
        preferred_port: Option<u16>,
    ) -> Result<Seeder> {
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
        let candidates = preferred_port.into_iter().chain(
            (LISTEN_PORT_START..=LISTEN_PORT_END).filter(|p| Some(*p) != preferred_port),
        );
        for port in candidates {
            for (ipv6, dht) in combos {
                match Session::new_with_opts(
                    download_dir.to_path_buf(),
                    session_options(port, ipv6, dht, cap, peer_limit, announce_port),
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
                            port,
                            ipv6,
                            announce_port,
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
            "could not start torrent session on {}{LISTEN_PORT_START}..{LISTEN_PORT_END} (last: {last})",
            preferred_port
                .map(|p| format!("{p}, then "))
                .unwrap_or_default()
        ))
    }

    /// Try to FETCH a file from the swarm — the other half of the network.
    ///
    /// ## Why this did not exist until 0.3.0
    ///
    /// Every node acquired its library over HTTP, from Archive.org and the CDN,
    /// and used BitTorrent only to serve. So every node was a seeder and no node
    /// was ever a leecher: there was nothing in the swarm asking for anything,
    /// and fleet-wide upload sat at exactly zero across 52 nodes. The swarm was
    /// real — ports open, torrents announced, peers reachable — and completely
    /// unused, because we had built only the supply side of it.
    ///
    /// An operator running three nodes found it by asking the obvious question:
    /// his nodes had served 0 bytes and he wanted to know whether node-to-node
    /// transfer was part of the design yet. It was not.
    ///
    /// ## What this does
    ///
    /// Adds the magnet live, waits for the swarm to produce the file, and gives
    /// up quickly if it cannot. "Quickly" is the whole design: a file no peer
    /// holds must cost a few seconds, not a stalled slot, because the HTTP path
    /// behind it always works. Two limits, both deliberate:
    ///
    ///   * `first_byte` — if no peer has sent anything by then, nobody has this
    ///     file. Give up and let HTTP have it.
    ///   * `stall` — progress started and then stopped. Same verdict as the HTTP
    ///     watchdog in download.rs, for the same reason.
    ///
    /// On success the file is already verified by BitTorrent's own piece hashes
    /// and already live in the session, so it begins seeding immediately — the
    /// node that received it becomes a source for the next one without a second
    /// pass. The caller still checks the size against the signed master list,
    /// because the swarm is not the authority on what a file should be.
    pub async fn fetch_file(
        &self,
        magnet: &str,
        dest_dir: &Path,
        expected_size: u64,
        first_byte: std::time::Duration,
        stall: std::time::Duration,
    ) -> Result<bool> {
        if magnet.trim().is_empty() {
            return Ok(false);
        }
        std::fs::create_dir_all(dest_dir).ok();

        let handle = self
            .session
            .add_torrent(
                AddTorrent::from_url(magnet),
                Some(AddTorrentOptions {
                    output_folder: Some(dest_dir.to_string_lossy().to_string()),
                    overwrite: true,
                    paused: false,
                    trackers: Some(trackers()),
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| anyhow!("add_torrent (fetch): {e:#}"))?;

        let handle = match handle.into_handle() {
            Some(h) => h,
            None => return Ok(false),
        };

        let started = std::time::Instant::now();
        let mut last_progress = std::time::Instant::now();
        let mut best: u64 = 0;

        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;

            // One stats() call, not two: `finished` and `progress_bytes` must
            // describe the same instant or a file that completes between the two
            // reads looks stalled.
            let st = handle.stats();
            let done = st.finished;
            let got = st.progress_bytes;

            if got > best {
                best = got;
                last_progress = std::time::Instant::now();
            }

            if done {
                // BitTorrent verified every piece to get here. The size check is
                // the caller's, against the SIGNED list — the swarm is not the
                // authority on what a file should be.
                return Ok(true);
            }
            // Nobody has it. This is the common case early in the network's life
            // and it must be cheap.
            if best == 0 && started.elapsed() >= first_byte {
                let _ = self.session.delete(handle.id().into(), false).await;
                return Ok(false);
            }
            // It started and stopped. Same verdict as the HTTP watchdog.
            if best > 0 && last_progress.elapsed() >= stall {
                let _ = self.session.delete(handle.id().into(), false).await;
                return Ok(false);
            }
            // A swarm that is slower than our own CDN is not worth the slot.
            // `expected_size` bounds the wait: a 4 MB sermon that has taken five
            // minutes is not going to arrive.
            let cap = std::time::Duration::from_secs(
                60 + (expected_size / (32 * 1024)).min(900),
            );
            if started.elapsed() >= cap {
                let _ = self.session.delete(handle.id().into(), false).await;
                return Ok(false);
            }
        }
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
    /// The torrents this node is LIVE on right now, as `(name, info_hash)`.
    ///
    /// Live, not held — and the distinction is the whole point. Under rotation
    /// the node keeps its entire library registered but PAUSED and brings a
    /// window live at a time; a paused torrent announces to nobody and serves
    /// nobody. "What is this node sharing" therefore means the live window,
    /// which is also, conveniently, bounded: `config window` caps it, so this
    /// never returns the 40,000 entries that made reporting impossible before.
    ///
    /// Capped again at `max` regardless, because a cap that depends on another
    /// setting being sane is not a cap.
    pub fn live_torrents(&self, max: usize) -> Vec<(String, String)> {
        self.session.with_torrents(|iter| {
            let mut out = Vec::new();
            for (_, t) in iter {
                if out.len() >= max {
                    break;
                }
                // Only torrents actually live and complete are being served.
                let st = t.stats();
                if !st.finished {
                    continue;
                }
                if !matches!(st.state, librqbit::TorrentStatsState::Live) {
                    continue;
                }
                if let Some(name) = t.name() {
                    out.push((name, t.info_hash().as_string()));
                }
            }
            out
        })
    }

    pub fn torrent_count(&self) -> usize {
        self.session.with_torrents(|iter| iter.count())
    }

    /// The TCP port this session is really listening on, locally.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The port peers are told to dial — the announce port when one was given,
    /// else the local one. This is the number that belongs in the heartbeat, in
    /// the reachability probe, and in anything shown to a person: it is the one
    /// the outside world actually uses.
    pub fn public_port(&self) -> u16 {
        self.announce_port.unwrap_or(self.port)
    }

    /// True when the listening socket was bound on `[::]` rather than `0.0.0.0`.
    pub fn is_ipv6_socket(&self) -> bool {
        self.ipv6
    }

    /// Count peers by DIRECTION across every live torrent.
    ///
    /// librqbit 9 does expose direction, but not on `PeerStats` itself:
    /// `counters.incoming_connections` is incremented only in
    /// `TorrentStateLive::add_incoming_peer`, reached only from
    /// `Session::task_listener`. So `incoming_connections > 0` means that peer
    /// dialled our listening socket; `connections > 0` without it means we
    /// dialled them. (`conn_kind` is NOT direction — it is Tcp / Utp / Socks.)
    ///
    /// `state: "all"` rather than the default "live" on purpose: the counters
    /// survive a peer going quiet, and a peer that took a whole sermon from us
    /// an hour ago and left is exactly the event we are trying not to lose.
    ///
    /// Cost: one snapshot per LIVE torrent. Under rotation only the live window
    /// (a few hundred at most) is ever live, so this is bounded regardless of
    /// how many thousand torrents the session holds. Call it on the slow poll,
    /// not the fast one.
    pub fn directions(&self) -> Directions {
        self.session.with_torrents(|iter| {
            let mut d = Directions::default();
            for (_id, t) in iter {
                // Only a live torrent has a peer table. Paused / initializing
                // ones are skipped: absence of data is not evidence of zero.
                let Some(live) = t.live() else { continue };
                d.torrents_checked += 1;
                let snapshot = live.per_peer_stats_snapshot(
                    serde_json::from_value(serde_json::json!({ "state": "all" }))
                        .unwrap_or_default(),
                );
                for (addr_str, peer) in snapshot.peers.iter() {
                    let inbound = peer.counters.incoming_connections > 0;
                    if inbound {
                        d.incoming += 1;
                    } else if peer.counters.connections > 0 {
                        d.outgoing += 1;
                    }
                    // Only an INBOUND connection counts as reachability proof.
                    // An outbound one to an IPv6 peer shows our egress works
                    // and nothing whatsoever about whether anyone can reach us.
                    if inbound {
                        if let Ok(addr) = addr_str.parse::<std::net::SocketAddr>() {
                            if crate::net::is_global_unicast_ipv6_peer(&addr) {
                                d.inbound_ipv6 = true;
                                d.inbound_ipv6_peers += 1;
                            }
                        }
                    }
                }
            }
            d
        })
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

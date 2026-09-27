//! SermonIndex headless seed node.
//!
//! Downloads the archive (audio by default) from the signed master list, holds
//! it, seeds it over BitTorrent (feature "seed"), heartbeats onto the live node
//! map, and serves the 4-screen dashboard on :8137 — all with no GUI. Runs as a
//! service on Linux / macOS / Windows and on a Raspberry Pi.

mod config;
mod dashboard;
mod download;
mod heartbeat;
mod masterlist;
mod net;
mod state;
mod system;
mod cfg;
mod natpmp;
mod peercheck;
mod update;
#[cfg(feature = "seed")]
mod seed;

use anyhow::{bail, Result};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use state::Shared;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("start");
    match cmd {
        "version" | "-V" | "--version" => {
            println!("sermonindex-node {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "help" | "-h" | "--help" => {
            print_help();
            Ok(())
        }
        "status" => run_status(),
        "config" | "set" | "settings" => cfg::run(&args),
        "peers" => run_peers(),
        "history" => run_history(),
        "port" => run_port(),
        "refresh" | "sweep" => run_refresh(),
        "seed" => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_seed(&args))
        }
        "update" => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            rt.block_on(run_update())
        }
        "quiet" => run_quiet(&args),
        "verify" => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_verify(&args))
        }
        "start" | "run" => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(run_daemon(&args))
        }
        other => {
            eprintln!("unknown command: {other}\n");
            print_help();
            std::process::exit(2);
        }
    }
}

fn print_help() {
    println!(
        r#"sermonindex-node — headless SermonIndex seed node

Downloads, verifies, holds, and SEEDS the SermonIndex archive over BitTorrent
with no GUI, and serves a live dashboard. Runs as a 24/7 service on a Raspberry
Pi, an old laptop, a NAS, or a server. Shares the ~/.sermonindex data directory
with the desktop app.

USAGE
  sermonindex-node [COMMAND] [OPTIONS]

COMMANDS
  start        Download + seed + heartbeat + serve the dashboard (default).
  status       Print node id, scope, paths, and cache state, then exit.
  quiet        View or edit quiet hours — weekly times the node goes silent
               (no seeding, serving, or downloading) so a church's livestream
               keeps the bandwidth. Local time; the running node applies
               changes within a minute, no restart needed.
                 quiet                          show the schedule
                 quiet add sun 08:00-13:00      Sunday mornings off
                 quiet add wed 18:00-21:30      Wednesday evenings off
                 quiet add sun,wed 08:00-13:00  several days at once
                 quiet remove 2                 delete window 2 from the list
                 quiet clear                    remove all quiet hours
  seed         Full-archive (video) access is granted per machine by a person.
               Audio scope needs no approval and is always available.
                 seed request --email you@example.com
                 seed status
  config       View or change any setting — the same controls the desktop app
               has, from the terminal. Writes settings.json atomically, so the
               app and the node can both edit it safely.
                 config                         show everything
                 config scope full              audio (~412 GB) | full (~2.4 TB)
                 config dir /mnt/library        where the library lives
                 config upload 2000             KB/s, or "off" for unlimited
                 config monthly-cap 500GB       total upload per month, or "off"
                 config schedule 22:00-06:00    only seed in this window ("off")
                 config public-port 51413       the port the WORLD dials, when a
                                                VPN or upstream NAT gives you a
                                                different one ("auto" to clear)
                 config public-ip 1.2.3.4       only for split tunnels (see docs)
                 config natpmp off              stop asking the gateway for a port
                 config p2p off                 download and hold; never serve
                 config source auto             auto | cdn | archive — which HTTP
                                                source to try first. "auto" (the
                                                default) sends audio to
                                                archive.org and video to our CDN
                 config downloads 8             how many files to fetch at once
                                                (1-16, default 4)
                 config dht on|off              join the DHT
                 config peers 16                connections per torrent
                 config window 4000             torrents live at once
                 config rotate 15               minutes per rotation window
                 config reset                   tuning back to this machine's
                                                defaults (keeps id/dir/quiet)
  peers        Who is connected right now, and which way round — the terminal
               view of the app's "Giving vs Taking".
  history      The last 7 days: peers in vs out, uploaded, files held.
  port         What port we bound, whether the internet can reach it, and what
               NAT-PMP/UPnP managed to do about it.
  refresh      Sweep for new and missing files RIGHT NOW instead of waiting for
               the hourly pass, and retry anything that failed earlier. Safe to
               run any time — the missing set is recomputed from what is on
               disk, so nothing is fetched twice. (also: sweep)
  update       Check whether a newer node has been released. Never installs
               anything on its own — a seed node mid-upload does not restart
               itself because a version number changed.
  verify       Audit the library against the signed master list (presence +
               exact size for every file) and report coverage. Read-only.
  version      Print the version.
  help         Show this help (also: -h, --help).

START OPTIONS
  --scope <audio|full>   What to hold and seed. "full" requires approval —
                         run `seed request` first; an unapproved node starts in
                         audio scope rather than refusing to run.
                           audio   ~400 GB  — every audio sermon (default)
                           full    ~2.4 TB  — audio + video, a complete backup
                         Falls back to settings.json "seed_scope", else audio.
  --dir <path>           Library storage directory (overrides settings.json
                         "storage_dir"). Point at a big drive, e.g. --dir /mnt/library.
  --no-download          Don't fetch anything new; only seed what's on disk and
                         serve the dashboard (a pure seeder / mirror).
  --no-dashboard         Don't start the local dashboard web server.
  --public-port <n>      The port peers should be told to dial, when it differs
                         from the one bound locally — a VPN with port forwarding
                         being the usual reason. Overrides settings.json for this
                         run only, so a wrapper that pulls a fresh port from a
                         VPN's CLI can pass it straight in:
                           sermonindex-node start --public-port "$(get-vpn-port)"
                         SI_PUBLIC_PORT does the same via the environment.
                         Normally unnecessary: NAT-PMP finds it automatically,
                         including inside a Proton VPN tunnel.
  --port N               Pin the LOCAL listening port (saved; 1-65535).
                         Pin one before writing a router rule by hand: a rule
                         names ONE port, and the default picks the first free
                         port in 42800-42839, so an unpinned node matches such a
                         rule only by luck. Below 1024 needs root on Linux/macOS.
                         If N is taken at startup the node falls back to the
                         default range and prints a WARNING saying so.
                         SI_LISTEN_PORT does the same via the environment.
  --no-port              Clear a pinned port; go back to the automatic range.
  --no-heartbeat         Don't announce to the network map (local / testing use).

CONFIGURATION   (~/.sermonindex/settings.json — shared with the desktop app)
  storage_dir            Absolute path to the library folder.
  seed_scope             "audio" or "full".
  upload_limit_enabled   true/false — cap the BitTorrent upload rate.
  upload_limit_kbps      Upload cap in KB/s when enabled (0 = unlimited).
  listen_port            Pinned local listening port (see --port). Unset = pick
                         the first free port in 42800-42839.
  node_id                Auto-generated on first run ("si-" + 16 hex). Your stable
                         identity on the node map — keep it.
  quiet_hours            Weekly silence windows (see the quiet command), e.g.
                         [{{"days":["sun"],"start":"08:00","end":"13:00"}}].
  active_torrents        How many torrents seed at once (default 2000; 0 = all).
                         The rest stay registered but paused, rotating in turn —
                         this is what keeps a 25k-sermon node inside 4 GB of RAM.
  rotate_minutes         Minutes per rotation window (default 15).

PATHS
  data dir     ~/.sermonindex
  library      <storage_dir> or ~/.sermonindex/downloads   (files: <shard>/<id>.mp3)
  master list  ~/.sermonindex/master-list.json   (verified, cached copy)
  torrents     ~/.sermonindex/torrents
  dashboard    ~/.sermonindex/dashboard.html     (optional — overrides the built-in UI)

DASHBOARD
  Served at http://localhost:8137/  (and http://<this-host-ip>:8137/ on your LAN).
  Four screens: Map, Network, Stats, System. Open in any browser, or run a
  full-screen kiosk on a small display. To customize it, drop your own HTML at
  ~/.sermonindex/dashboard.html and restart — no rebuild needed.

NETWORK / REACHABILITY
  BitTorrent listens on TCP 42800-42839 (first free port), dual-stack (IPv4 +
  IPv6), trying UPnP and NAT-PMP automatically. Every heartbeat the node asks the
  SermonIndex probe server to TCP-connect back over both IPv4 and IPv6 and reports
  the result, so the live map can classify it:
    seed  (blue)   — the node admin has granted seed status AND it is reachable
    node  (green)  — reachable from the internet (port open on IPv4 or IPv6)
    peer  (yellow) — running but not reachable inbound
  With native IPv6 (common on Telus, Starlink, most fibre) a node is reachable
  with NO port-forward at all — the global IPv6 address is dialed directly. The
  detected address is printed at startup. If you have only IPv4 behind NAT,
  forward TCP 42800 to this host (or rely on UPnP/NAT-PMP) to show as a green node.

  WRITING THE RULE YOURSELF. When UPnP and NAT-PMP are both unavailable, pin the
  port first (--port 42800) so the rule keeps matching, then:
    IPv4   forward TCP <port> to this host.
    IPv6   there is no NAT and nothing to forward — the router simply blocks
           unsolicited inbound. Add a PINHOLE allowing inbound TCP to
           [<your global IPv6>]:<port>. Routers call this "IPv6 Firewall",
           "Pinhole", "Allow Inbound IPv6" or "IPv6 Simple Security".
           `sermonindex-node status` prints the address and port to use.
  Seeding holds one open file per torrent, so a full library needs a high open-
  file limit — the installed service sets LimitNOFILE; elsewhere raise ulimit -n.

SERVICE   (Linux, after build-and-install.sh)
  sudo systemctl status  sermonindex-node
  sudo systemctl restart sermonindex-node
  journalctl -u sermonindex-node -f            # live progress
  sermonindex-node status                      # quick info

EXAMPLES
  sermonindex-node start
  sermonindex-node start --dir /mnt/library
  sermonindex-node seed request --email you@example.com   # then --scope full
  sermonindex-node start --no-download          # seed-only mirror
  sermonindex-node start --port 42800           # pin the port for a router rule
  sermonindex-node refresh                      # fetch new files + retry failures now
  sermonindex-node status

Every node running this appears on the live map and helps keep the archive
permanent. More: https://sermonindex.net
"#
    );
}

/// `sermonindex-node quiet …` — view or edit the weekly quiet-hours schedule.
/// The running daemon re-reads settings every minute, so changes apply without
/// a restart.
fn run_quiet(args: &[String]) -> Result<()> {
    let mut settings = config::load_settings();
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("list");
    let print_list = |settings: &serde_json::Value| {
        let windows = config::quiet_windows(settings);
        if windows.is_empty() {
            println!("No quiet hours scheduled — the node runs around the clock.");
        } else {
            println!("Quiet hours (local time — node pauses all seeding/serving):");
            for (i, w) in windows.iter().enumerate() {
                println!("  {}. {}", i + 1, config::window_label(w));
            }
        }
        match config::quiet_now(settings) {
            Some(why) => println!("\nRight now: QUIET — {why}"),
            None => println!("\nRight now: active"),
        }
    };
    match sub {
        "list" => {
            print_list(&settings);
            Ok(())
        }
        "add" => {
            let (days_s, range_s) = match (args.get(3), args.get(4)) {
                (Some(d), Some(r)) => (d.as_str(), r.as_str()),
                _ => {
                    eprintln!("usage: sermonindex-node quiet add <days> <start-end>");
                    eprintln!("  e.g. sermonindex-node quiet add sun 08:00-13:00");
                    eprintln!("       sermonindex-node quiet add wed 18:00-21:30");
                    eprintln!("       sermonindex-node quiet add sun,wed 08:00-13:00");
                    eprintln!("  days: sun,mon,tue,wed,thu,fri,sat | all | weekdays | weekend");
                    std::process::exit(2);
                }
            };
            let days = config::parse_days(days_s).unwrap_or_else(|| {
                eprintln!("Unrecognized days: {days_s}  (use e.g. sun,wed or all)");
                std::process::exit(2);
            });
            let (start, end) = match range_s.split_once('-') {
                Some((a, b)) => match (config::parse_hhmm(a), config::parse_hhmm(b)) {
                    (Some(a), Some(b)) if a != b => (a, b),
                    _ => {
                        eprintln!("Bad time range: {range_s}  (use HH:MM-HH:MM, 24-hour)");
                        std::process::exit(2);
                    }
                },
                None => {
                    eprintln!("Bad time range: {range_s}  (use HH:MM-HH:MM, 24-hour)");
                    std::process::exit(2);
                }
            };
            let entry = serde_json::json!({
                "days": config::DAY_NAMES
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| days & (1 << i) != 0)
                    .map(|(_, d)| *d)
                    .collect::<Vec<_>>(),
                "start": format!("{:02}:{:02}", start / 60, start % 60),
                "end": format!("{:02}:{:02}", end / 60, end % 60),
            });
            match settings.get_mut("quiet_hours") {
                Some(serde_json::Value::Array(a)) => a.push(entry),
                _ => settings["quiet_hours"] = serde_json::json!([entry]),
            }
            config::save_settings(&settings)?;
            println!("Added. The running node picks this up within a minute.\n");
            print_list(&settings);
            Ok(())
        }
        "remove" | "rm" => {
            let idx: usize = args
                .get(3)
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(|| {
                    eprintln!("usage: sermonindex-node quiet remove <number>  (see: quiet list)");
                    std::process::exit(2);
                });
            let removed = match settings.get_mut("quiet_hours") {
                Some(serde_json::Value::Array(a)) if idx >= 1 && idx <= a.len() => {
                    Some(a.remove(idx - 1))
                }
                _ => None,
            };
            match removed {
                Some(_) => {
                    config::save_settings(&settings)?;
                    println!("Removed window {idx}.\n");
                    print_list(&settings);
                    Ok(())
                }
                None => {
                    eprintln!("No quiet window number {idx} — run: sermonindex-node quiet list");
                    std::process::exit(2);
                }
            }
        }
        "clear" => {
            settings["quiet_hours"] = serde_json::json!([]);
            config::save_settings(&settings)?;
            println!("All quiet hours removed — the node runs around the clock.");
            Ok(())
        }
        other => {
            eprintln!("unknown quiet subcommand: {other}  (use: list, add, remove, clear)");
            std::process::exit(2);
        }
    }
}

fn arg_value(args: &[String], key: &str) -> Option<String> {
    args.iter()
        .position(|a| a == key)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn run_status() -> Result<()> {
    let mut settings = config::load_settings();
    let id = config::node_id(&mut settings);
    let scope = config::seed_scope(&settings);
    println!("SermonIndex node");
    println!("  node_id:    {id}");
    println!("  scope:      {scope}");
    println!("  content:    {}", config::source_mode_line(&settings, &scope));
    println!("  data dir:   {}", config::data_dir().display());
    println!("  storage:    {}", config::downloads_dir(&settings).display());
    println!(
        "  master list cache: {}",
        if masterlist::has_cache() {
            "present"
        } else {
            "none (will fetch on start)"
        }
    );
    println!("  dashboard:  http://localhost:{}/", config::DASHBOARD_PORT);
    println!("  quiet:      {}", config::describe_quiet(&settings));
    if let Some(why) = config::quiet_now(&settings) {
        println!("              QUIET RIGHT NOW — {why}");
    }
    println!("  version:    {}", update::current());

    // Everything above is read off disk and is true whether or not a node is
    // running. Everything below can only come from a live process.
    match live_stats() {
        Some(v) => {
            let n = &v["node"];
            let g = |k: &str| n.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
            println!("\n  RUNNING");
            println!("    port:       {}", g("port"));
            println!("    reachable:  {}", n.get("reachable").and_then(|x| x.as_str()).unwrap_or("unknown"));
            println!("    NAT-PMP:    {}", n.get("natpmp").and_then(|x| x.as_str()).unwrap_or("off"));
            println!("    held:       {} of {} ({:.1}%)", g("held"), g("catalog"),
                     n.get("coverage_pct").and_then(|x| x.as_f64()).unwrap_or(0.0));
            println!("    peers:      {} now · {} came to you this run", g("peers"), g("peers_in_peak"));
            println!("    master list v{}", g("masterlist_version"));
            if let Some(u) = n.get("update_available").and_then(|x| x.as_str()) {
                println!("\n    UPDATE AVAILABLE: {u} — run `sermonindex-node update`");
            }
        }
        None => println!("\n  Not running. Start with `sermonindex-node start`."),
    }
    Ok(())
}


// ── live-node queries ────────────────────────────────────────────────────────
//
// These read /stats off the RUNNING node rather than recomputing anything. That
// is the honest thing to do: the running process is the only thing that knows
// how many peers are connected, and a second process guessing would be a second
// answer to the same question. When nothing is running they say so plainly
// instead of printing zeroes, because "no node" and "a node serving nobody"
// are very different situations and must never look the same.

fn live_stats() -> Option<Value> {
    let url = format!("http://127.0.0.1:{}/stats", config::DASHBOARD_PORT);
    let out = std::process::Command::new("curl")
        .args(["-s", "-m", "3", &url])
        .output()
        .ok()?;
    serde_json::from_slice::<Value>(&out.stdout).ok()
}

fn need_running() -> Option<Value> {
    match live_stats() {
        Some(v) if v.get("node").is_some() => Some(v),
        _ => {
            println!("No running node found on port {}.", config::DASHBOARD_PORT);
            println!("Start one with `sermonindex-node start`, then try again.");
            None
        }
    }
}

fn run_peers() -> Result<()> {
    let Some(v) = need_running() else { return Ok(()) };
    let n = &v["node"];
    let g = |k: &str| n.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let (inb, out, peak, now) = (g("peers_in"), g("peers_out"), g("peers_in_peak"), g("peers"));

    println!("\nPeer connections");
    println!("  connected right now      {now}");
    println!("  came to you (now)        {inb}");
    println!("  you reached out (now)    {out}");
    println!("  came to you (this run)   {peak}   <- the one that means you are serving");
    println!();
    if peak == 0 {
        println!("  Nobody has connected TO you yet this run. You are still uploading to");
        println!("  every peer you reach, but you are not a meeting point others can find.");
        println!("  Run `sermonindex-node port` to see whether that is fixable here.");
    } else if inb >= out {
        println!("  More peers came to you than you reached out to — your node is a place");
        println!("  others can find. This is the healthy picture.");
    } else {
        println!("  You reach out more than peers come to you. Normal when the port is");
        println!("  closed, and still useful — you serve everyone you connect to.");
    }
    println!();
    Ok(())
}

fn run_history() -> Result<()> {
    let rows = state::load_history();
    let rows = rows.as_array().cloned().unwrap_or_default();
    if rows.is_empty() {
        println!("No history recorded yet — a row is written every hour once the node runs.");
        println!("File: {}", config::data_dir().join("history.jsonl").display());
        return Ok(());
    }
    // Fold hourly rows into calendar days, keeping each day's PEAK. The live
    // figure drops to 0 every time the rotation window moves off the torrents
    // those peers were on, so a day when twenty people took sermons from this
    // node could otherwise be written down as a zero.
    use std::collections::BTreeMap;
    let mut days: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new();
    for r in &rows {
        let ts = r.get("ts").and_then(|v| v.as_u64()).unwrap_or(0);
        if ts == 0 { continue; }
        let day = ts / 86_400;
        let key = format!("{day}");
        let g = |k: &str| r.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        let e = days.entry(key).or_insert((0, 0, 0, 0));
        e.0 = e.0.max(g("peers_in_peak").max(g("peers_in")));
        e.1 = e.1.max(g("peers_out"));
        e.2 = e.2.max(g("up"));
        e.3 = e.3.max(g("held"));
    }
    println!("\nLast {} day(s)\n", days.len());
    println!("  {:<12} {:>9} {:>9} {:>12} {:>9}", "day", "came to", "reached", "uploaded", "held");
    println!("  {}", "-".repeat(55));
    let mut prev_up = 0u64;
    for (i, (k, v)) in days.iter().enumerate() {
        let secs = k.parse::<u64>().unwrap_or(0) * 86_400;
        let label = day_label(secs);
        // `up` is cumulative, so difference consecutive days for the real
        // per-day figure. The first row has nothing to subtract from.
        let delta = if i == 0 { 0 } else { v.2.saturating_sub(prev_up) };
        prev_up = v.2;
        println!(
            "  {:<12} {:>9} {:>9} {:>12} {:>9}",
            label, v.0, v.1,
            if i == 0 { "-".to_string() } else { human_bytes(delta) },
            v.3
        );
    }
    println!();
    Ok(())
}

/// "16 Sep" from a unix timestamp. chrono is already a dependency, so there is
/// no reason to hand-roll civil-from-days and every reason not to.
fn day_label(secs: u64) -> String {
    use chrono::{Local, TimeZone};
    match Local.timestamp_opt(secs as i64, 0).single() {
        Some(dt) => dt.format("%-d %b").to_string(),
        None => "?".to_string(),
    }
}

/// Path of the "sweep now" sentinel. A file rather than a socket or an HTTP
/// route on the dashboard: the daemon may be running as another user or under
/// systemd, the dashboard port is deliberately unauthenticated, and a zero-byte
/// file in the data dir needs no privileges, no protocol and no new surface.
fn refresh_flag() -> std::path::PathBuf {
    config::data_dir().join("refresh-now")
}

/// Sleep up to `secs`, returning early when someone asks for a sweep.
async fn wait_or_refresh(secs: u64) {
    let flag = refresh_flag();
    let mut left = secs;
    while left > 0 {
        let slice = left.min(60);
        tokio::time::sleep(Duration::from_secs(slice)).await;
        left -= slice;
        if flag.exists() {
            let _ = std::fs::remove_file(&flag);
            println!("[masterlist] refresh requested — sweeping now");
            return;
        }
    }
}

/// `sermonindex-node refresh` — sweep for new and missing files right now.
///
/// The daemon already does this hourly, and that sweep is also the retry path
/// for anything that failed earlier (the missing set is recomputed from what is
/// actually on disk, so a file that did not land is simply missing again). This
/// is for the times you do not want to wait an hour: a new batch has just been
/// published, or a download failed while the connection was down and is now
/// back.
fn run_refresh() -> Result<()> {
    let dir = config::data_dir();
    std::fs::create_dir_all(&dir).ok();
    let flag = refresh_flag();
    std::fs::write(&flag, b"")?;
    match need_running() {
        Some(_) => {
            println!("\nSweep requested — the running node will start within a minute.");
            println!("  It will fetch anything new in your scope and retry whatever failed before.");
            println!("  Watch it:  journalctl -u sermonindex-node -f");
        }
        None => {
            println!("\nRequest saved. The node is not running right now —");
            println!("  it will sweep as soon as it next starts.");
        }
    }
    println!();
    Ok(())
}

fn run_port() -> Result<()> {
    let Some(v) = need_running() else { return Ok(()) };
    let n = &v["node"];
    let port = n.get("port").and_then(|x| x.as_u64()).unwrap_or(0);
    let reach = n.get("reachable").and_then(|x| x.as_str()).unwrap_or("unknown");
    let nat = n.get("natpmp").and_then(|x| x.as_str()).unwrap_or("off");

    let settings = config::load_settings();
    let pinned = config::listen_port(&settings);

    println!("\nPort and reachability");
    println!("  listening on     TCP {port}");
    match pinned {
        // The pin did not take. Say it here too, not just at startup — `status`
        // is where someone looks when their router rule has stopped working,
        // and this is the reason.
        Some(p) if p as u64 != port => {
            println!("                   PINNED {p} IS NOT IN USE — a router rule naming");
            println!("                   {p} will not match. Free it and restart, or");
            println!("                   re-pin with --port {port}.");
        }
        Some(p) => println!("                   (pinned with --port {p})"),
        None if port != config::LISTEN_PORT_START as u64 => {
            println!("                   (NOT the default {} — forward THIS one, and", config::LISTEN_PORT_START);
            println!("                   consider --port {port} so it stays this number)");
        }
        None => {}
    }
    println!("  NAT-PMP / PCP    {nat}");
    println!("  inbound test     {reach}");
    println!();
    let v6 = n.get("v6_inbound_seen").and_then(|x| x.as_bool()).unwrap_or(false);
    if v6 {
        println!("  IPv6            a peer has connected TO you over IPv6");
        println!();
        println!("  You are reachable. Someone out on the internet dialled this machine");
        println!("  directly — that is stronger evidence than any test we could run, and");
        println!("  it is the normal good result on Starlink, T-Mobile Home Internet and");
        println!("  mobile broadband. Nothing to forward. Nothing to change.");
        println!();
        return Ok(());
    }
    // The exact rule to write. An IPv6 pinhole needs this machine's own global
    // address, which nobody can be expected to find for themselves — and
    // without it the instruction "add a pinhole" is not actionable advice.
    if reach != "reachable" && !v6 {
        if let Some(addr) = net::global_ipv6() {
            println!("  To open it by hand, in your router's settings:");
            println!("    IPv4   forward TCP {port} to this machine");
            println!("    IPv6   allow inbound TCP to [{addr}]:{port}");
            println!("           (no forwarding — IPv6 has no NAT. Look for \"IPv6");
            println!("            Firewall\", \"Pinhole\" or \"Allow Inbound IPv6\".)");
            if pinned.is_none() {
                println!("    First: sermonindex-node start --port {port}");
                println!("           so the rule keeps matching after a restart.");
            }
            println!();
        }
    }
    match reach {
        "reachable" => println!("  Peers can dial you directly. Nothing to do."),
        "closed" => {
            println!("  Nothing has connected in YET.");
            println!();
            println!("  Two things to know before you change anything:");
            println!();
            println!("  1. The test above only checks the older kind of address (IPv4). If");
            println!("     your provider shares one of those between many homes — Starlink,");
            println!("     T-Mobile Home Internet, most mobile broadband — it will always");
            println!("     read closed, and there is no setting that fixes it.");
            println!();
            println!("  2. Those same providers give every home a real modern address");
            println!("     (IPv6), and peers reach you on that instead. We cannot test it");
            println!("     from here, so we watch for it: the moment a peer dials in over");
            println!("     IPv6 this will say so, and your node turns green on the map.");
            println!();
            println!("  If you DO have a normal connection and can reach your router's");
            println!("  settings, forwarding TCP {port} to this machine makes you reachable");
            println!("  on both. If you cannot, leave it — a node that only dials out still");
            println!("  uploads sermons to every peer it reaches, every day.");
        }
        _ => println!("  Not tested yet — the check runs a few minutes after start."),
    }
    println!();
    Ok(())
}

async fn run_update() -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(format!("sermonindex-node/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    println!("Running {}. Checking for a newer release…", update::current());
    match update::check(&client).await {
        Some(v) => println!("\n{}\n", update::install_hint(&v)),
        None => println!("\nYou are up to date (or the release manifest could not be reached).\n"),
    }
    Ok(())
}

/// Human-readable byte size (GiB/MiB) for reports.
fn human_bytes(b: u64) -> String {
    const GB: f64 = 1_073_741_824.0;
    const MB: f64 = 1_048_576.0;
    let f = b as f64;
    if f >= GB {
        format!("{:.1} GB", f / GB)
    } else {
        format!("{:.0} MB", f / MB)
    }
}

/// `verify` — audit the on-disk library against the ed25519-signed master list:
/// every in-scope file must be present at its EXACT signed byte size. This is the
/// authoritative "do we have the complete archive?" check. Fast (stat only, no
/// re-hash) and read-only — it never deletes or downloads anything.
async fn run_verify(args: &[String]) -> Result<()> {
    let mut settings = config::load_settings();
    let _ = config::node_id(&mut settings);
    if let Some(dir) = arg_value(args, "--dir") {
        settings["storage_dir"] = serde_json::json!(dir);
    }
    let scope = arg_value(args, "--scope").unwrap_or_else(|| config::seed_scope(&settings));

    let client = reqwest::Client::builder()
        .user_agent(format!("sermonindex-node/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()?;
    let ml = masterlist::fetch_verified(&client).await?;
    println!(
        "[verify] master list v{} — {} entries (ed25519 signature verified)",
        ml.version,
        ml.entries.len()
    );

    let want_audio = scope != "full";
    let entries: Vec<&masterlist::Entry> = ml
        .entries
        .values()
        .filter(|e| if want_audio { e.is_audio() } else { e.is_audio() || e.is_video() })
        .collect();
    let total = entries.len() as u64;

    let (mut present, mut missing, mut wrong, mut have_bytes, mut want_bytes) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for e in &entries {
        want_bytes += e.size;
        let p = config::file_path(&settings, &e.name);
        match std::fs::metadata(&p) {
            Ok(m) if m.len() == e.size => {
                present += 1;
                have_bytes += e.size;
            }
            Ok(_) => wrong += 1, // present but wrong size → corrupt/partial
            Err(_) => missing += 1,
        }
    }

    let pct = if total > 0 { present as f64 / total as f64 * 100.0 } else { 0.0 };
    println!("scope:                  {scope}  ({total} files, {} total)", human_bytes(want_bytes));
    println!("present & correct size: {present}");
    println!("missing:                {missing}");
    println!("wrong size (corrupt):   {wrong}");
    println!("on disk:                {}", human_bytes(have_bytes));
    println!("coverage:               {pct:.2}%");
    if missing == 0 && wrong == 0 {
        println!("\n\u{2713} COMPLETE — every file is present at its exact signed size.");
    } else {
        println!(
            "\n\u{2717} INCOMPLETE — run `sermonindex-node start` to fetch the {} remaining.",
            missing + wrong
        );
    }
    Ok(())
}

/// `sermonindex-node seed …` — ask for, and check, seed-node access.
///
/// ## Why the CLI has a gate at all (0.2.8)
///
/// `--scope full` is the whole archive including video: ~2.4 TB, and a
/// meaningful share of our egress bill. The desktop app has always required a
/// per-machine approval before it would show anyone the full-archive controls.
/// The CLI did not — and the public `/node-software/` page printed the
/// full-scope command verbatim, so anyone who found the page could pull
/// everything. That was not a decision anybody made; it was the gate simply
/// never having been carried across.
///
/// The backend this talks to is the SAME one the app has used since 0.0.3xx:
/// `GET /api/seed/access` and `POST /api/seed/request`, landing in the pending
/// queue on the admin console. Nothing new was invented here, which is the
/// point — two request paths with two sets of semantics would be a bug waiting
/// to happen.
///
/// Audio scope is NOT gated and never will be. The audio archive is what we
/// most want mirrored in as many places as possible; gating it would be working
/// against ourselves.
async fn run_seed(args: &[String]) -> Result<()> {
    let mut settings = config::load_settings();
    let node_id = config::node_id(&mut settings);
    let client = reqwest::Client::builder()
        .user_agent(format!("sermonindex-node/{}", env!("CARGO_PKG_VERSION")))
        .build()?;
    let sub = args.get(2).map(|s| s.as_str()).unwrap_or("status");

    match sub {
        "request" => {
            let email = arg_value(args, "--email").unwrap_or_default();
            let email = email.trim().to_string();
            // Not validation for its own sake: without a way to reach the
            // operator the admin has nothing to approve but an opaque id, and
            // the request will simply sit there.
            if email.is_empty() || !email.contains('@') {
                bail!(
                    "seed request needs an email the admin can reply to:\n  \
                     sermonindex-node seed request --email you@example.com"
                );
            }
            println!("[seed] node {node_id}");
            println!("[seed] requesting full-archive access for this machine…");
            match heartbeat::request_seed_access(&client, &node_id, &email).await {
                Some((true, _)) => {
                    println!("[seed] already approved — this node has full-archive access.");
                    println!("[seed] start it with:  sermonindex-node start --scope full");
                }
                Some((_, _)) => {
                    println!("[seed] request sent. A person reviews each one.");
                    println!("[seed] check with:  sermonindex-node seed status");
                    println!("[seed] once approved, `--scope full` works with no reinstall.");
                }
                None => {
                    bail!(
                        "could not reach the access service. Check the connection and try again,\n\
                         or email {} with this node id: {node_id}",
                        config::SEED_CONTACT_EMAIL
                    );
                }
            }
        }
        "status" | "check" => {
            println!("[seed] node {node_id}");
            if heartbeat::check_seed_access(&client, &node_id).await {
                println!("[seed] GRANTED — `--scope full` is available on this machine.");
            } else {
                println!("[seed] not granted.");
                println!(
                    "[seed] ask for it with:  sermonindex-node seed request --email you@example.com"
                );
                println!("[seed] audio scope (~412 GB) needs no approval and works now.");
            }
        }
        other => bail!(
            "unknown seed command '{other}' — try `seed request --email …` or `seed status`"
        ),
    }
    Ok(())
}

async fn run_daemon(args: &[String]) -> Result<()> {
    let mut settings = config::load_settings();
    let node_id = config::node_id(&mut settings);
    if let Some(dir) = arg_value(args, "--dir") {
        settings["storage_dir"] = serde_json::json!(dir);
        let _ = config::save_settings(&settings);
    }
    let requested_scope = arg_value(args, "--scope").unwrap_or_else(|| config::seed_scope(&settings));
    let no_download = args.iter().any(|a| a == "--no-download");
    let no_dashboard = args.iter().any(|a| a == "--no-dashboard");
    let no_heartbeat =
        args.iter().any(|a| a == "--no-heartbeat") || std::env::var("SI_NO_HEARTBEAT").is_ok();
    let upload_bps = config::upload_limit_bps(&settings);
    // --public-port is a RUN-ONLY override and is deliberately not saved. A
    // wrapper pulling a fresh port from a VPN's CLI on every start should not
    // be quietly rewriting the user's settings file each time, and a stale
    // port left behind in settings.json after the wrapper stops would be worse
    // than none at all — the node would confidently announce a port nothing is
    // listening on. Set SI_PUBLIC_PORT for the same effect from a systemd unit.
    if let Some(p) = arg_value(args, "--public-port") {
        if p.trim().parse::<u16>().map(|n| n > 0).unwrap_or(false) {
            std::env::set_var("SI_PUBLIC_PORT", p.trim());
        } else {
            eprintln!("[public-port] ignoring --public-port {p:?}: not a port number");
        }
    }
    // --port pins the LOCAL listening socket, and unlike --public-port it IS
    // saved. The two differ on purpose: a public port handed out by a VPN is
    // different on every start and must not be written down, whereas a listening
    // port exists precisely so that it is the same tomorrow as it is today —
    // that is the whole reason a router rule can name it.
    if let Some(p) = arg_value(args, "--port") {
        match p.trim().parse::<u16>() {
            Ok(n) if n > 0 => {
                settings["listen_port"] = serde_json::json!(n);
                let _ = config::save_settings(&settings);
            }
            _ => eprintln!("[port] ignoring --port {p:?}: want 1-65535"),
        }
    }
    if args.iter().any(|a| a == "--no-port") {
        // Remove the key rather than storing null, so an unset port reads as
        // genuinely absent in settings.json instead of as a value every future
        // reader has to special-case.
        if let Some(o) = settings.as_object_mut() {
            o.remove("listen_port");
        }
        let _ = config::save_settings(&settings);
    }
    let preferred_port = config::listen_port(&settings);
    // Only the seeding build binds a socket; a --no-default-features build
    // reads the setting and has nothing to do with it.
    #[cfg(not(feature = "seed"))]
    let _ = preferred_port;

    // ── seed-access gate (0.2.8) ────────────────────────────────────────────
    //
    // Full scope means video: ~2.4 TB and a real share of our egress bill. The
    // desktop app has always required a per-machine approval before showing
    // anyone the full-archive controls; the CLI did not, and the public
    // /node-software/ page printed the full-scope command verbatim. This closes
    // that, using the same backend the app uses — no second mechanism.
    //
    // It DOWNGRADES rather than refuses. A node that was running full scope
    // before this release keeps every file it already holds and carries on
    // seeding them; it simply stops fetching new video until it is approved.
    // Refusing to start would take a working mirror offline to enforce a policy
    // about what it downloads NEXT, which is the wrong trade in every case.
    //
    // Audio scope is never gated. The audio archive is the thing we most want
    // copied into as many hands as possible.
    let mut scope = requested_scope.clone();
    let mut full_pending = false;
    if requested_scope == "full" && !no_heartbeat {
        let probe = reqwest::Client::builder()
            .user_agent(format!("sermonindex-node/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .ok();
        let granted = match &probe {
            Some(c) => heartbeat::check_seed_access(c, &node_id).await,
            // Cannot reach the service — do NOT downgrade. An access check that
            // fails open on a network blip is a nuisance; one that fails closed
            // silently turns a trusted seed into an audio node the next time
            // the admin API hiccups, and nobody would know why.
            None => true,
        };
        if !granted {
            scope = "audio".to_string();
            full_pending = true;
            println!(
                "\n[seed] Full-archive scope needs per-machine approval, and this node\n\
                 [seed] does not have it yet. Starting in AUDIO scope (~412 GB) instead —\n\
                 [seed] everything already downloaded stays on disk and keeps seeding.\n\
                 [seed]\n\
                 [seed]   sermonindex-node seed request --email you@example.com\n\
                 [seed]\n\
                 [seed] node id: {node_id}\n\
                 [seed] Once approved it switches itself to full scope on the next\n\
                 [seed] hourly sweep — no restart, no reinstall.\n"
            );
        }
    }

    println!("SermonIndex node {} — scope={scope}", env!("CARGO_PKG_VERSION"));
    // Say what this node will actually DO for content, not just what scope it
    // holds. The desktop app puts its mode in a badge in the title bar; the CLI
    // had nowhere at all that named it, so an operator asking "is my node using
    // the swarm?" had no way to find out short of reading traffic.
    println!("  content:    {}", config::source_mode_line(&settings, &scope));
    println!("  node_id {node_id}");
    println!("  storage {}", config::downloads_dir(&settings).display());
    // The port named here is the one someone would put in a router rule, so it
    // must be the port we are about to try — not the constant. Telling a user
    // with `--port 10000` to pinhole 42800 is worse than saying nothing.
    let banner_port = preferred_port.unwrap_or(config::LISTEN_PORT_START);
    match net::global_ipv6() {
        Some(v6) => println!(
            "  reachable via IPv6 [{v6}]:{banner_port} — no port-forward needed (peers can reach this node directly)"
        ),
        // NOTE: this runs once at startup, and on many machines the DHCPv6
        // lease has not arrived yet — so "none" here does NOT mean the host
        // lacks IPv6 for the rest of the run. The heartbeat re-checks the
        // address every beat. Word it so a stale line can't be misread as a
        // standing fault (it cost real debugging time once).
        None => println!(
            "  no global IPv6 yet (re-checked every heartbeat; a DHCPv6 lease often\n  \
             arrives just after boot). Until then peering needs IPv4 UPnP/NAT-PMP or\n  \
             a forwarded TCP port {banner_port}"
        ),
    }

    let shared = Arc::new(Shared::new(node_id.clone(), scope.clone()));
    // Restore the IPv6 reachability proof from a previous run. It is a fact
    // about the past — a stranger on the internet once dialled this machine —
    // and a restart does not make it untrue.
    if let Some(ts) = state::load_v6_proof() {
        shared.v6_inbound_seen.store(true, Ordering::Relaxed);
        shared.v6_inbound_at.store(ts, Ordering::Relaxed);
    }

    // Dashboard HTTP server (own OS thread).
    if !no_dashboard {
        let sh = shared.clone();
        std::thread::spawn(move || dashboard::serve(sh));
    }

    let client = reqwest::Client::builder()
        .user_agent(format!("sermonindex-node/{}", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(15))
        .build()?;

    // ── local stats loop (2 s) ───────────────────────────────────────────────
    {
        let sh = shared.clone();
        let storage = config::downloads_dir(&settings);
        std::thread::spawn(move || {
            let mut meter = system::Meter::new();
            loop {
                let snap = meter.sample(&storage);
                sh.render(&snap);
                std::thread::sleep(Duration::from_secs(3));
            }
        });
    }

    // ── network view loop (30 s) ─────────────────────────────────────────────
    {
        let sh = shared.clone();
        let cl = client.clone();
        let id = node_id.clone();
        tokio::spawn(async move {
            loop {
                let (map, stats) = heartbeat::fetch_network(&cl).await;
                // Only replace the cached view when the fetch actually returned
                // nodes. A timed-out fetch (common while the download saturates
                // the link) must NOT blank the tiles to zero — keep last good.
                let has_nodes = map
                    .get("nodes")
                    .and_then(|v| v.as_array())
                    .map(|a| !a.is_empty())
                    .unwrap_or(false);
                if has_nodes {
                    *sh.network.lock().unwrap() = state::build_network_view(&map, &stats, &id);
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }

    // ── heartbeat (5 min) + liveness ping (180 s) ────────────────────────────
    if no_heartbeat {
        println!("[heartbeat] disabled (--no-heartbeat)");
    } else {
        let sh = shared.clone();
        let cl = client.clone();
        tokio::spawn(async move {
            let geo = heartbeat::geo(&cl).await;
            loop {
                // Probe the port the session REALLY bound, not the constant.
                // `Seeder::start` walks 42800..=42839 for a free one; testing
                // the constant on a node that landed on 42801 tested a closed
                // port forever and reported this node unreachable forever,
                // however correctly its owner had forwarded the live port.
                // 0 means seeding is off — fall back to the constant, which is
                // the only port anyone could have forwarded anyway.
                let port = match sh.listen_port.load(Ordering::Relaxed) {
                    0 => config::LISTEN_PORT_START,
                    p => p as u16,
                };
                let probe = heartbeat::probe_reachability(&cl, port).await;
                // Passive proof outranks the probe. The probe can only ever test
                // IPv4 (the edge has no outbound IPv6), so believing it alone
                // files every CGNAT household as unreachable — which is most of
                // the homes we are asking to run a node.
                let reachable = sh.reachable_verdict(probe);
                sh.set_reachable(reachable);
                // Blue ONLY when the node admin has granted this node seed status
                // (seed_access.enabled). We must keep reporting it each beat or the
                // server upsert reverts node_type to "user".
                let granted = heartbeat::check_seed_access(&cl, &sh.node_id).await;
                sh.set_granted(granted);
                // The response may carry a newer released version. This is a
                // free push channel — the request is happening regardless — so
                // a release can reach the fleet in one beat instead of waiting
                // out the 6-hour manifest poll.
                // What this node is SHARING, not what it holds — prepared by
                // the seed-stats poller (which is the task that has the
                // seeder) and taken here. Absent on most beats by design: the
                // server keeps the rows between reports, so an accurate figure
                // costs one bounded payload an hour instead of ~1.6 MB every
                // five minutes. Before this the CLI sent nothing at all and
                // every headless node read "sharing 0" while holding the whole
                // archive.
                let seeded_now = sh.seeded_report.lock().unwrap().take();

                if let Some(body) = heartbeat::beat(&cl, &sh, &geo, reachable, granted, seeded_now).await {
                    if let Some(v) = update::from_heartbeat(&body) {
                        *sh.update_available.lock().unwrap() = Some(v);
                    }
                    // Network-wide settings from the console. Both of these were
                    // delivered on every beat and read by nothing until 0.3.0.
                    let (mode, mlv) = heartbeat::remote_config(&body);
                    let (dl_max, hb) = heartbeat::remote_limits(&body);
                    if let Some(n) = dl_max {
                        if sh.dl_ceiling.swap(n, Ordering::Relaxed) != n {
                            println!("[config] network download ceiling is now {n} files at once");
                        }
                    }
                    if let Some(n) = hb {
                        if sh.hb_interval.swap(n, Ordering::Relaxed) != n {
                            println!("[config] heartbeat interval is now {n}s");
                        }
                    }
                    if let Some(m) = mode {
                        let mut cur = sh.source_mode.lock().unwrap();
                        if *cur != m {
                            // Name the behaviour, not just the key. An operator
                            // reading a log should not have to remember what the
                            // third option was called.
                            // Same words the console and the app use.
                            let (name, says) = config::source_mode_names(&m);
                            println!("[config] Content source: {name} — {says}");
                            *cur = m.clone();
                            drop(cur);
                            // Remembered on disk so `status` — a one-shot
                            // command that never sees a heartbeat — can still
                            // say which way the switch is thrown. It is a
                            // CACHE of what the console last said, not a local
                            // setting: the console remains the authority and
                            // overwrites this on the next beat.
                            let mut st = config::load_settings();
                            if let Some(o) = st.as_object_mut() {
                                o.insert(
                                    "last_source_mode".to_string(),
                                    serde_json::Value::String(m.clone()),
                                );
                            }
                            if let Err(e) = config::save_settings(&st) {
                                eprintln!("[config] could not remember the content source: {e:#}");
                            }
                        }
                    }
                    if let Some(v) = mlv {
                        let mut cur = sh.remote_ml_version.lock().unwrap();
                        // Empty means "first beat of this run" — adopt the value
                        // without sweeping, or every restart would trigger one.
                        if cur.is_empty() {
                            *cur = v;
                        } else if *cur != v {
                            *cur = v;
                            drop(cur);
                            println!("[masterlist] the console asked every node to refresh");
                            let _ = std::fs::write(refresh_flag(), b"");
                        }
                    }
                    // Peer-assisted reachability: the server may ask us to dial
                    // another node, because we can reach places our own probe
                    // edge cannot (it has no IPv6 at all). One per beat, and
                    // peercheck::perform refuses anything that is not a public
                    // address regardless of what the server said.
                    if let Some((ip, port, token)) = peercheck::request_from(&body) {
                        if let Some(open) = peercheck::perform(&ip, port).await {
                            *sh.pending_check.lock().unwrap() =
                                Some(peercheck::result_field(&token, open));
                        }
                    }
                }
                // Was a hard-coded 300. `heartbeat_interval` on the console
                // now moves it, within the 60–3600 the reader enforces.
                let every = match sh.hb_interval.load(Ordering::Relaxed) {
                    0 => 300,
                    n => n,
                };
                tokio::time::sleep(Duration::from_secs(every)).await;
            }
        });
        let cl = client.clone();
        let id = node_id.clone();
        tokio::spawn(async move {
            loop {
                heartbeat::ping(&cl, &id).await;
                tokio::time::sleep(Duration::from_secs(180)).await;
            }
        });
    }

    // ── master list ──────────────────────────────────────────────────────────
    let ml = masterlist::fetch_verified(&client).await?;
    println!("[masterlist] verified v{} — {} entries", ml.version, ml.entries.len());
    let want_audio = scope != "full";
    let entries: Vec<masterlist::Entry> = ml
        .entries
        .values()
        .filter(|e| if want_audio { e.is_audio() } else { e.is_audio() || e.is_video() })
        .cloned()
        .collect();
    shared.total.store(entries.len() as u64, Ordering::Relaxed);
    println!("[scope] {} files in scope", entries.len());

    // Count what's already present, and (with seeding) start seeding it.
    // The bound lives inside the vendored librqbit, which reads it from the
    // environment — set it before any session is created so the OnceLock inside
    // picks up the configured value rather than the built-in default.
    let max_peers = config::max_peers_per_torrent(&settings);
    std::env::set_var("SI_MAX_PEERS_PER_TORRENT", max_peers.to_string());
    let dht = config::dht_enabled(&settings);
    println!(
        "  peers   max {} discovered per torrent{}",
        max_peers,
        if max_peers == 0 { " (UNBOUNDED — upstream behaviour)" } else { "" }
    );
    println!("  dht     {}", if dht { "on" } else { "off — trackers only (fewer peers found)" });
    // Rotation keeps memory flat regardless of library size: the full library
    // stays registered (paused = ~6 KB each) while only a window is live.
    let rotate_window = config::active_torrents(&settings);
    let rotate_mins = config::rotate_minutes(&settings);
    if rotate_window > 0 && entries.len() > rotate_window {
        let cycles = entries.len().div_ceil(rotate_window);
        println!(
            "  rotate  {rotate_window} torrents live at a time, advancing every {rotate_mins} min \
             (full library ≈ every {:.1} h)",
            (cycles as f64 * rotate_mins as f64) / 60.0
        );
    } else if rotate_window > 0 {
        println!("  rotate  library fits the {rotate_window}-torrent window — everything stays live");
    } else {
        println!("  rotate  off — every torrent live at once (active_torrents=0)");
    }
    println!("  quiet   {}", config::describe_quiet(&settings));
    #[cfg(feature = "seed")]
    let peer_limit = config::peer_limit_per_torrent(&settings);
    println!(
        "  conns   {} per torrent ({:.0} GB RAM detected)",
        peer_limit,
        config::total_ram_gb()
    );

    // ── Public port ──────────────────────────────────────────────────────────
    //
    // This has to be settled BEFORE the session starts. librqbit stamps the
    // announce port into every tracker announce and every DHT announce_peer at
    // construction and never lets it change, so "discover it later" is not an
    // option — a session already telling 25,000 swarms to dial 42800 cannot be
    // talked out of it.
    //
    // Order of preference:
    //   1. An explicit setting (`config public-port`, or SI_PUBLIC_PORT) — a
    //      wrapper that pulls a port from a VPN's own CLI hands it over here.
    //   2. NAT-PMP, asked at startup. On a home router this maps the port we
    //      are about to bind; inside a Proton VPN tunnel it returns the
    //      provider's forwarded port, which is exactly the number we need and
    //      which nobody had to copy by hand.
    //   3. Nothing — announce the port we bind, which is right for everyone
    //      with a normal forwarded port.
    let explicit_public = config::public_port(&settings);
    let mut announce_port = explicit_public;
    let mut mapped: Option<natpmp::MappingResult> = None;

    if announce_port.is_none() && config::natpmp_enabled(&settings) {
        print!("  natpmp  asking the gateway… ");
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        mapped = natpmp::map_once(config::LISTEN_PORT_START).await;
        match &mapped {
            Some(m) => {
                println!(
                    "port {} via {} (granted {}s)",
                    m.tcp_external_port, m.gateway, m.lifetime_secs
                );
                if m.lifetime_secs <= 120 {
                    // A short lifetime is the signature of a VPN rather than a
                    // home router, and it is worth naming: it means this node's
                    // reachability depends on a renewal every few seconds, and
                    // on the tunnel staying up.
                    println!(
                        "          short lifetime — this looks like a VPN tunnel. The mapping\n\
                         \x20         will be renewed every {}s; if the tunnel drops, so does\n\
                         \x20         inbound reachability until it returns.",
                        (m.lifetime_secs / 2).clamp(15, 3600)
                    );
                }
                announce_port = Some(m.tcp_external_port);
            }
            None => println!("no gateway answered (normal on most networks)"),
        }
    } else if let Some(p) = explicit_public {
        println!("  public  port {p} (set explicitly — peers will be told to dial this)");
    }

    let seeder = match seed::Seeder::start(
        &config::downloads_dir(&settings),
        upload_bps,
        dht,
        peer_limit,
        announce_port,
        preferred_port,
    )
    .await
    {
        Ok(s) => {
            // Publish the port that was ACTUALLY bound so the reachability
            // probe, the heartbeat and /stats all report the same real number.
            // Report the PUBLIC port — the one the outside world dials — to
            // the probe, the heartbeat and /stats. On a plain node that is the
            // port we bound; behind a VPN it is the provider's forwarded port,
            // and testing the local one would fail for ever while the node was
            // in fact perfectly reachable.
            shared.listen_port.store(s.public_port() as u64, Ordering::Relaxed);
            shared.local_port.store(s.port() as u64, Ordering::Relaxed);
            if s.public_port() != s.port() {
                println!(
                    "  public  listening on {} · peers are told to dial {}",
                    s.port(),
                    s.public_port()
                );
            }
            // A pinned port that did not stick is worth shouting about. The
            // only reason to pin one is that a router rule names it, so silent
            // drift means that rule now points at nothing — and the node looks
            // unreachable for a reason nothing else on screen would explain.
            match preferred_port {
                Some(p) if p != s.port() => {
                    eprintln!(
                        "  WARNING port {p} was not available — listening on {} instead.\n           \
                         Any router forward or IPv6 pinhole naming {p} will NOT match.\n           \
                         Free {p} and restart, or re-pin with --port {}.",
                        s.port(),
                        s.port()
                    );
                }
                Some(p) => println!("  port    {p} (pinned)"),
                None => {}
            }
            // Keep the mapping alive, holding the SAME external port so the
            // number already announced to 25,000 swarms stays true. The renewal
            // interval comes from the lifetime the gateway granted, not the one
            // we asked for — Proton grants 60 seconds.
            if config::natpmp_enabled(&settings) {
                let st = shared.natpmp.clone();
                *st.lock().unwrap() = "trying".to_string();
                let hold = mapped.as_ref().map(|m| m.tcp_external_port).or(explicit_public);
                natpmp::spawn(s.port(), hold, st);
            } else {
                *shared.natpmp.lock().unwrap() = "off".to_string();
            }
            if s.port() != config::LISTEN_PORT_START {
                println!(
                    "  port    {} (not the default {} — forward THIS one)",
                    s.port(),
                    config::LISTEN_PORT_START
                );
            }
            if s.is_ipv6_socket() {
                println!(
                    "  socket  bound on [::] — if IPv4 inbound never opens, the\n\
                     \x20         listener may be IPv6-only on this host"
                );
            }
            Some(Arc::new(s))
        }
        Err(e) => {
            eprintln!("[seed] disabled — could not start session: {e:#}");
            None
        }
    };
    #[cfg(not(feature = "seed"))]
    let _ = upload_bps;

    let torrents_dir = config::torrents_dir();
    let _ = &torrents_dir; // used only under the "seed" feature
    let mut dl_state = state::load_download_state();
    // (path, master-list info_hash) — the hash lets seed_file reuse its cached
    // .torrent instead of re-hashing the whole file on every startup.
    let mut present_paths: Vec<(std::path::PathBuf, String)> = Vec::new();
    let mut present = 0u64;
    let mut present_bytes = 0u64;
    for e in &entries {
        let p = config::file_path(&settings, &e.name);
        if std::fs::metadata(&p).map(|m| m.len() == e.size).unwrap_or(false) {
            present += 1;
            present_bytes += e.size;
            shared.held.store(present, Ordering::Relaxed); // report immediately while scanning
            // Keep the reported storage in step with the count as the scan runs,
            // so an early heartbeat during a long scan is never wildly low.
            shared.storage_bytes.store(present_bytes, Ordering::Relaxed);
            dl_state[e.id()] = serde_json::json!({ "downloaded": true, "diskSize": e.size });
            present_paths.push((p, e.info_hash.clone()));
        }
    }
    state::save_download_state(&dl_state);
    println!("[library] {present}/{} already held ({:.1}% coverage)", entries.len(), shared.coverage_pct());

    // Register the already-present files for seeding in the BACKGROUND — re-hashing
    // thousands of files is slow and must not block the held count or downloads.
    #[cfg(feature = "seed")]
    let rotate_on = rotate_window > 0;
    #[cfg(feature = "seed")]
    if let Some(s) = seeder.clone() {
        let td = torrents_dir.clone();
        tokio::spawn(async move {
            // With rotation on, register everything PAUSED — near-zero live
            // state — and let the rotation task decide what is live. Without
            // it, go live immediately (the old behaviour).
            for (p, ih) in present_paths {
                let _ = s.seed_file(&p, &td, Some(&ih), rotate_on).await;
            }
            println!("[seed] finished registering {} held files for seeding", s.torrent_count());
        });
    }

    // ── quiet-hours supervisor ───────────────────────────────────────────────
    // Checks the schedule every minute, re-reading settings.json from disk so
    // `sermonindex-node quiet add …` (or a GUI edit — the file is shared)
    // applies without a restart. While quiet: every torrent paused, incoming
    // handshakes refused (wake-on-demand gated off), downloads held. The
    // re-pause repeats each minute so nothing that slipped live mid-transition
    // stays live. Heartbeats and the dashboard keep running.
    #[cfg(feature = "seed")]
    {
        let sh = shared.clone();
        let sd = seeder.clone();
        tokio::spawn(async move {
            let mut was_quiet = false;
            loop {
                let s = config::load_settings();
                let why = config::quiet_now(&s);
                let quiet = why.is_some();
                if quiet {
                    librqbit::SI_ACCEPT_INCOMING.store(false, Ordering::Relaxed);
                    sh.quiet.store(true, Ordering::Relaxed);
                    if let Some(sd) = &sd {
                        let n = sd.pause_all().await;
                        if !was_quiet {
                            println!(
                                "[quiet] {} — paused {n} torrents; seeding, serving and downloads suspended",
                                why.unwrap_or_default()
                            );
                        }
                    }
                } else {
                    librqbit::SI_ACCEPT_INCOMING.store(true, Ordering::Relaxed);
                    sh.quiet.store(false, Ordering::Relaxed);
                    if was_quiet {
                        println!("[quiet] window ended — resuming normal operation");
                    }
                }
                was_quiet = quiet;
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }

    #[cfg(not(feature = "seed"))]
    {
        // Download-only build: same schedule, gating just the fetch loop.
        let sh = shared.clone();
        tokio::spawn(async move {
            let mut was_quiet = false;
            loop {
                let s = config::load_settings();
                let quiet = config::quiet_now(&s).is_some();
                if quiet != was_quiet {
                    println!(
                        "[quiet] {}",
                        if quiet { "quiet hours — downloads suspended" } else { "window ended — resuming" }
                    );
                    was_quiet = quiet;
                }
                sh.quiet.store(quiet, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        });
    }

    // ── rotation task ────────────────────────────────────────────────────────
    // Advances the live window through the library every rotate_minutes. Runs
    // from the start so early-registered torrents begin seeding while the
    // (long) registration pass is still working through the rest.
    #[cfg(feature = "seed")]
    if let Some(s) = seeder.clone() {
        if rotate_on {
            let dwell = Duration::from_secs(rotate_mins * 60);
            // Stagger the starting window by node id so a fleet of nodes never
            // sweeps the library in lockstep (all announcing the SAME window —
            // the worst case for coverage). The id hash spreads cursors across
            // the range; rotate_tick reduces it mod the library size.
            let stagger = {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                node_id.hash(&mut h);
                h.finish() as usize
            };
            let sh = shared.clone();
            tokio::spawn(async move {
                let mut cursor = stagger;
                let mut last_tick: Option<std::time::Instant> = None;
                loop {
                    // During quiet hours the supervisor has paused everything —
                    // do nothing until the window ends. Dwell time keeps
                    // accruing, so rotation resumes immediately afterwards.
                    let quiet = sh.quiet.load(Ordering::Relaxed);
                    let due = last_tick.map(|t| t.elapsed() >= dwell).unwrap_or(true);
                    if !quiet && due {
                        let (live, kept, total) = s.rotate_tick(rotate_window, cursor).await;
                        if total >= rotate_window {
                            println!(
                                "[seed] rotation — {live} live of {total} (window at {}{})",
                                cursor % total,
                                if kept > 0 {
                                    format!(", {kept} kept for connected peers")
                                } else {
                                    String::new()
                                }
                            );
                            cursor = cursor.wrapping_add(rotate_window);
                            last_tick = Some(std::time::Instant::now());
                        }
                        // else: registration still filling the session — the
                        // tick above brought everything so far live; re-check
                        // soon (last_tick stays None → due stays true).
                    }
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            });
        }
    }

    // ── seed stats poller ────────────────────────────────────────────────────
    // Poll interval doubles as the integration step for uploaded_bytes below,
    // so the two can never drift apart.
    #[cfg(feature = "seed")]
    const POLL_SECS: u64 = 10;
    #[cfg(feature = "seed")]
    if let Some(s) = seeder.clone() {
        let sh = shared.clone();
        tokio::spawn(async move {
            // The direction scan walks the peer table of every LIVE torrent, so
            // it is much heavier than reading the session snapshot. Under
            // rotation only the live window is live (a few hundred at most), so
            // it is bounded — but there is no reason to pay it every 10 s when
            // the thing it measures moves on the scale of minutes.
            let mut tick: u64 = 0;
            loop {
                let (up, peers, torrents) = s.stats();
                sh.seed_up_bps.store(up, Ordering::Relaxed);
                sh.peers.store(peers, Ordering::Relaxed);
                sh.torrents.store(torrents, Ordering::Relaxed);
                // Prepare the seeded-torrent report on the first tick and then
                // hourly. This task is the one holding the seeder, and it
                // already runs on a timer — so the cadence lives here and the
                // heartbeat simply sends whatever is waiting. 2000 is a hard
                // cap on top of the rotation window: a cap that depends on
                // another setting being sane is not a cap.
                if tick % 360 == 0 {
                    let live = s.live_torrents(2000);
                    let mut m = serde_json::Map::new();
                    for (name, hash) in live {
                        let id = name.rsplit_once('.').map(|(a, _)| a).unwrap_or(&name).to_string();
                        m.insert(id, serde_json::json!({ "info_hash": hash }));
                    }
                    *sh.seeded_report.lock().unwrap() = Some(serde_json::Value::Object(m));
                }
                if tick % 6 == 0 {
                    // every ~60 s
                    let d = s.directions();
                    sh.set_directions(d.incoming, d.outgoing);
                    // The only honest IPv6 reachability signal we will ever get
                    // — our probe edge has no outbound IPv6, so this passive
                    // observation is the whole answer for a CGNAT household.
                    if d.inbound_ipv6 {
                        sh.note_v6_inbound();
                    }
                }
                tick = tick.wrapping_add(1);
                // librqbit exposes no lifetime "total uploaded" counter we can
                // read reliably, so approximate one: integrate the instantaneous
                // rate over this tick. Monotonic and self-consistent — and,
                // unlike the old placeholder, not permanently zero.
                sh.uploaded_bytes.fetch_add(up * POLL_SECS, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;
            }
        });
    }

    // ── download loop (work-stealing, bounded concurrency) ───────────────────
    //
    // 0.2.7 replaced `futures_util::stream::for_each_concurrent(4, …)` here,
    // and the reason is worth keeping written down because the old code looked
    // completely reasonable.
    //
    // `for_each_concurrent` pulls from its stream IN ORDER. Four slow files
    // therefore occupied all four slots and every healthy file behind them
    // waited — with thousands queued, one bad mirror presented as a total
    // freeze. Operators reported it as a hang, which it was not: it was strict
    // ordering meeting a source nobody was timing.
    //
    // Workers now pull from a shared queue whenever they are free, so a slow
    // file costs exactly one slot and nothing queues behind it. Paired with the
    // watchdog in download.rs (which gives up on a crawling source after ~30 s
    // and resumes from the next one), a bad mirror now costs seconds instead of
    // a night.
    if !no_download {
        let storage_root = config::downloads_dir(&settings);
        let missing: Vec<masterlist::Entry> = entries
            .iter()
            .filter(|e| {
                let p = storage_root.join(config::shard_for(&e.name)).join(&e.name);
                !std::fs::metadata(&p).map(|m| m.len() == e.size).unwrap_or(false)
            })
            .cloned()
            .collect();
        let total = entries.len();
        // The operator's own `config downloads <n>`, held under the network
        // ceiling the console publishes. Below it freely; never above it — that
        // is what makes the console value a ceiling rather than a second,
        // competing setting that silently wins or loses depending on order.
        let workers = {
            let local = config::download_workers(&settings);
            match shared.dl_ceiling.load(Ordering::Relaxed) {
                0 => local,
                ceiling => local.min(ceiling as usize),
            }
        };
        let prefer_archive = config::prefer_archive(&settings, &scope);
        println!(
            "[download] {} files to fetch ({workers} in parallel, {} first)",
            missing.len(),
            if prefer_archive { "Archive.org" } else { "CDN" }
        );

        let dl_state = std::sync::Arc::new(tokio::sync::Mutex::new(dl_state));
        // The queue every worker steals from. A Mutex<VecDeque> rather than an
        // mpsc channel because a worker must be able to put a job BACK (the
        // quiet-hours path below), and because "how many are left" is a useful
        // thing to be able to ask.
        let queue = std::sync::Arc::new(tokio::sync::Mutex::new(
            std::collections::VecDeque::from(missing),
        ));
        // Archive.org throttles per client, so cap how many workers may sit on
        // it at once. A worker that cannot get a permit does not WAIT — it
        // reorders its own source list to put the CDN first and gets on with
        // it. Blocking here would reintroduce exactly the stall we just removed.
        let archive_slots = std::sync::Arc::new(tokio::sync::Semaphore::new(
            config::archive_slots(&settings),
        ));

        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            let client = client.clone();
            let shared = shared.clone();
            let dl_state = dl_state.clone();
            let storage_root = storage_root.clone();
            let queue = queue.clone();
            let archive_slots = archive_slots.clone();
            let refresh = refresh_flag();
            #[cfg(feature = "seed")]
            let torrents_dir = torrents_dir.clone();
            #[cfg(feature = "seed")]
            let seeder = seeder.clone();
            handles.push(tokio::spawn(async move {
                loop {
                    // Stop pulling new work the moment the drive fills.
                    if shared.disk_full.load(Ordering::Relaxed) {
                        return;
                    }
                    // Quiet hours: don't START new fetches inside the window.
                    // A file already in flight finishes first — it is small.
                    while shared.quiet.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_secs(60)).await;
                    }
                    let e = match queue.lock().await.pop_front() {
                        Some(e) => e,
                        None => return, // queue drained — this worker is done
                    };
                    let mut urls: Vec<String> = e.download_urls_for(prefer_archive);
                    if urls.is_empty() {
                        continue;
                    }
                    // Hold an Archive permit only while we actually lead with
                    // Archive; `try_acquire` so a full house costs no time.
                    let _permit = if download::is_archive(&urls[0]) {
                        match archive_slots.clone().try_acquire_owned() {
                            Ok(p) => Some(p),
                            Err(_) => {
                                // Too many workers on Archive already — this
                                // one takes the CDN copy instead of queueing.
                                urls.sort_by_key(|u| download::is_archive(u));
                                None
                            }
                        }
                    } else {
                        None
                    };
                    let dest = storage_root.join(config::shard_for(&e.name)).join(&e.name);

                    // ── the swarm, first ─────────────────────────────────────
                    // Before 0.3.0 every node acquired its library over HTTP and
                    // used BitTorrent only to serve. Every node a seeder, no node
                    // ever a leecher — so there was no demand in the swarm and
                    // fleet-wide upload across 52 nodes was exactly zero. The
                    // network had a supply side and nothing else.
                    //
                    // This is the consumer side. It is OFF unless the console
                    // says otherwise, because until enough nodes are complete the
                    // swarm is the slower answer, and it gives up fast when no
                    // peer has the file — the HTTP path behind it always works,
                    // so a missed swarm fetch costs seconds, never a stalled slot.
                    //
                    // Where it pays: a node holding 96% of the archive and a node
                    // at 60% are usually in the same region, often the same
                    // operator's rack, and the files the second one still needs
                    // are exactly the Archive.org-only ones that trickle. Those
                    // become a transfer between two machines instead of a queue
                    // behind a throttle.
                    #[cfg(feature = "seed")]
                    let mut got_from_swarm = false;
                    #[cfg(feature = "seed")]
                    if !e.magnet.is_empty() {
                        let mode = shared.source_mode.lock().unwrap().clone();
                        if mode == "p2p" || mode == "hybrid" {
                            if let Some(sd) = &seeder {
                                if let Some(parent) = dest.parent() {
                                    match sd
                                        .fetch_file(
                                            &e.magnet,
                                            parent,
                                            e.size,
                                            Duration::from_secs(25),
                                            Duration::from_secs(45),
                                        )
                                        .await
                                    {
                                        Ok(true) => {
                                            // The swarm verified every piece, but
                                            // the SIGNED master list is the
                                            // authority on what the file should
                                            // be. Trusting the swarm on size
                                            // would let a bad torrent define
                                            // correctness.
                                            got_from_swarm = std::fs::metadata(&dest)
                                                .map(|m| m.len() == e.size)
                                                .unwrap_or(false);
                                        }
                                        Ok(false) => {}
                                        Err(err) => {
                                            eprintln!("[p2p] {}: {err:#}", e.name);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // `p2p` means the swarm ONLY — an operator who asked for that
                    // did not ask for a silent fall back to the CDN. `hybrid`
                    // does fall back, which is what makes it the safe default to
                    // recommend.
                    #[cfg(feature = "seed")]
                    if !got_from_swarm && shared.source_mode.lock().unwrap().as_str() == "p2p" {
                        continue;
                    }
                    #[cfg(feature = "seed")]
                    let outcome = if got_from_swarm {
                        Ok(download::Outcome::Downloaded)
                    } else {
                        download::ensure_file(&client, &urls, &dest, e.size).await
                    };
                    #[cfg(not(feature = "seed"))]
                    let outcome = download::ensure_file(&client, &urls, &dest, e.size).await;

                    /* WHERE IT CAME FROM. Counted only when the file actually
                       landed, so a failed CDN attempt after a failed swarm
                       attempt does not inflate either side. This is the figure
                       that tells an operator — and the console — whether P2P
                       Primary is doing anything at all. */
                    if matches!(outcome, Ok(download::Outcome::Downloaded)) {
                        #[cfg(feature = "seed")]
                        let via_swarm = got_from_swarm;
                        #[cfg(not(feature = "seed"))]
                        let via_swarm = false;
                        if via_swarm {
                            shared.swarm_files.fetch_add(1, Ordering::Relaxed);
                            shared.swarm_bytes.fetch_add(e.size, Ordering::Relaxed);
                        } else {
                            shared.http_files.fetch_add(1, Ordering::Relaxed);
                            shared.http_bytes.fetch_add(e.size, Ordering::Relaxed);
                        }
                    }
                    if matches!(outcome, Ok(download::Outcome::NoSpace)) {
                        // Say it ONCE, loudly, and stop asking for more. The
                        // node keeps seeding everything it already holds —
                        // a full disk is a reason to stop growing, never a
                        // reason to stop serving.
                        if !shared.disk_full.swap(true, Ordering::Relaxed) {
                            eprintln!(
                                "\n[disk] The drive is FULL. Downloading has stopped; seeding continues.\n\
                                 [disk] Free some space, or point the node at a bigger drive with:\n\
                                 [disk]   sermonindex-node config dir /path/to/bigger/drive\n"
                            );
                        }
                        return;
                    }
                    if shared.disk_full.load(Ordering::Relaxed) {
                        return;
                    }
                    if let Ok(download::Outcome::Downloaded) | Ok(download::Outcome::Skipped) = outcome {
                        let held = shared.held.fetch_add(1, Ordering::Relaxed) + 1;
                        shared.storage_bytes.fetch_add(e.size, Ordering::Relaxed);
                        {
                            let mut ds = dl_state.lock().await;
                            ds[e.id()] = serde_json::json!({ "downloaded": true, "diskSize": e.size });
                            if held % 25 == 0 {
                                state::save_download_state(&ds);
                            }
                        }
                        if held % 25 == 0 {
                            println!("[download] {held}/{total} held ({:.1}%)", shared.coverage_pct());
                        }
                        #[cfg(feature = "seed")]
                        if let Some(s) = &seeder {
                            // Freshly downloaded files go live immediately —
                            // the rotation folds them into its cycle on the
                            // next tick.
                            let _ = s.seed_file(&dest, &torrents_dir, Some(&e.info_hash), false).await;
                        }
                    }
                    // `refresh-now` used to be visible only BETWEEN passes, so
                    // during a long download the command appeared to do
                    // nothing and operators ran it repeatedly. Checking it here
                    // costs one stat() per file and makes it responsive: the
                    // flag is LEFT in place for the sweep loop to consume, we
                    // only stop early so that sweep can start.
                    if refresh.exists() {
                        println!("[download] refresh requested — ending this pass early");
                        return;
                    }
                }
            }));
        }
        for h in handles {
            let _ = h.await;
        }

        let ds = dl_state.lock().await;
        state::save_download_state(&ds);
        println!("[download] pass complete — {} held", shared.held.load(Ordering::Relaxed));
    }

    shared.downloading.store(false, Ordering::Relaxed);

    // ── history recorder ─────────────────────────────────────────────────────
    // `Trends` is 90 samples at 2 s — three minutes, in RAM, gone on restart.
    // That cannot answer "over a week, how much did I give versus take?", which
    // is the question a person running a node actually has. One appended line
    // an hour can.
    {
        let sh = shared.clone();
        tokio::spawn(async move {
            // Seed the served window from disk immediately, so a node that has
            // been running for months shows its history the moment it restarts
            // rather than drawing an empty chart for an hour.
            *sh.history.lock().unwrap() = state::load_history();
            loop {
                tokio::time::sleep(Duration::from_secs(config::HISTORY_INTERVAL_SECS)).await;
                let up = sh.uploaded_bytes.load(Ordering::Relaxed);
                let down = sh.storage_bytes.load(Ordering::Relaxed);
                state::append_history(&sh, up, down);
                *sh.history.lock().unwrap() = state::load_history();
            }
        });
    }

    // ── weekly integrity sweep ───────────────────────────────────────────────
    //
    // `verify` has always existed as a manual command, which means it runs when
    // somebody already suspects something. A 24/7 node needs the opposite: a
    // check that runs when nobody suspects anything, because that is when bit
    // rot and a half-written file from a power cut actually happen — and a
    // corrupt file does not announce itself, it just fails every piece hash for
    // every peer that ever asks, quietly, for months.
    //
    // Cheap by design: metadata only (presence + exact size), no re-hashing.
    // At ~30k files that is a stat() each, a few seconds even on an SD card.
    // Runs during quiet hours so it never competes with serving, and only when
    // a week has passed since the last one — recorded on disk, so restarting
    // the node does not restart the clock.
    {
        let sh = shared.clone();
        let settings_v = settings.clone();
        let entries_v: Vec<(String, u64)> =
            entries.iter().map(|e| (e.name.clone(), e.size)).collect();
        tokio::spawn(async move {
            let stamp = config::data_dir().join("last-verify");
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                let last = std::fs::read_to_string(&stamp)
                    .ok()
                    .and_then(|t| t.trim().parse::<u64>().ok())
                    .unwrap_or(0);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if now.saturating_sub(last) < 7 * 86_400 {
                    continue;
                }
                // Prefer a quiet window. If the node has none configured we
                // would wait for ever, so after 8 days run it regardless —
                // metadata-only, it costs almost nothing either way.
                if !sh.quiet.load(Ordering::Relaxed) && now.saturating_sub(last) < 8 * 86_400 {
                    continue;
                }
                let mut ok = 0u64;
                let mut missing = 0u64;
                let mut wrong = 0u64;
                for (name, size) in &entries_v {
                    let p = config::file_path(&settings_v, name);
                    match std::fs::metadata(&p) {
                        Ok(m) if m.len() == *size => ok += 1,
                        Ok(_) => wrong += 1,
                        Err(_) => missing += 1,
                    }
                }
                let _ = std::fs::write(&stamp, now.to_string());
                if wrong > 0 || missing > 0 {
                    println!(
                        "[verify] weekly check — {ok} good, {wrong} WRONG SIZE, {missing} missing"
                    );
                    if wrong > 0 {
                        println!(
                            "[verify] a wrong-size file fails every piece hash it is asked for. \
                             Delete those and the next refresh will re-fetch them."
                        );
                    }
                } else {
                    println!("[verify] weekly check — all {ok} files present and correct");
                }
                // Keep the reported figure honest after the sweep.
                sh.held.store(ok, Ordering::Relaxed);
            }
        });
    }

    // ── update check (notify only) ───────────────────────────────────────────
    {
        let sh = shared.clone();
        let cl = client.clone();
        tokio::spawn(async move {
            let mut announced: Option<String> = None;
            loop {
                if let Some(v) = update::check(&cl).await {
                    *sh.update_available.lock().unwrap() = Some(v.clone());
                    // Say it once per version, not once per check — a daemon
                    // that logs the same line every six hours for a month is a
                    // daemon whose log nobody reads.
                    if announced.as_deref() != Some(v.as_str()) {
                        println!("\n[update] {}\n", update::install_hint(&v));
                        announced = Some(v);
                    }
                }
                tokio::time::sleep(Duration::from_secs(config::UPDATE_CHECK_SECS)).await;
            }
        });
    }

    // ── master-list refresh ──────────────────────────────────────────────────
    // Before 0.2.2 the list was read once, at startup, and the missing-file set
    // computed once. A node up for three weeks served the library as it stood
    // three weeks ago, and only a restart ever fixed it. This also doubles as
    // the retry path for files that failed the first pass (Archive 503, a
    // transient DNS blip): the filter is recomputed from what is on disk, so a
    // file that did not land is simply missing again next hour.
    if !no_download {
        let cl = client.clone();
        let sh = shared.clone();
        let settings_r = settings.clone();
        let mut scope_r = scope.clone();
        // Set when the operator asked for full scope and did not have access at
        // startup. The sweep re-checks once an hour, so an approval granted
        // while the node is running takes effect on its own — the operator does
        // not have to be told to restart, and more to the point does not have
        // to be WATCHING to know it happened.
        let mut awaiting_seed_grant = full_pending;
        let node_id_r = node_id.clone();
        #[cfg(feature = "seed")]
        let torrents_dir_r = torrents_dir.clone();
        #[cfg(feature = "seed")]
        let seeder_r = seeder.clone();
        let mut known_version = ml.version;
        sh.masterlist_version.store(known_version, Ordering::Relaxed);
        tokio::spawn(async move {
            loop {
                // Sleep in one-minute slices rather than one long hour, so
                // `sermonindex-node refresh` can cut the wait short. The sweep
                // is idempotent — it recomputes what is missing from what is on
                // disk — so running it early costs nothing but a list fetch.
                wait_or_refresh(config::MASTERLIST_REFRESH_SECS).await;
                if sh.quiet.load(Ordering::Relaxed) {
                    continue;
                }
                // Was this node approved for full scope while it was running?
                // One cheap GET an hour, and only while a request is actually
                // outstanding — a node that never asked never calls this.
                if awaiting_seed_grant && heartbeat::check_seed_access(&cl, &node_id_r).await {
                    awaiting_seed_grant = false;
                    scope_r = "full".to_string();
                    println!(
                        "[seed] approved — switching to FULL scope. Video files will start\n\
                         [seed] arriving on this sweep. Nothing already held is affected."
                    );
                }
                // A full disk clears itself only by someone acting. Re-check
                // each hour rather than latching forever: they may well have
                // freed space, and nothing else would notice.
                if sh.disk_full.load(Ordering::Relaxed) {
                    let free = system::free_bytes(&config::downloads_dir(&settings_r));
                    if free > 2 * 1024 * 1024 * 1024 {
                        sh.disk_full.store(false, Ordering::Relaxed);
                        println!("[disk] space is available again — resuming downloads");
                    } else {
                        continue;
                    }
                }
                // Fail-closed, exactly like the startup fetch: an unverified
                // list is never used, and a failed fetch just means we try
                // again next hour with what we already have.
                let fresh = match masterlist::fetch_verified(&cl).await {
                    Ok(m) => m,
                    Err(e) => {
                        eprintln!("[masterlist] refresh failed (keeping v{known_version}): {e:#}");
                        continue;
                    }
                };
                let want_audio = scope_r != "full";
                let all: Vec<masterlist::Entry> = fresh
                    .entries
                    .values()
                    .filter(|e| if want_audio { e.is_audio() } else { e.is_audio() || e.is_video() })
                    .cloned()
                    .collect();
                sh.total.store(all.len() as u64, Ordering::Relaxed);
                if fresh.version != known_version {
                    println!(
                        "[masterlist] v{known_version} -> v{} — {} files in scope",
                        fresh.version,
                        all.len()
                    );
                    known_version = fresh.version;
                    sh.masterlist_version.store(known_version, Ordering::Relaxed);
                }

                let missing: Vec<masterlist::Entry> = all
                    .iter()
                    .filter(|e| {
                        let p = config::file_path(&settings_r, &e.name);
                        !std::fs::metadata(&p).map(|m| m.len() == e.size).unwrap_or(false)
                    })
                    .cloned()
                    .collect();
                if missing.is_empty() {
                    continue;
                }
                println!("[masterlist] {} new or missing file(s) — fetching", missing.len());

                let mut ds = state::load_download_state();
                let mut got = 0u64;
                // Ordering here follows the same rule as the opening pass —
                // `config source`, or scope when that is `auto`. This loop is
                // deliberately SERIAL: it runs hourly on top of a node that is
                // already seeding, and a catch-up sweep is not worth competing
                // with the upload the node exists to do. The watchdog in
                // download.rs is what keeps it honest — before 0.2.7 a single
                // crawling source here could eat the entire hour between
                // sweeps and the node would fall further behind every pass.
                let prefer_archive_r = config::prefer_archive(&settings_r, &scope_r);
                for e in missing {
                    if sh.quiet.load(Ordering::Relaxed) {
                        break; // resume next hour; the window matters more
                    }
                    let urls: Vec<String> = e.download_urls_for(prefer_archive_r);
                    if urls.is_empty() {
                        continue;
                    }
                    let dest = config::file_path(&settings_r, &e.name);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).ok();
                    }
                    match download::ensure_file(&cl, &urls, &dest, e.size).await {
                        Ok(download::Outcome::NoSpace) => {
                            if !sh.disk_full.swap(true, Ordering::Relaxed) {
                                eprintln!("[disk] drive full — new downloads paused, seeding continues");
                            }
                            break;
                        }
                        Ok(download::Outcome::Downloaded) | Ok(download::Outcome::Skipped) => {
                            sh.held.fetch_add(1, Ordering::Relaxed);
                            sh.storage_bytes.fetch_add(e.size, Ordering::Relaxed);
                            ds[e.id()] =
                                serde_json::json!({ "downloaded": true, "diskSize": e.size });
                            got += 1;
                            #[cfg(feature = "seed")]
                            if let Some(sd) = &seeder_r {
                                let _ = sd
                                    .seed_file(&dest, &torrents_dir_r, Some(&e.info_hash), false)
                                    .await;
                            }
                        }
                        _ => {}
                    }
                }
                if got > 0 {
                    state::save_download_state(&ds);
                    println!(
                        "[masterlist] added {got} file(s) — now holding {} ({:.1}%)",
                        sh.held.load(Ordering::Relaxed),
                        sh.coverage_pct()
                    );
                }
            }
        });
    }

    println!("[node] steady state — seeding + heartbeat + dashboard running. Ctrl-C to stop.");
    tokio::signal::ctrl_c().await.ok();
    println!("\n[node] shutting down.");
    Ok(())
}

//! Measure the REAL resident-memory cost per seeded torrent.
//!
//! The Pi holds ~25,500 individual torrents (one per sermon) and sits at ~2.7 GB
//! with almost no traffic. That points at fixed per-torrent state rather than
//! peer activity — but "points at" is not a measurement, so this measures it:
//! create N small files, seed each as its own torrent exactly the way the node
//! does, and sample RSS as they are added.
//!
//!   cargo run --release --bin memtest -- 1200
//!
//! Files are sparse, so the disk cost is nil and hashing is fast; what is being
//! measured is the fixed overhead a torrent carries, which is what scales.

use anyhow::Result;
use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    create_torrent, AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerMode,
    ListenerOptions, Session, SessionOptions,
};
use std::io::Write;
use std::path::PathBuf;

/// Resident set size of this process, in bytes, straight from the kernel.
///
/// Returns None rather than 0 when it cannot be read. This matters: when the
/// process runs out of file descriptors, opening /proc/self/statm ALSO fails,
/// and an earlier version returned 0 here — which looked exactly like a real
/// measurement of "no memory used" and silently invalidated a whole run.
fn rss_bytes_checked() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

fn rss_bytes() -> u64 {
    match rss_bytes_checked() {
        Some(v) => v,
        None => {
            eprintln!("\nFATAL: cannot read /proc/self/statm — almost certainly out of\n\
                       file descriptors. Raise the limit and re-run:\n\
                       \n    ulimit -n 1048576 && ./target/release/memtest <N>\n");
            std::process::exit(2);
        }
    }
}

/// Soft limit on open files, from /proc/self/limits.
fn fd_limit() -> u64 {
    std::fs::read_to_string("/proc/self/limits")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("Max open files"))
                .and_then(|l| l.split_whitespace().nth(3).and_then(|v| v.parse().ok()))
        })
        .unwrap_or(0)
}

/// How many file descriptors this process currently holds.
fn fds_open() -> usize {
    std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0)
}

/// The same trackers src/seed.rs announces to.
fn trackers() -> Vec<String> {
    vec![
        "udp://tracker.opentrackr.org:1337/announce".into(),
        "udp://open.demonii.com:1337/announce".into(),
        "udp://tracker.torrent.eu.org:451/announce".into(),
        "udp://exodus.desync.com:6969/announce".into(),
    ]
}

fn mb(b: u64) -> f64 {
    b as f64 / 1_048_576.0
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|v| v.parse().ok()).unwrap_or(1000);
    // The real node runs BOTH of these; the first version of this test had them
    // off, which is exactly why it measured 5.6 KB/torrent while the Pi sat at
    // 2.7 GB. Turn them on to reproduce what the node actually does.
    let use_dht = args.iter().any(|a| a == "--dht");
    let use_trackers = args.iter().any(|a| a == "--trackers");
    // Seconds to keep sampling AFTER every torrent is added. Discovered peers
    // arrive from announces over minutes, so growth shows up here, not during
    // the add loop.
    let watch_s: u64 = args.iter().position(|a| a == "--watch")
        .and_then(|i| args.get(i + 1)).and_then(|v| v.parse().ok()).unwrap_or(0);
    // Roughly a sermon's shape: 12 MB at 2 MiB pieces ≈ 6 pieces.
    let file_size: u64 = std::env::args().nth(2).and_then(|v| v.parse().ok()).unwrap_or(12 * 1024 * 1024);

    let dir = std::env::temp_dir().join("si-memtest");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)?;

    // Checked FIRST: one open file per seeded torrent, so an insufficient limit
    // makes the whole run meaningless. Fail before doing any work.
    let limit = fd_limit();
    if limit < (n as u64) + 256 {
        eprintln!("FATAL: open-file limit is {limit}, too low for {n} torrents.\n\
                   Each seeded torrent holds a file open. Re-run with:\n\
                   \n    ulimit -n 1048576 && ./target/release/memtest {n}\n");
        std::process::exit(1);
    }
    println!("torrents      : {n}");
    println!("dht           : {}", if use_dht { "ON" } else { "off" });
    println!("trackers      : {}", if use_trackers { "ON" } else { "off" });
    println!("open-file limit: {limit}");
    println!("file size     : {:.1} MB (sparse)", mb(file_size));
    println!("baseline RSS  : {:.1} MB\n", mb(rss_bytes()));

    // Sparse files: no real disk use, but a genuine length to hash over.
    let mut paths: Vec<PathBuf> = Vec::with_capacity(n);
    for i in 0..n {
        let p = dir.join(format!("si-memtest-{i:06}.mp3"));
        let f = std::fs::File::create(&p)?;
        f.set_len(file_size)?;
        paths.push(p);
    }
    let after_files = rss_bytes();
    println!("after creating {n} files: {:.1} MB\n", mb(after_files));

    // Same session options and seed path as src/seed.rs, inlined so this test
    // does not require restructuring the crate into a library.
    let session = Session::new_with_opts(
        dir.clone(),
        SessionOptions {
            fastresume: true,
            persistence: None,
            dht: if use_dht { Some(Default::default()) } else { None },
            // This sandbox has no IPv6, and librqbit's UDP tracker client binds
            // [::]. Trackers are irrelevant to per-torrent memory, so switch them
            // off rather than fail to start.
            disable_trackers: !use_trackers,
            disable_local_service_discovery: true,
            listen: Some(ListenerOptions {
                mode: ListenerMode::TcpOnly,
                listen_addr: (std::net::Ipv4Addr::UNSPECIFIED, 0u16).into(),
                enable_upnp_port_forwarding: false,
                ipv4_only: true,
                ..Default::default()
            }),
            peer_limit: Some(8),
            ..Default::default()
        },
    )
    .await?;
    let spawner = BlockingSpawner::new(2);
    let after_session = rss_bytes();
    println!("after session start     : {:.1} MB", mb(after_session));
    println!("  (session fixed cost   : {:.1} MB)\n", mb(after_session - after_files));

    let start = std::time::Instant::now();
    let mut last = after_session;
    let (mut succeeded, mut failures) = (0usize, 0usize);

    println!("{:>8}  {:>10}  {:>12}  {:>12}  {:>7}", "torrents", "RSS (MB)", "Δ since last", "per torrent", "fds");
    println!("{}", "-".repeat(62));

    let step = (n / 8).max(50);
    for (i, p) in paths.iter().enumerate() {
        // create_torrent + add_torrent(overwrite) — exactly what seed.rs does.
        let mut added_ok = false;
        if let Ok(created) = create_torrent(
            p,
            CreateTorrentOptions { name: p.file_name().and_then(|x| x.to_str()), ..Default::default() },
            &spawner,
        ).await {
            if let Ok(bytes) = created.as_bytes() {
                match session.add_torrent(
                    AddTorrent::from_bytes(bytes),
                    Some(AddTorrentOptions {
                        overwrite: true,
                        output_folder: Some(dir.to_string_lossy().to_string()),
                        trackers: if use_trackers { Some(trackers()) } else { None },
                        ..Default::default()
                    }),
                ).await {
                    Ok(_) => added_ok = true,
                    Err(e) => {
                        failures += 1;
                        if failures <= 3 {
                            eprintln!("  add_torrent failed (#{failures}): {e:#}");
                        }
                    }
                }
            }
        }
        if added_ok { succeeded += 1; }
        let done = i + 1;
        // Abort rather than report a number computed from torrents that never
        // actually loaded.
        if failures > 20 {
            eprintln!("\nABORTING: {failures} torrents failed to add — the result would be\n\
                       meaningless. Most likely the open-file limit. Re-run with:\n\
                       \n    ulimit -n 1048576 && ./target/release/memtest {n}\n");
            std::process::exit(3);
        }
        if done % step == 0 || done == n {
            let now = rss_bytes();
            let per = (now.saturating_sub(after_session)) as f64 / done as f64;
            println!(
                "{done:>8}  {:>10.1}  {:>12.1}  {:>10.1} KB  {:>7}",
                mb(now),
                mb(now.saturating_sub(last)),
                per / 1024.0,
                fds_open()
            );
            std::io::stdout().flush().ok();
            last = now;
        }
    }

    let total = rss_bytes();
    println!("\ntorrents added OK    : {succeeded} / {n}   (failures: {failures})");
    if succeeded == 0 {
        eprintln!("Nothing was added — no measurement to report.");
        std::process::exit(4);
    }
    // Divide by what actually loaded, not by what was attempted.
    let per_torrent = (total.saturating_sub(after_session)) as f64 / succeeded as f64;
    println!("\nelapsed              : {:.1}s", start.elapsed().as_secs_f64());
    println!("RSS at {n} torrents : {:.1} MB", mb(total));
    println!("cost per torrent     : {:.1} KB", per_torrent / 1024.0);
    println!(
        "\nEXTRAPOLATION to the Pi's 25,519 torrents:\n  {:.2} GB of per-torrent state (+ session/base)",
        (per_torrent * 25519.0) / 1_073_741_824.0
    );
    println!("  (measured over {succeeded} torrents; fds held: {})", fds_open());

    if watch_s > 0 {
        println!("\nWatching for {watch_s}s — peers discovered via announces land here.");
        println!("{:>8}  {:>10}  {:>14}  {:>7}", "elapsed", "RSS (MB)", "Δ since add", "fds");
        println!("{}", "-".repeat(46));
        let at_add = total;
        let t0 = std::time::Instant::now();
        while t0.elapsed().as_secs() < watch_s {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            let now = rss_bytes();
            println!(
                "{:>7}s  {:>10.1}  {:>14.1}  {:>7}",
                t0.elapsed().as_secs(), mb(now), mb(now.saturating_sub(at_add)), fds_open()
            );
            std::io::stdout().flush().ok();
        }
        let end = rss_bytes();
        let per = (end.saturating_sub(after_session)) as f64 / succeeded as f64;
        println!("\nAFTER WATCH");
        println!("  RSS              : {:.1} MB", mb(end));
        println!("  growth post-add  : {:.1} MB", mb(end.saturating_sub(at_add)));
        println!("  cost per torrent : {:.1} KB", per / 1024.0);
        println!("  → 25,519 torrents: {:.2} GB", (per * 25519.0) / 1_073_741_824.0);
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

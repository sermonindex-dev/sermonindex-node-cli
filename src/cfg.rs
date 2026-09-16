//! `sermonindex-node config …` — every GUI control, from the terminal.
//!
//! WHY THIS EXISTS
//!
//! The desktop app and this binary share `~/.sermonindex/settings.json`, and
//! the CLI has always READ nearly all of it. What it could not do was CHANGE
//! any of it. There were six commands — start, status, quiet, verify, version,
//! help — and `quiet` was the only one that wrote a setting. Everything else
//! meant opening settings.json in vim over SSH and hoping.
//!
//! That is the real parity gap. A desktop user gets a toggle; a headless user
//! gets a text editor and a decent chance of writing malformed JSON into a file
//! the node reads at boot — at which point the node falls back to defaults and
//! they have no idea why.
//!
//! Every write goes through `config::save_settings()` (temp file + atomic
//! rename, unknown keys preserved), so the GUI and the CLI can edit the same
//! file without either clobbering what the other set.
//!
//! A running node picks up quiet hours within a minute. Everything else here
//! takes effect at next start, and the command says so rather than letting
//! someone believe a live node just changed scope.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::config;

pub fn run(args: &[String]) -> Result<()> {
    let key = args.get(2).map(|s| s.as_str()).unwrap_or("");
    let val = args.get(3).map(|s| s.trim()).unwrap_or("");

    if key.is_empty() || key == "show" {
        return show();
    }

    let mut s = config::load_settings();
    let mut restart_note = true;

    match key {
        "scope" => {
            match val {
                "audio" | "full" => s["seed_scope"] = json!(val),
                _ => bail!("scope must be 'audio' (~412 GB) or 'full' (~2.4 TB)"),
            }
        }
        "dir" => {
            if val.is_empty() {
                bail!("give a path, e.g. config dir /mnt/library");
            }
            let p = std::path::PathBuf::from(val);
            if !p.is_dir() {
                bail!("not a directory: {val}");
            }
            // Writability is checked NOW, not at 3am when the download loop
            // first tries. A path that exists but is read-only (a disconnected
            // network share, a mount that came up wrong) is the classic way a
            // node ends up holding nothing and reporting no error.
            let probe = p.join(".sermonindex-write-test");
            if std::fs::write(&probe, b"x").is_err() {
                bail!("cannot write to {val} — check permissions or the mount");
            }
            let _ = std::fs::remove_file(&probe);
            s["storage_dir"] = json!(p.to_string_lossy());
        }
        "upload" => {
            if val.is_empty() {
                bail!("give a KB/s figure, or 'off' for unlimited");
            }
            if val == "off" || val == "0" || val == "unlimited" {
                s["upload_limit_enabled"] = json!(false);
                s["upload_limit_kbps"] = json!(0);
            } else {
                let n: u64 = val.parse().map_err(|_| anyhow::anyhow!("not a number: {val}"))?;
                s["upload_limit_enabled"] = json!(true);
                s["upload_limit_kbps"] = json!(n);
            }
        }
        "monthly-cap" => {
            if val.is_empty() {
                bail!("give a size such as 500GB, or 'off'");
            }
            if val == "off" || val == "0" {
                s["monthly_cap_enabled"] = json!(false);
            } else {
                let gb = parse_size_gb(val)?;
                s["monthly_cap_enabled"] = json!(true);
                s["monthly_cap_gb"] = json!(gb);
            }
        }
        "schedule" => {
            if val == "off" {
                s["seed_schedule_enabled"] = json!(false);
            } else {
                let (a, b) = val
                    .split_once('-')
                    .ok_or_else(|| anyhow::anyhow!("use HH:MM-HH:MM, e.g. 22:00-06:00"))?;
                config::parse_hhmm(a.trim())
                    .ok_or_else(|| anyhow::anyhow!("bad start time: {a}"))?;
                config::parse_hhmm(b.trim())
                    .ok_or_else(|| anyhow::anyhow!("bad end time: {b}"))?;
                s["seed_schedule_enabled"] = json!(true);
                s["seed_start"] = json!(a.trim());
                s["seed_end"] = json!(b.trim());
            }
        }
        "p2p" => s["p2p_enabled"] = json!(on_off(val)?),
        "dht" => s["dht_enabled"] = json!(on_off(val)?),
        "source" => match val {
            "cdn" | "archive" => s["content_mode"] = json!(val),
            _ => bail!("source must be 'cdn' or 'archive'"),
        },
        "peers" => {
            let n: u64 = val.parse().map_err(|_| anyhow::anyhow!("not a number: {val}"))?;
            if !(2..=200).contains(&n) {
                bail!("peers must be between 2 and 200");
            }
            s["peer_limit_per_torrent"] = json!(n);
        }
        "discovered" => {
            let n: u64 = val.parse().map_err(|_| anyhow::anyhow!("not a number: {val}"))?;
            s["max_peers_per_torrent"] = json!(n);
        }
        "window" => {
            let n: u64 = val.parse().map_err(|_| anyhow::anyhow!("not a number: {val}"))?;
            s["active_torrents"] = json!(n);
        }
        "rotate" => {
            let n: u64 = val.parse().map_err(|_| anyhow::anyhow!("not a number: {val}"))?;
            if n == 0 {
                bail!("rotate must be at least 1 minute");
            }
            s["rotate_minutes"] = json!(n);
        }
        "reset" => {
            // Only the tuning knobs. node_id, storage_dir and quiet_hours are
            // deliberately NOT cleared — losing a node_id costs you your place
            // on the map and your seed grant, and no "reset" the user typed
            // meant that.
            for k in [
                "peer_limit_per_torrent", "max_peers_per_torrent",
                "active_torrents", "rotate_minutes", "dht_enabled",
            ] {
                if let Some(o) = s.as_object_mut() {
                    o.remove(k);
                }
            }
            println!("Tuning reset to the defaults for this machine.");
            println!("(node id, storage folder and quiet hours were left alone.)");
            restart_note = false;
        }
        other => bail!("unknown setting '{other}' — run `sermonindex-node config` to see them all"),
    }

    config::save_settings(&s)?;
    println!("Saved.");
    if restart_note {
        println!("Restart the node for this to take effect.");
    }
    show()
}

fn on_off(v: &str) -> Result<bool> {
    match v {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => bail!("expected 'on' or 'off'"),
    }
}

/// "500GB" / "500 gb" / "2TB" / "750" (bare number = GB) → gigabytes.
fn parse_size_gb(v: &str) -> Result<f64> {
    let t = v.trim().to_lowercase().replace(' ', "");
    let (num, mult) = if let Some(n) = t.strip_suffix("tb") {
        (n, 1024.0)
    } else if let Some(n) = t.strip_suffix("gb") {
        (n, 1.0)
    } else if let Some(n) = t.strip_suffix("mb") {
        (n, 1.0 / 1024.0)
    } else {
        (t.as_str(), 1.0)
    };
    let n: f64 = num.parse().map_err(|_| anyhow::anyhow!("bad size: {v}"))?;
    if n <= 0.0 {
        bail!("size must be positive");
    }
    Ok(n * mult)
}

/// Print everything, in the same order the desktop Settings page shows it, so
/// someone can hold the two side by side and compare.
fn show() -> Result<()> {
    let s = config::load_settings();
    let ram = config::total_ram_gb();

    let yn = |b: bool| if b { "on" } else { "off" };
    let auto = |explicit: bool| if explicit { "" } else { "   (auto for this machine)" };

    println!("\nSermonIndex node settings   ({})", config::data_dir().join("settings.json").display());
    println!("{}", "─".repeat(62));

    println!("\n  Peer-to-Peer Network");
    println!("    p2p          {}", yn(config::p2p_enabled(&s)));
    let cap = config::upload_limit_bps(&s);
    println!(
        "    upload       {}",
        match cap {
            Some(bps) => format!("{} KB/s", bps / 1024),
            None => "unlimited".to_string(),
        }
    );

    println!("\n  Seeding Schedule & Limits");
    println!(
        "    schedule     {}",
        if s.get("seed_schedule_enabled").and_then(|v| v.as_bool()).unwrap_or(false) {
            format!(
                "{}-{}",
                s.get("seed_start").and_then(|v| v.as_str()).unwrap_or("?"),
                s.get("seed_end").and_then(|v| v.as_str()).unwrap_or("?")
            )
        } else {
            "off — seed any time".to_string()
        }
    );
    println!(
        "    monthly-cap  {}",
        match config::monthly_cap_bytes(&s) {
            Some(b) => format!("{:.0} GB", b as f64 / 1024.0 / 1024.0 / 1024.0),
            None => "off".to_string(),
        }
    );
    println!("    quiet        {}", config::describe_quiet(&s));

    println!("\n  Content");
    println!("    scope        {}", config::seed_scope(&s));
    println!("    dir          {}", config::downloads_dir(&s).display());
    println!("    source       {}", config::content_mode(&s));

    println!("\n  Tuning   (this machine has {ram:.1} GB RAM)");
    println!(
        "    dht          {}{}",
        yn(config::dht_enabled(&s)),
        auto(s.get("dht_enabled").is_some())
    );
    println!(
        "    peers        {} connections per torrent{}",
        config::peer_limit_per_torrent(&s),
        auto(s.get("peer_limit_per_torrent").is_some())
    );
    println!(
        "    discovered   {} addresses remembered per torrent{}",
        config::max_peers_per_torrent(&s),
        auto(s.get("max_peers_per_torrent").is_some())
    );
    println!(
        "    window       {} torrents live at once{}",
        config::active_torrents(&s),
        auto(s.get("active_torrents").is_some())
    );
    println!(
        "    rotate       every {} minutes{}",
        config::rotate_minutes(&s),
        auto(s.get("rotate_minutes").is_some())
    );

    println!("\n  Identity");
    println!("    node id      {}", s.get("node_id").and_then(|v| v.as_str()).unwrap_or("(not set yet)"));

    println!("\nChange any of these with:  sermonindex-node config <name> <value>");
    println!("  e.g.  config scope full  ·  config upload 2000  ·  config monthly-cap 500GB");
    println!("        config schedule 22:00-06:00  ·  config p2p off  ·  config reset\n");
    Ok(())
}

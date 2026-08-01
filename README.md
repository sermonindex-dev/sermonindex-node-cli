# SermonIndex Node — headless CLI (MVP)

A single, cross-platform command-line program that turns any always-on computer —
a Raspberry Pi, an old laptop, a NAS, a cloud box — into a **SermonIndex seed
node**, with **no GUI required**. It:

- fetches and **ed25519-verifies the signed master list** (the canonical set of
  files in the archive — fail-closed, an unverified list is never used);
- **downloads the audio library** (~400 GB) from the CDN, resumable and
  size-verified against the master list;
- **seeds** what it holds over BitTorrent (librqbit — the same engine as the
  desktop app), so it's a true node, not just a mirror;
- **heartbeats onto the live node map** at `app.sermonindex.net`, so it appears
  alongside every other node exactly like the desktop app;
- serves the **built-in 4-screen dashboard** on `http://<node>:8137/` — the same
  Map / Network / Stats / System view as the standalone kiosk, but built right
  into the node, so any browser or a small touchscreen can display it.

It shares the desktop app's `~/.sermonindex` data directory (settings, catalog,
download-state, downloads), so the two are interchangeable on the same machine.

## Is it one CLI for all platforms?

**One codebase, native binaries per platform** — the same way the desktop app
ships. Rust cross-compiles to:

| Platform | Target | Notes |
|---|---|---|
| Linux x86_64 | `x86_64-unknown-linux-gnu` | servers, old PCs |
| **Raspberry Pi / ARM Linux** | `aarch64-unknown-linux-gnu` | Pi 4 / Pi 5, cheap + cool |
| macOS (Apple Silicon) | `aarch64-apple-darwin` | |
| macOS (Intel) | `x86_64-apple-darwin` | |
| Windows | `x86_64-pc-windows-msvc` | `sermonindex-node.exe` |

There's no runtime to install — it's a self-contained binary. Easiest is to
**build on the target** (like you built the desktop app on the Pi); see below.
The dashboard is embedded in the binary, so the visual is bundled with every
node for anyone who wants it on a screen — and optional for those who don't.

## Quick start

### Raspberry Pi / Linux (build on the machine)

```bash
# from inside this folder (where Cargo.toml is)
bash build-and-install.sh
# storage on a big drive instead of ~/.sermonindex:
STORAGE=/mnt/library bash build-and-install.sh
```

That installs Rust if needed, compiles (release), installs the binary to
`/usr/local/bin`, and registers a **systemd service** that runs at boot and
restarts on its own. Then:

```bash
systemctl status sermonindex-node      # running?
journalctl -u sermonindex-node -f      # live logs
sermonindex-node status                # node id, coverage, paths
# dashboard:  http://localhost:8137/
```

### macOS

```bash
cargo build --release
sudo install -m 0755 target/release/sermonindex-node /usr/local/bin/sermonindex-node
# run at login + keep alive:
cp packaging/launchd/net.sermonindex.node.plist ~/Library/LaunchAgents/
launchctl load -w ~/Library/LaunchAgents/net.sermonindex.node.plist
```

### Windows

```powershell
cargo build --release
# then register .\target\release\sermonindex-node.exe as a startup task —
# see packaging/windows/install-service.md
```

## Usage

```
sermonindex-node [start]            download + seed + heartbeat + dashboard (default)
sermonindex-node status            print node id, coverage, and paths, then exit
sermonindex-node version

start options:
  --scope <audio|full>   what to hold (default: settings.json seed_scope, else audio)
  --dir <path>           storage directory (overrides settings.json storage_dir)
  --no-download          serve dashboard + seed existing files only
  --no-dashboard         don't start the local dashboard server
  --no-heartbeat         don't announce to the network (useful for testing)
```

Configuration lives in `~/.sermonindex/settings.json` (shared with the app):
`storage_dir`, `seed_scope` (`audio`|`full`), `upload_limit_enabled` +
`upload_limit_kbps`, `node_id` (auto-generated on first run).

## Vendored librqbit patch (important when upgrading)

`vendor/librqbit` is librqbit 9.0.0-rc.0 with **one** change: the per-torrent
map of discovered peers is bounded (`max_peers_per_torrent`, default 64).

Upstream inserts every newly-seen peer address and never caps or prunes that
map. With a handful of torrents that is fine. A seed holding ~25,500 of them
accumulates addresses from every tracker and DHT response until the machine
runs out of memory — measured at roughly +0.5 GB/hour on a 4 GB Pi, ending in
a frozen box. Note that librqbit's own `peer_limit` does not help: it caps
*concurrent connections*, not the map of known addresses.

If you bump the librqbit version, re-apply the patch in
`vendor/librqbit/src/torrent_state/live/peers/mod.rs` (search for
`max_discovered_peers`) and re-run:

```bash
cargo build --release --bin memtest
ulimit -n $(ulimit -Hn)
SI_MAX_PEERS_PER_TORRENT=0  ./target/release/memtest 1500 --dht --watch 900   # upstream: climbs
SI_MAX_PEERS_PER_TORRENT=64 ./target/release/memtest 1500 --dht --watch 900   # patched: flattens
```

## Features / build flags

- **Default build** includes seeding (`librqbit`). To build a lighter
  download-and-hold node (no seeding, much faster compile), use
  `cargo build --release --no-default-features`.

## What's verified

Built and tested end-to-end on x86_64 Linux:

- ✅ master list fetch **+ ed25519 signature verified** (33,301 entries)
- ✅ audio scope resolved (25,519 `.mp3` entries)
- ✅ real CDN downloads, resumable, **size-verified** against the signed list
- ✅ dashboard served at `:8137` with live **node + system + network** `/stats`
- ✅ network view pulled from `/api/node/map` + `/api/node/stats`
- ✅ compiles cleanly **with librqbit** (release + debug)

Seeding (the librqbit session) is a faithful port of the desktop app's
`torrent_node.rs` and compiles clean; it could not be *live-run* in the build
sandbox only because that sandbox has no IPv6 (librqbit's UDP tracker client
binds `[::]`). On a normal host — Pi, laptop, server — it binds and seeds like
the desktop app. The node also degrades gracefully: if a seed session can't
start, it keeps downloading and serving the dashboard.

## How it fits the architecture

The desktop app couples the *node* (the worker) and the *display* (a webview) in
one process. This CLI is the **worker on its own** — the correct shape for a 24/7
seed node — and it re-exposes the display as an optional embedded web page any
screen can show. Same network, same data dir, same swarm; just headless, lighter,
and service-managed.

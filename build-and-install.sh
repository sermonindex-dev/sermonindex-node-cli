#!/usr/bin/env bash
# Build the SermonIndex headless node from source and install it as a 24/7
# systemd service. Works on a Raspberry Pi (aarch64) or any Linux box.
#
#   bash build-and-install.sh                 # audio scope, storage in ~/.sermonindex
#   STORAGE=/mnt/library bash build-and-install.sh   # storage on a big drive
#
# Re-runnable. Run from inside the crate directory (where Cargo.toml is).
set -euo pipefail
log(){ printf '\n\033[1;33m== %s ==\033[0m\n' "$*"; }

USER_NAME="$(id -un)"
HOME_DIR="$HOME"
STORAGE="${STORAGE:-}"
SCOPE="${SCOPE:-audio}"
# Honour PREFIX so `PREFIX=$HOME/.local` from install.sh actually lands there
# instead of silently installing to /usr/local.
PREFIX="${PREFIX:-/usr/local}"
# Use sudo only when we are not already root — a minimal NAS/container root shell
# often has no sudo binary at all, and calling it unconditionally fails there.
SUDO=""
if [ "$(id -u)" != "0" ]; then
  if command -v sudo >/dev/null; then SUDO="sudo"; fi
fi

[ -f Cargo.toml ] || { echo "Run this from the crate directory (where Cargo.toml lives)."; exit 1; }

# ── Exec-capable temp dir (many NAS boxes mount /tmp as noexec) ───────────────
# Synology, QNAP, TrueNAS and hardened servers commonly mount /tmp with the
# `noexec` flag. rustup-init, cargo's build scripts, and proc-macro .so files
# ALL need to execute (or mmap PROT_EXEC) out of a temp dir, so a noexec TMPDIR
# breaks the build — the classic symptom is:
#   error: Cannot execute /tmp/tmp.XXXX/rustup-init (likely because of mounting
#          /tmp as noexec).
# Pick a temp dir we can actually execute from: prefer the system one, but fall
# back to a dir under $HOME, which is virtually always exec-capable. Everything
# below (rustup + cargo) inherits it through the exported TMPDIR.
exec_capable() {  # $1 = candidate dir; 0 if a file created there can be executed
  local d="$1" t
  mkdir -p "$d" 2>/dev/null || return 1
  t="$(mktemp "$d/si-exectest.XXXXXX" 2>/dev/null)" || return 1
  printf '#!/bin/sh\nexit 0\n' > "$t" 2>/dev/null || { rm -f "$t"; return 1; }
  chmod +x "$t" 2>/dev/null || { rm -f "$t"; return 1; }
  if "$t" >/dev/null 2>&1; then rm -f "$t"; return 0; fi
  rm -f "$t"; return 1
}
BUILD_TMP=""
for cand in "${TMPDIR:-/tmp}" "$HOME/.cache/sermonindex-build/tmp"; do
  if exec_capable "$cand"; then BUILD_TMP="$cand"; break; fi
done
[ -n "$BUILD_TMP" ] || {
  echo "No exec-capable temp dir found (both ${TMPDIR:-/tmp} and \$HOME look noexec)."
  echo "Set TMPDIR to a partition mounted WITHOUT noexec and re-run, e.g.:"
  echo "  TMPDIR=/volume1/tmp bash build-and-install.sh"
  exit 1
}
if [ "$BUILD_TMP" != "${TMPDIR:-/tmp}" ]; then
  log "Note: ${TMPDIR:-/tmp} is mounted noexec — using $BUILD_TMP for the build"
fi
export TMPDIR="$BUILD_TMP"
# Keep rustup/cargo state on the (exec-capable) home volume explicitly, so a
# preset RUSTUP_HOME/CARGO_HOME pointing at a noexec mount can't reintroduce the
# failure after we worked around TMPDIR.
export RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}"
export CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}"

log "Installing build dependencies"
if command -v apt-get >/dev/null; then
  sudo apt-get update
  sudo apt-get install -y build-essential pkg-config curl git
fi

log "Ensuring Rust (>= 1.85) is installed"
if ! command -v cargo >/dev/null; then
  # rustup-init runs from $TMPDIR, which we set above to an exec-capable path.
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
  . "$CARGO_HOME/env"
fi
[ -f "$CARGO_HOME/env" ] && . "$CARGO_HOME/env"
command -v cargo >/dev/null || {
  echo "Rust did not install. If the error mentioned '/tmp' and 'noexec', re-run with"
  echo "an exec-capable TMPDIR, e.g.:  TMPDIR=\"\$HOME/.sermonindex-tmp\" bash build-and-install.sh"
  exit 1
}
cargo --version

# On low-RAM machines (Pi 4/5 with 2–4 GB) the librqbit compile can OOM; add swap.
if [ "$(free -m | awk '/Swap:/{print $2}')" -lt 3000 ]; then
  log "Adding 4 GB build swap (low RAM detected)"
  if [ ! -e /swapfile-sicli ]; then
    sudo fallocate -l 4G /swapfile-sicli 2>/dev/null || sudo dd if=/dev/zero of=/swapfile-sicli bs=1M count=4096
    sudo chmod 600 /swapfile-sicli; sudo mkswap /swapfile-sicli
  fi
  sudo swapon /swapfile-sicli 2>/dev/null || true
fi

log "Building (release) — this compiles the BitTorrent engine and takes a while"
export CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-2}"
cargo build --release

log "Installing the binary to $PREFIX/bin"
# No sudo needed when installing inside $HOME.
BIN_SUDO="$SUDO"
case "$PREFIX" in "$HOME"*) BIN_SUDO="" ;; esac
$BIN_SUDO mkdir -p "$PREFIX/bin"
$BIN_SUDO install -m 0755 target/release/sermonindex-node "$PREFIX/bin/sermonindex-node"
"$PREFIX/bin/sermonindex-node" version

log "Installing the systemd service"
# Size the memory ceiling from this machine's RAM: leave ~1.2 GB for the OS,
# desktop and browser, and give the rest to the node.
RAM_MB="$(free -m | awk '/Mem:/{print $2}')"
MEM_HIGH="${MEM_HIGH:-$(( RAM_MB > 3000 ? RAM_MB - 1400 : RAM_MB * 55 / 100 ))}M"
MEM_MAX="${MEM_MAX:-$(( RAM_MB > 3000 ? RAM_MB - 1000 : RAM_MB * 70 / 100 ))}M"
log "Memory ceiling for the service: high=${MEM_HIGH} max=${MEM_MAX} (of ${RAM_MB}M total)"

UNIT=/etc/systemd/system/sermonindex-node.service
EXEC="$PREFIX/bin/sermonindex-node start --scope ${SCOPE}"
[ -n "$STORAGE" ] && EXEC="$EXEC --dir ${STORAGE}"
$SUDO tee "$UNIT" >/dev/null <<EOF
[Unit]
Description=SermonIndex seed node (headless)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=${USER_NAME}
Environment=HOME=${HOME_DIR}
ExecStart=${EXEC}
Restart=always
RestartSec=10
LimitNOFILE=1048576
Nice=5
# ── Memory ceiling ──────────────────────────────────────────────────────────
# librqbit applies its peer limit PER TORRENT and has no session-wide cap, so a
# full-scope seed (~25,500 torrents) has no natural upper bound on peer state.
# Without a ceiling the node grows until the machine itself runs out of memory,
# which on a Pi means the desktop and touchscreen lock up — the node takes down
# the whole box rather than just itself.
#
# MemoryHigh throttles and forces reclaim BEFORE anything is killed; MemoryMax is
# the hard stop where only this service is killed, and Restart=always brings it
# straight back. The result is a node that may briefly restart instead of a Pi
# that freezes. Sized for a 4 GB board — raise both on a machine with more RAM.
MemoryHigh=${MEM_HIGH}
MemoryMax=${MEM_MAX}
MemorySwapMax=512M
# ── Daily restart backstop ──────────────────────────────────────────────────
# Peers discovered per torrent accumulate in a map librqbit never bounds or
# prunes, so resident memory creeps even with DHT off. Restarting frees it.
#
# MEASURED on a Pi holding ~25,500 torrents:
#   dht off : ~0.7 GB -> ~1.4 GB over several hours  → daily restart is ample
#   dht on  : roughly +0.5 GB/hour, ceiling in 3-4 h → daily is NOT enough;
#             drop this to 10800 (3 h) if you set "dht_enabled": true
# The cost is a library re-scan on each restart, so keep the interval as long
# as the growth rate allows rather than restarting for its own sake.
RuntimeMaxSec=${RUNTIME_MAX:-86400}

[Install]
WantedBy=multi-user.target
EOF

$SUDO systemctl daemon-reload
$SUDO systemctl enable --now sermonindex-node.service
sleep 2

# ── Optional: touchscreen / monitor display ─────────────────────────────────
# Most nodes are headless, so this only installs when the machine actually has
# a desktop AND a browser — never on a server. Force with KIOSK=yes, skip with
# KIOSK=no.
KIOSK="${KIOSK:-auto}"
BROWSER="$(command -v chromium || command -v chromium-browser || true)"
GRAPHICAL="$(systemctl get-default 2>/dev/null || echo unknown)"
if [ "$KIOSK" = "no" ] || { [ "$KIOSK" = "auto" ] && { [ -z "$BROWSER" ] || [ "$GRAPHICAL" != "graphical.target" ]; }; }; then
  echo ""
  echo "Display: skipped (headless, or no browser found). Install later with:"
  echo "  KIOSK=yes bash build-and-install.sh"
else
  log "Installing the dashboard display (kiosk on this machine's screen)"
  [ -n "$BROWSER" ] || { echo "  ! no chromium found — install it first: sudo apt install -y chromium"; BROWSER="chromium"; }
  mkdir -p "$HOME_DIR/.local/bin" "$HOME_DIR/.config/autostart"
  # Chromium, deliberately: cog renders the page but does not deliver touch
  # events on the Pi's panel, so the on-screen buttons are dead under it.
  cat > "$HOME_DIR/.local/bin/si-kiosk.sh" <<KIOSKEOF
#!/usr/bin/env bash
export DISPLAY=\${DISPLAY:-:0}
URL="http://localhost:${DASH_PORT:-8137}/?view=compact"
PROFILE="\$HOME/.config/si-kiosk-profile"
# Wait for the node to answer, or at boot the page loads before the service is
# up and sits there showing a connection error.
for i in \$(seq 1 60); do curl -fsS -o /dev/null "\$URL" && break; sleep 2; done
while true; do
  # Chromium refuses to reuse a profile it thinks is still open, which is
  # exactly the state left behind by a power cut. Clear the lock each launch.
  rm -f "\$PROFILE"/Singleton* 2>/dev/null
  "$BROWSER" --kiosk --app="\$URL" --user-data-dir="\$PROFILE" \\
    --noerrdialogs --disable-infobars --disable-session-crashed-bubble \\
    --no-first-run --disable-pinch --overscroll-history-navigation=0
  sleep 3
done
KIOSKEOF
  chmod +x "$HOME_DIR/.local/bin/si-kiosk.sh"
  cat > "$HOME_DIR/.config/autostart/si-node-display.desktop" <<DESKTOPEOF
[Desktop Entry]
Type=Application
Name=SermonIndex Node Display
Exec=$HOME_DIR/.local/bin/si-kiosk.sh
X-GNOME-Autostart-enabled=true
DESKTOPEOF
  # Remove any earlier cog-based entry so two browsers can't both launch.
  rm -f "$HOME_DIR/.config/autostart/si-cog-kiosk.desktop" 2>/dev/null
  echo "  display installed — starts on boot, or now with:"
  echo "    nohup setsid $HOME_DIR/.local/bin/si-kiosk.sh >/tmp/kiosk.log 2>&1 &"
fi

log "Done"
cat <<EOF

The node is running as a service and will start on every boot.

  status:     sudo systemctl status sermonindex-node
  logs:       journalctl -u sermonindex-node -f
  dashboard:  http://localhost:8137/   (open in a browser, or point a kiosk at it)
  node info:  sermonindex-node status

It is downloading the ${SCOPE} library now, seeding what it holds, and it will
appear on the live node map within a few minutes. Forward TCP 42800 for the best
peer reachability.
EOF

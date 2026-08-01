#!/usr/bin/env bash
# SermonIndex headless node — one-line installer.
#
#   curl -fsSL https://sermonindex4.b-cdn.net/node-cli/install.sh | bash
#
# Prefer to read it first (a good habit with any piped installer):
#   curl -fsSL https://sermonindex4.b-cdn.net/node-cli/install.sh -o install.sh
#   less install.sh && bash install.sh
#
# What it does:
#   1. Reads the signed release index on the CDN and picks the newest version.
#   2. Downloads the prebuilt binary for this machine if one exists; otherwise
#      falls back to the source tarball and compiles it.
#   3. Verifies the SHA-256 published in that release's manifest BEFORE using
#      anything it downloaded. A mismatch aborts — it never installs unverified
#      bytes, the same fail-closed rule the node applies to the master list.
#   4. Installs to /usr/local/bin and registers the systemd service (Linux).
#
# Options (environment variables):
#   VERSION=v0.1.9     install a specific version instead of the newest
#   STORAGE=/mnt/lib   library directory (otherwise ~/.sermonindex/downloads)
#   SCOPE=full         audio (default, ~400 GB) or full (~2.4 TB)
#   PREFIX=~/.local    install location (default /usr/local)
#   NO_SERVICE=1       install the binary only, don't register a service
#   FROM_SOURCE=1      always compile, even if a binary is available
set -euo pipefail

CDN="${CDN:-https://sermonindex4.b-cdn.net}"
FEED="$CDN/node-cli/releases/releases.json"
PREFIX="${PREFIX:-/usr/local}"
SCOPE="${SCOPE:-audio}"

c_y=$'\033[1;33m'; c_g=$'\033[1;32m'; c_r=$'\033[1;31m'; c_0=$'\033[0m'
log(){ printf '\n%s== %s ==%s\n' "$c_y" "$*" "$c_0"; }
ok(){  printf '%s  ✓%s %s\n' "$c_g" "$c_0" "$*"; }
die(){ printf '\n%s✗ %s%s\n' "$c_r" "$*" "$c_0" >&2; exit 1; }

need(){ command -v "$1" >/dev/null 2>&1; }
for t in curl tar; do need "$t" || die "$t is required but not installed."; done

# sudo only where we actually need it (writing outside $HOME).
SUDO=""
case "$PREFIX" in
  "$HOME"*) : ;;
  *) if [ "$(id -u)" != "0" ]; then need sudo || die "Installing to $PREFIX needs root. Install sudo, run as root, or set PREFIX=\$HOME/.local"; SUDO="sudo"; fi ;;
esac

# ── Platform detection ───────────────────────────────────────────────────────
OS="$(uname -s)"; ARCH="$(uname -m)"
case "$OS" in
  Linux)  os_tag="linux" ;;
  Darwin) os_tag="darwin" ;;
  *) die "Unsupported OS: $OS. Build from source: https://sermonindex.net/node-software/" ;;
esac
case "$ARCH" in
  aarch64|arm64) arch_tag="aarch64" ;;
  x86_64|amd64)  arch_tag="x86_64" ;;
  armv7l|armv6l) die "32-bit ARM is not supported — a 64-bit OS is required on the Pi." ;;
  *) die "Unsupported architecture: $ARCH" ;;
esac
PLATFORM="${arch_tag}-${os_tag}"

log "SermonIndex headless node installer"
echo "  machine  : $OS $ARCH  →  $PLATFORM"
echo "  install  : $PREFIX/bin"

# ── Exec-capable temp dir (many NAS boxes mount /tmp as noexec) ───────────────
# Synology / QNAP / TrueNAS and hardened servers often mount /tmp `noexec`.
# rustup-init, cargo build scripts and proc-macro .so files must execute from a
# temp dir, so a noexec /tmp breaks a source build ("Cannot execute .../rustup-init
# … because of mounting /tmp as noexec"). Also, cargo unpacks and RUNS build
# tooling under the working dir, so the extracted source itself must live on an
# exec-capable filesystem. Choose one now and use it for every temp/build path.
exec_capable() {  # $1 = dir; 0 if a file created there can be executed
  local d="$1" t
  mkdir -p "$d" 2>/dev/null || return 1
  t="$(mktemp "$d/si-exectest.XXXXXX" 2>/dev/null)" || return 1
  printf '#!/bin/sh\nexit 0\n' > "$t" 2>/dev/null || { rm -f "$t"; return 1; }
  chmod +x "$t" 2>/dev/null || { rm -f "$t"; return 1; }
  if "$t" >/dev/null 2>&1; then rm -f "$t"; return 0; fi
  rm -f "$t"; return 1
}
BUILD_TMP=""
for cand in "${TMPDIR:-/tmp}" "$HOME/.cache/sermonindex/tmp"; do
  if exec_capable "$cand"; then BUILD_TMP="$cand"; break; fi
done
[ -n "$BUILD_TMP" ] || die "No exec-capable temp dir found — /tmp appears to be mounted 'noexec'.
  Re-run pointing TMPDIR at a partition without noexec, e.g.:
    curl -fsSL $CDN/node-cli/install.sh -o install.sh
    TMPDIR=\"\$HOME/.sermonindex-tmp\" bash install.sh"
if [ "$BUILD_TMP" != "${TMPDIR:-/tmp}" ]; then
  printf '  (note: %s is noexec — using %s for build steps)\n' "${TMPDIR:-/tmp}" "$BUILD_TMP"
fi
export TMPDIR="$BUILD_TMP"

# ── Resolve the release ──────────────────────────────────────────────────────
WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
curl -fsSL "$FEED?t=$(date +%s)" -o "$WORK/releases.json" \
  || die "Could not read the release index at $FEED"

# Python is used only to read JSON; it ships on every target platform. The
# resolver prefers a binary matching this machine and falls back to source.
read -r VER REL_URL ASSET_NAME ASSET_URL ASSET_SHA KIND <<EOF
$(python3 - "$WORK/releases.json" "$PLATFORM" "${VERSION:-}" "${FROM_SOURCE:-}" <<'PY'
import json, sys
feed, platform, want, from_source = sys.argv[1], sys.argv[2], sys.argv[3], sys.argv[4]
d = json.load(open(feed))
rels = d.get("releases") or []
if not rels: print("ERR ERR ERR ERR ERR ERR"); raise SystemExit
rel = next((r for r in rels if r.get("version") == want), None) if want else rels[0]
if rel is None: print("NOVER ERR ERR ERR ERR ERR"); raise SystemExit
files = rel.get("files") or []
binary = source = None
for f in files:
    n = (f.get("name") or "").lower()
    if n.endswith(".tar.gz"): source = f
    elif platform in n and not n.endswith(".json"): binary = f
pick, kind = (source, "source") if (from_source or not binary) else (binary, "binary")
if not pick: print("NOASSET ERR ERR ERR ERR ERR"); raise SystemExit
print(rel.get("version",""), rel.get("url",""), pick.get("name",""),
      pick.get("url",""), pick.get("sha256","") or "-", kind)
PY
)
EOF

case "$VER" in
  ERR)     die "The release index is empty." ;;
  NOVER)   die "Version ${VERSION:-} not found in the release index." ;;
  NOASSET) die "No downloadable asset for this platform." ;;
esac
ok "release $VER  ($KIND: $ASSET_NAME)"

# ── Download + verify ────────────────────────────────────────────────────────
log "Downloading"
curl -fL --progress-bar "$ASSET_URL" -o "$WORK/$ASSET_NAME" || die "Download failed: $ASSET_URL"

verify(){ # $1 file  $2 expected sha256
  [ "$2" = "-" ] && { printf '  ! no checksum published for this asset — skipping verification\n'; return 0; }
  local actual=""
  if need sha256sum; then actual="$(sha256sum "$1" | awk '{print $1}')"
  elif need shasum;  then actual="$(shasum -a 256 "$1" | awk '{print $1}')"
  else printf '  ! no sha256 tool available — skipping verification\n'; return 0; fi
  [ "$actual" = "$2" ] || die "CHECKSUM MISMATCH — refusing to install.
    expected $2
    actual   $actual
  The download was corrupted or tampered with. Nothing has been installed."
  ok "sha256 verified"
}
verify "$WORK/$ASSET_NAME" "$ASSET_SHA"

# ── Install ──────────────────────────────────────────────────────────────────
if [ "$KIND" = "binary" ]; then
  log "Installing the binary"
  chmod +x "$WORK/$ASSET_NAME"
  $SUDO mkdir -p "$PREFIX/bin"
  $SUDO install -m 0755 "$WORK/$ASSET_NAME" "$PREFIX/bin/sermonindex-node"
  ok "$PREFIX/bin/sermonindex-node"
else
  log "Building from source (this takes a while — it compiles the BitTorrent engine)"
  # Extract to a STABLE, exec-capable directory that SURVIVES a failed build, so
  # a manual retry is one `cd` away. (The old code extracted into the temp dir
  # that the EXIT trap deletes — which is why, after a failure, the pastor's
  # `tar -xzf sermonindex-node-*.tar.gz` in $HOME found "No such file": the tree
  # and the tarball were already gone.)
  # SI_BUILD_DIR lets you build on a big volume — important on a NAS, where $HOME
  # often sits on a small system partition but the storage lives on /volume1.
  SRC_KEEP="${SI_BUILD_DIR:-${HOME:?HOME is not set — pass SI_BUILD_DIR=/path instead}/sermonindex-node-src}"
  mkdir -p "$SRC_KEEP"

  # A release build of the BitTorrent engine needs real room (target/ runs to a
  # couple of GB). Check BEFORE spending an hour compiling into a full disk.
  FREE_MB="$(df -Pm "$SRC_KEEP" 2>/dev/null | awk 'NR==2{print $4}')"
  if [ -n "${FREE_MB:-}" ] && [ "$FREE_MB" -lt 3000 ] 2>/dev/null; then
    printf '  ! only %s MB free on %s — a release build needs ~3 GB.\n' "$FREE_MB" "$SRC_KEEP"
    printf '    Build on a bigger volume:  SI_BUILD_DIR=/volume1/build bash install.sh\n'
  fi

  # Do NOT wipe the directory: a previous target/ is 30-60 minutes of compiling on
  # a NAS or a Pi, and re-running the installer after a failure is exactly when
  # you least want to start from scratch. tar overwrites the source files it owns
  # and leaves target/ alone. Each version extracts under its own top-level dir,
  # so different versions coexist without staleness.
  TOP="$(tar -tzf "$WORK/$ASSET_NAME" 2>/dev/null | head -1 | cut -d/ -f1)"
  tar -xzf "$WORK/$ASSET_NAME" -C "$SRC_KEEP"
  if [ -n "${TOP:-}" ] && [ -f "$SRC_KEEP/$TOP/Cargo.toml" ]; then
    SRC="$SRC_KEEP/$TOP"
  else
    SRC="$(find "$SRC_KEEP" -maxdepth 2 -name Cargo.toml -exec dirname {} \; 2>/dev/null | head -1)"
  fi
  [ -n "$SRC" ] || die "Could not find the crate inside $ASSET_NAME"
  cd "$SRC"
  # build-and-install.sh handles Rust, an exec-capable TMPDIR, build swap on
  # low-RAM boards, the binary, and the service — reuse it rather than
  # duplicating that logic here.
  if [ -f build-and-install.sh ]; then
    if PREFIX="$PREFIX" STORAGE="${STORAGE:-}" SCOPE="$SCOPE" bash build-and-install.sh; then
      ok "source is kept at $SRC (safe to delete once you're happy: rm -rf \"$SRC_KEEP\")"
      log "Done"
      "$PREFIX/bin/sermonindex-node" status 2>/dev/null \
        || sermonindex-node status 2>/dev/null || true
      exit 0
    else
      die "Build failed. The extracted source has been KEPT so you can retry:
    $SRC
  Resume the build with:
    cd \"$SRC\" && bash build-and-install.sh
  If the error mentioned '/tmp' and 'noexec' (common on a NAS), force an
  exec-capable temp dir:
    cd \"$SRC\" && TMPDIR=\"\$HOME/.sermonindex-tmp\" bash build-and-install.sh"
    fi
  fi
  need cargo || die "Rust is required to build from source: https://rustup.rs"
  cargo build --release
  $SUDO install -m 0755 target/release/sermonindex-node "$PREFIX/bin/sermonindex-node"
  ok "$PREFIX/bin/sermonindex-node"
fi

# ── Service (Linux, binary path only — the source path already did it) ───────
if [ "$os_tag" = "linux" ] && [ -z "${NO_SERVICE:-}" ] && need systemctl; then
  log "Registering the systemd service"
  RAM_MB="$(free -m | awk '/Mem:/{print $2}')"
  MEM_HIGH="${MEM_HIGH:-$(( RAM_MB > 3000 ? RAM_MB - 1400 : RAM_MB * 55 / 100 ))}M"
  MEM_MAX="${MEM_MAX:-$(( RAM_MB > 3000 ? RAM_MB - 1000 : RAM_MB * 70 / 100 ))}M"
  EXEC="$PREFIX/bin/sermonindex-node start --scope $SCOPE"
  [ -n "${STORAGE:-}" ] && EXEC="$EXEC --dir $STORAGE"
  $SUDO tee /etc/systemd/system/sermonindex-node.service >/dev/null <<UNIT
[Unit]
Description=SermonIndex seed node (headless)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=$(id -un)
Environment=HOME=$HOME
ExecStart=$EXEC
Restart=always
RestartSec=10
LimitNOFILE=1048576
Nice=5
# Seeding tens of thousands of torrents has no natural memory ceiling, so bound
# the service: it can only ever take itself down, never the whole machine.
MemoryAccounting=yes
MemoryHigh=$MEM_HIGH
MemoryMax=$MEM_MAX
MemorySwapMax=512M

[Install]
WantedBy=multi-user.target
UNIT
  $SUDO systemctl daemon-reload
  $SUDO systemctl enable --now sermonindex-node.service
  ok "service running (memory ceiling ${MEM_HIGH}/${MEM_MAX} of ${RAM_MB}M)"
  if ! grep -qw memory /sys/fs/cgroup/cgroup.controllers 2>/dev/null; then
    printf '\n  ! The kernel memory cgroup controller is OFF, so that ceiling is not\n'
    printf '    enforced. On Raspberry Pi OS add to /boot/firmware/cmdline.txt:\n'
    printf '        cgroup_enable=memory cgroup_memory=1\n'
    printf '    then reboot.\n'
  fi
fi

log "Installed"
cat <<MSG
  sermonindex-node status        node id, scope, paths
  sermonindex-node verify        audit the library against the signed list
  sermonindex-node help          full reference
  http://localhost:8137/         live dashboard

MSG
"$PREFIX/bin/sermonindex-node" version 2>/dev/null || true

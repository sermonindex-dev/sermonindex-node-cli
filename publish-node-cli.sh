#!/usr/bin/env bash
# publish-node-cli.sh — publish a headless-node build to Bunny, so
# /node-software/ links the newest version with no site rebuild.
#
# Normally you do NOT run this by hand: pushing a tag (v0.2.0) makes GitHub
# Actions build the Linux binaries and run this script for you. Running it
# locally is still supported — it just publishes source-only unless dist/ has
# binaries in it.
#
# Artefacts land under
#   /node-cli/releases/v<VER>/     each with a manifest.json (sha256 per file)
# and an index at
#   /node-cli/releases/releases.json   (newest first)
# The page reads that index, then the newest release's manifest, and renders the
# download cards client-side. Publish a new build and the page follows it.
#
# Usage — from the crate directory (where Cargo.toml is):
#   BUNNY_STORAGE_KEY=... ./publish-node-cli.sh --dry-run   # show what would upload
#   BUNNY_STORAGE_KEY=... ./publish-node-cli.sh             # publish
#   VER=0.2.1 ./publish-node-cli.sh                         # override the version
#
# Binaries are OPTIONAL. With none present this still publishes the source
# tarball. CI drops compiled binaries in ./dist/ named so the platform is
# detectable by install.sh's resolver:
#   dist/sermonindex-node-v0.2.0-x86_64-linux
#   dist/sermonindex-node-v0.2.0-aarch64-linux
set -euo pipefail
log(){ printf '\n\033[1;33m== %s ==\033[0m\n' "$*"; }

ZONE="${BUNNY_STORAGE_ZONE:-sermonindex4}"
# REQUIRED — never hardcoded. This is a WRITE key for the storage zone; a copy
# committed to a repo would let anyone overwrite every published release.
# Locally: export it in your shell. In CI: repository secret BUNNY_STORAGE_KEY.
STORAGE_KEY="${BUNNY_STORAGE_KEY:?BUNNY_STORAGE_KEY is not set — export it (or set the repo secret) before publishing}"
STORAGE_HOST="${BUNNY_STORAGE_HOST:-storage.bunnycdn.com}"
CDN="${BUNNY_CDN_BASE:-https://sermonindex4.b-cdn.net}"
PREFIX="node-cli/releases"
DRY=0; [ "${1:-}" = "--dry-run" ] && DRY=1

[ -f Cargo.toml ] || { echo "Run this from the crate directory (where Cargo.toml lives)."; exit 1; }
VER="${VER:-$(awk -F'"' '/^version[[:space:]]*=/{print $2; exit}' Cargo.toml)}"
[ -n "$VER" ] || { echo "Could not read version from Cargo.toml"; exit 1; }
TAG="v${VER#v}"

log "Publishing headless node $TAG to $CDN/$PREFIX/$TAG/"

WORK="$(mktemp -d)"; trap 'rm -rf "$WORK"' EXIT
STAGE="$WORK/stage"; mkdir -p "$STAGE"

# ── 1. Source tarball (always) ───────────────────────────────────────────────
# Built from an explicit WHITELIST copied into a canonically-named directory.
#
# Whitelist, not exclude-list: this archive is world-readable on the CDN, and an
# exclude-list fails open — anything new (a stray key, a .env, an editor backup)
# ships unless someone remembers to exclude it. A whitelist fails closed.
#
# The canonical directory name also means the archive's top-level folder is
# always sermonindex-node-<tag>, regardless of what this checkout happens to be
# called — so the repo folder never has to be renamed at release time.
SRC_NAME="sermonindex-node-$TAG.tar.gz"
PKG="$WORK/pkg/sermonindex-node-$TAG"
mkdir -p "$PKG"
for item in Cargo.toml Cargo.lock README.md LICENSE install.sh build-and-install.sh \
            src assets packaging vendor; do
  [ -e "$item" ] && cp -R "$item" "$PKG/"
done
find "$PKG" -name '.DS_Store' -delete 2>/dev/null || true
tar -czf "$STAGE/$SRC_NAME" -C "$WORK/pkg" "sermonindex-node-$TAG"

# Fail closed: prove no secret is in the archive before anything uploads.
if tar -tzf "$STAGE/$SRC_NAME" | grep -qE 'publish-node-cli\.sh|secrets.*\.json|\.env$|id_[a-z0-9]+$'; then
  echo "ABORT: the source tarball contains a script or file that may carry credentials." >&2
  echo "Nothing has been uploaded." >&2
  exit 1
fi
echo "  source  $SRC_NAME  ($(du -h "$STAGE/$SRC_NAME" | cut -f1))"

# ── 2. Any compiled binaries in dist/ ────────────────────────────────────────
if [ -d dist ] && [ -n "$(ls -A dist 2>/dev/null)" ]; then
  for f in dist/*; do
    [ -f "$f" ] || continue
    cp "$f" "$STAGE/$(basename "$f")"
    echo "  binary  $(basename "$f")  ($(du -h "$f" | cut -f1))"
  done
else
  echo "  (no dist/ — publishing source only; CI is what produces the binaries)"
fi

# ── 3. Build this release's manifest.json ────────────────────────────────────
MANIFEST="$STAGE/manifest.json"
python3 - "$STAGE" "$TAG" "$CDN/$PREFIX/$TAG" "$MANIFEST" <<'PY'
import json, os, sys, datetime, hashlib
stage, tag, base, out = sys.argv[1:5]
def human(b):
    u=['B','KB','MB','GB']; i=0; b=float(b)
    while b>=1024 and i<len(u)-1: b/=1024; i+=1
    return f"{b:.1f} {u[i]}" if (b<10 and i>0) else f"{b:.0f} {u[i]}"
def sha256(path):
    h=hashlib.sha256()
    with open(path,'rb') as f:
        for chunk in iter(lambda: f.read(1<<20), b''): h.update(chunk)
    return h.hexdigest()
files=[]
for n in sorted(os.listdir(stage)):
    if n == "manifest.json": continue
    p=os.path.join(stage,n); sz=os.path.getsize(p)
    # sha256 is what install.sh checks BEFORE installing anything, so every
    # published asset must carry one.
    files.append({"name":n,"url":f"{base}/{n}","bytes":sz,"size":human(sz),
                  "sha256":sha256(p)})
json.dump({"version":tag,
           "date":datetime.date.today().isoformat(),
           "files":files}, open(out,"w"), indent=2)
print(f"  manifest: {len(files)} file(s)")
PY

put(){ # $1 local  $2 remote-path  $3 content-type
  if [ "$DRY" = "1" ]; then echo "  [dry-run] PUT $2"; return; fi
  echo "  ↑ $2"
  curl -fsS -X PUT "https://$STORAGE_HOST/$ZONE/$2" \
    -H "AccessKey: $STORAGE_KEY" -H "Content-Type: $3" \
    --data-binary @"$1" >/dev/null
}

log "Uploading release files"
for f in "$STAGE"/*; do
  n="$(basename "$f")"
  case "$n" in
    *.json) ct="application/json" ;;
    *.tar.gz) ct="application/gzip" ;;
    *) ct="application/octet-stream" ;;
  esac
  put "$f" "$PREFIX/$TAG/$n" "$ct"
done

# The installer lives at a stable URL (not per-version) so the documented
# one-liner never changes. Re-uploaded each publish so it stays current.
if [ -f install.sh ]; then
  log "Publishing the installer"
  put install.sh "node-cli/install.sh" "text/x-shellscript"
fi

# ── 4. Rebuild releases.json (newest first, this version deduped in) ─────────
# Fetched from the CDN rather than kept locally, so publishing from a different
# machine (or from CI) can't silently drop earlier releases.
log "Updating the release index"
INDEX="$WORK/releases.json"
curl -fsS "$CDN/$PREFIX/releases.json?t=$(date +%s)" -o "$WORK/existing.json" 2>/dev/null \
  || echo '{"releases":[]}' > "$WORK/existing.json"

python3 - "$WORK/existing.json" "$MANIFEST" "$TAG" "$CDN/$PREFIX/$TAG/" "$INDEX" <<'PY'
import json, sys
existing, manifest, tag, url, out = sys.argv[1:6]
try:
    idx=json.load(open(existing))
    if not isinstance(idx, dict) or "releases" not in idx: raise ValueError
except Exception:
    idx={"releases":[]}
m=json.load(open(manifest))
entry={"version":tag,"date":m["date"],"url":url,"files":m["files"]}
# Replace any same-version entry rather than duplicating it (re-publishing a
# version must be idempotent).
rest=[r for r in idx.get("releases",[]) if r.get("version")!=tag]
def key(r):
    # Sort newest-first by numeric version parts, not string order, so
    # v0.1.10 correctly outranks v0.1.9.
    return [int(x) if x.isdigit() else 0
            for x in str(r.get("version","0")).lstrip("v").split(".")]
idx["releases"]=sorted([entry]+rest, key=key, reverse=True)
json.dump(idx, open(out,"w"), indent=2)
print(f"  index now lists {len(idx['releases'])} release(s); newest {idx['releases'][0]['version']}")
PY

put "$INDEX" "$PREFIX/releases.json" "application/json"

# ── 5. Purge so the page sees it immediately ────────────────────────────────
API_KEY="${BUNNY_API_KEY:-}"
PURGE=("$CDN/$PREFIX/releases.json" "$CDN/node-cli/install.sh" "https://sermonindex.net/node-software/")
if [ -n "$API_KEY" ] && [ "$DRY" = "0" ]; then
  log "Purging CDN"
  for u in "${PURGE[@]}"; do
    enc="$(python3 -c "import urllib.parse,sys;print(urllib.parse.quote(sys.argv[1],safe=''))" "$u")"
    curl -fsS -X POST "https://api.bunny.net/purge?url=$enc&async=false" -H "AccessKey: $API_KEY" >/dev/null \
      && echo "  ✓ purged $u"
  done
else
  echo ""
  echo "⚠ No BUNNY_API_KEY (or dry run) — purge these in the Bunny dashboard:"
  for u in "${PURGE[@]}"; do echo "   $u"; done
fi

log "Done"
echo "Release  : $CDN/$PREFIX/$TAG/"
echo "Index    : $CDN/$PREFIX/releases.json"
echo "Live page: https://sermonindex.net/node-software/  (headless section)"

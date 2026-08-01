# Releasing the headless node CLI

Distribution is the same shape as the desktop app: **GitHub builds, Bunny
distributes.** Users never touch GitHub — they install from
`https://sermonindex4.b-cdn.net/node-cli/install.sh` and the
`/node-software/` page reads the release index off the CDN.

## Cutting a release

```bash
# 1. bump the version (this is the single source of truth)
vim Cargo.toml                 # version = "0.2.1"
cargo build --release          # refreshes Cargo.lock, proves it compiles

# 2. commit and tag
git commit -am "0.2.1 — <what changed>"
git tag v0.2.1
git push origin main --tags
```

Pushing the tag runs `.github/workflows/release.yml`, which:

1. builds **static musl** binaries for `x86_64-linux` and `aarch64-linux`,
2. fails the build if either binary is not actually static (a glibc-linked
   binary installs fine and then dies with `GLIBC_2.34 not found` on an older
   NAS — a worse failure than shipping nothing),
3. runs `publish-node-cli.sh`, which uploads the binaries, the source tarball
   and `install.sh` to Bunny, rewrites `releases.json`, and purges the edge.

Watch it at **Actions → Build & Publish CLI**. Nothing else is needed; the
download cards on `/node-software/` update themselves.

## Verify after a release

```bash
curl -fsS "https://sermonindex4.b-cdn.net/node-cli/releases/releases.json?t=$(date +%s)" \
  | python3 -m json.tool | head -30

cd /tmp && curl -fsSL https://sermonindex4.b-cdn.net/node-cli/install.sh -o si.sh
NO_SERVICE=1 PREFIX=/tmp/sitest bash si.sh && /tmp/sitest/bin/sermonindex-node version
```

## Required repository secrets

| Secret | What it is |
|---|---|
| `BUNNY_STORAGE_KEY` | **Write** key for the `sermonindex4` storage zone. |
| `BUNNY_API_KEY` | Bunny account key, used only to purge the edge after upload. |

`publish-node-cli.sh` refuses to run without `BUNNY_STORAGE_KEY` — it is never
hardcoded. A copy of that key in a repo would let anyone overwrite every
published release.

## Versioning

The CLI and the desktop app version **independently**. They are separate
artifacts on separate cadences (the app is past 0.0.330; the CLI is at 0.2.x),
and the app's auto-updater pushes a download to every user on each bump — so
tying them together would mean shipping no-op app updates just because the CLI
changed.

What *does* need to stay in step is the **shared settings file**
(`~/.sermonindex/settings.json`, read by both). When either side learns a new
key, the reader must ship first. Current contract:

| Setting | Understood by |
|---|---|
| `quiet_hours` with `days` | CLI ≥ 0.1.14 |
| `quiet_hours` with `dates` (one-off) | CLI ≥ 0.2.1 — **not yet released** |

> Do not let the desktop app write date-based quiet hours until a CLI that
> understands them is out. Older CLIs treat an entry with no `days` field as
> *every day*, so a one-off Christmas Eve window would silence the node daily,
> year-round, with nothing appearing broken.

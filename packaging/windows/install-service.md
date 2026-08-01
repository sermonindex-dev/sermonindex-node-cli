# Running the SermonIndex node as a Windows background service

The node is a single `sermonindex-node.exe`. Two easy ways to keep it running
24/7 on Windows:

## Option A — Task Scheduler (built in, no extra tools)

1. Put `sermonindex-node.exe` somewhere permanent, e.g. `C:\SermonIndex\sermonindex-node.exe`.
2. Open **Task Scheduler → Create Task**.
   - General: "Run whether user is logged on or not", "Run with highest privileges".
   - Triggers: **At startup**.
   - Actions: Start a program → `C:\SermonIndex\sermonindex-node.exe`, arguments `start --scope audio`.
   - Settings: "If the task fails, restart every 1 minute".
3. Save. The node starts at boot and restarts if it stops.

To point storage at another drive, add `--dir D:\SermonIndexLibrary` to the arguments.

## Option B — NSSM (nicer service management)

1. Download NSSM (https://nssm.cc), then in an admin PowerShell:
   ```
   nssm install SermonIndexNode "C:\SermonIndex\sermonindex-node.exe" "start --scope audio"
   nssm set SermonIndexNode AppDirectory "C:\SermonIndex"
   nssm start SermonIndexNode
   ```
2. Manage with `nssm stop/start/restart SermonIndexNode`.

## Firewall / reachability

Allow inbound **TCP 42800** (or the range 42800–42839) so peers can reach you —
Windows Defender Firewall → Inbound Rules → New Rule → Port → TCP 42800.

The dashboard is at http://localhost:8137/ — open it in any browser, or point a
kiosk/second screen at it.

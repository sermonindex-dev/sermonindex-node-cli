//! Resumable, size-verified HTTP download of a single file from its webseeds —
//! Streams to `<file>.part`, resumes with a Range header, and only renames into
//! place when the on-disk size exactly matches the signed master-list size.
//!
//! ## 0.2.7 — the throughput watchdog
//!
//! Until 0.2.7 this module had no notion of *time*. It walked its source list
//! on an error or a size mismatch and nothing else, so a connection that opened
//! cleanly and then trickled at 20 KB/s was, as far as the code was concerned,
//! working — and it held its download slot for hours while a 13 MB/s copy of
//! the very same file sat unused at position two in the list. Operators saw
//! that as a hang. It was not a hang; it was a transfer nobody was timing.
//!
//! Every chunk is now timed. A transfer that goes silent, or that sustains a
//! rate below the floor once it is past the grace allowance, is abandoned and
//! the next source is tried IMMEDIATELY. Nothing is thrown away when that
//! happens: the `.part` file stays exactly where it is and the next source
//! picks it up with a Range request, because the two copies are byte-identical
//! (the Archive copy was streamed from the CDN in the first place).

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

/// Is this a whole file of `expected` bytes — not merely the right length?
///
/// Length alone was the test everywhere until 0.3.4, and it has a hole. A
/// fetch from the swarm creates its file at FULL length before a byte arrives
/// (the torrent engine sizes the file up front, sparsely), and when that fetch
/// gave up the file was left behind: right length, contents mostly empty. The
/// HTTP fallback then saw the right length and skipped it. The node counted it
/// held, could never seed it (every piece fails its hash), and never fetched it
/// again.
///
/// Those files are sparse — the gaps take no disk space — so the space a file
/// actually occupies tells a whole file from a hollow one at no extra cost:
/// it is in the same `stat` we already make. A real download is fully written;
/// a hollow one occupies far less than its length.
///
/// Two guards against false alarms: files under 1 MB are not judged (block
/// rounding), and a filesystem that does not report allocation at all (some
/// network shares) is detected once at startup by `probe_allocation` and then
/// trusted on length, as before — never treated as one big pile of holes.
pub fn looks_complete(meta: &std::fs::Metadata, expected: u64) -> bool {
    meta.len() == expected && !is_hollow(meta)
}

/// `looks_complete` for a path that may not exist.
pub fn looks_complete_path(p: &Path, expected: u64) -> bool {
    std::fs::metadata(p).map(|m| looks_complete(&m, expected)).unwrap_or(false)
}

static HOLLOW_CHECK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Does the library's filesystem report how much space a file occupies? Write
/// 2 MB of real data, sync it, and look. If it claims to occupy nothing, this
/// filesystem cannot tell a hollow file from a whole one, so stop asking it —
/// otherwise every file would look hollow and the node would fetch the whole
/// library again.
pub fn probe_allocation(dir: &Path) {
    use std::io::Write;
    let probe = dir.join(".sermonindex-alloc-probe");
    let ok = (|| -> std::io::Result<bool> {
        std::fs::create_dir_all(dir)?;
        let mut f = std::fs::File::create(&probe)?;
        f.write_all(&vec![0xA5u8; 2 * 1024 * 1024])?;
        f.sync_all()?;
        let m = f.metadata()?;
        Ok(!is_hollow_raw(&m))
    })();
    let _ = std::fs::remove_file(&probe);
    if let Ok(false) = ok {
        HOLLOW_CHECK.store(false, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "[download] this filesystem does not report file allocation — files are checked by size only"
        );
    }
}

/// See `looks_complete`.
pub fn is_hollow(meta: &std::fs::Metadata) -> bool {
    HOLLOW_CHECK.load(std::sync::atomic::Ordering::Relaxed) && is_hollow_raw(meta)
}

#[cfg(unix)]
fn is_hollow_raw(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    let len = meta.len();
    if len < 1024 * 1024 {
        return false;
    }
    meta.blocks().saturating_mul(512) < len / 100 * 95
}
#[cfg(not(unix))]
fn is_hollow_raw(_meta: &std::fs::Metadata) -> bool {
    false
}

/// `looks_complete` for a path.
pub fn file_ok(path: &Path, expected: u64) -> bool {
    std::fs::metadata(path).map(|m| looks_complete(&m, expected)).unwrap_or(false)
}

/// Outcome of attempting one file.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// The disk is full. Distinct from a plain failure because the caller must
    /// STOP rather than try the next file: without this, a full drive produced
    /// one failure line per remaining file — tens of thousands of them — and
    /// the one line that mattered scrolled away in seconds.
    NoSpace,
    Skipped,     // already present and correct size
    Downloaded,  // fetched fresh (or resumed) successfully
    Failed,      // gave up after retries
}

/// How patient to be with a single transfer before trying the next source.
///
/// These are deliberately generous. The watchdog exists to catch a source that
/// has effectively stopped, not to police a modest connection: a node on rural
/// DSL must still be able to finish a file. 50 KB/s sustained is roughly a
/// twentieth of what the slowest healthy mirror delivers.
#[derive(Debug, Clone, Copy)]
pub struct Patience {
    /// No bytes at all for this long → abandon the source.
    pub stall: Duration,
    /// Sustained below `min_bps` for this long → abandon the source.
    pub slow_for: Duration,
    /// The floor, in bytes per second.
    pub min_bps: u64,
    /// Do not judge speed until this many bytes have arrived on this attempt.
    /// Small files and slow starts are not evidence of a bad source.
    pub grace_bytes: u64,
}

impl Default for Patience {
    fn default() -> Self {
        Self {
            stall: Duration::from_secs(60),
            slow_for: Duration::from_secs(30),
            min_bps: 50 * 1024,
            grace_bytes: 1024 * 1024,
        }
    }
}

/// How one attempt against one source ended.
#[derive(Debug, PartialEq)]
enum Try {
    /// Complete and verified; renamed into place.
    Done,
    /// Finished, but the wrong size. The `.part` has been discarded.
    Mismatch,
    /// Abandoned by the watchdog. The `.part` is INTACT and resumable.
    TooSlow { bps: u64 },
}

/// Is this URL served by Archive.org? The caller uses this to cap how many
/// download slots may sit on Archive.org at once — see `main.rs`. Archive is a
/// donated public good and the slowest of our sources under load; letting it
/// occupy every worker is how one bad afternoon there becomes our outage.
pub fn is_archive(url: &str) -> bool {
    let u = url.to_ascii_lowercase();
    u.contains("archive.org")
}

/// Ensure `dest` holds the file described by (`urls`, `expected_size`).
/// Resumable, size-verified, and timed.
///
/// `urls` is tried in order and then round-robin. A mirror that answers 404/403
/// is dropped for the rest of this file rather than retried; a mirror the
/// watchdog judges too slow is set aside for this file but given one more
/// chance if every other source is also exhausted (the network may simply have
/// been bad for a moment, and a second-rate source beats no source). There is
/// no pause before falling to the next source — the point of a fallback is that
/// it happens now. The pause is between full rounds, so a genuinely flaky
/// network still backs off.
/// Bytes the node must leave free on the library drive (see
/// config::keep_free_bytes). Zero until the daemon sets it, so tests and
/// one-off commands are never refused on a nearly-full scratch disk.
static KEEP_FREE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn set_keep_free(bytes: u64) {
    KEEP_FREE.store(bytes, std::sync::atomic::Ordering::Relaxed);
}

pub fn keep_free() -> u64 {
    KEEP_FREE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Is there room for `size` more bytes at `dest` and still the reserve left
/// over? A filesystem that reports nothing (0 total) is given the benefit of
/// the doubt; the write itself still stops at a truly full disk.
pub fn room_for(dest: &Path, size: u64) -> bool {
    let reserve = keep_free();
    if reserve == 0 {
        return true;
    }
    let (free, total) = crate::system::disk_space(dest);
    fits(free, total, size, reserve)
}

fn fits(free: u64, total: u64, size: u64, reserve: u64) -> bool {
    reserve == 0 || total == 0 || free >= size.saturating_add(reserve)
}

#[cfg(test)]
mod room_tests {
    #[test]
    fn keeps_the_reserve_free() {
        const G: u64 = 1 << 30;
        assert!(super::fits(20 * G, 100 * G, G, 10 * G));
        assert!(!super::fits(10 * G + G / 2, 100 * G, G, 10 * G), "would dip into the reserve");
        assert!(super::fits(G, 100 * G, 5 * G, 0), "0 = fill the drive (the write itself stops at full)");
        assert!(super::fits(0, 0, G, 10 * G), "a filesystem that reports nothing is not refused");
    }
}

pub async fn ensure_file(
    client: &reqwest::Client,
    urls: &[String],
    dest: &Path,
    expected_size: u64,
) -> Result<Outcome> {
    ensure_file_with(client, urls, dest, expected_size, Patience::default()).await
}

/// `ensure_file` with the watchdog thresholds spelled out. Kept separate so the
/// defaults live in exactly one place and tests can shorten them.
pub async fn ensure_file_with(
    client: &reqwest::Client,
    urls: &[String],
    dest: &Path,
    expected_size: u64,
    patience: Patience,
) -> Result<Outcome> {
    if urls.is_empty() {
        return Ok(Outcome::Failed);
    }
    // Already complete? Length AND contents — a hollow file left by a swarm
    // fetch that gave up is the right length and nearly empty (looks_complete).
    if let Ok(meta) = tokio::fs::metadata(dest).await {
        if looks_complete(&meta, expected_size) {
            return Ok(Outcome::Skipped);
        }
        if meta.len() == expected_size {
            // Not removed here: the download below lands in `.part` and is
            // renamed over it, so even a misjudged file is only replaced by
            // the same bytes, never lost.
            eprintln!(
                "[download] {} is the right size but mostly empty (an unfinished peer \
                 download) — fetching it again",
                dest.display()
            );
        }
    }
    if !room_for(dest, expected_size) {
        return Ok(Outcome::NoSpace);
    }
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await.ok();
    }
    let part = dest.with_extension(format!(
        "{}part",
        dest.extension()
            .and_then(|e| e.to_str())
            .map(|e| format!("{e}."))
            .unwrap_or_default()
    ));

    // `dead` is permanent for this file (404/403 — it will say the same thing
    // every time). `slow` is provisional: cleared for one more round if every
    // source ends up set aside, because a slow source still finishes and no
    // source does not.
    let mut dead = vec![false; urls.len()];
    let mut slow = vec![false; urls.len()];
    let mut second_chance_used = false;
    let mut last_err = String::new();
    let max_attempts = (5 * urls.len()).min(12);

    for attempt in 0..max_attempts {
        let i = attempt % urls.len();
        let round = attempt / urls.len();
        if !dead[i] && !slow[i] {
            match try_once(client, &urls[i], &part, dest, expected_size, patience).await {
                Ok(Try::Done) => return Ok(Outcome::Downloaded),
                Ok(Try::Mismatch) => { /* try the next source */ }
                Ok(Try::TooSlow { bps }) => {
                    // The .part is intact — the next source resumes from it.
                    eprintln!(
                        "[download] {} — source {} too slow ({} KB/s), switching",
                        dest.display(),
                        short_host(&urls[i]),
                        bps / 1024
                    );
                    slow[i] = true;
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    // A full disk is not a source problem and no other source
                    // will fix it. Report it as its own outcome so the caller
                    // can stop the whole pass — retrying every remaining file
                    // against every mirror just buries the one line that says
                    // what is actually wrong.
                    let low = msg.to_lowercase();
                    if low.contains("no space left")
                        || low.contains("os error 28")
                        || low.contains("disk full")
                        || low.contains("quota exceeded")
                    {
                        eprintln!("[download] DISK FULL while writing {}", dest.display());
                        return Ok(Outcome::NoSpace);
                    }
                    // A 404 or 403 will say the same thing every time. Retrying
                    // it just delays the source that would have worked.
                    if msg.contains("permanent HTTP") {
                        dead[i] = true;
                    }
                    last_err = msg;
                }
            }
        }
        // Everything set aside? If any of it was merely slow, un-set-aside it
        // once and let the best of a bad set of options finish the file.
        if dead.iter().zip(slow.iter()).all(|(d, s)| *d || *s) {
            if !second_chance_used && slow.iter().any(|s| *s) {
                second_chance_used = true;
                for s in slow.iter_mut() {
                    *s = false;
                }
                eprintln!(
                    "[download] {} — every source was slow; retrying the list once more",
                    dest.display()
                );
            } else {
                break;
            }
        }
        // Only pause once every source has been tried this round.
        if i == urls.len() - 1 {
            let secs = (1u64 << round.min(3)).min(8);
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }
    }
    if !last_err.is_empty() {
        eprintln!("[download] {} failed: {last_err}", dest.display());
    }
    Ok(Outcome::Failed)
}

/// Just the host, for a log line that has to fit on one terminal row.
fn short_host(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or(url)
        .to_string()
}

async fn try_once(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    dest: &Path,
    expected_size: u64,
    patience: Patience,
) -> Result<Try> {
    // Resume from however much of .part we already have.
    let have = tokio::fs::metadata(part).await.map(|m| m.len()).unwrap_or(0);
    let mut req = client.get(url);
    if have > 0 && have < expected_size {
        req = req.header("Range", format!("bytes={have}-"));
    }
    let resp = req.send().await.context("send request")?;
    let status = resp.status();
    if !status.is_success() && status.as_u16() != 206 {
        // Permanent client errors: don't hammer.
        if matches!(status.as_u16(), 400 | 401 | 403 | 404 | 410 | 451) {
            bail!("permanent HTTP {status}");
        }
        bail!("HTTP {status}");
    }

    let mut file = if status.as_u16() == 206 && have > 0 {
        // Server honored the range; append.
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(part)
            .await
            .context("open .part append")?
    } else {
        // Full body; start fresh.
        tokio::fs::File::create(part).await.context("create .part")?
    };

    // ── the watchdog ────────────────────────────────────────────────────────
    // Two independent tests, because they catch different failures:
    //
    //   stall — no chunk arrives within `patience.stall`. This is a connection
    //           that is open but dead; without the timeout the await simply
    //           never returns and the slot is held forever.
    //   slow  — chunks keep arriving, but the rate measured over rolling
    //           windows stays under the floor for `patience.slow_for`. This is
    //           the case that was costing whole nights: perfectly healthy
    //           protocol behaviour, uselessly slow.
    //
    // The rate is measured over WINDOWS rather than instantaneously, so one
    // slow chunk on an otherwise fine transfer proves nothing and a burst
    // cannot mask a sustained crawl.
    const WINDOW: Duration = Duration::from_secs(5);
    let mut got_this_attempt: u64 = 0;
    let mut window_start = Instant::now();
    let mut window_bytes: u64 = 0;
    let mut slow_for = Duration::ZERO;
    let mut last_rate: u64 = 0;

    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::time::timeout(patience.stall, stream.next()).await;
        let chunk = match next {
            // The source went silent. Keep the .part; the caller moves on.
            Err(_elapsed) => {
                file.flush().await.ok();
                return Ok(Try::TooSlow { bps: 0 });
            }
            Ok(None) => break, // body complete
            Ok(Some(c)) => c.context("stream chunk")?,
        };
        file.write_all(&chunk).await.context("write chunk")?;
        got_this_attempt += chunk.len() as u64;
        window_bytes += chunk.len() as u64;

        let elapsed = window_start.elapsed();
        if elapsed >= WINDOW {
            let bps = (window_bytes as f64 / elapsed.as_secs_f64()) as u64;
            last_rate = bps;
            // Below the floor only counts once enough has arrived that the
            // measurement means something.
            if got_this_attempt >= patience.grace_bytes && bps < patience.min_bps {
                slow_for += elapsed;
                if slow_for >= patience.slow_for {
                    file.flush().await.ok();
                    return Ok(Try::TooSlow { bps });
                }
            } else {
                slow_for = Duration::ZERO;
            }
            window_start = Instant::now();
            window_bytes = 0;
        }
    }
    let _ = last_rate;
    file.flush().await.ok();
    drop(file);

    let got = tokio::fs::metadata(part).await?.len();
    if got != expected_size {
        // Wrong size — discard and let the caller retry from scratch. This is
        // NOT the slow path: a truncated or wrong file is worse than no file,
        // and resuming from it would only propagate the corruption.
        tokio::fs::remove_file(part).await.ok();
        return Ok(Try::Mismatch);
    }
    // Make it durable, then let go of it. Synced before the rename so a power
    // cut can never leave a full-length file whose tail was never written —
    // the size check would have taken that for a whole file. And dropped from
    // the page cache afterwards: a node writing hundreds of gigabytes was
    // filling RAM with pages it would never read again, which is the bulk of
    // the "Memory:" figure systemctl showed sitting at the service's ceiling,
    // and on a 4 GB machine also running a desktop and a kiosk browser it is
    // memory those needed.
    let p = part.to_path_buf();
    let _ = tokio::task::spawn_blocking(move || settle(&p)).await;
    tokio::fs::rename(part, dest).await.context("rename into place")?;
    Ok(Try::Done)
}

/// fsync, then (Linux) tell the kernel these pages will not be read again.
fn settle(path: &Path) {
    if let Ok(f) = std::fs::File::open(path) {
        let _ = f.sync_data();
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: a plain advisory syscall on a descriptor we own; it
            // cannot affect memory safety whatever it returns.
            unsafe {
                libc::posix_fadvise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
            }
        }
    }
}

#[cfg(all(test, unix))]
mod hollow_tests {
    use super::*;
    use std::io::Write;

    /// The exact shape a failed swarm fetch left behind: the right length,
    /// written as a sparse file with almost nothing in it.
    #[test]
    fn a_sparse_file_of_the_right_length_is_not_complete() {
        let dir = std::env::temp_dir().join(format!("si-hollow-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hollow = dir.join("hollow.mp3");
        let f = std::fs::File::create(&hollow).unwrap();
        f.set_len(8 * 1024 * 1024).unwrap(); // what the torrent engine does
        drop(f);
        let full = dir.join("full.mp3");
        let mut f = std::fs::File::create(&full).unwrap();
        f.write_all(&vec![7u8; 8 * 1024 * 1024]).unwrap();
        f.sync_all().unwrap();
        drop(f);

        probe_allocation(&dir);
        assert!(!file_ok(&hollow, 8 * 1024 * 1024), "a hollow file must not count as held");
        assert!(file_ok(&full, 8 * 1024 * 1024), "a fully written file must count as held");
        assert!(!file_ok(&full, 8 * 1024 * 1024 + 1), "wrong length is still wrong");
        let _ = std::fs::remove_dir_all(&dir);
    }
}

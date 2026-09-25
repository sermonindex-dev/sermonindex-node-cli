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
    // Already complete?
    if let Ok(meta) = tokio::fs::metadata(dest).await {
        if meta.len() == expected_size {
            return Ok(Outcome::Skipped);
        }
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
    tokio::fs::rename(part, dest).await.context("rename into place")?;
    Ok(Try::Done)
}

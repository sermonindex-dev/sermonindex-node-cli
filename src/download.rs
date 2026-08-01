//! Resumable, size-verified HTTP download of a single file from its CDN webseed.
//! Streams to `<file>.part`, resumes with a Range header, and only renames into
//! place when the on-disk size exactly matches the signed master-list size.

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use std::path::Path;
use tokio::io::AsyncWriteExt;

/// Outcome of attempting one file.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    Skipped,     // already present and correct size
    Downloaded,  // fetched fresh (or resumed) successfully
    Failed,      // gave up after retries
}

/// Ensure `dest` holds the file described by (url, expected_size). Resumable and
/// verified. Retries transient errors with exponential backoff.
pub async fn ensure_file(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    expected_size: u64,
) -> Result<Outcome> {
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

    let mut last_err = String::new();
    for attempt in 0..5u32 {
        match try_once(client, url, &part, dest, expected_size).await {
            Ok(true) => return Ok(Outcome::Downloaded),
            Ok(false) => { /* size mismatch, retry fresh */ }
            Err(e) => last_err = format!("{e:#}"),
        }
        // backoff (capped low so a bad file doesn't hold a slot long)
        let secs = (2u64.pow(attempt)).min(8);
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    }
    if !last_err.is_empty() {
        eprintln!("[download] {} failed: {last_err}", dest.display());
    }
    Ok(Outcome::Failed)
}

async fn try_once(
    client: &reqwest::Client,
    url: &str,
    part: &Path,
    dest: &Path,
    expected_size: u64,
) -> Result<bool> {
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

    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("stream chunk")?;
        file.write_all(&chunk).await.context("write chunk")?;
    }
    file.flush().await.ok();
    drop(file);

    let got = tokio::fs::metadata(part).await?.len();
    if got != expected_size {
        // Wrong size — discard and let the caller retry from scratch.
        tokio::fs::remove_file(part).await.ok();
        return Ok(false);
    }
    tokio::fs::rename(part, dest).await.context("rename into place")?;
    Ok(true)
}

//! Fetch and ed25519-verify the canonical, signed master list — the authoritative
//! set of files in the archive. Fail-closed: an unverified list is never used.

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use std::path::Path;

use crate::config::{data_dir, MASTER_LIST_URL};

/// Compiled-in public key — identical to MASTER_LIST_PUBKEY_B64 in the app's lib.rs.
const PUBKEY_B64: &str = "ftEG8YFMh/SgY7kGKz2qGfZgKaLY/k4uvOzRgmJSk7o=";

#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub info_hash: String,
    pub magnet: String,
    #[serde(default)]
    pub webseeds: Vec<String>,
}

impl Entry {
    pub fn is_audio(&self) -> bool {
        self.name.to_lowercase().ends_with(".mp3")
    }
    pub fn is_video(&self) -> bool {
        self.name.to_lowercase().ends_with(".mp4")
    }
    /// Sermon id (name without extension) — the swarm/torrent identity.
    pub fn id(&self) -> &str {
        self.name
            .rsplit_once('.')
            .map(|(s, _)| s)
            .unwrap_or(&self.name)
    }
    /// Best HTTP download URL: the CDN webseed the master list ships per entry.
    pub fn download_url(&self) -> Option<&str> {
        self.webseeds.first().map(|s| s.as_str())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct MasterList {
    #[serde(default)]
    pub version: u64,
    #[serde(default)]
    pub piece_length: u64,
    #[serde(default)]
    pub trackers: Vec<String>,
    pub entries: std::collections::BTreeMap<String, Entry>,
}

/// Download the list + detached signature, verify ed25519 over the RAW bytes,
/// cache to <data>/master-list.json, then parse. On network failure, fall back
/// to the last verified cache.
pub async fn fetch_verified(client: &reqwest::Client) -> Result<MasterList> {
    let sig_url = format!("{MASTER_LIST_URL}.sig");
    let fetched = async {
        let body = client.get(MASTER_LIST_URL).send().await?.bytes().await?;
        let sig = client.get(&sig_url).send().await?.bytes().await?;
        Ok::<_, anyhow::Error>((body.to_vec(), sig.to_vec()))
    }
    .await;

    let raw = match fetched {
        Ok((body, sig)) => {
            verify(&body, &sig).context("master list signature verification failed")?;
            // Persist the verified copy for offline restarts.
            let cache = data_dir().join("master-list.json");
            std::fs::create_dir_all(data_dir()).ok();
            std::fs::write(&cache, &body).ok();
            body
        }
        Err(e) => {
            // Offline: use the cached verified copy if present.
            let cache = data_dir().join("master-list.json");
            if cache.exists() {
                eprintln!("[masterlist] fetch failed ({e}); using cached copy");
                std::fs::read(&cache).context("read cached master list")?
            } else {
                return Err(e).context("fetch master list");
            }
        }
    };

    let ml: MasterList = serde_json::from_slice(&raw).context("parse master list json")?;
    if ml.entries.is_empty() {
        bail!("master list has no entries");
    }
    Ok(ml)
}

/// Verify a detached ed25519 signature (base64 of 64 raw bytes) over `data`.
fn verify(data: &[u8], sig_b64: &[u8]) -> Result<()> {
    let pk_bytes = base64::engine::general_purpose::STANDARD
        .decode(PUBKEY_B64)
        .context("decode pubkey")?;
    let pk: [u8; 32] = pk_bytes
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("pubkey wrong length"))?;
    let key = VerifyingKey::from_bytes(&pk).context("bad pubkey")?;

    let sig_str = std::str::from_utf8(sig_b64).context("sig not utf8")?;
    let sig_raw = base64::engine::general_purpose::STANDARD
        .decode(sig_str.trim())
        .context("decode signature")?;
    let sig_arr: [u8; 64] = sig_raw
        .as_slice()
        .try_into()
        .map_err(|_| anyhow!("signature wrong length"))?;
    let sig = Signature::from_bytes(&sig_arr);

    key.verify_strict(data, &sig)
        .map_err(|_| anyhow!("signature invalid"))
}

/// Cheap check whether a cached master list exists (for status output).
pub fn cache_path() -> std::path::PathBuf {
    data_dir().join("master-list.json")
}
pub fn has_cache() -> bool {
    Path::new(&cache_path()).exists()
}

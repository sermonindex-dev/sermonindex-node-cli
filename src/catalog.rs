//! The sermon catalogue — titles and speakers for every file in the master list.
//!
//! WHY THE CLI NEEDS THIS
//!
//! The signed master list says what to hold and how to fetch it: an id, a size,
//! a torrent hash. It does not say who preached it or what it is called. That
//! was enough for a seed node, which holds everything, and not enough for a
//! person on an old laptop who wants "all of Leonard Ravenhill" — the thing the
//! desktop app's Bulk Download page has always offered.
//!
//! The desktop app ships this data inside the app. The CLI fetches the same
//! file from the CDN instead (`cli-catalog.json`), signed with the SAME key as
//! the master list and verified the same way: detached ed25519 over the raw
//! bytes, fail closed. An unverified catalogue is never cached and never used,
//! except through `SI_CATALOG`, a local path for development that the menu
//! labels as unverified wherever it shows.
//!
//! What a forged catalogue could do even then is mislabel. It cannot add a file
//! to the download set: every id it names is looked up in the verified master
//! list, which is the only thing downloads are fetched from.
//!
//! Format (compact, shared with the desktop app's torrent-catalog.json):
//!   { "s": [[speaker], …],
//!     "c": [[id, title, speakerIdx, topic, scripture, secs, sizeKB, archive,
//!            cdn, type(0 audio|1 video), views], …] }

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::collections::HashMap;

use crate::config::data_dir;
use crate::masterlist;

pub const CATALOG_URL: &str = "https://sermonindex1.b-cdn.net/torrents/cli-catalog.json";

/// Compare with runs of digits as numbers, ignoring case.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(c), Some(d)) if c.is_ascii_digit() && d.is_ascii_digit() => {
                let mut n1 = String::new();
                while let Some(c) = x.peek().copied().filter(|c| c.is_ascii_digit()) {
                    n1.push(c);
                    x.next();
                }
                let mut n2 = String::new();
                while let Some(d) = y.peek().copied().filter(|d| d.is_ascii_digit()) {
                    n2.push(d);
                    y.next();
                }
                let (t1, t2) = (n1.trim_start_matches('0'), n2.trim_start_matches('0'));
                let o = t1.len().cmp(&t2.len()).then_with(|| t1.cmp(t2));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(c), Some(d)) => {
                let o = c.to_lowercase().cmp(d.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Sermon {
    pub id: String,
    pub title: String,
    pub speaker: usize,
    pub size: u64,
    pub video: bool,
    pub secs: u64,
}

#[derive(Clone, Debug)]
pub struct Speaker {
    pub name: String,
    /// Indexes into `Catalog::sermons`.
    pub sermons: Vec<usize>,
    pub audio: usize,
    pub video: usize,
    pub audio_bytes: u64,
    pub all_bytes: u64,
}

#[derive(Default)]
pub struct Catalog {
    pub sermons: Vec<Sermon>,
    pub speakers: Vec<Speaker>,
    /// True when loaded from SI_CATALOG rather than a verified download.
    pub unverified: bool,
}

fn cache_path() -> std::path::PathBuf {
    data_dir().join("catalog.json")
}

impl Catalog {
    pub fn parse(raw: &[u8]) -> Result<Catalog> {
        let v: Value = serde_json::from_slice(raw).context("catalog is not JSON")?;
        let names: Vec<String> = v
            .get("s")
            .and_then(|s| s.as_array())
            .context("catalog has no speaker list")?
            .iter()
            .map(|e| match e {
                Value::Array(a) => a.first().and_then(|x| x.as_str()).unwrap_or("Unknown").to_string(),
                Value::String(s) => s.clone(),
                _ => "Unknown".to_string(),
            })
            .collect();
        let rows = v.get("c").and_then(|c| c.as_array()).context("catalog has no entries")?;
        let mut sermons = Vec::with_capacity(rows.len());
        for r in rows {
            let Some(a) = r.as_array() else { continue };
            let Some(id) = a.first().and_then(|x| x.as_str()) else { continue };
            let n = |i: usize| a.get(i).and_then(|x| x.as_u64()).unwrap_or(0);
            sermons.push(Sermon {
                id: id.to_string(),
                title: a.get(1).and_then(|x| x.as_str()).unwrap_or("").trim().to_string(),
                speaker: n(2) as usize,
                secs: n(5),
                size: n(6) * 1024,
                video: n(9) == 1,
            });
        }
        if sermons.is_empty() {
            bail!("catalog has no sermons");
        }

        let mut speakers: Vec<Speaker> = names
            .iter()
            .map(|n| Speaker {
                name: n.clone(),
                sermons: Vec::new(),
                audio: 0,
                video: 0,
                audio_bytes: 0,
                all_bytes: 0,
            })
            .collect();
        for (i, s) in sermons.iter().enumerate() {
            if s.speaker >= speakers.len() {
                continue;
            }
            let sp = &mut speakers[s.speaker];
            sp.sermons.push(i);
            sp.all_bytes += s.size;
            if s.video {
                sp.video += 1;
            } else {
                sp.audio += 1;
                sp.audio_bytes += s.size;
            }
        }
        // Dates are not in this file, so titles in natural order: series
        // read "Part 1, Part 2 … Part 10", not "Part 1, Part 10, Part 11".
        for sp in speakers.iter_mut() {
            sp.sermons.sort_by(|&a, &b| natural_cmp(&sermons[a].title, &sermons[b].title));
        }
        Ok(Catalog { sermons, speakers, unverified: false })
    }

    /// Speakers that have at least one sermon, by name.
    pub fn speaker_index(&self) -> HashMap<String, usize> {
        self.speakers
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.sermons.is_empty())
            .map(|(i, s)| (s.name.clone(), i))
            .collect()
    }

    /// The catalogue as last verified, from disk. No network.
    pub fn load_cached() -> Option<Catalog> {
        if let Some(c) = Self::load_dev_override() {
            return Some(c);
        }
        let raw = std::fs::read(cache_path()).ok()?;
        Catalog::parse(&raw).ok()
    }

    fn load_dev_override() -> Option<Catalog> {
        let p = std::env::var("SI_CATALOG").ok()?;
        let raw = std::fs::read(&p).ok()?;
        let mut c = Catalog::parse(&raw).ok()?;
        c.unverified = true;
        Some(c)
    }

    /// Fetch, verify, cache, parse. Falls back to the cached copy when the
    /// network is unavailable; never falls back to an unverified download.
    pub async fn fetch(client: &reqwest::Client) -> Result<Catalog> {
        if let Some(c) = Self::load_dev_override() {
            return Ok(c);
        }
        let sig_url = format!("{CATALOG_URL}.sig");
        let got = async {
            let r = client.get(CATALOG_URL).send().await?;
            if !r.status().is_success() {
                bail!("catalog: HTTP {}", r.status());
            }
            let body = r.bytes().await?;
            let s = client.get(&sig_url).send().await?;
            if !s.status().is_success() {
                bail!("catalog signature: HTTP {}", s.status());
            }
            let sig = s.bytes().await?;
            Ok::<_, anyhow::Error>((body.to_vec(), sig.to_vec()))
        }
        .await;
        match got {
            Ok((body, sig)) => {
                masterlist::verify(&body, &sig).context("catalog signature verification failed")?;
                std::fs::create_dir_all(data_dir()).ok();
                let tmp = cache_path().with_extension("json.tmp");
                if std::fs::write(&tmp, &body).is_ok() {
                    let _ = std::fs::rename(&tmp, cache_path());
                }
                Catalog::parse(&body)
            }
            Err(e) => match Self::load_cached() {
                Some(c) => Ok(c),
                None => Err(e),
            },
        }
    }

    /// Age of the cached copy, for deciding whether to refresh.
    pub fn cache_age() -> Option<std::time::Duration> {
        std::fs::metadata(cache_path()).ok()?.modified().ok()?.elapsed().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point SI_TEST_CATALOG at a signed cli-catalog.json (with its .sig next
    /// to it) to check a publish before uploading it. Skipped when unset.
    #[test]
    fn a_published_catalog_verifies_and_parses() {
        let Ok(p) = std::env::var("SI_TEST_CATALOG") else { return };
        let body = std::fs::read(&p).unwrap();
        let sig = std::fs::read(format!("{p}.sig")).unwrap();
        masterlist::verify(&body, &sig).expect("signature must verify with the node's key");
        let c = Catalog::parse(&body).unwrap();
        assert!(c.sermons.len() > 1000 && c.speaker_index().len() > 100);
        let mut tampered = body.clone();
        tampered[100] ^= 1;
        assert!(masterlist::verify(&tampered, &sig).is_err(), "a changed byte must fail");
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["Part 10", "Part 2", "part 1", "Part 11"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, vec!["part 1", "Part 2", "Part 10", "Part 11"]);
    }
}

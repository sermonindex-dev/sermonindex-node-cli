//! "My picks" — the speakers and sermons a person chose in the menu.
//!
//! Stored in `~/.sermonindex/picks.json`, next to settings.json, and read by
//! the running node on every sweep. Picks are ADDED to whatever the scope
//! holds: a seed node on the audio scope that picks a speaker gains that
//! speaker's videos, and a laptop on the `picks` scope holds nothing else.
//!
//! A picked speaker is stored by NAME, not as a list of files, and resolved
//! against the catalogue each sweep — so when a new sermon by that speaker is
//! published, it arrives on its own. Individually picked sermons are stored
//! by id.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::catalog::Catalog;
use crate::config::data_dir;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SpeakerPick {
    pub name: String,
    /// Include this speaker's videos as well as audio.
    #[serde(default)]
    pub video: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Picks {
    #[serde(default)]
    pub speakers: Vec<SpeakerPick>,
    #[serde(default)]
    pub sermons: Vec<String>,
}

pub fn path() -> std::path::PathBuf {
    data_dir().join("picks.json")
}

impl Picks {
    pub fn load() -> Picks {
        std::fs::read(path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    /// Temp file + rename, so a node reading mid-write never sees half a file.
    pub fn save(&self) -> Result<()> {
        std::fs::create_dir_all(data_dir())?;
        let tmp = path().with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, path())?;
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.speakers.is_empty() && self.sermons.is_empty()
    }

    pub fn speaker(&self, name: &str) -> Option<&SpeakerPick> {
        self.speakers.iter().find(|s| s.name == name)
    }

    pub fn set_speaker(&mut self, name: &str, video: bool) {
        match self.speakers.iter_mut().find(|s| s.name == name) {
            Some(s) => s.video = video,
            None => self.speakers.push(SpeakerPick { name: name.to_string(), video }),
        }
    }

    pub fn remove_speaker(&mut self, name: &str) {
        self.speakers.retain(|s| s.name != name);
    }

    pub fn has_sermon(&self, id: &str) -> bool {
        self.sermons.iter().any(|s| s == id)
    }

    pub fn toggle_sermon(&mut self, id: &str) {
        if self.has_sermon(id) {
            self.sermons.retain(|s| s != id);
        } else {
            self.sermons.push(id.to_string());
        }
    }

    /// Every file id these picks stand for. Speakers need the catalogue; with
    /// none cached only the individually picked sermons resolve, and the next
    /// sweep after the menu has fetched one picks up the rest.
    pub fn wanted_ids(&self, catalog: Option<&Catalog>) -> HashSet<String> {
        let mut out: HashSet<String> = self.sermons.iter().cloned().collect();
        if let Some(cat) = catalog {
            let idx = cat.speaker_index();
            for sp in &self.speakers {
                if let Some(&i) = idx.get(&sp.name) {
                    for &si in &cat.speakers[i].sermons {
                        let s = &cat.sermons[si];
                        if sp.video || !s.video {
                            out.insert(s.id.clone());
                        }
                    }
                }
            }
        }
        out
    }
}

/// The ids a running node should add to its scope right now: picks resolved
/// against the cached catalogue. Cheap when there are no picks — the catalogue
/// is only parsed if a speaker needs resolving.
pub fn wanted_now() -> HashSet<String> {
    let p = Picks::load();
    if p.is_empty() {
        return HashSet::new();
    }
    let cat = if p.speakers.is_empty() { None } else { Catalog::load_cached() };
    p.wanted_ids(cat.as_ref())
}

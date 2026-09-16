//! Version checking for the headless node — notify only, never silent install.
//!
//! WHY NOT AUTO-UPDATE
//!
//! The desktop app auto-updates and should: a person is sitting in front of it,
//! the relaunch is visible, and a banner asked first. A seed node is the
//! opposite case. It is a box in a church cupboard or on a shelf, often the
//! only copy of the library in its region, and frequently mid-upload to
//! somebody. A daemon that decides on its own to stop, swap its binary and
//! restart is a daemon that drops every peer it was serving, at a moment
//! nobody chose, for a reason nobody will see. A node one version behind costs
//! far less than that.
//!
//! So this module answers one question — "is there a newer release?" — and
//! then does nothing but say so: once a day in the log, and continuously on
//! /stats so the local dashboard can show a badge. Installing is a deliberate
//! act: `sermonindex-node update` prints exactly what to run.
//!
//! TWO SOURCES, NEITHER REQUIRED
//!
//!   1. `node-cli/releases/releases.json` — the index `publish-node-cli.sh`
//!      already writes and purges on every release, and the same one the
//!      /node-software/ page renders from. Reusing it means there is no second
//!      manifest to remember and no way for the two to disagree.
//!   2. `latest_cli_version` in the heartbeat response, which the server can
//!      set to push a version out between manifest polls. The heartbeat is
//!      already happening, so this costs nothing.
//!
//! Both are advisory: nothing here downloads or executes anything, so a
//! compromised answer can at worst print a wrong version number. That is the
//! reason this file is allowed to be this simple, and the reason it must stay
//! that way — the moment anything here fetches a binary, it needs the same
//! signature verification the master list has.

use serde_json::Value;

use crate::config::MANIFEST_URL;

/// Compare two dotted version strings numerically ("0.2.10" > "0.2.9", which
/// string comparison gets wrong). Missing components count as 0, so "0.3" and
/// "0.3.0" are equal. Anything unparseable in a component makes it 0 rather
/// than failing the comparison — a malformed manifest should not be able to
/// announce an upgrade.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let parse = |v: &str| -> Vec<u64> {
        v.trim()
            .trim_start_matches(['v', 'V'])
            .split(['.', '-', '+'])
            .map(|p| p.parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (a, b) = (parse(candidate), parse(current));
    let n = a.len().max(b.len());
    for i in 0..n {
        let (x, y) = (a.get(i).copied().unwrap_or(0), b.get(i).copied().unwrap_or(0));
        if x != y {
            return x > y;
        }
    }
    false
}

/// Our own version, without the "cli-" prefix `APP_VERSION` carries.
pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Fetch the manifest and return the released version if it is newer than ours.
///
/// Every failure path returns `None`: an unreachable CDN, a 404 (nothing
/// published yet), malformed JSON, a missing field. "We could not check" and
/// "there is no update" are the same outcome for a notifier, and neither is
/// worth waking anyone over.
pub async fn check(client: &reqwest::Client) -> Option<String> {
    let resp = client
        .get(MANIFEST_URL)
        .timeout(std::time::Duration::from_secs(20))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let j: Value = resp.json().await.ok()?;

    // releases.json is newest-first, but do not rely on that: take the highest
    // version present. A hand-edited index, a re-published older build, or a
    // sort that went wrong upstream must never be able to announce a downgrade.
    let mut best: Option<String> = None;
    for r in j.get("releases").and_then(|v| v.as_array())? {
        let Some(v) = r.get("version").and_then(|v| v.as_str()) else { continue };
        let v = v.trim_start_matches(['v', 'V']);
        if is_newer(v, current()) && best.as_deref().is_none_or(|b| is_newer(v, b)) {
            best = Some(v.to_string());
        }
    }
    best
}

/// Same test against a version the heartbeat response volunteered.
pub fn from_heartbeat(body: &Value) -> Option<String> {
    let v = body.get("latest_cli_version").and_then(|v| v.as_str())?;
    is_newer(v, current()).then(|| v.to_string())
}

/// The line printed for `sermonindex-node update`, and once a day by the
/// background check. Deliberately instructions rather than an action.
pub fn install_hint(latest: &str) -> String {
    format!(
        "A newer node is available: {latest} (running {}).\n\
         \n\
         Nothing has been installed — a seed node never updates itself while it\n\
         may be mid-upload to someone. To update, when it suits you:\n\
         \n\
           1. Stop the node (Ctrl-C, or `systemctl --user stop sermonindex-node`)\n\
           2. Download {latest} from https://sermonindex.net/md/node-software/\n\
           3. Replace the binary and start it again\n\
         \n\
         Your library, torrents and settings in ~/.sermonindex are untouched by\n\
         an upgrade — the node picks up exactly where it left off.",
        current()
    )
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_numerically_not_lexically() {
        assert!(is_newer("0.2.10", "0.2.9")); // the case string compare fails
        assert!(is_newer("0.3.0", "0.2.99"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("v0.2.2", "0.2.1")); // tolerate a leading v
        assert!(!is_newer("0.2.1", "0.2.1"));
        assert!(!is_newer("0.3", "0.3.0")); // missing components are zero
        assert!(!is_newer("0.1.0", "0.2.1")); // never downgrade
        assert!(!is_newer("garbage", "0.2.1")); // unparseable announces nothing
        assert!(!is_newer("", "0.2.1"));
    }
}

//! Finding out whether a newer release exists — the pure half of `puntu update`.
//!
//! The command itself (in `main.rs`) only moves bytes around: it asks GitHub for the latest
//! release, downloads the archive and runs the `install.sh` that is *inside* it. Everything
//! that needs deciding is here, where it can be tested: what the reply says, and whether
//! that is newer than what is running.

use std::cmp::Ordering;

/// The repository releases come from. `PUNTU_REPO=owner/name` overrides it, same as in
/// `install.sh` — a fork updates from itself.
pub const DEFAULT_REPO: &str = "dzumenovalfer/Puntu";

/// The repository to ask, honouring the `PUNTU_REPO` override.
pub fn repo() -> String {
    std::env::var("PUNTU_REPO")
        .ok()
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REPO.to_string())
}

/// Where to ask for the latest release: GitHub's API for [`repo`], unless
/// `PUNTU_UPDATE_URL` names another address that answers in the same shape — a mirror, or a
/// file on disk (`file://…`), which is how the whole update path gets tested without
/// publishing anything.
pub fn latest_release_url() -> String {
    std::env::var("PUNTU_UPDATE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| format!("https://api.github.com/repos/{}/releases/latest", repo()))
}

/// A release version: `0.9.6`, `v0.9.6`, `0.9.6-dev`.
///
/// Ordered the way semver orders them, as far as this project uses it: by the numbers, and
/// with a pre-release (`-dev`, `-rc1`) **before** the release of the same numbers. That last
/// part matters: a branch build calls itself `0.9.6-dev`, and plain string or `sort -V`
/// comparison puts it *after* `0.9.6` — so the one build that most needs the release would
/// be told it is already ahead of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Version {
    numbers: Vec<u64>,
    prerelease: bool,
}

impl Version {
    /// Parse a version or a tag. `None` when there is no number in it at all.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, !pre.is_empty()),
            None => (text, false),
        };
        // `+build` metadata says nothing about precedence.
        let core = core.split('+').next().unwrap_or(core);
        let numbers: Option<Vec<u64>> = core.split('.').map(|n| n.parse().ok()).collect();
        let mut numbers = numbers.filter(|n| !n.is_empty())?;
        // `1.2` and `1.2.0` are the same version; keep one spelling so `==` agrees with `cmp`.
        while numbers.len() > 1 && numbers.last() == Some(&0) {
            numbers.pop();
        }
        Some(Version { numbers, prerelease: pre })
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        // Missing components count as zero: `1.2` is `1.2.0`.
        let len = self.numbers.len().max(other.numbers.len());
        let part = |v: &Version, i: usize| v.numbers.get(i).copied().unwrap_or(0);
        (0..len)
            .map(|i| part(self, i).cmp(&part(other, i)))
            .find(|o| o.is_ne())
            // Same numbers: the pre-release is the older one.
            .unwrap_or_else(|| other.prerelease.cmp(&self.prerelease))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// What `puntu update` found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// The release is newer than what is running.
    Available { current: String, latest: String },
    /// Nothing to do.
    UpToDate { current: String },
    /// What is running is newer than the latest release — a build from a branch. Replacing
    /// it with the release would be a downgrade, so nothing is done.
    Ahead { current: String, latest: String },
}

/// Compare the running version with the latest release tag. `None` if either does not parse.
pub fn verdict(current: &str, latest_tag: &str) -> Option<Verdict> {
    let (cur, new) = (Version::parse(current)?, Version::parse(latest_tag)?);
    let latest = latest_tag.trim().trim_start_matches('v').to_string();
    let current = current.trim().to_string();
    Some(match new.cmp(&cur) {
        Ordering::Greater => Verdict::Available { current, latest },
        Ordering::Equal => Verdict::UpToDate { current },
        Ordering::Less => Verdict::Ahead { current, latest },
    })
}

/// The value of a top-level string field in GitHub's release JSON (`"tag_name": "v0.9.6"`).
///
/// A whole JSON parser for two strings would be a dependency for its own sake; the reply's
/// shape is fixed and these values contain nothing that needs unescaping. The first
/// occurrence wins, which is the release's own field — nested objects come later.
pub fn json_string<'a>(json: &'a str, field: &str) -> Option<&'a str> {
    let key = format!("\"{field}\"");
    let after = &json[json.find(&key)? + key.len()..];
    let after = after.trim_start().strip_prefix(':')?.trim_start();
    let value = after.strip_prefix('"')?;
    value.find('"').map(|end| &value[..end])
}

/// The download URL of the release archive for `arch` (`x86_64`), if the release has one.
pub fn asset_url<'a>(json: &'a str, arch: &str) -> Option<&'a str> {
    let name = format!("puntu-{arch}-linux.tar.gz");
    let key = "\"browser_download_url\"";
    let mut rest = json;
    while let Some(at) = rest.find(key) {
        rest = &rest[at..];
        let url = json_string(rest, "browser_download_url")?;
        if url.ends_with(&name) {
            return Some(url);
        }
        rest = &rest[key.len()..];
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn versions_order_by_number_not_by_text() {
        assert!(v("0.9.10") > v("0.9.9"), "ten is more than nine");
        assert!(v("0.10.0") > v("0.9.99"));
        assert!(v("1.0") > v("0.99.99"));
        assert_eq!(v("v0.9.6"), v("0.9.6"), "a tag and a version are the same thing");
        assert_eq!(v("1.2"), v("1.2.0"));
        assert_eq!(v("1.2").cmp(&v("1.2.0")), Ordering::Equal);
        assert!(v("1.0") > v("0.9") && v("1") == v("1.0.0"));
    }

    #[test]
    fn a_dev_build_is_older_than_its_release() {
        // The build this was written on called itself `0.9.6-dev`. Told that `0.9.6` is out,
        // it has to update — `sort -V` and plain string order both say the opposite.
        assert!(v("0.9.6") > v("0.9.6-dev"));
        assert!(v("0.9.6-rc1") < v("0.9.6"));
        // …but it is newer than the release before it.
        assert!(v("0.9.6-dev") > v("0.9.5"));
        assert_eq!(v("0.9.6+build7"), v("0.9.6"));
    }

    #[test]
    fn nonsense_is_not_a_version() {
        for bad in ["", "latest", "v", "1.x", "-dev"] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_verdict_never_offers_a_downgrade() {
        assert_eq!(
            verdict("0.9.5", "v0.9.6"),
            Some(Verdict::Available { current: "0.9.5".into(), latest: "0.9.6".into() })
        );
        assert_eq!(
            verdict("0.9.6-dev", "v0.9.6"),
            Some(Verdict::Available { current: "0.9.6-dev".into(), latest: "0.9.6".into() })
        );
        assert_eq!(verdict("0.9.6", "v0.9.6"), Some(Verdict::UpToDate { current: "0.9.6".into() }));
        assert_eq!(
            verdict("0.9.7-dev", "v0.9.6"),
            Some(Verdict::Ahead { current: "0.9.7-dev".into(), latest: "0.9.6".into() })
        );
        assert_eq!(verdict("0.9.6", "nightly"), None);
    }

    /// The parts of GitHub's `releases/latest` reply this reads, in its real shape.
    const REPLY: &str = r#"{
  "url": "https://api.github.com/repos/o/r/releases/1",
  "tag_name": "v0.9.6",
  "name": "v0.9.6",
  "prerelease": false,
  "assets": [
    {
      "name": "puntu-aarch64-linux.tar.gz",
      "browser_download_url": "https://github.com/o/r/releases/download/v0.9.6/puntu-aarch64-linux.tar.gz"
    },
    {
      "name": "puntu-x86_64-linux.tar.gz",
      "browser_download_url": "https://github.com/o/r/releases/download/v0.9.6/puntu-x86_64-linux.tar.gz"
    }
  ],
  "body": "notes mentioning \"tag_name\": \"v0.0.1\" must not win"
}"#;

    #[test]
    fn the_release_reply_is_read_without_a_json_parser() {
        assert_eq!(json_string(REPLY, "tag_name"), Some("v0.9.6"));
        assert_eq!(json_string(REPLY, "nope"), None);
        assert_eq!(
            asset_url(REPLY, "x86_64"),
            Some("https://github.com/o/r/releases/download/v0.9.6/puntu-x86_64-linux.tar.gz")
        );
        // The right architecture, not the first asset.
        assert!(asset_url(REPLY, "aarch64").unwrap().ends_with("puntu-aarch64-linux.tar.gz"));
        assert_eq!(asset_url(REPLY, "riscv64"), None, "no build for this machine");
        // Compact JSON (no spaces) reads the same.
        assert_eq!(json_string(r#"{"tag_name":"v1.2.3"}"#, "tag_name"), Some("v1.2.3"));
        // An error reply has neither.
        assert_eq!(json_string(r#"{"message":"Not Found"}"#, "tag_name"), None);
    }
}

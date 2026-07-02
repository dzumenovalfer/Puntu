//! Active keyboard layout: read, switch, and watch it on GNOME via `gsettings`.
//!
//! M1 shells out to `gsettings` (present on every GNOME box) instead of linking glib/GIO, so
//! the whole milestone builds without `-dev` packages. Layout switches happen at most once per
//! correction, so the subprocess cost is negligible. Swapping in in-process GIO is a later
//! optimization (plan note).

use std::process::Command;

use anyhow::{anyhow, Context, Result};

use crate::keymap::Lang;

const SCHEMA: &str = "org.gnome.desktop.input-sources";

/// Run `gsettings get SCHEMA key` and return trimmed stdout.
fn gsettings_get(key: &str) -> Result<String> {
    let out = Command::new("gsettings")
        .args(["get", SCHEMA, key])
        .output()
        .with_context(|| "running `gsettings get` (is GNOME running?)")?;
    if !out.status.success() {
        return Err(anyhow!("gsettings get {key} failed: {}", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The active input-source index (`current`).
pub fn current_index() -> Result<u32> {
    // Output looks like `uint32 1`.
    let raw = gsettings_get("current")?;
    raw.split_whitespace()
        .last()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| anyhow!("could not parse current index from {raw:?}"))
}

/// The configured input sources as `(type, id)` pairs, e.g. `[("xkb","us"), ("xkb","ru")]`.
pub fn sources() -> Result<Vec<(String, String)>> {
    let raw = gsettings_get("sources")?;
    Ok(parse_sources(&raw))
}

/// Parse the GVariant array of `(s, s)` tuples gsettings prints for `sources`.
fn parse_sources(raw: &str) -> Vec<(String, String)> {
    // Collect every single-quoted token in order, then pair them up (type, id).
    let mut tokens = Vec::new();
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\'' {
            let mut tok = String::new();
            for c2 in chars.by_ref() {
                if c2 == '\'' {
                    break;
                }
                tok.push(c2);
            }
            tokens.push(tok);
        }
    }
    if tokens.len() % 2 != 0 {
        tracing::warn!(
            "gsettings sources output had an odd number of quoted tokens; \
             dropping the trailing one: {raw:?}"
        );
    }
    tokens.chunks_exact(2).map(|p| (p[0].clone(), p[1].clone())).collect()
}

/// The language of an xkb id like `us`, `ru`, or `ru+phonetic`.
fn lang_of_id(id: &str) -> Option<Lang> {
    let base = id.split('+').next().unwrap_or(id);
    Lang::from_xkb_name(base)
}

/// The most-recently-used sources, `[(type, id), …]`. On GNOME the **first** element is the
/// currently active layout — and unlike `current`, Mutter keeps this up to date when the user
/// switches layout via the keyboard shortcut.
pub fn mru_sources() -> Result<Vec<(String, String)>> {
    let raw = gsettings_get("mru-sources")?;
    Ok(parse_sources(&raw))
}

/// The currently active language. Reads `mru-sources[0]` (authoritative on GNOME); falls back to
/// `current` + `sources` only if mru-sources is empty/unavailable.
pub fn active_lang() -> Result<Option<Lang>> {
    if let Ok(mru) = mru_sources() {
        if let Some((_, id)) = mru.first() {
            return Ok(lang_of_id(id));
        }
    }
    let idx = current_index()? as usize;
    let srcs = sources()?;
    Ok(srcs.get(idx).and_then(|(_, id)| lang_of_id(id)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sources_list() {
        let raw = "[('xkb', 'us'), ('xkb', 'ru')]";
        assert_eq!(parse_sources(raw), vec![
            ("xkb".to_string(), "us".to_string()),
            ("xkb".to_string(), "ru".to_string()),
        ]);
    }

    #[test]
    fn parses_variant_ids() {
        let raw = "[('xkb', 'us'), ('xkb', 'ru+phonetic')]";
        let s = parse_sources(raw);
        assert_eq!(lang_of_id(&s[0].1), Some(Lang::En));
        assert_eq!(lang_of_id(&s[1].1), Some(Lang::Ru));
    }

    #[test]
    fn odd_token_count_keeps_complete_pairs() {
        // Malformed gsettings output (odd number of quoted tokens) must not panic and must
        // keep every complete pair; the dangling token is dropped (and warned about).
        let raw = "[('xkb', 'us'), ('xkb')]";
        assert_eq!(parse_sources(raw), vec![("xkb".to_string(), "us".to_string())]);
    }
}

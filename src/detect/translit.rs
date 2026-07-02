//! Key-for-key transliteration between layouts (the "same physical key, other layout" map).
//!
//! Auto-correction replays key codes, so it doesn't need this. It's used for converting
//! arbitrary text where we only have the characters: the manual-convert hotkey acting on a
//! word, and (Phase 2) converting a selection/clipboard.

use crate::keymap::{char_for, find_key, Lang};

/// Convert `c` from layout `from` into the character on the same physical key in `to`.
/// Returns `c` unchanged if it isn't on a known key in `from`.
pub fn convert_char(c: char, from: Lang, to: Lang) -> char {
    match find_key(c, from) {
        Some((code, shift)) => char_for(code, shift, to).unwrap_or(c),
        None => c,
    }
}

/// Transliterate a whole string key-for-key from `from` into `to`.
pub fn convert(s: &str, from: Lang, to: Lang) -> String {
    s.chars().map(|c| convert_char(c, from, to)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn en_to_ru_roundtrip() {
        assert_eq!(convert("ghbdtn", Lang::En, Lang::Ru), "привет");
        assert_eq!(convert("привет", Lang::Ru, Lang::En), "ghbdtn");
    }

    #[test]
    fn ru_to_en_roundtrip() {
        assert_eq!(convert("руддщ", Lang::Ru, Lang::En), "hello");
        assert_eq!(convert("hello", Lang::En, Lang::Ru), "руддщ");
    }

    #[test]
    fn preserves_case_and_unknowns() {
        assert_eq!(convert("Ghbdtn", Lang::En, Lang::Ru), "Привет");
        // A digit is on the same key in both layouts → unchanged.
        assert_eq!(convert("a1", Lang::En, Lang::Ru), "ф1");
    }
}

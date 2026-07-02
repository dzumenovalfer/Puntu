//! Recognize the configured hotkeys from the (passively read) key stream.

use crate::config::Hotkeys;
use crate::keymap::Mods;

/// A user-triggered action recognized from a key chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotAction {
    /// Undo the last automatic correction (Pause/Break analog).
    Undo,
    /// Convert the current/last word on demand.
    ConvertLast,
    /// Add the current/last word to the never-correct exceptions.
    AddException,
    /// Force-convert the current/last word and remember to always convert it.
    ForceCorrect,
}

/// Return the action bound to `(code, mods)`, if any. Order matters only if bindings overlap;
/// the most specific (more modifiers) is checked first.
pub fn match_action(code: u16, mods: Mods, hk: &Hotkeys) -> Option<HotAction> {
    if hk.force_correct.matches(code, mods) {
        return Some(HotAction::ForceCorrect);
    }
    if hk.add_exception.matches(code, mods) {
        return Some(HotAction::AddException);
    }
    if hk.convert_last.matches(code, mods) {
        return Some(HotAction::ConvertLast);
    }
    if hk.undo.matches(code, mods) {
        return Some(HotAction::Undo);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Hotkeys;

    #[test]
    fn matches_defaults() {
        let hk = Hotkeys::default();
        let plain = Mods::default();
        let shift = Mods { shift: true, ..Default::default() };
        let ctrl = Mods { ctrl: true, ..Default::default() };
        let alt = Mods { alt: true, ..Default::default() };
        assert_eq!(match_action(119, plain, &hk), Some(HotAction::Undo));
        assert_eq!(match_action(119, shift, &hk), Some(HotAction::ConvertLast));
        assert_eq!(match_action(119, ctrl, &hk), Some(HotAction::AddException));
        assert_eq!(match_action(119, alt, &hk), Some(HotAction::ForceCorrect));
        assert_eq!(match_action(57, plain, &hk), None);
    }
}

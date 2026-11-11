//! Word accumulator with **trusted-context** tracking.
//!
//! The buffer stores only physical key codes; it doesn't track the layout. Tokenization is
//! layout-independent (a key is a "letter" if it's alphabetic in *either* layout), so the same
//! key codes produce the same word boundaries regardless of the active layout. The actual
//! language is supplied at [`finish`](WordBuffer::finish) time — read fresh from the system so we
//! never act on a stale guess.
//!
//! Trusted context: autocorrection is only offered for a word typed straight through with the
//! cursor at its end. Any navigation, mid-word edit, paste, focus change or mouse click marks the
//! buffer untrusted, and a mid-word cursor move discards the in-progress word entirely.

use crate::keymap::{char_for, KeyEvent, Lang};

/// A finished word handed to the detector.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletedWord {
    /// The physical keys (code + shift) that produced the word, in order.
    pub keys: Vec<(u16, bool)>,
    /// Text as it appears on screen (current layout).
    pub cur: String,
    /// The same keys interpreted through the other layout.
    pub alt: String,
    /// The active layout/language while the word was typed.
    pub lang: Lang,
    /// Whether the typing context was trusted for the whole word.
    pub trusted: bool,
}

impl CompletedWord {
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Build a word directly from physical keys (used by the `--stdin` test harness).
    pub fn from_keys(keys: Vec<(u16, bool)>, lang: Lang, trusted: bool) -> CompletedWord {
        build_word(&keys, lang, trusted)
    }
}

/// Accumulates key presses into words and tracks whether the context is trusted.
#[derive(Debug, Default)]
pub struct WordBuffer {
    keys: Vec<(u16, bool)>,
    /// The shift level to read each key at in the *other* layout — parallel to `keys`, and
    /// equal to its `bool` except under CapsLock (see [`KeyEvent::Letter`]).
    alt_shifts: Vec<bool>,
    trusted: bool,
}

impl WordBuffer {
    pub fn new() -> Self {
        WordBuffer {
            keys: Vec::with_capacity(32),
            alt_shifts: Vec::with_capacity(32),
            trusted: true,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn trusted(&self) -> bool {
        self.trusted
    }

    /// Mark the context untrusted and discard the partial word. Called on mouse clicks, focus
    /// changes, inactivity timeouts, and any editing/navigation key.
    pub fn invalidate(&mut self) {
        self.keys.clear();
        self.alt_shifts.clear();
        self.trusted = false;
    }

    /// Feed one classified key. Separators are handled by the caller (which calls [`finish`]).
    pub fn push(&mut self, ev: KeyEvent) {
        match ev {
            KeyEvent::Letter { code, shift, alt_shift, .. } => {
                // A new word (first letter on an empty buffer) starts trusted: it's typed
                // contiguously, so the chars before the cursor are exactly this word.
                //
                // "Exactly this word" is an assumption about what sits BEFORE the first
                // letter, and the buffer cannot check it — it only sees keys. A front-end
                // that knows better (the caret is right after a letter: the user backspaced
                // into a word and is retyping its ending) must mark the finished word
                // untrusted itself; the IBus engine does, see `PuntuEngine::suspect`.
                if self.keys.is_empty() {
                    self.trusted = true;
                }
                self.keys.push((code, shift));
                self.alt_shifts.push(alt_shift);
            }
            KeyEvent::Backspace => {
                // Deleting from the tail keeps the cursor at the word end → still trusted.
                self.keys.pop();
                self.alt_shifts.pop();
            }
            KeyEvent::Invalidate => self.invalidate(),
            KeyEvent::Separator | KeyEvent::Ignore => {}
        }
    }

    /// Close the current word, interpreting it in `lang`. Resets to a fresh trusted word.
    /// Returns `None` for an empty buffer (and restores trust at the clean boundary).
    pub fn finish(&mut self, lang: Lang) -> Option<CompletedWord> {
        if self.keys.is_empty() {
            self.trusted = true;
            return None;
        }
        let keys = std::mem::take(&mut self.keys);
        let alt_shifts = std::mem::take(&mut self.alt_shifts);
        let trusted = self.trusted;
        self.trusted = true;
        Some(build_word_with(&keys, &alt_shifts, lang, trusted))
    }

    /// A read-only snapshot of the current (unfinished) word in `lang`, for a manual-convert
    /// hotkey acting on the last word without consuming the buffer.
    pub fn snapshot(&self, lang: Lang) -> Option<CompletedWord> {
        if self.keys.is_empty() {
            return None;
        }
        Some(build_word_with(&self.keys, &self.alt_shifts, lang, self.trusted))
    }
}

fn build_word(keys: &[(u16, bool)], lang: Lang, trusted: bool) -> CompletedWord {
    let cur = render(keys, lang);
    let alt = render(keys, lang.other());
    CompletedWord { keys: keys.to_vec(), cur, alt, lang, trusted }
}

/// [`build_word`] for keys whose other-layout reading has its own shift levels (`alt_shifts`,
/// parallel to `keys`). `CompletedWord::keys` keeps the current-layout levels: that is what
/// the uinput daemon replays, and what `cur` is rendered from.
fn build_word_with(
    keys: &[(u16, bool)],
    alt_shifts: &[bool],
    lang: Lang,
    trusted: bool,
) -> CompletedWord {
    let cur = render(keys, lang);
    let alt: String = keys
        .iter()
        .zip(alt_shifts)
        .filter_map(|(&(code, _), &shift)| char_for(code, shift, lang.other()))
        .collect();
    CompletedWord { keys: keys.to_vec(), cur, alt, lang, trusted }
}

fn render(keys: &[(u16, bool)], lang: Lang) -> String {
    keys.iter()
        .filter_map(|&(code, shift)| char_for(code, shift, lang))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::{classify, Mods};

    /// Helper: classify + push a sequence of keycodes (no modifiers).
    fn type_keys(buf: &mut WordBuffer, codes: &[u16]) {
        for &c in codes {
            buf.push(classify(c, Mods::default(), Lang::En));
        }
    }

    #[test]
    fn completes_word_on_finish() {
        let mut buf = WordBuffer::new();
        type_keys(&mut buf, &[34, 35, 48, 32, 20, 49]); // ghbdtn
        let w = buf.finish(Lang::En).unwrap();
        assert_eq!(w.cur, "ghbdtn");
        assert_eq!(w.alt, "привет");
        assert!(w.trusted);
    }

    #[test]
    fn finish_picks_language_at_close_time() {
        let mut buf = WordBuffer::new();
        type_keys(&mut buf, &[35, 18, 38, 38, 24]); // h e l l o keys
        // Same keys, interpreted as RU (active) → "руддщ", with EN alt "hello".
        let w = buf.snapshot(Lang::Ru).unwrap();
        assert_eq!(w.cur, "руддщ");
        assert_eq!(w.alt, "hello");
    }

    #[test]
    fn backspace_from_tail_stays_trusted() {
        let mut buf = WordBuffer::new();
        type_keys(&mut buf, &[34, 35, 48]); // ghb
        buf.push(KeyEvent::Backspace); // -> gh
        let w = buf.finish(Lang::En).unwrap();
        assert_eq!(w.cur, "gh");
        assert!(w.trusted);
    }

    #[test]
    fn cursor_move_discards_partial_but_next_word_is_trusted() {
        let mut buf = WordBuffer::new();
        type_keys(&mut buf, &[34, 35, 48]); // "ghb" in progress
        buf.push(KeyEvent::Invalidate); // cursor moved mid-word → discard it
        assert!(buf.is_empty());
        type_keys(&mut buf, &[32, 20]); // fresh contiguous word
        let w = buf.finish(Lang::En).unwrap();
        assert!(w.trusted, "a fresh word after a cursor move is trusted");
    }

    #[test]
    fn other_layout_reading_can_sit_at_its_own_shift_level() {
        // CapsLock + the `;` key: `;` in US-QWERTY (punctuation ignores CapsLock), `Ж` in
        // ЙЦУКЕН. One key press, two levels — the buffer has to keep both.
        let mut buf = WordBuffer::new();
        buf.push(KeyEvent::Letter { code: 32, shift: true, alt_shift: true, cur: 'D', alt: 'В' });
        buf.push(KeyEvent::Letter { code: 33, shift: true, alt_shift: true, cur: 'F', alt: 'А' });
        buf.push(KeyEvent::Letter { code: 39, shift: false, alt_shift: true, cur: ';', alt: 'Ж' });
        let snap = buf.snapshot(Lang::En).unwrap();
        assert_eq!((snap.cur.as_str(), snap.alt.as_str()), ("DF;", "ВАЖ"));
        // Backspace drops the key from both readings together.
        buf.push(KeyEvent::Backspace);
        buf.push(KeyEvent::Letter { code: 39, shift: true, alt_shift: false, cur: ':', alt: 'ж' });
        let w = buf.finish(Lang::En).unwrap();
        assert_eq!((w.cur.as_str(), w.alt.as_str()), ("DF:", "ВАж"));
        // …and nothing is left behind for the next word.
        buf.push(KeyEvent::Letter { code: 34, shift: false, alt_shift: false, cur: 'g', alt: 'п' });
        let w = buf.finish(Lang::En).unwrap();
        assert_eq!((w.cur.as_str(), w.alt.as_str()), ("g", "п"));
    }

    #[test]
    fn clean_boundary_restores_trust() {
        let mut buf = WordBuffer::new();
        buf.push(KeyEvent::Invalidate);
        assert_eq!(buf.finish(Lang::En), None); // empty separator = clean boundary
        type_keys(&mut buf, &[34, 35]);
        let w = buf.finish(Lang::En).unwrap();
        assert!(w.trusted);
    }
}

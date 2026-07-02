//! Physical-key → character tables for the supported layouts, plus a pure classifier that
//! turns a raw evdev key event into a tokenizer-level [`KeyEvent`].
//!
//! For Milestone 1 (Russian ЙЦУКЕН ↔ English US QWERTY) we ship hand-written tables instead
//! of depending on libxkbcommon: for this fixed pair they are exact, and they keep the whole
//! M1 build free of system `-dev` packages. Swapping in xkbcommon for arbitrary layouts is a
//! later upgrade (see plan M4).

use std::fmt;

/// A language / layout. The two are 1:1 for the M1 pair, so we model them together.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lang {
    Ru,
    En,
}

impl Lang {
    /// The opposite member of the M1 pair.
    pub fn other(self) -> Lang {
        match self {
            Lang::Ru => Lang::En,
            Lang::En => Lang::Ru,
        }
    }

    /// xkb short name as reported by GNOME `input-sources` (`'us'` / `'ru'`).
    pub fn xkb_name(self) -> &'static str {
        match self {
            Lang::Ru => "ru",
            Lang::En => "us",
        }
    }

    pub fn from_xkb_name(name: &str) -> Option<Lang> {
        match name {
            "us" | "en" => Some(Lang::En),
            "ru" => Some(Lang::Ru),
            _ => None,
        }
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Lang::Ru => "ru",
            Lang::En => "en",
        })
    }
}

// ---- Linux input-event-codes we care about (subset of <linux/input-event-codes.h>) ----

pub const KEY_BACKSPACE: u16 = 14;
pub const KEY_TAB: u16 = 15;
pub const KEY_ENTER: u16 = 28;
pub const KEY_SPACE: u16 = 57;
pub const KEY_ESC: u16 = 1;
/// `v` — the paste shortcut key (`Ctrl+V`), recognized for clipboard-paste conversion.
pub const KEY_V: u16 = 47;

pub const KEY_LEFTSHIFT: u16 = 42;
pub const KEY_RIGHTSHIFT: u16 = 54;
pub const KEY_LEFTCTRL: u16 = 29;
pub const KEY_RIGHTCTRL: u16 = 97;
pub const KEY_LEFTALT: u16 = 56;
pub const KEY_RIGHTALT: u16 = 100;
pub const KEY_LEFTMETA: u16 = 125;
pub const KEY_RIGHTMETA: u16 = 126;
pub const KEY_CAPSLOCK: u16 = 58;

// Navigation / editing keys that must invalidate the trusted typing context.
const NAV_KEYS: &[u16] = &[
    103, // UP
    105, // LEFT
    106, // RIGHT
    108, // DOWN
    102, // HOME
    107, // END
    104, // PAGEUP
    109, // PAGEDOWN
    110, // INSERT
    111, // DELETE
];

/// One row of the layout table: a physical key and what it yields in each layout.
struct Key {
    code: u16,
    en_lo: char,
    en_hi: char,
    ru_lo: char,
    ru_hi: char,
}

const fn k(code: u16, en_lo: char, en_hi: char, ru_lo: char, ru_hi: char) -> Key {
    Key { code, en_lo, en_hi, ru_lo, ru_hi }
}

// Physical positions across the main alphanumeric block, with US-QWERTY and RU-ЙЦУКЕН values.
#[rustfmt::skip]
const KEYS: &[Key] = &[
    // number row
    k(2,'1','!','1','!'), k(3,'2','@','2','"'), k(4,'3','#','3','№'), k(5,'4','$','4',';'),
    k(6,'5','%','5','%'), k(7,'6','^','6',':'), k(8,'7','&','7','?'), k(9,'8','*','8','*'),
    k(10,'9','(','9','('), k(11,'0',')','0',')'), k(12,'-','_','-','_'), k(13,'=','+','=','+'),
    // top letter row
    k(16,'q','Q','й','Й'), k(17,'w','W','ц','Ц'), k(18,'e','E','у','У'), k(19,'r','R','к','К'),
    k(20,'t','T','е','Е'), k(21,'y','Y','н','Н'), k(22,'u','U','г','Г'), k(23,'i','I','ш','Ш'),
    k(24,'o','O','щ','Щ'), k(25,'p','P','з','З'), k(26,'[','{','х','Х'), k(27,']','}','ъ','Ъ'),
    // home row
    k(30,'a','A','ф','Ф'), k(31,'s','S','ы','Ы'), k(32,'d','D','в','В'), k(33,'f','F','а','А'),
    k(34,'g','G','п','П'), k(35,'h','H','р','Р'), k(36,'j','J','о','О'), k(37,'k','K','л','Л'),
    k(38,'l','L','д','Д'), k(39,';',':','ж','Ж'), k(40,'\'','"','э','Э'), k(41,'`','~','ё','Ё'),
    // bottom row
    k(44,'z','Z','я','Я'), k(45,'x','X','ч','Ч'), k(46,'c','C','с','С'), k(47,'v','V','м','М'),
    k(48,'b','B','и','И'), k(49,'n','N','т','Т'), k(50,'m','M','ь','Ь'), k(51,',','<','б','Б'),
    k(52,'.','>','ю','Ю'), k(53,'/','?','.',','), k(43,'\\','|','\\','/'),
];

fn lookup(code: u16) -> Option<&'static Key> {
    KEYS.iter().find(|k| k.code == code)
}

/// The character a physical key produces in `lang` given `shift`. `None` for non-printing keys.
pub fn char_for(code: u16, shift: bool, lang: Lang) -> Option<char> {
    let key = lookup(code)?;
    Some(match (lang, shift) {
        (Lang::En, false) => key.en_lo,
        (Lang::En, true) => key.en_hi,
        (Lang::Ru, false) => key.ru_lo,
        (Lang::Ru, true) => key.ru_hi,
    })
}

/// The string a separator key produces, so it can be appended to a pasted correction (pasting
/// "how " in one shot avoids a race where a separately-typed space lands before the paste).
pub fn separator_str(code: u16, shift: bool, lang: Lang) -> String {
    match code {
        KEY_SPACE => " ".to_string(),
        KEY_TAB => "\t".to_string(),
        KEY_ENTER => "\n".to_string(),
        _ => char_for(code, shift, lang).map(|c| c.to_string()).unwrap_or_default(),
    }
}

/// Reverse lookup: which physical key + shift produces `c` in `lang`. Used by the
/// transliterator to convert arbitrary text key-for-key into the other layout.
pub fn find_key(c: char, lang: Lang) -> Option<(u16, bool)> {
    for key in KEYS {
        let (lo, hi) = match lang {
            Lang::En => (key.en_lo, key.en_hi),
            Lang::Ru => (key.ru_lo, key.ru_hi),
        };
        if c == lo {
            return Some((key.code, false));
        }
        if c == hi {
            return Some((key.code, true));
        }
    }
    None
}

/// Modifier state at the moment a key is pressed.
#[derive(Clone, Copy, Debug, Default)]
pub struct Mods {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Mods {
    /// True when a layout-shortcut modifier (Ctrl/Alt/Meta) is held — i.e. this is a command
    /// or navigation chord, not ordinary text entry.
    pub fn is_chord(self) -> bool {
        self.ctrl || self.alt || self.meta
    }

    pub fn is_empty(self) -> bool {
        !self.shift && !self.ctrl && !self.alt && !self.meta
    }

    pub fn union(self, o: Mods) -> Mods {
        Mods {
            shift: self.shift || o.shift,
            ctrl: self.ctrl || o.ctrl,
            alt: self.alt || o.alt,
            meta: self.meta || o.meta,
        }
    }
}

/// Tokenizer-level meaning of a key press, independent of how it was captured.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyEvent {
    /// A letter that extends the current word.
    Letter { code: u16, shift: bool, cur: char, alt: char },
    /// Whitespace or punctuation that ends the current word.
    Separator,
    /// Backspace — trims the last char while keeping the cursor at the word end.
    Backspace,
    /// Navigation/editing/chord/focus change — the typing context can no longer be trusted.
    Invalidate,
    /// Pure modifier, caps-lock, F-keys, etc. — no effect on the buffer.
    Ignore,
}

/// Classify a key-down into a [`KeyEvent`] for the given active layout and modifier state.
///
/// This is the single pure entry point the capture layer feeds the buffer, which makes the
/// whole tokenizer + trusted-context policy unit-testable without any devices.
pub fn classify(code: u16, mods: Mods, lang: Lang) -> KeyEvent {
    match code {
        KEY_LEFTSHIFT | KEY_RIGHTSHIFT | KEY_LEFTCTRL | KEY_RIGHTCTRL | KEY_LEFTALT
        | KEY_RIGHTALT | KEY_LEFTMETA | KEY_RIGHTMETA | KEY_CAPSLOCK => KeyEvent::Ignore,
        KEY_BACKSPACE => KeyEvent::Backspace,
        KEY_SPACE | KEY_TAB | KEY_ENTER => KeyEvent::Separator,
        KEY_ESC => KeyEvent::Invalidate,
        c if NAV_KEYS.contains(&c) => KeyEvent::Invalidate,
        _ => {
            // A Ctrl/Alt/Meta chord is a shortcut (navigation, paste, select-all, …): it does
            // not enter trustworthy text, so it breaks the context.
            if mods.is_chord() {
                return KeyEvent::Invalidate;
            }
            match char_for(code, mods.shift, lang) {
                Some(cur) => {
                    let alt = char_for(code, mods.shift, lang.other());
                    // Part of a word if it's a letter in *either* layout. This catches Russian
                    // letters that sit on English-punctuation keys (б,ж,э,ю,х,ъ,ё) when EN is
                    // active, so words like "будем"/"ужин" can be detected and converted.
                    if cur.is_alphabetic() || alt.is_some_and(|a| a.is_alphabetic()) {
                        KeyEvent::Letter { code, shift: mods.shift, cur, alt: alt.unwrap_or(cur) }
                    } else {
                        // Punctuation/digit in both layouts → ends the word.
                        KeyEvent::Separator
                    }
                }
                // Unknown / unmapped key: treat as harmless.
                None => KeyEvent::Ignore,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn privet_maps_across_layouts() {
        // Physical keys for "ghbdtn" in US == "привет" in RU.
        let codes = [34u16, 35, 48, 32, 20, 49]; // g h b d t n
        let en: String = codes.iter().map(|&c| char_for(c, false, Lang::En).unwrap()).collect();
        let ru: String = codes.iter().map(|&c| char_for(c, false, Lang::Ru).unwrap()).collect();
        assert_eq!(en, "ghbdtn");
        assert_eq!(ru, "привет");
    }

    #[test]
    fn hello_maps_across_layouts() {
        // "руддщ" typed on RU keys == "hello" on EN keys.
        let codes = [35u16, 18, 38, 38, 24]; // h e l l o
        let ru: String = codes.iter().map(|&c| char_for(c, false, Lang::Ru).unwrap()).collect();
        let en: String = codes.iter().map(|&c| char_for(c, false, Lang::En).unwrap()).collect();
        assert_eq!(ru, "руддщ");
        assert_eq!(en, "hello");
    }

    #[test]
    fn shift_gives_uppercase() {
        assert_eq!(char_for(33, true, Lang::Ru), Some('А'));
        assert_eq!(char_for(33, false, Lang::Ru), Some('а'));
        assert_eq!(char_for(33, true, Lang::En), Some('F'));
    }

    #[test]
    fn classify_letter_separator_backspace() {
        let m = Mods::default();
        assert!(matches!(classify(20, m, Lang::En), KeyEvent::Letter { cur: 't', .. }));
        assert_eq!(classify(KEY_SPACE, m, Lang::En), KeyEvent::Separator);
        assert_eq!(classify(KEY_BACKSPACE, m, Lang::En), KeyEvent::Backspace);
        // digit row → separator (not part of a word)
        assert_eq!(classify(2, m, Lang::En), KeyEvent::Separator);
    }

    #[test]
    fn punctuation_key_is_a_letter_when_alpha_in_other_layout() {
        // In EN, the comma key produces ',' but is 'б' in RU — it must extend a word so words
        // like "будем" (which starts with б) are detected, not split on the comma.
        match classify(51, Mods::default(), Lang::En) {
            KeyEvent::Letter { cur, alt, .. } => {
                assert_eq!(cur, ',');
                assert_eq!(alt, 'б');
            }
            other => panic!("expected Letter, got {other:?}"),
        }
        // A digit is punctuation in both layouts → still a separator.
        assert_eq!(classify(3, Mods::default(), Lang::En), KeyEvent::Separator);
    }

    #[test]
    fn classify_invalidates_on_nav_and_chords() {
        assert_eq!(classify(105, Mods::default(), Lang::En), KeyEvent::Invalidate); // LEFT
        let ctrl = Mods { ctrl: true, ..Default::default() };
        assert_eq!(classify(30, ctrl, Lang::En), KeyEvent::Invalidate); // Ctrl+A
    }
}

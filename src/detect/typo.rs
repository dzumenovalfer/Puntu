//! Typo correction: the one common word a single slip of the finger away.
//!
//! This is a different kind of rewrite from the layout decision, and a riskier one. There the
//! two candidates are given — the same keys read through the other layout — and the question
//! is only which of them was meant. Here the candidate is *found*, so a mistake is not "the
//! wrong one of two readings" but a word the user never typed. Everything below is therefore
//! built around saying **no**:
//!
//! * only a word no dictionary knows (and the user has not listed as an exception);
//! * only Russian, and only with the big dictionary loaded — a 40 000-word list knows too few
//!   inflected forms for "unknown" to mean "mistyped";
//! * only a *mechanical* slip ([`Slip`]): two letters swapped, a letter doubled, a key not
//!   pressed, a neighbouring key hit instead of the right one or along with it. `ревью` →
//!   `ревю` is not one of those (the `ь` is two keys away from anything next to it);
//! * only into a **common** word — one from the frequency list, not any of the 1.5 million
//!   forms of the big dictionary. People mistype the words they type, and a perfectly good
//!   word the dictionary happens not to know (`коммит`, `токен`) is usually one "slip" away
//!   from some form nobody uses (`комми`, `токе`);
//! * only when exactly **one** common word is a slip away, counting every kind of slip;
//! * and only when the word is long enough for that kind of slip to mean something
//!   ([`Slip::min_len`]).
//!
//! The last two were tuned on data rather than taste. Against 7 600 synthetic typos of
//! frequent words and 236 real words the dictionary lacks (IT slang, chat forms, names), a
//! single floor of 5 letters for everything fixed 88 % of the typos and rewrote 7 % of the
//! real words; these per-slip floors fix 67 % and rewrite none of them, with no typo ever
//! turned into the wrong word. For a feature that changes what the user wrote, that is the
//! right end of the trade.
//!
//! The caller adds the rest: never in a terminal, never for a fragment of a word, a capital
//! only at the start of a sentence — and the result stays in the preedit, where one flip
//! takes it back and teaches the engine not to do it again.

use std::collections::BTreeMap;

use crate::keymap::{char_for, find_key, Lang};

/// Shortest word any slip is looked for in — the smallest of the [`Slip::min_len`] floors.
pub const MIN_LEN: usize = 5;

const RU_LETTERS: &str = "абвгдеёжзийклмнопрстуфхцчшщъыьэюя";

/// The ways one finger can go wrong.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Slip {
    /// Two adjacent letters in the wrong order (`првиет`).
    Swapped,
    /// A key pressed twice (`приввет`).
    Doubled,
    /// A key not pressed at all (`привт`).
    Missing,
    /// A neighbouring key instead of the right one (`привеь`).
    Neighbour,
    /// A neighbouring key *along with* the right one (`приывет`).
    Stray,
}

impl Slip {
    /// How long the typed word has to be before this slip is believed.
    ///
    /// A swap and a doubled letter are unmistakable: almost no real word is one of those
    /// away from another. The other three are how real words differ from each other
    /// (`кодить`/`водить`, `стартап`/`старта`), and only stop being that as words get long.
    fn min_len(self) -> usize {
        match self {
            Slip::Swapped | Slip::Doubled => 5,
            Slip::Missing => 6,
            Slip::Neighbour => 7,
            Slip::Stray => 8,
        }
    }
}

/// The three letter rows, top to bottom, as (first, last) evdev key codes. A keyboard is
/// staggered — each row sits half a key to the right of the one above, so `ф` is under the
/// gap between `й` and `ц` — which is what the "above" and "below" arithmetic in
/// [`neighbours`] encodes.
const ROWS: [(u16, u16); 3] = [(16, 27), (30, 40), (44, 52)];

/// The Russian letters on the keys physically next to the one that types `c` — where a finger
/// aiming at `c` lands instead. Empty for a character that is not on a letter key (`ё` lives
/// in the corner next to `1`, with no letter beside it).
pub fn neighbours(c: char) -> Vec<char> {
    let Some((code, _)) = find_key(c, Lang::Ru) else {
        return Vec::new();
    };
    let Some(row) = ROWS.iter().position(|&(lo, hi)| (lo..=hi).contains(&code)) else {
        return Vec::new();
    };
    let col = (code - ROWS[row].0) as i32;
    let mut around: Vec<(usize, i32)> = vec![(row, col - 1), (row, col + 1)];
    // The row above: this key sits under the gap between the keys at `col` and `col + 1`.
    if row > 0 {
        around.push((row - 1, col));
        around.push((row - 1, col + 1));
    }
    // The row below: it sits over the gap between `col - 1` and `col`.
    if row + 1 < ROWS.len() {
        around.push((row + 1, col - 1));
        around.push((row + 1, col));
    }
    around
        .into_iter()
        .filter_map(|(r, c)| {
            let (lo, hi) = ROWS[r];
            let code = lo as i32 + c;
            (c >= 0 && code <= hi as i32).then_some(code as u16)
        })
        .filter_map(|code| char_for(code, false, Lang::Ru))
        .filter(|n| n.is_alphabetic())
        .collect()
}

/// Every string that `word` (lower-case letters) could be a one-slip typo **of**, each with
/// the shortest length of `word` at which that reading is believed. A string reachable by
/// more than one kind of slip keeps the most lenient floor.
fn slips(word: &[char]) -> BTreeMap<String, usize> {
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    let n = word.len();
    let mut add = |chars: Vec<char>, slip: Slip| {
        let floor = out.entry(chars.into_iter().collect()).or_insert(usize::MAX);
        *floor = (*floor).min(slip.min_len());
    };
    for i in 0..n {
        if i + 1 < n && word[i] != word[i + 1] {
            let mut c = word.to_vec();
            c.swap(i, i + 1);
            add(c, Slip::Swapped);
        }
        for nb in neighbours(word[i]) {
            let mut c = word.to_vec();
            c[i] = nb;
            add(c, Slip::Neighbour);
        }
        // Is this letter one too many? It is if it repeats the letter beside it, or is a
        // keyboard neighbour of it.
        let beside = [i.checked_sub(1).map(|j| word[j]), word.get(i + 1).copied()];
        let extra = if beside.into_iter().flatten().any(|b| b == word[i]) {
            Some(Slip::Doubled)
        } else if beside.into_iter().flatten().any(|b| neighbours(b).contains(&word[i])) {
            Some(Slip::Stray)
        } else {
            None
        };
        if let Some(slip) = extra {
            let mut c = word.to_vec();
            c.remove(i);
            add(c, slip);
        }
    }
    // A letter is missing somewhere — any letter: a key that was not pressed can be any key.
    for i in 0..=n {
        for l in RU_LETTERS.chars() {
            let mut c = word.to_vec();
            c.insert(i, l);
            add(c, Slip::Missing);
        }
    }
    out.remove(&word.iter().collect::<String>());
    out
}

/// The correction for `word`, if there is exactly one.
///
/// `word` is taken as typed: all lower case, or Capitalized when `capitalized_ok` (the caller
/// knows it starts a sentence — otherwise a capital letter marks a name, and names are the
/// one kind of unknown word that is certainly not a typo). Anything else — mixed case, a
/// digit, a Latin letter, punctuation — is left alone.
///
/// `known`: is this a word at all (any dictionary, or the user's own lists)? `common`: is it
/// a word people actually type — the only kind worth correcting *to*? `refused`: has the
/// user said to leave it alone?
pub fn fix(
    word: &str,
    capitalized_ok: bool,
    known: impl Fn(&str) -> bool,
    common: impl Fn(&str) -> bool,
    refused: impl Fn(&str) -> bool,
) -> Option<String> {
    let mut chars = word.chars();
    let first = chars.next()?;
    let capitalized = first.is_uppercase();
    if (capitalized && !capitalized_ok) || chars.any(|c| c.is_uppercase()) {
        return None;
    }
    let lower: Vec<char> = word.to_lowercase().chars().collect();
    if lower.len() < MIN_LEN || !lower.iter().all(|c| RU_LETTERS.contains(*c)) {
        return None;
    }
    let lower_str: String = lower.iter().collect();
    if known(&lower_str) || refused(&lower_str) {
        return None;
    }
    // EVERY common word a slip away counts towards "exactly one", whatever its floor: a
    // slip too short to be believed still makes the word ambiguous. (Filtering by floor
    // first is how a typo gets "fixed" into a different word than the one that was meant.)
    let mut hits = slips(&lower).into_iter().filter(|(c, _)| common(c));
    let (only, floor) = hits.next()?;
    if hits.next().is_some() || lower.len() < floor {
        return None;
    }
    Some(if capitalized {
        let mut c = only.chars();
        c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
    } else {
        only
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dict<'a>(words: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |w| words.contains(&w)
    }
    fn never(_: &str) -> bool {
        false
    }
    /// `fix` with one word list serving as both the dictionary and the common words.
    fn fix1(word: &str, capitalized_ok: bool, words: &[&str]) -> Option<String> {
        fix(word, capitalized_ok, dict(words), dict(words), never)
    }

    #[test]
    fn neighbours_are_the_physically_adjacent_keys() {
        let set = |c| {
            let mut v = neighbours(c);
            v.sort();
            v.into_iter().collect::<String>()
        };
        // `а` (the F key): `в` and `п` beside it, `к`/`е` above, `с`/`м` below.
        assert_eq!(set('а'), "векмпс");
        // A corner key has fewer.
        assert_eq!(set('й'), "фц");
        assert_eq!(set('я'), "фчы");
        // Bottom row, right end: `ю` has `б` beside it and `д`/`ж` above.
        assert_eq!(set('ю'), "бдж");
        // `ё` sits apart from the letter block; a non-letter has no key at all.
        assert_eq!(set('ё'), "");
        assert_eq!(set('1'), "");
    }

    #[test]
    fn each_kind_of_slip_is_found() {
        let words = ["клавиатура"];
        for (typo, what) in [
            ("клавитаура", "two letters swapped"),
            ("клаввиатура", "a doubled letter"),
            ("клавитура", "a missing letter"),
            ("клавиатурв", "a neighbouring key instead (а → в)"),
            ("клаывиатура", "a neighbouring key as well (ы beside в)"),
        ] {
            assert_eq!(fix1(typo, false, &words).as_deref(), Some("клавиатура"), "{what}");
        }
        // Case follows what was typed — when a capital is allowed at all.
        assert_eq!(fix1("Клавитаура", true, &words).as_deref(), Some("Клавиатура"));
    }

    #[test]
    fn a_short_word_is_believed_only_for_the_unmistakable_slips() {
        let words = ["привет", "человек", "сегодня"];
        // Six letters: a swap or a doubled letter is enough…
        assert_eq!(fix1("првиет", false, &words).as_deref(), Some("привет"));
        assert_eq!(fix1("приввет", false, &words).as_deref(), Some("привет"));
        // …a neighbouring key is not (seven letters and up)…
        assert_eq!(fix1("привеь", false, &words), None);
        assert_eq!(fix1("человеу", false, &words).as_deref(), Some("человек"));
        // …a missing letter needs six letters left…
        assert_eq!(fix1("приве", false, &words), None);
        assert_eq!(fix1("челвек", false, &words).as_deref(), Some("человек"));
        // …and a stray neighbour eight.
        assert_eq!(fix1("приывет", false, &words), None);
        assert_eq!(fix1("сенгодня", false, &words).as_deref(), Some("сегодня"));
        // Under five letters nothing is looked for at all.
        assert_eq!(fix1("приз", false, &["приз", "прив"]), None);
        assert_eq!(fix1("први", false, &["прив"]), None);
    }

    #[test]
    fn what_is_not_a_slip_is_left_alone() {
        let words = ["привет", "ревю", "репозитарий"];
        // The word itself is fine.
        assert_eq!(fix1("привет", false, &words), None);
        // `ревью` is not in the dictionary and `ревю` is — but the extra `ь` is two keys
        // away from everything around it, so this is a different word, not a typo.
        assert_eq!(fix1("ревью", false, &words), None);
        // `о` and `а` are far apart on the keyboard: a spelling variant, not a slip.
        assert_eq!(fix1("репозиторий", false, &words), None);
        // Two slips.
        assert_eq!(fix1("првите", false, &words), None);
    }

    #[test]
    fn two_possible_words_is_no_answer() {
        // `столиа` is one missing letter away from both.
        assert_eq!(fix1("столиа", false, &["столица", "столика"]), None);
        assert_eq!(fix1("столиа", false, &["столица"]).as_deref(), Some("столица"));
        // Different kinds of slip count together: a neighbouring key from one word, a
        // missing letter from the other.
        assert_eq!(fix1("коронк", false, &["корона", "коронки"]), None);
        // A slip too short to be believed still makes the answer ambiguous: `кодить` is one
        // neighbouring key from `водить` (not believed at six letters) and one swap from
        // `коидть` — were that a word, the swap alone must not win.
        assert_eq!(fix1("кодить", false, &["водить", "коидть"]), None);
        assert_eq!(fix1("кодить", false, &["водить"]), None);
        assert_eq!(fix1("кодить", false, &["коидть"]).as_deref(), Some("коидть"));
    }

    #[test]
    fn only_a_common_word_is_worth_correcting_to() {
        // Every dictionary knows `комми` and `старта`; nobody types them. Words the
        // dictionary does NOT know — `коммит`, `стартапы` — are one "stray key" away from
        // such forms, and with any dictionary form as a candidate they were rewritten.
        let known = dict(&["комми", "стартап", "привет", "привес"]);
        let common = dict(&["привет"]);
        assert_eq!(fix("коммит", false, &known, &common, never), None);
        assert_eq!(fix("стартапы", false, &known, &common, never), None);
        // A typo of a common word is still fixed.
        assert_eq!(fix("првиет", false, &known, &common, never).as_deref(), Some("привет"));
        // A word that is merely rare is still a word: it is never "corrected" away.
        assert_eq!(fix("привес", false, &known, &common, never), None);
    }

    #[test]
    fn names_mixed_case_and_non_russian_are_not_touched() {
        let words = ["привет", "hello"];
        // A capital letter mid-sentence marks a name.
        assert_eq!(fix1("Првиет", false, &words), None);
        // Mixed case, all caps: typed that way on purpose.
        assert_eq!(fix1("прВиет", true, &words), None);
        assert_eq!(fix1("ПРВИЕТ", true, &words), None);
        // Latin, digits, punctuation.
        assert_eq!(fix1("hlelo", true, &words), None);
        assert_eq!(fix1("првиет1", true, &words), None);
        assert_eq!(fix1("при-вет", true, &words), None);
        assert_eq!(fix1("", true, &words), None);
    }

    #[test]
    fn a_refused_word_stays() {
        let known = dict(&["привет"]);
        // The user flipped this correction back once: it is on their list now.
        assert_eq!(fix("првиет", false, &known, &known, |w| w == "првиет"), None);
    }
}

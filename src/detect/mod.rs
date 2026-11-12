//! Detection: decide whether a finished word was typed in the wrong layout.

pub mod ngram;
pub mod translit;
pub mod typo;
pub mod userdict;

use std::collections::HashSet;
use std::sync::Arc;

use crate::buffer::CompletedWord;
use crate::config::DetectConfig;
use crate::keymap::Lang;
use ngram::Model;
use userdict::UserDict;

/// What to do with a finished word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Leave it alone.
    Leave,
    /// Convert to the other layout/language.
    Convert { to: Lang },
}

/// Per-language trigram models **and** the exact word sets they were trained on (used as a
/// dictionary: an exact match converts/leaves confidently, which the n-gram alone can't do for
/// inflected forms like `кнопку`).
#[derive(Clone, Default)]
pub struct Models {
    pub ru: Model,
    pub en: Model,
    ru_words: HashSet<String>,
    en_words: HashSet<String>,
    /// The two-letter words of each language that people actually type — see
    /// [`Models::is_common_short`].
    ru_short: HashSet<String>,
    en_short: HashSet<String>,
    /// Large optional Russian dictionary (1M+ word forms) as a compact FST for exact match.
    ru_big: Option<Arc<fst::Set<Vec<u8>>>>,
}

/// How deep into the Russian word list a two-letter entry may sit and still count as a word.
///
/// The list is ordered by frequency, and for two-letter entries that order is the whole
/// story: the real ones (не, ты, на, он, мы, вы, да, но, за, бы, же, из, ну, по, то, от, их,
/// её, до, со, ли, ни, об, ей, во, им, уж, ко, те …) all sit in the first few hundred lines,
/// and what follows is interjections and debris from the corpus it was built from (ть, дл,
/// чт, нм, лш, еы …) — 244 "words" out of 1089 possible letter pairs. The big dictionary adds
/// 418 more (аб, ав, вг, гг …). Taken at face value they made every second two-letter Latin
/// token "the wrong-layout form of a Russian word": `TV` → «ЕМ», `16 GB` → «16 ПИ», `10 km` →
/// «10 ль», `UK` → «ГЛ», and the `st`/`nd`/`rd`/`th` of every ordinal.
const COMMON_SHORT_RANK: usize = 1000;

impl Models {
    pub fn model(&self, lang: Lang) -> &Model {
        match lang {
            Lang::Ru => &self.ru,
            Lang::En => &self.en,
        }
    }

    /// Is `word` an exact entry in the dictionary for `lang` (built-in set, or the big FST)?
    pub fn is_word(&self, word: &str, lang: Lang) -> bool {
        let w = word.to_lowercase();
        let set = match lang {
            Lang::Ru => &self.ru_words,
            Lang::En => &self.en_words,
        };
        if set.contains(&w) {
            return true;
        }
        if lang == Lang::Ru {
            if let Some(big) = &self.ru_big {
                return big.contains(w.as_bytes());
            }
        }
        false
    }

    /// Is `word` in the bundled frequency list of `lang` (or among the words the user taught
    /// through `words.<lang>.txt`)? Unlike [`Self::is_word`] this leaves the big dictionary
    /// out: its 1.5 million forms are mostly words nobody types.
    pub fn is_common_word(&self, word: &str, lang: Lang) -> bool {
        let w = word.to_lowercase();
        match lang {
            Lang::Ru => self.ru_words.contains(&w),
            Lang::En => self.en_words.contains(&w),
        }
    }

    /// Is the big Russian dictionary loaded? Typo correction needs it: with only the bundled
    /// 40 000-word list most inflected forms are "unknown", and unknown stops meaning
    /// mistyped.
    pub fn has_big_dictionary(&self) -> bool {
        self.ru_big.is_some()
    }

    /// Is `word` one of the two-letter words of `lang` that people actually type?
    ///
    /// Two letters are too few for anything but a list: the trigram model sees three
    /// trigrams, two of them padding, and "is in a dictionary" is true of a third of all
    /// letter pairs (see [`COMMON_SHORT_RANK`]). So a two-letter word converts only when its
    /// other-layout reading is on this list — the same idea as the hand-written whitelist
    /// for single letters, built from the word lists instead.
    pub fn is_common_short(&self, word: &str, lang: Lang) -> bool {
        let w = word.to_lowercase();
        match lang {
            Lang::Ru => self.ru_short.contains(&w),
            Lang::En => self.en_short.contains(&w),
        }
    }

    pub fn from_words<I, S>(ru: I, en: I) -> Models
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let ru: Vec<String> = ru.into_iter().map(|s| s.as_ref().to_lowercase()).collect();
        let en: Vec<String> = en.into_iter().map(|s| s.as_ref().to_lowercase()).collect();
        let two_letters = |w: &&String| w.chars().count() == 2;
        Models {
            ru: Model::from_words(&ru),
            en: Model::from_words(&en),
            // The Russian list is frequency-ordered, so rank separates words from debris.
            ru_short: ru.iter().take(COMMON_SHORT_RANK).filter(two_letters).cloned().collect(),
            // The English one is alphabetical — rank says nothing — but its two-letter
            // entries are few and all real (am, an, as, at, be, by, do, go …).
            en_short: en.iter().filter(two_letters).cloned().collect(),
            ru_words: ru.into_iter().collect(),
            en_words: en.into_iter().collect(),
            ru_big: None,
        }
    }

    /// Build models from the word lists bundled in `data/` at compile time.
    pub fn builtin() -> Models {
        Models::from_words(
            builtin_words(include_str!("../../data/ru.words"), None),
            builtin_words(include_str!("../../data/en.words"), None),
        )
    }

    /// These models with `words` as the big Russian dictionary — what [`Models::load`] reads
    /// from `russian.fst`, built in memory. For tests, and for anything else that wants the
    /// dictionary-dependent behaviour (typo correction) without a file on disk.
    pub fn with_big_dictionary<I, S>(mut self, words: I) -> Models
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut words: Vec<String> = words.into_iter().map(|w| w.as_ref().to_lowercase()).collect();
        words.sort();
        words.dedup();
        // Sorted and deduplicated above, which is all `from_iter` asks for.
        self.ru_big = fst::Set::from_iter(words).ok().map(Arc::new);
        self
    }

    /// Built-in word lists **plus** the user's recognized-words files (`words.{ru,en}.txt` in
    /// `dir`). Adding e.g. a service name like `tiktok` makes its wrong-layout form convert.
    pub fn load(dir: &std::path::Path) -> Models {
        let mut m = Models::from_words(
            builtin_words(include_str!("../../data/ru.words"), Some(&dir.join("words.ru.txt"))),
            builtin_words(include_str!("../../data/en.words"), Some(&dir.join("words.en.txt"))),
        );
        m.ru_big = load_fst(&dir.join("russian.fst"));
        if m.ru_big.is_some() {
            tracing::info!("loaded big Russian dictionary (russian.fst)");
        }
        m
    }
}

/// Load a prebuilt FST set (e.g. the big Russian dictionary) if present.
fn load_fst(path: &std::path::Path) -> Option<Arc<fst::Set<Vec<u8>>>> {
    let bytes = std::fs::read(path).ok()?;
    fst::Set::new(bytes).ok().map(Arc::new)
}

/// Built-in word list lines, optionally extended with a user file (one word per line, `#`
/// comments allowed).
fn builtin_words(builtin: &str, user: Option<&std::path::Path>) -> Vec<String> {
    let mut words: Vec<String> =
        builtin.lines().map(|l| l.trim()).filter(|l| !l.is_empty()).map(String::from).collect();
    if let Some(path) = user {
        if let Ok(text) = std::fs::read_to_string(path) {
            words.extend(
                text.lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty() && !l.starts_with('#'))
                    .map(|l| l.to_lowercase()),
            );
        }
    }
    words
}

/// The detector: models + tunable thresholds.
pub struct Detector {
    models: Models,
    cfg: DetectConfig,
}

impl Detector {
    pub fn new(models: Models, cfg: DetectConfig) -> Detector {
        Detector { models, cfg }
    }

    pub fn set_config(&mut self, cfg: DetectConfig) {
        self.cfg = cfg;
    }


    pub fn set_models(&mut self, models: Models) {
        self.models = models;
    }

    /// Whether `word` is a known dictionary word in `lang`. Thin pub wrapper over
    /// [`Models::is_word`] for callers that hold a `&Detector` (e.g. the IBus engine,
    /// which uses it to decide between EN abbreviations and RU translations in
    /// DirectRussian mode).
    pub fn is_known_word(&self, word: &str, lang: Lang) -> bool {
        self.models.is_word(word, lang)
    }

    /// The word `word` is a one-slip typo of, if there is exactly one — see [`typo::fix`]
    /// for what counts and for everything that makes it say no. `word` is the Russian text
    /// about to be shown; `capitalized_ok` says whether a leading capital may be a sentence
    /// start rather than a name.
    ///
    /// Besides the dictionaries, the user's own lists count: a word they taught is a word,
    /// and one they put on a never-correct list — which is what flipping a correction back
    /// does — is left alone for good.
    pub fn typo_fix(&self, word: &str, capitalized_ok: bool, dict: &UserDict) -> Option<String> {
        if !self.models.has_big_dictionary() {
            return None;
        }
        typo::fix(
            word,
            capitalized_ok,
            |w| self.models.is_word(w, Lang::Ru) || dict.is_recognized(w, Lang::Ru),
            |w| self.models.is_common_word(w, Lang::Ru) || dict.is_recognized(w, Lang::Ru),
            |w| dict.is_exception(w, Lang::Ru),
        )
    }

    /// Decide what to do with a finished word. Only ever proposes a conversion in a trusted
    /// context (see [`crate::buffer`]); the force list is the one exception, since the user
    /// explicitly asked for those to always convert.
    pub fn decide(&self, word: &CompletedWord, dict: &UserDict) -> Decision {
        let cur_lang = word.lang;
        let other = cur_lang.other();

        // Force list wins regardless of trust/score.
        if dict.is_force(&word.cur, cur_lang) {
            return Decision::Convert { to: other };
        }

        // Everything below is auto-detection, which requires a trusted context.
        if !word.trusted {
            return Decision::Leave;
        }
        // Never touch exceptions.
        if dict.is_exception(&word.cur, cur_lang) {
            return Decision::Leave;
        }
        // Command-shaped tokens (paths, URLs, version strings, emails) — only bail when BOTH
        // layout readings look command-shaped. A wrong-layout word like `gm.n` (the EN rendering
        // of `пьют`: `.` is the key for `ю` in RU) trips the heuristic on `cur` because of the
        // dot, but its `alt` is plain Cyrillic letters and clearly a real word. Bailing on `cur`
        // alone systematically blocked conversion for any RU word containing the punctuation
        // keys (`,`=б, `.`=ю, `;`=ж, `'`=э, `[`=х, `]`=ъ, `` ` ``=ё) — which is most of them.
        if userdict::is_command_context(&word.cur) && userdict::is_command_context(&word.alt) {
            return Decision::Leave;
        }

        // Recognized words (e.g. learned service names): leave if the typed form is one; convert
        // confidently if the other-layout reading is one.
        if dict.is_recognized(&word.cur, cur_lang) {
            return Decision::Leave;
        }
        if dict.is_recognized(&word.alt, other) {
            return Decision::Convert { to: other };
        }

        // Built-in dictionary exact match: leave a real word typed as-is; convert when the
        // other-layout reading is a real word. Catches inflected forms (кнопку) the n-gram is
        // unsure about, while gibberish (тотпка, not in the dictionary) falls through to scoring.
        //
        // We check both the raw token AND its letters-only form, so a trailing `.` or punctuation
        // key in the middle (`xtcnm.` → `честью.`) doesn't prevent matching the real word.
        let cur_letters: String = word.cur.chars().filter(|c| c.is_alphabetic()).collect();
        let alt_letters: String = word.alt.chars().filter(|c| c.is_alphabetic()).collect();
        if self.models.is_word(&word.cur, cur_lang)
            || (!cur_letters.is_empty() && cur_letters != word.cur && self.models.is_word(&cur_letters, cur_lang))
        {
            return Decision::Leave;
        }
        let len = word.cur.chars().count();
        // Single-letter words: the n-gram model is unreliable on one char, so use a tiny
        // whitelist of real one-letter words per language. Convert only when the typed letter
        // isn't a word in the current language but its counterpart is in the other.
        if len == 1 {
            return decide_single(word, cur_lang, other, &self.models);
        }
        // Two-letter words: a list again, for the same reason — see `is_common_short`. This
        // comes BEFORE the dictionary match below on purpose: for two letters, "the other
        // reading is in a dictionary" is true of half of everything one can type.
        if len == 2 {
            return if self.models.is_common_short(&word.alt, other) {
                Decision::Convert { to: other }
            } else {
                Decision::Leave
            };
        }
        if self.models.is_word(&word.alt, other)
            || (!alt_letters.is_empty() && alt_letters != word.alt && self.models.is_word(&alt_letters, other))
        {
            return Decision::Convert { to: other };
        }
        if len < self.cfg.min_word_len {
            return Decision::Leave;
        }

        let score_cur = self.models.model(cur_lang).score(&word.cur);
        let score_alt = self.models.model(other).score(&word.alt);

        let cur_has_symbols = userdict::is_command_context(&word.cur);
        let alt_has_symbols = userdict::is_command_context(&word.alt);

        // Strong asymmetric signal: `cur` contains command-shaped symbols but `alt` is clean
        // alphabetic letters. In a wrong-layout word those symbols are real Cyrillic letters
        // (`.`=ю, `,`=б, `;`=ж, `'`=э, `[`=х, `]`=ъ, `` ` ``=ё), so scoring `cur` as-is is noise:
        // the trigrams involving `,`/`.`/`;` never appear in either language model and all hit
        // the smoothing floor, which is what makes the plain delta comparison meaningless here.
        //
        // The fix is to compare the LETTERS-ONLY projections instead of loosening the gate.
        // Stripping the punctuation keys from `cur` (`yflj,yj` → `yfljyj`) leaves two strings
        // both models can score honestly, so the ordinary thresholds keep their meaning. The
        // previous "loose floor" (`alt_valid_min - 2.5` = -5.8) sat BELOW the model's own
        // unseen-trigram default (ln(0.5/33) = -4.19), so it accepted essentially any string:
        // every out-of-dictionary word followed by a `.` or `,` — a name, a brand, a technical
        // term — was rewritten into Cyrillic gibberish (`puntu.` → `згтегю`).
        if cur_has_symbols && !alt_has_symbols {
            let letters_cur = self.models.model(cur_lang).score(&cur_letters);
            let letters_alt = self.models.model(other).score(&alt_letters);
            tracing::debug!(
                "asymmetric: cur={:?}/{:?} score={:.2}, alt={:?}/{:?} score={:.2} \
                 (need>{:.2}, alt>={:.2})",
                word.cur, cur_letters, letters_cur,
                word.alt, alt_letters, letters_alt,
                self.cfg.switch_delta, self.cfg.alt_valid_min
            );
            if letters_alt - letters_cur > self.cfg.switch_delta
                && letters_alt >= self.cfg.alt_valid_min
            {
                return Decision::Convert { to: other };
            }
            return Decision::Leave;
        }

        tracing::debug!(
            "scores cur={:.2} alt={:.2} delta={:.2} (need>{:.2}, alt>={:.2})",
            score_cur,
            score_alt,
            score_alt - score_cur,
            self.cfg.switch_delta,
            self.cfg.alt_valid_min
        );

        // Switch when the other layout is both clearly better *and* plausibly a real word.
        if score_alt - score_cur > self.cfg.switch_delta && score_alt >= self.cfg.alt_valid_min {
            Decision::Convert { to: other }
        } else {
            Decision::Leave
        }
    }
}

/// Convert wrong-layout words inside arbitrary text (e.g. clipboard contents pasted with Ctrl+V)
/// to the other layout, leaving correctly-typed words, punctuation, digits and whitespace
/// untouched. Each alphabetic run is judged independently by the [`Detector`]. Returns the
/// rewritten text only if at least one word actually changed, so callers can skip a no-op.
pub fn convert_text(text: &str, detector: &Detector, dict: &UserDict) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    for (seg, is_word) in segments(text) {
        if is_word {
            if let Some(alt) = convert_word(seg, detector, dict) {
                out.push_str(&alt);
                changed = true;
                continue;
            }
        }
        out.push_str(seg);
    }
    if changed {
        Some(out)
    } else {
        None
    }
}

/// Decide a single pasted word: map its characters back to physical keys, run the detector, and
/// return the other-layout form if it should convert. `None` if it should be left as-is or has a
/// character that doesn't map to any key (so we never half-convert a word).
fn convert_word(w: &str, detector: &Detector, dict: &UserDict) -> Option<String> {
    // A lone letter inside arbitrary pasted text (e.g. the `Z` in a timestamp, the `c` in `-c`)
    // is almost never a wrong-layout word — converting it (z→я, c→с) only mangles Latin/mixed
    // text. The live single-letter path (`decide_single`) stays for actual one-key typing.
    if w.chars().count() < 2 {
        return None;
    }
    let lang = if w.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)) {
        Lang::Ru
    } else {
        Lang::En
    };
    let keys: Vec<(u16, bool)> = w.chars().filter_map(|c| crate::keymap::find_key(c, lang)).collect();
    if keys.is_empty() || keys.len() != w.chars().count() {
        return None;
    }
    let word = CompletedWord::from_keys(keys, lang, true);
    match detector.decide(&word, dict) {
        Decision::Convert { .. } => Some(word.alt),
        Decision::Leave => None,
    }
}

/// Whether `c` is part of a word, using the **same rule as the live tokenizer**
/// ([`crate::keymap::classify`]): a char belongs to a word if it is a letter in *either* layout.
/// This keeps Russian letters that sit on English-punctuation keys (`,`=б, `.`=ю, `;`=ж, `'`=э,
/// `[`=х, `]`=ъ, `` ` ``=ё) attached to a wrong-layout word — so `,kjryjn` stays one token and
/// converts to `блокнот`, instead of splitting off the `,` and yielding `,локнот`. Punctuation
/// that is punctuation in both layouts (digits, `?`, `!`, `/`…) is still a separator.
fn is_word_char(c: char) -> bool {
    if c.is_alphabetic() {
        return true;
    }
    [Lang::En, Lang::Ru].into_iter().any(|lang| {
        crate::keymap::find_key(c, lang)
            .and_then(|(code, shift)| crate::keymap::char_for(code, shift, lang.other()))
            .is_some_and(|other| other.is_alphabetic())
    })
}

/// Split text into alternating runs of word chars and everything else (punctuation, digits,
/// whitespace), preserving every character so the pieces reassemble verbatim. The bool is `true`
/// for word runs. Uses [`is_word_char`] (the live-tokenizer rule) so a `?`/`.`/`!` separator stays
/// intact while a punctuation-key letter like `,`(б) extends the word it belongs to.
fn segments(text: &str) -> Vec<(&str, bool)> {
    let mut segs = Vec::new();
    let mut start = 0;
    let mut cur_word: Option<bool> = None;
    for (i, c) in text.char_indices() {
        let sp = is_word_char(c);
        match cur_word {
            Some(prev) if prev != sp => {
                segs.push((&text[start..i], prev));
                start = i;
                cur_word = Some(sp);
            }
            None => cur_word = Some(sp),
            _ => {}
        }
    }
    if let Some(prev) = cur_word {
        segs.push((&text[start..], prev));
    }
    segs
}

/// Real one-letter words per language: literary + common SMS-slang single letters.
/// English: `a` / `i` (grammar) + `u r y k n o` (SMS-style "you", "are", "why", "ok", …).
/// Russian: prepositions and one-letter words.
fn is_valid_single(c: char, lang: Lang) -> bool {
    match lang {
        // Conservative SMS-slang set. Adding more letters here makes us LESS likely to convert
        // the corresponding Russian one-letter word (so don't add `e`, which collides with the
        // Russian particle `у`, unless we have a stronger reason).
        Lang::En => matches!(c, 'a' | 'i' | 'u' | 'r' | 'y' | 'k' | 'n' | 'o'),
        Lang::Ru => matches!(c, 'а' | 'и' | 'о' | 'у' | 'я' | 'в' | 'к' | 'с'),
    }
}

/// Decide a one-letter word: leave it if it's a real word as typed, convert if its
/// counterpart is. Falls back to the hard-coded SMS-slang whitelist [`is_valid_single`] if
/// the dictionary doesn't have an opinion — the dictionary is the more flexible signal
/// (users can extend `data/en.words` or `data/ru.words` to teach single-letter slang).
fn decide_single(
    word: &CompletedWord,
    cur_lang: Lang,
    other: Lang,
    models: &Models,
) -> Decision {
    let lower = |s: &str| s.chars().next().and_then(|c| c.to_lowercase().next());
    // Prefer the dictionary's verdict over the whitelist — it's user-extensible.
    if models.is_word(&word.cur, cur_lang) {
        return Decision::Leave;
    }
    if let Some(c) = lower(&word.cur) {
        if is_valid_single(c, cur_lang) {
            return Decision::Leave;
        }
    }
    if models.is_word(&word.alt, other) {
        return Decision::Convert { to: other };
    }
    if let Some(a) = lower(&word.alt) {
        if is_valid_single(a, other) {
            return Decision::Convert { to: other };
        }
    }
    Decision::Leave
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::WordBuffer;
    use crate::keymap::{classify, Mods};

    fn ru_words() -> Vec<&'static str> {
        vec![
            "привет", "пример", "ветер", "вертеть", "привычка", "правда", "мир", "дом",
            "работа", "время", "человек", "слово", "дело", "место", "вода",
        ]
    }
    fn en_words() -> Vec<&'static str> {
        vec![
            "hello", "help", "world", "work", "word", "time", "people", "place", "water",
            "house", "thing", "yellow", "fellow", "follow", "below",
        ]
    }

    fn detector() -> Detector {
        Detector::new(Models::from_words(ru_words(), en_words()), DetectConfig::default())
    }

    fn word(codes: &[u16], lang: Lang) -> CompletedWord {
        let mut buf = WordBuffer::new();
        for &c in codes {
            buf.push(classify(c, Mods::default(), lang));
        }
        buf.finish(lang).unwrap()
    }

    #[test]
    fn converts_wrong_layout_russian() {
        // "ghbdtn" typed on EN → should convert to RU "привет".
        let w = word(&[34, 35, 48, 32, 20, 49], Lang::En);
        assert_eq!(detector().decide(&w, &UserDict::empty("/tmp/x".into())), Decision::Convert {
            to: Lang::Ru
        });
    }

    #[test]
    fn converts_wrong_layout_english() {
        // "руддщ" typed on RU → should convert to EN "hello".
        let w = word(&[35, 18, 38, 38, 24], Lang::Ru);
        assert_eq!(detector().decide(&w, &UserDict::empty("/tmp/x".into())), Decision::Convert {
            to: Lang::En
        });
    }

    #[test]
    fn leaves_valid_current_word() {
        // "привет" typed correctly on RU → leave it.
        let w = word(&[34, 35, 48, 32, 20, 49], Lang::Ru);
        assert_eq!(detector().decide(&w, &UserDict::empty("/tmp/x".into())), Decision::Leave);
    }

    #[test]
    fn exception_is_left_alone() {
        let mut dict = UserDict::empty("/tmp/x".into());
        // Pretend "ghbdtn" is an accepted exception (e.g. a nick).
        dict.add("ghbdtn", Lang::En, userdict::ListKind::Manual).unwrap();
        let w = word(&[34, 35, 48, 32, 20, 49], Lang::En);
        assert_eq!(detector().decide(&w, &dict), Decision::Leave);
        let _ = std::fs::remove_dir_all::<std::path::PathBuf>("/tmp/x".into());
    }

    #[test]
    fn explicitly_untrusted_word_is_left_alone() {
        // An untrusted context (e.g. a mid-word cursor move) is never auto-corrected.
        let keys = vec![(34u16, false), (35, false), (48, false), (32, false), (20, false), (49, false)];
        let w = crate::buffer::CompletedWord::from_keys(keys, Lang::En, false);
        assert!(!w.trusted);
        assert_eq!(detector().decide(&w, &UserDict::empty("/tmp/x".into())), Decision::Leave);
    }

    #[test]
    fn force_list_converts_even_if_untrusted() {
        let mut dict = UserDict::empty("/tmp/xf".into());
        dict.add("ghbdtn", Lang::En, userdict::ListKind::Force).unwrap();
        let w = word(&[34, 35, 48, 32, 20, 49], Lang::En);
        assert_eq!(detector().decide(&w, &dict), Decision::Convert { to: Lang::Ru });
        let _ = std::fs::remove_dir_all::<std::path::PathBuf>("/tmp/xf".into());
    }

    #[test]
    fn lone_punctuation_keys_stay_punctuation() {
        // `'`(э), `[`(х), `` ` ``(ё), `,`(б), `.`(ю) typed alone must stay as typed —
        // converting a lone apostrophe or bracket into a Cyrillic letter would corrupt
        // quoting and markup while writing English/code.
        for code in [40u16, 26, 41, 51, 52] {
            let w = word(&[code], Lang::En);
            assert_eq!(
                detector().decide(&w, &UserDict::empty("/tmp/xp".into())),
                Decision::Leave,
                "key {code} ({:?}) must not convert",
                w.cur
            );
        }
    }

    /// A word typed on the EN layout, built straight from its characters' physical keys.
    fn typed_en(s: &str) -> CompletedWord {
        let keys: Vec<(u16, bool)> = s
            .chars()
            .map(|c| crate::keymap::find_key(c, Lang::En).expect("char is on the US layout"))
            .collect();
        CompletedWord::from_keys(keys, Lang::En, true)
    }

    #[test]
    fn punctuation_key_word_needs_a_plausible_alt() {
        // The real bundled word lists — this branch is entirely about how the n-gram scores
        // compare, so the toy lists used by the other tests would prove nothing.
        let det = Detector::new(Models::builtin(), DetectConfig::default());
        let dict = UserDict::empty("/tmp/x-asym".into());

        // Wrong-layout words whose punctuation keys are real Cyrillic letters still convert:
        // `,`=б, `.`=ю are part of the word, and the Russian reading is clearly a word.
        for s in ["yflj,yj", "ghbdtn."] {
            assert_eq!(
                det.decide(&typed_en(s), &dict),
                Decision::Convert { to: Lang::Ru },
                "{s} must still convert"
            );
        }

        // …but an out-of-dictionary Latin word followed by a `.`/`,` must be LEFT ALONE.
        // The old gate (`alt_valid_min - 2.5` = -5.8) sat below the model's own unseen-trigram
        // default (-4.19), so it accepted any string at all and rewrote every name, brand and
        // technical term that ended a sentence into gibberish (`puntu.` → `згтегю`).
        for s in ["puntu.", "alfer.", "serde.", "evdev.", "myword."] {
            assert_eq!(det.decide(&typed_en(s), &dict), Decision::Leave, "{s} must be left alone");
        }
    }

    #[test]
    fn two_letter_words_convert_only_when_they_are_common() {
        let det = Detector::new(Models::builtin(), DetectConfig::default());
        let dict = UserDict::empty("/tmp/x-short".into());
        let decide = |s: &str| det.decide(&typed_en(s), &dict);

        // The Russian words people really do type with the layout not switched: не, мы, ты,
        // вы, на, но, он, то, по, за, из, до, от, да…
        for s in ["yt", "vs", "ns", "ds", "yf", "yj", "jy", "nj", "gj", "pf", "bp", "lj", "jn", "lf"] {
            assert_eq!(decide(s), Decision::Convert { to: Lang::Ru }, "{s} must still convert");
        }
        // …including the ones with a letter on a punctuation key: бы, же, об, уж.
        for s in [",s", ";t", "j,", "e;"] {
            assert_eq!(decide(s), Decision::Convert { to: Lang::Ru }, "{s} must still convert");
        }

        // Everything else that merely happens to be "in a dictionary" is left alone: the
        // lists hold a third of all possible letter pairs, which made TV «ЕМ», CV «СМ», a
        // `dr` «вк» and the ending of every ordinal (`1st`, `2nd`, `3rd`, `4th`) Cyrillic.
        for s in ["tv", "cv", "dr", "jr", "uk", "eu", "st", "nd", "rd", "th", "ed", "er", "ly", "km", "cm", "gb", "tb", "hz", "lb"] {
            assert_eq!(decide(s), Decision::Leave, "{s} must be left alone");
        }
        // A real two-letter English word was never touched and still is not.
        for s in ["am", "by", "do", "it", "we"] {
            assert_eq!(decide(s), Decision::Leave, "{s} is English");
        }
    }

    #[test]
    fn common_short_words_come_from_the_top_of_the_list() {
        // Rank is what separates a word from corpus debris, so a two-letter entry deep in a
        // (frequency-ordered) Russian list does not count — while every two-letter entry of
        // the alphabetical English list does.
        let mut ru: Vec<String> = vec!["не".into()];
        ru.extend((0..COMMON_SHORT_RANK).map(|i| format!("слово{i}")));
        ru.push("ть".into());
        let m = Models::from_words(ru, vec!["zoo".to_string(), "we".to_string()]);
        assert!(m.is_common_short("не", Lang::Ru));
        assert!(m.is_common_short("НЕ", Lang::Ru), "case-insensitive like every lookup");
        assert!(!m.is_common_short("ть", Lang::Ru), "in the list, but far too deep to be a word");
        assert!(m.is_word("ть", Lang::Ru), "…though still a dictionary entry");
        assert!(m.is_common_short("we", Lang::En));
        assert!(!m.is_common_short("zoo", Lang::En), "not two letters");
    }

    #[test]
    fn convert_text_fixes_wrong_layout_word() {
        let dict = UserDict::empty("/tmp/xc".into());
        assert_eq!(convert_text("ghbdtn", &detector(), &dict).as_deref(), Some("привет"));
    }

    #[test]
    fn convert_text_preserves_whitespace_and_good_words() {
        let dict = UserDict::empty("/tmp/xc".into());
        // "ghbdtn" converts; "hello" is a valid English word → left as-is. Spacing is preserved.
        assert_eq!(
            convert_text("  ghbdtn   hello ", &detector(), &dict).as_deref(),
            Some("  привет   hello ")
        );
    }

    #[test]
    fn convert_text_returns_none_when_nothing_changes() {
        let dict = UserDict::empty("/tmp/xc".into());
        assert_eq!(convert_text("hello world", &detector(), &dict), None);
        assert_eq!(convert_text("привет", &detector(), &dict), None);
    }

    #[test]
    fn convert_text_keeps_trailing_punctuation_verbatim() {
        // Regression: `?` must stay `?`, not be transliterated key-for-key into `&`
        // (RU Shift+7 → EN Shift+7). Only the letters convert.
        let dict = UserDict::empty("/tmp/xc".into());
        assert_eq!(convert_text("ghbdtn?", &detector(), &dict).as_deref(), Some("привет?"));
        assert_eq!(convert_text("ghbdtn!!!", &detector(), &dict).as_deref(), Some("привет!!!"));
    }

    #[test]
    fn convert_text_keeps_punctuation_key_letter_attached() {
        // Regression for the paste path: `,kjryjn` is `блокнот` typed on EN (the comma key is
        // `б`). The tokenizer must keep the leading `,` attached — like the live tokenizer —
        // not split it into `,` + `kjryjn` and yield `,локнот` (losing the `б`).
        let det = Detector::new(
            Models::from_words(vec!["блокнот"], Vec::<&str>::new()),
            DetectConfig::default(),
        );
        let dict = UserDict::empty("/tmp/xpc".into());
        assert_eq!(convert_text(",kjryjn", &det, &dict).as_deref(), Some("блокнот"));
    }

    #[test]
    fn convert_text_leaves_single_letters_in_mixed_text() {
        // Lone letters in arbitrary pasted text (the `Z` in a timestamp, the `c` in `-c`) must
        // not be converted (z→я, c→с) — that mangled real log output. Only multi-letter
        // wrong-layout words are touched.
        let dict = UserDict::empty("/tmp/xsl".into());
        assert_eq!(convert_text("21:53:11Z", &detector(), &dict), None);
        assert_eq!(convert_text("sg input -c run", &detector(), &dict), None);
    }
}

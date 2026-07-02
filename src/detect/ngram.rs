//! Character-trigram language model.
//!
//! Tiny and O(len) to score — ideal for the hot path. A model is trained from a word list
//! (`from_words`) and scores a word by its mean conditional log-probability P(c₃ | c₁c₂) with
//! add-k smoothing, normalized by length so short and long words are comparable.

use std::collections::HashMap;

const PAD: char = '\u{2}'; // start/boundary marker, won't appear in real words
/// Smoothing vocabulary size (roughly: distinct letters across both alphabets + a margin).
const VOCAB: f64 = 66.0;
const ADD_K: f64 = 0.5;

/// A trained trigram model.
#[derive(Clone, Debug, Default)]
pub struct Model {
    /// log P(c3 | c1, c2) for seen trigrams.
    tri: HashMap<(char, char, char), f64>,
    /// Fallback log P(· | c1, c2) for an unseen c3 given a seen context.
    ctx_fallback: HashMap<(char, char), f64>,
    /// Global fallback for an entirely unseen context.
    default: f64,
}

impl Model {
    /// Train from an iterator of words (lowercased internally).
    pub fn from_words<I, S>(words: I) -> Model
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut tri_counts: HashMap<(char, char, char), u32> = HashMap::new();
        let mut ctx_counts: HashMap<(char, char), u32> = HashMap::new();

        for w in words {
            let chars: Vec<char> = std::iter::once(PAD)
                .chain(std::iter::once(PAD))
                .chain(w.as_ref().to_lowercase().chars())
                .chain(std::iter::once(PAD))
                .collect();
            for win in chars.windows(3) {
                let key = (win[0], win[1], win[2]);
                *tri_counts.entry(key).or_insert(0) += 1;
                *ctx_counts.entry((win[0], win[1])).or_insert(0) += 1;
            }
        }

        let mut tri = HashMap::with_capacity(tri_counts.len());
        for (&(a, b, c), &n) in &tri_counts {
            let ctx = *ctx_counts.get(&(a, b)).unwrap_or(&0) as f64;
            let p = (n as f64 + ADD_K) / (ctx + ADD_K * VOCAB);
            tri.insert((a, b, c), p.ln());
        }
        let mut ctx_fallback = HashMap::with_capacity(ctx_counts.len());
        for (&(a, b), &ctx) in &ctx_counts {
            // Probability mass reserved for an unseen continuation of this context.
            let p = ADD_K / (ctx as f64 + ADD_K * VOCAB);
            ctx_fallback.insert((a, b), p.ln());
        }
        let default = (ADD_K / (ADD_K * VOCAB)).ln();
        Model { tri, ctx_fallback, default }
    }

    /// Mean log-probability per trigram of `word`. Higher (closer to 0) = more plausible.
    /// Returns `default` for empty input.
    pub fn score(&self, word: &str) -> f64 {
        let chars: Vec<char> = std::iter::once(PAD)
            .chain(std::iter::once(PAD))
            .chain(word.to_lowercase().chars())
            .chain(std::iter::once(PAD))
            .collect();
        if chars.len() < 3 {
            return self.default;
        }
        let mut sum = 0.0;
        let mut n = 0;
        for win in chars.windows(3) {
            let key = (win[0], win[1], win[2]);
            sum += self
                .tri
                .get(&key)
                .copied()
                .or_else(|| self.ctx_fallback.get(&(win[0], win[1])).copied())
                .unwrap_or(self.default);
            n += 1;
        }
        sum / n as f64
    }

    pub fn is_empty(&self) -> bool {
        self.tri.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_words_score_higher_than_gibberish() {
        let ru = Model::from_words(["привет", "пример", "ветер", "приветствие", "вертел"]);
        // "привет" should look far more Russian than "руддщ" (which is "hello" on RU keys).
        assert!(ru.score("привет") > ru.score("руддщ"));
    }

    #[test]
    fn english_model_prefers_english() {
        let en = Model::from_words(["hello", "help", "hell", "fellow", "yellow"]);
        assert!(en.score("hello") > en.score("ghbdtn"));
    }
}

//! The correction sequence and the undo stack.
//!
//! GNOME/Mutter locks our uinput virtual keyboard to a fixed layout, so we can't render the
//! corrected word by replaying key codes (Cyrillic would come out as Latin). Instead we insert
//! the target text via the **clipboard** (paste), which is layout-independent. The layout is left
//! unchanged on purpose (auto-switching disturbed focus/tracking) — wrong-layout words are
//! converted in place.
//!
//! The correction is split into [`clip_prepare`] → [`commit`] → [`clip_restore`] so the caller
//! can re-check for type-ahead *between* preparing the clipboard and the destructive part.

use std::time::{Duration, Instant};

use anyhow::Result;

use crate::buffer::CompletedWord;
use crate::clipboard;
use crate::input::emitter::{BackspaceSpeed, Emitter};
use crate::keymap::Lang;

/// Max time to wait for `wl-copy` to actually own the selection with our text before pasting.
const CLIPBOARD_CONFIRM_TIMEOUT: Duration = Duration::from_millis(400);
/// Minimum settle before the destructive backspace, even if the clipboard confirms instantly
/// (it does when the new text already equals the current clipboard, e.g. converting the same word
/// twice). Without this floor the backspace can race the app still rendering the just-typed word,
/// leaving the original in place and only appending the conversion (`brjyrb иконки`).
const CLIPBOARD_SETTLE_FLOOR: Duration = Duration::from_millis(120);
/// Let the injected Shift-release register before the destructive backspace starts. The user may
/// have been holding Shift (typing a separator like `_`/`?`); deleting while the app still thinks
/// Shift is down can turn the first Backspace into Shift+Backspace (a no-op or a different edit),
/// dropping the leftmost deletes.
const SHIFT_RELEASE_SETTLE: Duration = Duration::from_millis(20);
/// Safe variant: let all backspaces land before the paste. Without this the paste's Ctrl+V arrives
/// while the app is still processing the backspace burst, so the *last* deletes are dropped and
/// the left part of the original word survives (`,kjблокнот` instead of `блокнот`). Used on the
/// passive (no-grab) path where the user can still inject keystrokes meanwhile.
const POST_BACKSPACE_SETTLE_SAFE: Duration = Duration::from_millis(60);
/// Fast variant of the post-backspace settle, used inside the keyboard grab. The grab guarantees
/// no foreign events interleave between our backspaces and our Ctrl+V, so this only needs to
/// cover the compositor's own pipeline latency.
const POST_BACKSPACE_SETTLE_FAST: Duration = Duration::from_millis(20);

/// Whether this correction runs inside a keyboard grab (no possible type-ahead, settle's can be
/// aggressive) or on the passive path (must stay conservative).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitMode {
    /// Passive path — keep all the conservative settle's that protect against the user's
    /// concurrent typing colliding with our injected sequence.
    Safe,
    /// Grabbed path — the compositor cannot deliver foreign events to the focused app during
    /// our injection burst, so we shrink every settle to the minimum the compositor itself
    /// needs to keep up.
    Fast,
}

impl CommitMode {
    fn backspace_speed(self) -> BackspaceSpeed {
        match self {
            CommitMode::Safe => BackspaceSpeed::Safe,
            CommitMode::Fast => BackspaceSpeed::Fast,
        }
    }
    fn post_backspace_settle(self) -> Duration {
        match self {
            CommitMode::Safe => POST_BACKSPACE_SETTLE_SAFE,
            CommitMode::Fast => POST_BACKSPACE_SETTLE_FAST,
        }
    }
}
/// Time to let the app read the pasted selection before we restore the previous clipboard.
/// Generous because the failure mode is severe: if we restore too early, the compositor's
/// asynchronous handling of our injected Ctrl+V can land **after** the restore, pasting the
/// previous clipboard (often a stale converted word from an earlier correction) instead.
const PASTE_SETTLE: Duration = Duration::from_millis(350);

/// A correction we can undo.
#[derive(Clone, Debug)]
pub struct Correction {
    pub from: Lang,
    pub to: Lang,
    /// Text we put on screen (the converted form).
    pub converted: String,
    /// The original on-screen text (what the user actually typed).
    pub original: String,
    /// A separator the user already typed after the word (deleted & re-typed). `None` for
    /// mid-word manual conversions.
    pub trailing: Option<(u16, bool)>,
}

/// Put `replacement` on the clipboard and wait for `wl-copy` to own the selection. Returns the
/// user's previous clipboard so it can be restored. The original word is still on screen during
/// this wait — the caller should re-check for type-ahead before committing.
pub fn clip_prepare(replacement: &str) -> Result<Option<String>> {
    let start = Instant::now();
    let saved = clipboard::get().ok();
    clipboard::set(replacement)?;
    // Confirm `wl-copy` actually owns the selection with our text before returning, so the paste
    // can't grab the *previous* clipboard (which would insert stale, unrelated text). `wl-copy`
    // forks a server asynchronously, and a fixed delay sometimes lost the race. `get()` strips a
    // trailing newline, so compare against the same.
    let want = replacement.trim_end_matches('\n');
    let deadline = start + CLIPBOARD_CONFIRM_TIMEOUT;
    loop {
        if clipboard::get().ok().as_deref() == Some(want) {
            break;
        }
        if Instant::now() >= deadline {
            tracing::warn!("clipboard did not confirm within timeout; pasting anyway");
            break;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    // Floor the total wait so the destructive backspace never races the app still rendering the
    // user's just-typed word (the confirm above can return almost instantly).
    if let Some(remaining) = CLIPBOARD_SETTLE_FLOOR.checked_sub(start.elapsed()) {
        std::thread::sleep(remaining);
    }
    Ok(saved)
}

/// The destructive part: delete `backspaces` chars and paste the prepared clipboard (which
/// already includes the trailing separator, so there's no separate space tap to race the async
/// paste). `mode` selects conservative vs. aggressive settle's (use `Fast` only inside a grab).
/// `shift_was_held` lets us skip the shift-release dance — and its 20 ms settle — when no Shift
/// was active in the first place, which is the common case for an unshifted Space terminator.
pub fn commit(em: &mut Emitter, backspaces: usize, mode: CommitMode, shift_was_held: bool) -> Result<()> {
    // A separator like `_` (Shift+`-`) is typed with Shift held; release it first so the paste
    // below is a clean Ctrl+V and not Ctrl+Shift+V (which pastes nothing → the word vanishes),
    // and so the deletes below aren't seen as Shift+Backspace. Skip when Shift wasn't held to
    // save the 20 ms settle on the common unshifted-Space path.
    if shift_was_held {
        em.release_shift()?;
        std::thread::sleep(SHIFT_RELEASE_SETTLE);
    }
    em.backspace(backspaces, mode.backspace_speed())?;
    std::thread::sleep(mode.post_backspace_settle());
    em.paste()?;
    Ok(())
}

/// The replacement text to paste for converting `word`'s alt form, including the trailing
/// separator (typed in `lang`) so the word and its space are inserted atomically.
pub fn replacement(alt: &str, trailing: Option<(u16, bool)>, lang: Lang) -> String {
    match trailing {
        Some((code, shift)) => format!("{alt}{}", crate::keymap::separator_str(code, shift, lang)),
        None => alt.to_string(),
    }
}

/// Restore the user's clipboard once the app has consumed the paste.
pub fn clip_restore(saved: Option<String>) {
    if let Some(old) = saved {
        std::thread::sleep(PASTE_SETTLE);
        let _ = clipboard::set(&old);
    }
}

/// Build the [`Correction`] record for an applied conversion of `word` to `to`.
pub fn correction(word: &CompletedWord, to: Lang, trailing: Option<(u16, bool)>) -> Correction {
    Correction {
        from: word.lang,
        to,
        converted: word.alt.clone(),
        original: word.cur.clone(),
        trailing,
    }
}

/// Convert `word` to `to` in one shot (no type-ahead re-check — used by manual hotkeys, where the
/// user has paused to press the key).
pub fn apply_correction(
    em: &mut Emitter,
    word: &CompletedWord,
    to: Lang,
    trailing: Option<(u16, bool)>,
) -> Result<Correction> {
    let saved = clip_prepare(&replacement(&word.alt, trailing, word.lang))?;
    // Run commit, but ALWAYS restore the user's previous clipboard — even on error. Using `?`
    // here would early-return and silently wipe the original clipboard contents, since
    // `clip_prepare` already replaced them with our payload.
    let shift_held = trailing.is_some_and(|(_, sh)| sh);
    let result = commit(
        em,
        word.cur.chars().count() + trailing.is_some() as usize,
        CommitMode::Safe,
        shift_held,
    );
    clip_restore(saved);
    result?;
    Ok(correction(word, to, trailing))
}

/// Undo a correction: put the original text back. The caller learns the original word.
pub fn undo(em: &mut Emitter, c: &Correction) -> Result<()> {
    let saved = clip_prepare(&replacement(&c.original, c.trailing, c.from))?;
    let shift_held = c.trailing.is_some_and(|(_, sh)| sh);
    let result = commit(
        em,
        c.converted.chars().count() + c.trailing.is_some() as usize,
        CommitMode::Safe,
        shift_held,
    );
    clip_restore(saved);
    result
}

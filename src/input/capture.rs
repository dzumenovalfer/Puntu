//! The keyboard capture loop and the mouse-click watcher.

use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use anyhow::Result;
use evdev::{Device, InputEventKind};

use crate::action::{self, Correction};
use crate::buffer::{CompletedWord, WordBuffer};
use crate::detect::userdict::ListKind;
use crate::detect::Decision;
use crate::hotkeys::{self, HotAction};
use crate::input::emitter::Emitter;
use crate::input::{Flags, State};
use crate::keymap::{self, classify, KeyEvent, Lang, Mods};

/// Upper bound on clipboard length that paste-convert will rewrite. Anything longer (or
/// multi-line) is left untouched — rewriting it would mean a huge, destructive backspace burst.
const PASTE_CONVERT_MAX_CHARS: usize = 40;

/// Quiet period required after a converted word's separator before we actually commit the
/// correction. The fundamental race in our backspace+paste path is "user keeps typing while
/// we backspace and paste"; debouncing the commit until the user pauses eliminates the race
/// entirely (no concurrent writers). Tuned to feel responsive: 350 ms is shorter than a
/// typical inter-word pause for non-fast typists, so most corrections still fire on the next
/// word boundary; fast typists experience the correction landing once they pause briefly.
const COMMIT_DEBOUNCE: Duration = Duration::from_millis(350);

/// A correction that has been *decided* but not yet committed — we're waiting for the user to
/// stop typing for [`COMMIT_DEBOUNCE`] before actually backspacing and pasting, so our
/// destructive sequence can never interleave with their concurrent keystrokes.
struct PendingCorrection {
    word: CompletedWord,
    to: Lang,
    trailing: (u16, bool),
    lang: Lang,
    deadline: Instant,
}

/// Tracks left/right modifier keys separately so releasing one of a pair is handled correctly.
#[derive(Default)]
struct ModState {
    lshift: bool,
    rshift: bool,
    lctrl: bool,
    rctrl: bool,
    lalt: bool,
    ralt: bool,
    lmeta: bool,
    rmeta: bool,
}

impl ModState {
    fn update(&mut self, code: u16, down: bool) {
        match code {
            keymap::KEY_LEFTSHIFT => self.lshift = down,
            keymap::KEY_RIGHTSHIFT => self.rshift = down,
            keymap::KEY_LEFTCTRL => self.lctrl = down,
            keymap::KEY_RIGHTCTRL => self.rctrl = down,
            keymap::KEY_LEFTALT => self.lalt = down,
            keymap::KEY_RIGHTALT => self.ralt = down,
            keymap::KEY_LEFTMETA => self.lmeta = down,
            keymap::KEY_RIGHTMETA => self.rmeta = down,
            _ => {}
        }
    }

    fn mods(&self) -> Mods {
        Mods {
            shift: self.lshift || self.rshift,
            ctrl: self.lctrl || self.rctrl,
            alt: self.lalt || self.ralt,
            meta: self.lmeta || self.rmeta,
        }
    }
}

/// `EVIOCGRAB` ioctl number on Linux. We invoke it directly (instead of `Device::ungrab`) so the
/// RAII guard below can release the keyboard from `Drop` without holding `&mut Device` — which
/// would block all other device access while the grab is active. The value `_IOW('E', 0x90, int)`
/// is stable across all Linux architectures.
const EVIOCGRAB_IOCTL: libc::c_ulong = 0x40044590;

/// RAII guard for an evdev keyboard grab. Releases the grab on `Drop`, so the keyboard can
/// **never** be left captured by a code path that forgot the explicit `ungrab()` call —
/// including on panic, early return, or `?`-bail. The guard only holds the raw fd (not
/// `&mut Device`), so the device stays usable for `drain`/`fetch_events` while the grab is
/// active.
///
/// **Invariant**: the guard must drop **before** the underlying `Device` is dropped. In
/// `run_keyboard` both live on the stack with `Device` declared first, so stack unwinding
/// orders them correctly.
struct GrabGuard {
    fd: RawFd,
    armed: bool,
}

impl GrabGuard {
    /// Try to grab `dev`. Returns a guard regardless: check `is_grabbed()` to know if it worked.
    fn try_grab(dev: &mut Device) -> Self {
        let fd = dev.as_raw_fd();
        let armed = dev.grab().is_ok();
        Self { fd, armed }
    }

    fn is_grabbed(&self) -> bool {
        self.armed
    }
}

impl Drop for GrabGuard {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: `fd` is a valid evdev fd owned by the `Device` on the caller's stack,
            // which lives at least as long as this guard (see invariant above).
            unsafe {
                libc::ioctl(self.fd, EVIOCGRAB_IOCTL, 0);
            }
            self.armed = false;
        }
    }
}

/// A converted-selection pair we use to recognize stale-repeat Ctrl+Shift taps. We need both
/// fields because Wayland/Mutter often updates PRIMARY to point at the freshly-pasted text after
/// the conversion, so the next read of PRIMARY returns `converted` rather than the original —
/// matching either form means "we just did this; don't re-fire".
#[derive(Debug)]
struct LastSelection {
    original: String,
    converted: String,
}

/// Detects a "tap" of modifier keys: pressing and releasing modifiers with no other key in
/// between (e.g. tap Ctrl, or tap Ctrl+Shift). Used for the Punto-style convert hotkeys.
#[derive(Default)]
struct TapDetector {
    peak: Mods,
    cancelled: bool,
}

impl TapDetector {
    fn on_mod_down(&mut self, mods_now: Mods) {
        self.peak = self.peak.union(mods_now);
    }
    fn on_key_down(&mut self) {
        self.cancelled = true; // a non-modifier was pressed → this is a real chord, not a tap
    }
    /// Returns the tapped modifier set once *all* modifiers are released without a cancel.
    fn on_release(&mut self, mods_now: Mods) -> Option<Mods> {
        if !mods_now.is_empty() {
            return None; // still holding a modifier
        }
        let result = (!self.cancelled && !self.peak.is_empty()).then_some(self.peak);
        self.peak = Mods::default();
        self.cancelled = false;
        result
    }
}

fn is_modifier(code: u16) -> bool {
    matches!(
        code,
        keymap::KEY_LEFTSHIFT
            | keymap::KEY_RIGHTSHIFT
            | keymap::KEY_LEFTCTRL
            | keymap::KEY_RIGHTCTRL
            | keymap::KEY_LEFTALT
            | keymap::KEY_RIGHTALT
            | keymap::KEY_LEFTMETA
            | keymap::KEY_RIGHTMETA
    )
}

/// Count how many *visible characters* a sequence of physical key events has produced in the
/// focused app — i.e. the net change in cursor position. Used to widen the upcoming backspace
/// burst to also delete chars the user managed to type during the pre-grab window (before our
/// grab took effect): without this, those leaked chars survive in front of the corrected paste
/// (`ghпривет`). Counts letter / separator / printable key-downs as +1 and Backspace as -1;
/// modifiers, navigation, F-keys, repeats and releases don't move the cursor.
fn count_visible_chars(events: &[(u16, i32)]) -> usize {
    let mut net: i32 = 0;
    for &(code, value) in events {
        if value != 1 {
            continue; // only key-downs visible
        }
        if is_modifier(code) {
            continue;
        }
        match code {
            keymap::KEY_BACKSPACE => net = net.saturating_sub(1),
            // Letters, digits, separators, all printable keys → one visible char.
            // Use the same alphabet the tokenizer cares about: a key that maps to *any* char
            // in either layout produces a visible glyph.
            c if keymap::char_for(c, false, keymap::Lang::En).is_some()
                || keymap::char_for(c, false, keymap::Lang::Ru).is_some() =>
            {
                net += 1;
            }
            _ => {} // Navigation/F-keys/media — no visible change.
        }
    }
    net.max(0) as usize
}

/// The capture loop. **Passive**: we never grab the keyboard — the compositor delivers every
/// key to apps as usual, and we only *inject* corrections via the virtual device. So if this
/// daemon misbehaves or dies, typing is unaffected.
///
/// To never mangle text, a correction is only committed if the user hasn't typed a new key since
/// the word's separator: we prepare the clipboard, then drain any events that arrived during that
/// wait and abort if a real key-down is among them (the separator's own *release* doesn't count).
pub fn run_keyboard(mut dev: Device, mut em: Emitter, shared: State, flags: Flags) -> Result<()> {
    let fd = dev.as_raw_fd();
    set_nonblocking(fd);
    let mut buffer = WordBuffer::new();
    let mut modst = ModState::default();
    let mut tap = TapDetector::default();
    let mut undo: Option<Correction> = None;
    let mut pending: Vec<(u16, i32)> = Vec::new();
    // Set when the last action was a paste (Ctrl+V); a following separator triggers conversion of
    // the pasted clipboard text. Cleared by any typing/editing in between (see below).
    let mut pending_paste = false;
    // The last word that was completed and **left as-is** (with its trailing separator), so a
    // Ctrl tap / convert-last hotkey can still convert it after the space is typed. Cleared as
    // soon as anything else is typed or the cursor moves.
    let mut last: Option<(CompletedWord, (u16, bool))> = None;
    // The PRIMARY selection we last converted via Ctrl+Shift, **paired** with its converted
    // form. Wayland's Mutter typically re-highlights the freshly-pasted text, so PRIMARY ends
    // up holding the *converted* string after the conversion. Tracking both means a repeated
    // Ctrl+Shift with no new selection — whether PRIMARY still shows the original (cached) or
    // already the converted form — is skipped, so we can't run a destructive back-conversion
    // (`hello`→`руддщ`, then `руддщ`→`hello` on the next tap). Cleared on any key/mouse action.
    let mut last_selection: Option<LastSelection> = None;
    // A correction the detector decided on but we haven't committed yet — held until the user
    // is quiet for COMMIT_DEBOUNCE so our backspace+paste can't race their typing. Cleared
    // (without committing) by any letter key-down: continuing to type signals the user is mid-
    // word and doesn't want us to interrupt them right now.
    let mut pending_correction: Option<PendingCorrection> = None;

    loop {
        // If a correction is queued, fire it the moment the user has been quiet long enough.
        // Doing this BEFORE wait_readable means we don't block until the next physical key
        // event — the timeout below wakes us up at the deadline.
        if let Some(pc) = pending_correction.as_ref() {
            if Instant::now() >= pc.deadline {
                let pc = pending_correction.take().unwrap();
                let (replay, corr) = commit_pending_correction(&mut dev, &mut em, pc);
                if let Some(c) = corr {
                    undo = Some(c);
                }
                if !replay.is_empty() {
                    let mut merged = std::mem::take(&mut pending);
                    merged.extend(replay);
                    pending = merged;
                }
            }
        }

        // Take queued events (left over from a correction), or block until the keyboard has some.
        // `from_pending` marks a re-processed batch (e.g. keys replayed after a grabbed
        // correction): those events are *not* physically held, so the grab path must not wait for
        // their release.
        // `from_pending` is no longer consulted (debounce + commit_pending_correction
        // handle their own release-waiting), but we keep the destructuring slot for clarity.
        let (batch, _from_pending) = if pending.is_empty() {
            // If we have a queued correction, only wait until its deadline — then loop back
            // around so the check above can commit it.
            let timeout = pending_correction.as_ref().map(|pc| {
                pc.deadline.saturating_duration_since(Instant::now())
            });
            let (revents, timed_out) = wait_readable(fd, timeout);
            // Device disconnected / re-enumerated (e.g. after suspend) → exit so systemd restarts
            // us on a fresh device instead of silently spinning on a dead fd.
            if revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                anyhow::bail!("keyboard device error (poll revents={revents}); exiting to be restarted");
            }
            if timed_out {
                // Debounce deadline expired with no further input — loop back so the top of
                // the loop fires the queued correction.
                continue;
            }
            (drain(&mut dev), false)
        } else {
            (std::mem::take(&mut pending), true)
        };

        let mut idx = 0;
        while idx < batch.len() {
            let (code, value) = batch[idx];
            idx += 1;

            if value != 1 {
                if is_modifier(code) {
                    modst.update(code, value != 0);
                    if value == 0 {
                        if let Some(tapped) = tap.on_release(modst.mods()) {
                            // Modifier-tap actions are OFF by default: a bare-Ctrl tap firing
                            // switch_layout was undoing the user's own layout switch on every
                            // press (Right Ctrl is a common Linux/GNOME layout-switch keybind),
                            // and Ctrl+Shift has the same conflict. Behind a config flag so users
                            // who want Punto-style modifier triggers can opt in.
                            let taps_on = shared.lock().unwrap().cfg.enable_modifier_taps;
                            if taps_on {
                                if let Err(e) =
                                    handle_tap(tapped, &mut em, &mut last_selection, &shared)
                                {
                                    tracing::warn!("tap action failed: {e}");
                                }
                            }
                        }
                    }
                }
                continue;
            }
            if is_modifier(code) {
                modst.update(code, true);
                tap.on_mod_down(modst.mods());
                continue;
            }
            tap.on_key_down(); // a real key → cancel any pending modifier-tap
            last_selection = None; // a key press means a fresh selection may follow
            // Any new non-modifier key-down means the user is still typing, so a queued
            // correction can't fire now without risking the same race we tried to avoid.
            // Cancel it — the user effectively gave up on the auto-correct for that word.
            // They can still trigger it manually with Pause/Break or undo with Ctrl+Z later.
            if let Some(pc) = pending_correction.take() {
                tracing::debug!(
                    "cancelling queued correction {:?}: user kept typing",
                    pc.word.cur
                );
            }

            let mods = modst.mods();
            if flags.mouse_dirty.swap(false, Ordering::SeqCst) {
                buffer.invalidate();
                last = None;
                last_selection = None; // a click usually starts a new selection
            }

            // Hotkeys (inert keys like Pause/Break; can't be swallowed without a grab).
            let (hk, paste_convert) = {
                let s = shared.lock().unwrap();
                (s.cfg.hotkeys.clone(), s.cfg.paste_convert)
            };
            if let Some(act) = hotkeys::match_action(code, mods, &hk) {
                if let Err(e) =
                    handle_hotkey(act, &mut em, &mut buffer, &mut last, &mut undo, &shared, &flags)
                {
                    tracing::warn!("hotkey {act:?} failed: {e}");
                }
                continue;
            }

            // Paste (Ctrl+V): the pasted characters never arrive as key events, so the word buffer
            // can't see them — `Ctrl+V` is a chord and just invalidates the context. When
            // `paste_convert` is on, remember the paste so a following separator (the "paste a
            // word, then space" flow) can read the clipboard and convert it in place.
            if code == keymap::KEY_V && mods.ctrl && !mods.alt && !mods.meta {
                pending_paste = paste_convert;
                tracing::debug!("Ctrl+V seen → pending_paste={paste_convert} (paste_convert flag)");
                buffer.invalidate();
                last = None;
                continue;
            }

            // Tokenize with a fixed layout (tokenization is layout-independent); resolve the real
            // language only when the word closes — read fresh from `mru-sources`.
            let kev = classify(code, mods, Lang::En);
            if !matches!(kev, KeyEvent::Separator) {
                // Anything typed or edited after a paste means the pasted text is no longer the
                // run just before the cursor, so the pending paste-conversion can't be trusted.
                // Same gate applies to `last`: only typing/editing/navigation moves the cursor
                // off the last completed word. KeyEvent::Ignore covers Caps Lock, F-keys,
                // media/brightness, etc. — none of those move the cursor, so they must NOT
                // clear `last` (else a stray Caps Lock tap kills convert-last for the last word).
                if matches!(kev, KeyEvent::Letter { .. } | KeyEvent::Backspace | KeyEvent::Invalidate)
                {
                    pending_paste = false;
                    last = None;
                }
                buffer.push(kev);
                continue;
            }

            let lang = *flags.lang.lock().unwrap();
            // "Paste a word, then a space": convert the just-pasted clipboard text in place.
            // `std::mem::take` clears the pending-paste flag for *any* separator, so a non-space
            // separator (Enter/Tab) just disarms it without converting. The conversion itself fires
            // only on the intended gesture — a bare Space — never on Enter/Shift+Enter, which is a
            // line break: firing there (on a stale, possibly huge clipboard) is what destroyed a
            // whole line. Also requires an empty buffer, no chord, and not paused.
            if std::mem::take(&mut pending_paste) {
                let paused = flags.paused.load(Ordering::SeqCst);
                let word_gesture = code == keymap::KEY_SPACE && !mods.shift;
                if paste_convert && word_gesture && !mods.is_chord() && buffer.is_empty() && !paused {
                    tracing::debug!("space after paste → attempting paste-convert");
                    if let Some((rest, correction)) = try_paste_convert(
                        &mut dev,
                        &mut em,
                        &shared,
                        batch[idx..].to_vec(),
                        (code, mods.shift),
                        lang,
                    ) {
                        // Register the conversion so undo / convert-last can restore the original
                        // pasted text (a no-op conversion or a typed-ahead abort returns None).
                        if correction.is_some() {
                            undo = correction;
                        }
                        pending = rest;
                        break;
                    }
                } else {
                    tracing::debug!(
                        "paste-convert skipped at separator: space_gesture={word_gesture} flag={paste_convert} chord={} buf_empty={} paused={paused}",
                        mods.is_chord(),
                        buffer.is_empty()
                    );
                }
            }

            let word = match buffer.finish(lang) {
                Some(w) => w,
                None => continue,
            };
            // A fresh word just completed → the previous "last" is no longer at the cursor.
            last = None;
            if flags.paused.load(Ordering::SeqCst) {
                continue;
            }

            let (decision, dry) = {
                let s = shared.lock().unwrap();
                (s.detector.decide(&word, &s.dict), s.cfg.dry_run)
            };
            tracing::debug!(
                "word {:?} [{}] alt={:?} trusted={} → {:?}",
                word.cur, word.lang, word.alt, word.trusted, decision
            );
            let to = match decision {
                Decision::Convert { to } => to,
                Decision::Leave => {
                    // Looks fine as-is, but remember it (with its separator) so a Ctrl tap can
                    // still convert it after the space if the user disagrees.
                    last = Some((word, (code, mods.shift)));
                    continue;
                }
            };
            if dry {
                tracing::info!("[dry-run] would convert {:?} → {:?}", word.cur, word.alt);
                continue;
            }

            // Debounce the commit: the user just pressed the separator, but the destructive
            // backspace+paste burst is racy against any concurrent typing (the source of the
            // `dвы` / `sвырработаете` corruption under fast input). Queue the correction
            // instead, and fire it only after the user stays quiet for COMMIT_DEBOUNCE — the
            // race window then has no concurrent writer to collide with.
            tracing::debug!(
                "queued correction: {:?} → {:?} (commit in {:?} if user stays quiet)",
                word.cur, word.alt, COMMIT_DEBOUNCE
            );
            pending_correction = Some(PendingCorrection {
                word,
                to,
                trailing: (code, mods.shift),
                lang,
                deadline: Instant::now() + COMMIT_DEBOUNCE,
            });
        }
    }
}

/// Run a queued [`PendingCorrection`] now: drain leftover events, grab the keyboard, backspace
/// out the word + any pre-grab type-ahead, paste the corrected form, then replay user input
/// the app would otherwise have lost. Returns the events that should go into `pending` for the
/// next loop iteration, and the [`Correction`] record for undo (or `None` if commit failed).
///
/// Because the user paused for [`COMMIT_DEBOUNCE`] before we got here, the keyboard is quiet —
/// no concurrent writes contend with the burst, which was the original source of orphan
/// prefixes and merged-word bugs under fast input.
fn commit_pending_correction(
    dev: &mut Device,
    em: &mut Emitter,
    pc: PendingCorrection,
) -> (Vec<(u16, i32)>, Option<Correction>) {
    let PendingCorrection { word, to, trailing: (sep_code, sep_shift), lang, .. } = pc;
    let trailing = Some((sep_code, sep_shift));
    let replacement_text = action::replacement(&word.alt, trailing, lang);

    // Drain whatever's in the queue (likely just the separator's release at this point — the
    // user paused). Count any visible chars among them as type-ahead so we widen the backspace
    // to delete the leaked chars too, and replay them after the paste.
    let mut before: Vec<(u16, i32)> = drain(dev);
    let extra_typed_ahead = count_visible_chars(&before);
    let grab = GrabGuard::try_grab(dev);
    let grab_ok = grab.is_grabbed();
    tracing::debug!(
        "commit (debounced): sep code={sep_code} shift={sep_shift} replacement={:?} \
         backspaces={}(+{extra_typed_ahead} ahead) grab={grab_ok}",
        replacement_text,
        word.cur.chars().count() + 1,
    );

    let saved = match action::clip_prepare(&replacement_text) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("clipboard prepare failed: {e}");
            return (before, None);
        }
    };

    let mut correction = None;
    if grab_ok {
        let mut typed: Vec<(u16, i32)> = drain(dev);
        let bs = word.cur.chars().count() + 1 + extra_typed_ahead;
        match action::commit(em, bs, action::CommitMode::Fast, false) {
            Ok(()) => {
                tracing::info!("converted {:?} → {:?}", word.cur, word.alt);
                correction = Some(action::correction(&word, to, trailing));
            }
            Err(e) => tracing::warn!("correction commit failed: {e}"),
        }
        typed.extend(drain(dev));
        drop(grab);
        // Replay both before (deleted via widened backspace) and typed (lost to grab) so the
        // user's keystrokes survive in the correct order after our paste.
        let mut all_replay: Vec<(u16, i32)> =
            Vec::with_capacity(before.len() + typed.len());
        all_replay.extend(before.iter().copied());
        all_replay.extend(typed.iter().copied());
        if let Err(e) = em.replay_events(&all_replay) {
            tracing::warn!("replay after correction failed: {e}");
        }
        action::clip_restore(saved);
        before.extend(typed);
    } else {
        // Grab failed (rare): fall back to passive path with the safe settle's.
        before.extend(drain(dev));
        let typed_ahead = before.iter().any(|&(c, v)| v == 1 && !is_modifier(c));
        if typed_ahead {
            if let Some(s) = saved {
                let _ = crate::clipboard::set(&s);
            }
            tracing::debug!("skip autocorrect (typing ahead): {:?}", word.cur);
        } else {
            let bs = word.cur.chars().count() + 1;
            match action::commit(em, bs, action::CommitMode::Safe, false) {
                Ok(()) => {
                    tracing::info!("converted {:?} → {:?}", word.cur, word.alt);
                    correction = Some(action::correction(&word, to, trailing));
                }
                Err(e) => tracing::warn!("correction commit failed: {e}"),
            }
            action::clip_restore(saved);
        }
    }
    (before, correction)
}

/// Convert text just pasted from the clipboard (the "paste a word, then a separator" flow). Reads
/// the clipboard, converts any wrong-layout words, and—if anything changed—deletes the pasted text
/// plus the trailing separator and pastes the corrected form (separator included, so it lands
/// atomically). The user's clipboard is saved and restored.
///
/// Returns `Some((leftover_events, correction))` when it handled the separator so the caller can
/// `break`; the `correction` is `Some` only if a conversion was actually committed (so the caller
/// can register undo), and `None` if it was skipped (type-ahead / commit error). Returns the outer
/// `None` when there was nothing to convert and the caller should fall through to normal handling.
fn try_paste_convert(
    dev: &mut Device,
    em: &mut Emitter,
    shared: &State,
    after: Vec<(u16, i32)>,
    sep: (u16, bool),
    lang: Lang,
) -> Option<(Vec<(u16, i32)>, Option<Correction>)> {
    let pasted = match crate::clipboard::get() {
        Ok(t) if !t.is_empty() => t,
        Ok(_) => {
            tracing::debug!("paste-convert: clipboard is empty (nothing pasted?)");
            return None;
        }
        Err(e) => {
            tracing::debug!("paste-convert: clipboard read failed: {e}");
            return None;
        }
    };
    // SAFETY: never rewrite a large or multi-line paste. Converting it means deleting the whole
    // pasted block char-by-char (thousands of backspaces across newlines) — which destroyed text
    // and hung the daemon. Paste-convert is only meant for a short wrong-layout word or two.
    if pasted.contains('\n') || pasted.chars().count() > PASTE_CONVERT_MAX_CHARS {
        tracing::debug!(
            "paste-convert: skipping large/multi-line paste ({} chars)",
            pasted.chars().count()
        );
        return None;
    }
    let converted = match {
        let s = shared.lock().unwrap();
        crate::detect::convert_text(&pasted, &s.detector, &s.dict)
    } {
        Some(c) => c,
        None => {
            tracing::debug!("paste-convert: nothing to convert in {pasted:?}");
            return None;
        }
    };
    tracing::debug!("paste-convert: {pasted:?} → {converted:?}");

    let replacement = action::replacement(&converted, Some(sep), lang);
    // `clip_prepare` saves the current clipboard (still the pasted text) so we restore it after.
    let saved = match action::clip_prepare(&replacement) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("paste-convert clipboard prepare failed: {e}");
            return None;
        }
    };

    // Re-check for type-ahead before the destructive backspace, like the typed-word path.
    let mut rest = after;
    rest.extend(drain(dev));
    let correction = if rest.iter().any(|&(c, v)| v == 1 && !is_modifier(c)) {
        if let Some(s) = saved {
            let _ = crate::clipboard::set(&s);
        }
        tracing::debug!("skip paste-convert (typing ahead): {pasted:?}");
        None
    } else {
        // Paste-convert runs after the type-ahead drain above succeeded (no unmatched key-down
        // events), so we're effectively in a "quiet window" — safe to use Fast settle's. The
        // separator (Space) here is unshifted (paste-convert only fires on a bare Space).
        let corr = match action::commit(em, pasted.chars().count() + 1, action::CommitMode::Fast, false) {
            Ok(()) => {
                tracing::info!("paste-converted {pasted:?} → {converted:?}");
                // Record it so Ctrl-tap / undo restores the original pasted text. Undo deletes
                // the converted form (+ separator) and pastes `original` back.
                Some(Correction {
                    from: lang,
                    to: lang.other(),
                    converted: converted.clone(),
                    original: pasted.clone(),
                    trailing: Some(sep),
                })
            }
            Err(e) => {
                tracing::warn!("paste-convert commit failed: {e}");
                None
            }
        };
        action::clip_restore(saved);
        corr
    };
    Some((rest, correction))
}

/// Put the fd in non-blocking mode so [`drain`] never blocks.
fn set_nonblocking(fd: RawFd) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
    }
}

/// Block until the fd has events to read, with an optional timeout (`None` = forever).
/// Returns `(revents, timed_out)` — when `timed_out` is true, `revents` is 0 and the caller
/// should run its idle-tick work (e.g. fire a pending debounced correction) before re-arming
/// the wait.
fn wait_readable(fd: RawFd, timeout: Option<Duration>) -> (libc::c_short, bool) {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    let timeout_ms: libc::c_int = match timeout {
        None => -1,
        // Clamp to non-negative i32 (poll's "0 = return immediately" is the right behaviour
        // when the deadline has already passed).
        Some(d) => d.as_millis().min(i32::MAX as u128) as libc::c_int,
    };
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    (pfd.revents, rc == 0)
}

/// Read all currently-available key events (non-blocking). Empty when there are none.
fn drain(dev: &mut Device) -> Vec<(u16, i32)> {
    match dev.fetch_events() {
        Ok(events) => events
            .filter_map(|ev| match ev.kind() {
                InputEventKind::Key(k) => Some((k.code(), ev.value())),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn handle_hotkey(
    act: HotAction,
    em: &mut Emitter,
    buffer: &mut WordBuffer,
    last: &mut Option<(CompletedWord, (u16, bool))>,
    undo: &mut Option<Correction>,
    shared: &State,
    flags: &Flags,
) -> Result<()> {
    let dry = shared.lock().unwrap().cfg.dry_run;
    let lang = *flags.lang.lock().unwrap();
    // The current word, or — if already closed by a space — the last completed one.
    let take_word = |buffer: &mut WordBuffer, last: &mut Option<(CompletedWord, (u16, bool))>| {
        match buffer.finish(lang) {
            Some(w) => Some((w, None)),
            None => last.take().map(|(w, sep)| (w, Some(sep))),
        }
    };
    match act {
        HotAction::Undo => {
            if let Some(c) = undo.take() {
                action::undo(em, &c)?;
                shared.lock().unwrap().dict.add(&c.original, c.from, ListKind::Learned)?;
                tracing::info!("undid correction; learned {:?}", c.original);
            }
        }
        HotAction::ConvertLast => {
            // If there's a selection, convert it; otherwise convert the current/last word.
            let selection = crate::clipboard::get_primary().unwrap_or_default();
            if !selection.trim().is_empty() {
                if dry {
                    tracing::info!("[dry-run] would convert selection {:?}", selection);
                } else if let Err(e) = convert_selection(em, &selection) {
                    tracing::warn!("convert selection failed: {e}");
                }
            } else if let Some((w, trailing)) = take_word(buffer, last) {
                let to = w.lang.other();
                if dry {
                    tracing::info!("[dry-run] convert-last {:?} → {:?}", w.cur, w.alt);
                } else {
                    *undo = Some(action::apply_correction(em, &w, to, trailing)?);
                }
            }
        }
        HotAction::ForceCorrect => {
            if let Some((w, trailing)) = take_word(buffer, last) {
                let to = w.lang.other();
                if !dry {
                    *undo = Some(action::apply_correction(em, &w, to, trailing)?);
                }
                shared.lock().unwrap().dict.add(&w.cur, w.lang, ListKind::Force)?;
                tracing::info!("force-correct + remembered {:?}", w.cur);
            }
        }
        HotAction::AddException => {
            if let Some(c) = undo.take() {
                action::undo(em, &c)?;
                shared.lock().unwrap().dict.add(&c.original, c.from, ListKind::Manual)?;
                tracing::info!("reverted and excepted {:?}", c.original);
            } else if let Some(w) = buffer.snapshot(lang) {
                shared.lock().unwrap().dict.add(&w.cur, w.lang, ListKind::Manual)?;
                tracing::info!("excepted {:?}", w.cur);
            }
        }
    }
    Ok(())
}

/// Handle a modifier tap: **Ctrl** converts the current word, **Ctrl+Shift** converts the
/// selection (Punto-style triggers that work without a Pause/Break key).
fn handle_tap(
    mods: Mods,
    em: &mut Emitter,
    last_selection: &mut Option<LastSelection>,
    shared: &State,
) -> Result<()> {
    let only_ctrl = mods.ctrl && !mods.shift && !mods.alt && !mods.meta;
    let ctrl_shift = mods.ctrl && mods.shift && !mods.alt && !mods.meta;
    if !only_ctrl && !ctrl_shift {
        return Ok(());
    }
    let dry = shared.lock().unwrap().cfg.dry_run;

    if only_ctrl {
        // Ctrl tap → toggle the system keyboard layout (En↔Ru), like the language-switch key.
        if dry {
            tracing::info!("[dry-run] would switch keyboard layout");
        } else {
            em.switch_layout()?;
            tracing::info!("tap Ctrl → switched keyboard layout");
        }
    } else {
        // Ctrl+Shift tap → convert the selection. Guard against re-converting after the previous
        // conversion: PRIMARY may still hold the original OR may already hold the converted form
        // (Mutter re-highlights the pasted text). Both must skip, or we'd silently back-convert
        // `руддщ`→`hello` and paste the original back over what we just changed.
        let sel = crate::clipboard::get_primary().unwrap_or_default();
        if sel.trim().is_empty() {
            return Ok(());
        }
        if let Some(prev) = last_selection.as_ref() {
            if sel == prev.original || sel == prev.converted {
                tracing::debug!(
                    "Ctrl+Shift: selection matches last converted pair — skipping (PRIMARY is {})",
                    if sel == prev.original { "original" } else { "converted" }
                );
                return Ok(());
            }
        }
        if dry {
            tracing::info!("[dry-run] would convert selection {:?}", sel);
        } else {
            let converted = convert_selection(em, &sel)?;
            *last_selection = Some(LastSelection { original: sel, converted });
        }
    }
    Ok(())
}

/// Convert highlighted text (the PRIMARY selection) to the other layout and paste it over the
/// selection. The direction is guessed from the dominant script. Returns the converted text so
/// the caller can remember it alongside the original — needed because PRIMARY usually now holds
/// the converted form, and we mustn't back-convert it on a repeated tap.
fn convert_selection(em: &mut Emitter, text: &str) -> Result<String> {
    let cyrillic = text.chars().filter(|c| ('\u{0400}'..='\u{04FF}').contains(c)).count();
    let latin = text.chars().filter(|c| c.is_ascii_alphabetic()).count();
    let (from, to) = if cyrillic >= latin { (Lang::Ru, Lang::En) } else { (Lang::En, Lang::Ru) };

    let converted = crate::detect::translit::convert(text, from, to);
    let saved = action::clip_prepare(&converted)?;
    em.paste()?; // pasting over a selection replaces it
    action::clip_restore(saved);
    tracing::info!("converted selection ({from} → {to})");
    Ok(converted)
}

/// Read-only mouse watcher: any button press invalidates the trusted typing context.
pub fn run_mouse(mut dev: Device, flags: Flags) {
    loop {
        match dev.fetch_events() {
            Ok(events) => {
                for ev in events {
                    if let InputEventKind::Key(k) = ev.kind() {
                        if is_mouse_button(k.code()) && ev.value() == 1 {
                            flags.mouse_dirty.store(true, Ordering::SeqCst);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("mouse watcher stopped: {e}");
                return;
            }
        }
    }
}

fn is_mouse_button(code: u16) -> bool {
    // BTN_LEFT/RIGHT/MIDDLE/SIDE/EXTRA = 0x110..=0x114
    (0x110..=0x114).contains(&code)
}

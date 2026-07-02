//! uinput virtual keyboard: transparent re-emit of physical keys, plus the primitives the
//! correction sequence needs (backspace, replay key codes with shift).

use anyhow::{Context, Result};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AttributeSet, EventType, InputEvent, Key};

use std::time::Duration;

use crate::keymap::{
    KEY_BACKSPACE, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT, KEY_RIGHTSHIFT, KEY_SPACE,
};

const KEY_V: u16 = 47;

/// Small gap between injected key events of *different* keys (e.g. the Ctrl+V sequence). Distinct
/// keys aren't subject to auto-repeat coalescing, so a short gap is enough.
const KEY_EVENT_GAP: Duration = Duration::from_millis(10);

/// Gap between consecutive **identical** injected keys — specifically the Backspace burst that
/// deletes the word being corrected. A tight burst of the same key is mistaken by the
/// compositor/app for key auto-repeat and its *tail* is dropped, so the leftmost chars of the
/// original survive (`,блокнот`, `,kjблокнот` instead of `блокнот`). Spacing each tap above the
/// usual auto-repeat period (~30 keys/s ≈ 33 ms) makes every delete land. This is the one place
/// where identical keys are injected back-to-back, so only the Backspace path pays the cost.
const REPEAT_KEY_GAP_SAFE: Duration = Duration::from_millis(35);
/// Fast variant of [`REPEAT_KEY_GAP_SAFE`] used inside the keyboard grab. The grab makes the
/// user's concurrent typing impossible, so the *only* reason to space backspaces out is the
/// compositor's auto-repeat detection — which empirically reads ~33 ms between identical events
/// as "held". 20 ms stays comfortably under that threshold (~50 keys/s) on every compositor
/// we've tested, while shaving ~90 ms off a 6-char correction.
const REPEAT_KEY_GAP_FAST: Duration = Duration::from_millis(20);

/// How aggressively to inject a Backspace burst. The grab path uses `Fast`; passive
/// (no-grab) correction paths use `Safe` since type-ahead is possible there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackspaceSpeed {
    Safe,
    Fast,
}

pub struct Emitter {
    device: VirtualDevice,
}

impl Emitter {
    /// Create the virtual keyboard. Declares support for the whole key range so we can replay
    /// any physical key code verbatim.
    pub fn new() -> Result<Emitter> {
        let mut keys = AttributeSet::<Key>::new();
        for code in 1u16..=255 {
            keys.insert(Key::new(code));
        }
        let device = VirtualDeviceBuilder::new()
            .context("opening /dev/uinput (is the uinput module loaded and writable?)")?
            .name("puntu-virtual-keyboard")
            .with_keys(&keys)
            .context("declaring virtual keyboard keys")?
            .build()
            .context("building uinput device")?;
        Ok(Emitter { device })
    }

    /// Emit a single key event (value: 1=down, 0=up, 2=repeat). `emit` appends a SYN report.
    fn emit(&mut self, code: u16, value: i32) -> Result<()> {
        let ev = InputEvent::new(EventType::KEY, code, value);
        self.device.emit(&[ev]).context("emitting key event")?;
        Ok(())
    }

    /// Press and release a key, optionally with shift held.
    pub fn tap(&mut self, code: u16, shift: bool) -> Result<()> {
        if shift {
            self.emit(KEY_LEFTSHIFT, 1)?;
        }
        self.emit(code, 1)?;
        std::thread::sleep(KEY_EVENT_GAP);
        self.emit(code, 0)?;
        if shift {
            self.emit(KEY_LEFTSHIFT, 0)?;
        }
        Ok(())
    }

    /// Release both Shift keys. The user may be physically holding Shift when a correction fires —
    /// e.g. typing a separator like `_` (Shift+`-`), `?`, `:` — and without this our injected
    /// `Ctrl+V` reaches the app as `Ctrl+Shift+V`, which doesn't paste in most apps, so the
    /// backspaced word vanishes. We don't re-press: the user's own key-up keeps the state correct.
    /// (Alt/Meta can't be held here: a separator with them is a chord and never triggers a fix.)
    pub fn release_shift(&mut self) -> Result<()> {
        self.emit(KEY_LEFTSHIFT, 0)?;
        self.emit(KEY_RIGHTSHIFT, 0)?;
        Ok(())
    }

    /// Send `n` backspaces, spacing each tap by a gap so the compositor/app doesn't coalesce the
    /// burst as auto-repeat and drop its tail (which left a prefix of the original word,
    /// `,kjблокнот`). [`BackspaceSpeed::Safe`] uses the conservative 35 ms gap (passive path,
    /// where the user can type-ahead); `Fast` uses 20 ms inside a keyboard grab where
    /// type-ahead is physically impossible — that shaves ~15 ms per delete.
    pub fn backspace(&mut self, n: usize, speed: BackspaceSpeed) -> Result<()> {
        let gap = match speed {
            BackspaceSpeed::Safe => REPEAT_KEY_GAP_SAFE,
            BackspaceSpeed::Fast => REPEAT_KEY_GAP_FAST,
        };
        for i in 0..n {
            if i > 0 {
                std::thread::sleep(gap);
            }
            self.tap(KEY_BACKSPACE, false)?;
        }
        Ok(())
    }

    /// Replay a sequence of physical keys (code + shift).
    pub fn replay(&mut self, keys: &[(u16, bool)]) -> Result<()> {
        for &(code, shift) in keys {
            self.tap(code, shift)?;
        }
        Ok(())
    }

    /// Re-emit raw key events (`(code, value)` as read from evdev) to the app. Used after an
    /// atomic, *grabbed* correction to deliver the keystrokes the user made while we held the
    /// keyboard — they never reached the app.
    ///
    /// **Unmatched presses** (press captured but release lost — the evdev queue is 64 events
    /// deep, and a slow correction can let releases slip past the grab boundary) are replayed
    /// as a **synthesized press+release tap**: we emit the release ourselves immediately after
    /// the press, so the virtual keyboard never holds the key down (no compositor auto-repeat),
    /// but the keystroke still reaches the app. Earlier this path silently dropped unmatched
    /// presses to avoid keyboard-lockup — but that ate user keystrokes, most visibly the Space
    /// between fast-typed words, which silently merged them (`ghbdtnds` → `приветвы` instead of
    /// `привет вы`). Losing keystrokes is the worse failure mode in practice.
    ///
    /// Auto-repeat events (`value == 2`) are always dropped. The virtual device renders in a
    /// fixed (US) layout, which matches the active layout while typing wrong-layout words.
    pub fn replay_events(&mut self, events: &[(u16, i32)]) -> Result<()> {
        use std::collections::HashMap;

        // Pair each press with its matching release. `pending[code]` holds the indices of
        // presses we haven't yet matched. A release pops the most-recent unmatched press of
        // that code; that pair is kept.
        let mut pending: HashMap<u16, Vec<usize>> = HashMap::new();
        let mut keep: Vec<bool> = vec![false; events.len()];
        for (i, &(code, value)) in events.iter().enumerate() {
            match value {
                1 => {
                    pending.entry(code).or_default().push(i);
                }
                0 => {
                    if let Some(presses) = pending.get_mut(&code) {
                        if let Some(press_idx) = presses.pop() {
                            keep[press_idx] = true;
                            keep[i] = true;
                        }
                        // A bare release with no captured press is harmless — silently skip.
                    }
                }
                _ => {} // auto-repeat: ignored on both sides.
            }
        }

        // Unmatched presses (still in `pending`): emit each one as a synthesized tap so the
        // keystroke isn't lost. The synthesized release is what makes this safe: the virtual
        // keyboard returns to "no keys held" the moment we're done.
        let mut tap_after: std::collections::HashSet<usize> = std::collections::HashSet::new();
        let mut dropped_count = 0usize;
        for (_, indices) in pending.iter() {
            for &i in indices {
                keep[i] = true;
                tap_after.insert(i);
                dropped_count += 1;
            }
        }
        if dropped_count > 0 {
            tracing::debug!(
                "replay: {} press(es) had no matching release; synthesizing release to deliver \
                 the keystroke without holding the key",
                dropped_count
            );
        }

        // Replay the matched pairs. Do NOT `?` on a single emit failure — a stuck release
        // here is the precise condition that produced the original keyboard-lock bug. Log and
        // continue so every remaining release still gets attempted.
        for (i, &(code, value)) in events.iter().enumerate() {
            if !keep[i] {
                continue;
            }
            if let Err(e) = self.emit(code, value) {
                tracing::warn!("replay: emit({code}, {value}) failed: {e}");
            }
            std::thread::sleep(KEY_EVENT_GAP);
            // Unmatched press → synthesize the missing release right after so the virtual
            // device never holds a key down.
            if tap_after.contains(&i) {
                if let Err(e) = self.emit(code, 0) {
                    tracing::warn!("replay: synthesized release for code {code} failed: {e}");
                }
                std::thread::sleep(KEY_EVENT_GAP);
            }
        }
        Ok(())
    }

    /// Toggle the GNOME keyboard layout by injecting the default "switch input source" shortcut
    /// (`Super+Space`). Space is pressed *while Super is held* so Mutter sees the accelerator and
    /// NOT a bare Super tap (which would open the Activities overview). Small delays let the
    /// compositor register the sequence reliably.
    pub fn switch_layout(&mut self) -> Result<()> {
        self.emit(KEY_LEFTMETA, 1)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        self.emit(KEY_SPACE, 1)?;
        std::thread::sleep(std::time::Duration::from_millis(20));
        self.emit(KEY_SPACE, 0)?;
        self.emit(KEY_LEFTMETA, 0)?;
        Ok(())
    }

    /// Send Ctrl+V (paste). The 'v' key code is layout-independent for our purpose — what gets
    /// inserted is whatever text is on the clipboard, not a layout-rendered character.
    pub fn paste(&mut self) -> Result<()> {
        std::thread::sleep(KEY_EVENT_GAP);
        self.emit(KEY_LEFTCTRL, 1)?;
        std::thread::sleep(KEY_EVENT_GAP);
        self.emit(KEY_V, 1)?;
        std::thread::sleep(KEY_EVENT_GAP);
        self.emit(KEY_V, 0)?;
        std::thread::sleep(KEY_EVENT_GAP);
        self.emit(KEY_LEFTCTRL, 0)?;
        Ok(())
    }
}

//! IBus engine — receives the user's keystrokes from IBus and decides what to do.
//!
//! Strategy:
//! - Each "letter" key extends our [`WordBuffer`]; we return `true` so the app sees nothing
//!   yet, and emit `UpdatePreeditText` so the user still sees their typing in place.
//! - On a separator, we run the detector: Convert → the other-layout rendering, Leave → the
//!   word as typed. A hard separator (Enter/Tab) commits immediately in one atomic
//!   `commit_text` — no backspace, no clipboard, no race. A soft one (space) keeps the decided
//!   word **held in preedit** so the flip hotkey can still re-render it; it's committed when
//!   the next word starts, on a hard boundary, a chord, or a focus change.
//! - Backspace, navigation, chords pass through unchanged so app shortcuts still work.

use std::sync::Arc;

use librush::ibus::{IBusEngine, IBusEngineBackend, IBusFactory, IBusModifierState};
use tokio::sync::Mutex as AsyncMutex;
use tracing::debug;
use xkeysym::{KeyCode, Keysym};
use zbus::{fdo, object_server::SignalEmitter, ObjectServer};

/// A finished word **held in preedit** instead of committed, so the flip hotkey
/// (`Ctrl+` `` ` ``) can re-render it cleanly with NO deletion of committed text — the only
/// approach that's reliable across GTK/Qt/Chromium/Gecko on Wayland. It's committed for real
/// when the next word starts, on a hard boundary (Enter/Tab), or on focus change.
#[derive(Clone, Debug, Default)]
struct Held {
    /// What's currently shown in preedit (the decided rendering + trailing separator).
    shown: String,
    /// The other-layout rendering (+ separator) — the flip target.
    other: String,
    /// True when `shown` started as the detector's auto-converted rendering — flipping it
    /// back means the user rejected the conversion, which is worth learning.
    auto_converted: bool,
    /// The word exactly as typed (no separator) — what gets added to the learned list when
    /// the user flips an auto-conversion back.
    typed: String,
    /// Set once the rejection has been recorded, so repeated flips don't re-add it.
    learned: bool,
}

use crate::buffer::{CompletedWord, WordBuffer};
use crate::detect::userdict::{ListKind, UserDict};
use crate::detect::{Decision, Detector};
use crate::keymap::{self, KeyEvent, Lang, Mods};

/// What was tapped, if anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapKind {
    /// Just Ctrl pressed and released — toggle DirectRussian mode.
    Ctrl,
    /// Ctrl held + Shift pressed and released (or vice versa, in either order, with no
    /// non-modifier between). Convert the last word.
    CtrlShift,
}

/// Tap-detection state for the modifier-tap triggers.
///
/// IBus delivers modifier presses (Ctrl_L/Ctrl_R, Shift_L/Shift_R, etc.) as ordinary key
/// events. We track which modifiers have been pressed during the current "chain" (since the
/// last clean state) and fire on the matching final release — provided no non-modifier key
/// was pressed in between.
struct TapDetector {
    /// True while the chain hasn't been "spoiled" by a non-modifier key press.
    armed: bool,
    /// Each member that's currently held; the *peak* across the chain is remembered so a
    /// quick Ctrl+Shift tap (press both, then release both) fires `CtrlShift` regardless of
    /// release order.
    ctrl_down: u32,  // ref-count for L+R variants
    shift_down: u32,
    peak_ctrl: bool,
    peak_shift: bool,
    /// When the current chain started (first modifier press from a clean state).
    started: Option<std::time::Instant>,
    /// A chain longer than this is a held shortcut (Ctrl+click, an app-consumed chord…),
    /// not a deliberate tap — it must NOT fire. This is the main guard against accidental
    /// mode toggles / conversions when the app swallows the letter of a Ctrl-shortcut and
    /// we only ever see the modifier press + release.
    max_hold: std::time::Duration,
}

impl Default for TapDetector {
    fn default() -> Self {
        TapDetector::new(crate::config::DEFAULT_TAP_MAX_HOLD_MS)
    }
}

impl TapDetector {
    fn new(max_hold_ms: u64) -> Self {
        TapDetector {
            armed: false,
            ctrl_down: 0,
            shift_down: 0,
            peak_ctrl: false,
            peak_shift: false,
            started: None,
            max_hold: std::time::Duration::from_millis(max_hold_ms),
        }
    }
    fn cancel(&mut self) {
        self.armed = false;
        self.peak_ctrl = false;
        self.peak_shift = false;
    }
    fn ctrl_press(&mut self) {
        if self.ctrl_down == 0 && self.shift_down == 0 {
            self.armed = true;
            self.started = Some(std::time::Instant::now());
        }
        self.ctrl_down += 1;
        self.peak_ctrl = true;
    }
    fn shift_press(&mut self) {
        if self.ctrl_down == 0 && self.shift_down == 0 {
            self.armed = true;
            self.started = Some(std::time::Instant::now());
        }
        self.shift_down += 1;
        self.peak_shift = true;
    }
    /// Called on a modifier release. Returns the tap kind once *all* modifiers are released
    /// — that's the moment the gesture completes.
    fn ctrl_release(&mut self) -> Option<TapKind> {
        self.ctrl_down = self.ctrl_down.saturating_sub(1);
        self.maybe_fire()
    }
    fn shift_release(&mut self) -> Option<TapKind> {
        self.shift_down = self.shift_down.saturating_sub(1);
        self.maybe_fire()
    }
    fn maybe_fire(&mut self) -> Option<TapKind> {
        if self.ctrl_down > 0 || self.shift_down > 0 {
            return None; // still holding something
        }
        let quick = self
            .started
            .take()
            .is_some_and(|t| t.elapsed() <= self.max_hold);
        let fire = if !self.armed || !quick {
            None
        } else if self.peak_ctrl && self.peak_shift {
            Some(TapKind::CtrlShift)
        } else if self.peak_ctrl {
            Some(TapKind::Ctrl)
        } else {
            None // Shift-only tap isn't a recognised gesture
        };
        // Reset for the next chain.
        self.armed = false;
        self.peak_ctrl = false;
        self.peak_shift = false;
        fire
    }
}

/// Two modes the engine can be in. Toggled by a Ctrl tap.
///
/// `Correcting` is the default — user is typing English, we accumulate words and convert
/// "wrong-layout" ones to Russian.
///
/// `DirectRussian` is the Ctrl-tap-activated alternative — user IS typing Russian (but the
/// system layout is still `us`, because that's what activates our engine), so every English
/// letter is mapped key-for-key to its Russian counterpart and committed immediately. This
/// lets the user type Russian directly without needing a separate `xkb:ru::rus` input source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EngineMode {
    Correcting,
    DirectRussian,
}

impl EngineMode {
    fn toggle(self) -> Self {
        match self {
            EngineMode::Correcting => EngineMode::DirectRussian,
            EngineMode::DirectRussian => EngineMode::Correcting,
        }
    }
}

/// Resolved hotkey bindings — parsed from `IBusHotkeys` at startup. `None` for any field
/// means "disabled".
#[derive(Clone, Copy, Debug)]
pub struct HotkeyBindings {
    /// Full hotkey (keysym + modifiers). E.g. `Ctrl+grave` (default), `F12`, `Pause`.
    pub undo: Option<Hotkey>,
    pub mode_toggle_tap: Option<TapKind>,
    pub convert_last_tap: Option<TapKind>,
    /// Regular hotkey for selection conversion (not a tap). Use this if modifier-taps
    /// don't fire reliably — a normal keypress is unambiguous.
    pub convert_selection: Option<Hotkey>,
    /// Max press→release duration for a modifier tap to fire (see [`TapDetector::max_hold`]).
    pub tap_max_hold_ms: u64,
}

impl HotkeyBindings {
    /// Resolve the bindings from the full config: the `[ibus_hotkeys]` section plus the
    /// top-level `enable_modifier_taps` switch, which disables both tap gestures at once
    /// (same semantics as in the uinput daemon).
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        let hk = &cfg.ibus_hotkeys;
        let taps = cfg.enable_modifier_taps;
        HotkeyBindings {
            undo: parse_hotkey(&hk.undo_key),
            mode_toggle_tap: if taps { parse_tap_combo(&hk.mode_toggle) } else { None },
            convert_last_tap: if taps { parse_tap_combo(&hk.convert_last) } else { None },
            convert_selection: parse_hotkey(&hk.convert_selection_key),
            tap_max_hold_ms: cfg.tap_max_hold_ms,
        }
    }
}

/// One engine instance per focused input context. IBus calls `CreateEngine` whenever a new
/// text field gets focus and our engine is active for it.
pub struct PuntuEngine {
    detector: Arc<Detector>,
    /// User dictionaries — consulted by the detector on every finished word, and appended to
    /// (learned list) when the user flips an auto-conversion back.
    dict: Arc<AsyncMutex<UserDict>>,
    buffer: WordBuffer,
    /// Layout the user is virtually typing in for the **detector**. Always `En` because our
    /// engine is registered with `xkb:us` — IBus delivers Latin keysyms to us. (Russian mode
    /// is handled separately via [`EngineMode::DirectRussian`].)
    lang: Lang,
    id: u64,
    tap: TapDetector,
    mode: EngineMode,
    /// The just-finished word, kept in preedit (not committed) so the flip hotkey can
    /// re-render it without deleting committed text. See [`Held`].
    held: Option<Held>,
    /// Resolved hotkey bindings from config — undo key, mode-toggle tap, convert-last tap.
    hotkeys: HotkeyBindings,
    /// Run the detector on every finished word in Correcting mode (`!dry_run`). When off,
    /// words are held exactly as typed and only convert on the manual flip hotkey.
    autocorrect: bool,
    /// `IBusInputPurpose` of the focused field, delivered via the `ContentType` DBus
    /// property. Terminals (VTE sets TERMINAL) and password/PIN fields make the engine
    /// fully transparent — see [`Self::is_passthrough`].
    purpose: u32,
    /// True while an auxiliary-text hint is on screen, so the next letter can hide it.
    /// Shared (`Arc`) because the async selection-conversion task also shows hints.
    hint_shown: Arc<std::sync::atomic::AtomicBool>,
}

/// `IBusInputPurpose` values we care about (mirror `GtkInputPurpose`).
const PURPOSE_PASSWORD: u32 = 8;
const PURPOSE_PIN: u32 = 9;
const PURPOSE_TERMINAL: u32 = 10;

impl PuntuEngine {
    pub fn new(
        id: u64,
        detector: Arc<Detector>,
        dict: Arc<AsyncMutex<UserDict>>,
        hotkeys: HotkeyBindings,
        autocorrect: bool,
    ) -> Self {
        Self {
            detector,
            dict,
            buffer: WordBuffer::new(),
            lang: Lang::En,
            id,
            tap: TapDetector::new(hotkeys.tap_max_hold_ms),
            mode: EngineMode::Correcting,
            held: None,
            hotkeys,
            autocorrect,
            purpose: 0,
            hint_shown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Password / PIN fields: every keystroke passes through untouched — no buffering, no
    /// preedit, no hotkeys. A password must never sit in a preedit or be transliterated.
    ///
    /// Terminals are NOT in this list: there the rule is "no *automatic* conversions" (see
    /// [`Self::in_terminal`]) — manual use (Ctrl tap → RU-direct, `Ctrl+` `` ` `` flip) must
    /// keep working, and going fully transparent killed exactly that. Worse, it stuck: the
    /// daemon doesn't send a fresh `SetContentType` for clients that never set one, so after
    /// one terminal visit the engine stayed transparent in every app (hence the purpose
    /// reset in `focus_in`/`enable`).
    fn is_passthrough(&self) -> bool {
        matches!(self.purpose, PURPOSE_PASSWORD | PURPOSE_PIN)
    }

    /// Terminal field (VTE sets `InputPurpose::TERMINAL`): automatic conversions are off —
    /// the detector never rewrites a command line — but everything manual stays: mode
    /// toggle (RU-direct typing), the flip hotkey, preedit hold. Selection conversion is
    /// also blocked because terminals don't delete a selection on Backspace, so replacing
    /// it would append text instead.
    fn in_terminal(&self) -> bool {
        self.purpose == PURPOSE_TERMINAL
    }

    /// Show a short auxiliary-text hint near the caret (hidden again on the next letter).
    async fn show_hint(&mut self, se: &SignalEmitter<'_>, text: &str) {
        Self::show_hint_shared(se, &self.hint_shown, text).await;
    }

    /// [`Self::show_hint`] for contexts without `&mut self` (the async selection task).
    async fn show_hint_shared(
        se: &SignalEmitter<'_>,
        hint_shown: &std::sync::atomic::AtomicBool,
        text: &str,
    ) {
        let _ = Self::update_auxiliary_text(se, text.to_string(), true).await;
        hint_shown.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Hide the auxiliary hint if one is showing.
    async fn hide_hint(&mut self, se: &SignalEmitter<'_>) {
        if self.hint_shown.swap(false, std::sync::atomic::Ordering::Relaxed) {
            let _ = Self::update_auxiliary_text(se, String::new(), false).await;
        }
    }

    /// Commit `text`, logging (instead of swallowing) a DBus failure — a lost commit means
    /// lost user text, which must at least be visible in the logs.
    async fn commit_str(&self, se: &SignalEmitter<'_>, text: String) {
        if let Err(e) = Self::commit_text(se, text).await {
            tracing::warn!("[puntu-engine {}] commit_text failed: {e}", self.id);
        }
    }

    /// Handle a recognised modifier tap. Matches the tap kind against the configured
    /// `mode_toggle` and `convert_last` bindings, not hard-coded gestures — that way users
    /// can swap them (e.g. `mode_toggle = "Ctrl+Shift"` and `convert_last = "Ctrl"`) or
    /// disable one entirely with `"none"`.
    async fn handle_tap(&mut self, kind: TapKind, se: &SignalEmitter<'_>) {
        if self.hotkeys.mode_toggle_tap == Some(kind) {
            // CRITICAL: commit whatever the user was typing BEFORE toggling — otherwise the
            // preedit (the only place the in-progress word existed) is dropped on the floor
            // and the user sees their typing vanish. First the held (finished) word, then any
            // half-typed buffer; commit the buffer in the current mode's rendering.
            self.flush_held(se).await;
            if let Some(snap) = self.buffer.snapshot(self.lang) {
                let text = match self.mode {
                    EngineMode::Correcting => snap.cur,
                    EngineMode::DirectRussian => snap.alt,
                };
                if !text.is_empty() {
                    debug!(
                        "[puntu-engine {}] tap toggle: flushing in-progress {:?}",
                        self.id, text
                    );
                    self.commit_str(se, text).await;
                }
            }
            self.buffer.invalidate();
            // Explicitly clear preedit so the stale snapshot doesn't linger on screen.
            self.clear_preedit(se).await;
            self.mode = self.mode.toggle();
            let hint = match self.mode {
                EngineMode::Correcting => "EN auto-correct",
                EngineMode::DirectRussian => "RU direct",
            };
            debug!("[puntu-engine {}] {kind:?} tap → mode = {:?}", self.id, self.mode);
            self.show_hint(se, hint).await;
        } else if self.hotkeys.convert_last_tap == Some(kind) {
            // info-level: the tap not showing up in the logs at all means IBus never
            // delivered the modifier release events (known on some setups — use the
            // regular `convert_selection_key` hotkey there instead).
            tracing::info!("[puntu-engine {}] {kind:?} tap → convert selection", self.id);
            self.handle_convert_last(se);
        } else {
            // Recognised tap but no binding matches — silently ignore.
            debug!("[puntu-engine {}] {kind:?} tap → no binding", self.id);
        }
    }

    /// `Ctrl+` `` ` `` — flip the **held** word between its two layout readings. This is a pure
    /// preedit re-render: instant and reliable in every app, because it never deletes committed
    /// text (forwarded Backspaces / DeleteSurroundingText proved unreliable across GTK/Qt/
    /// Chromium/Gecko). If nothing is held (no word typed since the last commit), it's a no-op.
    ///
    /// Flipping an **auto-converted** word back is the user rejecting the correction, so the
    /// typed form is added to the learned list (once) and won't be auto-converted again.
    async fn handle_undo(&mut self, se: &SignalEmitter<'_>) {
        let Some(h) = self.held.as_mut() else {
            debug!("[puntu-engine {}] flip: nothing held", self.id);
            return;
        };
        std::mem::swap(&mut h.shown, &mut h.other);
        let shown = h.shown.clone();
        let learn = if h.auto_converted && !h.learned {
            h.learned = true;
            Some(h.typed.clone())
        } else {
            None
        };
        debug!("[puntu-engine {}] flip: held → {:?}", self.id, shown);
        self.update_preedit(se, &shown).await;
        if let Some(typed) = learn {
            // Correcting mode only auto-converts EN-rendered words, so the rejected form is EN.
            let mut dict = self.dict.lock().await;
            match dict.add(&typed, Lang::En, ListKind::Learned) {
                Ok(()) => tracing::info!(
                    "[puntu-engine {}] learned {typed:?} (undone auto-conversion)",
                    self.id
                ),
                Err(e) => tracing::warn!(
                    "[puntu-engine {}] could not persist learned word {typed:?}: {e}",
                    self.id
                ),
            }
        }
    }

    /// Ctrl+Shift tap → **convert the current mouse selection**. Read the highlighted text from
    /// PRIMARY (read-only), transliterate it, delete the selection with one forwarded
    /// `Backspace` (a single Backspace clears the whole selection), then commit the converted
    /// form over it. No clipboard write. No selection → hint, no-op.
    ///
    /// The whole thing runs in a **detached task**, after `process_key_event` has returned.
    /// Reading PRIMARY inline dead-locked: the compositor is still mid key event (waiting on
    /// our reply) and won't service `wl-paste`, so every read hit the 0.4 s timeout —
    /// `convert-selection: wl-paste failed (timeout/no owner)` on each attempt while the same
    /// command finished in ~100 ms from a shell.
    fn handle_convert_last(&mut self, se: &SignalEmitter<'_>) {
        if self.in_terminal() {
            // A terminal doesn't delete its selection on Backspace, so "replace" would
            // APPEND the converted text after the original (the pasted-command corruption).
            tracing::info!(
                "[puntu-engine {}] convert-selection: skipped (terminal field)",
                self.id
            );
            return;
        }
        // Do NOT flush the held word here: `commit_text` REPLACES an active selection in most
        // apps, so flushing would destroy the very selection we're about to convert. In
        // practice the mouse click that made the selection already triggered `reset()`, which
        // commits anything pending (see `flush_all`), so `held` is normally empty by now.
        let id = self.id;
        let detector = Arc::clone(&self.detector);
        let dict = Arc::clone(&self.dict);
        let hint_shown = Arc::clone(&self.hint_shown);
        let se = se.to_owned();
        tokio::spawn(async move {
            let selection =
                match tokio::task::spawn_blocking(move || read_primary_selection(id)).await {
                    Ok(sel) => sel,
                    Err(e) => {
                        tracing::warn!("[puntu-engine {id}] convert-selection task failed: {e}");
                        None
                    }
                };
            let Some(selection) = selection else {
                // The silent no-op here is what read as "Ctrl+Shift не работает" — say why.
                Self::show_hint_shared(&se, &hint_shown, "Puntu: нет выделения").await;
                return;
            };
            // Per-word detection first: only wrong-layout words convert; correctly-typed
            // words, punctuation and spacing stay. This is what fixes a mixed selection like
            // «почему то не переводит ghbdtn» — only the ghbdtn becomes привет, instead of
            // the whole phrase being transliterated into gibberish by dominant script.
            let converted = {
                let dict = dict.lock().await;
                crate::detect::convert_text(&selection, &detector, &dict)
            };
            // Fallback: the detector saw nothing to fix → the user wants a FORCE flip of text
            // that reads as valid (e.g. they typed real English but meant the Russian keys).
            // ONLY for a single non-command-shaped word: force-flipping a whole command line
            // (`code --ozone-platform=wayland …` still in PRIMARY while pasting into a
            // terminal with Ctrl+Shift+V, where the app swallows the V) appended garbage.
            let converted = match converted {
                Some(c) => c,
                None if force_translit_allowed(&selection) => force_translit(&selection),
                None => {
                    tracing::info!(
                        "[puntu-engine {id}] convert-selection: nothing to fix and selection \
                         is multi-word/command-shaped — skipping {selection:?}"
                    );
                    Self::show_hint_shared(
                        &se,
                        &hint_shown,
                        "Puntu: выделение не похоже на слово — не переведено",
                    )
                    .await;
                    return;
                }
            };
            if converted == selection {
                tracing::info!(
                    "[puntu-engine {id}] convert-selection: no change for {selection:?}"
                );
                Self::show_hint_shared(&se, &hint_shown, "Puntu: выделение уже в нужной раскладке")
                    .await;
                return;
            }
            tracing::info!(
                "[puntu-engine {id}] convert-selection: {selection:?} → {converted:?}"
            );
            forward_backspace(&se).await;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            if let Err(e) = Self::commit_text(&se, converted).await {
                tracing::warn!("[puntu-engine {id}] commit_text failed: {e}");
            }
        });
    }

    /// Commit the held word for real (it's now final) and clear the preedit. No-op if nothing
    /// is held. Called when the next word starts, on a hard boundary (Enter/Tab), a chord, or a
    /// focus change — so the pending word is never lost.
    async fn flush_held(&mut self, se: &SignalEmitter<'_>) {
        if let Some(h) = self.held.take() {
            debug!("[puntu-engine {}] flush held {:?}", self.id, h.shown);
            // Clear the preedit BEFORE committing. Some clients (Chromium/Electron with
            // text-input-v3) apply a trailing preedit-clear after the commit and clip the
            // just-committed word; clearing first sidesteps the reorder.
            self.clear_preedit(se).await;
            self.commit_str(se, h.shown).await;
        }
    }

    /// Commit EVERYTHING pending — the held word, then any half-typed buffer — and clear the
    /// preedit. The preedit is the only place this text exists; any lifecycle event that
    /// invalidates the context (reset, focus change, disable, navigation) must first turn it
    /// into real text, or the user watches their word vanish from the screen. That's exactly
    /// what happened on a mouse click: the app sent `reset()`, the old code dropped the held
    /// word, and the last typed word disappeared.
    async fn flush_all(&mut self, se: &SignalEmitter<'_>) {
        self.flush_held(se).await;
        if let Some(snap) = self.buffer.snapshot(self.lang) {
            let shown = match self.mode {
                EngineMode::Correcting => snap.cur,
                EngineMode::DirectRussian => snap.alt,
            };
            // Same ordering as `flush_held`: preedit off first, then commit.
            self.clear_preedit(se).await;
            if !shown.is_empty() {
                self.commit_str(se, shown).await;
            }
        }
        self.buffer.invalidate();
    }

    /// Show `text` as the preedit (cursor at end; hidden when empty).
    async fn update_preedit(&self, se: &SignalEmitter<'_>, text: &str) {
        let n = text.chars().count() as u32;
        let _ = Self::update_preedit_text(
            se,
            text.to_string(),
            n,
            !text.is_empty(),
            librush::ibus::IBusPreeditFocusMode::Commit,
        )
        .await;
    }

    /// Hide the preedit.
    async fn clear_preedit(&self, se: &SignalEmitter<'_>) {
        let _ = Self::update_preedit_text(
            se,
            String::new(),
            0,
            false,
            librush::ibus::IBusPreeditFocusMode::Commit,
        )
        .await;
    }

    /// Pick the `(shown, other, auto_converted)` renderings for a finished word per the
    /// current mode. `shown` is the default the engine holds; `other` is what `Ctrl+` `` ` ``
    /// flips to; `auto_converted` marks a detector-driven conversion (so flipping it back
    /// learns the typed form). Correcting runs the detector (trusted context, user dictionaries,
    /// command guard, trigram scoring — see [`Detector::decide`]); DirectRussian defaults to
    /// the Russian rendering unless the Latin reading is a real word/abbreviation and the
    /// Russian one isn't.
    async fn decide_renderings(&self, word: &CompletedWord) -> (String, String, bool) {
        match self.mode {
            EngineMode::Correcting => {
                if !self.autocorrect || self.in_terminal() {
                    // dry_run or a terminal: hold the word exactly as typed; conversion only
                    // on the manual flip. In a terminal an auto-rewrite of what turns out to
                    // be a command/flag is never acceptable — «в терминале только вручную».
                    return (word.cur.clone(), word.alt.clone(), false);
                }
                let dict = self.dict.lock().await;
                match self.detector.decide(word, &dict) {
                    Decision::Convert { .. } => {
                        debug!(
                            "[puntu-engine {}] auto-convert {:?} → {:?}",
                            self.id, word.cur, word.alt
                        );
                        (word.alt.clone(), word.cur.clone(), true)
                    }
                    Decision::Leave => (word.cur.clone(), word.alt.clone(), false),
                }
            }
            EngineMode::DirectRussian => {
                let cur_is_real_en = self.detector.is_known_word(&word.cur, self.lang);
                let alt_is_real_ru =
                    self.detector.is_known_word(&word.alt, self.lang.other());
                if cur_is_real_en && !alt_is_real_ru {
                    (word.cur.clone(), word.alt.clone(), false)
                } else {
                    (word.alt.clone(), word.cur.clone(), false)
                }
            }
        }
    }
}

impl IBusEngine for PuntuEngine {
    async fn process_key_event(
        &mut self,
        se: SignalEmitter<'_>,
        _server: &ObjectServer,
        keyval: Keysym,
        _keycode: KeyCode,
        state: IBusModifierState,
    ) -> fdo::Result<bool> {
        let released = state.release();
        // Trace EVERY incoming event so we can debug why a configured hotkey isn't firing.
        // Includes the raw keyval (hex) + modifier bits so we can see exactly what IBus
        // delivered for "Ctrl+`" or "Ctrl+Shift".
        debug!(
            "[puntu-engine {}] event: keysym={:?} (raw=0x{:04x}) state=0x{:08x} \
             ctrl={} shift={} alt={} super={} released={}",
            self.id,
            keyval,
            keyval.raw(),
            state.raw_value(),
            state.control(),
            state.shift(),
            state.mod1(),
            state.mod4(),
            released,
        );
        // Terminals / password / PIN fields: fully transparent, nothing below runs. This is
        // what guarantees a terminal never sees an auto-conversion (the user's hard rule).
        if self.is_passthrough() {
            return Ok(false);
        }
        // Undo hotkey (default `Ctrl+grave`, configurable via `ibus_hotkeys.undo_key`).
        // Matches on press with exact modifier state.
        if let Some(undo_hk) = self.hotkeys.undo {
            if undo_hk.matches(keyval, &state) && !released {
                debug!("[puntu-engine {}] undo hotkey matched", self.id);
                // The non-modifier press spoils any armed tap chain. Without this, the
                // Ctrl release *after* `Ctrl+grave` would fire the Ctrl tap and flip the
                // engine mode as a side effect of undoing.
                self.tap.cancel();
                self.handle_undo(&se).await;
                return Ok(true);
            }
        }
        // Convert-selection hotkey (default `Ctrl+Alt+s`). Same selection-conversion
        // semantics as the Ctrl+Shift tap, but as a regular keypress — can't be confused
        // with a chord by accident (the chord-vs-tap ambiguity is what made the tap version
        // unreliable on some setups).
        if let Some(sel_hk) = self.hotkeys.convert_selection {
            if sel_hk.matches(keyval, &state) && !released {
                debug!("[puntu-engine {}] convert-selection hotkey matched", self.id);
                self.tap.cancel(); // same reason as the undo hotkey above
                self.handle_convert_last(&se);
                return Ok(true);
            }
        }
        // Track modifier-tap chains. Both Ctrl and Shift are tracked; a release that
        // empties the chain may fire `Ctrl` (toggle mode) or `CtrlShift` (convert last).
        match keyval {
            Keysym::Control_L | Keysym::Control_R => {
                if released {
                    if let Some(kind) = self.tap.ctrl_release() {
                        self.handle_tap(kind, &se).await;
                    }
                } else {
                    self.tap.ctrl_press();
                }
                return Ok(false);
            }
            Keysym::Shift_L | Keysym::Shift_R => {
                if released {
                    if let Some(kind) = self.tap.shift_release() {
                        self.handle_tap(kind, &se).await;
                    }
                } else {
                    self.tap.shift_press();
                }
                return Ok(false);
            }
            _ => {}
        }
        if !matches!(
            keyval,
            Keysym::Alt_L
                | Keysym::Alt_R
                | Keysym::Super_L
                | Keysym::Super_R
                | Keysym::Caps_Lock
        ) && !released
        {
            // Any non-modifier press while a tap was armed turns it into a chord — cancel.
            self.tap.cancel();
        }
        // We only act on key presses. Releases pass through unchanged.
        if released {
            return Ok(false);
        }
        let mods = Mods {
            shift: state.shift(),
            ctrl: state.control(),
            alt: state.mod1(),
            meta: state.super_(),
        };
        // Chords (Ctrl+anything, Alt+anything, Super+anything) are shortcuts: flush the held
        // word so it isn't lost, drop any half-typed buffer, and forward so the app handles it.
        if mods.is_chord() {
            self.flush_held(&se).await;
            self.buffer.invalidate();
            return Ok(false);
        }

        let kev = classify_keysym(keyval, mods, self.lang);
        debug!(
            "[puntu-engine {}] keysym={:?} mode={:?} → {:?}",
            self.id, keyval, self.mode, kev
        );

        // Unified lazy-commit handling for both modes. A finished word is **held in preedit**
        // (not committed) until the next word starts, a hard boundary (Enter/Tab), a chord, or
        // a focus change — so `Ctrl+` `` ` `` can re-render it with no deletion of committed
        // text. `decide_renderings` picks the shown default and the flip target per mode.
        match kev {
            KeyEvent::Letter { .. } => {
                // Typing resumes: drop any lingering hint, commit the held word (it's now
                // final), then start the new one.
                self.hide_hint(&se).await;
                self.flush_held(&se).await;
                self.buffer.push(kev);
                if let Some(snap) = self.buffer.snapshot(self.lang) {
                    let shown = match self.mode {
                        EngineMode::Correcting => snap.cur,
                        EngineMode::DirectRussian => snap.alt,
                    };
                    self.update_preedit(&se, &shown).await;
                }
                Ok(true)
            }
            KeyEvent::Backspace => {
                if !self.buffer.is_empty() {
                    self.buffer.push(kev); // pops the last letter
                    let shown = self
                        .buffer
                        .snapshot(self.lang)
                        .map(|w| match self.mode {
                            EngineMode::Correcting => w.cur,
                            EngineMode::DirectRussian => w.alt,
                        })
                        .unwrap_or_default();
                    self.update_preedit(&se, &shown).await;
                    Ok(true)
                } else if self.held.is_some() {
                    // Backspace right after a held word: commit it, then let the Backspace
                    // delete from the now-real text (one user-initiated keystroke).
                    self.flush_held(&se).await;
                    Ok(false)
                } else {
                    Ok(false)
                }
            }
            KeyEvent::Separator => {
                let hard = matches!(keyval, Keysym::Return | Keysym::Tab | Keysym::KP_Enter);
                let raw_sep = keysym_to_char(keyval).unwrap_or(' ');
                // In RU-direct mode a separator key must render its Russian-layout character
                // (Shift+7 → `?`, Shift+4 → `;`, Shift+2 → `"` …), not the Latin keysym IBus
                // delivered. Chars on the same key in both layouts (space, digits) map to
                // themselves.
                let sep = match self.mode {
                    EngineMode::DirectRussian => {
                        crate::detect::translit::convert_char(raw_sep, Lang::En, Lang::Ru)
                    }
                    EngineMode::Correcting => raw_sep,
                };
                if let Some(word) = self.buffer.finish(self.lang) {
                    let (shown_word, other_word, auto_converted) =
                        self.decide_renderings(&word).await;
                    // Any previously held word is now final.
                    self.flush_held(&se).await;
                    if hard {
                        // Enter/Tab: commit the word immediately, then forward the key so the
                        // app acts on it (sends the message / inserts a tab).
                        self.commit_str(&se, shown_word).await;
                        self.clear_preedit(&se).await;
                        Ok(false)
                    } else {
                        // Space (soft): hold the word + separator in preedit, uncommitted, so
                        // the flip hotkey can still re-render it.
                        let held = Held {
                            shown: format!("{shown_word}{sep}"),
                            other: format!("{other_word}{sep}"),
                            auto_converted,
                            typed: word.cur.clone(),
                            learned: false,
                        };
                        self.update_preedit(&se, &held.shown).await;
                        self.held = Some(held);
                        Ok(true)
                    }
                } else if self.held.is_some() {
                    if hard {
                        self.flush_held(&se).await;
                        Ok(false)
                    } else {
                        // Extra separator after a held word — append it to the held preedit.
                        if let Some(h) = self.held.as_mut() {
                            h.shown.push(sep);
                            h.other.push(sep);
                        }
                        let shown =
                            self.held.as_ref().map(|h| h.shown.clone()).unwrap_or_default();
                        self.update_preedit(&se, &shown).await;
                        Ok(true)
                    }
                } else if sep != raw_sep {
                    // RU-direct punctuation with nothing buffered: the app can't render the
                    // Russian char from the forwarded Latin keysym, so commit it ourselves.
                    self.commit_str(&se, sep.to_string()).await;
                    Ok(true)
                } else {
                    // Nothing buffered or held — forward the separator.
                    Ok(false)
                }
            }
            KeyEvent::Invalidate => {
                // Navigation / Esc / Delete / Home / End. The cursor is about to move, so any
                // pending preedit MUST become real text first and the preedit MUST be cleared —
                // otherwise it lingers at the old spot, desyncs from the moved cursor, and the
                // next keystroke mangles the word ("стирается всё слово кроме той буквы").
                self.flush_all(&se).await;
                Ok(false)
            }
            KeyEvent::Ignore => Ok(false),
        }
    }

    async fn enable(
        &mut self,
        _se: SignalEmitter<'_>,
        _server: &ObjectServer,
    ) -> fdo::Result<()> {
        debug!("[puntu-engine {}] enable", self.id);
        self.buffer.invalidate();
        self.held = None;
        self.tap.cancel();
        // A new context: forget the previous one's purpose. Clients that care (terminals,
        // password fields) set it again right after; clients that don't would otherwise
        // inherit the stale value — one terminal visit left the engine transparent
        // EVERYWHERE until the next explicit SetContentType.
        self.purpose = 0;
        Ok(())
    }

    async fn disable(
        &mut self,
        se: SignalEmitter<'_>,
        _server: &ObjectServer,
    ) -> fdo::Result<()> {
        debug!("[puntu-engine {}] disable", self.id);
        // Switching away from the engine must not eat the word that only exists in preedit.
        self.flush_all(&se).await;
        self.tap.cancel();
        Ok(())
    }

    async fn focus_in(
        &mut self,
        _se: SignalEmitter<'_>,
        _server: &ObjectServer,
    ) -> fdo::Result<()> {
        debug!("[puntu-engine {}] focus_in", self.id);
        // Same reset as `enable`: purpose describes the field being left otherwise.
        self.purpose = 0;
        Ok(())
    }

    async fn focus_out(
        &mut self,
        se: SignalEmitter<'_>,
        _server: &ObjectServer,
    ) -> fdo::Result<()> {
        debug!("[puntu-engine {}] focus_out", self.id);
        // Commit the held word AND any half-typed buffer so nothing is lost when focus
        // leaves the field.
        self.flush_all(&se).await;
        // Drop any half-tracked modifier tap: a Ctrl held across a focus change (Ctrl+click,
        // window switch) must not fire the mode toggle when it's finally released.
        self.tap.cancel();
        Ok(())
    }

    async fn reset(
        &mut self,
        se: SignalEmitter<'_>,
        _server: &ObjectServer,
    ) -> fdo::Result<()> {
        debug!("[puntu-engine {}] reset", self.id);
        // Apps send `reset` on mouse clicks and cursor moves. The held word / half-typed
        // buffer exist ONLY in preedit at this point — dropping them here (the old behaviour)
        // is what made the last typed word visibly VANISH on a click ("слово пропало",
        // "стирается слово"). Commit them at the spot where the user already saw them instead.
        self.flush_all(&se).await;
        self.tap.cancel();
        Ok(())
    }

    fn set_content_type(&mut self, purpose: u32, hints: u32) -> fdo::Result<()> {
        if purpose != self.purpose {
            tracing::info!(
                "[puntu-engine {}] content type: purpose={purpose} hints=0x{hints:x}{}",
                self.id,
                if matches!(purpose, PURPOSE_PASSWORD | PURPOSE_PIN | PURPOSE_TERMINAL) {
                    " → transparent (terminal/password field)"
                } else {
                    ""
                }
            );
        }
        self.purpose = purpose;
        Ok(())
    }
}

/// IBus's factory: hands librush a `create_engine(name)` so it can spawn a fresh engine each
/// time a new input context activates ours. We share the immutable detector/dict so we don't
/// re-parse the dictionary per text field.
pub struct PuntuFactory {
    detector: Arc<Detector>,
    dict: Arc<AsyncMutex<UserDict>>,
    hotkeys: HotkeyBindings,
    autocorrect: bool,
    next_id: u64,
}

impl PuntuFactory {
    /// `dict` is shared: the caller keeps a clone for the hot-reload watcher, so `puntu dict
    /// add/learn/rm` edits reach every live engine without a restart.
    pub fn new(
        detector: Detector,
        dict: Arc<AsyncMutex<UserDict>>,
        hotkeys: HotkeyBindings,
        autocorrect: bool,
    ) -> Self {
        Self {
            detector: Arc::new(detector),
            dict,
            hotkeys,
            autocorrect,
            next_id: 1,
        }
    }
}

impl IBusFactory<PuntuEngine> for PuntuFactory {
    fn create_engine(&mut self, name: String) -> Result<PuntuEngine, String> {
        if name != crate::ibus::runtime::ENGINE_NAME {
            return Err(format!(
                "unknown engine {name:?}; this factory only serves {:?}",
                crate::ibus::runtime::ENGINE_NAME
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        debug!("CreateEngine({name}) → engine #{id}");
        Ok(PuntuEngine::new(
            id,
            Arc::clone(&self.detector),
            Arc::clone(&self.dict),
            self.hotkeys,
            self.autocorrect,
        ))
    }
}

/// Classify an IBus keysym the same way our evdev tokenizer does.
fn classify_keysym(keyval: Keysym, mods: Mods, lang: Lang) -> KeyEvent {
    use xkeysym::Keysym as K;
    match keyval {
        K::BackSpace => KeyEvent::Backspace,
        K::space | K::Return | K::Tab | K::KP_Enter => KeyEvent::Separator,
        // Numpad text keys are ALWAYS separators — the numpad never types letters, so the
        // main-row rule "'.' is ю in RU → part of a word" must not apply to KP_Decimal.
        // (Without any classification they were Ignore → forwarded, and the forwarded char
        // landed before the still-held preedit word: "+ctrl", "-порт".)
        K::KP_0 | K::KP_1 | K::KP_2 | K::KP_3 | K::KP_4 | K::KP_5 | K::KP_6 | K::KP_7
        | K::KP_8 | K::KP_9 | K::KP_Add | K::KP_Subtract | K::KP_Multiply | K::KP_Divide
        | K::KP_Decimal | K::KP_Separator | K::KP_Equal | K::KP_Space => KeyEvent::Separator,
        // Numpad navigation (NumLock off) moves the cursor exactly like the main-row keys —
        // it must invalidate too, or the preedit desyncs from the moved cursor.
        K::Escape
        | K::Left
        | K::Right
        | K::Up
        | K::Down
        | K::Home
        | K::End
        | K::Page_Up
        | K::Page_Down
        | K::Delete
        | K::Insert
        | K::KP_Left
        | K::KP_Right
        | K::KP_Up
        | K::KP_Down
        | K::KP_Home
        | K::KP_End
        | K::KP_Page_Up
        | K::KP_Page_Down
        | K::KP_Delete
        | K::KP_Insert
        | K::KP_Begin => KeyEvent::Invalidate,
        _ => {
            let Some(cur_char) = keysym_to_char(keyval) else {
                return KeyEvent::Ignore;
            };
            let Some((code, shift)) = keymap::find_key(cur_char, lang) else {
                return KeyEvent::Ignore;
            };
            let alt = keymap::char_for(code, shift, lang.other()).unwrap_or(cur_char);
            if cur_char.is_alphabetic() || alt.is_alphabetic() {
                KeyEvent::Letter { code, shift: mods.shift, cur: cur_char, alt }
            } else {
                KeyEvent::Separator
            }
        }
    }
}

// (Removed earlier `switch_layout_via_ibus` — switching IBus engines from inside an
// engine doesn't round-trip: once another engine activates, we no longer receive the next
// Ctrl-tap. Internal `EngineMode` toggle replaces it.)

/// Read the PRIMARY selection (mouse-highlighted text) **read-only**. Returns the selection
/// when it's usable, else `None` (no selection / multi-line / too long / failure). We never
/// write the clipboard.
///
/// PRIMARY is what mouse-selected text lands in on X11/Wayland; it stays in sync without an
/// explicit Ctrl+C, which is what makes this work without intercepting mouse events.
fn read_primary_selection(engine_id: u64) -> Option<String> {
    use std::process::Command;

    // Run under `timeout` so a `wl-paste` that hangs (no primary-selection owner) can't leak
    // a process. This runs in a detached task AFTER the key event completes, so it no longer
    // blocks input — 1.5 s gives the compositor time to service wl-paste even under load.
    let out = match Command::new("timeout")
        .args(["1.5", "wl-paste", "--primary", "--no-newline"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        Ok(o) => {
            // info-level on purpose: these are the answers to "I pressed the hotkey and
            // nothing happened", and the default log filter is `info`.
            tracing::info!(
                "[puntu-engine {engine_id}] convert-selection: wl-paste failed (timeout/no owner): {}",
                String::from_utf8_lossy(&o.stderr).trim()
            );
            return None;
        }
        Err(e) => {
            tracing::info!(
                "[puntu-engine {engine_id}] convert-selection: wl-paste not available ({e})"
            );
            return None;
        }
    };
    let selection = String::from_utf8_lossy(&out.stdout).trim_end_matches('\n').to_string();
    if selection.is_empty() {
        tracing::info!(
            "[puntu-engine {engine_id}] convert-selection: empty PRIMARY (nothing selected?)"
        );
        return None;
    }
    if selection.contains('\n') || selection.chars().count() > 500 {
        tracing::info!(
            "[puntu-engine {engine_id}] convert-selection: skipping large/multi-line selection ({} chars)",
            selection.chars().count()
        );
        return None;
    }
    Some(selection)
}

/// The force-flip fallback is allowed only for a single word (no whitespace) that doesn't
/// look like a command/path/flag — that's the deliberate "я выделил слово, переведи" case.
/// Anything longer or command-shaped only converts via the per-word detector, so a stale
/// PRIMARY selection can never be mangled wholesale by an accidental Ctrl+Shift.
fn force_translit_allowed(selection: &str) -> bool {
    !selection.chars().any(char::is_whitespace)
        && !crate::detect::userdict::is_command_context(selection)
}

/// Force-transliterate `s` key-for-key to the other layout, picking the direction by which
/// script dominates. Used only as the fallback when the per-word detector finds nothing to
/// fix — i.e. the user explicitly wants valid-looking text flipped anyway.
fn force_translit(s: &str) -> String {
    let cyrillic = s.chars().filter(|c| ('\u{0400}'..='\u{04FF}').contains(c)).count();
    let latin = s.chars().filter(|c| c.is_ascii_alphabetic()).count();
    let (from, to) = if cyrillic >= latin { (Lang::Ru, Lang::En) } else { (Lang::En, Lang::Ru) };
    crate::detect::translit::convert(s, from, to)
}

/// On Wayland the `ForwardKeyEvent` keycode is a **raw evdev code** (NOT the X11 evdev+8
/// convention): `KEY_BACKSPACE` = 14. A valid keycode is REQUIRED — keycode 0 is dropped, and
/// keycode 22 was wrong (that's `KEY_U`, so forwarded "Backspaces" typed `uuuu…`). Apps that
/// read the keyval instead of the keycode (Qt/Telegram) already worked; keycode-driven apps
/// (GTK, Chromium) need the correct evdev code here.
const KEYCODE_BACKSPACE: u32 = 14;
/// IBus modifier-state bit that marks a key *release* (vs press).
const RELEASE_MASK: u32 = 1 << 30;

/// Emit `org.freedesktop.IBus.Engine.ForwardKeyEvent(keyval, keycode, state)` — sends a real
/// key event to the focused app. Works in non-GTK apps (Chromium/Gecko) **only with a valid
/// keycode**, so callers must pass the right one (not 0).
async fn forward_key(
    se: &SignalEmitter<'_>,
    keyval: u32,
    keycode: u32,
    state: u32,
) -> zbus::Result<()> {
    se.emit("org.freedesktop.IBus.Engine", "ForwardKeyEvent", &(keyval, keycode, state))
        .await
}

/// Forward one Backspace as a press+release pair with a valid keycode. On a field with an
/// active selection a single Backspace deletes the whole selection; otherwise it deletes one
/// character before the cursor.
async fn forward_backspace(se: &SignalEmitter<'_>) {
    let bs = Keysym::BackSpace.raw();
    if let Err(e) = forward_key(se, bs, KEYCODE_BACKSPACE, 0).await {
        tracing::warn!("forwarding Backspace press failed: {e}");
    }
    if let Err(e) = forward_key(se, bs, KEYCODE_BACKSPACE, RELEASE_MASK).await {
        tracing::warn!("forwarding Backspace release failed: {e}");
    }
}

/// A parsed key-with-modifiers binding. Each field can be true even without the modifier
/// pressed elsewhere — they're requirements, not flags-on-press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hotkey {
    pub keysym: Keysym,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Hotkey {
    /// Does this hotkey match the given press? Checks the key AND the modifier state.
    ///
    /// **Case-insensitive ASCII** for letters: IBus delivers the *Shift-adjusted* keysym
    /// (Shift+c → keysym `C`, not `c`), but config stores the lowercase form (`Ctrl+Shift+c`).
    /// We normalise the incoming ASCII A-Z to a-z before comparing so `Ctrl+Shift+c` matches
    /// whether the keyboard delivered `c` or `C`.
    ///
    /// Extra modifiers cause a mismatch — `Ctrl+grave` doesn't fire when Ctrl+Alt+grave
    /// is pressed.
    pub fn matches(&self, keyval: Keysym, state: &IBusModifierState) -> bool {
        let normalised = ascii_keysym_to_lower(keyval);
        let stored_normalised = ascii_keysym_to_lower(self.keysym);
        stored_normalised == normalised
            && self.ctrl == state.control()
            && self.shift == state.shift()
            && self.alt == state.mod1()
            && self.meta == state.mod4()
    }
}

/// If `k` is an ASCII A-Z keysym, return its a-z counterpart; otherwise return `k`
/// unchanged. Used to make hotkey matching insensitive to Shift on letter keys.
fn ascii_keysym_to_lower(k: Keysym) -> Keysym {
    let raw = k.raw();
    if (0x41..=0x5A).contains(&raw) {
        Keysym::new(raw + 0x20) // 'A'..='Z' → 'a'..='z'
    } else {
        k
    }
}

/// Parse a hotkey string like `"Pause"`, `"F12"`, `"Ctrl+grave"`, `"Ctrl+Shift+u"` into a
/// [`Hotkey`]. The last `+`-separated segment is the key name; everything before are
/// modifiers (case-insensitive). Returns `None` for `"none"`/empty/unparseable input.
///
/// Recognised modifier names: `ctrl` / `control`, `shift`, `alt`, `super` / `meta` / `win`.
///
/// Key name resolution (in order):
///   1. Special "function" key whitelist (`Pause`, `F1`..`F12`, `Insert`, `Menu`,
///      `ScrollLock`, `Tab`, `Return`, `Escape`, `BackSpace`, `Space`, `Delete`).
///   2. Symbolic names for common punctuation (`grave` = `` ` ``, `slash`, `apostrophe`,
///      `comma`, `period`, `semicolon`, `minus`, `equal`, `bracketleft`/`right`).
///   3. A single ASCII char → its Unicode keysym (so `"u"`, `"a"`, `"5"` work as-is).
pub(crate) fn parse_hotkey(s: &str) -> Option<Hotkey> {
    let raw = s.trim();
    if raw.eq_ignore_ascii_case("none") || raw.is_empty() {
        return None;
    }
    let parts: Vec<&str> = raw.split('+').map(str::trim).filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return None;
    }
    let (key_name, mods) = parts.split_last()?;
    let mut hk = Hotkey { keysym: Keysym::NoSymbol, ctrl: false, shift: false, alt: false, meta: false };
    for m in mods {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => hk.ctrl = true,
            "shift" => hk.shift = true,
            "alt" | "mod1" => hk.alt = true,
            "super" | "meta" | "win" | "mod4" => hk.meta = true,
            _ => return None,
        }
    }
    hk.keysym = parse_keysym_universal(key_name)?;
    Some(hk)
}

/// Parse the key part of a hotkey — function keys, punctuation names, or a single ASCII
/// char. Case-insensitive for symbolic names; ASCII single chars pass through verbatim.
fn parse_keysym_universal(name: &str) -> Option<Keysym> {
    // Symbolic names first (case-insensitive). xkeysym has plenty more — this is the set
    // a user is likely to bind to.
    match name.to_ascii_lowercase().as_str() {
        // Function keys
        "pause" | "break" => return Some(Keysym::Pause),
        "scrolllock" | "scroll_lock" => return Some(Keysym::Scroll_Lock),
        "insert" | "ins" => return Some(Keysym::Insert),
        "delete" | "del" => return Some(Keysym::Delete),
        "menu" => return Some(Keysym::Menu),
        "tab" => return Some(Keysym::Tab),
        "return" | "enter" => return Some(Keysym::Return),
        "escape" | "esc" => return Some(Keysym::Escape),
        "backspace" => return Some(Keysym::BackSpace),
        "space" => return Some(Keysym::space),
        "home" => return Some(Keysym::Home),
        "end" => return Some(Keysym::End),
        "pageup" | "page_up" => return Some(Keysym::Page_Up),
        "pagedown" | "page_down" => return Some(Keysym::Page_Down),
        // Function keys f1..f12
        "f1" => return Some(Keysym::F1),
        "f2" => return Some(Keysym::F2),
        "f3" => return Some(Keysym::F3),
        "f4" => return Some(Keysym::F4),
        "f5" => return Some(Keysym::F5),
        "f6" => return Some(Keysym::F6),
        "f7" => return Some(Keysym::F7),
        "f8" => return Some(Keysym::F8),
        "f9" => return Some(Keysym::F9),
        "f10" => return Some(Keysym::F10),
        "f11" => return Some(Keysym::F11),
        "f12" => return Some(Keysym::F12),
        // Punctuation symbolic names (xkb convention)
        "grave" => return Some(Keysym::grave),
        "apostrophe" | "quote" => return Some(Keysym::apostrophe),
        "slash" => return Some(Keysym::slash),
        "backslash" => return Some(Keysym::backslash),
        "comma" => return Some(Keysym::comma),
        "period" | "dot" => return Some(Keysym::period),
        "semicolon" => return Some(Keysym::semicolon),
        "colon" => return Some(Keysym::colon),
        "minus" | "hyphen" | "dash" => return Some(Keysym::minus),
        "equal" | "equals" => return Some(Keysym::equal),
        "bracketleft" | "leftbracket" => return Some(Keysym::bracketleft),
        "bracketright" | "rightbracket" => return Some(Keysym::bracketright),
        _ => {}
    }
    // Single ASCII char fallback — `"u"`, `"a"`, `"5"`, `"\\"` etc. Lowercased so it
    // matches IBus's case-folded keysym for the key, regardless of Shift.
    let lower = name.to_ascii_lowercase();
    if lower.chars().count() == 1 {
        let c = lower.chars().next().unwrap();
        if c.is_ascii() {
            // Keysyms for printable ASCII are the Unicode code point itself.
            return Some(Keysym::new(c as u32));
        }
    }
    None
}

/// Backwards-compat shim — the rest of the code expects `parse_keysym_name` to return just
/// a Keysym for the simple cases. New code should call [`parse_hotkey`] instead.
#[allow(dead_code)]
pub(crate) fn parse_keysym_name(name: &str) -> Option<Keysym> {
    parse_hotkey(name).map(|h| h.keysym)
}

/// Parse a tap-modifier combo from config (`"Ctrl"`, `"Shift"`, `"Ctrl+Shift"`, `"none"`)
/// into a `TapKind` discriminator. Returns `None` for `"none"` (disabled).
pub(crate) fn parse_tap_combo(s: &str) -> Option<TapKind> {
    let parts: std::collections::BTreeSet<String> = s
        .split('+')
        .map(|p| p.trim().to_ascii_lowercase())
        .collect();
    if parts.contains("none") || parts.is_empty() {
        return None;
    }
    let only = |names: &[&str]| {
        parts.len() == names.len() && names.iter().all(|n| parts.contains(*n))
    };
    if only(&["ctrl"]) {
        Some(TapKind::Ctrl)
    } else if only(&["ctrl", "shift"]) {
        Some(TapKind::CtrlShift)
    } else {
        None
    }
}

/// Convert an IBus keysym to its Unicode character when one exists. For Latin-1 keysyms the
/// keysym IS the Unicode code point; for `0x01000000..` the low 24 bits are.
///
/// Numpad keysyms (NumLock on) produce text but live outside the Latin-1 range, so they're
/// mapped explicitly. Before this they classified as `Ignore` and were *forwarded* to the
/// app while the last word was still held (uncommitted) in preedit — the forwarded char
/// landed BEFORE the held word: typing `ctrl ` then numpad `+` produced `+ctrl `, and a
/// numpad `-` before `порт ` produced `-порт` (the user-reported reorder bugs).
fn keysym_to_char(keyval: Keysym) -> Option<char> {
    match keyval {
        Keysym::KP_0 => return Some('0'),
        Keysym::KP_1 => return Some('1'),
        Keysym::KP_2 => return Some('2'),
        Keysym::KP_3 => return Some('3'),
        Keysym::KP_4 => return Some('4'),
        Keysym::KP_5 => return Some('5'),
        Keysym::KP_6 => return Some('6'),
        Keysym::KP_7 => return Some('7'),
        Keysym::KP_8 => return Some('8'),
        Keysym::KP_9 => return Some('9'),
        Keysym::KP_Add => return Some('+'),
        Keysym::KP_Subtract => return Some('-'),
        Keysym::KP_Multiply => return Some('*'),
        Keysym::KP_Divide => return Some('/'),
        Keysym::KP_Decimal => return Some('.'),
        Keysym::KP_Separator => return Some(','),
        Keysym::KP_Equal => return Some('='),
        Keysym::KP_Space => return Some(' '),
        _ => {}
    }
    let raw = keyval.raw();
    if raw >= 0x01000000 {
        char::from_u32(raw & 0xffffff)
    } else if (0x20..0xff).contains(&raw) {
        char::from_u32(raw)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::translit::convert_char;

    #[test]
    fn ctrl_tap_fires_and_chord_cancels() {
        let mut tap = TapDetector::default();
        tap.ctrl_press();
        assert_eq!(tap.ctrl_release(), Some(TapKind::Ctrl));
        // A non-modifier press mid-chain (what process_key_event calls cancel() for)
        // must spoil the gesture.
        tap.ctrl_press();
        tap.cancel();
        assert_eq!(tap.ctrl_release(), None);
    }

    #[test]
    fn ctrl_shift_tap_fires_regardless_of_release_order() {
        let mut tap = TapDetector::default();
        tap.ctrl_press();
        tap.shift_press();
        assert_eq!(tap.ctrl_release(), None); // shift still held
        assert_eq!(tap.shift_release(), Some(TapKind::CtrlShift));

        tap.ctrl_press();
        tap.shift_press();
        assert_eq!(tap.shift_release(), None);
        assert_eq!(tap.ctrl_release(), Some(TapKind::CtrlShift));
    }

    #[test]
    fn cancel_mid_hold_survives_until_all_released() {
        // Focus change while Ctrl is held (Ctrl+click): cancel() must keep the eventual
        // release from firing the mode toggle.
        let mut tap = TapDetector::default();
        tap.ctrl_press();
        tap.cancel();
        assert_eq!(tap.ctrl_release(), None);
        // The next clean tap works again.
        tap.ctrl_press();
        assert_eq!(tap.ctrl_release(), Some(TapKind::Ctrl));
    }

    #[test]
    fn long_hold_does_not_fire_tap() {
        // A Ctrl (or Ctrl+Shift) held longer than `max_hold` is a shortcut the app may have
        // swallowed the letter of (Ctrl+Shift+V in a terminal) — it must NOT fire on release.
        let mut tap = TapDetector::new(500);
        tap.ctrl_press();
        tap.started = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        assert_eq!(tap.ctrl_release(), None);
        // The next quick tap still works.
        tap.ctrl_press();
        assert_eq!(tap.ctrl_release(), Some(TapKind::Ctrl));

        let mut tap = TapDetector::new(500);
        tap.ctrl_press();
        tap.shift_press();
        tap.started = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        assert_eq!(tap.shift_release(), None);
        assert_eq!(tap.ctrl_release(), None);
    }

    #[test]
    fn force_translit_fallback_is_gated() {
        // Deliberate case: a single selected word flips even when it reads as valid.
        assert!(force_translit_allowed("hello"));
        assert!(force_translit_allowed("привет"));
        // Command lines / multi-word selections never force-flip — this is what appended
        // `сщву --щящту…` after a Ctrl+Shift+V paste in a terminal.
        assert!(!force_translit_allowed("code --ozone-platform=wayland"));
        assert!(!force_translit_allowed("--force"));
        assert!(!force_translit_allowed("два слова"));
        assert!(!force_translit_allowed("v0.1"));
    }

    #[test]
    fn numpad_keys_are_text_or_navigation_not_ignore() {
        // KP_Add/KP_Subtract etc. produce text; classifying them Ignore forwarded the char
        // ahead of the held preedit word ("+ctrl", "-порт"). They must be Separators.
        for (k, c) in [
            (Keysym::KP_Add, '+'),
            (Keysym::KP_Subtract, '-'),
            (Keysym::KP_Multiply, '*'),
            (Keysym::KP_Divide, '/'),
            (Keysym::KP_5, '5'),
            (Keysym::KP_0, '0'),
            (Keysym::KP_Decimal, '.'),
        ] {
            assert_eq!(keysym_to_char(k), Some(c), "{k:?}");
            assert_eq!(
                classify_keysym(k, Mods::default(), Lang::En),
                KeyEvent::Separator,
                "{k:?} must classify as Separator"
            );
        }
        // NumLock-off numpad = navigation → must invalidate, same as the main-row keys.
        for k in [Keysym::KP_Home, Keysym::KP_Left, Keysym::KP_Page_Down, Keysym::KP_Delete] {
            assert_eq!(
                classify_keysym(k, Mods::default(), Lang::En),
                KeyEvent::Invalidate,
                "{k:?} must classify as Invalidate"
            );
        }
    }

    #[test]
    fn purpose_policy() {
        let hk = HotkeyBindings::from_config(&crate::config::Config::default());
        let dict = UserDict::empty(std::env::temp_dir().join("puntu-test-purpose"));
        let det = Detector::new(
            crate::detect::Models::default(),
            crate::config::DetectConfig::default(),
        );
        let mut e = PuntuEngine::new(
            1,
            Arc::new(det),
            Arc::new(AsyncMutex::new(dict)),
            hk,
            true,
        );
        // Passwords/PINs: fully transparent.
        for p in [PURPOSE_PASSWORD, PURPOSE_PIN] {
            e.purpose = p;
            assert!(e.is_passthrough(), "purpose {p} must be passthrough");
            assert!(!e.in_terminal());
        }
        // Terminals: NOT transparent — manual mode (RU-direct, flip) must keep working;
        // only automatic conversions are suppressed (checked in decide_renderings).
        e.purpose = PURPOSE_TERMINAL;
        assert!(!e.is_passthrough());
        assert!(e.in_terminal());
        // Ordinary fields.
        e.purpose = 0;
        assert!(!e.is_passthrough());
        assert!(!e.in_terminal());
    }

    #[test]
    fn enable_modifier_taps_off_disables_both_taps() {
        let mut cfg = crate::config::Config::default();
        cfg.enable_modifier_taps = false;
        let hk = HotkeyBindings::from_config(&cfg);
        assert_eq!(hk.mode_toggle_tap, None);
        assert_eq!(hk.convert_last_tap, None);
        // Regular (non-tap) hotkeys stay active.
        assert!(hk.undo.is_some());
        assert!(hk.convert_selection.is_some());

        cfg.enable_modifier_taps = true;
        let hk = HotkeyBindings::from_config(&cfg);
        assert_eq!(hk.mode_toggle_tap, Some(TapKind::Ctrl));
        assert_eq!(hk.convert_last_tap, Some(TapKind::CtrlShift));
    }

    #[test]
    fn direct_russian_separator_maps_to_ru_punctuation() {
        // The RU-direct separator remap (process_key_event's Separator arm) rides on
        // translit::convert_char; these are the keys whose RU rendering differs from EN.
        for (en, ru) in [('&', '?'), ('$', ';'), ('^', ':'), ('@', '"'), ('#', '№')] {
            assert_eq!(convert_char(en, Lang::En, Lang::Ru), ru);
        }
        // Space and digits sit on the same keys in both layouts — unchanged.
        assert_eq!(convert_char(' ', Lang::En, Lang::Ru), ' ');
        assert_eq!(convert_char('5', Lang::En, Lang::Ru), '5');
    }
}

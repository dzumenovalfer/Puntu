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
    /// Where `shown` came from, which decides what flipping it back *means*.
    source: HeldSource,
    /// The word exactly as typed (no separator) — what gets added to the learned list when
    /// the user flips an auto-conversion back.
    typed: String,
    /// The other-layout word (no separator) — the conversion target. Used by the manual-
    /// conversion counter and the remember hotkey to name the pair without re-deriving it
    /// from `shown`/`other` (those carry accumulated separators).
    converted: String,
    /// Set once the rejection has been recorded, so repeated flips don't re-add it.
    learned: bool,
    /// Set once a forward flip has been counted, so toggling back and forth on one word
    /// doesn't inflate the manual-conversion counter.
    counted: bool,
    /// Which hold this is. The idle-commit timer captures the value it was armed with and
    /// only commits while it still matches — so a timer left over from a word that has since
    /// been flushed, flipped or extended can never fire on the new one.
    generation: u64,
}

/// How the held rendering was arrived at. Flipping a word back with `Ctrl+` `` ` `` means
/// something different in each case, and the engine has to tell them apart:
///
/// * `Typed` — the detector left the word alone, so flipping is the user converting it by
///   hand; that feeds the "you keep converting this, remember it?" counter.
/// * `AutoConverted` — the detector rewrote it and the user is rejecting that, which is worth
///   learning (the typed form goes on the never-correct list).
/// * `Replacement` — the user's own `replacements.txt` expanded it. Neither reaction applies:
///   there is nothing to learn from undoing a rule they wrote themselves, and counting it as a
///   manual conversion would offer to "remember" a word they never typed.
///
/// An enum rather than a pair of bools, so the impossible fourth state can't be written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HeldSource {
    #[default]
    Typed,
    AutoConverted,
    Replacement,
}

/// The held word, shared with the idle-commit timer ([`PuntuEngine::arm_hold_timer`]).
/// A `std::sync::Mutex` on purpose: its guard is `!Send`, so the compiler rejects any attempt
/// to hold the lock across an `.await` — exactly the mistake that would deadlock the engine.
type HeldSlot = Arc<std::sync::Mutex<Option<Held>>>;

use crate::buffer::{CompletedWord, WordBuffer};
use crate::detect::userdict::{ListKind, UserDict};
use crate::detect::{Decision, Detector};
use crate::keymap::{self, KeyEvent, Lang, Mods};

// `ModCombo` and its parser live in `config` so BOTH front-ends can honour the configured
// gesture — the uinput daemon builds without the `ibus` feature.
pub use crate::config::{parse_tap_combo, ModCombo};

/// The four tracked modifiers (index into [`TapDetector`] arrays).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mod {
    Ctrl = 0,
    Shift = 1,
    Alt = 2,
    Super = 3,
}

/// Tap-detection state for the modifier-tap triggers.
///
/// IBus delivers modifier presses (Ctrl_L/Ctrl_R, Shift_L/Shift_R, …) as ordinary key
/// events. We track which modifiers have been pressed during the current "chain" (since the
/// last clean state) and fire the peak combo on the final release — provided no
/// non-modifier key was pressed in between.
struct TapDetector {
    /// True while the chain hasn't been "spoiled" by a non-modifier key press.
    armed: bool,
    /// Held ref-count per modifier (L+R variants).
    down: [u32; 4],
    /// The *peak* across the chain, so a quick Ctrl+Shift tap (press both, release both)
    /// fires `Ctrl+Shift` regardless of release order.
    peak: [bool; 4],
    /// When the current chain started (first modifier press from a clean state).
    started: Option<std::time::Instant>,
    /// A single-modifier chain longer than this is a held shortcut (Ctrl+click, an
    /// app-consumed chord…), not a deliberate tap — it must NOT fire. Multi-modifier combos
    /// are deliberate by construction and are exempt.
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
            down: [0; 4],
            peak: [false; 4],
            started: None,
            max_hold: std::time::Duration::from_millis(max_hold_ms),
        }
    }
    fn cancel(&mut self) {
        self.armed = false;
        self.peak = [false; 4];
    }
    /// Adopt a new `tap_max_hold_ms` from a reloaded config. Only the threshold changes; an
    /// in-flight chain keeps its ref-counts, so retuning this mid-gesture can't strand a
    /// modifier as permanently held.
    fn set_max_hold(&mut self, ms: u64) {
        let max_hold = std::time::Duration::from_millis(ms);
        if self.max_hold != max_hold {
            self.max_hold = max_hold;
        }
    }
    /// `was_down` = the modifier bit from the event's state, which reflects the state
    /// BEFORE this press. `false` with a non-zero ref-count means we missed a release
    /// (it happened while focus was elsewhere — Ctrl+click into another window). Resync,
    /// or the count never returns to zero and taps go PERMANENTLY dead until restart.
    fn press(&mut self, m: Mod, was_down: bool) {
        let i = m as usize;
        if !was_down {
            self.down[i] = 0;
        }
        if self.down.iter().all(|&d| d == 0) {
            self.armed = true;
            self.started = Some(std::time::Instant::now());
        }
        self.down[i] += 1;
        self.peak[i] = true;
    }
    /// Forget everything, including the held ref-counts — for lifecycle events (focus
    /// change, enable/disable) after which pending releases may never arrive.
    fn hard_reset(&mut self) {
        self.down = [0; 4];
        self.cancel();
    }
    /// Called on a modifier release. Returns the peak combo once *all* modifiers are
    /// released — that's the moment the gesture completes.
    fn release(&mut self, m: Mod) -> Option<ModCombo> {
        let i = m as usize;
        self.down[i] = self.down[i].saturating_sub(1);
        self.maybe_fire()
    }
    fn maybe_fire(&mut self) -> Option<ModCombo> {
        if self.down.iter().any(|&d| d > 0) {
            return None; // still holding something
        }
        let quick = self
            .started
            .take()
            .is_some_and(|t| t.elapsed() <= self.max_hold);
        let combo = ModCombo {
            ctrl: self.peak[0],
            shift: self.peak[1],
            alt: self.peak[2],
            sup: self.peak[3],
        };
        // Multi-modifier combos are deliberate gestures — no hold limit (users pause while
        // looking at the selection). Single-modifier taps must be quick, or a held shortcut
        // whose letter the app swallowed would toggle the mode. A bare-Shift tap is never a
        // gesture (it's an aborted capital letter) — the config parser refuses it too.
        let bare_shift = combo == (ModCombo { shift: true, ..Default::default() });
        let fire = if !self.armed || combo.size() == 0 || bare_shift {
            None
        } else if combo.size() >= 2 || quick {
            Some(combo)
        } else {
            None
        };
        // Reset for the next chain.
        self.armed = false;
        self.peak = [false; 4];
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
    pub mode_toggle_tap: Option<ModCombo>,
    pub convert_last_tap: Option<ModCombo>,
    /// Regular key (not a tap) that toggles the EN↔RU mode — for users who want a
    /// GNOME-Tweaks-style switch key (`Pause`, `CapsLock`, `Super+space`, …).
    pub mode_toggle_key: Option<Hotkey>,
    /// Regular hotkey for selection conversion (not a tap). Use this if modifier-taps
    /// don't fire reliably — a normal keypress is unambiguous.
    pub convert_selection: Option<Hotkey>,
    /// Remember a word in the dictionary (mouse selection, else the held word).
    pub remember: Option<Hotkey>,
    /// Cycle the case of the held word (`слово` → `Слово` → `СЛОВО`).
    pub case: Option<Hotkey>,
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
            mode_toggle_key: parse_hotkey(&hk.mode_toggle_key),
            convert_selection: parse_hotkey(&hk.convert_selection_key),
            remember: parse_hotkey(&hk.remember_key),
            case: parse_hotkey(&hk.case_key),
            tap_max_hold_ms: cfg.tap_max_hold_ms,
        }
    }
}

/// One engine instance per focused input context. IBus calls `CreateEngine` whenever a new
/// text field gets focus and our engine is active for it.
pub struct PuntuEngine {
    detector: DetectorSlot,
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
    held: HeldSlot,
    /// Bumped every time a word is held (or the held word is extended), so a stale
    /// idle-commit timer can tell it no longer owns what's in [`Self::held`].
    held_gen: u64,
    /// Everything from `config.toml` — hotkeys, autocorrect, case fixing, the idle-commit
    /// delay, the client policy. Shared and swapped whole by the config watcher, so an edit
    /// takes effect on the next keystroke instead of on the next `ibus restart`.
    settings: SettingsSlot,
    /// `IBusInputPurpose` of the focused field, delivered via the `ContentType` DBus
    /// property. Terminals (VTE sets TERMINAL) and password/PIN fields make the engine
    /// fully transparent — see [`Self::is_passthrough`].
    purpose: u32,
    /// True while an auxiliary-text hint is on screen, so the next letter can hide it.
    /// Shared (`Arc`) because the async selection-conversion task also shows hints.
    hint_shown: Arc<std::sync::atomic::AtomicBool>,
    /// Tray pause: while set, every keystroke passes through untouched. Flipped by the
    /// config-dir watcher when the `paused` marker file appears/disappears.
    paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Manual-conversion counter per converted word (shared across engines): after
    /// `suggest_after` manual conversions of the same word, a zenity dialog offers to
    /// remember it. Value = (count, last typed form — for the dialog text).
    convert_counts: ConvertCounts,
    /// The last (original → converted) selection pair — the stale-PRIMARY guard.
    last_converted: LastConverted,
    /// The client-reported text around the caret with the selection bounds
    /// (`SetSurroundingText`): (text, cursor, anchor) in chars. The IM-native way to read
    /// the current selection — Chromium/Electron (Claude, VS Code) send it, while their
    /// Wayland PRIMARY publishing is unreliable.
    surrounding: Option<(String, u32, u32)>,
    /// Were we transparent (paused / password field) at the previous key event? Used to spot
    /// the moment transparency switches ON, which is when anything still pending has to be
    /// committed — see [`Self::become_transparent`].
    was_transparent: bool,
    /// The client's `IBusCapabilite` bits, or `None` until it reports them. Absent is treated
    /// as capable: IBus older than the `SetCapabilities` call must not silently disable us.
    caps: Option<u32>,
    /// The focused client's name, as it passed it to `CreateInputContext` (`"gtk-im"`,
    /// `"xim"`, `"SDL2_Application"`, …). Empty until `FocusInId` reports one.
    client: String,
    /// The `(client, caps)` pair last written to the log, so the line below is printed once
    /// per distinct combination instead of on every report.
    logged: Option<(String, Option<u32>)>,
    /// Consecutive failed/timed-out DBus emits. Reset by any success.
    emit_failures: u32,
    /// Latched once [`MAX_EMIT_FAILURES`] emits fail in a row: the engine gives up and goes
    /// transparent so the user can at least type. Cleared on `focus_in`/`enable`.
    degraded: bool,
}

/// Shared manual-conversion counter: converted word → (count, last typed form).
type ConvertCounts = Arc<std::sync::Mutex<std::collections::HashMap<String, (u32, String)>>>;

/// The detector, swappable so the dictionary watcher can rebuild the language models while
/// engines are live. Readers clone the inner `Arc` under a brief read lock and then use it
/// freely — including across `.await`, which a lock guard could never survive.
pub type DetectorSlot = Arc<std::sync::RwLock<Arc<Detector>>>;

/// The last (original → converted, when) selection triple, shared across engines. Used to
/// refuse converting a STALE primary selection — see [`is_stale_selection`].
type LastConverted = Arc<std::sync::Mutex<Option<(String, String, std::time::Instant)>>>;

/// Lock a mutex, ignoring poisoning.
///
/// These mutexes guard plain data (the held word, the conversion tallies) that is rebuilt on
/// the next keystroke anyway, so a panic elsewhere leaves nothing genuinely inconsistent. What
/// it *would* leave, with `.unwrap()`, is every later key event panicking on the poison — one
/// bad moment turning into a keyboard that stays broken until the daemon is restarted, which
/// is the exact failure mode we are here to remove.
fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// How long after a conversion a matching PRIMARY text is treated as residue. The harmful
/// replay (tap while typing the next word) happens within seconds; a DELIBERATE re-selection
/// of the same word later must convert again — «стало хуже переводить» when it didn't.
const STALE_PAIR_WINDOW: std::time::Duration = std::time::Duration::from_secs(15);

/// The selected span of the client-reported surrounding text, if any: the chars between
/// `cursor` and `anchor` (either order). `None` when there is no selection.
fn surrounding_selection(sur: &Option<(String, u32, u32)>) -> Option<String> {
    let (text, cursor, anchor) = sur.as_ref()?;
    let (a, b) = (*cursor.min(anchor) as usize, *cursor.max(anchor) as usize);
    if a == b {
        return None;
    }
    let sel: String = text.chars().skip(a).take(b - a).collect();
    (!sel.is_empty()).then_some(sel)
}

/// Is `sel` just the residue of the previous conversion still sitting in PRIMARY? After a
/// replacement the selection buffer keeps the OLD text (the replaced original — or, in some
/// apps, the inserted form). Wayland has no "selection cleared" signal we could use, and
/// apps that never publish PRIMARY at all (egui/winit — Puntu's own window) leave whatever
/// was there before. Converting that residue re-inserted the previous word at the caret —
/// the reported «вставка переведённого слова при наборе следующего». The uinput daemon has
/// carried the same guard since M2 (capture.rs: "selection matches last converted pair").
fn is_stale_selection(last: &Option<(String, String, std::time::Instant)>, sel: &str) -> bool {
    last.as_ref().is_some_and(|(orig, conv, when)| {
        let s = sel.trim();
        when.elapsed() <= STALE_PAIR_WINDOW && (s == orig.trim() || s == conv.trim())
    })
}

/// `IBusInputPurpose` values we care about (mirror `GtkInputPurpose`).
const PURPOSE_PASSWORD: u32 = 8;
const PURPOSE_PIN: u32 = 9;
const PURPOSE_TERMINAL: u32 = 10;

/// `IBusCapabilite` bit for "this client displays `UpdatePreeditText`". Everything the user
/// types lives in the preedit until it is committed, so without this bit the client shows
/// **nothing at all** while typing — the keyboard looks dead even though the engine is
/// happily processing every key.
const CAP_PREEDIT_TEXT: u32 = 1 << 0;

/// Ceiling on any single DBus emit made while answering a key event. The client is blocked on
/// our `ProcessKeyEvent` reply for as long as we sit in there, so an emit that never completes
/// is a keyboard that never responds. Past this, the emit is treated as failed and we get out
/// of the way. Generous next to a healthy emit (microseconds) and short enough that a user
/// would read a one-off as a hiccup rather than a freeze.
const EMIT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

/// Consecutive failed emits after which the engine stops trying and turns transparent for the
/// rest of the focus. Corrections stop working; typing keeps working — which is the right way
/// round. One-offs (a busy daemon) are absorbed by the counter reset on the next success.
const MAX_EMIT_FAILURES: u32 = 3;

/// The plain-data engine settings, bundled so the constructor doesn't grow an argument per
/// config key, and so a reload can swap the whole set at once.
#[derive(Clone, Debug)]
pub struct EngineOptions {
    /// Run the detector on every finished word in Correcting mode (`!dry_run`).
    pub autocorrect: bool,
    /// Fix accidental-caps signatures (`пРИВЕТ`, `ПРивет`) on finished words.
    pub fix_case: bool,
    /// `[learning] suggest_after`; 0 disables the "remember this word?" offer.
    pub suggest_after: u32,
    /// How long a finished word may sit in preedit before it commits itself; 0 disables.
    pub hold_commit_ms: u64,
    /// Which clients the engine refuses to touch (games, clients with no preedit).
    pub clients: crate::config::ClientPolicy,
}

impl EngineOptions {
    /// Read the options out of a loaded config. `dry_run` doubles as the auto-correct kill
    /// switch: detect-but-don't-touch means words are held exactly as typed and only convert
    /// on the manual flip hotkey.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        EngineOptions {
            autocorrect: !cfg.dry_run,
            fix_case: cfg.fix_case,
            suggest_after: cfg.learning.suggest_after,
            hold_commit_ms: cfg.hold_commit_ms,
            clients: cfg.ibus_clients.clone(),
        }
    }

    /// The idle-commit delay, or `None` when the timer is disabled (`hold_commit_ms = 0`).
    fn hold_commit(&self) -> Option<std::time::Duration> {
        (self.hold_commit_ms > 0).then(|| std::time::Duration::from_millis(self.hold_commit_ms))
    }
}

/// Everything an engine reads out of `config.toml`, resolved once per reload.
///
/// Engines hold the [`SettingsSlot`], not a copy: a config edit swaps the `Arc` inside and
/// every live engine sees it on its next keystroke. Copying these into each engine at
/// creation is what made changing a setting require an `ibus restart` — which drops the
/// engine out of every window for a couple of seconds and loses the word held in preedit.
#[derive(Clone, Debug)]
pub struct EngineSettings {
    pub hotkeys: HotkeyBindings,
    pub opts: EngineOptions,
}

impl EngineSettings {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        EngineSettings {
            hotkeys: HotkeyBindings::from_config(cfg),
            opts: EngineOptions::from_config(cfg),
        }
    }
}

/// The live settings, swappable by the config watcher while engines are running. Same shape
/// and the same reasoning as [`DetectorSlot`]: readers clone the inner `Arc` under a brief
/// read lock and are then free to `.await` — which a lock guard could never survive.
pub type SettingsSlot = Arc<std::sync::RwLock<Arc<EngineSettings>>>;

/// Why the engine is staying out of the way, for the log line — and `None` when it isn't.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Passthrough {
    /// Password / PIN field: a password must never sit in a preedit or be transliterated.
    Secret,
    /// The client never renders a preedit, so anything held there is invisible.
    NoPreedit,
    /// The client is on the configured passthrough list (SDL games and friends).
    Client,
    /// Too many emits failed in a row — see [`MAX_EMIT_FAILURES`].
    Degraded,
}

impl PuntuEngine {
    pub fn new(
        id: u64,
        detector: DetectorSlot,
        dict: Arc<AsyncMutex<UserDict>>,
        settings: SettingsSlot,
        paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
        convert_counts: ConvertCounts,
        last_converted: LastConverted,
    ) -> Self {
        let tap_max_hold_ms = settings
            .read()
            .map(|s| s.hotkeys.tap_max_hold_ms)
            .unwrap_or(crate::config::DEFAULT_TAP_MAX_HOLD_MS);
        Self {
            detector,
            dict,
            buffer: WordBuffer::new(),
            lang: Lang::En,
            id,
            tap: TapDetector::new(tap_max_hold_ms),
            mode: EngineMode::Correcting,
            held: Arc::new(std::sync::Mutex::new(None)),
            held_gen: 0,
            settings,
            purpose: 0,
            hint_shown: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            paused,
            convert_counts,
            last_converted,
            surrounding: None,
            was_transparent: false,
            caps: None,
            client: String::new(),
            logged: None,
            emit_failures: 0,
            degraded: false,
        }
    }

    /// The settings in force right now. Cloning the `Arc` out from under the read lock keeps
    /// the lock held for nanoseconds and leaves the caller free to `.await` while using it —
    /// the same trick as [`Self::detector`].
    fn settings(&self) -> Arc<EngineSettings> {
        Arc::clone(&self.settings.read().unwrap_or_else(|e| e.into_inner()))
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
        self.passthrough_reason().is_some()
    }

    /// Why the engine is transparent right now, or `None` when it isn't. Split out from
    /// [`Self::is_passthrough`] so the log line can say *which* rule fired — "Puntu ничего не
    /// делает в этом приложении" is unanswerable otherwise.
    fn passthrough_reason(&self) -> Option<Passthrough> {
        if matches!(self.purpose, PURPOSE_PASSWORD | PURPOSE_PIN) {
            return Some(Passthrough::Secret);
        }
        let clients = &self.settings().opts.clients;
        // A client that never draws a preedit would show nothing at all while the user types,
        // because that is the only place a word lives before it is committed.
        if clients.require_preedit_capability
            && self.caps.is_some_and(|c| c & CAP_PREEDIT_TEXT == 0)
        {
            return Some(Passthrough::NoPreedit);
        }
        // Games (SDL opens an IBus context for its text input, so WASD arrives here looking
        // exactly like typing) and anything else the user listed.
        if clients.matches_client(&self.client) {
            return Some(Passthrough::Client);
        }
        if self.degraded {
            return Some(Passthrough::Degraded);
        }
        None
    }

    /// Terminal field (VTE sets `InputPurpose::TERMINAL`): automatic conversions are off —
    /// the detector never rewrites a command line — but everything manual stays: mode
    /// toggle (RU-direct typing), the flip hotkey, preedit hold. Selection conversion is
    /// also blocked because terminals don't delete a selection on Backspace, so replacing
    /// it would append text instead.
    fn in_terminal(&self) -> bool {
        self.purpose == PURPOSE_TERMINAL
    }

    /// The current detector. Cloning the `Arc` out from under the read lock keeps the lock
    /// held for nanoseconds and leaves the caller free to `.await` while using it.
    fn detector(&self) -> Arc<Detector> {
        Arc::clone(&self.detector.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Is a finished word currently held in preedit?
    fn is_holding(&self) -> bool {
        lock(&self.held).is_some()
    }

    /// Take the held word out of the shared slot (also disarming any pending idle-commit
    /// timer, which checks the generation before touching anything).
    fn take_held(&mut self) -> Option<Held> {
        lock(&self.held).take()
    }

    /// Hold `held` in preedit and (re)start the idle-commit countdown for it.
    ///
    /// A finished word lives ONLY in the preedit so the flip hotkey can re-render it without
    /// deleting committed text. The cost is that it belongs to a caret position we stop
    /// controlling the moment the user reaches for the mouse: apps move the caret on a click
    /// and only then send `reset()`, so committing there put the word wherever the user had
    /// just clicked («вставляется слово, которое печатал последним»). Committing it on its own
    /// after a short idle keeps the word where it was typed, and leaves nothing for a later
    /// click to displace.
    fn hold(&mut self, se: &SignalEmitter<'_>, mut held: Held) {
        self.held_gen = self.held_gen.wrapping_add(1);
        held.generation = self.held_gen;
        *lock(&self.held) = Some(held);
        self.arm_hold_timer(se);
    }

    /// Restart the idle countdown for the word already held — the user just did something to
    /// it (flipped its layout, cycled its case, typed another separator after it). Bumping the
    /// generation also retires the timer armed by the previous interaction, so the countdown
    /// really restarts instead of two timers racing.
    fn touch_hold(&mut self, se: &SignalEmitter<'_>) {
        self.held_gen = self.held_gen.wrapping_add(1);
        let generation = self.held_gen;
        {
            let mut guard = lock(&self.held);
            let Some(h) = guard.as_mut() else { return };
            h.generation = generation;
        }
        self.arm_hold_timer(se);
    }

    /// Commit the held word by itself once the user has been idle for `hold_commit`, provided
    /// it is still the same word this timer was armed for.
    fn arm_hold_timer(&self, se: &SignalEmitter<'_>) {
        let Some(delay) = self.settings().opts.hold_commit() else {
            return;
        };
        let slot = Arc::clone(&self.held);
        let generation = self.held_gen;
        let se = se.to_owned();
        let id = self.id;
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let Some(h) = take_if_current(&slot, generation) else { return };
            // The first one is INFO so «слово всё ещё уезжает по клику» can be answered from
            // the log without a debug build; the rest are DEBUG (one per word is too noisy).
            static ANNOUNCED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !ANNOUNCED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::info!(
                    "[puntu-engine {id}] idle commit active: held word committed after {:?} \
                     ({:?})",
                    delay,
                    h.shown
                );
            } else {
                debug!("[puntu-engine {id}] idle commit of held {:?}", h.shown);
            }
            // Preedit off first, then commit — same ordering as `flush_held`. Both are bounded
            // so a wedged DBus path parks one task briefly instead of one per typed word,
            // forever.
            let _ = tokio::time::timeout(
                EMIT_TIMEOUT,
                <PuntuEngine as IBusEngineBackend>::update_preedit_text(
                    &se,
                    String::new(),
                    0,
                    false,
                    librush::ibus::IBusPreeditFocusMode::Commit,
                ),
            )
            .await;
            match tokio::time::timeout(
                EMIT_TIMEOUT,
                <PuntuEngine as IBusEngineBackend>::commit_text(&se, h.shown),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => tracing::warn!("[puntu-engine {id}] idle commit failed: {e}"),
                Err(_) => tracing::warn!("[puntu-engine {id}] idle commit timed out"),
            }
        });
    }

    /// Await one DBus emit under [`EMIT_TIMEOUT`] and record whether it worked. Returns
    /// `false` on error **or** timeout — callers must treat both the same way, because from
    /// the user's seat "the text never appeared" and "the text appeared four seconds later"
    /// are the same bug.
    ///
    /// Every emit made while answering a key event goes through here. That is what makes
    /// `ProcessKeyEvent` bounded: the client is blocked on our reply until we return, so an
    /// emit that hangs is a keyboard that hangs.
    async fn guard_emit(
        &mut self,
        what: &str,
        fut: impl std::future::Future<Output = zbus::Result<()>>,
    ) -> bool {
        let ok = match tokio::time::timeout(EMIT_TIMEOUT, fut).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                tracing::warn!("[puntu-engine {}] {what} failed: {e}", self.id);
                false
            }
            Err(_) => {
                tracing::warn!(
                    "[puntu-engine {}] {what} timed out after {EMIT_TIMEOUT:?} — \
                     giving the key back to the app",
                    self.id
                );
                false
            }
        };
        self.note_emit(ok);
        ok
    }

    /// Track consecutive emit failures and latch [`Self::degraded`] once there are too many.
    /// A wedged DBus path must cost the user corrections, never their typing.
    fn note_emit(&mut self, ok: bool) {
        if ok {
            self.emit_failures = 0;
            return;
        }
        self.emit_failures += 1;
        if self.emit_failures >= MAX_EMIT_FAILURES && !self.degraded {
            self.degraded = true;
            // error-level: this is the state behind «puntu перестал печатать, помог только
            // перезапуск», and it must be findable in the journal without a debug build.
            tracing::error!(
                "[puntu-engine {}] {} DBus emits failed in a row (client={:?}) — going \
                 transparent until the next focus change; typing is unaffected, corrections \
                 are off",
                self.id,
                self.emit_failures,
                self.client,
            );
            notify(
                "Puntu отключился в этом поле: IBus не принимает текст.\n\
                 Ввод работает как обычно. Диагностика: puntu-ibus doctor",
            );
        }
    }

    /// The preedit update didn't reach the client, so `text` is currently visible **nowhere**.
    /// Commit it as ordinary text and start the word over.
    ///
    /// This is the whole point of checking the emit result: the engine claims a letter key
    /// (`Ok(true)`) on the promise that the preedit will show it. When that promise breaks,
    /// silently keeping the key is how typing "stops working" with no error anywhere — the
    /// user sees an empty screen and reaches for a restart. Better to lose the correction for
    /// this word and keep the letters.
    ///
    /// Returns `Ok(true)`: the key is accounted for, either committed here or (if the commit
    /// failed too) unrecoverable, and forwarding it on top would duplicate it.
    async fn bail_out(&mut self, se: &SignalEmitter<'_>, text: String) -> fdo::Result<bool> {
        tracing::warn!(
            "[puntu-engine {}] preedit did not reach the client — committing {text:?} as \
             plain text",
            self.id
        );
        self.clear_preedit(se).await;
        self.commit_str(se, text).await;
        self.buffer.invalidate();
        self.take_held();
        Ok(true)
    }

    /// Log who the engine is talking to and what it decided about them — **on the first key
    /// pressed** in a context, not when the facts arrive.
    ///
    /// Logging on arrival looked obvious and was wrong. IBus reports the name (`FocusInId`)
    /// and the capabilities (`SetCapabilities`) as separate calls in either order, and GNOME
    /// Shell sends capabilities twice (`0x09`, then `0x29` once surrounding text is on), so
    /// every focus change wrote three or four lines of half-built state — plus a set for the
    /// `fake` context nobody ever types into. Waiting for a keystroke means the facts have
    /// settled, and only fields the user actually uses say anything at all.
    ///
    /// This is *the* line to grep for: what an app calls itself, whether it can render a
    /// preedit, and whether Puntu is staying out of it.
    fn log_client_state(&mut self) {
        let now = (self.client.clone(), self.caps);
        if self.logged.as_ref() == Some(&now) {
            return;
        }
        self.logged = Some(now);
        tracing::info!(
            "[puntu-engine {}] client={:?} caps={} → {}",
            self.id,
            self.client,
            self.caps.map(|c| format!("0x{c:02x}")).unwrap_or_else(|| "?".into()),
            match self.passthrough_reason() {
                Some(r) => format!("transparent ({r:?})"),
                None => "active".to_string(),
            }
        );
    }

    /// Un-latch [`Self::degraded`] on a lifecycle event. Whatever wedged the DBus path
    /// belonged to the context we just left, so the next one deserves a fresh try — a fuse
    /// that only ever blows would turn one bad moment into "Puntu is dead until I restart it".
    fn recover(&mut self) {
        if self.degraded {
            tracing::info!("[puntu-engine {}] re-arming after a degraded context", self.id);
        }
        self.degraded = false;
        self.emit_failures = 0;
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
        let _ = tokio::time::timeout(
            EMIT_TIMEOUT,
            Self::update_auxiliary_text(se, text.to_string(), true),
        )
        .await;
        hint_shown.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Hide the auxiliary hint if one is showing.
    async fn hide_hint(&mut self, se: &SignalEmitter<'_>) {
        if self.hint_shown.swap(false, std::sync::atomic::Ordering::Relaxed) {
            // Runs inside the key event, so it goes through the guard like every other emit.
            self.guard_emit("hide_hint", Self::update_auxiliary_text(se, String::new(), false))
                .await;
        }
    }

    /// Commit `text`. Returns whether it actually reached the client — a lost commit is lost
    /// user text, so callers that still hold the text must know.
    async fn commit_str(&mut self, se: &SignalEmitter<'_>, text: String) -> bool {
        self.guard_emit("commit_text", Self::commit_text(se, text)).await
    }

    /// Handle a recognised modifier tap. Matches the tap kind against the configured
    /// `mode_toggle` and `convert_last` bindings, not hard-coded gestures — that way users
    /// can swap them (e.g. `mode_toggle = "Ctrl+Shift"` and `convert_last = "Ctrl"`) or
    /// disable one entirely with `"none"`.
    async fn handle_tap(&mut self, combo: ModCombo, se: &SignalEmitter<'_>) {
        let hotkeys = self.settings().hotkeys;
        if hotkeys.mode_toggle_tap == Some(combo) {
            debug!("[puntu-engine {}] {combo:?} tap → mode toggle", self.id);
            self.toggle_mode(se).await;
        } else if hotkeys.convert_last_tap == Some(combo) {
            // info-level: the tap not showing up in the logs at all means IBus never
            // delivered the modifier release events (known on some setups — use the
            // regular `convert_selection_key` hotkey there instead).
            tracing::info!("[puntu-engine {}] {combo:?} tap → convert selection", self.id);
            self.handle_convert_last(se);
        } else {
            // Recognised tap but no binding matches — silently ignore.
            debug!("[puntu-engine {}] {combo:?} tap → no binding", self.id);
        }
    }

    /// Toggle Correcting ↔ DirectRussian — from the mode-toggle tap or the mode-toggle key.
    async fn toggle_mode(&mut self, se: &SignalEmitter<'_>) {
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
                    "[puntu-engine {}] mode toggle: flushing in-progress {:?}",
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
        debug!("[puntu-engine {}] mode = {:?}", self.id, self.mode);
        self.show_hint(se, hint).await;
    }

    /// `Ctrl+` `` ` `` — flip the **held** word between its two layout readings. This is a pure
    /// preedit re-render: instant and reliable in every app, because it never deletes committed
    /// text (forwarded Backspaces / DeleteSurroundingText proved unreliable across GTK/Qt/
    /// Chromium/Gecko). If nothing is held (no word typed since the last commit), it's a no-op.
    ///
    /// Flipping an **auto-converted** word back is the user rejecting the correction, so the
    /// typed form is added to the learned list (once) and won't be auto-converted again.
    async fn handle_undo(&mut self, se: &SignalEmitter<'_>) {
        // Scoped so the (non-Send) lock guard is gone before the first `.await` below.
        let Some((shown, learn, manual)) = ({
            let mut guard = lock(&self.held);
            guard.as_mut().map(|h| {
                std::mem::swap(&mut h.shown, &mut h.other);
                let learn = if h.source == HeldSource::AutoConverted && !h.learned {
                    h.learned = true;
                    Some(h.typed.clone())
                } else {
                    None
                };
                // A forward flip (detector left the word as typed, the user converted it by
                // hand) is a manual conversion — feed the repeat counter, once per held word.
                // A Replacement is excluded: undoing a rule the user wrote themselves is not
                // evidence they want that word remembered.
                let manual = if h.source == HeldSource::Typed
                    && !h.counted
                    && starts_with_word(&h.shown, &h.converted)
                {
                    h.counted = true;
                    Some((h.typed.clone(), h.converted.clone()))
                } else {
                    None
                };
                (h.shown.clone(), learn, manual)
            })
        }) else {
            debug!("[puntu-engine {}] flip: nothing held", self.id);
            return;
        };
        // The user is working on this word — restart its idle countdown.
        self.touch_hold(se);
        debug!("[puntu-engine {}] flip: held → {:?}", self.id, shown);
        self.update_preedit(se, &shown).await;
        if let Some((typed, converted)) = manual {
            note_manual_conversion(
                &self.convert_counts,
                self.settings().opts.suggest_after,
                &self.detector(),
                &self.dict,
                &self.hint_shown,
                se,
                self.id,
                &typed,
                &converted,
            );
        }
        if let Some(typed) = learn {
            // Correcting mode only auto-converts EN-rendered words, so the rejected form is EN.
            let mut dict = self.dict.lock().await;
            match dict.add(&typed, Lang::En, ListKind::Learned) {
                Ok(()) => {
                    tracing::info!(
                        "[puntu-engine {}] learned {typed:?} (undone auto-conversion)",
                        self.id
                    );
                    // The silent version of this is how the user ended up with words on the
                    // never-correct list without knowing (the «eds» case) — say it out loud.
                    notify(&format!(
                        "Больше не исправляю «{typed}» (вы откатили автозамену).\n\
                         Вернуть: puntu dict rm {typed} или окно «puntu dict ui»"
                    ));
                }
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
        let detector = self.detector();
        let dict = Arc::clone(&self.dict);
        let hint_shown = Arc::clone(&self.hint_shown);
        let counts = Arc::clone(&self.convert_counts);
        let last_converted = Arc::clone(&self.last_converted);
        let suggest_after = self.settings().opts.suggest_after;
        // The IM-native selection, straight from the client — consumed one-shot: if the
        // client never re-reports after our replacement, a second tap must NOT reuse the
        // old bounds (that re-inserted the previous word).
        let surround_sel = surrounding_selection(&self.surrounding.take());
        let se = se.to_owned();
        tokio::spawn(async move {
            let from_client = surround_sel.is_some();
            let selection = if let Some(sel) = surround_sel {
                tracing::info!(
                    "[puntu-engine {id}] convert-selection: from surrounding text {sel:?}"
                );
                Some(sel)
            } else {
                match tokio::task::spawn_blocking(move || read_primary_selection(id)).await {
                    Ok(sel) => sel,
                    Err(e) => {
                        tracing::warn!("[puntu-engine {id}] convert-selection task failed: {e}");
                        None
                    }
                }
            };
            let Some(selection) = selection else {
                // The silent no-op here is what read as "Ctrl+Shift не работает" — say why.
                Self::show_hint_shared(&se, &hint_shown, "Puntu: нет выделения").await;
                return;
            };
            // Stale-PRIMARY guard: PRIMARY keeps the old text after a replacement, so a tap
            // with nothing newly selected would re-insert the previous word at the caret.
            // The client-reported selection skips it — it is fresh by construction (one-shot
            // and voided by any key press or reset).
            if !from_client
                && is_stale_selection(&lock(&last_converted).clone(), &selection)
            {
                tracing::info!(
                    "[puntu-engine {id}] convert-selection: PRIMARY still holds the previous \
                     pair ({selection:?}) — skipping"
                );
                Self::show_hint_shared(
                    &se,
                    &hint_shown,
                    "Puntu: нет нового выделения — выделите текст заново",
                )
                .await;
                return;
            }
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
            // Command-shaped selections are refused — force-flipping a command line
            // (`code --ozone-platform=wayland …` still in PRIMARY while pasting into a
            // terminal with Ctrl+Shift+V, where the app swallows the V) appended garbage.
            let converted = match converted {
                Some(c) => c,
                None => match force_flip_fallback(&selection) {
                    Some(f) => f,
                    None => {
                        tracing::info!(
                            "[puntu-engine {id}] convert-selection: nothing to fix and \
                             selection is command-shaped — skipping {selection:?}"
                        );
                        Self::show_hint_shared(
                            &se,
                            &hint_shown,
                            "Puntu: выделение похоже на команду — не переведено",
                        )
                        .await;
                        return;
                    }
                },
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
            *lock(&last_converted) =
                Some((selection.clone(), converted.clone(), std::time::Instant::now()));
            forward_backspace(&se).await;
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            // A selection conversion is a manual conversion — feed the repeat counter
            // (single clean words only; `learnable` inside filters the rest).
            note_manual_conversion(
                &counts,
                suggest_after,
                &detector,
                &dict,
                &hint_shown,
                &se,
                id,
                selection.trim(),
                converted.trim(),
            );
            if let Err(e) = Self::commit_text(&se, converted).await {
                tracing::warn!("[puntu-engine {id}] commit_text failed: {e}");
            }
        });
    }

    /// `Ctrl+Alt+U` — cycle the case of the **held** word (`слово` → `Слово` → `СЛОВО`).
    /// A pure preedit re-render, exactly like the layout flip: instant, no deletions of
    /// committed text. No-op when nothing is held.
    async fn handle_case_cycle(&mut self, se: &SignalEmitter<'_>) {
        let Some(shown) = ({
            let mut guard = lock(&self.held);
            guard.as_mut().map(|h| {
                h.shown = cycle_case(&h.shown);
                // Carry the new case over to the flip target too, or flipping the layout
                // afterwards silently threw the case away (`Слово ` → `ckjdj `).
                h.other = match_case(&h.other, &h.shown);
                h.shown.clone()
            })
        }) else {
            debug!("[puntu-engine {}] case cycle: nothing held", self.id);
            return;
        };
        self.touch_hold(se);
        debug!("[puntu-engine {}] case cycle -> {:?}", self.id, shown);
        self.update_preedit(se, &shown).await;
    }

    /// `Ctrl+Alt+D` — remember a word in the dictionary: the mouse selection when there is
    /// one, else the held (last) word in its currently shown form. Detached task — reading
    /// PRIMARY inline would deadlock the key event (same as convert-selection).
    fn handle_remember(&mut self, se: &SignalEmitter<'_>) {
        let id = self.id;
        // Fallback when nothing is selected: whichever form of the held word is on screen.
        // A replacement is skipped — what's on screen there is the user's own snippet, and
        // offering to "remember" it as a dictionary word makes no sense.
        let fallback = lock(&self.held).as_ref().filter(|h| h.source != HeldSource::Replacement).map(
            |h| {
                if starts_with_word(&h.shown, &h.converted) {
                    h.converted.clone()
                } else {
                    h.typed.clone()
                }
            },
        );
        let detector = self.detector();
        let dict = Arc::clone(&self.dict);
        let hint_shown = Arc::clone(&self.hint_shown);
        let se = se.to_owned();
        tokio::spawn(async move {
            let selection = tokio::task::spawn_blocking(move || read_primary_selection(id))
                .await
                .ok()
                .flatten();
            let Some(candidate) = selection.or(fallback) else {
                Self::show_hint_shared(
                    &se,
                    &hint_shown,
                    "Puntu: нечего запоминать — выделите слово",
                )
                .await;
                return;
            };
            let Some((word, lang)) = learnable(&candidate) else {
                Self::show_hint_shared(
                    &se,
                    &hint_shown,
                    &format!("Puntu: «{}» не похоже на слово — не запомнил", candidate.trim()),
                )
                .await;
                return;
            };
            if detector.is_known_word(&word, lang)
                || dict.lock().await.is_recognized(&word, lang)
            {
                Self::show_hint_shared(
                    &se,
                    &hint_shown,
                    &format!("Puntu: «{word}» уже в словаре"),
                )
                .await;
                return;
            }
            let wrong = crate::detect::translit::convert(&word, lang, lang.other());
            if learn_recognized(&dict, &word, lang, id).await {
                notify(&format!("Запомнил «{word}» ({wrong} → {word})"));
                Self::show_hint_shared(
                    &se,
                    &hint_shown,
                    &format!("Puntu: запомнил «{word}» ({wrong} → {word})"),
                )
                .await;
            }
        });
    }

    /// Commit the held word for real (it's now final) and clear the preedit. No-op if nothing
    /// is held. Called when the next word starts, on a hard boundary (Enter/Tab), a chord, or a
    /// focus change — so the pending word is never lost.
    async fn flush_held(&mut self, se: &SignalEmitter<'_>) {
        // `take_held` first, as its own statement: the lock guard must be dropped before the
        // awaits below (and a `if let` scrutinee would keep it alive for the whole block).
        let held = self.take_held();
        if let Some(h) = held {
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

    /// Show `text` as the preedit (cursor at end; hidden when empty). Returns whether the
    /// update reached the client: the preedit is the ONLY place a half-typed word exists, so
    /// a caller that swallowed the key on the strength of this must undo that decision when
    /// it returns `false`.
    async fn update_preedit(&mut self, se: &SignalEmitter<'_>, text: &str) -> bool {
        let n = text.chars().count() as u32;
        self.guard_emit(
            "update_preedit_text",
            Self::update_preedit_text(
                se,
                text.to_string(),
                n,
                !text.is_empty(),
                librush::ibus::IBusPreeditFocusMode::Commit,
            ),
        )
        .await
    }

    /// Hide the preedit.
    async fn clear_preedit(&mut self, se: &SignalEmitter<'_>) -> bool {
        self.guard_emit(
            "clear_preedit",
            Self::update_preedit_text(
                se,
                String::new(),
                0,
                false,
                librush::ibus::IBusPreeditFocusMode::Commit,
            ),
        )
        .await
    }

    /// Pick the `(shown, other, source)` renderings for a finished word per the current mode.
    /// `shown` is the default the engine holds; `other` is what `Ctrl+` `` ` `` flips to;
    /// `source` says how `shown` was arrived at (see [`HeldSource`]).
    ///
    /// Order matters. The user's own replacement table wins over everything — it is explicit
    /// configuration, not a guess. Failing that, Correcting runs the detector (trusted context,
    /// user dictionaries, command guard, trigram scoring — see [`Detector::decide`]), and
    /// DirectRussian defaults to the Russian rendering unless the Latin reading is a real
    /// word/abbreviation and the Russian one isn't.
    async fn decide_renderings(&self, word: &CompletedWord) -> (String, String, HeldSource) {
        let detector = self.detector();
        let settings = self.settings();
        // Anything that forbids rewriting the user's text forbids it for replacements too, so
        // this guard comes first and covers both. In a terminal an auto-rewrite of what turns
        // out to be a command/flag is never acceptable — «в терминале только вручную».
        let rewrites_allowed = settings.opts.autocorrect && !self.in_terminal();
        if rewrites_allowed {
            let expanded = {
                let dict = self.dict.lock().await;
                expand_replacement(&dict, word, self.mode)
            };
            if let Some((value, typed)) = expanded {
                debug!("[puntu-engine {}] replacement {typed:?} → {value:?}", self.id);
                // `fix_case` deliberately skipped: the value is the user's text, not a word
                // we watched them type, so there is no accidental-caps signature to fix.
                return (value, typed, HeldSource::Replacement);
            }
        }
        let (mut shown, other, source) = match self.mode {
            EngineMode::Correcting => {
                if !rewrites_allowed {
                    // dry_run or a terminal: hold the word exactly as typed; conversion only
                    // on the manual flip.
                    return (word.cur.clone(), word.alt.clone(), HeldSource::Typed);
                }
                let dict = self.dict.lock().await;
                match detector.decide(word, &dict) {
                    Decision::Convert { .. } => {
                        debug!(
                            "[puntu-engine {}] auto-convert {:?} → {:?}",
                            self.id, word.cur, word.alt
                        );
                        (word.alt.clone(), word.cur.clone(), HeldSource::AutoConverted)
                    }
                    Decision::Leave => {
                        (word.cur.clone(), word.alt.clone(), HeldSource::Typed)
                    }
                }
            }
            EngineMode::DirectRussian => {
                // Consult the USER dictionaries too, not only the built-in ones: a word
                // taught via Ctrl+Alt+D / the app («devops») must keep its Latin reading in
                // RU-direct mode right away — the built-in models load once at startup, so
                // without this the teaching visibly "did nothing" until an engine restart.
                let dict = self.dict.lock().await;
                let cur_is_real_en = detector.is_known_word(&word.cur, self.lang)
                    || dict.is_recognized(&word.cur, self.lang);
                let alt_is_real_ru = detector.is_known_word(&word.alt, self.lang.other())
                    || dict.is_recognized(&word.alt, self.lang.other());
                if cur_is_real_en && !alt_is_real_ru {
                    (word.cur.clone(), word.alt.clone(), HeldSource::Typed)
                } else {
                    (word.alt.clone(), word.cur.clone(), HeldSource::Typed)
                }
            }
        };
        // Accidental-caps signatures — on the FINAL rendering, after the layout decision
        // (gHBDTN with CapsLock becomes пРИВЕТ first, Привет second). `other` (the flip
        // target) stays untouched, so the flip still restores exactly what was typed.
        if settings.opts.fix_case {
            let dict = self.dict.lock().await;
            let known = |w: &str| {
                let lang = if w.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)) {
                    Lang::Ru
                } else {
                    Lang::En
                };
                detector.is_known_word(w, lang) || dict.is_recognized(w, lang)
            };
            if let Some(fixed) = fix_case_word(&shown, known) {
                debug!("[puntu-engine {}] case fix {:?} -> {:?}", self.id, shown, fixed);
                shown = fixed;
            }
        }
        (shown, other, source)
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
        // Now that a key has actually arrived, the client's name and capabilities have
        // settled — say who this is and what we decided. No-op unless something changed.
        self.log_client_state();
        // Password / PIN fields and the tray pause («выключить временно») make the engine
        // fully transparent: nothing below runs, every keystroke goes straight to the app.
        //
        // Both can switch on while a word is still held in preedit — the pause marker is
        // flipped by a watcher thread, the purpose by the client. The preedit is the only
        // place that word exists, so it has to become real text at that moment; otherwise it
        // stayed pending, the keys typed afterwards reached the app first, and the word
        // finally landed AFTER them on the next reset/focus change.
        let transparent =
            self.is_passthrough() || self.paused.load(std::sync::atomic::Ordering::Relaxed);
        if transparent && !self.was_transparent {
            tracing::info!(
                "[puntu-engine {}] going transparent ({}) — flushing pending text",
                self.id,
                match self.passthrough_reason() {
                    Some(r) => format!("{r:?}"),
                    None => "paused".to_string(),
                }
            );
            self.flush_all(&se).await;
        }
        self.was_transparent = transparent;
        if transparent {
            return Ok(false);
        }
        // One read of the live settings per key event — everything below uses this snapshot,
        // so a config reload landing mid-event can't change the rules halfway through.
        // `HotkeyBindings` is `Copy`, so this leaves no `Arc` alive across the awaits.
        let hotkeys = self.settings().hotkeys;
        // The tap threshold is the one setting the detector caches, so it needs pushing in.
        self.tap.set_max_hold(hotkeys.tap_max_hold_ms);
        // Undo hotkey (default `Ctrl+grave`, configurable via `ibus_hotkeys.undo_key`).
        // Matches on press with exact modifier state.
        //
        // Only claimed while a word is actually held: with nothing to flip this is a no-op,
        // and swallowing the key anyway stole the app's own shortcut — the default
        // `Ctrl+` `` ` `` is "toggle terminal" in VS Code, so it simply stopped working
        // everywhere the engine was active. Falling through hands the key to the app.
        if let Some(undo_hk) = hotkeys.undo {
            if undo_hk.matches(keyval, &state) && !released && self.is_holding() {
                debug!("[puntu-engine {}] undo hotkey matched", self.id);
                // The non-modifier press spoils any armed tap chain. Without this, the
                // Ctrl release *after* `Ctrl+grave` would fire the Ctrl tap and flip the
                // engine mode as a side effect of undoing.
                self.tap.cancel();
                self.handle_undo(&se).await;
                return Ok(true);
            }
        }
        // Mode-toggle key (default "none"): a GNOME-Tweaks-style layout-switch key
        // (`Pause`, `CapsLock`, …) as an alternative to the modifier tap.
        if let Some(mt_hk) = hotkeys.mode_toggle_key {
            if mt_hk.matches(keyval, &state) && !released {
                debug!("[puntu-engine {}] mode-toggle key matched", self.id);
                self.tap.cancel();
                self.toggle_mode(&se).await;
                return Ok(true);
            }
        }
        // Convert-selection hotkey (default `Ctrl+Alt+s`). Same selection-conversion
        // semantics as the Ctrl+Shift tap, but as a regular keypress — can't be confused
        // with a chord by accident (the chord-vs-tap ambiguity is what made the tap version
        // unreliable on some setups).
        if let Some(sel_hk) = hotkeys.convert_selection {
            if sel_hk.matches(keyval, &state) && !released {
                debug!("[puntu-engine {}] convert-selection hotkey matched", self.id);
                self.tap.cancel(); // same reason as the undo hotkey above
                self.handle_convert_last(&se);
                return Ok(true);
            }
        }
        // Case-cycle hotkey (default `Ctrl+Alt+u`): слово → Слово → СЛОВО on the held word —
        // the case counterpart of the layout flip. Claimed only while a word is held, for the
        // same reason as the flip hotkey above.
        if let Some(case_hk) = hotkeys.case {
            if case_hk.matches(keyval, &state) && !released && self.is_holding() {
                debug!("[puntu-engine {}] case-cycle hotkey matched", self.id);
                self.tap.cancel(); // same reason as the undo hotkey above
                self.handle_case_cycle(&se).await;
                return Ok(true);
            }
        }
        // Remember-word hotkey (default `Ctrl+Alt+d`): add the selected (or held) word to
        // the dictionary so its wrong-layout form converts from now on.
        if let Some(rem_hk) = hotkeys.remember {
            if rem_hk.matches(keyval, &state) && !released {
                debug!("[puntu-engine {}] remember hotkey matched", self.id);
                self.tap.cancel(); // same reason as the undo hotkey above
                self.handle_remember(&se);
                return Ok(true);
            }
        }
        // Track modifier-tap chains. Both Ctrl and Shift are tracked; a release that
        // empties the chain may fire `Ctrl` (toggle mode) or `CtrlShift` (convert last).
        match keyval {
            Keysym::Control_L | Keysym::Control_R => {
                if released {
                    if let Some(combo) = self.tap.release(Mod::Ctrl) {
                        self.handle_tap(combo, &se).await;
                    }
                } else {
                    self.tap.press(Mod::Ctrl, state.control());
                }
                return Ok(false);
            }
            Keysym::Shift_L | Keysym::Shift_R => {
                if released {
                    if let Some(combo) = self.tap.release(Mod::Shift) {
                        self.handle_tap(combo, &se).await;
                    }
                } else {
                    self.tap.press(Mod::Shift, state.shift());
                }
                return Ok(false);
            }
            Keysym::Alt_L | Keysym::Alt_R => {
                if released {
                    if let Some(combo) = self.tap.release(Mod::Alt) {
                        self.handle_tap(combo, &se).await;
                    }
                } else {
                    self.tap.press(Mod::Alt, state.mod1());
                }
                return Ok(false);
            }
            Keysym::Super_L | Keysym::Super_R => {
                if released {
                    if let Some(combo) = self.tap.release(Mod::Super) {
                        self.handle_tap(combo, &se).await;
                    }
                } else {
                    self.tap.press(Mod::Super, state.mod4());
                }
                return Ok(false);
            }
            _ => {}
        }
        if keyval != Keysym::Caps_Lock && !released {
            // Any non-modifier press while a tap was armed turns it into a chord — cancel.
            self.tap.cancel();
            // …and it is about to change the text, so the client-reported surrounding
            // text (with its selection bounds) is no longer true.
            self.surrounding = None;
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

        let kev = classify_keysym(keyval, self.lang);
        debug!(
            "[puntu-engine {}] keysym={:?} mode={:?} → {:?}",
            self.id, keyval, self.mode, kev
        );

        // The user is typing again, so any hint on screen has been read (or ignored). Hiding
        // it here rather than only on a letter means a hint left by a mode toggle or a failed
        // selection conversion doesn't sit near the caret until the next word — pressing
        // space, Enter or an arrow key clears it too. No-op (not even a DBus call) when no
        // hint is showing.
        self.hide_hint(&se).await;

        // Unified lazy-commit handling for both modes. A finished word is **held in preedit**
        // (not committed) until the next word starts, a hard boundary (Enter/Tab), a chord, or
        // a focus change — so `Ctrl+` `` ` `` can re-render it with no deletion of committed
        // text. `decide_renderings` picks the shown default and the flip target per mode.
        match kev {
            KeyEvent::Letter { .. } => {
                // Typing resumes: commit the held word (it's now final), then start the new one.
                self.flush_held(&se).await;
                self.buffer.push(kev);
                if let Some(snap) = self.buffer.snapshot(self.lang) {
                    let shown = match self.mode {
                        EngineMode::Correcting => snap.cur,
                        EngineMode::DirectRussian => snap.alt,
                    };
                    if !self.update_preedit(&se, &shown).await {
                        return self.bail_out(&se, shown).await;
                    }
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
                    if !self.update_preedit(&se, &shown).await {
                        return self.bail_out(&se, shown).await;
                    }
                    Ok(true)
                } else if self.is_holding() {
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
                //
                // The NUMPAD is exempt: it is layout-independent, so its symbols are the same
                // in every layout. Remapping them ran the numpad char through the main-row
                // table and printed the wrong key entirely — numpad `/` came out as `.` (the
                // main-row slash key is `.` in ЙЦУКЕН), numpad `.` as `ю`, numpad `,` as `б`.
                let sep = match self.mode {
                    EngineMode::DirectRussian if !is_numpad(keyval) => {
                        crate::detect::translit::convert_char(raw_sep, Lang::En, Lang::Ru)
                    }
                    _ => raw_sep,
                };
                if let Some(word) = self.buffer.finish(self.lang) {
                    let (shown_word, other_word, source) =
                        self.decide_renderings(&word).await;
                    // Any previously held word is now final.
                    self.flush_held(&se).await;
                    if hard {
                        // Enter/Tab: commit the word immediately, then forward the key so the
                        // app acts on it (sends the message / inserts a tab).
                        //
                        // Preedit off BEFORE the commit, for the same reason `flush_held`
                        // does it in that order: Chromium/Electron with text-input-v3 apply a
                        // trailing preedit-clear after the commit and clip the word that was
                        // just committed. Here that costs a whole submitted message.
                        self.clear_preedit(&se).await;
                        self.commit_str(&se, shown_word).await;
                        Ok(false)
                    } else {
                        // Space (soft): hold the word + separator in preedit, uncommitted, so
                        // the flip hotkey can still re-render it.
                        //
                        // The conversion target is simply the other-layout reading of what was
                        // typed. Deriving it by comparing `shown_word` with `word.cur` broke
                        // whenever `fix_case` rewrote `shown` without any layout change: the
                        // strings then differed, and `converted` ended up being the *typed*
                        // word — so the manual-conversion counter never fired for it and
                        // «запомнить слово» offered the wrong form.
                        let held = Held {
                            shown: format!("{shown_word}{sep}"),
                            other: format!("{other_word}{sep}"),
                            source,
                            typed: word.cur.clone(),
                            converted: word.alt.clone(),
                            learned: false,
                            counted: false,
                            generation: 0, // assigned by `hold`
                        };
                        if !self.update_preedit(&se, &held.shown).await {
                            return self.bail_out(&se, held.shown).await;
                        }
                        self.hold(&se, held);
                        Ok(true)
                    }
                } else if self.is_holding() {
                    if hard {
                        self.flush_held(&se).await;
                        Ok(false)
                    } else {
                        // Extra separator after a held word — append it to the held preedit.
                        let shown = {
                            let mut guard = lock(&self.held);
                            guard
                                .as_mut()
                                .map(|h| {
                                    h.shown.push(sep);
                                    h.other.push(sep);
                                    h.shown.clone()
                                })
                                .unwrap_or_default()
                        };
                        // Still typing into this hold — restart its idle countdown.
                        self.touch_hold(&se);
                        if !self.update_preedit(&se, &shown).await {
                            return self.bail_out(&se, shown).await;
                        }
                        Ok(true)
                    }
                } else if sep != raw_sep {
                    // RU-direct punctuation with nothing buffered: the app can't render the
                    // Russian char from the forwarded Latin keysym, so commit it ourselves.
                    // If that commit didn't land, forward the key instead — the app then
                    // inserts the Latin character, which is wrong but visible and fixable;
                    // swallowing it would drop the keystroke without a trace.
                    let committed = self.commit_str(&se, sep.to_string()).await;
                    Ok(committed)
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
        se: SignalEmitter<'_>,
        _server: &ObjectServer,
    ) -> fdo::Result<()> {
        debug!("[puntu-engine {}] enable", self.id);
        // Anything still pending belongs to the previous context and exists ONLY in preedit —
        // dropping it here (the old behaviour) silently ate the word and left its preedit on
        // screen. Turn it into real text first, exactly like `disable`/`focus_out`/`reset` do.
        self.flush_all(&se).await;
        self.tap.hard_reset();
        // A new context: forget the previous one's purpose. Clients that care (terminals,
        // password fields) set it again right after; clients that don't would otherwise
        // inherit the stale value — one terminal visit left the engine transparent
        // EVERYWHERE until the next explicit SetContentType.
        self.purpose = 0;
        self.recover();
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
        self.surrounding = None;
        self.tap.hard_reset();
        Ok(())
    }

    async fn focus_in(
        &mut self,
        se: SignalEmitter<'_>,
        _server: &ObjectServer,
    ) -> fdo::Result<()> {
        debug!("[puntu-engine {}] focus_in", self.id);
        // Same reset as `enable`: purpose describes the field being left otherwise.
        self.purpose = 0;
        self.recover();
        // Some clients only start reporting surrounding text after the engine asks for
        // it — the static ActiveSurroundingText property alone is not always honoured.
        let _ = se
            .emit("org.freedesktop.IBus.Engine", "RequireSurroundingText", &())
            .await;
        Ok(())
    }

    /// Same as [`Self::focus_in`], plus the client's name — which is the only way to tell a
    /// game apart from a text field. IBus sends this **instead of** `FocusIn` once the
    /// `FocusId` property reads true, so it must do everything `focus_in` does.
    async fn focus_in_id(
        &mut self,
        se: SignalEmitter<'_>,
        server: &ObjectServer,
        object_path: String,
        client: String,
    ) -> fdo::Result<()> {
        self.client = client;
        debug!(
            "[puntu-engine {}] focus_in client={:?} context={object_path}",
            self.id, self.client
        );
        self.focus_in(se, server).await
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
        // Drop the purpose with the context it belonged to. IBus reuses ONE engine object for
        // every input context, so a password/terminal purpose left behind by a field the user
        // has already left makes the engine transparent for whatever they type NEXT — the
        // engine looks dead everywhere until something happens to set the purpose again.
        // `focus_in` resets it too; doing it on the way out as well means transparency can
        // never outlive the field that asked for it.
        self.purpose = 0;
        // Capabilities and the client name describe that same field — a game's "no preedit"
        // must not follow the user into their editor, and vice versa.
        self.caps = None;
        self.client.clear();
        // Drop any half-tracked modifier tap: a Ctrl held across a focus change (Ctrl+click,
        // window switch) must not fire the mode toggle when it's finally released.
        self.surrounding = None;
        self.tap.hard_reset();
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
        self.surrounding = None;
        self.tap.hard_reset();
        Ok(())
    }

    fn set_surrounding_text(
        &mut self,
        text: String,
        cursor_pos: u32,
        anchor_pos: u32,
    ) -> fdo::Result<()> {
        // DEBUG, not INFO: clients re-report the surrounding text on **every** caret and
        // selection change, so dragging a selection across a paragraph emits a line per
        // character. At info level that buried everything else in the log — and the log is
        // where a user is told to look when something misbehaves.
        tracing::debug!(
            "[puntu-engine {}] surrounding: caret={cursor_pos} anchor={anchor_pos} len={}",
            self.id,
            text.chars().count()
        );
        self.surrounding = Some((text, cursor_pos, anchor_pos));
        Ok(())
    }

    fn set_capabilities(&mut self, caps: u32) -> fdo::Result<()> {
        // Recorded silently; `log_client_state` reports it on the next keystroke, once the
        // client has finished making up its mind (GNOME Shell sends this twice per focus).
        self.caps = Some(caps);
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
    detector: DetectorSlot,
    dict: Arc<AsyncMutex<UserDict>>,
    settings: SettingsSlot,
    paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Manual-conversion counter, shared by every engine this factory creates.
    convert_counts: ConvertCounts,
    last_converted: LastConverted,
    next_id: u64,
}

impl PuntuFactory {
    /// `dict` and `detector` are shared: the caller keeps clones for the hot-reload watcher, so
    /// `puntu dict add/learn/rm` edits — and rebuilt language models — reach every live engine
    /// without a restart.
    pub fn new(
        detector: DetectorSlot,
        dict: Arc<AsyncMutex<UserDict>>,
        settings: SettingsSlot,
        paused: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            detector,
            dict,
            settings,
            paused,
            convert_counts: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            last_converted: Arc::new(std::sync::Mutex::new(None)),
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
            Arc::clone(&self.settings),
            std::sync::Arc::clone(&self.paused),
            Arc::clone(&self.convert_counts),
            Arc::clone(&self.last_converted),
        ))
    }
}

/// Classify an IBus keysym the same way our evdev tokenizer does. Unlike the evdev
/// tokenizer this needs no modifier state: IBus hands us the keysym *after* xkb applied
/// Shift and CapsLock, so the keysym alone says which case was typed.
fn classify_keysym(keyval: Keysym, lang: Lang) -> KeyEvent {
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
                // `shift` comes from the KEYSYM, not from the physical Shift state: IBus
                // delivers the already-adjusted keysym, so CapsLock ON gives `G` with
                // `state.shift() == false`. The buffer stores only `(code, shift)` and
                // re-renders through `char_for`, so taking `mods.shift` here inverted the
                // case of everything typed with CapsLock on.
                KeyEvent::Letter { code, shift, cur: cur_char, alt }
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

/// Is `word` worth remembering in the dictionary, and in which language? Trims, lowercases,
/// and refuses anything that isn't a single clean-script word: whitespace inside, digits or
/// command punctuation (`--force`, `v0.1`), mixed Cyrillic/Latin, or a single letter.
fn learnable(word: &str) -> Option<(String, Lang)> {
    let w = word.trim().to_lowercase();
    if w.chars().count() < 2
        || w.chars().any(char::is_whitespace)
        || crate::detect::userdict::is_command_context(&w)
    {
        return None;
    }
    let lang = if w.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)) {
        Lang::Ru
    } else {
        Lang::En
    };
    let clean = w.chars().all(|c| match lang {
        Lang::Ru => ('\u{0400}'..='\u{04FF}').contains(&c),
        Lang::En => c.is_ascii_alphabetic(),
    });
    clean.then_some((w, lang))
}

/// Fire a GNOME desktop notification (best effort — a missing `notify-send` is ignored).
/// The aux-text hint near the caret is easy to miss or absent in some apps; a saved word
/// must be *visibly* confirmed, or the user can't tell learning worked at all.
fn notify(body: &str) {
    let _ = std::process::Command::new("notify-send")
        .args(["--app-name=Puntu", "--icon=input-keyboard-symbolic", "Puntu", body])
        .spawn();
}

/// Persist `word` as a recognized dictionary word (its wrong-layout form will convert).
/// Returns `false` when it was already there. The hot-reload watcher then propagates the
/// file change to every running engine.
async fn learn_recognized(
    dict: &AsyncMutex<UserDict>,
    word: &str,
    lang: Lang,
    id: u64,
) -> bool {
    let mut d = dict.lock().await;
    if d.is_recognized(word, lang) {
        return false;
    }
    match d.add(word, lang, ListKind::Recognized) {
        Ok(()) => {
            tracing::info!("[puntu-engine {id}] learned {word:?} as a recognized {lang} word");
            true
        }
        Err(e) => {
            tracing::warn!("[puntu-engine {id}] could not persist {word:?}: {e}");
            false
        }
    }
}

/// Words that never reach `suggest_after` stay in the counter forever, so the map only ever
/// grows over a session. It is a "did this happen a few times in a row" heuristic, not a
/// history: past this many distinct words, start over rather than grow without bound.
const MAX_TRACKED_CONVERSIONS: usize = 256;

/// Bump the manual-conversion counter for `word`. Returns `true` when the count reaches
/// `suggest_after` — the entry is then reset, so declining the offer doesn't re-ask on the
/// very next conversion.
fn bump_conversion_count(
    counts: &ConvertCounts,
    suggest_after: u32,
    word: &str,
    typed: &str,
) -> bool {
    let mut m = lock(counts);
    if m.len() >= MAX_TRACKED_CONVERSIONS && !m.contains_key(word) {
        // Dropping the tallies costs at most a delayed offer for words converted once or
        // twice long ago — which is exactly the set worth forgetting.
        m.clear();
    }
    let entry = m.entry(word.to_string()).or_insert((0, String::new()));
    entry.0 += 1;
    entry.1 = typed.trim().to_string();
    if entry.0 >= suggest_after {
        m.remove(word);
        true
    } else {
        false
    }
}

/// Count a manual conversion of `converted` (typed as `typed`) and, on reaching
/// `suggest_after`, spawn a zenity question offering to remember the word. Words already in
/// the dictionaries are not counted. No-op when `suggest_after` is 0.
fn note_manual_conversion(
    counts: &ConvertCounts,
    suggest_after: u32,
    detector: &Arc<Detector>,
    dict: &Arc<AsyncMutex<UserDict>>,
    hint_shown: &Arc<std::sync::atomic::AtomicBool>,
    se: &SignalEmitter<'_>,
    id: u64,
    typed: &str,
    converted: &str,
) {
    if suggest_after == 0 {
        return;
    }
    let Some((word, lang)) = learnable(converted) else {
        return;
    };
    if detector.is_known_word(&word, lang) {
        return; // built-in dictionaries already know it — nothing to learn
    }
    if !bump_conversion_count(counts, suggest_after, &word, typed) {
        return;
    }
    let dict = Arc::clone(dict);
    let hint_shown = Arc::clone(hint_shown);
    let se = se.to_owned();
    let typed = typed.trim().to_string();
    tokio::spawn(async move {
        if dict.lock().await.is_recognized(&word, lang) {
            return; // learned meanwhile (remember hotkey / CLI)
        }
        let text = format!("Запомнить слово «{word}»?\n({typed} → {word})");
        let yes = tokio::task::spawn_blocking(move || {
            std::process::Command::new("zenity")
                .args([
                    "--question",
                    "--title=Puntu",
                    &format!("--text={text}"),
                    "--ok-label=Запомнить",
                    "--cancel-label=Нет",
                ])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false);
        if yes && learn_recognized(&dict, &word, lang, id).await {
            notify(&format!("Запомнил «{word}» ({typed} → {word})"));
            PuntuEngine::show_hint_shared(&se, &hint_shown, &format!("Puntu: запомнил «{word}»"))
                .await;
        }
    });
}


/// Capitalize: the first alphabetic char uppercase, everything after it lowercase.
/// Non-alphabetic chars (a trailing separator in a held preedit) pass through.
fn capitalize(w: &str) -> String {
    let mut out = String::with_capacity(w.len());
    let mut first = true;
    for c in w.chars() {
        if c.is_alphabetic() && first {
            first = false;
            out.extend(c.to_uppercase());
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

/// The two accidental-caps signatures. Returns the corrected word, or `None` when the case
/// looks intentional:
///   * `пРИВЕТ` (first lower, ALL the rest upper) — CapsLock + Shift on the first letter;
///     impossible in real orthography, fixed without a dictionary;
///   * `ПРивет` (exactly two leading capitals, rest lower, ≥4 letters) — a late Shift
///     release; fixed only when the lowercase form is a dictionary word.
/// ALL-CAPS words (НАТО), mixed-case names (iPhone, КамАЗ) and anything with digits or
/// punctuation never match.
fn fix_case_word(word: &str, known: impl Fn(&str) -> bool) -> Option<String> {
    if word.is_empty() || word.chars().any(|c| !c.is_alphabetic()) {
        return None;
    }
    let chars: Vec<char> = word.chars().collect();
    if chars.len() >= 3
        && chars[0].is_lowercase()
        && chars[1..].iter().all(|c| c.is_uppercase())
    {
        return Some(capitalize(word));
    }
    if chars.len() >= 4
        && chars[0].is_uppercase()
        && chars[1].is_uppercase()
        && chars[2..].iter().all(|c| c.is_lowercase())
        && known(&word.to_lowercase())
    {
        return Some(capitalize(word));
    }
    None
}

/// Look the finished word up in the user's replacement table and, on a hit, return
/// `(value, what_was_typed)`.
///
/// **Both readings of the keys are tried**, which is the point: `ривет = привет` has to fire
/// whether the user typed Russian letters or hit the same keys with a US layout active
/// (`hbdtn`). Forgetting to switch layout is the situation Puntu exists for, and a replacement
/// table that only worked in one of them would be the one feature that didn't help there.
///
/// The reading matching the current mode is tried first, so if two keys collide across layouts
/// the outcome is predictable rather than dependent on hash order.
///
/// Case follows what was typed (`Ривет` → `Привет`), reusing [`match_case`] — but only for
/// single-word values. In `адр = ул. Пушкина, д. 1` the capitalisation is part of the text the
/// user wrote, and re-casing it would be vandalism.
fn expand_replacement(
    dict: &UserDict,
    word: &CompletedWord,
    mode: EngineMode,
) -> Option<(String, String)> {
    let (first, second) = match mode {
        EngineMode::Correcting => (&word.cur, &word.alt),
        EngineMode::DirectRussian => (&word.alt, &word.cur),
    };
    let (typed, value) = [first, second]
        .into_iter()
        .find_map(|reading| dict.replacement(reading).map(|v| (reading.clone(), v)))?;
    let value = if value.chars().any(char::is_whitespace) {
        value.to_string()
    } else {
        match_case(value, &typed)
    };
    Some((value, typed))
}

/// Re-case `word` the way `model` is cased: all-lower, Capitalized, or ALL-CAPS. Used to keep
/// the flip target in step with the case-cycle hotkey.
fn match_case(word: &str, model: &str) -> String {
    let upper: String = model.chars().flat_map(|c| c.to_uppercase()).collect();
    let lower: String = model.chars().flat_map(|c| c.to_lowercase()).collect();
    if model == upper && model != lower {
        word.chars().flat_map(|c| c.to_uppercase()).collect()
    } else if model == capitalize(model) && model != lower {
        capitalize(word)
    } else {
        word.chars().flat_map(|c| c.to_lowercase()).collect()
    }
}

/// Cycle the case: `слово` → `Слово` → `СЛОВО` → `слово`. State is detected on the string
/// as-is, so it works on a held preedit (word + trailing separator) too.
fn cycle_case(w: &str) -> String {
    let lower: String = w.chars().flat_map(|c| c.to_lowercase()).collect();
    let upper: String = w.chars().flat_map(|c| c.to_uppercase()).collect();
    let cap = capitalize(w);
    if w == lower && cap != lower {
        cap
    } else if w == cap && upper != cap {
        upper
    } else {
        lower
    }
}

/// The force-flip fallback: the deliberate "я выделил, переведи" case when the detector saw
/// nothing wrong. Selections arrive with edge whitespace (double-click grabs the trailing
/// space — `"работал "`), so the core is trimmed for the check and the edges are kept
/// verbatim in the result. Any command-shaped token (flag/path/version — `--force`, `v0.1`)
/// refuses the flip, so a stale PRIMARY with a command line can never be mangled wholesale
/// by an accidental Ctrl+Shift. Returns `None` when the flip is not allowed.
fn force_flip_fallback(selection: &str) -> Option<String> {
    let core = selection.trim();
    if core.is_empty()
        || core
            .split_whitespace()
            .any(crate::detect::userdict::is_command_context)
    {
        return None;
    }
    let lead = &selection[..selection.len() - selection.trim_start().len()];
    let trail = &selection[selection.trim_end().len()..];
    Some(format!("{lead}{}{trail}", force_translit(core)))
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
    let args = (keyval, keycode, state);
    let emit = se.emit("org.freedesktop.IBus.Engine", "ForwardKeyEvent", &args);
    match tokio::time::timeout(EMIT_TIMEOUT, emit).await {
        Ok(r) => r,
        // Bounded like every other emit: the selection-conversion task that calls this must
        // not sit forever holding a half-applied replacement.
        Err(_) => Err(zbus::Error::InputOutput(Arc::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "ForwardKeyEvent timed out",
        )))),
    }
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
        "capslock" | "caps_lock" | "caps" => return Some(Keysym::Caps_Lock),
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

/// Does the shown preedit start with `word`, ignoring case? Case-insensitive because the
/// accidental-caps fix and the case-cycle hotkey both rewrite what is shown (`привет ` →
/// `Привет `) while the word we compare against keeps the case it was rendered with.
fn starts_with_word(shown: &str, word: &str) -> bool {
    !word.is_empty() && shown.to_lowercase().starts_with(&word.to_lowercase())
}

/// Take the held word out of `slot`, but only while it is still the one `generation` was
/// armed for. This is what keeps a fired idle-commit timer from touching a word the engine
/// has meanwhile flushed, flipped or replaced — i.e. from committing the same text twice.
fn take_if_current(slot: &HeldSlot, generation: u64) -> Option<Held> {
    let mut guard = lock(slot);
    match guard.as_ref() {
        Some(h) if h.generation == generation => guard.take(),
        _ => None,
    }
}

/// Is this a numpad keysym? The numpad is **layout-independent** — `/`, `*`, `-`, `+`, `.`,
/// `,` and the digits are the same in US-QWERTY and ЙЦУКЕН alike — so its characters must
/// never go through the main-row transliteration table (see the RU-direct separator remap).
fn is_numpad(keyval: Keysym) -> bool {
    use xkeysym::Keysym as K;
    matches!(
        keyval,
        K::KP_0 | K::KP_1 | K::KP_2 | K::KP_3 | K::KP_4 | K::KP_5 | K::KP_6 | K::KP_7
            | K::KP_8 | K::KP_9 | K::KP_Add | K::KP_Subtract | K::KP_Multiply | K::KP_Divide
            | K::KP_Decimal | K::KP_Separator | K::KP_Equal | K::KP_Space | K::KP_Enter
    )
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

    fn ctrl() -> ModCombo {
        ModCombo { ctrl: true, ..Default::default() }
    }
    fn ctrl_shift() -> ModCombo {
        ModCombo { ctrl: true, shift: true, ..Default::default() }
    }

    #[test]
    fn ctrl_tap_fires_and_chord_cancels() {
        let mut tap = TapDetector::default();
        tap.press(Mod::Ctrl, false);
        assert_eq!(tap.release(Mod::Ctrl), Some(ctrl()));
        // A non-modifier press mid-chain (what process_key_event calls cancel() for)
        // must spoil the gesture.
        tap.press(Mod::Ctrl, false);
        tap.cancel();
        assert_eq!(tap.release(Mod::Ctrl), None);
    }

    #[test]
    fn ctrl_shift_tap_fires_regardless_of_release_order() {
        let mut tap = TapDetector::default();
        tap.press(Mod::Ctrl, false);
        tap.press(Mod::Shift, false);
        assert_eq!(tap.release(Mod::Ctrl), None); // shift still held
        assert_eq!(tap.release(Mod::Shift), Some(ctrl_shift()));

        tap.press(Mod::Ctrl, false);
        tap.press(Mod::Shift, false);
        assert_eq!(tap.release(Mod::Shift), None);
        assert_eq!(tap.release(Mod::Ctrl), Some(ctrl_shift()));
    }

    #[test]
    fn cancel_mid_hold_survives_until_all_released() {
        // Focus change while Ctrl is held (Ctrl+click): cancel() must keep the eventual
        // release from firing the mode toggle.
        let mut tap = TapDetector::default();
        tap.press(Mod::Ctrl, false);
        tap.cancel();
        assert_eq!(tap.release(Mod::Ctrl), None);
        // The next clean tap works again.
        tap.press(Mod::Ctrl, false);
        assert_eq!(tap.release(Mod::Ctrl), Some(ctrl()));
    }

    #[test]
    fn long_hold_does_not_fire_tap() {
        // A Ctrl (or Ctrl+Shift) held longer than `max_hold` is a shortcut the app may have
        // swallowed the letter of (Ctrl+Shift+V in a terminal) — it must NOT fire on release.
        let mut tap = TapDetector::new(500);
        tap.press(Mod::Ctrl, false);
        tap.started = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        assert_eq!(tap.release(Mod::Ctrl), None);
        // The next quick tap still works.
        tap.press(Mod::Ctrl, false);
        assert_eq!(tap.release(Mod::Ctrl), Some(ctrl()));

        // Ctrl+Shift is a deliberate two-modifier gesture — it fires even after a long
        // hold (the user paused to look at the selection before releasing).
        let mut tap = TapDetector::new(500);
        tap.press(Mod::Ctrl, false);
        tap.press(Mod::Shift, false);
        tap.started = Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
        assert_eq!(tap.release(Mod::Shift), None);
        assert_eq!(tap.release(Mod::Ctrl), Some(ctrl_shift()));
    }

    #[test]
    fn force_flip_fallback_is_gated() {
        // Deliberate case: selected words flip even when they read as valid.
        assert_eq!(force_flip_fallback("hello").as_deref(), Some("руддщ"));
        // Double-click selections carry the trailing space — trimmed for the check, kept in
        // the output (this exact case showed «выделение не похоже на слово» to the user).
        assert_eq!(force_flip_fallback("работал ").as_deref(), Some("hf,jnfk "));
        // Multi-word phrases of plain words are allowed.
        assert!(force_flip_fallback("два слова").is_some());
        // Command lines never force-flip — this is what appended `сщву --щящту…` after a
        // Ctrl+Shift+V paste in a terminal.
        assert_eq!(force_flip_fallback("code --ozone-platform=wayland"), None);
        assert_eq!(force_flip_fallback("--force"), None);
        assert_eq!(force_flip_fallback("v0.1"), None);
        assert_eq!(force_flip_fallback("   "), None);
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
                classify_keysym(k, Lang::En),
                KeyEvent::Separator,
                "{k:?} must classify as Separator"
            );
        }
        // NumLock-off numpad = navigation → must invalidate, same as the main-row keys.
        for k in [Keysym::KP_Home, Keysym::KP_Left, Keysym::KP_Page_Down, Keysym::KP_Delete] {
            assert_eq!(
                classify_keysym(k, Lang::En),
                KeyEvent::Invalidate,
                "{k:?} must classify as Invalidate"
            );
        }
    }

    #[test]
    fn lost_release_resyncs_on_next_press() {
        // A Ctrl release that happened while focus was elsewhere (Ctrl+click into another
        // window) never reaches the engine: the ref-count sticks at 1 and every later tap
        // is dead — `maybe_fire` waits forever for "all released". The state bits of the
        // NEXT press say Ctrl was NOT held, which must resync the count.
        let mut tap = TapDetector::default();
        tap.press(Mod::Ctrl, false); // press seen…
        // …release lost. Later the user taps Ctrl again:
        tap.press(Mod::Ctrl, false); // state: Ctrl was not held → resync
        assert_eq!(tap.release(Mod::Ctrl), Some(ctrl()));
        // A legitimately held second Ctrl (state bit true) keeps its count.
        tap.press(Mod::Ctrl, false);
        tap.press(Mod::Ctrl, true);
        assert_eq!(tap.release(Mod::Ctrl), None); // one Ctrl still down
        assert_eq!(tap.release(Mod::Ctrl), Some(ctrl()));
    }

    #[test]
    fn hard_reset_clears_stuck_counts() {
        let mut tap = TapDetector::default();
        tap.press(Mod::Ctrl, false);
        tap.hard_reset(); // focus change while held
        tap.press(Mod::Ctrl, false);
        assert_eq!(tap.release(Mod::Ctrl), Some(ctrl()));
    }

    #[test]
    fn alt_shift_tap_fires_like_the_system_layout_switch() {
        // The GNOME-Tweaks-style combos (Alt+Shift, Ctrl+Alt, …) are now valid gestures.
        let mut tap = TapDetector::default();
        tap.press(Mod::Alt, false);
        tap.press(Mod::Shift, false);
        assert_eq!(tap.release(Mod::Shift), None); // alt still held
        assert_eq!(
            tap.release(Mod::Alt),
            Some(ModCombo { alt: true, shift: true, ..Default::default() })
        );
        // Config parsing round-trips the same combo; bare Shift is refused.
        assert_eq!(
            parse_tap_combo("Alt+Shift"),
            Some(ModCombo { alt: true, shift: true, ..Default::default() })
        );
        assert_eq!(parse_tap_combo("Ctrl+Alt"), Some(ModCombo { ctrl: true, alt: true, ..Default::default() }));
        assert_eq!(parse_tap_combo("Shift"), None);
        assert_eq!(parse_tap_combo("none"), None);
        // The mode-toggle key parses CapsLock and Pause.
        assert_eq!(parse_hotkey("CapsLock").map(|h| h.keysym), Some(Keysym::Caps_Lock));
        assert_eq!(parse_hotkey("Pause").map(|h| h.keysym), Some(Keysym::Pause));
    }

    #[test]
    fn shown_word_is_matched_ignoring_case() {
        // `fix_case` and the case-cycle hotkey rewrite what is SHOWN without changing the
        // layout, so matching the shown preedit against the conversion target has to ignore
        // case — otherwise the manual-conversion counter never fires for a word whose case
        // was corrected, and «запомнить слово» offers the wrong form.
        assert!(starts_with_word("Привет ", "привет"));
        assert!(starts_with_word("СЛОВО ", "слово"));
        assert!(!starts_with_word("ghbdtn ", "привет"));
        assert!(!starts_with_word("привет ", ""));
    }

    #[test]
    fn idle_commit_only_fires_for_the_word_it_was_armed_for() {
        let held = |generation| Held {
            shown: "привет ".into(),
            other: "ghbdtn ".into(),
            typed: "ghbdtn".into(),
            converted: "привет".into(),
            source: HeldSource::AutoConverted,
            learned: false,
            counted: false,
            generation,
        };
        let slot: HeldSlot = Arc::new(std::sync::Mutex::new(Some(held(7))));

        // A timer armed for an older hold must not touch the word now in the slot — otherwise
        // a word the user is still typing on gets committed out from under them, or the same
        // text is committed twice.
        assert!(take_if_current(&slot, 6).is_none());
        assert!(slot.lock().unwrap().is_some(), "the current hold must survive a stale timer");

        // The timer that owns the hold takes it, exactly once.
        assert_eq!(take_if_current(&slot, 7).map(|h| h.shown).as_deref(), Some("привет "));
        assert!(take_if_current(&slot, 7).is_none(), "an empty slot has nothing to commit");
    }

    #[test]
    fn letter_case_comes_from_the_keysym_not_the_shift_state() {
        // IBus delivers the keysym AFTER xkb applied Shift and CapsLock. With CapsLock on
        // the user gets `G` while `state.shift()` is false; taking the physical Shift state
        // instead of the keysym inverted the case of everything typed with CapsLock on
        // (`ПРИВЕТ` committed as `привет`), because the buffer re-renders from `(code, shift)`.
        let upper = classify_keysym(Keysym::G, Lang::En);
        assert_eq!(
            upper,
            KeyEvent::Letter { code: 34, shift: true, cur: 'G', alt: 'П' },
            "an uppercase keysym must record shift=true regardless of the modifier state"
        );
        let lower = classify_keysym(Keysym::g, Lang::En);
        assert_eq!(lower, KeyEvent::Letter { code: 34, shift: false, cur: 'g', alt: 'п' });
        // And the buffer renders back exactly what was typed.
        let mut buf = WordBuffer::new();
        buf.push(upper);
        buf.push(lower);
        let w = buf.finish(Lang::En).unwrap();
        assert_eq!(w.cur, "Gg");
        assert_eq!(w.alt, "Пп");
    }

    #[test]
    fn surrounding_selection_extracts_the_span() {
        // Cursor/anchor in either order; char (not byte) offsets — Cyrillic-safe.
        let sur = Some(("привет мир".to_string(), 7, 10));
        assert_eq!(surrounding_selection(&sur).as_deref(), Some("мир"));
        let sur = Some(("привет мир".to_string(), 10, 7));
        assert_eq!(surrounding_selection(&sur).as_deref(), Some("мир"));
        // No selection when the bounds coincide, or nothing was reported.
        assert_eq!(surrounding_selection(&Some(("привет".to_string(), 3, 3))), None);
        assert_eq!(surrounding_selection(&None), None);
    }

    #[test]
    fn stale_primary_selection_is_refused() {
        let now = std::time::Instant::now();
        let last = Some(("drk".to_string(), "вкл".to_string(), now));
        // Both halves of the previous pair are residue, not a fresh selection.
        assert!(is_stale_selection(&last, "drk"));
        assert!(is_stale_selection(&last, "вкл"));
        assert!(is_stale_selection(&last, " drk ")); // trailing space from double-click
        // A genuinely new selection converts as usual.
        assert!(!is_stale_selection(&last, "ghbdtn"));
        assert!(!is_stale_selection(&None, "drk"));
        // The guard expires: re-selecting the same word later must convert again.
        let old = Some((
            "drk".to_string(),
            "вкл".to_string(),
            now - (STALE_PAIR_WINDOW + std::time::Duration::from_secs(1)),
        ));
        assert!(!is_stale_selection(&old, "drk"));
    }

    #[test]
    fn case_signatures_fix_and_intentional_case_survives() {
        let known = |w: &str| ["привет", "работа"].contains(&w);
        // Паттерн 1: CapsLock-инверсия — чинится без словаря.
        assert_eq!(fix_case_word("пРИВЕТ", known).as_deref(), Some("Привет"));
        assert_eq!(fix_case_word("hELLO", known).as_deref(), Some("Hello"));
        // Паттерн 2: поздний Shift — только словарные слова.
        assert_eq!(fix_case_word("ПРивет", known).as_deref(), Some("Привет"));
        assert_eq!(fix_case_word("РАбота", known).as_deref(), Some("Работа"));
        assert_eq!(fix_case_word("КАмаз", known), None); // не в словаре
        // Намеренный регистр не трогается.
        assert_eq!(fix_case_word("ПРИВЕТ", known), None); // весь капс
        assert_eq!(fix_case_word("Привет", known), None); // уже правильно
        assert_eq!(fix_case_word("привет", known), None);
        assert_eq!(fix_case_word("iPhone", known), None); // смешанный регистр
        assert_eq!(fix_case_word("КамАЗ", known), None);
        assert_eq!(fix_case_word("яК", known), None); // короткое для паттерна 1
        assert_eq!(fix_case_word("v0.1", known), None); // не только буквы
        assert_eq!(fix_case_word("", known), None);
    }

    #[test]
    fn case_cycle_carries_over_to_the_flip_target() {
        // Cycling the case then flipping the layout must not throw the case away.
        assert_eq!(match_case("ckjdj ", "Слово "), "Ckjdj ");
        assert_eq!(match_case("ckjdj ", "СЛОВО "), "CKJDJ ");
        assert_eq!(match_case("CKJDJ ", "слово "), "ckjdj ");
        // A word with no letters to case (a bare separator) is left alone.
        assert_eq!(match_case(" ", " "), " ");
    }

    #[test]
    fn case_cycle_rotates_and_survives_separators() {
        assert_eq!(cycle_case("слово"), "Слово");
        assert_eq!(cycle_case("Слово"), "СЛОВО");
        assert_eq!(cycle_case("СЛОВО"), "слово");
        // Held-preedit со шлейфом-сепаратором.
        assert_eq!(cycle_case("слово "), "Слово ");
        assert_eq!(cycle_case("Слово "), "СЛОВО ");
        assert_eq!(cycle_case("СЛОВО "), "слово ");
        // Произвольный регистр нормализуется в нижний.
        assert_eq!(cycle_case("сЛоВо"), "слово");
    }

    #[test]
    fn learnable_accepts_words_and_filters_junk() {
        assert_eq!(learnable("привет"), Some(("привет".into(), Lang::Ru)));
        assert_eq!(learnable(" Увы "), Some(("увы".into(), Lang::Ru)));
        assert_eq!(learnable("tiktok"), Some(("tiktok".into(), Lang::En)));
        // Command-shaped, multi-word, digits, mixed script, single letters — never learned.
        assert_eq!(learnable("--force"), None);
        assert_eq!(learnable("v0.1"), None);
        assert_eq!(learnable("два слова"), None);
        assert_eq!(learnable("прив3т"), None);
        assert_eq!(learnable("приvет"), None);
        assert_eq!(learnable("я"), None);
        assert_eq!(learnable("  "), None);
    }

    #[test]
    fn conversion_counter_fires_on_threshold_and_resets() {
        let counts: ConvertCounts = Arc::new(std::sync::Mutex::new(Default::default()));
        assert!(!bump_conversion_count(&counts, 3, "привет", "ghbdtn"));
        assert!(!bump_conversion_count(&counts, 3, "привет", "ghbdtn"));
        // A different word doesn't interfere.
        assert!(!bump_conversion_count(&counts, 3, "увы", "eds"));
        assert!(bump_conversion_count(&counts, 3, "привет", "ghbdtn"));
        // The entry was reset — declining the offer doesn't re-ask immediately.
        assert!(!bump_conversion_count(&counts, 3, "привет", "ghbdtn"));
        // Threshold 1 fires on the first conversion.
        assert!(bump_conversion_count(&counts, 1, "тест", "ntcn"));
    }

    #[test]
    fn conversion_counter_does_not_grow_without_bound() {
        // Words that never reach the threshold used to stay in the map for the whole session.
        let counts: ConvertCounts = Arc::new(std::sync::Mutex::new(Default::default()));
        for i in 0..(MAX_TRACKED_CONVERSIONS * 2) {
            assert!(!bump_conversion_count(&counts, 3, &format!("слово{i}"), "ckjdj"));
        }
        assert!(
            counts.lock().unwrap().len() <= MAX_TRACKED_CONVERSIONS,
            "the counter must stay bounded"
        );
    }

    /// An engine wired up from `cfg`, plus the settings slot it reads through — so a test can
    /// swap the settings the way the config watcher does. Nothing here touches DBus: the
    /// transparency rules are pure state, which is exactly why they are testable.
    fn test_engine(cfg: &crate::config::Config) -> (PuntuEngine, SettingsSlot) {
        let dict = UserDict::empty(std::env::temp_dir().join("puntu-test-policy"));
        let det = Detector::new(
            crate::detect::Models::default(),
            crate::config::DetectConfig::default(),
        );
        let settings: SettingsSlot =
            Arc::new(std::sync::RwLock::new(Arc::new(EngineSettings::from_config(cfg))));
        let engine = PuntuEngine::new(
            1,
            Arc::new(std::sync::RwLock::new(Arc::new(det))),
            Arc::new(AsyncMutex::new(dict)),
            Arc::clone(&settings),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            Arc::new(std::sync::Mutex::new(Default::default())),
            Arc::new(std::sync::Mutex::new(None)),
        );
        (engine, settings)
    }

    /// Publish `cfg` the way the config watcher does.
    fn swap_settings(slot: &SettingsSlot, cfg: &crate::config::Config) {
        *slot.write().unwrap() = Arc::new(EngineSettings::from_config(cfg));
    }

    #[test]
    fn purpose_policy() {
        let (mut e, _slot) = test_engine(&crate::config::Config::default());
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
    fn a_client_without_preedit_support_is_transparent() {
        // The engine shows a half-typed word only in the preedit, so a client that doesn't
        // render one displays nothing at all while typing — «puntu перестал печатать».
        let (mut e, _slot) = test_engine(&crate::config::Config::default());
        assert!(!e.is_passthrough(), "capabilities unknown → assume the client is fine");

        e.caps = Some(CAP_PREEDIT_TEXT | 0x20);
        assert!(!e.is_passthrough());

        e.caps = Some(0x08); // FOCUS only — no preedit
        assert_eq!(e.passthrough_reason(), Some(Passthrough::NoPreedit));
    }

    #[test]
    fn preedit_capability_rule_can_be_turned_off() {
        let mut cfg = crate::config::Config::default();
        cfg.ibus_clients.require_preedit_capability = false;
        let (mut e, _slot) = test_engine(&cfg);
        e.caps = Some(0x08);
        assert!(!e.is_passthrough(), "the rule is opt-out for clients that under-report");
    }

    #[test]
    fn listed_clients_are_transparent() {
        // The reported case: an SDL game opens an IBus context, so WASD arrives looking like
        // typing and gets swallowed into the word buffer instead of moving the character.
        let (mut e, _slot) = test_engine(&crate::config::Config::default());
        e.caps = Some(CAP_PREEDIT_TEXT);
        e.client = "SDL2_Application".to_string();
        assert_eq!(e.passthrough_reason(), Some(Passthrough::Client));

        e.client = "gtk-im".to_string();
        assert!(!e.is_passthrough());

        // No client name at all (IBus older than FocusInId) must not disable the engine.
        e.client.clear();
        assert!(!e.is_passthrough());
    }

    #[test]
    fn secret_fields_outrank_every_other_rule() {
        // A password must never be buffered, whatever else is true of the client.
        let (mut e, _slot) = test_engine(&crate::config::Config::default());
        e.client = "SDL2_Application".to_string();
        e.purpose = PURPOSE_PASSWORD;
        assert_eq!(e.passthrough_reason(), Some(Passthrough::Secret));
    }

    #[test]
    fn emit_failures_latch_degraded_and_a_focus_change_clears_it() {
        let (mut e, _slot) = test_engine(&crate::config::Config::default());
        for _ in 1..MAX_EMIT_FAILURES {
            e.note_emit(false);
            assert!(!e.is_passthrough(), "a stray failure must not disable the engine");
        }
        e.note_emit(false);
        assert_eq!(e.passthrough_reason(), Some(Passthrough::Degraded));

        // The fuse is per-context: the next field deserves a fresh try, or one bad moment
        // would last until the daemon is restarted — the bug we are fixing.
        e.recover();
        assert!(!e.is_passthrough());

        // A success mid-way resets the run, so slow-but-working stays working.
        e.note_emit(false);
        e.note_emit(true);
        for _ in 1..MAX_EMIT_FAILURES {
            e.note_emit(false);
        }
        assert!(!e.is_passthrough());
    }

    /// A finished word as the engine sees it: `cur` is what the keys type in the active
    /// layout, `alt` the same keys read through the other one.
    fn word(cur: &str, alt: &str) -> CompletedWord {
        CompletedWord {
            keys: Vec::new(),
            cur: cur.to_string(),
            alt: alt.to_string(),
            lang: Lang::En,
            trusted: true,
        }
    }

    fn dict_with_replacements(tag: &str, pairs: &[(&str, &str)]) -> UserDict {
        let mut d = UserDict::empty(std::env::temp_dir().join(format!("puntu-test-{tag}")));
        for (k, v) in pairs {
            d.set_replacement(k, v).unwrap();
        }
        d
    }

    #[test]
    fn replacement_fires_on_either_reading_of_the_keys() {
        // The point of matching both readings: forgetting to switch layout is the situation
        // Puntu exists for, so `ривет = привет` has to work whether the user typed Russian
        // letters or hit the same keys with US active (`hbdtn`).
        let d = dict_with_replacements("repl-both", &[("ривет", "привет")]);

        // Typed with the RU layout: the Cyrillic reading is `cur`.
        let w = word("ривет", "hbdtn");
        assert_eq!(
            expand_replacement(&d, &w, EngineMode::Correcting),
            Some(("привет".to_string(), "ривет".to_string()))
        );
        // Same keys, layout not switched: now the Cyrillic reading is `alt`.
        let w = word("hbdtn", "ривет");
        assert_eq!(
            expand_replacement(&d, &w, EngineMode::Correcting),
            Some(("привет".to_string(), "ривет".to_string()))
        );
        // A word with no rule is left alone.
        assert_eq!(expand_replacement(&d, &word("привет", "ghbdtn"), EngineMode::Correcting), None);
    }

    #[test]
    fn colliding_keys_resolve_by_the_current_mode() {
        // Both readings have a rule. Which one wins must not depend on hash order: the reading
        // the current mode renders by default is tried first.
        let d = dict_with_replacements("repl-collide", &[("no", "number"), ("тщ", "точно")]);
        let w = word("no", "тщ");
        assert_eq!(
            expand_replacement(&d, &w, EngineMode::Correcting).map(|(v, _)| v),
            Some("number".to_string()),
            "Correcting shows the Latin reading, so its rule wins"
        );
        assert_eq!(
            expand_replacement(&d, &w, EngineMode::DirectRussian).map(|(v, _)| v),
            Some("точно".to_string()),
            "RU-direct shows the Cyrillic reading, so its rule wins"
        );
    }

    #[test]
    fn replacement_case_follows_the_typed_word_but_spares_snippets() {
        let d = dict_with_replacements(
            "repl-case",
            &[("ривет", "привет"), ("адр", "ул. Пушкина, д. 1")],
        );
        let got = |typed: &str| {
            expand_replacement(&d, &word(typed, "?"), EngineMode::Correcting).map(|(v, _)| v)
        };
        assert_eq!(got("ривет"), Some("привет".to_string()));
        assert_eq!(got("Ривет"), Some("Привет".to_string()));
        assert_eq!(got("РИВЕТ"), Some("ПРИВЕТ".to_string()));
        // A multi-word value carries its own capitalisation — re-casing it would be vandalism.
        assert_eq!(got("Адр"), Some("ул. Пушкина, д. 1".to_string()));
        assert_eq!(got("АДР"), Some("ул. Пушкина, д. 1".to_string()));
    }

    #[test]
    fn flipping_a_replacement_teaches_nothing() {
        // A replacement is a rule the user wrote. Undoing it is not evidence that the word
        // should go on the never-correct list, nor that they keep converting it by hand — the
        // two reactions `handle_undo` has for the other sources.
        let mut h = Held {
            shown: "привет ".to_string(),
            other: "ривет ".to_string(),
            source: HeldSource::Replacement,
            typed: "ривет".to_string(),
            converted: "hbdtn".to_string(),
            ..Held::default()
        };
        let learns = h.source == HeldSource::AutoConverted && !h.learned;
        let counts = h.source == HeldSource::Typed && !h.counted;
        assert!(!learns, "nothing to learn from undoing your own rule");
        assert!(!counts, "and it is not a manual conversion either");
        // The flip itself still works — that is what keeps a replacement undoable.
        std::mem::swap(&mut h.shown, &mut h.other);
        assert_eq!(h.shown, "ривет ");
    }

    #[test]
    fn a_config_reload_reaches_a_live_engine() {
        // The whole point of the settings slot: an engine created before the edit must obey
        // it. Copying the settings in at creation is what made every change need an
        // `ibus restart`, which drops the engine out of every window and loses the held word.
        let mut cfg = crate::config::Config::default();
        let (mut e, slot) = test_engine(&cfg);
        e.caps = Some(CAP_PREEDIT_TEXT);
        e.client = "Vim".to_string();
        assert!(!e.is_passthrough());

        cfg.ibus_clients.passthrough_clients = vec!["vim".to_string()];
        swap_settings(&slot, &cfg);
        assert_eq!(
            e.passthrough_reason(),
            Some(Passthrough::Client),
            "the running engine must see the new passthrough list"
        );

        // …and back again, without recreating the engine.
        cfg.ibus_clients.passthrough_clients.clear();
        swap_settings(&slot, &cfg);
        assert!(!e.is_passthrough());
    }

    #[test]
    fn reloaded_hold_commit_and_tap_threshold_take_effect() {
        // Two values that used to be baked in at construction: the idle-commit delay (read
        // straight from the slot) and the tap threshold (cached inside TapDetector, so it has
        // to be pushed in on each key event).
        let mut cfg = crate::config::Config::default();
        cfg.hold_commit_ms = 0;
        cfg.tap_max_hold_ms = 500;
        let (mut e, slot) = test_engine(&cfg);
        assert_eq!(e.settings().opts.hold_commit(), None, "0 disables the idle commit");

        cfg.hold_commit_ms = 800;
        cfg.tap_max_hold_ms = 120;
        swap_settings(&slot, &cfg);
        assert_eq!(
            e.settings().opts.hold_commit(),
            Some(std::time::Duration::from_millis(800))
        );

        // What `process_key_event` does once per event.
        e.tap.set_max_hold(e.settings().hotkeys.tap_max_hold_ms);
        assert_eq!(e.tap.max_hold, std::time::Duration::from_millis(120));
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
        assert_eq!(hk.mode_toggle_tap, Some(ctrl()));
        assert_eq!(hk.convert_last_tap, Some(ctrl_shift()));
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

    #[test]
    fn numpad_symbols_are_never_transliterated() {
        // The numpad is layout-independent, so RU-direct must leave its characters alone.
        // Running them through the main-row table printed a different key entirely.
        for k in [
            Keysym::KP_Divide,
            Keysym::KP_Decimal,
            Keysym::KP_Separator,
            Keysym::KP_Add,
            Keysym::KP_Subtract,
            Keysym::KP_Multiply,
            Keysym::KP_5,
        ] {
            assert!(is_numpad(k), "{k:?} must be recognised as numpad");
        }
        // What the guard prevents: the main-row table would turn these into other keys.
        assert_eq!(convert_char('/', Lang::En, Lang::Ru), '.');
        assert_eq!(convert_char('.', Lang::En, Lang::Ru), 'ю');
        assert_eq!(convert_char(',', Lang::En, Lang::Ru), 'б');
        // The main row itself is NOT numpad and must keep being remapped in RU-direct.
        for k in [Keysym::slash, Keysym::period, Keysym::comma, Keysym::grave] {
            assert!(!is_numpad(k), "{k:?} is a main-row key");
        }
    }
}

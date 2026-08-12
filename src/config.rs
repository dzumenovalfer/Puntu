//! TOML configuration: thresholds, hotkeys, runtime paths. Hot-reloaded by the daemon.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Directory holding config + user dictionaries (`~/.config/puntu` by default).
pub fn config_dir() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "puntu") {
        return dirs.config_dir().to_path_buf();
    }
    // Fallback if XDG lookup fails for some reason.
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".config").join("puntu")
}

/// Path of the daemon's control socket (CLI ⇄ daemon).
pub fn socket_path() -> PathBuf {
    if let Some(dirs) = directories::ProjectDirs::from("", "", "puntu") {
        if let Some(rt) = dirs.runtime_dir() {
            return rt.join("puntu.sock");
        }
    }
    std::env::temp_dir().join(format!("puntu-{}.sock", uid()))
}

/// The real user id. Read from the kernel, not from `$UID`: that is a shell *variable* and is
/// not exported, so the fallback path collapsed to one shared `/tmp/puntu-user.sock` for
/// everyone — and the second user on a machine could neither unlink it (sticky /tmp) nor bind
/// it, leaving them with no control socket at all.
fn uid() -> u32 {
    // SAFETY: `getuid` is always successful and takes no arguments.
    unsafe { libc::getuid() }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Detect-but-don't-inject. Safe mode for validating behaviour before going live.
    pub dry_run: bool,
    /// Convert text pasted from the clipboard: when enabled, pasting (Ctrl+V) followed by a
    /// Space converts any wrong-layout words in the pasted text to the other layout in place
    /// (see `try_paste_convert` in `input/capture.rs`; capped at `PASTE_CONVERT_MAX_CHARS`,
    /// single-line only). Off by default. A file-based, hot-reloaded flag so the CLI
    /// (`puntu config set paste_convert true`) and a future GUI can both toggle it via the
    /// shared [`Config`] load/save, with no extra daemon protocol.
    pub paste_convert: bool,
    /// Treat a bare tap of Ctrl (press+release with nothing in between) as a layout-switch
    /// hotkey, and a Ctrl+Shift tap as "convert selection". ON by default — this is the
    /// Punto-style trigger most users expect. Turn OFF if you've bound Right Ctrl (or any other
    /// bare Ctrl gesture) to your system layout switcher: in that setup every manual switch
    /// would be instantly undone by our injected Super+Space.
    pub enable_modifier_taps: bool,
    /// Fix accidental-caps signatures: `пРИВЕТ` → `Привет` (CapsLock + Shift on the first
    /// letter) and `ПРивет` → `Привет` (Shift released late; dictionary-checked). ALL-CAPS
    /// words and mixed-case names (iPhone, КамАЗ) are never touched.
    pub fix_case: bool,
    /// Max duration (ms) between the first modifier press and the last release for a
    /// modifier tap to fire. Longer chains are held shortcuts (Ctrl+click, an app-consumed
    /// chord like Ctrl+Shift+V in a terminal) and are ignored. Default 500.
    pub tap_max_hold_ms: u64,
    /// How long (ms) a finished word may stay uncommitted in the preedit, where the flip
    /// hotkey can still re-render it, before the engine commits it on its own.
    ///
    /// The word lives only in the preedit, so it belongs to a caret position we stop
    /// controlling as soon as the user reaches for the mouse: apps move the caret on a click
    /// and only then reset the input context, which used to drop the word wherever the click
    /// landed. Committing after a short idle keeps it where it was typed.
    ///
    /// The trade-off is the flip window: `Ctrl+` `` ` `` only re-renders a word that is still
    /// held. Raise this if you flip words late; lower it if a click still moves a word.
    /// `0` disables the timer entirely (the word waits indefinitely, as before).
    pub hold_commit_ms: u64,
    pub detect: DetectConfig,
    /// Dictionary-learning behaviour (the repeat-conversion suggestion dialog etc.).
    pub learning: LearningConfig,
    pub hotkeys: Hotkeys,
    /// Hotkeys for the IBus engine front-end. The IBus engine deals in xkb **keysyms**
    /// (string names like `"Pause"`, `"F12"`, `"Insert"`), not evdev keycodes, so it gets a
    /// separate section from [`Hotkeys`]. The flip/undo default is `"Ctrl+grave"` (present on
    /// every keyboard); `"Pause"`, `"F12"`, `"ScrollLock"`, `"Menu"`, `"Insert"` also work.
    pub ibus_hotkeys: IBusHotkeys,
    /// Which clients the IBus engine leaves alone entirely (games, clients with no preedit).
    pub ibus_clients: ClientPolicy,
}

/// IBus-side hotkey configuration. All values are xkb keysym names (case-insensitive;
/// see /usr/include/X11/keysymdef.h or `xkeysym::Keysym::*` for the full list).
///
/// `mode_toggle` and `convert_last_modifier` are MODIFIER-tap triggers — they fire on a
/// press-and-release of the named modifier(s) with no other key in between, just like Punto.
/// Recognised modifier names: `Ctrl`, `Shift`, `Alt`, `Super`, `Ctrl+Shift`, `Ctrl+Alt`, etc.
/// Set any of them to `"none"` to disable that gesture.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct IBusHotkeys {
    /// Flip the last (held) word between its two layout readings — undo/redo of the
    /// conversion. Default `"Ctrl+grave"` (works on every keyboard, unlike Pause).
    pub undo_key: String,
    /// Modifier tap that toggles Correcting↔DirectRussian mode. Any `+`-joined combination
    /// of `Ctrl`/`Shift`/`Alt`/`Super` (e.g. `"Alt+Shift"`, like the system layout switch);
    /// `"none"` disables. Default `"Ctrl"`.
    pub mode_toggle: String,
    /// Regular key (not a tap) that toggles the mode — e.g. `"Pause"`, `"CapsLock"`,
    /// `"Super+space"`. `"none"` (default) disables. NB: CapsLock also flips the caps state.
    pub mode_toggle_key: String,
    /// Modifier tap that re-converts the last commit (swap RU↔EN reading). Default
    /// `"Ctrl+Shift"`.
    pub convert_last: String,
    /// Regular hotkey (not a tap) that converts the current mouse-selected text via the
    /// PRIMARY selection clipboard. Use this if modifier-tap is unreliable on your setup —
    /// it's a normal keypress so it can't be confused with a chord. Default `"Ctrl+Alt+s"`.
    pub convert_selection_key: String,
    /// Remember a word in the dictionary: the mouse selection if there is one, else the
    /// last (held) word. Default `"Ctrl+Alt+d"` (d = dictionary); `"none"` disables.
    pub remember_key: String,
    /// Cycle the case of the last (held) word: `слово` → `Слово` → `СЛОВО` → `слово` —
    /// the case counterpart of the layout flip. Default `"Ctrl+Alt+u"` (u = uppercase);
    /// `"none"` disables.
    pub case_key: String,
}

impl Default for IBusHotkeys {
    fn default() -> Self {
        IBusHotkeys {
            // Ctrl+` (backtick) — works on every keyboard, including laptops without Pause.
            undo_key: "Ctrl+grave".to_string(),
            mode_toggle: "Ctrl".to_string(),
            mode_toggle_key: "none".to_string(),
            convert_last: "Ctrl+Shift".to_string(),
            // Ctrl+Alt+S — `s` for "selection", and Ctrl+Alt combos are mostly free in
            // user space (GNOME reserves Super+*, Alt+F* for window mgmt; terminals use
            // Ctrl+Shift+*; Electron apps grab Ctrl+Shift+`/I/J for devtools). Override
            // freely via `puntu config set convert_selection_key '...'`.
            convert_selection_key: "Ctrl+Alt+s".to_string(),
            remember_key: "Ctrl+Alt+d".to_string(),
            case_key: "Ctrl+Alt+u".to_string(),
        }
    }
}

/// Which clients the IBus engine refuses to touch at all.
///
/// The engine shows every letter the user types through the IBus **preedit** and only commits
/// it later, so a client that doesn't render preedit displays *nothing at all* while typing —
/// the «перестал печатать» report. The same swallowing is what makes a game unplayable: WASD
/// goes into the word buffer instead of to the game. Both are the same question — "is this
/// client a text field we can work with?" — so both live here.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientPolicy {
    /// Go transparent in clients that don't advertise `IBUS_CAP_PREEDIT_TEXT`. On by default:
    /// holding text in a preedit the client will never draw is strictly worse than not
    /// correcting at all. Turn off only if some client under-reports its capabilities and you
    /// would rather have corrections there.
    pub require_preedit_capability: bool,
    /// Client names (as passed to `CreateInputContext`) the engine stays transparent for.
    /// Matched case-insensitively as a **substring**, so `"SDL"` covers `SDL2_Application`
    /// and `SDL3_Application`. The engine logs `client=…` on every focus change, so an app
    /// that misbehaves can be read straight out of the journal and added here.
    pub passthrough_clients: Vec<String>,
    /// Also stay transparent in every X11/XWayland client that comes through IBus's XIM
    /// bridge (client name `xim`). Off by default — that would cover a lot of ordinary apps —
    /// but it is the one switch that reliably silences Wine/Proton games, which reach IBus
    /// through XIM rather than SDL.
    pub passthrough_xim: bool,
}

impl Default for ClientPolicy {
    fn default() -> Self {
        ClientPolicy {
            require_preedit_capability: true,
            // SDL creates its IBus context as soon as text input is enabled — which older SDL2
            // does at window creation — so a game's movement keys arrive here looking exactly
            // like typing.
            passthrough_clients: vec!["SDL".to_string()],
            passthrough_xim: false,
        }
    }
}

impl ClientPolicy {
    /// Is `client` on the passthrough list? Case-insensitive substring match; an empty client
    /// name (IBus older than 1.5.27, which never sends one) matches nothing, and an empty
    /// pattern is ignored rather than matching everything.
    pub fn matches_client(&self, client: &str) -> bool {
        if client.is_empty() {
            return false;
        }
        let client = client.to_lowercase();
        if self.passthrough_xim && client == "xim" {
            return true;
        }
        self.passthrough_clients
            .iter()
            .filter(|p| !p.trim().is_empty())
            .any(|p| client.contains(&p.trim().to_lowercase()))
    }
}

/// Dictionary-learning behaviour.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct LearningConfig {
    /// After this many *manual* conversions of the same word (forward flip via the undo key,
    /// or a single-word selection conversion) the engine offers — in a zenity question
    /// dialog — to remember the word in the dictionary. `0` disables the offer.
    pub suggest_after: u32,
}

impl Default for LearningConfig {
    fn default() -> Self {
        LearningConfig { suggest_after: 3 }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct DetectConfig {
    /// Minimum word length considered for autocorrection (avoids `a`, `я`, …).
    pub min_word_len: usize,
    /// How much better (in mean trigram log-prob) the other layout must score to switch.
    pub switch_delta: f64,
    /// The other-layout candidate must itself look like a real word (score ≥ this) before we
    /// switch — guards against converting a valid word into gibberish.
    pub alt_valid_min: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Hotkeys {
    /// Undo the last automatic correction (Punto's Pause/Break analog).
    pub undo: Hotkey,
    /// Manually convert the last word, even if auto-detect didn't fire.
    pub convert_last: Hotkey,
    /// Add the last word to the never-correct exceptions list.
    pub add_exception: Hotkey,
    /// Force-convert the last word and remember to always convert it.
    pub force_correct: Hotkey,
}

/// A modifier combo + key. Empty modifiers means "bare key".
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    /// Linux key code (e.g. 119 = Pause/Break).
    pub code: u16,
}

impl Default for Hotkey {
    fn default() -> Self {
        Hotkey { ctrl: false, alt: false, shift: false, meta: false, code: 0 }
    }
}

impl Hotkey {
    pub const fn new(code: u16, ctrl: bool, alt: bool, shift: bool) -> Self {
        Hotkey { ctrl, alt, shift, meta: false, code }
    }

    /// Does a key press with the given modifier state match this hotkey?
    pub fn matches(&self, code: u16, m: crate::keymap::Mods) -> bool {
        self.code != 0
            && self.code == code
            && self.ctrl == m.ctrl
            && self.alt == m.alt
            && self.shift == m.shift
            && self.meta == m.meta
    }
}

/// A modifier-set gesture: press the set (any order), release it, with no other key in
/// between. `Ctrl`, `Ctrl+Shift`, `Alt+Shift`, `Ctrl+Alt`, `Super` … — matched against the
/// configured `mode_toggle` / `convert_last` bindings, like the system layout-switch options
/// in GNOME Tweaks.
///
/// Lives here rather than in the IBus engine because BOTH front-ends have to honour the
/// gesture the user picked, and the uinput daemon builds without the `ibus` feature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct ModCombo {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub sup: bool,
}

impl ModCombo {
    pub fn size(self) -> u32 {
        self.ctrl as u32 + self.shift as u32 + self.alt as u32 + self.sup as u32
    }
}

impl From<crate::keymap::Mods> for ModCombo {
    fn from(m: crate::keymap::Mods) -> Self {
        ModCombo { ctrl: m.ctrl, shift: m.shift, alt: m.alt, sup: m.meta }
    }
}

/// Parse a tap-modifier combo from config (`"Ctrl"`, `"Alt+Shift"`, `"Ctrl+Alt"`, `"Super"`,
/// `"none"`) into a [`ModCombo`]. Returns `None` for `"none"`/empty/unparseable input, and
/// for a bare `"Shift"` — that gesture is an aborted capital letter, never a deliberate tap.
pub fn parse_tap_combo(s: &str) -> Option<ModCombo> {
    let mut combo = ModCombo::default();
    for p in s.split('+') {
        match p.trim().to_ascii_lowercase().as_str() {
            "" => {}
            "ctrl" | "control" => combo.ctrl = true,
            "shift" => combo.shift = true,
            "alt" => combo.alt = true,
            "super" | "meta" | "win" => combo.sup = true,
            _ => return None, // includes "none"
        }
    }
    if combo.size() == 0 || combo == (ModCombo { shift: true, ..Default::default() }) {
        return None;
    }
    Some(combo)
}

const KEY_PAUSE: u16 = 119;

/// Default for `tap_max_hold_ms` — a deliberate modifier tap is well under half a second.
pub const DEFAULT_TAP_MAX_HOLD_MS: u64 = 500;

/// Default for `hold_commit_ms`. Long enough to notice a wrong word and reach for the flip
/// hotkey, short enough that a word doesn't survive until the user clicks somewhere else.
pub const DEFAULT_HOLD_COMMIT_MS: u64 = 1500;

impl Default for Config {
    fn default() -> Self {
        Config {
            dry_run: false,
            paste_convert: false,
            enable_modifier_taps: true,
            fix_case: true,
            tap_max_hold_ms: DEFAULT_TAP_MAX_HOLD_MS,
            hold_commit_ms: DEFAULT_HOLD_COMMIT_MS,
            detect: DetectConfig::default(),
            learning: LearningConfig::default(),
            hotkeys: Hotkeys::default(),
            ibus_hotkeys: IBusHotkeys::default(),
            ibus_clients: ClientPolicy::default(),
        }
    }
}

impl Default for DetectConfig {
    fn default() -> Self {
        // Tuned against the bundled word lists. The alt-must-look-like-a-real-word gate
        // (`alt_valid_min`) keeps precision high, so `switch_delta` can be modest enough to
        // catch short common words (но/не/все), whose deltas are small.
        DetectConfig { min_word_len: 2, switch_delta: 0.7, alt_valid_min: -3.3 }
    }
}

impl Default for Hotkeys {
    fn default() -> Self {
        // All on Pause/Break with different modifiers — compact and in the spirit of Punto.
        Hotkeys {
            undo: Hotkey::new(KEY_PAUSE, false, false, false),
            convert_last: Hotkey::new(KEY_PAUSE, false, false, true), // Shift+Break
            add_exception: Hotkey::new(KEY_PAUSE, true, false, false), // Ctrl+Break
            force_correct: Hotkey::new(KEY_PAUSE, false, true, false), // Alt+Break
        }
    }
}

impl Config {
    pub fn path() -> PathBuf {
        config_dir().join("config.toml")
    }

    /// Load config from the default path, falling back to defaults if it doesn't exist.
    pub fn load() -> Result<Config> {
        let path = Self::path();
        if !path.exists() {
            return Ok(Config::default());
        }
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg = toml::from_str(&text)
            .with_context(|| format!("parsing config {}", path.display()))?;
        Ok(cfg)
    }

    /// Write the current config (used to scaffold a default file on first run).
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path())
    }

    /// Write the config to a specific path. Used by `puntu config set` (honouring `--config`) and
    /// by any future GUI editing the same file; the running daemon picks up the change via
    /// hot-reload. NOTE: serializes the whole file, so hand-written comments are not preserved —
    /// switching to `toml_edit` to keep them is a later polish.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_roundtrips_through_toml() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.detect.min_word_len, cfg.detect.min_word_len);
        assert_eq!(back.hotkeys.undo, cfg.hotkeys.undo);
        assert!(!back.paste_convert, "paste_convert defaults off");
    }

    #[test]
    fn missing_paste_convert_defaults_off() {
        // Old config files written before the flag existed must still load (serde default).
        let cfg: Config = toml::from_str("dry_run = true\n").unwrap();
        assert!(cfg.dry_run);
        assert!(!cfg.paste_convert);
    }

    #[test]
    fn modifier_taps_default_on() {
        // The Punto-style Ctrl-tap → switch_layout trigger is on by default — this is the
        // gesture most users expect. The flag exists so anyone who's bound a bare Ctrl gesture
        // (e.g. Right Ctrl) to their system layout switcher can turn it off and avoid the
        // double-switch.
        let cfg = Config::default();
        assert!(cfg.enable_modifier_taps);
        let from_old: Config = toml::from_str("dry_run = false\n").unwrap();
        assert!(from_old.enable_modifier_taps, "old config files inherit the default");
    }

    #[test]
    fn hotkey_matches_modifiers() {
        let hk = Hotkey::new(119, false, false, true);
        let shift = crate::keymap::Mods { shift: true, ..Default::default() };
        let plain = crate::keymap::Mods::default();
        assert!(hk.matches(119, shift));
        assert!(!hk.matches(119, plain));
        assert!(!hk.matches(120, shift));
    }

    #[test]
    fn client_policy_matches_sdl_by_substring() {
        // The default `"SDL"` entry has to cover every SDL generation and casing, since that
        // is the one client name a game reliably reports.
        let p = ClientPolicy::default();
        assert!(p.matches_client("SDL2_Application"));
        assert!(p.matches_client("sdl3_application"));
        assert!(!p.matches_client("gtk-im"));
        assert!(!p.matches_client("QIBusPlatformInputContext"));
    }

    #[test]
    fn client_policy_ignores_empty_names_and_patterns() {
        // IBus older than 1.5.27 never sends a client name; an empty pattern (a stray comma in
        // the config) must not turn into "match everything" and silently disable the engine.
        let mut p = ClientPolicy::default();
        assert!(!p.matches_client(""));
        p.passthrough_clients = vec![String::new(), "  ".to_string()];
        assert!(!p.matches_client("gtk-im"));
    }

    #[test]
    fn client_policy_xim_is_opt_in() {
        let mut p = ClientPolicy { passthrough_clients: Vec::new(), ..Default::default() };
        assert!(!p.matches_client("xim"), "XIM apps stay corrected by default");
        p.passthrough_xim = true;
        assert!(p.matches_client("xim"));
        assert!(!p.matches_client("gtk-im"), "the switch is XIM-only");
    }

    #[test]
    fn client_policy_survives_old_config_files() {
        // Config files written before the section existed must still load, with the protective
        // default on — otherwise upgrading would silently keep the invisible-typing bug.
        let old: Config = toml::from_str("dry_run = false\n").unwrap();
        assert!(old.ibus_clients.require_preedit_capability);
        assert!(old.ibus_clients.matches_client("SDL2_Application"));
    }
}

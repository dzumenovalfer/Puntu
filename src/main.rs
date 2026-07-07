//! Puntu command-line entry point.
//!
//! Subcommands `stdin` and `dict` work in the pure-core build
//! (`cargo run --no-default-features -- …`), so detection and dictionaries can be exercised
//! without root or devices. The actual daemon and the pause/resume/status client need the
//! default `daemon` feature.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use puntu::buffer::CompletedWord;
use puntu::config::{self, Config};
use puntu::detect::userdict::{is_command_context, ListKind, UserDict};
use puntu::detect::{Decision, Detector, Models};
use puntu::keymap::{self, Lang};

#[derive(Parser)]
#[command(name = "puntu", version, about = "Automatic keyboard-layout corrector")]
struct Cli {
    /// Override the config file path.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the daemon (default).
    Run {
        /// Detect but never inject — safe mode for validation.
        #[arg(long)]
        dry_run: bool,
        /// Use this input device instead of autodetecting the keyboard.
        #[arg(long)]
        device: Option<String>,
    },
    /// Feed words from stdin and print the detector's decision (no devices needed).
    Stdin,
    /// Manage user dictionaries.
    Dict {
        #[command(subcommand)]
        op: DictOp,
    },
    /// View or change config flags (written to config.toml; a running daemon applies them on
    /// hot-reload). Shared write path for the CLI and a future GUI.
    Config {
        #[command(subcommand)]
        op: ConfigOp,
    },
    /// Open the settings window (zenity): learning toggles, hotkeys, thresholds.
    Settings,
    /// Pause autocorrection in the running daemon.
    Pause,
    /// Resume autocorrection in the running daemon.
    Resume,
    /// Query the running daemon.
    Status,
    /// Diagnostic: after 3s, run one correction (clipboard → Ctrl+V) to verify the mechanism.
    TestPaste,
    /// Diagnostic: after 3s, type "ghbdtn " then correct it to "привет " (full backspace+paste).
    TestCorrect,
    /// Diagnostic: after 3s, type `text` key-for-key on a US-locked virtual keyboard, then run
    /// the detector word-by-word and replace any wrong-layout words inline. Good for sanity
    /// checks across the whole alphabet (e.g. a Russian pangram).
    TestType {
        /// Russian text. Each word is mapped key-for-key to Latin (the wrong-layout form),
        /// typed out, and then corrected back via the same path the daemon uses.
        text: String,
    },
    /// Diagnostic: inject Super+Space (the layout-switch shortcut) and report if the layout changed.
    TestSwitch,
    /// Build a compact FST from a big word list (one word per line) for exact-match lookup.
    /// Output defaults to ~/.config/puntu/russian.fst.
    BuildDict {
        input: PathBuf,
        output: Option<PathBuf>,
    },
    /// Switch how Puntu intercepts your typing. Two frontends are available:
    ///   - `ibus`   — IBus engine: keystrokes go through us BEFORE the app; mostly race-free.
    ///   - `uinput` — Legacy daemon: reads /dev/input/event* + injects corrections via uinput.
    ///   - `off`    — Disable both.
    /// Stops the previously-active frontend and starts the requested one. Doesn't require sudo
    /// (uses `systemctl --user` and `ibus engine`).
    Mode {
        #[arg(value_enum)]
        target: ModeTarget,
    },
}

#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum ModeTarget {
    /// Activate the IBus engine front-end (recommended; needs `puntu-ibus install` first).
    Ibus,
    /// Activate the uinput daemon front-end (the original implementation).
    Uinput,
    /// Stop both frontends — Puntu does nothing until you switch a mode back on.
    Off,
    /// Print which mode is currently active (no changes).
    Status,
}

#[derive(Subcommand)]
enum ConfigOp {
    /// Print the current effective config as TOML.
    Show,
    /// Set a boolean flag and persist it. Known keys: `dry_run`, `paste_convert`.
    Set {
        /// Flag name (e.g. `paste_convert`).
        key: String,
        /// New value: true/false/on/off/1/0.
        value: String,
    },
}

#[derive(Subcommand)]
enum DictOp {
    /// Show a list.
    List {
        #[arg(long)]
        manual: bool,
        #[arg(long)]
        learned: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        commands: bool,
        #[arg(long)]
        ru: bool,
        #[arg(long)]
        en: bool,
    },
    /// Add a word (to exceptions by default).
    Add {
        word: String,
        /// Add to the always-convert list instead.
        #[arg(long)]
        force: bool,
        /// Add to the commands list instead.
        #[arg(long)]
        command: bool,
        #[arg(long)]
        ru: bool,
        #[arg(long)]
        en: bool,
    },
    /// Teach a recognized word (e.g. a service name) so its wrong-layout form converts.
    Learn {
        word: String,
        #[arg(long)]
        ru: bool,
        #[arg(long)]
        en: bool,
    },
    /// Open a simple dictionary window (zenity): word pairs («привет / ghbdtn»), add, remove.
    Ui,
    /// Remove a word from every list.
    Rm { word: String },
    /// Remove a single learned word.
    Forget {
        word: String,
        #[arg(long)]
        ru: bool,
        #[arg(long)]
        en: bool,
    },
    /// Clear all auto-learned words.
    ClearLearned,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cfg = load_config(cli.config.as_deref())?;

    match cli.cmd.unwrap_or(Cmd::Run { dry_run: false, device: None }) {
        Cmd::Run { dry_run, device } => run_daemon(cfg, dry_run, device),
        Cmd::Stdin => run_stdin(cfg),
        Cmd::Dict { op } => run_dict(op),
        Cmd::Config { op } => run_config(op, cli.config.as_deref()),
        Cmd::Settings => run_settings_ui(cli.config.as_deref()),
        Cmd::Pause => send("pause"),
        Cmd::Resume => send("resume"),
        Cmd::Status => send("status"),
        Cmd::TestPaste => test_paste(),
        Cmd::TestCorrect => test_correct(),
        Cmd::TestType { text } => test_type(cfg, text),
        Cmd::TestSwitch => test_switch(),
        Cmd::BuildDict { input, output } => build_dict(input, output),
        Cmd::Mode { target } => run_mode(target),
    }
}

/// Switch between IBus engine and uinput daemon front-ends. Doesn't require root: uses
/// `systemctl --user` to manage our service, and `ibus engine <name>` to flip the active
/// engine. Each transition leaves the system in a consistent state — exactly one frontend
/// active (or none, for `off`).
fn run_mode(target: ModeTarget) -> Result<()> {
    use std::process::Command;

    // Helpers we'll need repeatedly. Failures of the "other" frontend during a switch are
    // logged but not fatal — what matters is that the requested frontend ends up active.
    fn stop_uinput_daemon() {
        // Try graceful stop first via the systemd user service we ship.
        let _ = Command::new("systemctl").args(["--user", "stop", "puntu.service"]).status();
        // Fallback: read the daemon's pidfile and kill only that PID.
        // CANNOT use `pkill -x puntu` here — this very process is named `puntu`, so pkill
        // would terminate ourselves mid-command (the previous version of this code did exactly
        // that). The daemon writes its PID to ~/.config/puntu/puntu.pid on startup; if the
        // file's absent or stale, the daemon isn't actually running and there's nothing to do.
        let pidfile = puntu::config::config_dir().join("puntu.pid");
        if let Ok(content) = std::fs::read_to_string(&pidfile) {
            if let Ok(pid) = content.trim().parse::<i32>() {
                if pid != std::process::id() as i32 {
                    // SIGTERM the daemon; ignore failures (already dead, etc.).
                    unsafe {
                        libc::kill(pid, libc::SIGTERM);
                    }
                    // Give it ~200ms to release the keyboard, then check.
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
    }
    fn start_uinput_daemon() -> Result<()> {
        let r = Command::new("systemctl").args(["--user", "start", "puntu.service"]).status();
        if matches!(r, Ok(s) if s.success()) {
            return Ok(());
        }
        // Fallback: launch directly. Detach via setsid so the daemon survives this shell exit.
        let exe = std::env::current_exe()
            .context("locating puntu binary for fallback launch")?;
        let child = Command::new("setsid")
            .arg(&exe)
            .arg("run")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .with_context(|| format!("launching {} run", exe.display()))?;
        // Detach: drop the child handle so we don't wait().
        std::mem::forget(child);
        Ok(())
    }
    fn enable_ibus_engine() -> Result<()> {
        let out = Command::new("ibus").args(["engine", "puntu"]).output()
            .context("running `ibus engine puntu` (is ibus installed?)")?;
        if !out.status.success() {
            anyhow::bail!(
                "`ibus engine puntu` failed: {}\nTry `puntu-ibus install` and re-run.",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
    fn disable_ibus_engine() {
        let fallback = std::env::var("PUNTU_FALLBACK_ENGINE")
            .unwrap_or_else(|_| "xkb:us::eng".to_string());
        let _ = Command::new("ibus").args(["engine", &fallback]).status();
    }
    fn current_ibus_engine() -> String {
        Command::new("ibus")
            .arg("engine")
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }
    fn uinput_running() -> bool {
        // Use the pidfile rather than `pgrep -x puntu`: pgrep would match this very process
        // (we're also named `puntu`), giving a false positive on status. The daemon writes
        // its pid to ~/.config/puntu/puntu.pid; we check if that PID is actually alive AND is
        // someone other than us.
        let pidfile = puntu::config::config_dir().join("puntu.pid");
        let Ok(content) = std::fs::read_to_string(&pidfile) else {
            return false;
        };
        let Ok(pid) = content.trim().parse::<i32>() else {
            return false;
        };
        if pid == std::process::id() as i32 {
            return false;
        }
        // Signal 0 = check that the process exists without sending anything.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    match target {
        ModeTarget::Ibus => {
            stop_uinput_daemon();
            enable_ibus_engine()?;
            println!("mode: ibus (IBus engine active; uinput daemon stopped)");
        }
        ModeTarget::Uinput => {
            disable_ibus_engine();
            start_uinput_daemon()?;
            println!("mode: uinput (legacy daemon running; IBus engine deactivated)");
        }
        ModeTarget::Off => {
            stop_uinput_daemon();
            disable_ibus_engine();
            println!("mode: off (no frontend active)");
        }
        ModeTarget::Status => {
            let engine = current_ibus_engine();
            let daemon = if uinput_running() { "running" } else { "not running" };
            let mode = match (engine.as_str(), uinput_running()) {
                ("puntu", false) => "ibus",
                ("puntu", true) => "ibus + uinput (BOTH active — likely misconfigured)",
                (_, true) => "uinput",
                (_, false) => "off",
            };
            println!("mode: {mode}");
            println!("  IBus engine: {engine}");
            println!("  uinput daemon: {daemon}");
        }
    }
    Ok(())
}

/// Build a compact FST set from a big word list (one Russian word per line) for low-memory
/// exact-match lookup. ~1.5M words compress to a few MB and load near-instantly.
fn build_dict(input: PathBuf, output: Option<PathBuf>) -> Result<()> {
    let out = output.unwrap_or_else(|| config::config_dir().join("russian.fst"));
    eprintln!("Reading {}…", input.display());
    let text = std::fs::read_to_string(&input)?;
    let mut words: Vec<String> = text
        .lines()
        .map(|l| l.trim().to_lowercase())
        .filter(|w| is_cyrillic_word(w))
        .collect();
    words.sort();
    words.dedup();
    eprintln!("{} unique words → building FST…", words.len());

    let mut builder = fst::SetBuilder::memory();
    for w in &words {
        builder.insert(w)?;
    }
    let bytes = builder.into_inner()?;
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&out, &bytes)?;
    eprintln!("Wrote {} ({:.1} MB). Restart the daemon to use it.", out.display(), bytes.len() as f64 / 1e6);
    Ok(())
}

/// A Cyrillic word of length 2–20 (lowercase а-яё), excluding affix entries like `-ка`.
fn is_cyrillic_word(w: &str) -> bool {
    let n = w.chars().count();
    (2..=20).contains(&n) && w.chars().all(|c| ('\u{0430}'..='\u{044F}').contains(&c) || c == 'ё')
}

fn load_config(path: Option<&std::path::Path>) -> Result<Config> {
    match path {
        // Mirror `Config::load`: a not-yet-created file means "use defaults", so `--config` works
        // on first run (and `config set` can scaffold it).
        Some(p) if p.exists() => Config::load_from(p),
        Some(_) => Ok(Config::default()),
        None => Config::load(),
    }
}

#[cfg(feature = "daemon")]
fn init_logging() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("PUNTU_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

// ---- daemon (feature-gated) ----

#[cfg(feature = "daemon")]
fn run_daemon(mut cfg: Config, dry_run: bool, device: Option<String>) -> Result<()> {
    init_logging();
    if dry_run {
        cfg.dry_run = true;
    }
    puntu::input::run(cfg, device)
}

#[cfg(not(feature = "daemon"))]
fn run_daemon(_cfg: Config, _dry_run: bool, _device: Option<String>) -> Result<()> {
    anyhow::bail!("this build has no `daemon` feature; rebuild with default features to run the daemon")
}

#[cfg(feature = "daemon")]
fn send(cmd: &str) -> Result<()> {
    let resp = puntu::ipc::send_command(&config::socket_path(), cmd)?;
    println!("{resp}");
    Ok(())
}

#[cfg(not(feature = "daemon"))]
fn send(_cmd: &str) -> Result<()> {
    anyhow::bail!("this build has no `daemon` feature; pause/resume/status need the daemon")
}

#[cfg(feature = "daemon")]
fn test_paste() -> Result<()> {
    use std::time::Duration;
    let mut em = puntu::input::emitter::Emitter::new()?;
    if !puntu::clipboard::available() {
        eprintln!("warning: wl-copy not found — install wl-clipboard");
    }
    eprintln!("Focus a text field NOW. In 3s I'll put 'ПРОВЕРКА ' on the clipboard and Ctrl+V it.");
    std::thread::sleep(Duration::from_secs(3));
    let saved = puntu::clipboard::get().ok();
    puntu::clipboard::set("ПРОВЕРКА ")?;
    std::thread::sleep(Duration::from_millis(120));
    em.paste()?; // Ctrl+V
    std::thread::sleep(Duration::from_millis(250));
    if let Some(s) = saved {
        let _ = puntu::clipboard::set(&s);
    }
    eprintln!("Done. If 'ПРОВЕРКА ' appeared in the field, the clipboard-paste correction works.");
    Ok(())
}

#[cfg(not(feature = "daemon"))]
fn test_paste() -> Result<()> {
    anyhow::bail!("this build has no `daemon` feature; test-paste needs the daemon")
}

#[cfg(feature = "daemon")]
fn test_correct() -> Result<()> {
    use std::time::Duration;
    let mut em = puntu::input::emitter::Emitter::new()?;
    eprintln!("Focus a text field NOW. In 3s I'll type 'ghbdtn ' then correct it to 'привет '.");
    std::thread::sleep(Duration::from_secs(3));

    // Type "ghbdtn " (our virtual device is us-locked, so these keys render as Latin).
    for code in [34u16, 35, 48, 32, 20, 49, 57] {
        em.tap(code, false)?;
        std::thread::sleep(Duration::from_millis(40));
    }
    std::thread::sleep(Duration::from_millis(400));

    // Correct exactly like the daemon does: clipboard "привет ", backspace 7, paste.
    let saved = puntu::clipboard::get().ok();
    puntu::clipboard::set("привет ")?;
    std::thread::sleep(Duration::from_millis(120));
    em.backspace(7, puntu::input::emitter::BackspaceSpeed::Safe)?;
    em.paste()?;
    std::thread::sleep(Duration::from_millis(250));
    if let Some(s) = saved {
        let _ = puntu::clipboard::set(&s);
    }
    eprintln!("Done. You should have seen 'ghbdtn ' turn into 'привет '.");
    Ok(())
}

#[cfg(not(feature = "daemon"))]
fn test_correct() -> Result<()> {
    anyhow::bail!("this build has no `daemon` feature; test-correct needs the daemon")
}

/// Diagnostic: type each word of `text` in its **wrong-layout** form (Russian → Latin), then ask
/// the detector what to do and apply the conversion the same way the daemon would. Lets us
/// exercise the whole bottom layer (uinput typing, clipboard, backspace, paste) across the entire
/// alphabet from a single command — e.g. a Russian pangram covers every letter mapping.
#[cfg(feature = "daemon")]
fn test_type(cfg: Config, text: String) -> Result<()> {
    use puntu::buffer::CompletedWord;
    use puntu::detect::{Decision, Detector, Models};
    use puntu::detect::userdict::UserDict;
    use std::time::Duration;

    let dir = config::config_dir();
    let detector = Detector::new(Models::load(&dir), cfg.detect.clone());
    let dict = UserDict::load(dir).unwrap_or_else(|_| UserDict::empty(config::config_dir()));
    let mut em = puntu::input::emitter::Emitter::new()?;

    eprintln!(
        "Focus a text field NOW. In 3s I'll type each Russian word as its wrong-layout (Latin) \
         form on a US-locked virtual keyboard, then auto-correct each one back."
    );
    std::thread::sleep(Duration::from_secs(3));

    let mut converted_count = 0usize;
    let mut left_count = 0usize;
    let mut total_words = 0usize;

    // Split on whitespace but remember which separator followed each word so the typed-out
    // version reads naturally. Anything that doesn't tokenise as Russian letters is typed
    // verbatim (digits, punctuation, Latin words) — we only convert RU runs.
    for (word, trailing_space) in split_with_trailing(&text) {
        let lang = puntu::Lang::Ru;
        // Map each char to (code, shift) in RU; chars that aren't on the RU layout are skipped.
        let keys: Vec<(u16, bool)> = word
            .chars()
            .filter_map(|c| keymap::find_key(c, lang))
            .collect();
        if keys.is_empty() {
            // Not a Russian word — type it as-is (Latin / punctuation) and move on.
            type_string_us(&mut em, word)?;
            if trailing_space {
                em.tap(keymap::KEY_SPACE, false)?;
                std::thread::sleep(Duration::from_millis(40));
            }
            continue;
        }
        total_words += 1;
        // Type the wrong-layout form key-for-key. Our virtual keyboard renders in US.
        for &(code, shift) in &keys {
            em.tap(code, shift)?;
            std::thread::sleep(Duration::from_millis(25));
        }
        if trailing_space {
            em.tap(keymap::KEY_SPACE, false)?;
            std::thread::sleep(Duration::from_millis(25));
        }
        std::thread::sleep(Duration::from_millis(150));

        // Build the same CompletedWord the daemon would and ask the detector.
        let cw = CompletedWord::from_keys(keys.clone(), puntu::Lang::En, true);
        let trailing = trailing_space.then_some((keymap::KEY_SPACE, false));
        let decision = detector.decide(&cw, &dict);
        match decision {
            Decision::Convert { to: _ } => {
                converted_count += 1;
                eprintln!("  convert  {:>14?} → {:?}", cw.cur, cw.alt);
                if let Err(e) = puntu::action::apply_correction(&mut em, &cw, puntu::Lang::Ru, trailing) {
                    eprintln!("    !! correction failed: {e}");
                }
            }
            Decision::Leave => {
                left_count += 1;
                eprintln!("  leave    {:>14?}  (alt was {:?})", cw.cur, cw.alt);
            }
        }
    }
    eprintln!(
        "\nDone. {total_words} RU word(s) typed; {converted_count} converted, {left_count} left as-is."
    );
    Ok(())
}

/// Type an arbitrary ASCII string (Latin letters, digits, common punctuation) on the US-locked
/// virtual keyboard, character by character. Unmappable chars are silently dropped.
#[cfg(feature = "daemon")]
fn type_string_us(em: &mut puntu::input::emitter::Emitter, s: &str) -> Result<()> {
    use std::time::Duration;
    for c in s.chars() {
        if let Some((code, shift)) = keymap::find_key(c, puntu::Lang::En) {
            em.tap(code, shift)?;
            std::thread::sleep(Duration::from_millis(25));
        }
    }
    Ok(())
}

/// Split `text` into (word, trailing_space?) chunks, preserving the order so the typed-out
/// stream reads naturally. A run of non-space chars is a "word"; whitespace becomes the
/// trailing flag on the previous word.
#[cfg(feature = "daemon")]
fn split_with_trailing(text: &str) -> Vec<(&str, bool)> {
    let mut out: Vec<(&str, bool)> = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if let Some(s) = start.take() {
                out.push((&text[s..i], true));
            } else if let Some(last) = out.last_mut() {
                last.1 = true;
            }
        } else if start.is_none() {
            start = Some(i);
        }
    }
    if let Some(s) = start {
        out.push((&text[s..], false));
    }
    out
}

#[cfg(not(feature = "daemon"))]
fn test_type(_cfg: Config, _text: String) -> Result<()> {
    anyhow::bail!("this build has no `daemon` feature; test-type needs the daemon")
}

#[cfg(feature = "daemon")]
fn test_switch() -> Result<()> {
    use std::time::Duration;
    let mut em = puntu::input::emitter::Emitter::new()?;
    let before = puntu::layout::active_lang().ok().flatten();
    eprintln!("Active layout before: {before:?}. Injecting Super+Space in 1s…");
    std::thread::sleep(Duration::from_secs(1));
    em.switch_layout()?;
    std::thread::sleep(Duration::from_millis(400));
    let after = puntu::layout::active_lang().ok().flatten();
    eprintln!("Active layout after:  {after:?}");
    if before != after {
        eprintln!("OK: layout switched. (And no Activities overview should have opened.)");
    } else {
        eprintln!("No change — the Super+Space shortcut may differ; check `gsettings get org.gnome.desktop.wm.keybindings switch-input-source`.");
    }
    Ok(())
}

#[cfg(not(feature = "daemon"))]
fn test_switch() -> Result<()> {
    anyhow::bail!("this build has no `daemon` feature; test-switch needs the daemon")
}

// ---- stdin harness (core) ----

fn run_stdin(cfg: Config) -> Result<()> {
    use std::io::BufRead;
    let det = Detector::new(Models::load(&config::config_dir()), cfg.detect);
    let dict = UserDict::load(config::config_dir()).unwrap_or_else(|_| UserDict::empty(config::config_dir()));

    eprintln!("Type words (as they'd appear on screen); Ctrl-D to end.");
    eprintln!("Layout is guessed per word (Cyrillic→ru, Latin→en).\n");

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        for token in line.split_whitespace() {
            let lang = guess_lang(token);
            let keys: Vec<(u16, bool)> =
                token.chars().filter_map(|c| keymap::find_key(c, lang)).collect();
            if keys.is_empty() {
                continue;
            }
            let word = CompletedWord::from_keys(keys, lang, true);
            let decision = det.decide(&word, &dict);
            let verdict = match decision {
                Decision::Convert { to } => format!("CONVERT → {} ({:?})", word.alt, to),
                Decision::Leave => {
                    if is_command_context(&word.cur) {
                        "leave (command-like)".to_string()
                    } else {
                        "leave".to_string()
                    }
                }
            };
            println!("{:<16} [{}]  {}", word.cur, lang, verdict);
        }
    }
    Ok(())
}

/// Guess the layout a token was typed in: any Cyrillic char ⇒ Russian.
fn guess_lang(token: &str) -> Lang {
    if token.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)) {
        Lang::Ru
    } else {
        Lang::En
    }
}

// ---- config flags (core) ----

/// View or toggle config flags. File-based: writes `config.toml` (honouring `--config`); a
/// running daemon applies the change via hot-reload, and a future GUI uses the same load/save.
fn run_config(op: ConfigOp, path: Option<&std::path::Path>) -> Result<()> {
    let cfg_path = path.map(|p| p.to_path_buf()).unwrap_or_else(Config::path);
    match op {
        ConfigOp::Show => {
            print!("{}", toml::to_string_pretty(&load_config(path)?)?);
        }
        ConfigOp::Set { key, value } => {
            let mut cfg = load_config(path)?;
            // Boolean flags — value parsed as on/off/true/false/1/0.
            let bool_keys = ["dry_run", "paste_convert", "enable_modifier_taps", "fix_case"];
            // String flags — IBus hotkey names. `undo_key`: keysym name (Pause, F12, Insert,
            // Menu, ScrollLock, F1..F12). `mode_toggle` / `convert_last`: modifier tap combo
            // (Ctrl, Shift, Ctrl+Shift, or "none" to disable).
            let string_keys = ["undo_key", "mode_toggle", "convert_last", "convert_selection_key"];
            let display = match key.as_str() {
                "dry_run" => { cfg.dry_run = parse_bool(&value)?; format!("{}", cfg.dry_run) }
                "paste_convert" => { cfg.paste_convert = parse_bool(&value)?; format!("{}", cfg.paste_convert) }
                "enable_modifier_taps" => { cfg.enable_modifier_taps = parse_bool(&value)?; format!("{}", cfg.enable_modifier_taps) }
                "fix_case" => { cfg.fix_case = parse_bool(&value)?; format!("{}", cfg.fix_case) }
                "tap_max_hold_ms" => {
                    cfg.tap_max_hold_ms = value.trim().parse().context("expected milliseconds (e.g. 500)")?;
                    format!("{}", cfg.tap_max_hold_ms)
                }
                "suggest_after" => {
                    cfg.learning.suggest_after =
                        value.trim().parse().context("expected a count (e.g. 3; 0 disables)")?;
                    format!("{}", cfg.learning.suggest_after)
                }
                "remember_key" => { cfg.ibus_hotkeys.remember_key = value.clone(); value.clone() }
                "mode_toggle_key" => { cfg.ibus_hotkeys.mode_toggle_key = value.clone(); value.clone() }
                "case_key" => { cfg.ibus_hotkeys.case_key = value.clone(); value.clone() }
                "undo_key" => { cfg.ibus_hotkeys.undo_key = value.clone(); value.clone() }
                "mode_toggle" => { cfg.ibus_hotkeys.mode_toggle = value.clone(); value.clone() }
                "convert_last" => { cfg.ibus_hotkeys.convert_last = value.clone(); value.clone() }
                "convert_selection_key" => { cfg.ibus_hotkeys.convert_selection_key = value.clone(); value.clone() }
                other => anyhow::bail!(
                    "unknown config key {other:?}\n\
                     boolean keys: {bool_keys:?}\n\
                     string keys: {string_keys:?}\n\
                     examples:\n  \
                     puntu config set undo_key 'Ctrl+grave'\n  \
                     puntu config set convert_selection_key 'Ctrl+Shift+c'\n  \
                     puntu config set mode_toggle Ctrl\n  \
                     puntu config set convert_last none"
                ),
            };
            cfg.save_to(&cfg_path)?;
            println!(
                "set {key} = {display} in {} (restart puntu-ibus or run `puntu mode ibus` to apply)",
                cfg_path.display()
            );
        }
    }
    Ok(())
}

fn parse_bool(v: &str) -> Result<bool> {
    match v.trim().to_ascii_lowercase().as_str() {
        "true" | "on" | "1" | "yes" => Ok(true),
        "false" | "off" | "0" | "no" => Ok(false),
        other => anyhow::bail!("expected a boolean (true/false/on/off/1/0), got {other:?}"),
    }
}

// ---- dict management (core) ----

fn run_dict(op: DictOp) -> Result<()> {
    let dir = config::config_dir();
    std::fs::create_dir_all(&dir).ok();
    let mut dict = UserDict::load(dir)?;

    match op {
        DictOp::List { manual, learned, force, commands, ru, en } => {
            let kind = pick_kind(manual, learned, force, commands);
            if kind == ListKind::Command {
                print_list(&dict, ListKind::Command, Lang::En); // language-neutral
            } else {
                for lang in langs(ru, en) {
                    print_list(&dict, kind, lang);
                }
            }
        }
        DictOp::Add { word, force, command, ru, en } => {
            let kind = if command {
                ListKind::Command
            } else if force {
                ListKind::Force
            } else {
                ListKind::Manual
            };
            let lang = pick_lang(&word, ru, en);
            dict.add(&word, lang, kind)?;
            println!("added {word:?} to {kind:?} [{lang}]");
            // The three lists are easy to mix up — say what each add actually did, and point
            // at the command the user probably wanted ("teach the engine a new word").
            let other = puntu::detect::translit::convert(&word, lang, lang.other());
            match kind {
                ListKind::Manual => {
                    println!(
                        "  Manual = exception list: {word:?} will now NEVER be auto-corrected.\n\
                         \x20 To teach the engine a word (so its wrong-layout form {other:?} \
                         converts to {word:?}), use:\n\
                         \x20     puntu dict learn {word}\n\
                         \x20 To always convert a specific typed form, add the form you TYPE:\n\
                         \x20     puntu dict add {other} --force"
                    );
                }
                ListKind::Force => {
                    println!(
                        "  Force list: typing {word:?} will now ALWAYS convert to {other:?}."
                    );
                }
                _ => {}
            }
            println!("  (the running engine picks this up within a second — no restart)");
        }
        DictOp::Learn { word, ru, en } => {
            let lang = pick_lang(&word, ru, en);
            dict.add(&word, lang, ListKind::Recognized)?;
            let other = puntu::detect::translit::convert(&word, lang, lang.other());
            println!(
                "learned {word:?} as a recognized {lang} word: typing {other:?} will convert \
                 to {word:?} (picked up within a second — no restart)"
            );
        }
        DictOp::Ui => run_dict_ui(&mut dict)?,
        DictOp::Rm { word } => {
            dict.remove(&word)?;
            println!("removed {word:?} from all lists");
        }
        DictOp::Forget { word, ru, en } => {
            let lang = pick_lang(&word, ru, en);
            dict.forget(&word, lang)?;
            println!("forgot learned {word:?} [{lang}]");
        }
        DictOp::ClearLearned => {
            dict.clear_learned()?;
            println!("cleared all learned words");
        }
    }
    Ok(())
}

/// The zenity-based settings window (`puntu settings`): parameters with their current
/// values; pick one to toggle or edit it. Booleans flip in place, everything else opens an
/// entry dialog pre-filled with the current value. Writes go through the same
/// `Config::save_to` as the CLI; since the engine reads the config at startup, a restart is
/// offered when anything changed.
fn run_settings_ui(path: Option<&std::path::Path>) -> Result<()> {
    use std::process::Command;

    if Command::new("zenity").arg("--version").output().is_err() {
        anyhow::bail!("zenity не найден — установите его:  sudo apt install zenity");
    }
    let cfg_path = path.map(|p| p.to_path_buf()).unwrap_or_else(Config::path);
    let onoff = |b: bool| if b { "вкл".to_string() } else { "выкл".to_string() };
    let mut changed = false;

    loop {
        let mut cfg = load_config(path)?;
        // (key, name, current value, description). `key` is a hidden machine id — zenity
        // prints it for the selected row (`--print-column=1 --hide-column=1`).
        let rows: Vec<(&str, &str, String, &str)> = vec![
            ("autocorrect", "Автокоррекция", onoff(!cfg.dry_run),
             "исправлять слова, набранные не в той раскладке"),
            ("enable_modifier_taps", "Тапы модификаторов", onoff(cfg.enable_modifier_taps),
             "тап Ctrl = смена режима; тап Ctrl+Shift = перевод выделения"),
            ("mode_toggle", "Тап смены режима EN↔RU", cfg.ibus_hotkeys.mode_toggle.clone(),
             "Ctrl, Shift, Ctrl+Shift или none"),
            ("convert_last", "Тап перевода выделения", cfg.ibus_hotkeys.convert_last.clone(),
             "Ctrl+Shift, Ctrl или none"),
            ("undo_key", "Флип последнего слова", cfg.ibus_hotkeys.undo_key.clone(),
             "например Ctrl+grave, F12, Pause"),
            ("convert_selection_key", "Перевод выделения (клавиша)",
             cfg.ibus_hotkeys.convert_selection_key.clone(),
             "надёжная альтернатива тапу"),
            ("remember_key", "Запомнить слово (клавиша)", cfg.ibus_hotkeys.remember_key.clone(),
             "запоминает выделенное/последнее слово; none = выключить"),
            ("suggest_after", "Предлагать запомнить после N переводов",
             cfg.learning.suggest_after.to_string(),
             "окно-вопрос после N ручных переводов слова; 0 = не предлагать"),
            ("tap_max_hold_ms", "Макс. длительность тапа, мс", cfg.tap_max_hold_ms.to_string(),
             "удержание дольше — это шорткат, не тап"),
            ("dict", "Словарь…", "открыть".into(), "пары слов, добавление и удаление"),
        ];
        let mut args: Vec<String> = [
            "--list",
            "--title=Настройки Puntu",
            "--text=Выберите параметр и нажмите «Изменить».",
            "--column=key",
            "--column=Параметр",
            "--column=Значение",
            "--column=Описание",
            "--hide-column=1",
            "--print-column=1",
            "--width=720",
            "--height=560",
            "--ok-label=Изменить",
            "--cancel-label=Закрыть",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        for (key, name, value, desc) in &rows {
            args.push(key.to_string());
            args.push(name.to_string());
            args.push(value.clone());
            args.push(desc.to_string());
        }

        let out = Command::new("zenity").args(&args).output()?;
        if !out.status.success() {
            break; // «Закрыть»
        }
        let key = String::from_utf8_lossy(&out.stdout).trim().to_string();

        // A small prompt pre-filled with the current value; returns None on cancel.
        let ask = |title: &str, current: &str| -> Option<String> {
            let o = Command::new("zenity")
                .args([
                    "--entry",
                    "--title=Настройки Puntu",
                    &format!("--text={title}"),
                    &format!("--entry-text={current}"),
                ])
                .output()
                .ok()?;
            o.status
                .success()
                .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        };
        let complain = |msg: &str| {
            let _ = Command::new("zenity")
                .args(["--error", "--title=Настройки Puntu", &format!("--text={msg}")])
                .status();
        };

        match key.as_str() {
            "autocorrect" => {
                cfg.dry_run = !cfg.dry_run;
            }
            "enable_modifier_taps" => {
                cfg.enable_modifier_taps = !cfg.enable_modifier_taps;
            }
            "mode_toggle" => {
                let Some(v) = ask("Тап смены режима (Ctrl, Shift, Ctrl+Shift, none):", &cfg.ibus_hotkeys.mode_toggle) else { continue };
                cfg.ibus_hotkeys.mode_toggle = v;
            }
            "convert_last" => {
                let Some(v) = ask("Тап перевода выделения (Ctrl+Shift, Ctrl, none):", &cfg.ibus_hotkeys.convert_last) else { continue };
                cfg.ibus_hotkeys.convert_last = v;
            }
            "undo_key" => {
                let Some(v) = ask("Клавиша флипа последнего слова:", &cfg.ibus_hotkeys.undo_key) else { continue };
                cfg.ibus_hotkeys.undo_key = v;
            }
            "convert_selection_key" => {
                let Some(v) = ask("Клавиша перевода выделения:", &cfg.ibus_hotkeys.convert_selection_key) else { continue };
                cfg.ibus_hotkeys.convert_selection_key = v;
            }
            "remember_key" => {
                let Some(v) = ask("Клавиша «запомнить слово» (none = выключить):", &cfg.ibus_hotkeys.remember_key) else { continue };
                cfg.ibus_hotkeys.remember_key = v;
            }
            "suggest_after" => {
                let Some(v) = ask("Предлагать запомнить после скольких переводов (0 = никогда):", &cfg.learning.suggest_after.to_string()) else { continue };
                match v.parse() {
                    Ok(n) => cfg.learning.suggest_after = n,
                    Err(_) => {
                        complain("Нужно число, например 3");
                        continue;
                    }
                }
            }
            "tap_max_hold_ms" => {
                let Some(v) = ask("Максимальная длительность тапа в миллисекундах:", &cfg.tap_max_hold_ms.to_string()) else { continue };
                match v.parse() {
                    Ok(n) => cfg.tap_max_hold_ms = n,
                    Err(_) => {
                        complain("Нужно число, например 500");
                        continue;
                    }
                }
            }
            "dict" => {
                let dir = config::config_dir();
                std::fs::create_dir_all(&dir).ok();
                let mut dict = UserDict::load(dir)?;
                run_dict_ui(&mut dict)?;
                continue;
            }
            _ => continue,
        }
        cfg.save_to(&cfg_path)?;
        changed = true;
    }

    if changed {
        let apply = Command::new("zenity")
            .args([
                "--question",
                "--title=Настройки Puntu",
                "--text=Настройки сохранены. Перезапустить движок, чтобы применить?",
                "--ok-label=Перезапустить",
                "--cancel-label=Позже",
            ])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if apply {
            let _ = Command::new("ibus").arg("restart").status();
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _ = Command::new("ibus").args(["engine", "puntu"]).status();
        }
    }
    Ok(())
}

/// The zenity-based dictionary window (`puntu dict ui`): a list of «word / typed-as» pairs
/// across the user lists, with add and remove. Pure subprocess calls — no GUI toolkit
/// dependency; the running engine picks every change up via hot-reload within a second.
fn run_dict_ui(dict: &mut UserDict) -> Result<()> {
    use std::process::Command;

    if Command::new("zenity").arg("--version").output().is_err() {
        anyhow::bail!("zenity не найден — установите его:  sudo apt install zenity");
    }

    let list_label = |kind: ListKind| match kind {
        ListKind::Recognized => "словарь (переводить)",
        ListKind::Force => "всегда переводить",
        ListKind::Learned => "не исправлять (обучено)",
        ListKind::Manual => "не исправлять",
        ListKind::Command => "команда",
    };

    loop {
        let mut args: Vec<String> = [
            "--list",
            "--title=Словарь Puntu",
            "--text=Выберите слово и нажмите «Удалить», или «Добавить» новое.",
            "--column=Слово",
            "--column=Набирается как",
            "--column=Список",
            "--width=600",
            "--height=640",
            "--ok-label=Удалить",
            "--cancel-label=Закрыть",
            "--extra-button=Добавить",
        ]
        .into_iter()
        .map(String::from)
        .collect();

        let mut empty = true;
        for lang in [Lang::Ru, Lang::En] {
            for kind in [ListKind::Force, ListKind::Learned, ListKind::Manual] {
                for w in dict.list(kind, lang) {
                    args.push(w.clone());
                    args.push(puntu::detect::translit::convert(&w, lang, lang.other()));
                    args.push(list_label(kind).to_string());
                    empty = false;
                }
            }
            // Recognized separately: show only the user's own words, not the built-in seeds.
            for w in dict.user_recognized(lang) {
                args.push(w.clone());
                args.push(puntu::detect::translit::convert(&w, lang, lang.other()));
                args.push(list_label(ListKind::Recognized).to_string());
                empty = false;
            }
        }
        if empty {
            // zenity --list refuses to open with zero rows — show a placeholder.
            args.push("(словарь пуст)".into());
            args.push("—".into());
            args.push("—".into());
        }

        let out = Command::new("zenity").args(&args).output()?;
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();

        if out.status.success() {
            // «Удалить» pressed with a row selected → stdout = the word (first column).
            if stdout.is_empty() || stdout.starts_with('(') {
                continue;
            }
            dict.remove(&stdout)?;
            println!("removed {stdout:?} from all lists");
        } else if stdout == "Добавить" {
            let add = Command::new("zenity")
                .args([
                    "--entry",
                    "--title=Puntu",
                    "--text=Слово в правильной раскладке (например «привет» или «tiktok»):",
                ])
                .output()?;
            if !add.status.success() {
                continue;
            }
            let word = String::from_utf8_lossy(&add.stdout).trim().to_lowercase();
            if word.is_empty() {
                continue;
            }
            let lang = pick_lang(&word, false, false);
            dict.add(&word, lang, ListKind::Recognized)?;
            let wrong = puntu::detect::translit::convert(&word, lang, lang.other());
            println!("learned {word:?} [{lang}]");
            let _ = Command::new("zenity")
                .args([
                    "--info",
                    "--title=Puntu",
                    &format!("--text=Запомнено: {wrong} → {word}"),
                    "--timeout=3",
                ])
                .status();
        } else {
            break; // «Закрыть» or the window was closed
        }
    }
    Ok(())
}

fn pick_kind(_manual: bool, learned: bool, force: bool, commands: bool) -> ListKind {
    // `manual` is the default when no other flag is set.
    if commands {
        ListKind::Command
    } else if learned {
        ListKind::Learned
    } else if force {
        ListKind::Force
    } else {
        ListKind::Manual
    }
}

fn langs(ru: bool, en: bool) -> Vec<Lang> {
    match (ru, en) {
        (true, false) => vec![Lang::Ru],
        (false, true) => vec![Lang::En],
        _ => vec![Lang::Ru, Lang::En],
    }
}

fn pick_lang(word: &str, ru: bool, en: bool) -> Lang {
    if ru {
        Lang::Ru
    } else if en {
        Lang::En
    } else {
        guess_lang(word)
    }
}

fn print_list(dict: &UserDict, kind: ListKind, lang: Lang) {
    let items = dict.list(kind, lang);
    let header = if kind == ListKind::Command {
        format!("{kind:?}")
    } else {
        format!("{kind:?} [{lang}]")
    };
    println!("== {header} ({}) ==", items.len());
    for w in items {
        println!("  {w}");
    }
}

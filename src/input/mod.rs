//! Daemon orchestration: open devices (passive read, no grab), spawn the helper threads, and
//! run the capture loop.

pub mod capture;
pub mod devices;
pub mod emitter;

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

use crate::config::{self, Config};
use crate::detect::userdict::UserDict;
use crate::detect::{Detector, Models};
use crate::ipc::{self, Control};
use crate::keymap::Lang;
use crate::layout;

/// State shared between the capture loop and the watcher/IPC/reload threads.
pub struct Shared {
    pub detector: Detector,
    pub dict: UserDict,
    pub cfg: Config,
}

pub type State = Arc<Mutex<Shared>>;

/// Lock-free flags polled by the capture loop.
#[derive(Clone)]
pub struct Flags {
    pub paused: Arc<AtomicBool>,
    /// Set by the mouse thread on any click → invalidate the trusted context.
    pub mouse_dirty: Arc<AtomicBool>,
    /// Cached active layout, refreshed in the background by [`layout_watcher`] so the capture
    /// loop never has to fork `gsettings` in its hot path — a previously per-keystroke cost
    /// that under load could stall keystroke processing long enough to drop events.
    pub lang: Arc<Mutex<Lang>>,
}

/// Run the daemon. `device` optionally overrides keyboard autodetection.
pub fn run(cfg: Config, device: Option<String>) -> Result<()> {
    // Two layers of defense against running two daemons concurrently — they would fight over
    // every correction (double backspace / double paste, lost keystrokes).
    //
    //   1. IPC status ping — fast and gives a friendly error when the existing daemon is
    //      responsive.
    //   2. Exclusive `flock` on a pidfile — catches the case the first daemon is **hung** and
    //      not answering IPC. The kernel releases the lock automatically on process exit
    //      (including SIGKILL), so a crashed previous run can't leave us locked out.
    if ipc::send_command(&config::socket_path(), "status").is_ok() {
        anyhow::bail!(
            "another puntu daemon is already running — stop it first \
             (`systemctl --user stop puntu` or `pkill -x puntu`)"
        );
    }
    let _instance_lock = acquire_instance_lock()?;

    // Ensure config dir exists and scaffold a *clean default* config on first run — never
    // persist a transient CLI override like `--dry-run`.
    let dir = config::config_dir();
    std::fs::create_dir_all(&dir).ok();
    if !Config::path().exists() {
        let _ = Config::default().save();
    }

    let models = Models::load(&dir);
    let dict = UserDict::load(dir).unwrap_or_else(|e| {
        tracing::warn!("could not load user dictionaries: {e}");
        UserDict::empty(config::config_dir())
    });
    let lang = layout::active_lang().ok().flatten().unwrap_or(Lang::En);
    tracing::info!("starting; active layout = {lang}, dry_run = {}", cfg.dry_run);
    if cfg.paste_convert {
        tracing::info!("paste conversion enabled: Ctrl+V then a separator converts pasted text");
    }
    if !crate::clipboard::available() {
        tracing::warn!(
            "wl-copy not found — corrections need it. Install with: sudo apt install wl-clipboard"
        );
    }

    let detector = Detector::new(models, cfg.detect.clone());
    let shared: State = Arc::new(Mutex::new(Shared { detector, dict, cfg }));
    let flags = Flags {
        paused: Arc::new(AtomicBool::new(false)),
        mouse_dirty: Arc::new(AtomicBool::new(false)),
        lang: Arc::new(Mutex::new(lang)),
    };

    // Open the keyboard for *passive* reading. We deliberately do NOT EVIOCGRAB it: the
    // compositor still delivers every key to apps, so if this daemon ever misbehaves or dies,
    // typing keeps working untouched. We only inject corrections via the virtual device.
    let (kbd_desc, keyboard) = match device {
        Some(p) => {
            let dev = devices::open(&p)?;
            let name = dev.name().unwrap_or("?").to_string();
            (format!("{p} ({name})"), dev)
        }
        None => {
            let (path, dev) = devices::find_keyboard()?;
            let name = dev.name().unwrap_or("?").to_string();
            (format!("{} ({name})", path.display()), dev)
        }
    };
    tracing::info!("listening on keyboard: {kbd_desc} (passive, no grab)");

    let emitter = emitter::Emitter::new()?;

    // (No layout watcher: the active layout is read fresh from `mru-sources` per word, which is
    // authoritative — `gsettings current` is not reliably updated when Mutter switches layout.)

    // Mouse click watcher (read-only) for context invalidation.
    if let Some((_, pointer)) = devices::find_pointer() {
        let flags = flags.clone();
        std::thread::spawn(move || capture::run_mouse(pointer, flags));
    } else {
        tracing::warn!("no pointer device found; mouse clicks won't invalidate the buffer");
    }

    // Layout cache poller: keeps `flags.lang` in sync with the system's active xkb layout so
    // the capture loop doesn't fork `gsettings` per keystroke.
    {
        let lang_cache = flags.lang.clone();
        std::thread::spawn(move || layout_watcher(lang_cache));
    }

    // Config + dictionary hot-reload.
    {
        let shared = shared.clone();
        std::thread::spawn(move || reload_watcher(shared));
    }

    // IPC control socket.
    {
        let control = Control { paused: flags.paused.clone() };
        std::thread::spawn(move || {
            if let Err(e) = ipc::serve(config::socket_path(), control) {
                tracing::error!("ipc server stopped: {e}");
            }
        });
    }

    // Capture loop runs on this thread.
    capture::run_keyboard(keyboard, emitter, shared, flags)
}

/// Hold an exclusive [`flock`] on `puntu.pid` for the lifetime of this daemon. The lock is
/// released by the kernel when the file descriptor closes — on normal exit, on panic-unwind, or
/// on SIGKILL — so a previous crashed run can't leave us permanently locked out.
///
/// Returns the `File` handle the caller MUST keep alive (dropping it releases the lock). The
/// pidfile contents are overwritten with the current PID so `pidof`/users can find the daemon.
fn acquire_instance_lock() -> Result<std::fs::File> {
    use std::fs::OpenOptions;
    use std::io::{Seek, SeekFrom, Write};
    use std::os::unix::io::AsRawFd;

    let pidfile = config::config_dir().join("puntu.pid");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&pidfile)
        .with_context(|| format!("opening pidfile {pidfile:?}"))?;

    // SAFETY: `flock` is a standard POSIX syscall; we hand it a valid fd from our own File.
    // LOCK_NB makes it non-blocking — if someone else has the lock, we get EWOULDBLOCK and bail.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let stale_pid = std::fs::read_to_string(&pidfile)
            .ok()
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        anyhow::bail!(
            "another puntu instance holds the lock at {pidfile:?} (pid: {stale_pid}). \
             Stop it first: `pkill -x puntu`. If it's hung and not responding, \
             `kill -9 {stale_pid}` will force-release the kernel lock."
        );
    }

    // Truncate and write our PID so external tools can discover us.
    file.seek(SeekFrom::Start(0)).context("pidfile seek")?;
    file.set_len(0).context("pidfile truncate")?;
    write!(file, "{}", std::process::id()).context("pidfile write")?;
    file.flush().context("pidfile flush")?;
    Ok(file)
}

/// Poll `gsettings` every 100ms and publish the active layout to the shared cache. Errors
/// (gsettings hung, GNOME restarting) are swallowed — the previous cached value remains in
/// effect so the capture loop never stalls waiting for a system service.
fn layout_watcher(cache: Arc<Mutex<Lang>>) {
    loop {
        std::thread::sleep(std::time::Duration::from_millis(100));
        if let Ok(Some(l)) = layout::active_lang() {
            if let Ok(mut c) = cache.lock() {
                if *c != l {
                    tracing::debug!("active layout changed: {} → {}", *c, l);
                    *c = l;
                }
            }
        }
    }
}

/// What kind of reload is pending, accumulated across debounced events.
#[derive(Default)]
struct PendingReload {
    config: bool,
    dict: bool,
    models: bool,
}

impl PendingReload {
    fn any(&self) -> bool {
        self.config || self.dict || self.models
    }

    /// Classify a changed path: which subsystem(s) does it belong to?
    fn observe(&mut self, path: &std::path::Path) {
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            return;
        };
        match name {
            "config.toml" => self.config = true,
            // FST → rebuild language models.
            "russian.fst" => self.models = true,
            // Commands list is language-neutral and only lives in the dict.
            "commands.txt" => self.dict = true,
            // Recognized-words lists feed BOTH the language models (as a training extension)
            // AND the dict's recognized set, so they must rebuild both.
            n if n.starts_with("words.") && n.ends_with(".txt") => {
                self.models = true;
                self.dict = true;
            }
            // Per-language user dictionaries (e.g. `manual.ru.txt`, `force.en.txt`,
            // `learned.ru.txt`) → hot-swap the dict only. The previous matcher used bare
            // `manual.txt`/`force.txt`/`learned.txt` and never fired for the real file names,
            // so CLI dict edits silently failed to reach the running daemon.
            n if n.ends_with(".txt")
                && (n.starts_with("manual.")
                    || n.starts_with("force.")
                    || n.starts_with("learned.")) =>
            {
                self.dict = true;
            }
            _ => {}
        }
    }
}

/// Apply a debounced reload. Heavy work (reading the 35 MB FST, training n-grams, parsing
/// config TOML) is done **outside** the `Shared` mutex so the capture loop is only blocked for
/// the few microseconds of an `Arc` swap — not the few hundred milliseconds of disk I/O. Before
/// this split, every config-dir change paused keystroke processing long enough for the kernel
/// evdev queue to overflow and **drop events** under fast typing.
fn apply_reload(shared: &State, pending: &PendingReload) {
    let started = std::time::Instant::now();
    let new_cfg = if pending.config {
        match Config::load() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!("config reload failed (keeping old): {e}");
                None
            }
        }
    } else {
        None
    };
    let new_models = if pending.models {
        Some(Models::load(&config::config_dir()))
    } else {
        None
    };
    let prep = started.elapsed();

    let locked = std::time::Instant::now();
    if let Ok(mut s) = shared.lock() {
        if let Some(cfg) = new_cfg {
            s.detector.set_config(cfg.detect.clone());
            s.cfg = cfg;
        }
        if let Some(models) = new_models {
            s.detector.set_models(models);
        }
        if pending.dict {
            if let Err(e) = s.dict.reload() {
                tracing::warn!("dict reload failed: {e}");
            }
        }
    }
    tracing::debug!(
        "reload applied (config={} dict={} models={}): prep {:?}, lock {:?}",
        pending.config,
        pending.dict,
        pending.models,
        prep,
        locked.elapsed()
    );
}

/// Watch the config directory and reload config + dictionaries on change. Events are
/// **debounced** (300 ms trailing edge) and classified by filename so only the affected
/// subsystem reloads — e.g. editing `manual.txt` no longer rebuilds the 35 MB language models.
/// All disk I/O happens outside the `Shared` mutex so the capture loop never stalls.
fn reload_watcher(shared: State) {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Duration, Instant};

    /// `notify` typically emits 2-3 events per logical save (file-create then writes). Collect
    /// them and only reload after this much quiet — long enough to coalesce a burst, short
    /// enough that the user sees their edit take effect almost immediately.
    const DEBOUNCE: Duration = Duration::from_millis(300);

    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("hot-reload disabled: {e}");
            return;
        }
    };
    let dir = config::config_dir();
    if watcher.watch(&dir, RecursiveMode::NonRecursive).is_err() {
        return;
    }

    let mut pending = PendingReload::default();
    let mut deadline: Option<Instant> = None;
    loop {
        let timeout = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3600),
        };
        match rx.recv_timeout(timeout) {
            Ok(Ok(ev)) => {
                for p in &ev.paths {
                    pending.observe(p);
                }
                // Editor swap-files etc. don't match any subsystem — don't even arm the timer.
                if pending.any() {
                    deadline = Some(Instant::now() + DEBOUNCE);
                }
            }
            Ok(Err(e)) => {
                tracing::debug!("notify error: {e}");
            }
            Err(RecvTimeoutError::Timeout) => {
                if pending.any() {
                    apply_reload(&shared, &pending);
                    pending = PendingReload::default();
                }
                deadline = None;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PendingReload;
    use std::path::PathBuf;

    fn observe(name: &str) -> PendingReload {
        let mut p = PendingReload::default();
        p.observe(&PathBuf::from(name));
        p
    }

    #[test]
    fn observes_per_language_dict_files() {
        // Regression: the real on-disk names are `manual.{lang}.txt`, etc. — the previous
        // matcher used bare `manual.txt` and never fired, so CLI dict edits never reached the
        // running daemon. All four per-language variants must trigger a dict reload.
        for name in [
            "manual.ru.txt",
            "manual.en.txt",
            "force.ru.txt",
            "force.en.txt",
            "learned.ru.txt",
            "learned.en.txt",
        ] {
            let p = observe(name);
            assert!(p.dict, "{name} should set dict");
            assert!(!p.config && !p.models, "{name} should not touch config/models");
        }
    }

    #[test]
    fn observes_commands_file() {
        let p = observe("commands.txt");
        assert!(p.dict, "commands.txt should set dict");
    }

    #[test]
    fn observes_words_files_for_both_models_and_dict() {
        // `words.{lang}.txt` feeds both the language model AND the dict's recognized set,
        // so a change must rebuild both — otherwise newly recognized service names work in
        // the n-gram but not in the exact-match recognized path.
        for name in ["words.ru.txt", "words.en.txt"] {
            let p = observe(name);
            assert!(p.dict, "{name} should set dict");
            assert!(p.models, "{name} should set models");
        }
    }

    #[test]
    fn observes_config_and_fst() {
        assert!(observe("config.toml").config);
        assert!(observe("russian.fst").models);
    }

    #[test]
    fn ignores_unrelated_files() {
        // Editor swap-files, backup tildes, dotfiles, random binaries — none of these are
        // subsystems we own, so they must not trigger a reload (which would burn the n-gram
        // rebuild and dict re-read for nothing).
        for name in ["config.toml.swp", "config.toml~", ".#config.toml", "random.bin"] {
            let p = observe(name);
            assert!(
                !p.config && !p.dict && !p.models,
                "{name} should not arm any reload"
            );
        }
    }
}

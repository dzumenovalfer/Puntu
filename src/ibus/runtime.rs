//! Boot sequence + main loop for the IBus front-end.
//!
//! 1. Read the IBus private bus address (via the `librush::ibus::get_ibus_addr` helper,
//!    which queries `ibus address` or `$IBUS_ADDRESS`).
//! 2. Build the shared [`Detector`] + [`UserDict`] from disk.
//! 3. Hand both to a [`PuntuFactory`] and register that factory with IBus via
//!    `librush::ibus::IBus::new`.
//! 4. Idle until SIGINT/SIGTERM — `librush` keeps the DBus loop running on the connection
//!    it created for us.

use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::signal;
use tokio::sync::Mutex as AsyncMutex;
use tracing::info;

use crate::config;
use crate::detect::userdict::UserDict;
use crate::detect::{Detector, Models};
use crate::ibus::engine::{
    DetectorSlot, EngineOptions, HotkeyBindings, PuntuEngine, PuntuFactory,
};

/// Our DBus well-known name — also the component name in the registry XML.
pub const BUS_NAME: &str = "org.freedesktop.IBus.Puntu";
/// Engine identifier — what GNOME shows in the input switcher and what `IBus` passes to
/// `CreateEngine`.
pub const ENGINE_NAME: &str = "puntu";

pub async fn run() -> Result<()> {
    let addr = librush::ibus::get_ibus_addr()
        .map_err(|e| anyhow::anyhow!("could not get IBus address: {e}"))?;
    info!("connecting to IBus at {addr}");

    let dir = config::config_dir();
    let models = Models::load(&dir);
    let dict = UserDict::load(dir.clone()).unwrap_or_else(|e| {
        tracing::warn!("could not load user dictionaries: {e}; using empty");
        UserDict::empty(dir)
    });
    let cfg = config::Config::load().unwrap_or_default();
    let detector = Detector::new(models, cfg.detect.clone());
    let hotkeys = HotkeyBindings::from_config(&cfg);
    let opts = EngineOptions::from_config(&cfg);
    info!(
        "hotkeys: undo={:?} mode_toggle={:?} convert_last={:?} taps_enabled={} autocorrect={} \
         hold_commit_ms={}",
        cfg.ibus_hotkeys.undo_key,
        cfg.ibus_hotkeys.mode_toggle,
        cfg.ibus_hotkeys.convert_last,
        cfg.enable_modifier_taps,
        opts.autocorrect,
        cfg.hold_commit_ms,
    );
    // Logged on its own line because it is the first thing to check when the engine "does
    // nothing" in one app but works everywhere else.
    info!(
        "client policy: require_preedit_capability={} passthrough_clients={:?} \
         passthrough_xim={}",
        opts.clients.require_preedit_capability,
        opts.clients.passthrough_clients,
        opts.clients.passthrough_xim,
    );

    // Share the dict between the engines and a hot-reload watcher, so `puntu dict add/learn`
    // (and hand-edits of the list files) take effect within ~300 ms — no engine restart.
    // The uinput daemon always had this; the IBus engine loading the dict once at startup is
    // why "puntu dict add … did nothing" until now.
    let dict = Arc::new(AsyncMutex::new(dict));
    // Same for the detector: `words.{ru,en}.txt` feed the language models (and `russian.fst`
    // is the big dictionary), so teaching a word has to rebuild those too — not just the
    // dict's recognized set. Without this the engine kept the models it booted with.
    let detector: DetectorSlot = Arc::new(std::sync::RwLock::new(Arc::new(detector)));
    // Tray pause flag: initial state from the marker file, then live via the watcher.
    let paused = Arc::new(std::sync::atomic::AtomicBool::new(paused_path().exists()));
    spawn_dict_reload_watcher(
        Arc::clone(&dict),
        Arc::clone(&detector),
        cfg.detect.clone(),
        Arc::clone(&paused),
    );

    let factory = PuntuFactory::new(
        Arc::clone(&detector),
        dict,
        hotkeys,
        opts,
        Arc::clone(&paused),
    );

    let _ibus = librush::ibus::IBus::<PuntuEngine, PuntuFactory>::new(
        addr,
        factory,
        BUS_NAME.to_string(),
    )
    .await
    .map_err(|e| anyhow::anyhow!("registering with IBus failed: {e}"))?;
    info!("registered IBus factory as {BUS_NAME}; engine ready");

    // Block until SIGINT/SIGTERM. The DBus connection inside `_ibus` keeps serving on its
    // own task; we just need to stay alive.
    tokio::select! {
        _ = signal::ctrl_c() => info!("SIGINT received, shutting down"),
        _ = sigterm() => info!("SIGTERM received, shutting down"),
    }

    Ok(())
}

/// Spawn the dictionary hot-reload watcher on its own OS thread. `notify` delivers events on
/// a std channel and the reload takes the dict mutex with `blocking_lock`, so this must live
/// outside the tokio runtime.
fn paused_path() -> std::path::PathBuf {
    config::config_dir().join("paused")
}

fn spawn_dict_reload_watcher(
    dict: Arc<AsyncMutex<UserDict>>,
    detector: DetectorSlot,
    detect_cfg: crate::config::DetectConfig,
    paused: Arc<std::sync::atomic::AtomicBool>,
) {
    if let Err(e) = std::thread::Builder::new()
        .name("puntu-dict-reload".into())
        .spawn(move || dict_reload_watcher(dict, detector, detect_cfg, paused))
    {
        tracing::warn!("dictionary hot-reload disabled (thread spawn failed): {e}");
    }
}

/// Watch `~/.config/puntu` and re-read the user word lists when they change. Events are
/// debounced (300 ms trailing edge) — `notify` emits several events per logical save — and
/// filtered to the dict files, so writes to `config.toml`, `russian.fst` or the control
/// socket don't trigger pointless reloads. Same approach as the uinput daemon's
/// `reload_watcher` (`input/mod.rs`), minus the config/models parts the engine reads at boot.
fn dict_reload_watcher(
    dict: Arc<AsyncMutex<UserDict>>,
    detector: DetectorSlot,
    detect_cfg: crate::config::DetectConfig,
    paused: Arc<std::sync::atomic::AtomicBool>,
) {
    use notify::{RecursiveMode, Watcher};
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::{Duration, Instant};

    const DEBOUNCE: Duration = Duration::from_millis(300);

    /// The files `UserDict::reload` reads (see `ListKind::file_name`).
    fn is_dict_file(p: &std::path::Path) -> bool {
        matches!(
            p.file_name().and_then(|n| n.to_str()),
            Some(
                "manual.ru.txt"
                    | "manual.en.txt"
                    | "learned.ru.txt"
                    | "learned.en.txt"
                    | "force.ru.txt"
                    | "force.en.txt"
                    | "words.ru.txt"
                    | "words.en.txt"
                    | "commands.txt"
            )
        )
    }

    /// The files `Models::load` reads. `words.{ru,en}.txt` are in BOTH lists: they extend the
    /// dict's recognized set *and* train the language models, so a taught word has to rebuild
    /// both — that is what the uinput daemon's watcher has always done (`input/mod.rs`).
    /// `russian.fst` was not watched at all, so `puntu build-dict` needed an engine restart.
    fn is_model_file(p: &std::path::Path) -> bool {
        matches!(
            p.file_name().and_then(|n| n.to_str()),
            Some("words.ru.txt" | "words.en.txt" | "russian.fst")
        )
    }

    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = match notify::recommended_watcher(tx) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("dictionary hot-reload disabled: {e}");
            return;
        }
    };
    let dir = config::config_dir();
    if let Err(e) = watcher.watch(&dir, RecursiveMode::NonRecursive) {
        tracing::warn!("dictionary hot-reload disabled (cannot watch {}): {e}", dir.display());
        return;
    }
    info!("watching {} for dictionary edits", dir.display());

    let mut dirty = false;
    let mut models_dirty = false;
    let mut deadline: Option<Instant> = None;
    loop {
        let timeout = match deadline {
            Some(d) => d.saturating_duration_since(Instant::now()),
            None => Duration::from_secs(3600),
        };
        match rx.recv_timeout(timeout) {
            Ok(Ok(ev)) => {
                // The tray's pause marker takes effect immediately — no debounce.
                if ev.paths.iter().any(|p| {
                    p.file_name().and_then(|n| n.to_str()) == Some("paused")
                }) {
                    let now = paused_path().exists();
                    paused.store(now, std::sync::atomic::Ordering::Relaxed);
                    info!("pause marker changed: paused = {now}");
                }
                if ev.paths.iter().any(|p| is_dict_file(p)) {
                    dirty = true;
                }
                if ev.paths.iter().any(|p| is_model_file(p)) {
                    models_dirty = true;
                }
                if dirty || models_dirty {
                    deadline = Some(Instant::now() + DEBOUNCE);
                }
            }
            Ok(Err(e)) => tracing::debug!("notify error: {e}"),
            Err(RecvTimeoutError::Timeout) => {
                if dirty {
                    match dict.blocking_lock().reload() {
                        Ok(()) => info!("user dictionaries reloaded"),
                        Err(e) => tracing::warn!("dictionary reload failed: {e}"),
                    }
                    dirty = false;
                }
                if models_dirty {
                    // Train + read the FST OUTSIDE the lock: this takes hundreds of
                    // milliseconds, and engines only ever hold the lock long enough to clone
                    // the `Arc` out of it.
                    let rebuilt =
                        Arc::new(Detector::new(Models::load(&dir), detect_cfg.clone()));
                    match detector.write() {
                        Ok(mut slot) => {
                            *slot = rebuilt;
                            info!("language models rebuilt");
                        }
                        Err(e) => tracing::warn!("could not swap the detector: {e}"),
                    }
                    models_dirty = false;
                }
                deadline = None;
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

async fn sigterm() {
    #[cfg(unix)]
    {
        if let Ok(mut s) = signal::unix::signal(signal::unix::SignalKind::terminate()) {
            s.recv().await;
        } else {
            std::future::pending::<()>().await;
        }
    }
    #[cfg(not(unix))]
    std::future::pending::<()>().await;
}

/// Generate the component XML the IBus daemon reads from
/// `~/.local/share/ibus/component/puntu.xml` (or system-wide) at startup. This is what makes
/// our engine discoverable in GNOME's input switcher.
///
/// (A `<setup>` element briefly hooked `puntu-app` into GNOME Settings → Клавиатура in
/// v0.6.2; removed at the user's request — the tray and the app-menu entry cover it.)
pub fn component_xml(exec_path: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<!-- Puntu IBus engine. Reinstall by re-running `install.sh`. -->
<component>
    <name>{BUS_NAME}</name>
    <description>Puntu Auto Layout Corrector (Russian ↔ English)</description>
    <exec>{exec_path}</exec>
    <version>{version}</version>
    <author>Puntu Contributors</author>
    <license>MIT</license>
    <textdomain>puntu</textdomain>
    <engines>
        <engine>
            <name>{ENGINE_NAME}</name>
            <language>en</language>
            <license>MIT</license>
            <author>Puntu Contributors</author>
            <layout>us</layout>
            <longname>Puntu (RU/EN auto-correct)</longname>
            <description>Automatic wrong-layout correction for Russian ↔ English</description>
            <icon>input-keyboard-symbolic</icon>
            <rank>99</rank>
            <symbol>P</symbol>
        </engine>
    </engines>
</component>
"#
    )
}

/// Write `puntu.xml` to the user-local IBus component directory and remind the caller to run
/// `ibus restart`. Used by the `install` helper subcommand of `puntu-ibus`.
pub fn install_component_xml(exec_path: &str) -> Result<std::path::PathBuf> {
    let dir = dirs_local_ibus_components()?;
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("puntu.xml");
    let xml = component_xml(exec_path);
    std::fs::write(&path, xml).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn dirs_local_ibus_components() -> Result<std::path::PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".local").join("share"))
        })
        .context("neither XDG_DATA_HOME nor HOME is set")?;
    Ok(data_home.join("ibus").join("component"))
}

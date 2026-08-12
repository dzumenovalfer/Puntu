//! `puntu-ibus` — IBus engine front-end (M5).
//!
//! Usage:
//!   puntu-ibus install      Drop the component XML into ~/.local/share/ibus/component/
//!                           and print the manual sudo step needed for system install.
//!   puntu-ibus enable       Switch IBus to the puntu engine (the active input source).
//!   puntu-ibus disable      Switch IBus back to the standard US layout (xkb:us::eng).
//!   puntu-ibus status       Show the current IBus engine.
//!   puntu-ibus doctor       Check the whole chain: session, IBus, IM env, helpers, config.
//!   puntu-ibus              Run as IBus engine (what IBus calls via the <exec> field).

use std::process::{Command, ExitCode};

use anyhow::Result;
use tracing_subscriber::{fmt, EnvFilter};

const DEFAULT_FALLBACK_ENGINE: &str = "xkb:us::eng";

fn main() -> ExitCode {
    init_logging();

    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str);

    let result: Result<()> = match mode {
        Some("install") => cmd_install(),
        Some("enable") => cmd_set_engine(puntu::ibus::runtime::ENGINE_NAME),
        Some("disable") => {
            // Allow override via env var so users with a non-US fallback can choose.
            let fallback = std::env::var("PUNTU_FALLBACK_ENGINE")
                .unwrap_or_else(|_| DEFAULT_FALLBACK_ENGINE.to_string());
            cmd_set_engine(&fallback)
        }
        Some("status") => cmd_status(),
        Some("doctor") => cmd_doctor(),
        Some("--help") | Some("-h") | Some("help") => {
            print_help();
            Ok(())
        }
        Some(other) => Err(anyhow::anyhow!(
            "unknown subcommand {other:?} — see `puntu-ibus --help`"
        )),
        None => cmd_run_engine(),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn cmd_install() -> Result<()> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let path = puntu::ibus::runtime::install_component_xml(&exe)?;
    println!("Wrote {}", path.display());
    println!();
    println!("Next steps (manual, one-time):");
    println!("  # IBus only reads /usr/share/ibus/component/, not the user-local dir, so:");
    println!("  sudo cp {} /usr/share/ibus/component/puntu.xml", path.display());
    println!("  ibus restart");
    println!();
    println!("Then enable the engine:");
    println!("  puntu-ibus enable");
    println!();
    println!("And switch back to the regular layout with:");
    println!("  puntu-ibus disable");
    Ok(())
}

/// Tell ibus-daemon to switch the global engine by shelling out to `ibus engine <name>`
/// (the `ibus` CLI ships with the ibus package we depend on anyway).
fn cmd_set_engine(name: &str) -> Result<()> {
    let out = Command::new("ibus").args(["engine", name]).output()?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("ibus engine {name} failed: {}", stderr.trim());
    }
    // Confirm it stuck.
    let now = current_engine().unwrap_or_else(|_| "<unknown>".to_string());
    println!("active engine: {now}");
    if now != name && !now.is_empty() {
        eprintln!(
            "warning: requested {name:?} but ibus reports {now:?}. Try `ibus restart` if puntu \
             isn't registered yet, or check `ibus list-engine | grep puntu`."
        );
    }
    Ok(())
}

fn cmd_status() -> Result<()> {
    let now = current_engine()?;
    println!("active engine: {now}");
    // Show whether our component is registered + visible to ibus.
    let listed = Command::new("ibus").arg("list-engine").output()?;
    let listed_out = String::from_utf8_lossy(&listed.stdout);
    let registered = listed_out
        .lines()
        .any(|line| line.contains(puntu::ibus::runtime::ENGINE_NAME));
    println!(
        "puntu engine registered: {}",
        if registered { "yes" } else { "no" }
    );
    // And whether our service process is running.
    let ps = Command::new("pgrep").args(["-af", "puntu-ibus"]).output();
    if let Ok(ps) = ps {
        let running = String::from_utf8_lossy(&ps.stdout);
        let count = running.lines().filter(|l| !l.contains("status")).count();
        println!("puntu-ibus processes running: {count}");
    }
    Ok(())
}

fn current_engine() -> Result<String> {
    let out = Command::new("ibus").arg("engine").output()?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// One-shot check of everything between a key press and a correction. Answers the two reports
/// that a log alone can't: "Puntu just does nothing here" (which link in the chain is missing)
/// and "it doesn't work under Hyprland" (outside GNOME nothing sets the input-method
/// environment or starts ibus-daemon for you).
///
/// Read-only: it inspects and prints, never changes anything.
fn cmd_doctor() -> Result<()> {
    let mut problems: Vec<String> = Vec::new();
    let desktop = env_or_unset("XDG_CURRENT_DESKTOP");
    let gnome = desktop.to_lowercase().contains("gnome");

    println!("== Session ==");
    row("XDG_SESSION_TYPE", &env_or_unset("XDG_SESSION_TYPE"));
    row("XDG_CURRENT_DESKTOP", &desktop);
    row("desktop family", if gnome { "GNOME" } else { "not GNOME" });

    println!();
    println!("== IBus ==");
    let daemon = process_running("ibus-daemon");
    row("ibus-daemon", if daemon { "running" } else { "NOT RUNNING" });
    if !daemon {
        problems.push(
            "ibus-daemon is not running. GNOME starts it for you; elsewhere add \
             `ibus-daemon -drxR` to your session autostart (Hyprland: exec-once)."
                .to_string(),
        );
    }
    let user_xml = dirs_component_xml();
    let system_xml = std::path::PathBuf::from("/usr/share/ibus/component/puntu.xml");
    row(
        "component (user)",
        &match &user_xml {
            Some(p) if p.exists() => p.display().to_string(),
            Some(p) => format!("missing ({})", p.display()),
            None => "unknown (no HOME/XDG_DATA_HOME)".to_string(),
        },
    );
    row(
        "component (system)",
        &if system_xml.exists() {
            system_xml.display().to_string()
        } else {
            "missing".to_string()
        },
    );
    if !system_xml.exists() && !user_xml.as_ref().is_some_and(|p| p.exists()) {
        problems.push(
            "The component XML is nowhere to be found — run `puntu-ibus install`, then \
             `ibus restart`."
                .to_string(),
        );
    }
    let listed = Command::new("ibus")
        .arg("list-engine")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let registered = listed.contains(puntu::ibus::runtime::ENGINE_NAME);
    row("engine registered", if registered { "yes" } else { "NO" });
    if !registered {
        problems.push(
            "IBus doesn't list the puntu engine. Install the component and run `ibus restart`."
                .to_string(),
        );
    }
    let active = current_engine().unwrap_or_else(|_| "<ibus not answering>".to_string());
    row("active engine", &active);
    if active != puntu::ibus::runtime::ENGINE_NAME {
        problems.push(format!(
            "The active engine is {active:?}, not \"puntu\" — switch with `puntu-ibus enable`."
        ));
    }
    // Outside GNOME this is what makes the engine come back after a restart: GNOME keeps its
    // own input-source list, everyone else relies on IBus's own preload list.
    let preload = gsettings_get("org.freedesktop.ibus.general", "preload-engines");
    row("preload-engines", preload.as_deref().unwrap_or("(unreadable)"));
    if !gnome && !preload.as_deref().unwrap_or("").contains("puntu") {
        problems.push(
            "Outside GNOME, IBus loads what's in `preload-engines`. Add puntu with: \
             gsettings set org.freedesktop.ibus.general preload-engines \"['puntu']\""
                .to_string(),
        );
    }
    row("puntu-ibus processes", &engine_process_count().to_string());

    println!();
    println!("== Input-method environment ==");
    // GNOME exports these itself; every other session has to, and a missing XMODIFIERS is the
    // single most common reason an engine "works in some apps only".
    let gtk = env_or_unset("GTK_IM_MODULE");
    let qt = env_or_unset("QT_IM_MODULE");
    let xmod = env_or_unset("XMODIFIERS");
    row("GTK_IM_MODULE", &gtk);
    row("QT_IM_MODULE", &qt);
    row("XMODIFIERS", &xmod);
    if !gnome {
        // On GNOME, Mutter drives IBus through the compositor and these are set for you. Off
        // GNOME they are the whole connection: IBus has no input-method-v2 implementation, so
        // apps reach it through these client-side IM modules, not through text-input-v3.
        if !gtk.contains("ibus") {
            problems.push("GTK apps need GTK_IM_MODULE=ibus.".to_string());
        }
        if !qt.contains("ibus") {
            problems.push("Qt apps need QT_IM_MODULE=ibus.".to_string());
        }
        if !xmod.contains("ibus") {
            problems.push(
                "XMODIFIERS doesn't point at ibus — X11/XWayland apps will never reach the \
                 engine. Set XMODIFIERS=@im=ibus in your session."
                    .to_string(),
            );
        }
    }

    println!();
    println!("== Helpers ==");
    for (cmd, why) in [
        ("wl-paste", "reading the mouse selection (Ctrl+Alt+S)"),
        ("notify-send", "visible confirmation when a word is learned"),
        ("zenity", "the \"remember this word?\" dialog"),
    ] {
        match which(cmd) {
            Some(p) => row(cmd, &p.display().to_string()),
            None => {
                row(cmd, "missing");
                problems.push(format!("`{cmd}` is not installed — {why} won't work."));
            }
        }
    }

    println!();
    println!("== Puntu ==");
    let cfg_path = puntu::config::Config::path();
    row("config", &cfg_path.display().to_string());
    let cfg = puntu::config::Config::load().unwrap_or_default();
    let paused = puntu::config::config_dir().join("paused").exists();
    row("paused (tray)", if paused { "YES — engine is off" } else { "no" });
    if paused {
        problems.push(
            "Puntu is paused (the tray marker file exists) — that alone stops every \
             correction."
                .to_string(),
        );
    }
    row("autocorrect", if cfg.dry_run { "off (dry_run)" } else { "on" });
    row(
        "require_preedit_capability",
        &cfg.ibus_clients.require_preedit_capability.to_string(),
    );
    row("passthrough_clients", &format!("{:?}", cfg.ibus_clients.passthrough_clients));
    row("passthrough_xim", &cfg.ibus_clients.passthrough_xim.to_string());
    row(
        "engine log",
        &log_path().map(|p| p.display().to_string()).unwrap_or_else(|| "(stderr)".into()),
    );

    println!();
    if problems.is_empty() {
        println!("No problems found.");
        println!(
            "If typing still misbehaves in one app, look for its `client=…` line in the \
             engine log and add that name to [ibus_clients] passthrough_clients."
        );
    } else {
        println!("Problems found ({}):", problems.len());
        for (i, p) in problems.iter().enumerate() {
            println!("  {}. {p}", i + 1);
        }
    }
    Ok(())
}

/// Print one `label  value` row, aligned.
fn row(label: &str, value: &str) {
    println!("  {label:<26} {value}");
}

/// An environment variable's value, or `(unset)`. An empty value counts as unset — that is
/// what `env = GTK_IM_MODULE,` in a compositor config leaves behind, and it is just as broken.
fn env_or_unset(key: &str) -> String {
    match std::env::var(key) {
        Ok(v) if !v.is_empty() => v,
        _ => "(unset)".to_string(),
    }
}

/// Is a process with this exact name running? `pgrep -x` rather than a substring match, so
/// `ibus-daemon` isn't confused with `ibus-x11` or our own command line.
fn process_running(name: &str) -> bool {
    Command::new("pgrep")
        .args(["-x", name])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// How many engine processes are alive. More than one means two engines fight over the same
/// keystrokes; zero means ibus-daemon hasn't launched ours (or it crashed).
fn engine_process_count() -> usize {
    Command::new("pgrep")
        .args(["-af", "puntu-ibus"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.contains("doctor") && !l.contains("status"))
                .count()
        })
        .unwrap_or(0)
}

fn gsettings_get(schema: &str, key: &str) -> Option<String> {
    let out = Command::new("gsettings").args(["get", schema, key]).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Resolve `cmd` on `PATH` without shelling out (`which` itself isn't always installed).
fn which(cmd: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(cmd))
            .find(|p| p.is_file())
    })
}

fn dirs_component_xml() -> Option<std::path::PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".local").join("share"))
        })?;
    Some(data_home.join("ibus").join("component").join("puntu.xml"))
}

fn cmd_run_engine() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()?;
    runtime.block_on(puntu::ibus::run())
}

fn print_help() {
    println!("puntu-ibus — IBus engine front-end for Puntu");
    println!();
    println!("USAGE:");
    println!("    puntu-ibus              Run as IBus engine (called by ibus-daemon)");
    println!("    puntu-ibus install      Print the steps to install the component XML");
    println!("    puntu-ibus enable       Switch IBus to the puntu engine");
    println!("    puntu-ibus disable      Switch back to the regular layout (xkb:us::eng)");
    println!("    puntu-ibus status       Show current engine + registration state");
    println!("    puntu-ibus doctor       Full check: session, IBus, IM env, helpers, config");
    println!();
    println!("ENV:");
    println!("    PUNTU_FALLBACK_ENGINE   Engine to switch to on `disable` (default xkb:us::eng)");
    println!("    PUNTU_LOG               Tracing filter (e.g. `puntu=debug`)");
    println!();
    println!("LOG:");
    match log_path() {
        Some(p) => println!("    {}", p.display()),
        None => println!("    (stderr — neither XDG_STATE_HOME nor HOME is set)"),
    }
}

/// Where the engine's log goes. `$XDG_STATE_HOME/puntu/engine.log`, falling back to
/// `~/.local/state/puntu/engine.log`.
///
/// NOT a fixed path in `/tmp`: that is world-writable, so the second user on a machine got
/// EACCES and no logs at all, and an attacker could pre-create the name as a symlink and have
/// us truncate whatever it pointed at (the file is recreated on every start).
fn log_path() -> Option<std::path::PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".local").join("state"))
        })?
        .join("puntu");
    std::fs::create_dir_all(&state).ok()?;
    Some(state.join("engine.log"))
}

fn init_logging() {
    let filter = EnvFilter::try_from_env("PUNTU_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    // ibus-daemon swallows the engine's stderr, so diagnosing anything used to require a
    // wrapper script. Log to a file instead (truncated on every start — one session's worth):
    // it is the first thing to look at when something misbehaves.
    match log_path().and_then(|p| std::fs::File::create(p).ok()) {
        Some(file) => {
            fmt()
                .with_env_filter(filter)
                .with_target(false)
                .with_ansi(false)
                .with_writer(std::sync::Mutex::new(file))
                .init();
        }
        None => fmt().with_env_filter(filter).with_target(false).init(),
    }
}

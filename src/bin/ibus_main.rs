//! `puntu-ibus` — IBus engine front-end (M5).
//!
//! Usage:
//!   puntu-ibus install      Drop the component XML into ~/.local/share/ibus/component/
//!                           and print the manual sudo step needed for system install.
//!   puntu-ibus enable       Switch IBus to the puntu engine (the active input source).
//!   puntu-ibus disable      Switch IBus back to the standard US layout (xkb:us::eng).
//!   puntu-ibus status       Show the current IBus engine.
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
    println!();
    println!("ENV:");
    println!("    PUNTU_FALLBACK_ENGINE   Engine to switch to on `disable` (default xkb:us::eng)");
    println!("    PUNTU_LOG               Tracing filter (e.g. `puntu=debug`)");
}

fn init_logging() {
    let filter = EnvFilter::try_from_env("PUNTU_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

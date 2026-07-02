//! Wayland clipboard access via `wl-copy` / `wl-paste` (the `wl-clipboard` package).
//!
//! We can't make our uinput virtual keyboard render Cyrillic — GNOME/Mutter locks it to a
//! fixed layout — so corrections insert the target text by putting it on the clipboard and
//! pasting (Ctrl+V), which is layout-independent.
//!
//! All subprocess calls are wrapped in hard timeouts so the daemon can't deadlock if `wl-copy`
//! or `wl-paste` hangs (which happens on GNOME 50.1 under compositor load, fullscreen apps,
//! or shell restarts). Without these, a hung subprocess inside a grabbed correction would
//! freeze the user's keyboard until the daemon was killed manually. See `input::capture` for
//! the matching RAII `GrabGuard` that releases the keyboard on any error path.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// Max time we wait for `wl-copy` to consume our input before SIGKILL'ing it. A healthy run
/// is ~30ms; 600ms gives ample headroom while still releasing the grab quickly on a hang.
const COPY_TIMEOUT: Duration = Duration::from_millis(600);
/// Max time we wait for `wl-paste` to return the current selection.
const PASTE_TIMEOUT: Duration = Duration::from_millis(400);

/// Wait for `child` to exit; if it doesn't within `timeout`, SIGKILL it and return Err. Used
/// for subprocesses we only need to know finished (no stdout to collect).
fn wait_or_kill(child: &mut Child, timeout: Duration, what: &str) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match child
            .try_wait()
            .with_context(|| format!("try_wait {what}"))?
        {
            Some(status) if !status.success() => {
                anyhow::bail!("{what} exited unsuccessfully: {status}");
            }
            Some(_) => return Ok(()),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("{what} timed out after {timeout:?}");
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Spawn `cmd`, collect its stdout via a reader thread (so the pipe buffer can't backstall the
/// child), enforce a timeout on the whole thing. SIGKILLs the child on timeout. Used for
/// subprocesses we need stdout from (the clipboard contents).
fn capture_with_timeout(mut cmd: Command, timeout: Duration, what: &str) -> Result<Vec<u8>> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {what}"))?;
    let mut stdout = child
        .stdout
        .take()
        .with_context(|| format!("{what} stdout"))?;

    // Drain stdout from a worker thread so the kernel pipe buffer never fills and blocks the
    // child (which would defeat our timeout: `try_wait` keeps returning None forever).
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let result = stdout.read_to_end(&mut buf);
        let _ = tx.send((result, buf));
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child
            .try_wait()
            .with_context(|| format!("try_wait {what}"))?
        {
            Some(status) => {
                if !status.success() {
                    anyhow::bail!("{what} exited unsuccessfully: {status}");
                }
                // Reader thread finishes shortly after EOF.
                let (read_result, buf) = rx
                    .recv_timeout(Duration::from_millis(100))
                    .with_context(|| format!("{what} reader thread"))?;
                read_result.with_context(|| format!("reading {what} stdout"))?;
                return Ok(buf);
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!("{what} timed out after {timeout:?}");
                }
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

/// Put `text` on the clipboard. `wl-copy` reads stdin, forks a server to serve the selection,
/// and the foreground process exits — so this returns promptly under normal conditions. Bounded
/// by [`COPY_TIMEOUT`] so a stalled compositor can't deadlock the daemon mid-correction.
pub fn set(text: &str) -> Result<()> {
    let mut child = Command::new("wl-copy")
        .arg("--type")
        .arg("text/plain;charset=utf-8")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("running wl-copy (install the `wl-clipboard` package)")?;
    child
        .stdin
        .take()
        .context("wl-copy stdin")?
        .write_all(text.as_bytes())
        .context("writing to wl-copy")?;
    wait_or_kill(&mut child, COPY_TIMEOUT, "wl-copy")?;
    Ok(())
}

/// Read the current clipboard (best-effort, used to save/restore around a correction). Bounded
/// by [`PASTE_TIMEOUT`].
pub fn get() -> Result<String> {
    let mut cmd = Command::new("wl-paste");
    cmd.arg("--no-newline");
    let buf = capture_with_timeout(cmd, PASTE_TIMEOUT, "wl-paste")?;
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// Read the current PRIMARY selection (highlighted text) — used by the convert-selection hotkey.
pub fn get_primary() -> Result<String> {
    let mut cmd = Command::new("wl-paste");
    cmd.arg("--primary").arg("--no-newline");
    let buf = capture_with_timeout(cmd, PASTE_TIMEOUT, "wl-paste --primary")?;
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// Whether `wl-copy` is on PATH.
pub fn available() -> bool {
    which("wl-copy")
}

fn which(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| Path::new(&dir).join(bin).is_file())
        })
        .unwrap_or(false)
}

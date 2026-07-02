//! Tiny line-based control socket so the CLI (and a future GUI) can pause/resume/query the
//! daemon. Dictionary edits go straight to the files and are picked up by hot-reload, so they
//! don't need the socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Result};

/// Cap on every socket read/write, both sides. The protocol is one line each way, so a healthy
/// exchange takes microseconds; the cap only exists so a wedged peer can't hang the other side
/// forever (a stuck daemon hanging the CLI, or a silent client blocking the daemon's single
/// accept loop).
const IO_TIMEOUT: Duration = Duration::from_secs(2);

#[cfg(feature = "daemon")]
use std::os::unix::net::UnixListener;
#[cfg(feature = "daemon")]
use std::path::PathBuf;
#[cfg(feature = "daemon")]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "daemon")]
use std::sync::Arc;

#[cfg(feature = "daemon")]
use anyhow::Context;

/// State the IPC server can observe/mutate.
#[cfg(feature = "daemon")]
#[derive(Clone)]
pub struct Control {
    pub paused: Arc<AtomicBool>,
}

/// Run the control server. Blocks; intended to run on its own thread.
#[cfg(feature = "daemon")]
pub fn serve(socket: PathBuf, control: Control) -> Result<()> {
    // Stale socket from a previous run would block bind().
    let _ = std::fs::remove_file(&socket);
    if let Some(parent) = socket.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let listener = UnixListener::bind(&socket)
        .with_context(|| format!("binding control socket {}", socket.display()))?;
    // Owner-only: the socket may land in a shared directory (the /tmp fallback when there's
    // no XDG runtime dir), where default permissions would let any local user pause the daemon.
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(e) =
            std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
        {
            tracing::warn!("could not restrict socket permissions: {e}");
        }
    }
    tracing::info!("control socket listening at {}", socket.display());

    for stream in listener.incoming() {
        match stream {
            Ok(s) => {
                if let Err(e) = handle(s, &control) {
                    tracing::warn!("ipc client error: {e}");
                }
            }
            Err(e) => tracing::warn!("ipc accept error: {e}"),
        }
    }
    Ok(())
}

#[cfg(feature = "daemon")]
fn handle(stream: UnixStream, control: &Control) -> Result<()> {
    // The accept loop is single-threaded: without a read timeout, one client that connects
    // and never sends a line would block the control socket for everyone, forever.
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let resp = match line.trim() {
        "pause" => {
            control.paused.store(true, Ordering::SeqCst);
            "ok: paused".to_string()
        }
        "resume" => {
            control.paused.store(false, Ordering::SeqCst);
            "ok: resumed".to_string()
        }
        "status" => {
            let p = control.paused.load(Ordering::SeqCst);
            format!("ok: {}", if p { "paused" } else { "active" })
        }
        other => format!("err: unknown command {other:?}"),
    };
    let mut w = stream;
    writeln!(w, "{resp}")?;
    Ok(())
}

/// Client: send one command to the daemon and return its reply.
pub fn send_command(socket: &Path, cmd: &str) -> Result<String> {
    let mut stream = UnixStream::connect(socket)
        .map_err(|e| anyhow!("connecting to daemon at {} ({e}); is it running?", socket.display()))?;
    // Bounded I/O so `puntu pause|resume|status` can't hang forever on a wedged daemon.
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    writeln!(stream, "{cmd}")?;
    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    reader.read_line(&mut resp)?;
    Ok(resp.trim().to_string())
}

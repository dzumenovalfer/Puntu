//! Puntu tray front-end (`puntu-gui`).
//!
//! A StatusNotifierItem (system-tray) indicator: shows whether the daemon is active or paused,
//! lets you toggle it, and quits. It is a thin client over the daemon's control socket — the
//! same `pause`/`resume`/`status` commands the CLI uses — so anything the tray does is also
//! doable from the terminal (`puntu pause|resume|status`) and vice versa.
//!
//! Built only with `--features gui`. It deliberately avoids the evdev/uinput (`daemon`) deps:
//! it talks to the already-running daemon, it does not capture input itself.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use ksni::{Status, ToolTip, Tray, TrayMethods};

/// What we last learned about the daemon over the control socket.
#[derive(Clone, Copy, PartialEq, Eq)]
enum DaemonState {
    Active,
    Paused,
    /// Socket missing/unreachable — the daemon isn't running.
    Down,
}

/// Query the daemon's status over the control socket. Never panics: a missing socket or any
/// error maps to `Down` so the tray degrades gracefully.
fn query(socket: &Path) -> DaemonState {
    match puntu::ipc::send_command(socket, "status") {
        Ok(resp) if resp.contains("paused") => DaemonState::Paused,
        Ok(resp) if resp.contains("active") => DaemonState::Active,
        _ => DaemonState::Down,
    }
}

struct PuntuTray {
    socket: PathBuf,
    state: DaemonState,
}

impl PuntuTray {
    /// Flip pause/resume on the daemon, then optimistically reflect the new state (the poll loop
    /// will reconcile if the command actually failed).
    fn toggle(&mut self) {
        let cmd = match self.state {
            DaemonState::Active => "pause",
            DaemonState::Paused => "resume",
            DaemonState::Down => return,
        };
        match puntu::ipc::send_command(&self.socket, cmd) {
            Ok(_) => {
                self.state = match self.state {
                    DaemonState::Active => DaemonState::Paused,
                    DaemonState::Paused => DaemonState::Active,
                    DaemonState::Down => DaemonState::Down,
                }
            }
            Err(_) => self.state = DaemonState::Down,
        }
    }
}

impl Tray for PuntuTray {
    fn id(&self) -> String {
        "puntu".into()
    }

    fn title(&self) -> String {
        "Puntu".into()
    }

    fn icon_name(&self) -> String {
        match self.state {
            DaemonState::Active => "input-keyboard-symbolic".into(),
            DaemonState::Paused => "changes-prevent-symbolic".into(),
            DaemonState::Down => "action-unavailable-symbolic".into(),
        }
    }

    fn status(&self) -> Status {
        match self.state {
            DaemonState::Active => Status::Active,
            DaemonState::Paused | DaemonState::Down => Status::Passive,
        }
    }

    fn tool_tip(&self) -> ToolTip {
        let description = match self.state {
            DaemonState::Active => "Автокоррекция раскладки включена",
            DaemonState::Paused => "Автокоррекция на паузе",
            DaemonState::Down => "Демон не запущен",
        };
        ToolTip {
            title: "Puntu".into(),
            description: description.into(),
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
        }
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let running = self.state != DaemonState::Down;
        let header = match self.state {
            DaemonState::Active => "Статус: активна",
            DaemonState::Paused => "Статус: пауза",
            DaemonState::Down => "Статус: демон не запущен",
        };
        vec![
            StandardItem {
                label: header.into(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Активна".into(),
                checked: self.state == DaemonState::Active,
                enabled: running,
                activate: Box::new(|this: &mut Self| this.toggle()),
                ..Default::default()
            }
            .into(),
            // Окно настроек — следующий этап; пункт зарезервирован и пока выключен.
            StandardItem {
                label: "Открыть настройки…".into(),
                icon_name: "preferences-system-symbolic".into(),
                enabled: false,
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Выход".into(),
                icon_name: "application-exit-symbolic".into(),
                activate: Box::new(|_| std::process::exit(0)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let socket = puntu::config::socket_path();
    let tray = PuntuTray {
        state: query(&socket),
        socket: socket.clone(),
    };

    let handle = tray
        .spawn()
        .await
        .map_err(|e| anyhow::anyhow!("could not register the tray icon (is a StatusNotifier host running?): {e}"))?;

    // Poll so the icon reflects changes made elsewhere (CLI `puntu pause`, a hotkey, the daemon
    // stopping). Cheap: one local socket round-trip every couple of seconds.
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let next = query(&socket);
        let updated = handle
            .update(move |t: &mut PuntuTray| {
                if t.state != next {
                    t.state = next;
                }
            })
            .await;
        // `update` returns None once the tray service has shut down — then so should we.
        if updated.is_none() {
            break;
        }
    }
    Ok(())
}

//! Puntu tray (`puntu-gui`) — индикатор и быстрые действия для IBus-движка.
//!
//! * левый клик / «Открыть настройки…» — запускает `puntu-app`;
//! * «Приостановить» — переключает файл-маркер `paused` в каталоге конфига; watcher движка
//!   поднимает общий флаг за доли секунды, и каждый keystroke проходит насквозь. Иконка
//!   показывает состояние (пауза / выключен / работает);
//! * «Выключить движок» — `puntu-ibus disable` (источник ввода возвращается на обычную
//!   раскладку); пункт превращается в «Включить движок».
//!
//! Собирается только с `--features gui`. Никаких прав и демонов не требует: файл-маркер,
//! `ibus engine` и запуск соседних бинарников.

use std::path::PathBuf;
use std::time::Duration;

use ksni::menu::{CheckmarkItem, MenuItem, StandardItem};
use ksni::{Status, ToolTip, Tray, TrayMethods};

fn paused_path() -> PathBuf {
    puntu::config::config_dir().join("paused")
}

fn is_paused() -> bool {
    paused_path().exists()
}

fn set_paused(p: bool) {
    if p {
        let _ = std::fs::write(paused_path(), b"paused by tray\n");
    } else {
        let _ = std::fs::remove_file(paused_path());
    }
}

/// A binary installed next to us, falling back to `$PATH`.
fn sibling(name: &str) -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from(name))
}

fn open_app() {
    let _ = std::process::Command::new(sibling("puntu-app")).spawn();
}

/// Is the Puntu engine the active IBus input source right now?
fn engine_active() -> bool {
    std::process::Command::new("ibus")
        .arg("engine")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "puntu")
        .unwrap_or(false)
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct State {
    paused: bool,
    engine_on: bool,
}

fn read_state() -> State {
    State { paused: is_paused(), engine_on: engine_active() }
}

struct PuntuTray {
    state: State,
}

impl Tray for PuntuTray {
    fn id(&self) -> String {
        "puntu".into()
    }

    fn title(&self) -> String {
        "Puntu".into()
    }

    fn icon_name(&self) -> String {
        if !self.state.engine_on {
            "action-unavailable-symbolic".into()
        } else if self.state.paused {
            "media-playback-pause-symbolic".into()
        } else {
            "input-keyboard-symbolic".into()
        }
    }

    fn status(&self) -> Status {
        Status::Active // always visible — this is the control point
    }

    fn tool_tip(&self) -> ToolTip {
        let description = if !self.state.engine_on {
            "Движок выключен"
        } else if self.state.paused {
            "Приостановлен"
        } else {
            "Работает"
        };
        ToolTip {
            title: "Puntu".into(),
            description: description.into(),
            icon_name: self.icon_name(),
            icon_pixmap: Vec::new(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        open_app();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let header = if !self.state.engine_on {
            "Статус: движок выключен"
        } else if self.state.paused {
            "Статус: приостановлен"
        } else {
            "Статус: работает"
        };
        let engine_on = self.state.engine_on;
        vec![
            StandardItem { label: header.into(), enabled: false, ..Default::default() }.into(),
            MenuItem::Separator,
            StandardItem {
                label: "Открыть настройки…".into(),
                icon_name: "preferences-system-symbolic".into(),
                activate: Box::new(|_| open_app()),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Приостановить".into(),
                checked: self.state.paused,
                enabled: engine_on,
                activate: Box::new(|t: &mut Self| {
                    t.state.paused = !t.state.paused;
                    set_paused(t.state.paused);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if engine_on {
                    "Выключить движок".into()
                } else {
                    "Включить движок".into()
                },
                icon_name: "system-shutdown-symbolic".into(),
                activate: Box::new(move |t: &mut Self| {
                    let cmd = if engine_on { "disable" } else { "enable" };
                    let _ = std::process::Command::new(sibling("puntu-ibus")).arg(cmd).status();
                    t.state.engine_on = !engine_on;
                }),
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
    let tray = PuntuTray { state: read_state() };
    let handle = tray.spawn().await.map_err(|e| {
        anyhow::anyhow!(
            "не удалось зарегистрировать значок в трее (нужно расширение AppIndicator): {e}"
        )
    })?;

    // Отражаем изменения, сделанные не из трея: пауза из приложения/CLI, смена источника
    // ввода через Super+Space. Дёшево: файл + один subprocess раз в 2 секунды.
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let next = read_state();
        let updated = handle
            .update(move |t: &mut PuntuTray| {
                t.state = next;
            })
            .await;
        // `update` возвращает None, когда сервис трея остановлен — тогда выходим и мы.
        if updated.is_none() {
            break;
        }
    }
    Ok(())
}

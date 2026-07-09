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
    if let Ok(mut child) = std::process::Command::new(sibling("puntu-app")).spawn() {
        // Дожинаем ребёнка в фоне: без wait() каждое закрытое окно настроек висело бы
        // зомби, пока жив трей, и pgrep-подсчёты экземпляров считали бы его живым.
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
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
        // Puntu's own symbolic icons, installed into the hicolor theme by install.sh
        // (~/.local/share/icons/hicolor/scalable/status/). The shell recolours them to the
        // panel foreground, so they read on both light and dark top bars.
        if !self.state.engine_on {
            "puntu-disabled-symbolic".into()
        } else if self.state.paused {
            "puntu-paused-symbolic".into()
        } else {
            "puntu-symbolic".into()
        }
    }

    fn icon_theme_path(&self) -> String {
        // The dir with our icons, verbatim. The AppIndicator host resolves names here
        // directly, so the icon shows up even when the shell started before install.sh
        // created ~/.local/share/icons and hasn't rescanned the theme yet.
        std::env::var("HOME")
            .map(|h| format!("{h}/.local/share/icons/hicolor/scalable/status"))
            .unwrap_or_default()
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
                // «Выключен» поглощает «на паузе»: у выключенного движка галочка паузы
                // не показывается, даже если маркер остался от внешних действий.
                checked: self.state.paused && engine_on,
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
                    // Любой щелчок тумблера снимает паузу: выключенный движок не может
                    // быть «на паузе», а включённый не должен молча стартовать замороженным.
                    t.state.paused = false;
                    set_paused(false);
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

/// Другой живой `puntu-gui` (кроме нас)? Автостарт логина и запуск из `puntu-app`
/// могут стартовать нас одновременно — дубликат обязан тихо выйти, иначе в панели
/// окажется два значка.
fn another_instance_running() -> bool {
    let me = std::process::id();
    std::process::Command::new("pgrep")
        // --runstates без Z: зомби — это не работающий экземпляр.
        .args(["-x", "--runstates", "R,S,D,T", "puntu-gui"])
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .any(|pid| pid != me)
        })
        .unwrap_or(false)
}

/// Регистрация в трее с повторами: при логине автостарт обгоняет расширение
/// AppIndicator (StatusNotifierWatcher ещё не поднят), и первая попытка проваливается —
/// раньше это молча оставляло сессию без значка состояния.
async fn register_with_retry() -> anyhow::Result<ksni::Handle<PuntuTray>> {
    let mut last = None;
    for _ in 0..60 {
        match (PuntuTray { state: read_state() }).spawn().await {
            Ok(handle) => return Ok(handle),
            Err(e) => last = Some(e),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    Err(anyhow::anyhow!(
        "не удалось зарегистрировать значок в трее (нужно расширение AppIndicator): {}",
        last.map(|e| e.to_string()).unwrap_or_default()
    ))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    if another_instance_running() {
        return Ok(());
    }

    loop {
        let handle = register_with_retry().await?;

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
            // `update` возвращает None, когда сервис трея остановлен.
            if updated.is_none() {
                break;
            }
        }
        // Сервис пропал — обычно перезапуск gnome-shell или расширения. Регистрируемся
        // заново, а не выходим: раньше любой рестарт шелла навсегда убирал значок.
    }
}

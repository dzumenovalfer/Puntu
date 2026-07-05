//! Puntu — единое окно настроек и словаря (`puntu-app`).
//!
//! * **Настройки** — тумблеры вкл/выкл, назначение клавиш нажатием, выбор тап-жестов.
//! * **Словарь** — пары «правильное слово / как набирается», поиск, добавление; действие
//!   каждого слова (переводить / не исправлять / всегда переводить) меняется на месте.
//!
//! Правки словаря движок подхватывает сам (hot-reload, ~секунда). Настройки движок читает
//! при старте — после изменений внизу появляется кнопка «Применить», и приложение честно
//! сообщает, перезапустился движок или нет.

use std::sync::mpsc;

use eframe::egui;
use puntu::config::{self, Config};
use puntu::detect::translit;
use puntu::detect::userdict::{ListKind, UserDict};
use puntu::keymap::Lang;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([680.0, 600.0])
            .with_min_inner_size([560.0, 440.0])
            .with_title("Puntu")
            // Frameless + transparent: we draw our own GNOME-style rounded window with a
            // headerbar (system decorations on Wayland are square and alien-looking).
            .with_decorations(false)
            .with_transparent(true)
            // Matches puntu.desktop, so the dock/alt-tab show the proper icon and name.
            .with_app_id("puntu"),
        ..Default::default()
    };
    eframe::run_native(
        "Puntu",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_zoom_factor(1.1);
            let (accent, dark) = read_system_theme();
            apply_adwaita_theme(&cc.egui_ctx, accent, dark);
            Ok(Box::new(App::new()))
        }),
    )
}

/// Read the system look from GNOME: (accent colour, dark?). Follows the «Внешний вид»
/// settings — accent-color ('blue', 'purple', …) and color-scheme ('prefer-dark').
fn read_system_theme() -> (egui::Color32, bool) {
    let get = |key: &str| {
        std::process::Command::new("gsettings")
            .args(["get", "org.gnome.desktop.interface", key])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().trim_matches('\'').to_string())
            .unwrap_or_default()
    };
    let dark = get("color-scheme").contains("dark");
    let c = |r: u8, g: u8, b: u8| egui::Color32::from_rgb(r, g, b);
    // The GNOME 47+ accent palette.
    let accent = match get("accent-color").as_str() {
        "teal" => c(0x21, 0x90, 0xa4),
        "green" => c(0x36, 0x88, 0x3c),
        "yellow" => c(0xc8, 0x88, 0x00),
        "orange" => c(0xed, 0x5b, 0x00),
        "red" => c(0xe6, 0x2d, 0x42),
        "pink" => c(0xd5, 0x61, 0x99),
        "purple" => c(0x91, 0x41, 0xac),
        "slate" => c(0x6f, 0x83, 0x96),
        _ => c(0x35, 0x84, 0xe4), // blue / unknown
    };
    (accent, dark)
}

/// Approximate the GNOME Adwaita look with the SYSTEM accent colour and light/dark scheme —
/// when the user changes the theme in «Внешний вид», the app follows (polled live).
fn apply_adwaita_theme(ctx: &egui::Context, accent: egui::Color32, dark: bool) {
    let theme = if dark { egui::Theme::Dark } else { egui::Theme::Light };
    ctx.set_theme(theme);
    let mut style = (*ctx.style_of(theme)).clone();
    let v = &mut style.visuals;
    if dark {
        v.panel_fill = egui::Color32::from_rgb(0x24, 0x24, 0x24); // Adwaita dark window
        v.window_fill = v.panel_fill;
        v.extreme_bg_color = egui::Color32::from_rgb(0x1e, 0x1e, 0x1e);
        v.faint_bg_color = egui::Color32::from_rgb(0x30, 0x30, 0x30); // card
        v.widgets.noninteractive.bg_fill = v.faint_bg_color;
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(0x3a, 0x3a, 0x3a);
        v.widgets.inactive.weak_bg_fill = v.widgets.inactive.bg_fill;
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0x45, 0x45, 0x45);
        v.widgets.hovered.weak_bg_fill = v.widgets.hovered.bg_fill;
    } else {
        v.panel_fill = egui::Color32::from_rgb(0xfa, 0xfa, 0xfa); // Adwaita light window
        v.window_fill = v.panel_fill;
        v.extreme_bg_color = egui::Color32::WHITE;
        v.faint_bg_color = egui::Color32::WHITE; // card
        v.widgets.noninteractive.bg_fill = v.faint_bg_color;
        v.widgets.inactive.bg_fill = egui::Color32::from_rgb(0xeb, 0xeb, 0xeb);
        v.widgets.inactive.weak_bg_fill = v.widgets.inactive.bg_fill;
        v.widgets.hovered.bg_fill = egui::Color32::from_rgb(0xdd, 0xdd, 0xdd);
        v.widgets.hovered.weak_bg_fill = v.widgets.hovered.bg_fill;
    }
    v.widgets.active.bg_fill = accent;
    v.selection.bg_fill = accent;
    v.selection.stroke = egui::Stroke::new(1.0, egui::Color32::WHITE);
    let round = |w: &mut egui::style::WidgetVisuals| w.corner_radius = 8.into();
    round(&mut v.widgets.noninteractive);
    round(&mut v.widgets.inactive);
    round(&mut v.widgets.hovered);
    round(&mut v.widgets.active);
    round(&mut v.widgets.open);
    style.spacing.item_spacing = egui::vec2(10.0, 8.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    ctx.set_style_of(theme, style);
}

/// The layout-switch options offered in the settings list, GNOME-Tweaks style.
const SWITCH_OPTIONS: &[(&str, &str)] = &[
    ("Ctrl", "тап Ctrl"),
    ("Ctrl+Shift", "тап Ctrl+Shift"),
    ("Alt+Shift", "тап Alt+Shift (как системное переключение)"),
    ("Ctrl+Alt", "тап Ctrl+Alt"),
    ("Super", "тап Super (Win)"),
];

#[derive(PartialEq, Clone, Copy)]
enum Tab {
    Settings,
    Dictionary,
}

/// Which hotkey field is currently waiting for a key press.
#[derive(PartialEq, Clone, Copy)]
enum Capture {
    Undo,
    ConvertSelection,
    Remember,
    ModeToggleKey,
}

/// One dictionary row, with everything needed to move the word between lists.
struct Row {
    /// The form stored in its list (typed form for exceptions/force, correct for recognized).
    stored: String,
    /// Правильное написание.
    correct: String,
    /// Как оно набирается не в той раскладке.
    typed: String,
    /// Language of `correct`.
    correct_lang: Lang,
    /// Language of `typed`.
    typed_lang: Lang,
    action: Action,
}

/// What happens to the word — the third dictionary column.
#[derive(PartialEq, Clone, Copy)]
enum Action {
    Convert,      // Recognized: кривая форма переводится в правильную
    Leave,        // Manual/Learned: никогда не исправлять
    ForceConvert, // Force: набранная форма всегда переводится
}

impl Action {
    fn label(self) -> &'static str {
        match self {
            Action::Convert => "переводить",
            Action::Leave => "не исправлять",
            Action::ForceConvert => "всегда переводить",
        }
    }
}

struct App {
    cfg: Config,
    dict: UserDict,
    tab: Tab,
    capture: Option<Capture>,
    /// Config changed since the engine was last (re)started.
    dirty: bool,
    search: String,
    new_word: String,
    status: String,
    /// remember_key value before it was switched off, to restore on re-enable.
    remember_prev: String,
    /// Result channel of the engine-restart thread (None = no restart in flight).
    restart_rx: Option<mpsc::Receiver<String>>,
    /// Peak modifiers seen during an active capture — releasing them all without a plain
    /// key assigns a modifier TAP (`Ctrl`, `Alt+Shift`, …) instead of a hotkey.
    capture_peak: egui::Modifiers,
    /// Live system-theme updates (accent colour, dark?) from the gsettings poller thread.
    theme_rx: mpsc::Receiver<(egui::Color32, bool)>,
    /// Previous bindings, so a switch turned back on restores what the user had.
    mode_prev: (String, String),    // (tap combo, key)
    convert_prev: (String, String), // (tap combo, key)
    undo_prev: String,
}

impl App {
    fn new() -> Self {
        let dir = config::config_dir();
        std::fs::create_dir_all(&dir).ok();
        let dict = UserDict::load(dir.clone()).unwrap_or_else(|_| UserDict::empty(dir));
        let cfg = Config::load().unwrap_or_default();
        let or_default = |v: &str, def: &str| {
            if v.trim().is_empty() || v.eq_ignore_ascii_case("none") {
                def.to_string()
            } else {
                v.to_string()
            }
        };
        let remember_prev = or_default(&cfg.ibus_hotkeys.remember_key, "Ctrl+Alt+d");
        let mode_prev = (
            or_default(&cfg.ibus_hotkeys.mode_toggle, "Ctrl"),
            cfg.ibus_hotkeys.mode_toggle_key.clone(),
        );
        let convert_prev = (
            or_default(&cfg.ibus_hotkeys.convert_last, "Ctrl+Shift"),
            or_default(&cfg.ibus_hotkeys.convert_selection_key, "Ctrl+Alt+s"),
        );
        let undo_prev = or_default(&cfg.ibus_hotkeys.undo_key, "Ctrl+grave");
        // Follow the system theme live: poll gsettings in the background, apply on change.
        let (theme_tx, theme_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut last = None;
            loop {
                let cur = read_system_theme();
                if last != Some(cur) {
                    last = Some(cur);
                    if theme_tx.send(cur).is_err() {
                        return;
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(3));
            }
        });
        App {
            cfg,
            dict,
            tab: Tab::Settings,
            capture: None,
            dirty: false,
            search: String::new(),
            new_word: String::new(),
            status: String::new(),
            remember_prev,
            restart_rx: None,
            capture_peak: egui::Modifiers::NONE,
            theme_rx,
            mode_prev,
            convert_prev,
            undo_prev,
        }
    }

    fn start_capture(&mut self, target: Capture) {
        self.capture = Some(target);
        self.capture_peak = egui::Modifiers::NONE;
    }

    fn save_cfg(&mut self) {
        match self.cfg.save_to(&Config::path()) {
            Ok(()) => {
                self.dirty = true;
                self.status.clear();
            }
            Err(e) => self.status = format!("Не удалось сохранить: {e}"),
        }
    }

    /// Restart the engine in a background thread and report the actual outcome — «движок
    /// перезапускается…» that never resolves was unreadable.
    fn restart_engine(&mut self) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let ok = std::process::Command::new("ibus")
                .arg("restart")
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _ = std::process::Command::new("ibus").args(["engine", "puntu"]).status();
            let active = std::process::Command::new("ibus")
                .arg("engine")
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "puntu")
                .unwrap_or(false);
            let _ = tx.send(if ok && active {
                "Движок перезапущен, Puntu активен".to_string()
            } else if ok {
                "Движок перезапущен; выберите Puntu в переключателе раскладок".to_string()
            } else {
                "Не удалось перезапустить движок (ibus restart)".to_string()
            });
        });
        self.restart_rx = Some(rx);
        self.dirty = false;
        self.status = "Перезапускаю движок…".to_string();
    }

    /// Collect dictionary rows across the user lists (built-in seeds excluded).
    fn rows(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for lang in [Lang::Ru, Lang::En] {
            for w in self.dict.user_recognized(lang) {
                rows.push(Row {
                    stored: w.clone(),
                    correct: w.clone(),
                    typed: translit::convert(&w, lang, lang.other()),
                    correct_lang: lang,
                    typed_lang: lang.other(),
                    action: Action::Convert,
                });
            }
            for (kind, action) in
                [(ListKind::Force, Action::ForceConvert), (ListKind::Learned, Action::Leave), (ListKind::Manual, Action::Leave)]
            {
                for w in self.dict.list(kind, lang) {
                    // Exception/force lists store the form AS TYPED; its counterpart is the
                    // "correct" reading.
                    rows.push(Row {
                        stored: w.clone(),
                        correct: translit::convert(&w, lang, lang.other()),
                        typed: w.clone(),
                        correct_lang: lang.other(),
                        typed_lang: lang,
                        action,
                    });
                }
            }
        }
        rows
    }

    /// Move a word to the list matching `action` («последнее действие побеждает»).
    fn apply_action(&mut self, row: &Row, action: Action) {
        let result = (|| -> anyhow::Result<String> {
            self.dict.remove(&row.stored)?;
            match action {
                Action::Convert => {
                    self.dict.add(&row.correct, row.correct_lang, ListKind::Recognized)?;
                    Ok(format!("«{}» теперь переводится ({} -> {})", row.correct, row.typed, row.correct))
                }
                Action::Leave => {
                    self.dict.add(&row.typed, row.typed_lang, ListKind::Manual)?;
                    Ok(format!("«{}» больше не исправляется", row.typed))
                }
                Action::ForceConvert => {
                    self.dict.add(&row.typed, row.typed_lang, ListKind::Force)?;
                    Ok(format!("«{}» всегда переводится в «{}»", row.typed, row.correct))
                }
            }
        })();
        self.status = match result {
            Ok(msg) => msg,
            Err(e) => format!("Ошибка: {e}"),
        };
    }
}


/// GNOME-style round titlebar button with a painted glyph.
#[derive(Clone, Copy)]
enum WinGlyph {
    Min,
    Max,
    Close,
}

fn window_button(ui: &mut egui::Ui, glyph: WinGlyph) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let vis = ui.style().interact(&resp);
        ui.painter().circle_filled(rect.center(), 12.0, vis.bg_fill);
        let c = rect.center();
        let r = 4.0;
        let s = egui::Stroke::new(1.5, vis.fg_stroke.color);
        match glyph {
            WinGlyph::Min => {
                ui.painter().line_segment([c + egui::vec2(-r, 3.0), c + egui::vec2(r, 3.0)], s);
            }
            WinGlyph::Max => {
                ui.painter().rect_stroke(
                    egui::Rect::from_center_size(c, egui::vec2(2.0 * r, 2.0 * r)),
                    1.0,
                    s,
                    egui::StrokeKind::Inside,
                );
            }
            WinGlyph::Close => {
                ui.painter().line_segment([c + egui::vec2(-r, -r), c + egui::vec2(r, r)], s);
                ui.painter().line_segment([c + egui::vec2(-r, r), c + egui::vec2(r, -r)], s);
            }
        }
    }
    resp
}

/// GNOME-style toggle switch (from the egui demo gallery).
fn toggle(ui: &mut egui::Ui, on: &mut bool) -> egui::Response {
    let desired_size = ui.spacing().interact_size.y * egui::vec2(2.0, 1.0);
    let (rect, mut response) = ui.allocate_exact_size(desired_size, egui::Sense::click());
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let how_on = ui.ctx().animate_bool_responsive(response.id, *on);
        let visuals = ui.style().interact_selectable(&response, *on);
        let rect = rect.expand(visuals.expansion);
        let radius = 0.5 * rect.height();
        ui.painter().rect(
            rect,
            radius,
            visuals.bg_fill,
            visuals.bg_stroke,
            egui::StrokeKind::Inside,
        );
        let circle_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
        let center = egui::pos2(circle_x, rect.center().y);
        ui.painter()
            .circle(center, 0.75 * radius, visuals.fg_stroke.color, visuals.fg_stroke);
    }
    response
}

/// One settings row: name + short description on the left, the control on the right.
fn row(ui: &mut egui::Ui, name: &str, desc: &str, control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(name).strong());
            if !desc.is_empty() {
                ui.label(egui::RichText::new(desc).weak().size(11.0));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
    ui.separator();
}


/// An Adwaita-style card (rounded box) with an expander arrow, a title and an optional
/// master switch in the header — the GNOME Settings row look. Returns `true` when the
/// switch changed. The body renders only while the switch is on.
fn card_switch(
    ui: &mut egui::Ui,
    id: &str,
    title: &str,
    desc: &str,
    on: &mut bool,
    body: impl FnOnce(&mut egui::Ui),
) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(10)
        .inner_margin(10)
        .outer_margin(egui::Margin { bottom: 6, ..Default::default() })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            let state = egui::collapsing_header::CollapsingState::load_with_default_open(
                ui.ctx(),
                egui::Id::new(id),
                false,
            );
            state
                .show_header(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.label(egui::RichText::new(title).strong());
                        if !desc.is_empty() {
                            ui.label(egui::RichText::new(desc).weak().size(11.0));
                        }
                    });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if toggle(ui, on).changed() {
                            changed = true;
                        }
                    });
                })
                .body(|ui| {
                    if *on {
                        body(ui);
                    } else {
                        ui.label(egui::RichText::new("выключено").weak());
                    }
                });
        });
    changed
}

/// Is a tap-combo config value effectively "off"?
fn parse_off(v: &str) -> bool {
    v.trim().is_empty() || v.eq_ignore_ascii_case("none")
}

/// Map an egui key press to the config's hotkey syntax (`Ctrl+Alt+d`, `Ctrl+grave`, `F12`).
fn key_to_binding(key: egui::Key, m: egui::Modifiers) -> Option<String> {
    use egui::Key as K;
    let name = match key {
        K::A => "a", K::B => "b", K::C => "c", K::D => "d", K::E => "e", K::F => "f",
        K::G => "g", K::H => "h", K::I => "i", K::J => "j", K::K => "k", K::L => "l",
        K::M => "m", K::N => "n", K::O => "o", K::P => "p", K::Q => "q", K::R => "r",
        K::S => "s", K::T => "t", K::U => "u", K::V => "v", K::W => "w", K::X => "x",
        K::Y => "y", K::Z => "z",
        K::Num0 => "0", K::Num1 => "1", K::Num2 => "2", K::Num3 => "3", K::Num4 => "4",
        K::Num5 => "5", K::Num6 => "6", K::Num7 => "7", K::Num8 => "8", K::Num9 => "9",
        K::F1 => "F1", K::F2 => "F2", K::F3 => "F3", K::F4 => "F4", K::F5 => "F5",
        K::F6 => "F6", K::F7 => "F7", K::F8 => "F8", K::F9 => "F9", K::F10 => "F10",
        K::F11 => "F11", K::F12 => "F12",
        K::Backtick => "grave",
        K::Minus => "minus",
        K::Equals => "equal",
        K::Comma => "comma",
        K::Period => "period",
        K::Semicolon => "semicolon",
        K::Slash => "slash",
        K::Backslash => "backslash",
        K::OpenBracket => "bracketleft",
        K::CloseBracket => "bracketright",
        K::Insert => "Insert",
        K::Home => "home",
        K::End => "end",
        K::Space => "space",
        _ => return None,
    };
    let mut s = String::new();
    if m.ctrl {
        s.push_str("Ctrl+");
    }
    if m.alt {
        s.push_str("Alt+");
    }
    if m.shift {
        s.push_str("Shift+");
    }
    if m.command && !m.ctrl {
        s.push_str("Super+");
    }
    s.push_str(name);
    Some(s)
}

/// Human-friendly display of a stored binding (`Ctrl+grave` → `Ctrl + \``).
fn binding_label(b: &str) -> String {
    if b.eq_ignore_ascii_case("none") {
        return "—".to_string();
    }
    b.split('+')
        .map(|p| match p.trim().to_ascii_lowercase().as_str() {
            "grave" => "`".to_string(),
            "ctrl" | "control" => "Ctrl".to_string(),
            "alt" => "Alt".to_string(),
            "shift" => "Shift".to_string(),
            "super" | "meta" | "win" => "Super".to_string(),
            other if other.len() == 1 => other.to_uppercase(),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" + ")
}

impl App {
    fn settings_tab(&mut self, ui: &mut egui::Ui) {
        let mut save = false;

        // Hotkey capture: a modal window on top of everything. Two outcomes:
        //   * a plain key (with or without modifiers) → a hotkey binding (`Ctrl+r`, `F9`);
        //   * ONLY modifiers pressed and then all released → a modifier tap (`Ctrl`,
        //     `Alt+Shift`) — bare modifiers never arrive as key events.
        if let Some(target) = self.capture {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            let mods_now = ui.ctx().input(|i| i.modifiers);
            self.capture_peak = egui::Modifiers {
                alt: self.capture_peak.alt | mods_now.alt,
                ctrl: self.capture_peak.ctrl | mods_now.ctrl,
                shift: self.capture_peak.shift | mods_now.shift,
                mac_cmd: false,
                command: self.capture_peak.command | (mods_now.command && !mods_now.ctrl),
            };
            egui::Modal::new(egui::Id::new("capture_modal")).show(ui.ctx(), |ui| {
                ui.set_width(340.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("Назначение").strong().size(15.0));
                    ui.add_space(6.0);
                    ui.label("Нажмите клавишу или сочетание.");
                    ui.label(
                        egui::RichText::new(
                            "Можно только модификаторы: Ctrl, Alt+Shift…\nEsc — отмена",
                        )
                        .weak()
                        .size(11.0),
                    );
                });
            });
            let key_captured = ui.ctx().input(|i| {
                for ev in &i.events {
                    if let egui::Event::Key { key, pressed: true, modifiers, .. } = ev {
                        if *key == egui::Key::Escape {
                            return Some(None);
                        }
                        if let Some(b) = key_to_binding(*key, *modifiers) {
                            return Some(Some(b));
                        }
                    }
                }
                None
            });
            let tap_combo = if key_captured.is_none()
                && mods_now.is_none()
                && !self.capture_peak.is_none()
            {
                let mut parts = Vec::new();
                if self.capture_peak.ctrl {
                    parts.push("Ctrl");
                }
                if self.capture_peak.alt {
                    parts.push("Alt");
                }
                if self.capture_peak.shift {
                    parts.push("Shift");
                }
                if self.capture_peak.command {
                    parts.push("Super");
                }
                let combo = parts.join("+");
                (combo != "Shift").then_some(combo)
            } else {
                None
            };
            let mut done = false;
            if let Some(result) = key_captured {
                if let Some(binding) = result {
                    match target {
                        Capture::Undo => {
                            self.cfg.ibus_hotkeys.undo_key = binding.clone();
                            self.undo_prev = binding;
                        }
                        Capture::ConvertSelection => {
                            self.cfg.ibus_hotkeys.convert_selection_key = binding.clone();
                            self.convert_prev.1 = binding;
                        }
                        Capture::Remember => {
                            self.cfg.ibus_hotkeys.remember_key = binding.clone();
                            self.remember_prev = binding;
                        }
                        Capture::ModeToggleKey => {
                            self.cfg.ibus_hotkeys.mode_toggle_key = binding.clone();
                            self.cfg.ibus_hotkeys.mode_toggle = "none".to_string();
                            self.mode_prev = ("none".to_string(), binding);
                        }
                    }
                    save = true;
                }
                done = true;
            } else if let Some(combo) = tap_combo {
                match target {
                    Capture::ModeToggleKey => {
                        self.cfg.ibus_hotkeys.mode_toggle = combo.clone();
                        self.cfg.ibus_hotkeys.mode_toggle_key = "none".to_string();
                        self.cfg.enable_modifier_taps = true;
                        self.mode_prev = (combo, "none".to_string());
                    }
                    Capture::ConvertSelection => {
                        self.cfg.ibus_hotkeys.convert_last = combo.clone();
                        self.cfg.enable_modifier_taps = true;
                        self.convert_prev.0 = combo;
                    }
                    Capture::Undo | Capture::Remember => {}
                }
                save = true;
                done = true;
            }
            if done {
                self.capture = None;
                self.capture_peak = egui::Modifiers::NONE;
            }
        }

        let mut capture_request: Option<Capture> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            let cfg = &mut self.cfg;
            let remember_prev = &self.remember_prev;
            let mode_prev = &mut self.mode_prev;
            let convert_prev = &mut self.convert_prev;
            let undo_prev = &mut self.undo_prev;

            // ============== Автопереключение (главный раздел) ==============
            let mut master_on = !cfg.dry_run;
            if card_switch(
                ui,
                "master",
                "Автопереключение",
                "исправление слов, набранных не в той раскладке",
                &mut master_on,
                |ui| {
                    // ---- внутренние настройки автокоррекции ----
                    let mut suggest = cfg.learning.suggest_after > 0;
                    row(ui, "Предлагать запоминание", "после N ручных переводов слова", |ui| {
                        if toggle(ui, &mut suggest).changed() {
                            cfg.learning.suggest_after = if suggest { 3 } else { 0 };
                            save = true;
                        }
                        if cfg.learning.suggest_after > 0 {
                            let mut n = cfg.learning.suggest_after;
                            if ui
                                .add(egui::DragValue::new(&mut n).range(1..=9).prefix("N = "))
                                .changed()
                            {
                                cfg.learning.suggest_after = n;
                                save = true;
                            }
                        }
                    });

                    let mut remember_on =
                        !parse_off(&cfg.ibus_hotkeys.remember_key);
                    row(ui, "Запомнить слово", "выделенное/последнее слово — в словарь", |ui| {
                        if toggle(ui, &mut remember_on).changed() {
                            cfg.ibus_hotkeys.remember_key = if remember_on {
                                remember_prev.clone()
                            } else {
                                "none".to_string()
                            };
                            save = true;
                        }
                        if remember_on
                            && ui
                                .button(binding_label(&cfg.ibus_hotkeys.remember_key))
                                .clicked()
                        {
                            capture_request = Some(Capture::Remember);
                        }
                    });

                    let mut undo_on = !parse_off(&cfg.ibus_hotkeys.undo_key);
                    row(ui, "Флип последнего слова", "перевести туда-обратно", |ui| {
                        if toggle(ui, &mut undo_on).changed() {
                            cfg.ibus_hotkeys.undo_key = if undo_on {
                                undo_prev.clone()
                            } else {
                                "none".to_string()
                            };
                            save = true;
                        }
                        if undo_on
                            && ui.button(binding_label(&cfg.ibus_hotkeys.undo_key)).clicked()
                        {
                            capture_request = Some(Capture::Undo);
                        }
                    });

                    // ---- Переключение на другую раскладку ----
                    let mut switch_on = !(parse_off(&cfg.ibus_hotkeys.mode_toggle)
                        && parse_off(&cfg.ibus_hotkeys.mode_toggle_key));
                    if card_switch(
                        ui,
                        "mode_switch",
                        "Переключение на другую раскладку",
                        "режим EN-автокоррекции ↔ прямой русский ввод",
                        &mut switch_on,
                        |ui| {
                            let key_mode = !parse_off(&cfg.ibus_hotkeys.mode_toggle_key);
                            let current_tap = cfg.ibus_hotkeys.mode_toggle.to_ascii_lowercase();
                            for (combo, label) in SWITCH_OPTIONS {
                                let selected =
                                    !key_mode && current_tap == combo.to_ascii_lowercase();
                                if ui.radio(selected, *label).clicked() && !selected {
                                    cfg.ibus_hotkeys.mode_toggle = combo.to_string();
                                    cfg.ibus_hotkeys.mode_toggle_key = "none".to_string();
                                    cfg.enable_modifier_taps = true;
                                    *mode_prev = (combo.to_string(), "none".to_string());
                                    save = true;
                                }
                            }
                            row(ui, "Своя клавиша", "клавиша или сочетание — нажатием", |ui| {
                                let lbl = if key_mode {
                                    binding_label(&cfg.ibus_hotkeys.mode_toggle_key)
                                } else {
                                    "назначить…".to_string()
                                };
                                if ui.button(lbl).clicked() {
                                    capture_request = Some(Capture::ModeToggleKey);
                                }
                            });
                        },
                    ) {
                        if switch_on {
                            cfg.ibus_hotkeys.mode_toggle = mode_prev.0.clone();
                            cfg.ibus_hotkeys.mode_toggle_key = mode_prev.1.clone();
                            if parse_off(&cfg.ibus_hotkeys.mode_toggle)
                                && parse_off(&cfg.ibus_hotkeys.mode_toggle_key)
                            {
                                cfg.ibus_hotkeys.mode_toggle = "Ctrl".to_string();
                            }
                            cfg.enable_modifier_taps = true;
                        } else {
                            *mode_prev = (
                                cfg.ibus_hotkeys.mode_toggle.clone(),
                                cfg.ibus_hotkeys.mode_toggle_key.clone(),
                            );
                            cfg.ibus_hotkeys.mode_toggle = "none".to_string();
                            cfg.ibus_hotkeys.mode_toggle_key = "none".to_string();
                        }
                        save = true;
                    }

                    // ---- Перевод выделенного текста ----
                    let mut conv_on = !(parse_off(&cfg.ibus_hotkeys.convert_last)
                        && parse_off(&cfg.ibus_hotkeys.convert_selection_key));
                    if card_switch(
                        ui,
                        "convert",
                        "Перевод выделенного текста",
                        "выделите мышью и нажмите жест или клавишу",
                        &mut conv_on,
                        |ui| {
                            let current = cfg.ibus_hotkeys.convert_last.to_ascii_lowercase();
                            for (combo, label) in SWITCH_OPTIONS {
                                let selected = current == combo.to_ascii_lowercase();
                                if ui.radio(selected, *label).clicked() && !selected {
                                    cfg.ibus_hotkeys.convert_last = combo.to_string();
                                    cfg.enable_modifier_taps = true;
                                    convert_prev.0 = combo.to_string();
                                    save = true;
                                }
                            }
                            row(ui, "Своя клавиша", "работает независимо от жеста", |ui| {
                                let lbl = if parse_off(&cfg.ibus_hotkeys.convert_selection_key)
                                {
                                    "назначить…".to_string()
                                } else {
                                    binding_label(&cfg.ibus_hotkeys.convert_selection_key)
                                };
                                if ui.button(lbl).clicked() {
                                    capture_request = Some(Capture::ConvertSelection);
                                }
                            });
                        },
                    ) {
                        if conv_on {
                            cfg.ibus_hotkeys.convert_last = convert_prev.0.clone();
                            cfg.ibus_hotkeys.convert_selection_key = convert_prev.1.clone();
                            cfg.enable_modifier_taps = true;
                        } else {
                            // «Выключено — значит выключено полностью»: и жест, и клавиша.
                            *convert_prev = (
                                cfg.ibus_hotkeys.convert_last.clone(),
                                cfg.ibus_hotkeys.convert_selection_key.clone(),
                            );
                            cfg.ibus_hotkeys.convert_last = "none".to_string();
                            cfg.ibus_hotkeys.convert_selection_key = "none".to_string();
                        }
                        save = true;
                    }

                    // ---- Длительность тапа ----
                    row(ui, "Длительность тапа", "дольше — считается шорткатом, не тапом", |ui| {
                        let mut ms = cfg.tap_max_hold_ms;
                        if ui
                            .add(egui::DragValue::new(&mut ms).range(100..=2000).suffix(" мс"))
                            .changed()
                        {
                            cfg.tap_max_hold_ms = ms;
                            save = true;
                        }
                    });
                },
            ) {
                cfg.dry_run = !master_on;
                save = true;
            }
        });

        if let Some(target) = capture_request {
            self.start_capture(target);
        }
        if save {
            self.save_cfg();
        }
    }

    fn dictionary_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("🔍 Поиск:");
            ui.add(egui::TextEdit::singleline(&mut self.search).desired_width(220.0));
        });
        ui.horizontal(|ui| {
            ui.label("Новое слово (правильное написание):");
            let add_field =
                ui.add(egui::TextEdit::singleline(&mut self.new_word).desired_width(180.0));
            let submitted =
                add_field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("➕ Добавить").clicked() || submitted)
                && !self.new_word.trim().is_empty()
            {
                let word = self.new_word.trim().to_lowercase();
                let lang = if word.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)) {
                    Lang::Ru
                } else {
                    Lang::En
                };
                match self.dict.add(&word, lang, ListKind::Recognized) {
                    Ok(()) => {
                        let wrong = translit::convert(&word, lang, lang.other());
                        self.status = format!("Запомнено: {wrong} -> {word}");
                        self.new_word.clear();
                    }
                    Err(e) => self.status = format!("Ошибка: {e}"),
                }
            }
        });
        ui.separator();

        let mut rows = self.rows();
        let filter = self.search.trim().to_lowercase();
        if !filter.is_empty() {
            rows.retain(|r| r.correct.contains(&filter) || r.typed.contains(&filter));
        }

        let mut pending: Option<(usize, Action)> = None;
        let mut remove: Option<String> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("dict")
                .num_columns(4)
                .striped(true)
                .min_col_width(120.0)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Слово").strong());
                    ui.label(egui::RichText::new("Набирается как").strong());
                    ui.label(egui::RichText::new("Действие").strong());
                    ui.label("");
                    ui.end_row();
                    for (i, r) in rows.iter().enumerate() {
                        ui.label(&r.correct);
                        ui.label(&r.typed);
                        let mut action = r.action;
                        egui::ComboBox::from_id_salt(format!("act{i}"))
                            .selected_text(action.label())
                            .show_ui(ui, |ui| {
                                for opt in
                                    [Action::Convert, Action::Leave, Action::ForceConvert]
                                {
                                    if ui
                                        .selectable_label(action == opt, opt.label())
                                        .clicked()
                                        && action != opt
                                    {
                                        action = opt;
                                    }
                                }
                            });
                        if action != r.action {
                            pending = Some((i, action));
                        }
                        if ui.small_button("🗑").on_hover_text("Удалить").clicked() {
                            remove = Some(r.stored.clone());
                        }
                        ui.end_row();
                    }
                });
            if rows.is_empty() {
                ui.label(egui::RichText::new("Словарь пуст").weak());
            }
        });
        if let Some((i, action)) = pending {
            let row = &rows[i];
            let row = Row {
                stored: row.stored.clone(),
                correct: row.correct.clone(),
                typed: row.typed.clone(),
                correct_lang: row.correct_lang,
                typed_lang: row.typed_lang,
                action: row.action,
            };
            self.apply_action(&row, action);
        }
        if let Some(w) = remove {
            match self.dict.remove(&w) {
                Ok(()) => self.status = format!("Удалено: {w}"),
                Err(e) => self.status = format!("Ошибка: {e}"),
            }
        }
    }
}

impl eframe::App for App {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0; 4] // transparent — the rounded window is painted by us
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // Follow the system theme (accent colour / dark) live.
        if let Ok((accent, dark)) = self.theme_rx.try_recv() {
            apply_adwaita_theme(ui.ctx(), accent, dark);
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(3));

        // Engine-restart progress: poll the background thread's answer.
        if let Some(rx) = &self.restart_rx {
            match rx.try_recv() {
                Ok(msg) => {
                    self.status = msg;
                    self.restart_rx = None;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ui.ctx().request_repaint_after(std::time::Duration::from_millis(300));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.status = "Не удалось перезапустить движок".to_string();
                    self.restart_rx = None;
                }
            }
        }

        // The rounded window itself (GNOME look; square when maximized).
        let maximized = ui.input(|i| i.viewport().maximized.unwrap_or(false));
        let radius: f32 = if maximized { 0.0 } else { 14.0 };
        let rect = ui.max_rect();
        let border = if ui.visuals().dark_mode {
            egui::Color32::from_gray(0x50)
        } else {
            egui::Color32::from_gray(0xc8)
        };
        ui.painter().rect(
            rect,
            radius,
            ui.visuals().panel_fill,
            egui::Stroke::new(1.0, border),
            egui::StrokeKind::Inside,
        );

        // Headerbar: tabs on the left (GNOME view switcher), window buttons on the right,
        // empty space drags the window, double-click maximizes.
        egui::Panel::top("titlebar")
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 10,
                right: 8,
                top: 8,
                bottom: 4,
            }))
            .show(ui, |ui| {
                let bar = ui.max_rect();
                let drag =
                    ui.interact(bar, egui::Id::new("title_drag"), egui::Sense::click_and_drag());
                if drag.drag_started() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
                }
                if drag.double_clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                }
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.tab, Tab::Settings, "⚙ Настройки");
                    ui.selectable_value(&mut self.tab, Tab::Dictionary, "📖 Словарь");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if window_button(ui, WinGlyph::Close).clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        if window_button(ui, WinGlyph::Max).clicked() {
                            ui.ctx()
                                .send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
                        }
                        if window_button(ui, WinGlyph::Min).clicked() {
                            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Minimized(true));
                        }
                    });
                });
            });

        egui::Panel::bottom("status")
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 4,
                bottom: 10,
            }))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if self.dirty {
                        if ui.button("Применить (перезапустить движок)").clicked() {
                            self.restart_engine();
                        }
                    } else if self.restart_rx.is_none() {
                        ui.label(
                            egui::RichText::new("Изменения словаря применяются сами").weak(),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(&self.status);
                    });
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::new().inner_margin(egui::Margin {
                left: 12,
                right: 12,
                top: 4,
                bottom: 4,
            }))
            .show(ui, |ui| match self.tab {
                Tab::Settings => self.settings_tab(ui),
                Tab::Dictionary => self.dictionary_tab(ui),
            });

        // Frameless windows lose the compositor resize edges — a drag handle in the
        // bottom-right corner brings resizing back.
        if !maximized {
            let br = egui::Rect::from_min_max(rect.max - egui::vec2(18.0, 18.0), rect.max);
            let resp = ui.interact(br, egui::Id::new("resize_br"), egui::Sense::drag());
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeSouthEast);
            }
            if resp.drag_started() {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::BeginResize(
                    egui::ResizeDirection::SouthEast,
                ));
            }
        }

        // Never enable the system IME for this window: the engine would otherwise
        // auto-correct the very words being typed into the dictionary fields (the reported
        // «дописывает часть» corruption). Plain winit key events bypass IBus entirely.
        ui.ctx().output_mut(|o| o.ime = None);
    }
}

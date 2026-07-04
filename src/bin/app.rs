//! Puntu — единое окно настроек и словаря (`puntu-app`).
//!
//! Одно приложение с двумя вкладками:
//! * **Настройки** — GNOME-подобные переключатели вкл/выкл для каждого механизма, назначение
//!   клавиш «нажатием» (кнопка → нажмите сочетание), краткие описания.
//! * **Словарь** — пары «слово / как набирается» по всем пользовательским спискам, поиск,
//!   добавление и удаление.
//!
//! Изменения настроек пишутся в `config.toml` сразу; движок читает конфиг при старте, поэтому
//! внизу появляется кнопка «Применить» (перезапуск движка). Правки словаря движок подхватывает
//! сам через hot-reload — перезапуск не нужен.

use eframe::egui;
use puntu::config::{self, Config};
use puntu::detect::translit;
use puntu::detect::userdict::{ListKind, UserDict};
use puntu::keymap::Lang;

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([620.0, 560.0])
            .with_min_inner_size([520.0, 420.0])
            .with_title("Puntu"),
        ..Default::default()
    };
    eframe::run_native(
        "Puntu",
        options,
        Box::new(|cc| {
            cc.egui_ctx.set_zoom_factor(1.1);
            Ok(Box::new(App::new()))
        }),
    )
}

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
}

impl App {
    fn new() -> Self {
        let dir = config::config_dir();
        std::fs::create_dir_all(&dir).ok();
        let dict = UserDict::load(dir.clone()).unwrap_or_else(|_| UserDict::empty(dir));
        let cfg = Config::load().unwrap_or_default();
        let remember_prev = if cfg.ibus_hotkeys.remember_key.eq_ignore_ascii_case("none") {
            "Ctrl+Alt+d".to_string()
        } else {
            cfg.ibus_hotkeys.remember_key.clone()
        };
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
        }
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

    fn restart_engine(&mut self) {
        // Detached: `ibus restart` takes a couple of seconds — don't freeze the UI.
        let _ = std::process::Command::new("sh")
            .args(["-c", "ibus restart && sleep 2 && ibus engine puntu"])
            .spawn();
        self.dirty = false;
        self.status = "Движок перезапускается…".to_string();
    }
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

        // Hotkey capture overlay: while active, eat the next key press.
        if let Some(target) = self.capture {
            ui.scope(|ui| {
                ui.visuals_mut().override_text_color = Some(egui::Color32::LIGHT_BLUE);
                ui.label("Нажмите сочетание клавиш…   (Esc — отмена)");
            });
            ui.separator();
            let captured = ui.ctx().input(|i| {
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
            if let Some(result) = captured {
                if let Some(binding) = result {
                    match target {
                        Capture::Undo => self.cfg.ibus_hotkeys.undo_key = binding,
                        Capture::ConvertSelection => {
                            self.cfg.ibus_hotkeys.convert_selection_key = binding
                        }
                        Capture::Remember => {
                            self.cfg.ibus_hotkeys.remember_key = binding.clone();
                            self.remember_prev = binding;
                        }
                    }
                    save = true;
                }
                self.capture = None;
            }
        }

        egui::ScrollArea::vertical().show(ui, |ui| {
            let cfg = &mut self.cfg;

            let mut autocorrect = !cfg.dry_run;
            row(ui, "Автокоррекция", "исправлять слова не в той раскладке", |ui| {
                if toggle(ui, &mut autocorrect).changed() {
                    cfg.dry_run = !autocorrect;
                    save = true;
                }
            });

            row(ui, "Тапы модификаторов", "тап Ctrl — режим EN↔RU; Ctrl+Shift — перевод выделения", |ui| {
                if toggle(ui, &mut cfg.enable_modifier_taps).changed() {
                    save = true;
                }
            });

            let mut suggest = cfg.learning.suggest_after > 0;
            row(ui, "Предлагать запоминание", "после N ручных переводов одного слова", |ui| {
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

            let mut remember_on = !cfg.ibus_hotkeys.remember_key.eq_ignore_ascii_case("none");
            row(ui, "Запомнить слово", "клавиша: выделенное/последнее слово — в словарь", |ui| {
                if toggle(ui, &mut remember_on).changed() {
                    cfg.ibus_hotkeys.remember_key = if remember_on {
                        self.remember_prev.clone()
                    } else {
                        "none".to_string()
                    };
                    save = true;
                }
                if remember_on
                    && ui.button(binding_label(&cfg.ibus_hotkeys.remember_key)).clicked()
                {
                    self.capture = Some(Capture::Remember);
                }
            });

            row(ui, "Флип последнего слова", "перевести последнее слово туда-обратно", |ui| {
                if ui.button(binding_label(&cfg.ibus_hotkeys.undo_key)).clicked() {
                    self.capture = Some(Capture::Undo);
                }
            });

            row(ui, "Перевод выделения (клавиша)", "альтернатива тапу Ctrl+Shift", |ui| {
                if ui
                    .button(binding_label(&cfg.ibus_hotkeys.convert_selection_key))
                    .clicked()
                {
                    self.capture = Some(Capture::ConvertSelection);
                }
            });

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
        });

        if save {
            self.save_cfg();
        }
    }

    fn dictionary_tab(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Поиск:");
            ui.text_edit_singleline(&mut self.search);
            ui.separator();
            let add = ui.text_edit_singleline(&mut self.new_word);
            let clicked = ui.button("Добавить").clicked();
            if (clicked || (add.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))))
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
                        self.status = format!("Запомнено: {wrong} → {word}");
                        self.new_word.clear();
                    }
                    Err(e) => self.status = format!("Ошибка: {e}"),
                }
            }
        });
        ui.separator();

        // (word, wrong form, list label) across the user lists.
        let mut rows: Vec<(String, String, &str)> = Vec::new();
        for lang in [Lang::Ru, Lang::En] {
            for w in self.dict.user_recognized(lang) {
                rows.push((w.clone(), translit::convert(&w, lang, lang.other()), "переводить"));
            }
            for (kind, label) in [
                (ListKind::Force, "всегда переводить"),
                (ListKind::Learned, "не исправлять"),
                (ListKind::Manual, "не исправлять"),
            ] {
                for w in self.dict.list(kind, lang) {
                    rows.push((w.clone(), translit::convert(&w, lang, lang.other()), label));
                }
            }
        }
        let filter = self.search.trim().to_lowercase();
        if !filter.is_empty() {
            rows.retain(|(w, alt, _)| w.contains(&filter) || alt.contains(&filter));
        }

        let mut remove: Option<String> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            egui::Grid::new("dict")
                .num_columns(4)
                .striped(true)
                .min_col_width(110.0)
                .show(ui, |ui| {
                    ui.label(egui::RichText::new("Слово").strong());
                    ui.label(egui::RichText::new("Набирается как").strong());
                    ui.label(egui::RichText::new("Действие").strong());
                    ui.label("");
                    ui.end_row();
                    for (w, alt, label) in &rows {
                        ui.label(w);
                        ui.label(alt);
                        ui.label(*label);
                        if ui.small_button("✕").clicked() {
                            remove = Some(w.clone());
                        }
                        ui.end_row();
                    }
                });
            if rows.is_empty() {
                ui.label(egui::RichText::new("Словарь пуст").weak());
            }
        });
        if let Some(w) = remove {
            match self.dict.remove(&w) {
                Ok(()) => self.status = format!("Удалено: {w}"),
                Err(e) => self.status = format!("Ошибка: {e}"),
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::top("tabs").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Settings, "Настройки");
                ui.selectable_value(&mut self.tab, Tab::Dictionary, "Словарь");
            });
        });
        egui::Panel::bottom("status").show(ui, |ui| {
            ui.horizontal(|ui| {
                if self.dirty {
                    if ui.button("Применить (перезапустить движок)").clicked() {
                        self.restart_engine();
                    }
                } else {
                    ui.label(egui::RichText::new("Изменения словаря применяются сами").weak());
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(&self.status);
                });
            });
        });
        egui::CentralPanel::default().show(ui, |ui| match self.tab {
            Tab::Settings => self.settings_tab(ui),
            Tab::Dictionary => self.dictionary_tab(ui),
        });
    }
}

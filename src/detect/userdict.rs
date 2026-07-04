//! User-managed word lists and the command/technical-token guard.
//!
//! Four kinds of list, stored as plain one-word-per-line text files in `~/.config/puntu`
//! (human-editable, hot-reloaded by the daemon):
//!   * `manual.{ru,en}.txt`  — exceptions: never auto-correct these
//!   * `learned.{ru,en}.txt` — auto-added when you Undo a correction
//!   * `force.{ru,en}.txt`   — always convert these
//!   * `commands.txt`        — English commands/utilities treated as exceptions
//!
//! Plus `is_command_context`, a heuristic that spots paths/flags/identifiers so we never
//! mangle terminal or code input.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::keymap::Lang;

/// Which list a word belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListKind {
    Manual,
    Learned,
    Force,
    Command,
    /// Recognized words (e.g. service names) — exact-match valid words, so a wrong-layout form
    /// whose other-layout reading is a recognized word converts confidently.
    Recognized,
}

impl ListKind {
    fn file_name(self, lang: Lang) -> String {
        match self {
            ListKind::Manual => format!("manual.{lang}.txt"),
            ListKind::Learned => format!("learned.{lang}.txt"),
            ListKind::Force => format!("force.{lang}.txt"),
            ListKind::Command => "commands.txt".to_string(),
            ListKind::Recognized => format!("words.{lang}.txt"),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct LangLists {
    manual: HashSet<String>,
    learned: HashSet<String>,
    force: HashSet<String>,
    recognized: HashSet<String>,
}

/// All user dictionaries, in memory, for O(1) hot-path checks.
#[derive(Clone, Debug)]
pub struct UserDict {
    dir: PathBuf,
    ru: LangLists,
    en: LangLists,
    commands: HashSet<String>,
}

/// A small built-in seed of common commands so the guard works before the user edits anything.
const BUILTIN_COMMANDS: &[&str] = &[
    "git", "ls", "cd", "rm", "mv", "cp", "cat", "sudo", "apt", "npm", "npx", "yarn", "pnpm",
    "cargo", "rustc", "rustup", "go", "make", "cmake", "gcc", "clang", "python", "python3",
    "pip", "node", "deno", "docker", "kubectl", "ssh", "scp", "curl", "wget", "grep", "sed",
    "awk", "find", "tar", "gzip", "vim", "nano", "code", "tmux", "systemctl", "journalctl",
    "ping", "ip", "ifconfig", "ps", "kill", "top", "htop", "df", "du", "chmod", "chown",
    "mkdir", "touch", "echo", "export", "source", "bash", "sh", "zsh", "man", "sudo",
];

/// Common English tech abbreviations that aren't dictionary words, so their wrong-layout forms
/// (e.g. `гш` → `ui`) convert out of the box. Seeded into the recognized set for English.
const BUILTIN_RECOGNIZED_EN: &[&str] = &[
    "ui", "ux", "api", "db", "os", "id", "url", "uri", "css", "html", "js", "ts", "jsx", "tsx",
    "sql", "ai", "ml", "io", "cli", "gui", "json", "xml", "yaml", "toml", "csv", "md", "app",
    "dev", "prod", "repo", "pr", "ci", "qa", "ux", "oop", "crud", "jwt", "sdk", "ide", "vm",
    "vps", "dns", "tcp", "udp", "ssl", "tls", "ftp", "smtp", "http", "https", "ssh", "url",
    "ram", "cpu", "gpu", "ssd", "hdd", "usb", "pdf", "png", "jpg", "jpeg", "svg", "gif", "exe",
    "dll", "env", "auth", "admin", "url", "src", "img", "btn", "nav", "config", "cfg", "len",
];

impl UserDict {
    /// Build an empty dictionary set rooted at `dir`, seeded with the built-in command list.
    pub fn empty(dir: PathBuf) -> UserDict {
        let commands = BUILTIN_COMMANDS.iter().map(|s| s.to_string()).collect();
        let en = LangLists {
            recognized: BUILTIN_RECOGNIZED_EN.iter().map(|s| s.to_string()).collect(),
            ..LangLists::default()
        };
        UserDict { dir, ru: LangLists::default(), en, commands }
    }

    /// Load all lists from disk (missing files are treated as empty). Built-in commands are
    /// always merged in.
    pub fn load(dir: PathBuf) -> Result<UserDict> {
        let mut d = UserDict::empty(dir);
        d.reload()?;
        Ok(d)
    }

    /// Re-read every list file from disk (used on startup and on hot-reload).
    pub fn reload(&mut self) -> Result<()> {
        self.ru = self.load_lang(Lang::Ru)?;
        self.en = self.load_lang(Lang::En)?;
        // Built-in recognized abbreviations ∪ user file.
        self.en.recognized.extend(BUILTIN_RECOGNIZED_EN.iter().map(|s| s.to_string()));
        // Commands = built-in seed ∪ user file.
        let mut commands: HashSet<String> =
            BUILTIN_COMMANDS.iter().map(|s| s.to_string()).collect();
        commands.extend(read_list(&self.dir.join("commands.txt"))?);
        self.commands = commands;
        Ok(())
    }

    fn load_lang(&self, lang: Lang) -> Result<LangLists> {
        Ok(LangLists {
            manual: read_list(&self.dir.join(ListKind::Manual.file_name(lang)))?,
            learned: read_list(&self.dir.join(ListKind::Learned.file_name(lang)))?,
            force: read_list(&self.dir.join(ListKind::Force.file_name(lang)))?,
            recognized: read_list(&self.dir.join(ListKind::Recognized.file_name(lang)))?,
        })
    }

    /// Is `word` an exact recognized word in `lang` (built-in dictionaries aside)?
    pub fn is_recognized(&self, word: &str, lang: Lang) -> bool {
        self.lists(lang).recognized.contains(&word.to_lowercase())
    }

    fn lists(&self, lang: Lang) -> &LangLists {
        match lang {
            Lang::Ru => &self.ru,
            Lang::En => &self.en,
        }
    }

    /// Is `word` (in `lang`) on a never-correct list (manual, learned, or a known command)?
    pub fn is_exception(&self, word: &str, lang: Lang) -> bool {
        let w = word.to_lowercase();
        let l = self.lists(lang);
        l.manual.contains(&w) || l.learned.contains(&w) || self.commands.contains(&w)
    }

    /// Is `word` (in `lang`) on the always-convert list?
    pub fn is_force(&self, word: &str, lang: Lang) -> bool {
        self.lists(lang).force.contains(&word.to_lowercase())
    }

    /// Add `word` to a list (both in memory and on disk).
    ///
    /// Conflicting entries are resolved so the **latest user action wins**:
    /// * learning a Recognized word (`увы`) removes its wrong-layout form (`eds`) from the
    ///   exception lists — a stale exception silently blocked the freshly-taught conversion
    ///   (exceptions are checked before recognized words in `Detector::decide`);
    /// * adding an exception (flip-back auto-learning) removes the counterpart from the
    ///   recognized list, so the dictionary doesn't show both «переводить» and
    ///   «не исправлять» rows for one pair.
    pub fn add(&mut self, word: &str, lang: Lang, kind: ListKind) -> Result<()> {
        let w = word.to_lowercase();
        match kind {
            ListKind::Recognized => {
                let wrong = crate::detect::translit::convert(&w, lang, lang.other());
                self.drop_from(&wrong, lang.other(), &[ListKind::Manual, ListKind::Learned])?;
            }
            ListKind::Manual | ListKind::Learned => {
                let counterpart = crate::detect::translit::convert(&w, lang, lang.other());
                self.drop_from(&counterpart, lang.other(), &[ListKind::Recognized])?;
            }
            _ => {}
        }
        match (kind, lang) {
            (ListKind::Command, _) => {
                self.commands.insert(w.clone());
            }
            (ListKind::Manual, Lang::Ru) => {
                self.ru.manual.insert(w.clone());
            }
            (ListKind::Manual, Lang::En) => {
                self.en.manual.insert(w.clone());
            }
            (ListKind::Learned, Lang::Ru) => {
                self.ru.learned.insert(w.clone());
            }
            (ListKind::Learned, Lang::En) => {
                self.en.learned.insert(w.clone());
            }
            (ListKind::Force, Lang::Ru) => {
                self.ru.force.insert(w.clone());
            }
            (ListKind::Force, Lang::En) => {
                self.en.force.insert(w.clone());
            }
            (ListKind::Recognized, Lang::Ru) => {
                self.ru.recognized.insert(w.clone());
            }
            (ListKind::Recognized, Lang::En) => {
                self.en.recognized.insert(w.clone());
            }
        }
        append_line(&self.dir.join(kind.file_name(lang)), &w)
    }

    /// Remove `word` from the given lists of one language, rewriting only files that
    /// actually changed. Used for conflict resolution in [`Self::add`].
    fn drop_from(&mut self, word: &str, lang: Lang, kinds: &[ListKind]) -> Result<()> {
        for &kind in kinds {
            let set = self.set_mut(lang, kind);
            if set.remove(word) {
                let snapshot = persistable(set, lang, kind);
                write_list(&self.dir.join(kind.file_name(lang)), &snapshot)?;
            }
        }
        Ok(())
    }

    /// Remove `word` from every list of every language and rewrite affected files.
    pub fn remove(&mut self, word: &str) -> Result<()> {
        let w = word.to_lowercase();
        for lang in [Lang::Ru, Lang::En] {
            for kind in
                [ListKind::Manual, ListKind::Learned, ListKind::Force, ListKind::Recognized]
            {
                let set = self.set_mut(lang, kind);
                if set.remove(&w) {
                    let snapshot = persistable(set, lang, kind);
                    write_list(&self.dir.join(kind.file_name(lang)), &snapshot)?;
                }
            }
        }
        if self.commands.remove(&w) {
            // Only persist the user-added portion (drop the built-ins on rewrite).
            let builtins: HashSet<&str> = BUILTIN_COMMANDS.iter().copied().collect();
            let extra: Vec<String> =
                self.commands.iter().filter(|c| !builtins.contains(c.as_str())).cloned().collect();
            write_list(&self.dir.join("commands.txt"), &extra)?;
        }
        Ok(())
    }

    /// Remove a single learned word for `lang` (the `dict forget` command).
    pub fn forget(&mut self, word: &str, lang: Lang) -> Result<()> {
        let w = word.to_lowercase();
        let set = self.set_mut(lang, ListKind::Learned);
        if set.remove(&w) {
            let snapshot: Vec<String> = set.iter().cloned().collect();
            write_list(&self.dir.join(ListKind::Learned.file_name(lang)), &snapshot)?;
        }
        Ok(())
    }

    /// Clear all learned words (both languages).
    pub fn clear_learned(&mut self) -> Result<()> {
        self.ru.learned.clear();
        self.en.learned.clear();
        write_list(&self.dir.join(ListKind::Learned.file_name(Lang::Ru)), &[])?;
        write_list(&self.dir.join(ListKind::Learned.file_name(Lang::En)), &[])?;
        Ok(())
    }

    /// Recognized words the **user** added — built-in seeds (api, css, …) filtered out.
    /// What a dictionary UI shows and lets the user delete.
    pub fn user_recognized(&self, lang: Lang) -> Vec<String> {
        let mut v = persistable(&self.lists(lang).recognized, lang, ListKind::Recognized);
        v.sort();
        v
    }

    /// Snapshot a list for display (the `dict list` command).
    pub fn list(&self, kind: ListKind, lang: Lang) -> Vec<String> {
        let mut v: Vec<String> = match kind {
            ListKind::Command => self.commands.iter().cloned().collect(),
            ListKind::Manual => self.lists(lang).manual.iter().cloned().collect(),
            ListKind::Learned => self.lists(lang).learned.iter().cloned().collect(),
            ListKind::Force => self.lists(lang).force.iter().cloned().collect(),
            ListKind::Recognized => self.lists(lang).recognized.iter().cloned().collect(),
        };
        v.sort();
        v
    }

    fn set_mut(&mut self, lang: Lang, kind: ListKind) -> &mut HashSet<String> {
        let lists = match lang {
            Lang::Ru => &mut self.ru,
            Lang::En => &mut self.en,
        };
        match kind {
            ListKind::Manual => &mut lists.manual,
            ListKind::Learned => &mut lists.learned,
            ListKind::Force => &mut lists.force,
            ListKind::Recognized => &mut lists.recognized,
            ListKind::Command => unreachable!("commands are language-neutral"),
        }
    }
}

/// The subset of an in-memory set worth writing to the user's file: built-in seeds are merged
/// into memory on load (recognized EN abbreviations), so persisting the raw set would dump all
/// of them into the user file the first time it's rewritten. Filter them back out — the same
/// treatment `remove` gives the built-in command list.
fn persistable(set: &HashSet<String>, lang: Lang, kind: ListKind) -> Vec<String> {
    if kind == ListKind::Recognized && lang == Lang::En {
        set.iter()
            .filter(|w| !BUILTIN_RECOGNIZED_EN.contains(&w.as_str()))
            .cloned()
            .collect()
    } else {
        set.iter().cloned().collect()
    }
}

/// Heuristic: does `token` look like a command / path / flag / identifier rather than a word?
/// Such tokens are never auto-corrected, so terminal and code input stays intact.
pub fn is_command_context(token: &str) -> bool {
    token.chars().any(|c| matches!(c, '/' | '\\' | '.' | '~' | '-' | '_' | '@' | ':' ))
        || token.chars().any(|c| c.is_ascii_digit())
}

fn read_list(path: &Path) -> Result<HashSet<String>> {
    if !path.exists() {
        return Ok(HashSet::new());
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading list {}", path.display()))?;
    Ok(text
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_lowercase())
        .collect())
}

fn write_list(path: &Path, words: &[String]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut sorted: Vec<&String> = words.iter().collect();
    sorted.sort();
    let body = sorted.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("\n");
    std::fs::write(path, body + "\n").with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn append_line(path: &Path, word: &str) -> Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{word}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Per-test directory: tests in one binary share a pid and run in parallel, so the dir
    /// must be tagged per test or one test's `remove_dir_all` races another's file writes
    /// (flaked on CI).
    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("puntu-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        p
    }

    #[test]
    fn exception_suppresses_and_forget_restores() {
        let mut d = UserDict::empty(tmp("exception"));
        assert!(!d.is_exception("превед", Lang::Ru));
        d.add("превед", Lang::Ru, ListKind::Manual).unwrap();
        assert!(d.is_exception("превед", Lang::Ru));
        d.remove("превед").unwrap();
        assert!(!d.is_exception("превед", Lang::Ru));
    }

    #[test]
    fn learned_forget_is_targeted() {
        let mut d = UserDict::empty(tmp("learned"));
        d.add("ghbdtn", Lang::En, ListKind::Learned).unwrap();
        d.add("ghbdtycr", Lang::En, ListKind::Learned).unwrap();
        d.forget("ghbdtn", Lang::En).unwrap();
        assert!(!d.is_exception("ghbdtn", Lang::En));
        assert!(d.is_exception("ghbdtycr", Lang::En));
    }

    #[test]
    fn builtin_commands_are_exceptions() {
        let d = UserDict::empty(tmp("builtin"));
        assert!(d.is_exception("git", Lang::En));
        assert!(d.is_exception("ls", Lang::En));
    }

    #[test]
    fn remove_does_not_dump_builtin_recognized_into_user_file() {
        let dir = tmp("remove");
        let mut d = UserDict::empty(dir.clone());
        d.add("tiktok", Lang::En, ListKind::Recognized).unwrap();
        d.add("zoom", Lang::En, ListKind::Recognized).unwrap();
        d.remove("tiktok").unwrap();
        // The rewrite must keep only the user's own words — not the ~80 built-in
        // abbreviations (api, css, …) that are merged into memory on load.
        let file = std::fs::read_to_string(dir.join("words.en.txt")).unwrap();
        let words: Vec<&str> = file.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(words, vec!["zoom"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_resolves_conflicts_latest_wins() {
        let dir = tmp("conflict");
        let mut d = UserDict::empty(dir.clone());
        // A stale exception («eds» flipped back once) silently blocked a freshly-taught
        // word — teaching «увы» must clear it, or the user sees learning "not working".
        d.add("eds", Lang::En, ListKind::Learned).unwrap();
        assert!(d.is_exception("eds", Lang::En));
        d.add("увы", Lang::Ru, ListKind::Recognized).unwrap();
        assert!(!d.is_exception("eds", Lang::En), "teaching must clear the blocking exception");
        assert!(d.is_recognized("увы", Lang::Ru));
        // The reverse: a later flip-back exception clears the recognized entry.
        d.add("eds", Lang::En, ListKind::Learned).unwrap();
        assert!(!d.is_recognized("увы", Lang::Ru), "flip-back must clear the taught word");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn command_context_heuristic() {
        assert!(is_command_context("/usr/bin"));
        assert!(is_command_context("--force"));
        assert!(is_command_context("v0.1"));
        assert!(!is_command_context("привет"));
        assert!(!is_command_context("hello"));
    }
}

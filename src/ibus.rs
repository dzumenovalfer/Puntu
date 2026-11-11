//! IBus engine front-end — an alternative to the uinput/clipboard correction path.
//!
//! The IBus engine sits between the keyboard and the focused app: when our engine is active,
//! every keystroke the user makes is delivered to *us first* via `ProcessKeyEvent`. We can
//! either consume the key (buffer it), forward it to the app unchanged, or replace a stretch
//! of text by emitting `CommitText`. That eliminates the fundamental race condition of the
//! uinput path: there is no other writer to the app to collide with.
//!
//! # How registration works
//!
//! IBus discovers engines by reading `~/.local/share/ibus/component/*.xml` (or the system-wide
//! `/usr/share/ibus/component/`) at startup, NOT through a runtime DBus call. So to make our
//! engine visible in GNOME's input switcher:
//!
//!   1. Install `data/puntu.xml` to `~/.local/share/ibus/component/puntu.xml`.
//!   2. Run `ibus restart` (or `ibus write-cache --system && ibus restart`).
//!   3. The XML's `<exec>` field points at our binary; IBus launches it on demand.
//!
//! The binary itself connects to the IBus private bus (`ibus address`), registers an
//! [`Factory`], and idles waiting for `CreateEngine` calls — handled here via the [`librush`]
//! crate which provides correctly-serialised IBusText/IBusEngine bindings without dragging
//! in C GObject.

mod engine;
pub mod runtime;

pub use runtime::run;

/// Would the engine accept `binding` as a hotkey (`Ctrl+grave`, `F12`, `Ctrl+Alt+d`)?
/// `"none"` and the empty string are valid too — they mean "disabled".
///
/// For front-ends that write the config: a binding the engine cannot parse does not produce
/// an error anywhere, it just turns the hotkey off, so it has to be refused at the door.
pub fn is_valid_hotkey(binding: &str) -> bool {
    let b = binding.trim();
    b.is_empty() || b.eq_ignore_ascii_case("none") || engine::parse_hotkey(b).is_some()
}

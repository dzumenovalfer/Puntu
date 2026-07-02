//! Puntu — automatic keyboard-layout corrector (Punto Switcher analog) for Ubuntu.
//!
//! The crate is split into a **pure core** (layout tables, tokenizer + trusted-context
//! buffer, n-gram detector, user dictionaries, config) that builds and unit-tests without
//! any system libraries, and a **daemon layer** (`input`, `action`, `hotkeys`, `ipc`) behind
//! the `daemon` feature that talks to evdev/uinput and the running session.
//!
//! Run core tests in isolation with: `cargo test --no-default-features --lib`.

pub mod buffer;
pub mod config;
pub mod detect;
// The control-socket *client* (`ipc::send_command`) is always available so lightweight
// front-ends (the tray/GUI) can pause/resume/query the daemon without pulling in evdev/uinput.
// The socket *server* (`ipc::serve`, `ipc::Control`) stays behind the `daemon` feature.
pub mod ipc;
pub mod keymap;
pub mod layout;

#[cfg(feature = "daemon")]
pub mod action;
#[cfg(feature = "daemon")]
pub mod clipboard;
#[cfg(feature = "daemon")]
pub mod hotkeys;
#[cfg(feature = "daemon")]
pub mod input;

// IBus engine front-end — an alternative to the uinput correction path. The IBus engine sees
// the user's keystrokes BEFORE they reach the focused app, so correction is atomic and
// there's no race with concurrent typing. M5 in PLAN.md.
#[cfg(feature = "ibus")]
pub mod ibus;

pub use keymap::Lang;

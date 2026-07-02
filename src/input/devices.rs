//! Locate the physical keyboard and pointer by capability, not by name.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use evdev::{Device, Key, RelativeAxisType};

/// Find the real text keyboard. Picking the *first* device with letter keys is wrong: gaming mice
/// (e.g. a Razer Basilisk) expose a second "keyboard" node for macros that has A–Z + Enter and no
/// `BTN_LEFT`, so it passed the old filter — and when it was chosen, the daemon listened on (and,
/// with a grab, blocked) the mouse instead of the keyboard. Instead we require a *full* keyboard
/// signature and, among matches, pick the device reporting the **most** keys (a real keyboard has
/// ~100+; a mouse's macro node far fewer). Pointer devices (relative axes / `BTN_LEFT`) are
/// excluded outright.
pub fn find_keyboard() -> Result<(PathBuf, Device)> {
    let mut best: Option<(PathBuf, Device, usize)> = None;
    for (path, dev) in evdev::enumerate() {
        if !is_keyboard(&dev) {
            continue;
        }
        let keys = dev.supported_keys().map(|k| k.iter().count()).unwrap_or(0);
        if best.as_ref().map_or(true, |(_, _, n)| keys > *n) {
            best = Some((path, dev, keys));
        }
    }
    best.map(|(p, d, _)| (p, d)).ok_or_else(|| {
        anyhow!("no keyboard found in /dev/input (are you in the `input` group? did you re-login?)")
    })
}

/// Find the first device that has mouse buttons (used only to read clicks for invalidation).
pub fn find_pointer() -> Option<(PathBuf, Device)> {
    evdev::enumerate().find(|(_, dev)| is_pointer(dev))
}

/// A real text keyboard: the full core signature (letters, space, Enter, Esc, both base
/// modifiers), and *not* a pointer. The richer signature rejects a mouse's macro keyboard node,
/// which typically lacks the modifier/Esc set.
fn is_keyboard(dev: &Device) -> bool {
    if is_pointer(dev) {
        return false;
    }
    match dev.supported_keys() {
        Some(keys) => [
            Key::KEY_A,
            Key::KEY_Z,
            Key::KEY_SPACE,
            Key::KEY_ENTER,
            Key::KEY_ESC,
            Key::KEY_LEFTCTRL,
            Key::KEY_LEFTSHIFT,
        ]
        .iter()
        .all(|k| keys.contains(*k)),
        None => false,
    }
}

/// A pointer: reports mouse buttons or relative motion. Such a device is never a typing keyboard
/// (and must never be grabbed — that would freeze the cursor/touchpad).
fn is_pointer(dev: &Device) -> bool {
    let has_button = dev.supported_keys().is_some_and(|k| k.contains(Key::BTN_LEFT));
    let has_motion =
        dev.supported_relative_axes().is_some_and(|a| a.contains(RelativeAxisType::REL_X));
    has_button || has_motion
}

/// Open a device by explicit path (for `--device`).
pub fn open(path: &str) -> Result<Device> {
    Device::open(path).map_err(|e| anyhow!("opening {path}: {e}"))
}

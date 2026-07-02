# Puntu

An automatic keyboard-layout corrector for **Ubuntu (Wayland/GNOME)** — an open-source analog
of Punto Switcher. If you type `ghbdtn` when you meant `привет`, Puntu fixes it in place.

Puntu is an **IBus engine**: it sees your keystrokes *before* they reach the focused app, so a
correction is a single atomic write — no backspacing, no clipboard tricks, no race with your
typing.

## Why an IBus engine

On Wayland/GNOME, external clients can't capture or inject global keystrokes (Mutter reserves
the input-method protocol for IBus). So instead of fighting that, Puntu *is* an input method:

- **Intercept:** IBus routes each keystroke for the focused field to the engine first.
- **Analyze:** letters accumulate into a word, shown live as **preedit** (underlined) text;
  the app sees nothing yet. The word is scored against the same physical keys read in the other
  layout with a character-trigram model + dictionaries.
- **Correct:** on a separator (space/enter/tab/punctuation) the engine commits either the word
  as typed or its other-layout form — in one `commit_text`. Atomic, instant, layout-independent.

### Fixes Punto's classic bug

Auto-correction only fires in a **trusted context**: a word typed straight through with the
cursor at its end. Any navigation, mid-word edit, or focus change marks the buffer untrusted, so
editing one letter in the middle of a word never corrupts it.

## Install (one command)

Targets Ubuntu/Debian GNOME (Wayland or X11). Because Puntu is an IBus engine, there's **no**
`/dev/input` access, uinput, `input` group, udev rule, systemd service, or re-login involved —
ibus-daemon launches the engine on demand.

```sh
curl -fsSL https://raw.githubusercontent.com/dzumenovalfer/Puntu/main/install.sh | bash
```

This downloads the **latest prebuilt release** (binaries + the big Russian dictionary — no
Rust toolchain needed), installs the runtime dependencies (`ibus`, `wl-clipboard`), registers
the IBus component, restarts IBus, and adds **Puntu** to your GNOME input sources. Re-run any
time to update — it's idempotent and never rewrites your existing layouts.

From a source checkout, `./install.sh` does the same but builds with cargo (installing rustup
if needed).

Then switch to it with **Super+Space** (or the input-source icon) and pick **Puntu**.

```sh
puntu-ibus status      # is the engine registered + active?
puntu-ibus enable      # make Puntu the active engine
puntu-ibus disable     # back to the plain US layout (xkb:us::eng)
```

## Using it

When **Puntu** is the active source you keep typing on a US keyboard. Two modes:

| Mode               | How to get there      | What it does                                            |
|--------------------|-----------------------|---------------------------------------------------------|
| **EN auto-correct** (default) | active on switch | type English; words you typed in the wrong layout (`ghbdtn`) auto-convert to Russian (`привет`) on the next separator |
| **RU direct**      | tap **Ctrl** once     | every key maps straight to its Russian letter, committed as you go — type Russian without a separate `ru` source |

Tap **Ctrl** again to toggle back. A small hint (`EN auto-correct` / `RU direct`) shows the
current mode.

### Terminals and password fields are untouched

When the focused field reports itself as a **terminal** (GNOME Terminal/Console and other
VTE-based apps do) or a **password/PIN** field, Puntu goes fully transparent: no buffering, no
preedit, no auto-correction, no hotkeys. A command line can never be mangled by a correction,
and passwords never sit in a preedit.

### Hotkeys (defaults; configurable in `~/.config/puntu/config.toml`)

| Key                | Action                                                       |
|--------------------|--------------------------------------------------------------|
| **tap Ctrl**       | Toggle EN auto-correct ↔ RU direct                           |
| `Ctrl+` `` ` ``    | Undo the last commit (restore exactly what you typed)        |
| **tap Ctrl+Shift** | Convert the current mouse selection to the other layout      |
| `Ctrl+Alt+S`       | Convert the current mouse selection (regular key, if taps are unreliable on your setup) |

> Modifier **taps** (press-and-release with no other key between) fire on release; IBus doesn't
> deliver release events on every setup, so the regular `Ctrl+Alt+S` hotkey is provided as a
> dependable alternative. Override any binding in `config.toml` under `[ibus_hotkeys]`.
> A tap only counts when the whole press→release fits in `tap_max_hold_ms` (default 500 ms), so
> held shortcuts like Ctrl+click can't accidentally toggle the mode. When a selection conversion
> does nothing, a hint near the caret says why (no selection / nothing to fix).

### Dictionaries (via the `puntu` CLI)

The engine shares its dictionaries with the CLI, so you teach it the same way:

```sh
puntu dict add <word> [--force]   # exception (never convert) / always-convert
puntu dict learn <word>           # a real word (e.g. a service name) — its wrong-layout
                                  #   form will convert (ешлещл → tiktok)
puntu dict rm <word>              # remove from every list
puntu dict list [--manual|--learned|--force] [--ru|--en]
```

Single-letter words are handled too (`ш`→`i`, `b`→`и`). Dictionary edits are **hot-reloaded**:
the running engine picks them up within a second, no restart. For the full command reference
see [COMMANDS.md](COMMANDS.md).

### Big Russian dictionary (optional, automatic)

`install.sh` builds a ~1.5M-word-form FST (~2 MB) from `dictionaries/russian.utf-8` into
`~/.config/puntu/russian.fst`; the engine loads it at startup for confident exact matches on
inflected forms. Rebuild manually with `puntu build-dict dictionaries/russian.utf-8`.

## Try the detector without installing

The detection core runs with no devices, IBus, or permissions:

```sh
cargo run --no-default-features -- stdin
# type:  ghbdtn привет руддщ hello git
cargo run --no-default-features -- dict add превед   # never correct this word
```

## Tests

```sh
cargo test --no-default-features --lib     # pure-core unit tests (layout, buffer, detector)
cargo test --no-default-features --features ibus   # also build the IBus engine
```

## Implementation notes

For the fixed ЙЦУКЕН/QWERTY pair, Puntu ships **hand-written layout tables** instead of linking
libxkbcommon, which keeps the build free of system `-dev` packages. The engine is built on
`librush` (pure-Rust IBus bindings over `zbus`, no GObject). The earlier evdev/uinput daemon
(`--features daemon`) remains in the tree as a fallback for non-GNOME compositors; see
[PLAN.md](PLAN.md) for the migration history.

## License

MIT

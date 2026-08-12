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

> On **X11**, typing and auto-correction work as usual, but converting a *mouse selection*
> does not: it reads the selection with `wl-paste`, which is Wayland-only. Everything that
> acts on the word you just typed (auto-correct, the flip hotkey, case fixes) is unaffected.

```sh
curl -fsSL https://raw.githubusercontent.com/dzumenovalfer/Puntu/main/install.sh | bash
```

This downloads the **latest prebuilt release** (binaries + the big Russian dictionary — no
Rust toolchain needed), installs the runtime dependencies (`ibus`, `wl-clipboard`), registers
the IBus component, restarts IBus, and adds **Puntu** to your GNOME input sources. Re-run any
time to update — it's idempotent and never rewrites your existing layouts.

From a source checkout, `./install.sh` does the same but builds with cargo (installing rustup
if needed). To move an existing install to a newer build, use the update path instead:

```sh
./install.sh --update
```

It pulls, rebuilds, reinstalls the binaries and restarts IBus — and skips everything that is
one-time (system packages, icons, menu entry, Electron flags), so it needs no sudo unless the
engine's path actually changed. Your config and word lists are never touched.

Then switch to it with **Super+Space** (or the input-source icon) and pick **Puntu**.

```sh
puntu-ibus status      # is the engine registered + active?
puntu-ibus doctor      # full check: session, IBus, IM env, helpers, config
puntu-ibus enable      # make Puntu the active engine
puntu-ibus disable     # back to the plain US layout (xkb:us::eng)
```

### Hyprland, sway and other non-GNOME sessions

Puntu works there, but the session has to provide two things GNOME provides for you: a running
`ibus-daemon`, and the environment that points apps at it. The installer sets up everything on
its side (including IBus's own `preload-engines`, which is what a non-GNOME session reads
instead of the GNOME input-source list) and prints the rest. For Hyprland, in `hyprland.conf`:

```
exec-once = ibus-daemon -drxR
env = GTK_IM_MODULE,ibus
env = QT_IM_MODULE,ibus
env = XMODIFIERS,@im=ibus
```

All three variables matter here. IBus ships no `input-method-v2` implementation, so on wlroots
compositors apps reach it through these client-side IM modules — not through the compositor's
`text-input-v3`, which is how it works on GNOME (where nothing needs setting at all).

Then `puntu-ibus doctor` checks the whole chain and names anything still missing.

## Using it

When **Puntu** is the active source you keep typing on a US keyboard. Two modes:

| Mode               | How to get there      | What it does                                            |
|--------------------|-----------------------|---------------------------------------------------------|
| **EN auto-correct** (default) | active on switch | type English; words you typed in the wrong layout (`ghbdtn`) auto-convert to Russian (`привет`) on the next separator |
| **RU direct**      | tap **Ctrl** once     | every key maps straight to its Russian letter, committed as you go — type Russian without a separate `ru` source |

Tap **Ctrl** again to toggle back. A small hint (`EN auto-correct` / `RU direct`) shows the
current mode.

### Electron/Chromium apps (VS Code, Claude, Chrome, Slack…)

An IBus engine only sees keystrokes from apps connected to the input-method framework.
Electron/Chromium apps must run in **native Wayland with IME enabled** — otherwise *no*
system input method works in them (Puntu, Chinese or Japanese input alike; it's a
platform-wide limitation, not a Puntu one).

`install.sh` handles this automatically: it sets `ELECTRON_OZONE_PLATFORM_HINT=auto`
(takes effect after the next login) and writes the flags into VS Code's
`~/.vscode/argv.json`. For other Electron apps add these flags to their launcher
(`.desktop` file):

```
--ozone-platform-hint=auto --enable-wayland-ime --wayland-text-input-version=3
```

> ⚠️ **Snap VS Code cannot work with any input method**: its launcher hard-codes
> `--ozone-platform=x11`, and its bundled runtime can't start under the host Wayland stack
> when bypassed. Install the official .deb from
> [code.visualstudio.com](https://code.visualstudio.com/download) instead — settings and
> extensions are preserved.

### Terminals: manual only; password fields: untouched

When the focused field reports itself as a **terminal** (GNOME Terminal/Console and other
VTE-based apps do), automatic conversions are off — a command line can never be mangled by a
correction. Manual use still works: tap **Ctrl** for RU-direct typing, `Ctrl+` `` ` `` to flip
the last word. Selection conversion is disabled there (terminals don't replace a selection).
**Password/PIN** fields make Puntu fully transparent — passwords never sit in a preedit.

### Games, and apps where Puntu should keep out

A word you're typing lives in the IBus **preedit** until Puntu commits it. That has one
consequence worth knowing: a client that doesn't draw a preedit would show *nothing at all*
while you type. Puntu refuses to work in such clients rather than swallow your text — and if
DBus stops accepting text mid-session, it commits what it holds, turns itself off for that
field, and lets you keep typing instead of going quiet until a restart.

The same swallowing is what breaks games: SDL opens an IBus context for its text input, so
**WASD arrives looking exactly like typing**. Clients named in `[ibus_clients]` are left
completely alone; the default covers SDL:

```toml
[ibus_clients]
require_preedit_capability = true       # no preedit → Puntu keeps out
passthrough_clients = ["SDL"]           # case-insensitive substring match
passthrough_xim = false                 # true also silences Wine/Proton (they come via XIM)
```

If a game still eats your keys, find its name in the engine log
(`~/.local/state/puntu/engine.log`, look for `client=…`) and add it to the list — the config
is hot-reloaded, no restart needed. Wine/Proton games reach IBus through the XIM bridge and
report themselves as `xim`, so `passthrough_xim = true` is the switch for those; it also
turns Puntu off in ordinary X11/XWayland apps, which is the trade-off.

### Hotkeys (defaults; configurable in `~/.config/puntu/config.toml`)

| Key                | Action                                                       |
|--------------------|--------------------------------------------------------------|
| **tap Ctrl**       | Toggle EN auto-correct ↔ RU direct                           |
| `Ctrl+` `` ` ``    | Undo the last commit (restore exactly what you typed)        |
| **tap Ctrl+Shift** | Convert the current mouse selection to the other layout      |
| `Ctrl+Alt+S`       | Convert the current mouse selection (regular key, if taps are unreliable on your setup) |
| `Ctrl+Alt+D`       | Remember the selected (or last typed) word in the dictionary |

> Modifier **taps** (press-and-release with no other key between) fire on release; IBus doesn't
> deliver release events on every setup, so the regular `Ctrl+Alt+S` hotkey is provided as a
> dependable alternative. Override any binding in `config.toml` under `[ibus_hotkeys]`.
> A tap only counts when the whole press→release fits in `tap_max_hold_ms` (default 500 ms), so
> held shortcuts like Ctrl+click can't accidentally toggle the mode. When a selection conversion
> does nothing, a hint near the caret says why (no selection / nothing to fix).

### Teaching the dictionary

Three ways to add words, each can be turned off:

1. **Repeat-conversion offer** — when you manually convert the *same* word for the third
   time (forward flip via `Ctrl+` `` ` `` or a single-word selection conversion), a dialog
   offers to remember it; from then on its wrong-layout form converts automatically.
   Configure with `puntu config set suggest_after <n>` (`0` disables).
2. **`Ctrl+Alt+D`** — remember the selected word (or the last typed one) right away. A hint
   near the caret confirms it. Rebind via `puntu config set remember_key '...'`.
3. **Dictionary window** — `puntu dict ui`: a list of «привет / ghbdtn» pairs across all
   your lists, with add and remove buttons (zenity).

Rejecting an auto-conversion (flip it back with `Ctrl+` `` ` ``) still teaches the opposite:
the word is added to the never-correct list. Every dictionary change fires a desktop
notification and reaches the running engine within a second — no restart.

### The Puntu app (settings + dictionary)

`install.sh` adds a **Puntu** application to the menu (`puntu-app`): one window with two
tabs. *Настройки* — on/off switches for auto-correction, taps and each learning mechanism,
hotkey bindings assigned by **pressing the keys**, thresholds. *Словарь* — your word pairs
(«привет / ghbdtn») with search, add and remove. Dictionary edits reach the running engine
within a second; settings changes offer an engine restart button. (`puntu settings` and
`puntu dict ui` remain as zenity-based CLI fallbacks.)

### Dictionaries (via the `puntu` CLI)

The engine shares its dictionaries with the CLI, so you teach it the same way:

```sh
puntu dict add <word> [--force]   # exception (never convert) / always-convert
puntu dict learn <word>           # a real word (e.g. a service name) — its wrong-layout
                                  #   form will convert (ешлещл → tiktok)
puntu dict rm <word>              # remove from every list
puntu dict list [--manual|--learned|--force] [--ru|--en]
puntu dict export [file]          # all your own words → one portable file (stdout if no path)
puntu dict import <file>          # merge it back on another machine
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

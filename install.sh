#!/usr/bin/env bash
# Puntu installer — one command, two modes.
#
#   Remote (recommended):   curl -fsSL https://raw.githubusercontent.com/dzumenovalfer/Puntu/main/install.sh | bash
#     Downloads the latest GitHub release (prebuilt x86_64 binaries + the big Russian
#     dictionary) — no Rust toolchain needed. Falls back to a source build when there is no
#     matching release for your architecture.
#
#   Local (inside a git checkout):   ./install.sh
#     Builds from source with cargo (installs rustup if missing).
#
# Both modes then: register the IBus component, restart IBus, add Puntu to GNOME input
# sources. Puntu is an IBus engine, so there is **no** /dev/input access, uinput, `input`
# group, udev rule, systemd service, or re-login involved — ibus-daemon launches the engine
# on demand via the component's <exec> path.
#
# Re-run safe (idempotent). Needs sudo only for `apt` and the system-wide component copy.
#   ./install.sh --no-sudo  # skip apt + the /usr/share copy (you provide ibus/wl-clipboard)
set -euo pipefail

# The GitHub repository releases are fetched from (override with PUNTU_REPO=owner/name).
PUNTU_REPO="${PUNTU_REPO:-dzumenovalfer/Puntu}"

say()  { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*"; }

NO_SUDO=0
[[ "${1:-}" == "--no-sudo" ]] && NO_SUDO=1

# Local mode = the script sits inside a source checkout. When piped via `curl | bash`,
# BASH_SOURCE is unset/stdin and there is no Cargo.toml next to it → remote mode.
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-/dev/null}")" 2>/dev/null && pwd || echo /nonexistent)"
LOCAL=0
[[ -f "$REPO_DIR/Cargo.toml" ]] && LOCAL=1

# 1. System packages ----------------------------------------------------------
# Remote mode only needs the runtime deps; a source build also needs a C toolchain.
if [[ "$NO_SUDO" -eq 0 ]]; then
  if command -v apt-get >/dev/null 2>&1; then
    PKGS=(ibus wl-clipboard curl)
    [[ "$LOCAL" -eq 1 ]] && PKGS+=(build-essential)
    say "Installing system packages (${PKGS[*]})…"
    sudo apt-get update -qq || warn "apt-get update failed (continuing)"
    sudo apt-get install -y "${PKGS[@]}" || warn "could not install some system packages"
  else
    warn "apt-get not found — install manually: ibus, wl-clipboard (and a C compiler for source builds)"
  fi
else
  warn "Skipping system packages (--no-sudo). Need: ibus, wl-clipboard."
fi

command -v ibus >/dev/null 2>&1 || warn "ibus not on PATH — Puntu needs IBus running (GNOME default)."

CONFIG_DIR="$HOME/.config/puntu"
mkdir -p "$CONFIG_DIR"

# 2. Obtain the binaries ------------------------------------------------------
if [[ "$LOCAL" -eq 1 ]]; then
  # ---- source build (original flow) ----
  if ! command -v cargo >/dev/null 2>&1; then
    say "Installing Rust toolchain (rustup)…"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
    # shellcheck disable=SC1091
    source "$HOME/.cargo/env"
  fi
  command -v cargo >/dev/null 2>&1 || { warn "cargo still not found — install Rust manually"; exit 1; }
  command -v cc    >/dev/null 2>&1 || { warn "no C linker (cc) — install build-essential"; exit 1; }

  # `--no-default-features --features ibus` builds only the engine and the pure-core CLI
  # (dict / config / build-dict). It skips evdev/uinput entirely — no device dependencies.
  say "Building and installing puntu-ibus + puntu (release)…"
  cargo install --path "$REPO_DIR" --no-default-features --features ibus --force
  BIN_DIR="$HOME/.cargo/bin"

  if [[ -f "$REPO_DIR/dictionaries/russian.utf-8" ]]; then
    say "Building the big Russian dictionary (≈1.5M words → ~2MB FST)…"
    "$BIN_DIR/puntu" build-dict "$REPO_DIR/dictionaries/russian.utf-8" \
      || warn "could not build the dictionary FST (engine still works without it)"
  fi
else
  # ---- prebuilt release ----
  ARCH="$(uname -m)"
  ASSET_URL=""
  if [[ "$ARCH" == "x86_64" ]]; then
    say "Looking up the latest release of $PUNTU_REPO…"
    ASSET_URL="$(curl -fsSL "https://api.github.com/repos/$PUNTU_REPO/releases/latest" 2>/dev/null \
      | grep -o '"browser_download_url": *"[^"]*puntu-x86_64-linux\.tar\.gz"' \
      | head -n1 | sed 's/.*"\(https[^"]*\)"/\1/')" || true
  fi
  if [[ -n "$ASSET_URL" ]]; then
    say "Downloading $ASSET_URL…"
    TMP="$(mktemp -d)"
    trap 'rm -rf "$TMP"' EXIT
    curl -fsSL "$ASSET_URL" -o "$TMP/puntu.tar.gz"
    tar -xzf "$TMP/puntu.tar.gz" -C "$TMP"
    BIN_DIR="$HOME/.local/bin"
    mkdir -p "$BIN_DIR"
    install -m 0755 "$TMP/puntu/puntu" "$TMP/puntu/puntu-ibus" "$BIN_DIR/"
    if [[ -f "$TMP/puntu/russian.fst" ]]; then
      say "Installing the big Russian dictionary → $CONFIG_DIR/russian.fst"
      install -m 0644 "$TMP/puntu/russian.fst" "$CONFIG_DIR/russian.fst"
    fi
    case ":$PATH:" in
      *":$BIN_DIR:"*) ;;
      *) warn "$BIN_DIR is not on PATH — the engine still works (IBus uses absolute paths), but add it to use the \`puntu\` CLI." ;;
    esac
  else
    # No release / non-x86_64 → clone and hand over to the local (source-build) flow.
    warn "No prebuilt release found for $ARCH — building from source instead."
    command -v git >/dev/null 2>&1 || { warn "git is required for the source fallback"; exit 1; }
    SRC="$HOME/.local/share/puntu-src"
    if [[ -d "$SRC/.git" ]]; then
      git -C "$SRC" pull --ff-only || warn "could not update $SRC (continuing with what's there)"
    else
      git clone --depth 1 "https://github.com/$PUNTU_REPO" "$SRC"
    fi
    exec bash "$SRC/install.sh" ${NO_SUDO:+$( [[ "$NO_SUDO" -eq 1 ]] && echo --no-sudo )}
  fi
fi

ENGINE_BIN="$BIN_DIR/puntu-ibus"
[[ -x "$ENGINE_BIN" ]] || { warn "puntu-ibus did not install to $ENGINE_BIN"; exit 1; }

# 3. Make sure the legacy uinput daemon is not also running -------------------
# Two correctors writing to the same text field fight each other. The IBus engine is the only
# one we want now, so stop+disable the old systemd service if a previous install enabled it.
if systemctl --user list-unit-files 2>/dev/null | grep -q '^puntu\.service'; then
  say "Disabling the legacy uinput daemon (IBus engine replaces it)…"
  systemctl --user disable --now puntu puntu-tray >/dev/null 2>&1 || true
fi

# 4. Register the IBus component ----------------------------------------------
# `puntu-ibus install` writes ~/.local/share/ibus/component/puntu.xml with <exec> pointing at
# the binary we just installed (an absolute, stable path ibus-daemon can launch).
say "Registering the IBus component…"
"$ENGINE_BIN" install >/dev/null
USER_XML="$HOME/.local/share/ibus/component/puntu.xml"
if [[ "$NO_SUDO" -eq 0 && -f "$USER_XML" ]]; then
  # IBus reliably scans /usr/share/ibus/component/; copy there so the engine is always found.
  sudo install -m 0644 "$USER_XML" /usr/share/ibus/component/puntu.xml \
    || warn "could not copy component to /usr/share/ibus/component (user-local copy may suffice)"
fi

# 5. Restart IBus so it discovers the (re)registered component ----------------
say "Restarting IBus to pick up the engine…"
ibus restart >/dev/null 2>&1 || warn "ibus restart failed — log out/in if Puntu doesn't appear."
sleep 1

# 6. Add Puntu to GNOME input sources + activate it ---------------------------
# So it shows up under Super+Space / the input-source icon and survives a reboot. Done
# defensively: we only append if it isn't already there, and never rewrite an existing layout.
if command -v gsettings >/dev/null 2>&1 \
   && gsettings writable org.gnome.desktop.input-sources sources >/dev/null 2>&1; then
  CUR="$(gsettings get org.gnome.desktop.input-sources sources 2>/dev/null || echo '[]')"
  if [[ "$CUR" == *"'puntu'"* ]]; then
    say "Puntu already in GNOME input sources."
  else
    say "Adding Puntu to GNOME input sources…"
    case "$CUR" in
      "@a(ss) []"|"[]"|"") NEW="[('ibus', 'puntu')]" ;;
      *)                   NEW="${CUR%]}, ('ibus', 'puntu')]" ;;
    esac
    gsettings set org.gnome.desktop.input-sources sources "$NEW" \
      || warn "could not edit input sources — add 'Puntu' via Settings → Keyboard → Input Sources."
  fi
fi
ibus engine puntu >/dev/null 2>&1 || true

# 7. App-menu entries: the settings and dictionary windows --------------------
# Makes Puntu discoverable as an application (GNOME overview search: «Puntu»), not only as
# a CLI: «Puntu — настройки» opens the zenity settings window, «Puntu — словарь» the
# dictionary editor.
say "Installing application menu entries…"
APPS_DIR="$HOME/.local/share/applications"
mkdir -p "$APPS_DIR"
cat > "$APPS_DIR/puntu-settings.desktop" <<DESK
[Desktop Entry]
Name=Puntu — настройки
Name[en]=Puntu Settings
Comment=Настройки автопереключения раскладки
Comment[en]=Keyboard layout auto-corrector settings
Exec=$BIN_DIR/puntu settings
Icon=input-keyboard
Type=Application
Terminal=false
Categories=Utility;Settings;
Keywords=puntu;keyboard;layout;раскладка;настройки;
DESK
cat > "$APPS_DIR/puntu-dictionary.desktop" <<DESK
[Desktop Entry]
Name=Puntu — словарь
Name[en]=Puntu Dictionary
Comment=Словарь автопереключения раскладки
Comment[en]=Keyboard layout auto-corrector dictionary
Exec=$BIN_DIR/puntu dict ui
Icon=input-keyboard
Type=Application
Terminal=false
Categories=Utility;
Keywords=puntu;dictionary;словарь;
DESK
update-desktop-database "$APPS_DIR" 2>/dev/null || true

# 8. Electron/Chromium apps: enable the system input method -------------------
# An IBus engine only sees keys from apps connected to the input-method framework. Electron
# apps must run in native Wayland with IME enabled, or NO system input method works in them
# (Puntu, Chinese, Japanese — alike). One-time and harmless when already set.
say "Configuring Electron apps for the system input method…"
ENV_CONF="$HOME/.config/environment.d/90-puntu-electron.conf"
if ! grep -qs "ELECTRON_OZONE_PLATFORM_HINT" "$ENV_CONF" 2>/dev/null; then
  mkdir -p "$(dirname "$ENV_CONF")"
  printf 'ELECTRON_OZONE_PLATFORM_HINT=auto\n' > "$ENV_CONF"
  say "  ELECTRON_OZONE_PLATFORM_HINT=auto set (applies after the next login)"
fi

# VS Code (.deb / non-snap): persist the Wayland-IME flags in argv.json.
if command -v code >/dev/null 2>&1 && ! readlink -f "$(command -v code)" | grep -q "^/snap/"; then
  ARGV="$HOME/.vscode/argv.json"
  mkdir -p "$HOME/.vscode"
  if [[ ! -f "$ARGV" ]]; then
    printf '{\n\t"ozone-platform-hint": "auto",\n\t"enable-wayland-ime": true,\n\t"wayland-text-input-version": 3\n}\n' > "$ARGV"
    say "  VS Code: created $ARGV with Wayland-IME flags (restart VS Code)"
  elif ! grep -q "enable-wayland-ime" "$ARGV"; then
    sed -i '0,/{/s//{\n\t"ozone-platform-hint": "auto",\n\t"enable-wayland-ime": true,\n\t"wayland-text-input-version": 3,/' "$ARGV"
    say "  VS Code: added Wayland-IME flags to $ARGV (restart VS Code)"
  fi
fi

# Snap VS Code is a dead end: its launcher hard-codes `--ozone-platform=x11` as the LAST
# argument (last flag wins in Chromium), and its core20 runtime can't start under the host
# Wayland stack when bypassing the wrapper. Nothing we can configure — tell the user.
if command -v snap >/dev/null 2>&1 && snap list code >/dev/null 2>&1; then
  warn "Snap VS Code cannot use system input methods (its launcher hard-codes X11)."
  warn "For Puntu (or any IME) in VS Code, install the official .deb instead:"
  warn "  sudo snap remove code"
  warn "  wget -qO /tmp/code.deb 'https://code.visualstudio.com/sha/download?build=stable&os=linux-deb-x64'"
  warn "  sudo apt install -y /tmp/code.deb    # settings and extensions are preserved"
fi

say "Done."
cat <<EOF

Next steps:
  • Switch to Puntu with Super+Space (or the input-source icon) and pick "Puntu".
  • Status / registration:   puntu-ibus status
  • Turn the engine off:     puntu-ibus disable      (back to xkb:us::eng)
  • Manage words:            puntu dict learn <service> ; puntu dict add <word>
  • Engine logs:             PUNTU_LOG=puntu=debug ibus restart ; journalctl --user -f | grep puntu
EOF

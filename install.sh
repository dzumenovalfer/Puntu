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
  cargo install --path "$REPO_DIR" --no-default-features --features ibus,app,gui --force
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
    # The unified settings+dictionary window (absent from pre-0.4 releases).
    [[ -f "$TMP/puntu/puntu-app" ]] && install -m 0755 "$TMP/puntu/puntu-app" "$BIN_DIR/"
    # The tray indicator (absent from pre-0.7 releases).
    [[ -f "$TMP/puntu/puntu-gui" ]] && install -m 0755 "$TMP/puntu/puntu-gui" "$BIN_DIR/"
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

# 6c. Icons: the app icon + the three tray-status icons ----------------------
# Installed into the user's hicolor theme so `Icon=puntu` (desktop files) and the tray's
# `puntu*-symbolic` names resolve. The status icons are symbolic (single-colour), so the
# shell recolours them to the panel foreground on both light and dark top bars.
say "Installing icons…"
ICONS="$HOME/.local/share/icons/hicolor"
mkdir -p "$ICONS/scalable/apps" "$ICONS/scalable/status"

# In a source checkout the SVGs live in ./icons; remote installs use the copies embedded
# below. `put_icon` prefers the checkout, so local edits to ./icons take effect at once.
put_icon() {  # put_icon <repo-relative-src> <dest> ; returns non-zero if not in the checkout
  [[ -f "$REPO_DIR/$1" ]] && install -m 0644 "$REPO_DIR/$1" "$2"
}

put_icon icons/puntu.svg "$ICONS/scalable/apps/puntu.svg" || cat > "$ICONS/scalable/apps/puntu.svg" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" width="128" height="128" viewBox="0 0 128 128">
  <defs>
    <linearGradient id="tile" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#62a0ea"/>
      <stop offset="1" stop-color="#1a63c4"/>
    </linearGradient>
    <linearGradient id="key" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#ffffff"/>
      <stop offset="1" stop-color="#e9f0f9"/>
    </linearGradient>
  </defs>
  <rect x="8" y="8" width="112" height="112" rx="27" fill="url(#tile)"/>
  <path d="M35 8h58a27 27 0 0 1 27 27v9c0 8-64 14-112 0v-9A27 27 0 0 1 35 8Z" fill="#ffffff" opacity="0.10"/>
  <rect x="28" y="32" width="72" height="68" rx="16" fill="#123a72" opacity="0.30"/>
  <rect x="28" y="28" width="72" height="68" rx="16" fill="url(#key)" stroke="#d3deee" stroke-width="1"/>
  <path transform="translate(38.8,36.8) scale(2.1)" d="M12 4V1L8 5l4 4V6c3.31 0 6 2.69 6 6 0 1.01-.25 1.97-.7 2.8l1.46 1.46C19.54 15.03 20 13.57 20 12c0-4.42-3.58-8-8-8zm0 14c-3.31 0-6-2.69-6-6 0-1.01.25-1.97.7-2.8L5.24 7.74C4.46 8.97 4 10.43 4 12c0 4.42 3.58 8 8 8v3l4-4-4-4v3z" fill="#1a63c4"/>
</svg>
SVG

put_icon icons/puntu-symbolic.svg "$ICONS/scalable/status/puntu-symbolic.svg" || cat > "$ICONS/scalable/status/puntu-symbolic.svg" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">
  <path fill="#2e3436" fill-rule="evenodd"
        d="M4.6 2.5h6.8A2.6 2.6 0 0 1 14 5.1v5.8a2.6 2.6 0 0 1-2.6 2.6H4.6A2.6 2.6 0 0 1 2 10.9V5.1a2.6 2.6 0 0 1 2.6-2.6Z
           M4.7 5.5h3.7v-0.8l2.2 1.45-2.2 1.45v-0.8h-3.7Z
           M11.3 9.2h-3.7v-0.8l-2.2 1.45 2.2 1.45v-0.8h3.7Z"/>
</svg>
SVG

put_icon icons/puntu-paused-symbolic.svg "$ICONS/scalable/status/puntu-paused-symbolic.svg" || cat > "$ICONS/scalable/status/puntu-paused-symbolic.svg" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">
  <path fill="#2e3436" fill-rule="evenodd"
        d="M4.6 2.5h6.8A2.6 2.6 0 0 1 14 5.1v5.8a2.6 2.6 0 0 1-2.6 2.6H4.6A2.6 2.6 0 0 1 2 10.9V5.1a2.6 2.6 0 0 1 2.6-2.6Z
           M5.5 5.7a0.6 0.6 0 0 1 0.6-0.6h0.4a0.6 0.6 0 0 1 0.6 0.6v4.6a0.6 0.6 0 0 1-0.6 0.6h-0.4a0.6 0.6 0 0 1-0.6-0.6Z
           M8.9 5.7a0.6 0.6 0 0 1 0.6-0.6h0.4a0.6 0.6 0 0 1 0.6 0.6v4.6a0.6 0.6 0 0 1-0.6 0.6h-0.4a0.6 0.6 0 0 1-0.6-0.6Z"/>
</svg>
SVG

put_icon icons/puntu-disabled-symbolic.svg "$ICONS/scalable/status/puntu-disabled-symbolic.svg" || cat > "$ICONS/scalable/status/puntu-disabled-symbolic.svg" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">
  <path fill="#2e3436" fill-rule="evenodd"
        d="M4.6 2.5h6.8A2.6 2.6 0 0 1 14 5.1v5.8a2.6 2.6 0 0 1-2.6 2.6H4.6A2.6 2.6 0 0 1 2 10.9V5.1a2.6 2.6 0 0 1 2.6-2.6Z
           M5.1 4.1h5.8a1.5 1.5 0 0 1 1.5 1.5v4.8a1.5 1.5 0 0 1-1.5 1.5H5.1a1.5 1.5 0 0 1-1.5-1.5V5.6a1.5 1.5 0 0 1 1.5-1.5Z
           M9.98 4.3l1.24 1-5.2 6.4-1.24-1Z"/>
</svg>
SVG

gtk-update-icon-cache -f -t "$ICONS" >/dev/null 2>&1 || true

# 7. App-menu entry: the Puntu application (settings + dictionary in one window) ----------
say "Installing the application menu entry…"
APPS_DIR="$HOME/.local/share/applications"
mkdir -p "$APPS_DIR"
# Drop the two split entries earlier versions installed.
rm -f "$APPS_DIR/puntu-settings.desktop" "$APPS_DIR/puntu-dictionary.desktop"
cat > "$APPS_DIR/puntu.desktop" <<DESK
[Desktop Entry]
Name=Puntu
Comment=Настройки и словарь автопереключения раскладки
Comment[en]=Keyboard layout auto-corrector: settings and dictionary
Exec=$BIN_DIR/puntu-app
Icon=puntu
Type=Application
Terminal=false
# Match the window to this entry (and its Icon) when the app runs under X11/XWayland,
# where the shell sees WM_CLASS instead of the Wayland app_id.
StartupWMClass=puntu
Categories=Utility;Settings;
Keywords=puntu;keyboard;layout;раскладка;настройки;словарь;dictionary;
DESK
update-desktop-database "$APPS_DIR" 2>/dev/null || true

# 7b. Tray indicator: autostart + launch now ----------------------------------
# Open the app / pause temporarily / disable the engine, with a status icon.
if [[ -x "$BIN_DIR/puntu-gui" ]]; then
  say "Setting up the tray indicator (autostart)…"
  AUTOSTART_DIR="$HOME/.config/autostart"
  mkdir -p "$AUTOSTART_DIR"
  cat > "$AUTOSTART_DIR/puntu-tray.desktop" <<DESK
[Desktop Entry]
Name=Puntu Tray
Comment=Индикатор и быстрые действия Puntu
Exec=$BIN_DIR/puntu-gui
Icon=puntu
Type=Application
Terminal=false
X-GNOME-Autostart-enabled=true
NoDisplay=true
DESK
  # Restart, not just start-if-absent: a tray from the previous version would otherwise
  # keep running with the old binary (and the old icons) until the next login.
  pkill -x puntu-gui >/dev/null 2>&1 || true
  sleep 0.5
  setsid "$BIN_DIR/puntu-gui" >/dev/null 2>&1 < /dev/null &
fi

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

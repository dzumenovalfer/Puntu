#!/usr/bin/env bash
# End-to-end test of the IBus engine, without IBus.
#
#   tests/e2e/run.sh                      # the debug build in target/, every scenario
#   tests/e2e/run.sh <puntu-ibus> [set]   # any binary — e.g. the installed one, to compare;
#                                         # set: typing|caps|keys|secret|surrounding|typos|order|all
#
# What the unit tests cannot reach is the key-event handler itself: it needs a D-Bus
# connection to emit on. So this starts a PRIVATE `dbus-daemon`, points the engine at it
# through IBUS_ADDRESS, and lets driver.py play ibus-daemon and a text field. The session's
# real IBus is never touched — the engine under test runs with an empty environment, its own
# config/state directories and an empty PATH (no notify-send, zenity or wl-paste).
#
# Needs: dbus-daemon, python3 with PyGObject (python3-gi). Not part of `cargo test` for that
# reason. Build first:  cargo build --no-default-features --features ibus
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
BIN="${1:-$HERE/../../target/debug/puntu-ibus}"
WHICH="${2:-all}"
[[ -x "$BIN" ]] || { echo "no engine binary at $BIN — build it first (see the header)"; exit 2; }
BIN="$(readlink -f "$BIN")"
for tool in dbus-daemon python3; do
  command -v "$tool" >/dev/null 2>&1 || { echo "$tool is required"; exit 2; }
done

W="$(mktemp -d)"
DPID=""; EPID=""
cleanup() {
  [[ -n "$EPID" ]] && kill "$EPID" 2>/dev/null
  [[ -n "$DPID" ]] && kill "$DPID" 2>/dev/null
  rm -rf "$W"
}
trap cleanup EXIT
mkdir -p "$W/cfg/puntu" "$W/state" "$W/home" "$W/emptybin"
# No idle-commit timer (it would make the checks depend on timing); debug log for post-mortems;
# typo fixing on, so its scenarios have something to check.
printf 'hold_commit_ms = 0\ndebug_log = true\nfix_typos = true\n' > "$W/cfg/puntu/config.toml"

# The typo corrector only works with the big dictionary, so build one for the test config —
# with the `puntu` CLI that sits next to the engine under test. Without it (or without the
# word list) the typo scenarios are skipped; everything else does not need the dictionary.
CLI="$(dirname "$BIN")/puntu"
WORDS="$HERE/../../dictionaries/russian.utf-8"
export PUNTU_E2E_TYPOS=0
if [[ -x "$CLI" && -f "$WORDS" ]] \
   && env -i HOME="$W/home" XDG_CONFIG_HOME="$W/cfg" "$CLI" build-dict "$WORDS" \
        "$W/cfg/puntu/russian.fst" >/dev/null 2>&1; then
  PUNTU_E2E_TYPOS=1
fi

# An abstract socket: a path under $W could exceed the 108 bytes a unix socket name may have.
dbus-daemon --session --nofork --address="unix:abstract=puntu-e2e-$$" --print-address=3 \
  3> "$W/addr" &
DPID=$!
for _ in $(seq 100); do [[ -s "$W/addr" ]] && break; sleep 0.05; done
ADDR="$(head -n1 "$W/addr")"
[[ -n "$ADDR" ]] || { echo "the private dbus-daemon did not start"; exit 2; }

env -i HOME="$W/home" XDG_CONFIG_HOME="$W/cfg" XDG_STATE_HOME="$W/state" \
    IBUS_ADDRESS="$ADDR" PATH="$W/emptybin" "$BIN" > "$W/engine.out" 2>&1 &
EPID=$!

python3 -I "$HERE/driver.py" "$ADDR" "$WHICH"
RC=$?

# The engine must notice its bus going away and exit by itself: one that lives on after an
# ibus-daemon crash is the orphan `puntu-ibus doctor` reports as a second engine.
kill "$DPID" 2>/dev/null; wait "$DPID" 2>/dev/null; DPID=""
for _ in $(seq 60); do kill -0 "$EPID" 2>/dev/null || break; sleep 0.05; done
if kill -0 "$EPID" 2>/dev/null; then
  echo "  FAIL the engine is still running 3 s after its bus went away"
  RC=1
else
  echo "  ok   the engine exited by itself once its bus was gone"
  EPID=""
fi

if [[ "$RC" -ne 0 ]]; then
  KEEP="$(mktemp -t puntu-e2e-engine-XXXXXX.log)"
  cp "$W/state/puntu/engine.log" "$KEEP" 2>/dev/null && echo "engine log kept at $KEEP"
fi
exit "$RC"

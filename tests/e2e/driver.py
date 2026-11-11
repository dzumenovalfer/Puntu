#!/usr/bin/env python3
"""End-to-end driver for the IBus engine — see run.sh, which starts everything this needs.

Plays both parts the engine normally talks to: **ibus-daemon** (CreateEngine, FocusInId,
SetCapabilities, the ContentType property, ProcessKeyEvent …) and **a text field** (it applies
CommitText / UpdatePreeditText / ForwardKeyEvent, and inserts the character itself whenever the
engine passes a key on). Each check then compares what would be on screen with what should be.

Everything goes over a private `dbus-daemon`, never the session's IBus, so it is safe to run
while Puntu is your active input method.

Usage: driver.py <bus-address> [typing|caps|keys|secret|surrounding|order|all]
"""
import sys, time
from gi.repository import Gio, GLib

ADDR = sys.argv[1]
WHICH = sys.argv[2] if len(sys.argv) > 2 else "all"

BUS = "org.freedesktop.IBus.Puntu"
ENGINE_IF = "org.freedesktop.IBus.Engine"
RELEASE = 1 << 30
LOCK = 1 << 1
SHIFT = 1 << 0
CTRL = 1 << 2

conn = Gio.DBusConnection.new_for_address_sync(
    ADDR,
    Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
    None, None)

# wait for the engine to own its name
for _ in range(100):
    try:
        r = conn.call_sync("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
                           "NameHasOwner", GLib.Variant("(s)", (BUS,)), None, 0, 2000, None)
        if r.unpack()[0]:
            break
    except Exception:
        pass
    time.sleep(0.05)
else:
    print("engine did not appear on the bus"); sys.exit(2)

path = conn.call_sync(BUS, "/org/freedesktop/IBus/Factory", "org.freedesktop.IBus.Factory",
                      "CreateEngine", GLib.Variant("(s)", ("puntu",)), None, 0, 5000, None).unpack()[0]

ctx = GLib.MainContext.default()


class Field:
    """A text field as an app would keep it: committed text + the preedit shown after it."""

    def __init__(self):
        self.text = ""
        self.preedit = ""
        self.hints = []
        self.events = []

    def screen(self):
        return self.text + self.preedit


field = Field()


def ibus_text(v):
    # v is a variant holding (sa{sv}sv); the string is field 2
    return v.unpack()[2] if isinstance(v.unpack(), tuple) else v.get_child_value(2).get_string()


def on_signal(_c, _sender, _path, _iface, name, params):
    if name == "CommitText":
        t = params.get_child_value(0).get_variant().get_child_value(2).get_string()
        field.text += t
        field.events.append(("commit", t))
    elif name == "UpdatePreeditText":
        t = params.get_child_value(0).get_variant().get_child_value(2).get_string()
        visible = params.get_child_value(2).get_boolean()
        field.preedit = t if visible else ""
        field.events.append(("preedit", field.preedit))
    elif name == "ForwardKeyEvent":
        keyval, _keycode, state = params.unpack()
        if keyval == 0xff08 and not (state & RELEASE):
            field.text = field.text[:-1]
        field.events.append(("forward", keyval, state))
    elif name == "UpdateAuxiliaryText":
        t = params.get_child_value(0).get_variant().get_child_value(2).get_string()
        if params.get_child_value(1).get_boolean():
            field.hints.append(t)


conn.signal_subscribe(None, ENGINE_IF, None, path, None, Gio.DBusSignalFlags.NONE, on_signal)


def pump(ms=0):
    end = time.time() + ms / 1000.0
    while True:
        while ctx.iteration(False):
            pass
        if time.time() >= end:
            break
        time.sleep(0.002)


def call(method, sig=None, args=None, timeout=5000):
    params = GLib.Variant(sig, args) if sig else None
    r = conn.call_sync(BUS, path, ENGINE_IF, method, params, None, 0, timeout, None)
    pump()
    return r.unpack() if r is not None else None


def call_async(method, sig=None, args=None):
    """Fire and forget — exactly how ibus-daemon sends everything but ProcessKeyEvent."""
    params = GLib.Variant(sig, args) if sig else None
    conn.call(BUS, path, ENGINE_IF, method, params, None, 0, 5000, None, None, None)


def set_content_type(purpose, hints=0, sync=True):
    params = GLib.Variant("(ssv)", (ENGINE_IF, "ContentType", GLib.Variant("(uu)", (purpose, hints))))
    if sync:
        conn.call_sync(BUS, path, "org.freedesktop.DBus.Properties", "Set", params, None, 0, 5000, None)
        pump()
    else:
        conn.call(BUS, path, "org.freedesktop.DBus.Properties", "Set", params, None, 0, 5000, None, None, None)


KEYSYMS = {
    "BackSpace": 0xff08, "Return": 0xff0d, "Tab": 0xff09, "Escape": 0xff1b, "Left": 0xff51,
    "End": 0xff57, "F5": 0xffc2, "Control_L": 0xffe3, "Shift_L": 0xffe1, "Caps_Lock": 0xffe5,
    "grave": 0x60,
}


def press(keyval, state=0, char=None):
    """One key press + release, with the app's own reaction when the engine passes the key on."""
    handled = call("ProcessKeyEvent", "(uuu)", (keyval, 0, state))[0]
    if not handled:
        if keyval == KEYSYMS["BackSpace"]:
            field.text = field.text[:-1]
        elif keyval == KEYSYMS["Return"]:
            field.text += "\n"
        elif keyval == KEYSYMS["Tab"]:
            field.text += "\t"
        elif char is not None and not (state & CTRL):
            field.text += char
    call("ProcessKeyEvent", "(uuu)", (keyval, 0, state | RELEASE))
    return handled


def type_text(s, state=0):
    """Type printable ASCII as the US layout would deliver it (keysym == code point)."""
    for ch in s:
        if ch == "\b":
            press(KEYSYMS["BackSpace"], state)
        elif ch == "\n":
            press(KEYSYMS["Return"], state)
        else:
            st = state
            if ch.isupper() and not (state & LOCK):
                st |= SHIFT
            press(ord(ch), st, ch)


def tap_ctrl():
    call("ProcessKeyEvent", "(uuu)", (KEYSYMS["Control_L"], 0, 0))
    call("ProcessKeyEvent", "(uuu)", (KEYSYMS["Control_L"], 0, CTRL | RELEASE))


def focus(client="e2e", ctx_path="/org/freedesktop/IBus/InputContext_e2e", caps=0x29):
    call("FocusInId", "(ss)", (ctx_path, client))
    call("SetCapabilities", "(u)", (caps,))


def blur(ctx_path="/org/freedesktop/IBus/InputContext_e2e"):
    call("FocusOutId", "(s)", (ctx_path,))


def fresh(mode_ru=False):
    """New empty field, engine in a known state (EN auto-correct unless mode_ru)."""
    global field
    blur()
    call("Reset")
    field.__init__()
    set_content_type(0, 0)
    focus()
    # make sure we are in Correcting mode: the engine has no query, so track it ourselves
    global RU
    if RU != mode_ru:
        tap_ctrl()
        RU = mode_ru
    field.__init__()


RU = False
results = []


def check(name, got, want):
    ok = got == want
    results.append(ok)
    print(("  ok   " if ok else "  FAIL ") + name)
    if not ok:
        print("         got:  %r\n         want: %r" % (got, want))


def scenario_typing():
    print("== typing ==")
    fresh()
    type_text("ghbdtn ")
    check("wrong-layout word converts (held in preedit)", field.screen(), "привет ")
    type_text("hello ")
    check("…and is committed when the next word starts", (field.text, field.preedit), ("привет ", "hello "))

    fresh()
    type_text("walkes \b\bd ")
    check("retyped ending stays as typed (walkes→walked)", field.screen(), "walked ")

    fresh()
    type_text("thw \b\be ")
    check("retyped last letter stays Latin (thw→the)", field.screen(), "the ")

    fresh()
    type_text("words \b\b\bds ")
    check("retyped two-letter ending (words)", field.screen(), "words ")

    fresh()
    type_text("5th 1st 2nd 3rd ")
    check("ordinals keep their endings", field.screen(), "5th 1st 2nd 3rd ")

    fresh()
    type_text("see thw \b\b\b\bns ")
    check("a word deleted whole and retyped IS a word again (ns→ты)", field.screen(), "see ты ")

    fresh()
    type_text("TV UK 16 GB 10 km ")
    check("two-letter abbreviations are left alone", field.screen(), "TV UK 16 GB 10 km ")

    fresh()
    type_text("ghbdtn/ rfr ltkf& ")
    check("punctuation follows the converted word", field.screen(), "привет. как дела? ")

    fresh()
    type_text("ghbdtn? rfr ltkf/ ")
    check("Russian comma and full stop", field.screen(), "привет, как дела. ")

    fresh()
    type_text("and/or what? ")
    check("English punctuation untouched", field.screen(), "and/or what? ")

    fresh()
    type_text("ghbdtn\n")
    check("Enter commits the converted word and reaches the app", field.screen(), "привет\n")


def scenario_caps():
    print("== CapsLock ==")
    fresh(mode_ru=True)
    type_text("DF;YJ ", state=LOCK)
    check("RU-direct, CapsLock: ВАЖНО", field.screen(), "ВАЖНО ")
    fresh(mode_ru=True)
    type_text("'NJ ", state=LOCK)
    check("RU-direct, CapsLock: ЭТО is not 'fixed' into Это", field.screen(), "ЭТО ")
    fresh(mode_ru=False)
    type_text("DF;YJ ", state=LOCK)
    check("auto-correct, CapsLock: DF;YJ → ВАЖНО", field.screen(), "ВАЖНО ")
    fresh(mode_ru=False)
    type_text("IT'S ", state=LOCK)
    check("auto-correct, CapsLock: English stays as typed", field.screen(), "IT'S ")


def scenario_keys():
    print("== function keys / fragments after a mode toggle ==")
    fresh()
    type_text("hello")
    press(KEYSYMS["F5"])
    check("F5 commits the word in progress first", (field.text, field.preedit), ("hello", ""))

    fresh()
    type_text("ghb")
    press(KEYSYMS["Left"])
    check("an arrow key commits the fragment as typed", (field.text, field.preedit), ("ghb", ""))


def scenario_secret():
    print("== password field ==")
    fresh()
    set_content_type(8, 0)
    type_text("gfhjkm\n")
    check("password goes straight through", (field.text, field.preedit), ("gfhjkm\n", ""))
    # ContentType handled BEFORE the FocusIn that it was sent after (the write is its own task)
    fresh()
    blur()
    set_content_type(8, 0)
    focus()
    field.__init__()
    type_text("gfhjkm ")
    check("…also when ContentType was handled before FocusIn", (field.text, field.preedit), ("gfhjkm ", ""))
    # second password field: the daemon sends nothing new
    blur()
    focus(ctx_path="/org/freedesktop/IBus/InputContext_other")
    field.__init__()
    type_text("gfhjkm ")
    check("…and in a second password field (no new ContentType)", (field.text, field.preedit), ("gfhjkm ", ""))
    blur(ctx_path="/org/freedesktop/IBus/InputContext_other")
    set_content_type(0, 0)
    focus()


def scenario_surrounding():
    print("== what the client reports before the caret ==")
    def report(text, cursor):
        it = GLib.Variant("(sa{sv}sv)", ("IBusText", {}, text,
                                            GLib.Variant("(sa{sv}av)", ("IBusAttrList", {}, []))))
        call("SetSurroundingText", "(vuu)", (it, cursor, cursor))
    fresh()
    call("Reset")                       # a click …
    report("walke", 5)                  # … at the end of an existing word
    field.__init__(); field.text = "walke"
    type_text("d ")
    check("click at the end of a word, then a letter: a fragment", field.screen(), "walked ")
    fresh()
    call("Reset")
    report("walke ", 6)                 # after a space: a word of its own
    field.__init__(); field.text = "walke "
    type_text("d ")
    check("click after a space, then a letter: a word (d→в)", field.screen(), "walke в ")


def scenario_order(n=300):
    """Fire FocusOutId/FocusInId/SetCapabilities bursts without waiting, like ibus-daemon does,
    then check which client the engine believes it is talking to."""
    print("== order of calls ==")
    bad = 0
    for i in range(n):
        old = "/ctx/%d" % i
        new = "/ctx/%d" % (i + 1)
        # daemon order: FocusOutId(old) → FocusInId(new) → SetCapabilities
        call_async("FocusOutId", "(s)", (old,))
        call_async("FocusInId", "(ss)", (new, "SDL2_Application" if i % 2 else "gtk3-im:app"))
        call_async("SetCapabilities", "(u)", (0x29,))
        # a key: the engine must be transparent exactly for the SDL client
        handled = call("ProcessKeyEvent", "(uuu)", (ord("w"), 0, 0))[0]
        call("ProcessKeyEvent", "(uuu)", (ord("w"), 0, RELEASE))
        expect_handled = (i % 2 == 0)
        if handled != expect_handled:
            bad += 1
        # leave things clean for the next round
        if handled:
            call("Reset")
    print("  %d bursts, %d handled with the WRONG client state" % (n, bad))
    results.append(bad == 0)
    print(("  ok   " if bad == 0 else "  FAIL ") + "client state always matches the last FocusIn")
    call_async("FocusOutId", "(s)", ("/ctx/%d" % n,))
    pump(50)


SETS = {
    "typing": scenario_typing,
    "caps": scenario_caps,
    "keys": scenario_keys,
    "secret": scenario_secret,
    "surrounding": scenario_surrounding,
    "order": scenario_order,
}
focus()
for name, fn in SETS.items():
    if WHICH in ("all", name):
        fn()
print("\n%d checks, %d failed" % (len(results), results.count(False)))
sys.exit(1 if False in results else 0)

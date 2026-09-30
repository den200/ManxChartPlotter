#!/usr/bin/env python3
"""Virtual touchscreen for the Pi rig, which has none: a uinput multitouch
device that the compositor hands to Manx as real touch.

    sudo python3 ~/manx/deploy/pi-touch.py swipe x0 y0 x1 y1 [ms]
    sudo python3 ~/manx/deploy/pi-touch.py tap x y

Needs python3-evdev (apt).
Screen coordinates on the 1920x1080 output."""
import sys, time
from evdev import UInput, AbsInfo, ecodes as e

W, H = 1920, 1080
caps = {
    e.EV_KEY: [e.BTN_TOUCH],
    e.EV_ABS: [
        (e.ABS_X, AbsInfo(0, 0, W - 1, 0, 0, 0)),
        (e.ABS_Y, AbsInfo(0, 0, H - 1, 0, 0, 0)),
        (e.ABS_MT_SLOT, AbsInfo(0, 0, 9, 0, 0, 0)),
        (e.ABS_MT_TRACKING_ID, AbsInfo(0, 0, 65535, 0, 0, 0)),
        (e.ABS_MT_POSITION_X, AbsInfo(0, 0, W - 1, 0, 0, 0)),
        (e.ABS_MT_POSITION_Y, AbsInfo(0, 0, H - 1, 0, 0, 0)),
    ],
}
ui = UInput(caps, name="manx-test-touch", input_props=[e.INPUT_PROP_DIRECT])
time.sleep(1.5)  # let the compositor pick the device up

def at(x, y):
    ui.write(e.EV_ABS, e.ABS_MT_POSITION_X, int(x)); ui.write(e.EV_ABS, e.ABS_MT_POSITION_Y, int(y))
    ui.write(e.EV_ABS, e.ABS_X, int(x)); ui.write(e.EV_ABS, e.ABS_Y, int(y))

def down(x, y):
    ui.write(e.EV_ABS, e.ABS_MT_SLOT, 0); ui.write(e.EV_ABS, e.ABS_MT_TRACKING_ID, 42)
    at(x, y); ui.write(e.EV_KEY, e.BTN_TOUCH, 1); ui.syn()

def up():
    ui.write(e.EV_ABS, e.ABS_MT_TRACKING_ID, -1); ui.write(e.EV_KEY, e.BTN_TOUCH, 0); ui.syn()

cmd = sys.argv[1]
if cmd == "swipe":
    x0, y0, x1, y1 = map(float, sys.argv[2:6])
    ms = float(sys.argv[6]) if len(sys.argv) > 6 else 300
    steps = 20
    down(x0, y0)
    for i in range(1, steps + 1):
        time.sleep(ms / 1000 / steps)
        at(x0 + (x1 - x0) * i / steps, y0 + (y1 - y0) * i / steps); ui.syn()
    time.sleep(0.03)
    up()
elif cmd == "tap":
    x, y = map(float, sys.argv[2:4])
    down(x, y); time.sleep(0.08); up()
time.sleep(0.5)
ui.close()

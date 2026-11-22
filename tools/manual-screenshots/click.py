#!/usr/bin/env python3
"""click.py <window-id> <x> <y> : left-click at window-relative x,y via XTest."""
import sys, time
from Xlib import X, display
from Xlib.ext import xtest
d = display.Display()
w = d.create_resource_object('window', int(sys.argv[1], 16))
g = w.get_geometry(); t = w.translate_coords(d.screen().root, 0, 0)
ax, ay = -t.x + int(sys.argv[2]), -t.y + int(sys.argv[3])
xtest.fake_input(d, X.MotionNotify, x=ax, y=ay); d.sync(); time.sleep(0.15)
xtest.fake_input(d, X.ButtonPress, 1); d.sync(); time.sleep(0.08)
xtest.fake_input(d, X.ButtonRelease, 1); d.sync()
print(f"clicked root {ax},{ay}")

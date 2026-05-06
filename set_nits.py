#!/usr/bin/env python3
"""
set_nits.py — 5×5 cm (200×200 px) pure-white tkinter window that doubles as
a calibrated light emitter for the MacBook Pro 13" 2019 internal panel.

Type a target nits value (0–500) into the entry at the bottom and hit
Return (or click "Set"). The script:
  1. Clamps the value to [0, 500].
  2. Inverts the panel's perceptual brightness curve (gamma 2.2 by default,
     or the piecewise sRGB EOTF) to obtain the 0.0–1.0 slider value that
     the macOS backlight API expects.
  3. Calls the private `DisplayServicesSetBrightness` symbol via ctypes,
     which is the same backend that drives the menu-bar slider and the
     F1/F2 keys.

The whole window is painted #FFFFFF so that every visible pixel passes
100% of the backlight — the displayed luminance therefore matches the
panel's physical output. The entry and "Set" button are kept tiny and
docked to the bottom edge so they don't shadow the measurement area.
"""

from __future__ import annotations

import ctypes
import shutil
import subprocess
import sys
import tkinter as tk

PANEL_MAX_NITS = 500.0          # MBP 13" 2019 internal display rating
DEFAULT_GAMMA = 2.2             # perceptual curve exponent
CURVE = "gamma"                 # "gamma" or "srgb"

WHITE = "#FFFFFF"
SUBTLE = "#BBBBBB"              # entry/button text — light grey, low contrast
ERROR = "#CC4444"


# ---------------------------------------------------------------------------
# Nits → 0..1 slider value
# ---------------------------------------------------------------------------

def nits_to_slider_gamma(nits: float, max_nits: float, gamma: float) -> float:
    """Inverse of L = L_max * s^gamma  →  s = (L/L_max)^(1/gamma)."""
    ratio = max(0.0, nits / max_nits)
    return ratio ** (1.0 / gamma) if ratio > 0 else 0.0


def nits_to_slider_srgb(nits: float, max_nits: float) -> float:
    """Inverse of the piecewise sRGB EOTF."""
    y = max(0.0, nits / max_nits)
    if y <= 0.0031308:
        return 12.92 * y
    return 1.055 * (y ** (1.0 / 2.4)) - 0.055


def nits_to_slider(nits: float) -> float:
    if CURVE == "srgb":
        s = nits_to_slider_srgb(nits, PANEL_MAX_NITS)
    else:
        s = nits_to_slider_gamma(nits, PANEL_MAX_NITS, DEFAULT_GAMMA)
    return max(0.0, min(1.0, s))


# ---------------------------------------------------------------------------
# Backlight backends
# ---------------------------------------------------------------------------

def set_brightness_displayservices(value: float) -> tuple[bool, str]:
    """Call the private DisplayServices framework via ctypes."""
    try:
        cg = ctypes.CDLL(
            "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics"
        )
        ds = ctypes.CDLL(
            "/System/Library/PrivateFrameworks/"
            "DisplayServices.framework/DisplayServices"
        )
    except OSError as exc:
        return False, f"framework load failed: {exc}"

    cg.CGMainDisplayID.restype = ctypes.c_uint32
    display_id = cg.CGMainDisplayID()

    ds.DisplayServicesSetBrightness.argtypes = [ctypes.c_uint32, ctypes.c_float]
    ds.DisplayServicesSetBrightness.restype = ctypes.c_int

    rc = ds.DisplayServicesSetBrightness(display_id, ctypes.c_float(value))
    if rc != 0:
        return False, f"DisplayServicesSetBrightness rc={rc}"
    return True, "DisplayServices"


def set_brightness_cli(value: float) -> tuple[bool, str]:
    exe = shutil.which("brightness")
    if exe is None:
        return False, "`brightness` CLI not installed"
    try:
        subprocess.run([exe, f"{value:.6f}"], check=True,
                       capture_output=True, text=True)
        return True, "brightness CLI"
    except subprocess.CalledProcessError as exc:
        return False, f"brightness CLI failed: {exc}"


def set_brightness(value: float) -> tuple[bool, str]:
    ok, info = set_brightness_displayservices(value)
    if ok:
        return ok, info
    return set_brightness_cli(value)


# ---------------------------------------------------------------------------
# GUI
# ---------------------------------------------------------------------------

class NitsEmitter(tk.Tk):
    def __init__(self) -> None:
        super().__init__()
        self.title("nits")
        self.geometry("200x200")
        self.resizable(False, False)
        self.configure(bg=WHITE)
        self.attributes("-topmost", True)
        try:
            self.tk.call("::tk::unsupported::MacWindowStyle", "style",
                         self._w, "moveableModal", "")
        except tk.TclError:
            pass  # non-mac or unsupported tk build

        # The whole window is white. The entry + button live in a thin strip
        # at the bottom so the upper ~85% of the panel is unbroken white.
        strip = tk.Frame(self, bg=WHITE, height=22)
        strip.pack(side="bottom", fill="x", pady=(0, 4))
        strip.pack_propagate(False)

        self.entry = tk.Entry(
            strip, width=5, justify="center",
            bd=0, relief="flat", highlightthickness=0,
            bg=WHITE, fg=SUBTLE, insertbackground=SUBTLE,
            font=("Helvetica", 11),
        )
        self.entry.pack(side="left", padx=(60, 4))
        self.entry.bind("<Return>", self.on_submit)
        self.entry.bind("<KP_Enter>", self.on_submit)
        self.entry.focus_set()

        self.set_btn = tk.Label(
            strip, text="Set", bg=WHITE, fg=SUBTLE,
            font=("Helvetica", 10), cursor="hand2",
        )
        self.set_btn.pack(side="left")
        self.set_btn.bind("<Button-1>", self.on_submit)

        # status line, kept the same colour as the background by default so
        # it is invisible until something needs to be reported.
        self.status = tk.Label(
            self, text="", bg=WHITE, fg=WHITE,
            font=("Helvetica", 9),
        )
        self.status.pack(side="bottom")

        # Esc closes — handy when the window is on top of everything.
        self.bind("<Escape>", lambda _e: self.destroy())

    # ----- event handlers --------------------------------------------------

    def on_submit(self, _event: object = None) -> None:
        raw = self.entry.get().strip()
        if not raw:
            return
        try:
            nits = float(raw)
        except ValueError:
            self.flash_status(f"bad number: {raw!r}", error=True)
            return

        clamped = max(0.0, min(PANEL_MAX_NITS, nits))
        slider = nits_to_slider(clamped)
        ok, info = set_brightness(slider)

        if not ok:
            self.flash_status(info, error=True)
            return

        msg = f"{clamped:g} nits → s={slider:.3f}"
        if clamped != nits:
            msg += "  (clamped)"
        self.flash_status(msg, error=False)

    def flash_status(self, msg: str, *, error: bool) -> None:
        self.status.configure(text=msg, fg=ERROR if error else SUBTLE)
        # Fade the status back into the white background after a moment so
        # the emitter stays clean.
        self.after(1800, lambda: self.status.configure(fg=WHITE))


def main() -> int:
    if sys.platform != "darwin":
        print("warning: this tool targets macOS; the brightness call will "
              "almost certainly fail elsewhere.", file=sys.stderr)
    NitsEmitter().mainloop()
    return 0


if __name__ == "__main__":
    sys.exit(main())

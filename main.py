"""BPW34 + MCP6004 transimpedance amplifier — DC sweep of photocurrent.

Topology:
    - MCP6004 op-amp:
        Non-inverting input (+) -> GND
        Inverting input (-)     -> N_INV (summing junction)
        Output                  -> VOUT
        V+ supply               -> VDD (3.3 V)
        V- supply               -> GND
    - BPW34 photodiode (photovoltaic mode):
        Anode   -> GND
        Cathode -> N_INV
        Photocurrent modeled as an ideal current source in parallel with the
        diode, flowing from cathode (N_INV) through the source to anode (GND).
    - Feedback network between N_INV and VOUT:
        R_F (swept: 1 MΩ, 100 kΩ, 10 kΩ, 1 kΩ) in parallel with C_F = 100 nF.

For positive photocurrent the source pulls charge out of N_INV, so the op-amp
sources current back through R_F and VOUT = +I_photo * R_F (until it clips at
the supply rails). C_F is open at DC and has no effect on this sweep.
"""

import os
from pathlib import Path

# Debian/Ubuntu's libngspice0 only ships the SONAME-versioned libngspice.so.0; the
# unversioned symlink belongs to libngspice0-dev. PySpice's cffi loader hard-codes
# dlopen("libngspice.so"), so point it at the versioned file via NGSPICE_LIBRARY_PATH.
if "NGSPICE_LIBRARY_PATH" not in os.environ:
    for _candidate in (
        "/usr/lib/x86_64-linux-gnu/libngspice.so.0",
        "/usr/lib/libngspice.so.0",
        "/usr/local/lib/libngspice.so.0",
    ):
        if Path(_candidate).exists():
            os.environ["NGSPICE_LIBRARY_PATH"] = _candidate
            break

import matplotlib

matplotlib.use("Agg")  # headless save-to-file
import matplotlib.pyplot as plt
import numpy as np

import PySpice.Logging.Logging as Logging
from PySpice.Spice.Netlist import Circuit
from PySpice.Spice.NgSpice.Shared import NgSpiceShared, NgSpiceCommandError

# PySpice 1.5 mis-flags any non-"Warning:" line on ngspice stderr as a fatal
# error — including informational "Note: ... gmin stepping" messages emitted
# during normal DC operating-point convergence. Patch exec_command to swallow
# the spurious failure if every stderr line is a benign Note/step/completion.
_orig_exec_command = NgSpiceShared.exec_command


def _exec_command_lenient(self, command, join_lines=True):
    try:
        return _orig_exec_command(self, command, join_lines=join_lines)
    except NgSpiceCommandError:
        stderr_lines = list(self._stderr)
        benign = stderr_lines and all(
            line.startswith("Note:")
            or "gmin" in line
            or "step" in line.lower()
            or "completed" in line.lower()
            for line in stderr_lines
        )
        if not benign:
            raise
        return self.stdout if join_lines else self._stdout


NgSpiceShared.exec_command = _exec_command_lenient

logger = Logging.setup_logging(logging_level="WARNING")

ROOT = Path(__file__).resolve().parent
SPICE_MODELS_DIR = ROOT / "spice_models"
MCP6001_LIB = SPICE_MODELS_DIR / "MCP6001.lib"
BPW34_LIB = SPICE_MODELS_DIR / "BPW34.lib"

VDD = 3.3
C_F = 100e-9  # 100 nF (104)

R_F_VALUES = [
    ("1 MΩ", 1e6),
    ("100 kΩ", 100e3),
    ("10 kΩ", 10e3),
    ("1 kΩ", 1e3),
]

I_PHOTO_START = 0.0
I_PHOTO_STOP = 50e-6  # 50 µA
I_PHOTO_STEP = 0.25e-6  # 0.25 µA -> 201 points


def build_tia(r_f: float) -> Circuit:
    circuit = Circuit("BPW34_MCP6004_TIA")
    circuit.include(str(MCP6001_LIB))
    circuit.include(str(BPW34_LIB))

    circuit.V("DD", "VDD", circuit.gnd, VDD)

    # MCP6001 macromodel pin order: 1=NonInv 2=Inv 3=V+ 4=V- 5=Output
    # (one macromodel covers MCP6001/2/4 — channel count differs only at the package).
    circuit.X("U1", "MCP6001",
              circuit.gnd, "N_INV", "VDD", circuit.gnd, "VOUT")

    # BPW34: SPICE diode primitive, pin order is anode then cathode.
    circuit.D("1", circuit.gnd, "N_INV", model="BPW34")

    # Photocurrent source: + at cathode, - at anode -> conventional current
    # flows from N_INV through the source to GND, i.e. cathode->anode.
    circuit.I("photo", "N_INV", circuit.gnd, 0)

    circuit.R("F", "N_INV", "VOUT", r_f)
    circuit.C("F", "N_INV", "VOUT", C_F)

    return circuit


def simulate(r_f: float):
    circuit = build_tia(r_f)
    simulator = circuit.simulator(temperature=25, nominal_temperature=25)
    # The Microchip macromodel uses PSpice POLY/TABLE/TC syntax. ngspice
    # needs PSpice compatibility set as a control command BEFORE the netlist
    # is parsed; ".options ngbehavior=ps" lands too late at the end of the deck.
    NgSpiceShared.new_instance().exec_command("set ngbehavior=ps")
    analysis = simulator.dc(
        Iphoto=slice(I_PHOTO_START, I_PHOTO_STOP, I_PHOTO_STEP)
    )
    i_photo = np.array(analysis.sweep, dtype=float)
    vout = np.array(analysis["vout"], dtype=float)
    return i_photo, vout


def main() -> None:
    fig, ax = plt.subplots(figsize=(9, 6))

    for label, r_f in R_F_VALUES:
        print(f"Simulating R_F = {label} ...")
        i_photo, vout = simulate(r_f)
        ax.plot(i_photo * 1e6, vout, label=f"R_F = {label}", linewidth=1.6)

    ax.axhline(VDD, color="gray", linestyle="--", linewidth=0.8,
               label=f"VDD rail = {VDD} V")
    ax.axhline(0, color="gray", linestyle=":", linewidth=0.8,
               label="GND rail = 0 V")

    ax.set_xlabel("Photocurrent  I_photo  (µA)")
    ax.set_ylabel("VOUT (V)")
    ax.set_title("BPW34 + MCP6004 TIA — VOUT vs. Photocurrent")
    ax.set_xlim(I_PHOTO_START * 1e6, I_PHOTO_STOP * 1e6)
    ax.set_ylim(-0.2, VDD + 0.3)
    ax.grid(True, alpha=0.3)
    ax.legend(loc="best")

    out_path = ROOT / "tia_sweep.png"
    fig.tight_layout()
    fig.savefig(out_path, dpi=140)
    print(f"\nSaved plot to {out_path}")


if __name__ == "__main__":
    main()

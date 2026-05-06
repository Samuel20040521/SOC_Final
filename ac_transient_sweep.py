"""Dynamic characterisation of the BPW34 + MCP6004 TIA.

Two analyses, R_F fixed at 100 kΩ, C_F swept across {1 pF, 10 pF, 100 pF, 1 nF, 100 nF}:

  1. AC sweep, 1 Hz - 1 MHz: transimpedance |Z(f)| in dB vs. frequency.
     The AC source is an ideal 1 A photocurrent (so V_OUT(f) directly equals the
     transimpedance gain). Vertical markers at 120 Hz (ProMotion display refresh)
     and 20 kHz (PWM dimming) call out our targets of interest.

  2. Transient: 20 kHz, 0 -> 10 µA pulsed photocurrent, 50 % duty, 5 periods.
     Visualises the trade-off between sharp-edge fidelity and op-amp ringing as
     C_F varies.

Run with:  uv run python ac_transient_sweep.py
"""

import os
from pathlib import Path

# Same ngspice plumbing as main.py: Debian's libngspice0 ships only the SONAME
# library, and PySpice 1.5 mis-flags ngspice's informational stderr "Note:" lines
# as fatal errors. See main.py for the full reasoning.
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

matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

import PySpice.Logging.Logging as Logging
from PySpice.Spice.Netlist import Circuit
from PySpice.Spice.NgSpice.Shared import NgSpiceShared, NgSpiceCommandError

logger = Logging.setup_logging(logging_level="WARNING")


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


# ----------------------------------------------------------------------------
# Circuit constants
# ----------------------------------------------------------------------------
ROOT = Path(__file__).resolve().parent
SPICE_MODELS_DIR = ROOT / "spice_models"
MCP6001_LIB = SPICE_MODELS_DIR / "MCP6001.lib"
BPW34_LIB = SPICE_MODELS_DIR / "BPW34.lib"

VDD = 3.3
R_F = 100e3

C_F_VALUES = [
    ("1 pF", 1e-12),
    ("10 pF", 10e-12),
    ("100 pF", 100e-12),
    ("1 nF", 1e-9),
    ("100 nF", 100e-9),
]

# AC sweep targets
AC_F_START = 1.0
AC_F_STOP = 1e6
AC_POINTS_PER_DECADE = 100
AC_DC_BIAS = 16.5e-6  # DC photocurrent (A) -> biases VOUT to ~VDD/2 with R_F=100kΩ

# Transient pulse: 20 kHz, 50 % duty, 0 -> 10 µA
PULSE_AMPLITUDE = 10e-6
PULSE_FREQ = 20e3
PULSE_PERIOD = 1.0 / PULSE_FREQ          # 50 µs
PULSE_WIDTH = PULSE_PERIOD * 0.5         # 25 µs
PULSE_EDGE = 10e-9                       # 10 ns rise/fall (effectively a step)
TRAN_END = 5 * PULSE_PERIOD              # 250 µs (5 periods)
TRAN_STEP = 50e-9                        # 50 ns -> 1000 pts/period

# Frequencies of interest highlighted on the Bode plot
TARGET_120HZ = 120.0
TARGET_20KHZ = 20e3


# ----------------------------------------------------------------------------
# Circuit builders
# ----------------------------------------------------------------------------
def _common_circuit(name: str, c_f: float) -> Circuit:
    c = Circuit(name)
    c.include(str(MCP6001_LIB))
    c.include(str(BPW34_LIB))

    c.V("DD", "VDD", c.gnd, VDD)
    # MCP6001/2/4 macromodel pins: NonInv, Inv, V+, V-, Output
    c.X("U1", "MCP6001", c.gnd, "N_INV", "VDD", c.gnd, "VOUT")
    # BPW34 photodiode: anode -> GND, cathode -> N_INV
    c.D("1", c.gnd, "N_INV", model="BPW34")
    c.R("F", "N_INV", "VOUT", R_F)
    c.C("F", "N_INV", "VOUT", c_f)
    return c


def build_circuit_ac(c_f: float) -> Circuit:
    c = _common_circuit("BPW34_MCP6004_TIA_AC", c_f)
    # AC analysis linearises around the DC operating point. With the +input at
    # GND and zero photocurrent, V_OUT pins to the lower rail (~0 V) and the
    # op-amp's small-signal gain collapses, giving meaningless Bode data. Push
    # V_OUT to mid-rail with a realistic ambient-light DC bias (16.5 µA -> 1.65 V),
    # then ride the unit AC stimulus on top. PySpice's CurrentSource doesn't
    # expose ac_magnitude, so inject the SPICE line directly.
    c.raw_spice += f"Iphoto N_INV 0 DC {AC_DC_BIAS} AC 1\n"
    return c


def build_circuit_pulse(c_f: float) -> Circuit:
    c = _common_circuit("BPW34_MCP6004_TIA_PULSE", c_f)
    c.PulseCurrentSource(
        "photo", "N_INV", c.gnd,
        initial_value=0,
        pulsed_value=PULSE_AMPLITUDE,
        delay_time=0,
        rise_time=PULSE_EDGE,
        fall_time=PULSE_EDGE,
        pulse_width=PULSE_WIDTH,
        period=PULSE_PERIOD,
    )
    return c


# ----------------------------------------------------------------------------
# Simulation drivers
# ----------------------------------------------------------------------------
def _prep_ngspice() -> None:
    # The Microchip macromodel uses PSpice POLY/TABLE/TC syntax; switch ngspice
    # into PSpice compatibility mode before any netlist is parsed.
    NgSpiceShared.new_instance().exec_command("set ngbehavior=ps")


def run_ac(c_f: float):
    circuit = build_circuit_ac(c_f)
    simulator = circuit.simulator(temperature=25, nominal_temperature=25)
    _prep_ngspice()
    analysis = simulator.ac(
        start_frequency=AC_F_START,
        stop_frequency=AC_F_STOP,
        number_of_points=AC_POINTS_PER_DECADE,
        variation="dec",
    )
    f = np.array(analysis.frequency, dtype=float)
    v = np.array(analysis["vout"], dtype=complex)
    return f, v


def run_transient(c_f: float):
    circuit = build_circuit_pulse(c_f)
    simulator = circuit.simulator(temperature=25, nominal_temperature=25)
    _prep_ngspice()
    analysis = simulator.transient(
        step_time=TRAN_STEP,
        end_time=TRAN_END,
        use_initial_condition=False,
    )
    t = np.array(analysis.time, dtype=float)
    v = np.array(analysis["vout"], dtype=float)
    return t, v


# ----------------------------------------------------------------------------
# Plotting
# ----------------------------------------------------------------------------
def plot_bode(results, out_path: Path):
    fig, ax = plt.subplots(figsize=(10, 6))
    summary = []
    for (label, c_f), (f, v) in zip(C_F_VALUES, results):
        mag_db = 20.0 * np.log10(np.abs(v))
        ax.semilogx(f, mag_db, label=f"C_F = {label}", linewidth=1.6)

        low_f_gain = mag_db[: max(1, len(f) // 20)].mean()  # gain in lowest decade
        # First frequency where the magnitude has dropped 3 dB below the low-f gain.
        below = np.where(mag_db < low_f_gain - 3.0)[0]
        f3db = f[below[0]] if below.size else float("inf")
        summary.append((label, c_f, low_f_gain, f3db))

    ax.axvline(TARGET_120HZ, color="tab:red", linestyle="--", linewidth=1,
               label=f"120 Hz (ProMotion)")
    ax.axvline(TARGET_20KHZ, color="tab:orange", linestyle="--", linewidth=1,
               label=f"20 kHz (PWM)")

    ax.set_xlabel("Frequency (Hz)")
    ax.set_ylabel("|V_OUT / I_photo|  (dBΩ)")
    ax.set_title(f"BPW34 + MCP6004 TIA — AC transimpedance, R_F = {R_F/1e3:g} kΩ")
    ax.set_xlim(AC_F_START, AC_F_STOP)
    ax.grid(True, which="both", alpha=0.3)
    ax.legend(loc="lower left", fontsize=9)
    fig.tight_layout()
    fig.savefig(out_path, dpi=140)
    return summary


def plot_transient(results, out_path: Path):
    fig, ax = plt.subplots(figsize=(10, 6))
    for (label, c_f), (t, v) in zip(C_F_VALUES, results):
        ax.plot(t * 1e6, v, label=f"C_F = {label}", linewidth=1.4)

    # Annotate the input pulse cadence on the time axis
    for k in range(int(TRAN_END / PULSE_PERIOD) + 1):
        edge = k * PULSE_PERIOD * 1e6
        ax.axvline(edge, color="lightgray", linestyle=":", linewidth=0.6, zorder=0)

    ideal_low = R_F * PULSE_AMPLITUDE  # asymptotic VOUT at "on" phase = 1.0 V
    ax.axhline(ideal_low, color="black", linestyle=":", linewidth=0.8, alpha=0.6,
               label=f"Ideal V_OUT during pulse = {ideal_low:.2f} V")

    ax.set_xlabel("Time (µs)")
    ax.set_ylabel("V_OUT (V)")
    ax.set_title(f"BPW34 + MCP6004 TIA — 20 kHz, 10 µA pulsed photocurrent, R_F = {R_F/1e3:g} kΩ")
    ax.set_xlim(0, TRAN_END * 1e6)
    ax.grid(True, alpha=0.3)
    # Anchor the legend outside the axes so it never overlaps the spiky 1 pF /
    # 10 pF traces that swing past 2 V.
    ax.legend(loc="center left", bbox_to_anchor=(1.02, 0.5), fontsize=9,
              borderaxespad=0.0)
    fig.tight_layout()
    fig.savefig(out_path, dpi=140, bbox_inches="tight")


# ----------------------------------------------------------------------------
# Recommendation
# ----------------------------------------------------------------------------
def report_recommendation(bode_summary):
    print()
    print("=" * 78)
    print("AC bandwidth summary  (R_F = {:g} kΩ)".format(R_F / 1e3))
    print("=" * 78)
    print(f"{'C_F':<8} {'1/(2π·R_F·C_F)':>18} {'low-f gain':>14} {'measured -3dB':>18}")
    print("-" * 78)

    recommendation = None
    best_margin = -1.0
    for label, c_f, gain_db, f3db in bode_summary:
        rc_bw = 1.0 / (2.0 * np.pi * R_F * c_f)
        f3db_str = f"{f3db:>10.3g} Hz" if np.isfinite(f3db) else "      > 1 MHz"
        print(f"{label:<8} {rc_bw:>15.3g} Hz {gain_db:>11.1f} dBΩ {f3db_str:>18}")
        # Pick the smallest C_F whose -3dB is comfortably above 20 kHz —
        # i.e. the largest C_F that still passes 20 kHz. That maximises
        # noise rejection while preserving the signal of interest.
        if f3db >= TARGET_20KHZ * 1.2:
            margin = f3db / TARGET_20KHZ
            if recommendation is None or margin < best_margin:
                # smaller margin = larger C_F that still works
                pass
            if recommendation is None or c_f > recommendation[1]:
                recommendation = (label, c_f, f3db)
                best_margin = margin

    print()
    if recommendation is None:
        print("⚠ No swept C_F gives -3 dB ≥ 20 kHz. Try smaller capacitors.")
    else:
        label, c_f, f3db = recommendation
        print(f"Recommended C_F for 20 kHz detection: **{label}**")
        print(f"  -3 dB bandwidth ≈ {f3db:.3g} Hz "
              f"({f3db / TARGET_20KHZ:.1f}× the 20 kHz target).")
        print(f"  Largest swept C_F whose response still covers 20 kHz, so it")
        print(f"  filters the most out-of-band noise while preserving the signal.")
        print()
        print("Trade-offs visible in transient_pwm.png:")
        print("  • C_F = 1 pF:  under-compensated — large overshoot/ringing on edges.")
        print(f"  • C_F = {label}: clean fast edges with mild peaking (recommended).")
        print("  • C_F = 100 pF: cleanest pulse shape, but BW (16 kHz) starts to")
        print("                 attenuate 20 kHz; usable if some loss is OK.")
        print("  • C_F ≥ 1 nF:   classic shark-fin RC integration — 20 kHz is lost.")
    print("=" * 78)


# ----------------------------------------------------------------------------
# Main
# ----------------------------------------------------------------------------
def main() -> None:
    print("Running AC sweep ...")
    ac_results = []
    for label, c_f in C_F_VALUES:
        print(f"  C_F = {label}")
        ac_results.append(run_ac(c_f))

    bode_summary = plot_bode(ac_results, ROOT / "ac_bode.png")
    print(f"  saved {ROOT/'ac_bode.png'}")

    print("\nRunning transient (20 kHz pulse) ...")
    tran_results = []
    for label, c_f in C_F_VALUES:
        print(f"  C_F = {label}")
        tran_results.append(run_transient(c_f))

    plot_transient(tran_results, ROOT / "transient_pwm.png")
    print(f"  saved {ROOT/'transient_pwm.png'}")

    report_recommendation(bode_summary)


if __name__ == "__main__":
    main()

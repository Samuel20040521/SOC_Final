# SOC_Final — BPW34 + MCP6004 Transimpedance Amplifier

PySpice-based simulation of a single-supply transimpedance amplifier (TIA) built around a Vishay **BPW34** silicon PIN photodiode and a Microchip **MCP6004** rail-to-rail CMOS op-amp, plus the bench-measurement comparison from a breadboard build.

The project covers three analyses:

1. **DC sweep** of the photocurrent (`main.py`) — visualises the linear transimpedance gain `V_OUT = I_photo · R_F` and the rail clipping at `V_DD = 3.3 V` for `R_F ∈ {1 MΩ, 100 kΩ, 10 kΩ, 1 kΩ}`.
2. **Bench validation** (`report.md` + `data/exp_260506.dat`) — multimeter readings at four illumination levels, cross-checked against the simulation. Two independent estimates of room-light photocurrent agree to 0.7 %, validating the gain equation; the dark-output offset is decomposed into its real-world contributors (residual ambient light, op-amp `V_os`).
3. **Dynamic characterisation** (`ac_transient_sweep.py`) — AC Bode and 20 kHz pulsed-photocurrent transient at fixed `R_F = 100 kΩ` over `C_F ∈ {1 pF, 10 pF, 100 pF, 1 nF, 100 nF}`, to find the optimal feedback capacitor for detecting 120 Hz (ProMotion display refresh) and 20 kHz (PWM dimming) signals.

## Prerequisites

- [`uv`](https://github.com/astral-sh/uv) for Python project management.
- ngspice with shared library + XSPICE codemodels:

  ```sh
  sudo apt install libngspice0 ngspice
  ```

  `libngspice0` provides the shared library that PySpice loads via `cffi`. The `ngspice` package is required for `spinit` and the `*.cm` codemodels — without them, PSpice `POLY` E/G-sources inside the MCP6001 macromodel fail to convert into XSPICE A-blocks.

The repository wires PySpice to the SONAME-versioned `libngspice.so.0` automatically (Debian's `libngspice0` ships only that, not the unversioned symlink).

## Quick start

```sh
uv sync                                  # install Python deps from uv.lock
uv run python main.py                    # DC sweep -> tia_sweep.png
uv run python ac_transient_sweep.py      # AC + transient -> ac_bode.png, transient_pwm.png
```

## Repository layout

```
SOC_Final/
├── main.py                  # DC sweep of photocurrent across four R_F values
├── ac_transient_sweep.py    # AC Bode + 20 kHz pulse transient across five C_F values
├── pyproject.toml / uv.lock # dependencies: PySpice, schemdraw, numpy, matplotlib
├── spice_models/
│   ├── MCP6001.lib          # Microchip macromodel (covers MCP6001/2/4)
│   └── BPW34.lib            # BPW34 .MODEL diode primitive
├── data/
│   └── exp_260506.dat       # bench measurements: V_OUT vs R_F for 4 lighting conditions
├── report.md                # simulation-vs-bench comparison report
├── report/main.tex          # (placeholder for a LaTeX writeup)
├── tia_sweep.png            # DC sweep result
├── ac_bode.png              # AC transimpedance |Z(f)| vs frequency
└── transient_pwm.png        # V_OUT for 20 kHz, 10 µA pulsed photocurrent
```

## Circuit topology

Single 3.3 V supply. The op-amp's non-inverting input is tied to GND; the inverting input is the summing junction `N_INV`. The BPW34 is in photovoltaic mode with its anode at GND and cathode at `N_INV`, so photocurrent flows externally from cathode to anode through an ideal current source placed in parallel with the diode. Feedback is `R_F ∥ C_F` between `N_INV` and `V_OUT`.

```
        VDD = 3.3 V
           │
           │     ┌──────┐
           ├─────┤V+    │
           │     │      ├─ V_OUT ─┬──────── output
           │ ┌──┤V- N_INV│         │
           │ │   │MCP6004│         │
           │ │   └──┬───┘         R_F (and C_F in parallel)
        GND│ │      │              │
           │ │      ├──────────────┘
           │ │      │
           │ │      ├─── cathode ──┐
           │ │   ┌──┴──┐            │
           │ │   │BPW34│            ⊕  I_photo (cathode -> anode)
           │ │   └──┬──┘            │
           │ │      │ anode         │
           │ │      └───────────────┘
           │ │      │
        GND ┴ GND   GND
```

## Notes on the PySpice + ngspice plumbing

Three quirks were encountered and worked around in the scripts:

1. **Library path.** PySpice's cffi loader calls `dlopen("libngspice.so")`; Debian's `libngspice0` provides only `libngspice.so.0`. The scripts set `NGSPICE_LIBRARY_PATH` to the SONAME-versioned file before importing PySpice.
2. **PSpice compatibility.** The Microchip macromodel uses PSpice POLY/TABLE/TC syntax. ngspice needs `set ngbehavior=ps` issued as a *control* command before the netlist is parsed — `.options ngbehavior=ps` lands too late at the end of the deck.
3. **Spurious `NgSpiceCommandError`.** PySpice 1.5 mis-flags any non-"Warning:" line on ngspice's stderr as a fatal error, including informational `Note: dynamic gmin stepping…` messages emitted during normal DC operating-point convergence. `NgSpiceShared.exec_command` is wrapped to swallow the false-positive failure when every stderr line is a benign Note/step/completion.

## Recommended C_F for dynamic detection

From `ac_transient_sweep.py` at `R_F = 100 kΩ`:

| C_F      | Simulated -3 dB | 120 Hz | 20 kHz | Notes                                                  |
|----------|-----------------|:------:|:------:|--------------------------------------------------------|
| 1 pF     | 224 kHz         | ✓      | ✓      | Under-compensated — peaking + ringing                  |
| **10 pF**| **170 kHz**     | ✓      | ✓      | **Recommended** for 20 kHz PWM detection               |
| 100 pF   | 16 kHz          | ✓      | ~−3 dB | Cleanest pulse shape; starts attenuating 20 kHz        |
| 1 nF     | 1.6 kHz         | ✓      | ✗      | OK for 120 Hz only                                     |
| 100 nF   | 16 Hz           | ✗      | ✗      | Original value — kills *both* targets                  |

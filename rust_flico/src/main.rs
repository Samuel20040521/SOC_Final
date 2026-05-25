use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use web_time::{Duration, Instant};

#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Mutex;
#[cfg(not(target_arch = "wasm32"))]
use std::thread;
#[cfg(not(target_arch = "wasm32"))]
use web_time::{SystemTime, UNIX_EPOCH};
#[cfg(not(target_arch = "wasm32"))]
use crossbeam_channel::{bounded, Receiver, Sender, TryRecvError};
#[cfg(not(target_arch = "wasm32"))]
use clap::Parser;

use eframe::egui;
use egui_plot::{Line, Plot, PlotBounds, PlotPoints};
use realfft::num_complex::Complex;
use realfft::RealFftPlanner;

// SI-prefix formatter. `value` is in the base unit (e.g. seconds, hertz, amps).
// Picks one of M / k / (none) / m / μ / n based on magnitude and returns a
// short string like "1.23 ms" or "8.00 kHz". Precision adapts to the leading
// digit so we always show ~3 significant figures.
fn fmt_si(value: f64, unit: &str) -> String {
    if !value.is_finite() {
        return format!("— {}", unit);
    }
    if value == 0.0 {
        return format!("0 {}", unit);
    }
    let av = value.abs();
    let (scale, prefix) = if av >= 1e6 {
        (1e-6, "M")
    } else if av >= 1e3 {
        (1e-3, "k")
    } else if av >= 1.0 {
        (1.0, "")
    } else if av >= 1e-3 {
        (1e3, "m")
    } else if av >= 1e-6 {
        (1e6, "\u{03BC}") // μ
    } else if av >= 1e-9 {
        (1e9, "n")
    } else {
        (1e12, "p")
    };
    let v = value * scale;
    let av2 = v.abs();
    if av2 >= 100.0 {
        format!("{:.1} {}{}", v, prefix, unit)
    } else if av2 >= 10.0 {
        format!("{:.2} {}{}", v, prefix, unit)
    } else {
        format!("{:.3} {}{}", v, prefix, unit)
    }
}

const SAMPLE_RATE: f32 = 16_000.0;
const BUF_LEN: usize = 80_000;
const UNDO_CAP: usize = 64;
const FFT_PACE_MS: u128 = 100;

// Pico wire-protocol constants (must match firmware_cpp/main.cpp).
//   - Frame sync byte: 0xA0 | range_index   (range in low nibble, 0..3)
//   - ADC: 12-bit, low byte then high nibble
//   - Status lines start with '#' and end with '\n'
#[cfg(not(target_arch = "wasm32"))]
const PICO_ADC_REF: f32 = 3.3;
#[cfg(not(target_arch = "wasm32"))]
const PICO_ADC_MAX: f32 = 4095.0;
#[cfg(not(target_arch = "wasm32"))]
const PICO_GEAR_R_OHMS: [f32; 4] = [1_100.0, 10_100.0, 100_100.0, 1_000_100.0];

// ============================================================================
// Shared FFT computer: owned by the native worker thread, also held inline by
// the wasm App (single-threaded). Same compute path either way.
// ============================================================================
struct FftComputer {
    planner: RealFftPlanner<f32>,
    hann_cache: HashMap<usize, Vec<f32>>,
    freq_cache: HashMap<u64, Arc<[f32]>>,
    work: Vec<f32>,
    spec: Vec<Complex<f32>>,
}

impl FftComputer {
    fn new() -> Self {
        Self {
            planner: RealFftPlanner::<f32>::new(),
            hann_cache: HashMap::new(),
            freq_cache: HashMap::new(),
            work: Vec::with_capacity(BUF_LEN.next_power_of_two()),
            spec: Vec::with_capacity(BUF_LEN.next_power_of_two() / 2 + 1),
        }
    }

    fn compute(
        &mut self,
        samples: &[f32],
        fs: f32,
        t_lo_ms: f64,
        t_hi_ms: f64,
        seq: u64,
    ) -> Option<FftResult> {
        let n = samples.len();
        if n < 16 {
            return None;
        }
        let t0 = Instant::now();
        let n_pad = n.next_power_of_two();

        let hann = self.hann_cache.entry(n).or_insert_with(|| {
            (0..n)
                .map(|i| {
                    let x = i as f32 / (n as f32 - 1.0).max(1.0);
                    0.5 - 0.5 * (std::f32::consts::TAU * x).cos()
                })
                .collect()
        });
        let win_sum: f32 = hann.iter().sum();
        let mean: f32 = samples.iter().sum::<f32>() / n as f32;

        self.work.clear();
        self.work.reserve(n_pad);
        for (i, &x) in samples.iter().enumerate() {
            self.work.push((x - mean) * hann[i]);
        }
        self.work.resize(n_pad, 0.0);

        let r2c = self.planner.plan_fft_forward(n_pad);
        self.spec.clear();
        self.spec.resize(n_pad / 2 + 1, Complex::new(0.0, 0.0));
        if r2c.process(&mut self.work, &mut self.spec).is_err() {
            return None;
        }

        let scale = 2.0 / win_sum.max(1.0);
        let mags: Vec<f32> = self.spec.iter().map(|c| c.norm() * scale).collect();

        let key = ((n_pad as u64) << 32) | (fs.to_bits() as u64);
        let freqs = self
            .freq_cache
            .entry(key)
            .or_insert_with(|| {
                let df = fs / n_pad as f32;
                let v: Vec<f32> = (0..n_pad / 2 + 1).map(|k| k as f32 * df).collect();
                Arc::from(v.into_boxed_slice())
            })
            .clone();

        let elapsed_ms = t0.elapsed().as_secs_f32() * 1000.0;
        Some(FftResult {
            seq,
            freqs,
            mags,
            t_lo_ms,
            t_hi_ms,
            n,
            n_pad,
            elapsed_ms,
        })
    }
}

struct FftResult {
    seq: u64,
    freqs: Arc<[f32]>,
    mags: Vec<f32>,
    t_lo_ms: f64,
    t_hi_ms: f64,
    n: usize,
    n_pad: usize,
    elapsed_ms: f32,
}

// ============================================================================
// Synth signal — pure function shared between native thread + wasm pump.
// ============================================================================
fn synth_sample(emitted_n: u64) -> f32 {
    let t = emitted_n as f32 / SAMPLE_RATE;
    let tau = std::f32::consts::TAU;
    1.0 * (tau * 3.0 * t).sin()
        + 0.3 * (tau * 60.0 * t).sin()
        + 0.1 * (tau * 120.0 * t).sin()
        + 0.05 * (tau * 2400.0 * t).sin()
}

// ============================================================================
// Native-only: threaded synth source + FFT worker + channels.
// ============================================================================
#[cfg(not(target_arch = "wasm32"))]
struct SharedSamples {
    samples: VecDeque<f32>,
}

#[cfg(not(target_arch = "wasm32"))]
struct FftJob {
    seq: u64,
    samples: Arc<[f32]>,
    fs: f32,
    t_lo_ms: f64,
    t_hi_ms: f64,
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_fft_worker(rx: Receiver<FftJob>, tx: Sender<FftResult>) -> thread::JoinHandle<()> {
    thread::Builder::new()
        .name("fft-worker".into())
        .spawn(move || {
            let mut computer = FftComputer::new();
            while let Ok(mut job) = rx.recv() {
                loop {
                    match rx.try_recv() {
                        Ok(newer) => job = newer,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                if let Some(result) = computer.compute(
                    &job.samples,
                    job.fs,
                    job.t_lo_ms,
                    job.t_hi_ms,
                    job.seq,
                ) {
                    if tx.send(result).is_err() {
                        return;
                    }
                }
            }
        })
        .expect("spawn fft worker")
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_synth_source(
    shared: Arc<Mutex<SharedSamples>>,
    running: Arc<AtomicBool>,
    emitted: Arc<AtomicU64>,
) {
    thread::spawn(move || {
        let start = Instant::now();
        let mut emitted_local: u64 = 0;
        loop {
            let elapsed = start.elapsed().as_secs_f64();
            let target = (elapsed * SAMPLE_RATE as f64) as u64;
            if target > emitted_local {
                let batch = (target - emitted_local) as usize;
                if running.load(Ordering::Relaxed) {
                    let mut s = shared.lock().unwrap();
                    for k in 0..batch {
                        let i = emitted_local + 1 + k as u64;
                        let y = synth_sample(i);
                        if s.samples.len() == BUF_LEN {
                            s.samples.pop_front();
                        }
                        s.samples.push_back(y);
                    }
                }
                emitted_local = target;
                emitted.store(emitted_local, Ordering::Relaxed);
            }
            thread::sleep(Duration::from_millis(2));
        }
    });
}

// ============================================================================
// Native-only: Pico USB-CDC source. Mirrors read_data.py's StreamParser.
// ============================================================================
#[cfg(not(target_arch = "wasm32"))]
fn auto_detect_pico_port() -> Option<String> {
    let ports = serialport::available_ports().ok()?;
    for p in &ports {
        let mut desc = String::new();
        if let serialport::SerialPortType::UsbPort(info) = &p.port_type {
            if let Some(m) = &info.manufacturer { desc.push_str(&m.to_lowercase()); }
            desc.push(' ');
            if let Some(prod) = &info.product { desc.push_str(&prod.to_lowercase()); }
        }
        let name_low = p.port_name.to_lowercase();
        if name_low.contains("usbmodem")
            || desc.contains("pico")
            || desc.contains("rp2040")
            || desc.contains("raspberry")
        {
            return Some(p.port_name.clone());
        }
    }
    // Fallback: any /dev/cu.usbmodem* on macOS, /dev/ttyACM* on Linux.
    for p in &ports {
        let n = &p.port_name;
        if n.contains("usbmodem") || n.contains("ttyACM") {
            return Some(n.clone());
        }
    }
    None
}

#[cfg(not(target_arch = "wasm32"))]
fn spawn_pico_source(
    port_name: String,
    shared: Arc<Mutex<SharedSamples>>,
    running: Arc<AtomicBool>,
    emitted: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let mut port = serialport::new(&port_name, 115_200)
        .timeout(Duration::from_millis(50))
        .open()?;
    // Kick the Pico into MON mode.
    let _ = port.write_all(b"m");
    let _ = port.flush();

    thread::Builder::new()
        .name("pico-reader".into())
        .spawn(move || {
            let mut buf: Vec<u8> = Vec::with_capacity(16 * 1024);
            let mut chunk = [0u8; 4096];
            let mut emitted_local: u64 = 0;
            let mut batch: Vec<f32> = Vec::with_capacity(2048);

            loop {
                match port.read(&mut chunk) {
                    Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
                    Err(e) => {
                        eprintln!("pico-reader: serial read failed: {}", e);
                        return;
                    }
                }

                batch.clear();
                let mut i = 0usize;
                let len = buf.len();
                while i < len {
                    let b0 = buf[i];

                    // ASCII status line — '#' ... '\n'
                    if b0 == b'#' {
                        if let Some(rel_nl) = buf[i..].iter().position(|&b| b == b'\n') {
                            let line = std::str::from_utf8(&buf[i..i + rel_nl])
                                .unwrap_or("")
                                .trim();
                            eprintln!("[pico] {}", line);
                            i += rel_nl + 1;
                            continue;
                        } else {
                            break; // wait for more bytes
                        }
                    }

                    if b0 == 0x0A || b0 == 0x0D {
                        i += 1;
                        continue;
                    }

                    if (b0 & 0xF0) == 0xA0 && len - i >= 3 {
                        let b1 = buf[i + 1];
                        let b2 = buf[i + 2];
                        if (b2 & 0xF0) == 0 {
                            let range_idx = (b0 & 0x0F) as usize;
                            let adc = (b1 as u16) | ((b2 as u16) << 8);
                            let v = adc as f32 * (PICO_ADC_REF / PICO_ADC_MAX);
                            let r = PICO_GEAR_R_OHMS[range_idx.min(3)];
                            batch.push(v / r);
                            i += 3;
                            continue;
                        }
                    }
                    // Resync — drop one byte.
                    i += 1;
                }
                buf.drain(..i);

                if !batch.is_empty() {
                    if running.load(Ordering::Relaxed) {
                        let mut s = shared.lock().unwrap();
                        for &y in &batch {
                            if s.samples.len() == BUF_LEN {
                                s.samples.pop_front();
                            }
                            s.samples.push_back(y);
                        }
                    }
                    emitted_local += batch.len() as u64;
                    emitted.store(emitted_local, Ordering::Relaxed);
                }
            }
        })?;
    Ok(())
}

// ============================================================================
// PerfRing
// ============================================================================
struct PerfRing {
    samples: [f32; 60],
    head: usize,
    filled: usize,
}
impl PerfRing {
    fn new() -> Self {
        Self {
            samples: [0.0; 60],
            head: 0,
            filled: 0,
        }
    }
    fn push(&mut self, x: f32) {
        self.samples[self.head] = x;
        self.head = (self.head + 1) % self.samples.len();
        self.filled = (self.filled + 1).min(self.samples.len());
    }
    fn mean(&self) -> f32 {
        if self.filled == 0 {
            return 0.0;
        }
        self.samples.iter().take(self.filled).sum::<f32>() / self.filled as f32
    }
}

// ============================================================================
// App — cfg-gated data plumbing, identical UI on both targets.
// ============================================================================
struct App {
    // Common state ----------------------------------------------------------
    view_x: Option<(f64, f64)>,
    undo: Vec<(f64, f64)>,
    redo: Vec<(f64, f64)>,
    pending_x: Option<(f64, f64)>,
    last_view_change_at: Option<Instant>,

    job_seq: u64,
    latest_result: Option<FftResult>,
    last_fft_at: Option<Instant>,

    status: String,
    t_snapshot_ms: PerfRing,
    t_inter_ms: PerfRing,
    last_frame: Instant,
    show_details: bool,

    // Native: threaded plumbing --------------------------------------------
    #[cfg(not(target_arch = "wasm32"))]
    shared: Arc<Mutex<SharedSamples>>,
    #[cfg(not(target_arch = "wasm32"))]
    running: Arc<AtomicBool>,
    #[cfg(not(target_arch = "wasm32"))]
    samples_emitted: Arc<AtomicU64>,
    #[cfg(not(target_arch = "wasm32"))]
    job_tx: Sender<FftJob>,
    #[cfg(not(target_arch = "wasm32"))]
    result_rx: Receiver<FftResult>,

    // Wasm: inline state ---------------------------------------------------
    #[cfg(target_arch = "wasm32")]
    samples: VecDeque<f32>,
    #[cfg(target_arch = "wasm32")]
    samples_emitted: u64,
    #[cfg(target_arch = "wasm32")]
    synth_start: Instant,
    #[cfg(target_arch = "wasm32")]
    running: bool,
    #[cfg(target_arch = "wasm32")]
    fft: FftComputer,
}

impl App {
    #[cfg(not(target_arch = "wasm32"))]
    fn new(
        shared: Arc<Mutex<SharedSamples>>,
        running: Arc<AtomicBool>,
        emitted: Arc<AtomicU64>,
        job_tx: Sender<FftJob>,
        result_rx: Receiver<FftResult>,
    ) -> Self {
        Self {
            view_x: Some((-(BUF_LEN as f64) * 1000.0 / SAMPLE_RATE as f64, 0.0)),
            undo: Vec::with_capacity(UNDO_CAP),
            redo: Vec::with_capacity(UNDO_CAP),
            pending_x: Some((-(BUF_LEN as f64) * 1000.0 / SAMPLE_RATE as f64, 0.0)),
            last_view_change_at: None,
            job_seq: 0,
            latest_result: None,
            last_fft_at: None,
            status: String::new(),
            t_snapshot_ms: PerfRing::new(),
            t_inter_ms: PerfRing::new(),
            last_frame: Instant::now(),
            show_details: true,
            shared,
            running,
            samples_emitted: emitted,
            job_tx,
            result_rx,
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn new_wasm() -> Self {
        Self {
            view_x: Some((-(BUF_LEN as f64) * 1000.0 / SAMPLE_RATE as f64, 0.0)),
            undo: Vec::with_capacity(UNDO_CAP),
            redo: Vec::with_capacity(UNDO_CAP),
            pending_x: Some((-(BUF_LEN as f64) * 1000.0 / SAMPLE_RATE as f64, 0.0)),
            last_view_change_at: None,
            job_seq: 0,
            latest_result: None,
            last_fft_at: None,
            status: String::new(),
            t_snapshot_ms: PerfRing::new(),
            t_inter_ms: PerfRing::new(),
            last_frame: Instant::now(),
            show_details: true,
            samples: VecDeque::with_capacity(BUF_LEN),
            samples_emitted: 0,
            synth_start: Instant::now(),
            running: true,
            fft: FftComputer::new(),
        }
    }

    // ----- platform-uniform accessors -----
    fn is_running(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.running.load(Ordering::Relaxed)
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.running
        }
    }
    fn toggle_running(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let cur = self.running.load(Ordering::Relaxed);
            self.running.store(!cur, Ordering::Relaxed);
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.running = !self.running;
        }
    }
    fn samples_emitted_count(&self) -> u64 {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.samples_emitted.load(Ordering::Relaxed)
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.samples_emitted
        }
    }

    // Pump synth inline (wasm only — on native the synth thread fills the buffer).
    #[cfg(target_arch = "wasm32")]
    fn pump_synth(&mut self) {
        let elapsed = self.synth_start.elapsed().as_secs_f64();
        let target = (elapsed * SAMPLE_RATE as f64) as u64;
        if target > self.samples_emitted {
            let batch = (target - self.samples_emitted) as usize;
            if self.running {
                for k in 0..batch {
                    let i = self.samples_emitted + 1 + k as u64;
                    let y = synth_sample(i);
                    if self.samples.len() == BUF_LEN {
                        self.samples.pop_front();
                    }
                    self.samples.push_back(y);
                }
            }
            self.samples_emitted = target;
        }
    }

    fn take_snapshot(&self) -> Vec<f32> {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let s = self.shared.lock().unwrap();
            s.samples.iter().copied().collect()
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.samples.iter().copied().collect()
        }
    }

    fn drain_fft_results(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            while let Ok(r) = self.result_rx.try_recv() {
                match &self.latest_result {
                    Some(cur) if cur.seq > r.seq => {}
                    _ => self.latest_result = Some(r),
                }
            }
        }
        // wasm: results stored synchronously in dispatch_fft
    }

    fn dispatch_fft(&mut self, snapshot: &[f32], buf_t_lo: f64, buf_t_hi: f64, fs: f32) {
        let now = Instant::now();
        let due = self
            .last_fft_at
            .map_or(true, |t| now.duration_since(t).as_millis() >= FFT_PACE_MS);
        let view_just_changed = self
            .last_view_change_at
            .zip(self.last_fft_at)
            .map(|(c, e)| c >= e)
            .unwrap_or(true);
        if !due && !view_just_changed {
            return;
        }

        let (t_lo_ms, t_hi_ms) = self.view_x.unwrap_or((buf_t_lo, buf_t_hi));
        let n = snapshot.len();
        if n < 64 {
            return;
        }
        let (i_lo, i_hi) = slice_indices(t_lo_ms, t_hi_ms, n, fs);
        if i_hi <= i_lo || i_hi - i_lo < 64 {
            return;
        }

        self.job_seq += 1;
        let seq = self.job_seq;

        #[cfg(not(target_arch = "wasm32"))]
        {
            let slice_vec: Vec<f32> = snapshot[i_lo..i_hi].to_vec();
            let arc: Arc<[f32]> = Arc::from(slice_vec.into_boxed_slice());
            let job = FftJob {
                seq,
                samples: arc,
                fs,
                t_lo_ms,
                t_hi_ms,
            };
            if self.job_tx.try_send(job).is_ok() {
                self.last_fft_at = Some(now);
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(r) = self
                .fft
                .compute(&snapshot[i_lo..i_hi], fs, t_lo_ms, t_hi_ms, seq)
            {
                self.latest_result = Some(r);
                self.last_fft_at = Some(now);
            }
        }
    }

    fn push_undo(&mut self, prev: (f64, f64)) {
        if let Some(&last) = self.undo.last() {
            let span = (last.1 - last.0).abs().max(1.0);
            let dev = ((prev.0 - last.0).abs() + (prev.1 - last.1).abs()) / span;
            if dev < 1e-4 {
                return;
            }
        }
        self.undo.push(prev);
        if self.undo.len() > UNDO_CAP {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn act_undo(&mut self) {
        if let Some(prev) = self.undo.pop() {
            if let Some(cur) = self.view_x {
                self.redo.push(cur);
            }
            self.view_x = Some(prev);
            self.pending_x = Some(prev);
        }
    }

    fn act_redo(&mut self) {
        if let Some(next) = self.redo.pop() {
            if let Some(cur) = self.view_x {
                self.undo.push(cur);
                if self.undo.len() > UNDO_CAP {
                    self.undo.remove(0);
                }
            }
            self.view_x = Some(next);
            self.pending_x = Some(next);
        }
    }

    fn act_reset(&mut self, buf_t_lo_ms: f64, buf_t_hi_ms: f64) {
        let target = (buf_t_lo_ms, buf_t_hi_ms);
        if let Some(cur) = self.view_x {
            let span = (cur.1 - cur.0).abs().max(1.0);
            let dev = ((cur.0 - target.0).abs() + (cur.1 - target.1).abs()) / span;
            if dev < 1e-3 {
                return;
            }
            self.push_undo(cur);
        }
        self.view_x = Some(target);
        self.pending_x = Some(target);
    }
}

fn slice_indices(t_lo_ms: f64, t_hi_ms: f64, n: usize, fs: f32) -> (usize, usize) {
    let to_i = |t_ms: f64| -> i64 {
        ((t_ms * fs as f64 / 1000.0) + n as f64).round() as i64
    };
    let lo = to_i(t_lo_ms).clamp(0, n as i64) as usize;
    let hi = to_i(t_hi_ms).clamp(0, n as i64) as usize;
    if lo > hi {
        (hi, lo)
    } else {
        (lo, hi)
    }
}

fn data_y_range_in_x(samples: &[f32], x_lo_ms: f64, x_hi_ms: f64, fs: f32) -> (f64, f64) {
    let n = samples.len();
    if n == 0 {
        return (-1.0, 1.0);
    }
    let (lo, hi) = slice_indices(x_lo_ms, x_hi_ms, n, fs);
    if hi <= lo {
        return (-1.0, 1.0);
    }
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for &x in &samples[lo..hi] {
        if x < min {
            min = x;
        }
        if x > max {
            max = x;
        }
    }
    if !min.is_finite() || !max.is_finite() {
        return (-1.0, 1.0);
    }
    let pad = ((max - min) * 0.1).max(1e-6);
    ((min - pad) as f64, (max + pad) as f64)
}

#[cfg(not(target_arch = "wasm32"))]
fn export_csv(snapshot: &[f32], sample_rate: f32) -> std::io::Result<String> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = format!("flicker_{}.csv", ts);
    let mut f = std::fs::File::create(&path)?;
    writeln!(f, "sample_idx,t_ms,value")?;
    let n = snapshot.len();
    for (i, &y) in snapshot.iter().enumerate() {
        let t_ms = (i as f64 - n as f64 + 1.0) * 1000.0 / sample_rate as f64;
        writeln!(f, "{},{:.4},{:.6}", i, t_ms, y)?;
    }
    Ok(path)
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        let inter = frame_start
            .duration_since(self.last_frame)
            .as_secs_f32()
            * 1000.0;
        self.t_inter_ms.push(inter);
        self.last_frame = frame_start;

        // Wasm pumps its own synth; native runs in a thread.
        #[cfg(target_arch = "wasm32")]
        self.pump_synth();

        let mut export_request = false;
        let mut key_undo = false;
        let mut key_redo = false;
        let mut key_reset = false;
        ctx.input(|i| {
            if i.key_pressed(egui::Key::Space) {
                self.toggle_running();
            }
            if i.key_pressed(egui::Key::E) {
                export_request = true;
            }
            if i.key_pressed(egui::Key::Tab) {
                self.show_details = !self.show_details;
            }
            if i.key_pressed(egui::Key::Q) {
                #[cfg(not(target_arch = "wasm32"))]
                std::process::exit(0);
            }
            let cmd = i.modifiers.command;
            let shift = i.modifiers.shift;
            if (cmd && i.key_pressed(egui::Key::Z) && !shift)
                || i.key_pressed(egui::Key::ArrowLeft)
            {
                key_undo = true;
            }
            if (cmd && i.key_pressed(egui::Key::Z) && shift)
                || (cmd && i.key_pressed(egui::Key::Y))
                || i.key_pressed(egui::Key::ArrowRight)
            {
                key_redo = true;
            }
            if i.key_pressed(egui::Key::R) {
                key_reset = true;
            }
        });

        let t0 = Instant::now();
        let snapshot = self.take_snapshot();
        self.t_snapshot_ms
            .push(t0.elapsed().as_secs_f32() * 1000.0);
        let n = snapshot.len();

        if export_request {
            #[cfg(not(target_arch = "wasm32"))]
            {
                match export_csv(&snapshot, SAMPLE_RATE) {
                    Ok(p) => self.status = format!("saved {} ({} samples)", p, snapshot.len()),
                    Err(e) => self.status = format!("save failed: {}", e),
                }
            }
            #[cfg(target_arch = "wasm32")]
            {
                self.status = "CSV export is only enabled in the native build.".into();
            }
        }

        let buf_t_lo_ms = -(BUF_LEN as f64) * 1000.0 / SAMPLE_RATE as f64;
        let buf_t_hi_ms = 0.0;

        if key_undo {
            self.act_undo();
        }
        if key_redo {
            self.act_redo();
        }
        if key_reset {
            self.act_reset(buf_t_lo_ms, buf_t_hi_ms);
        }

        self.drain_fft_results();
        self.dispatch_fft(&snapshot, buf_t_lo_ms, buf_t_hi_ms, SAMPLE_RATE);

        let time_points: PlotPoints = snapshot
            .iter()
            .enumerate()
            .map(|(i, &y)| {
                let t_ms = (i as f64 - n as f64) * 1000.0 / SAMPLE_RATE as f64;
                [t_ms, y as f64]
            })
            .collect();

        let (can_undo, can_redo, can_reset_now) = (
            !self.undo.is_empty(),
            !self.redo.is_empty(),
            self.view_x
                .map(|(lo, hi)| {
                    let span = (buf_t_hi_ms - buf_t_lo_ms).abs().max(1.0);
                    let dev =
                        ((lo - buf_t_lo_ms).abs() + (hi - buf_t_hi_ms).abs()) / span;
                    dev > 1e-3
                })
                .unwrap_or(false),
        );

        let mut overlay_undo = false;
        let mut overlay_redo = false;
        let mut overlay_reset = false;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading(
                egui::RichText::new("Flicker Scope (Rust) — zoom-linked FFT").size(22.0),
            );

            let running_state = self.is_running();
            let (state_txt, state_col) = if running_state {
                ("● RUN", egui::Color32::from_rgb(34, 197, 94))
            } else {
                ("■ STOP", egui::Color32::from_rgb(248, 113, 113))
            };
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(state_txt)
                        .size(16.0)
                        .strong()
                        .color(state_col),
                );
                let hud = if self.show_details {
                    format!(
                        "Fs = {:.0} Hz   buffer N = {}   emitted = {}",
                        SAMPLE_RATE,
                        n,
                        self.samples_emitted_count()
                    )
                } else {
                    format!("Fs = {:.0} Hz", SAMPLE_RATE)
                };
                ui.label(egui::RichText::new(hud).size(14.0));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(
                        egui::RichText::new(
                            "[space] Run/Stop  [e] CSV  [tab] details  [q] quit",
                        )
                        .size(13.0)
                        .color(egui::Color32::from_gray(160)),
                    );
                });
            });

            if self.show_details {
                let fft_info = match &self.latest_result {
                    Some(r) => format!(
                        "FFT: N={} pad={} took {:.2} ms",
                        r.n, r.n_pad, r.elapsed_ms,
                    ),
                    None => "FFT: (waiting)".to_string(),
                };
                let avg_inter = self.t_inter_ms.mean();
                let fps = if avg_inter > 0.01 {
                    1000.0 / avg_inter
                } else {
                    0.0
                };
                ui.label(
                    egui::RichText::new(format!(
                        "perf: snapshot {:5.2} ms   inter-frame {:5.2} ms   ≈ {:.1} fps   |   {}",
                        self.t_snapshot_ms.mean(),
                        avg_inter,
                        fps,
                        fft_info,
                    ))
                    .size(13.0)
                    .color(egui::Color32::from_rgb(6, 182, 212))
                    .monospace(),
                );
            }

            if !self.status.is_empty() {
                ui.label(
                    egui::RichText::new(&self.status)
                        .size(13.0)
                        .color(egui::Color32::from_rgb(251, 191, 36)),
                );
            }
            ui.separator();

            let avail_h = ui.available_height();
            let pane_h = (avail_h - 32.0) * 0.5;

            let view_x_for_label = self.view_x.unwrap_or((buf_t_lo_ms, buf_t_hi_ms));
            let t_start_s = view_x_for_label.0 * 1e-3;
            let t_end_s = view_x_for_label.1 * 1e-3;
            let t_span_s = (view_x_for_label.1 - view_x_for_label.0) * 1e-3;
            let time_title = if self.show_details {
                format!(
                    "Time domain — I(t)   t \u{2208} [{}, {}]   span = {}",
                    fmt_si(t_start_s, "s"),
                    fmt_si(t_end_s, "s"),
                    fmt_si(t_span_s, "s"),
                )
            } else {
                format!(
                    "Time domain — I(t)   t \u{2208} [{}, {}]",
                    fmt_si(t_start_s, "s"),
                    fmt_si(t_end_s, "s"),
                )
            };
            ui.label(egui::RichText::new(time_title).size(15.0).strong());

            let pending = self.pending_x.take();
            let pending_was_set = pending.is_some();

            let time_response = Plot::new("time")
                .height(pane_h - 24.0)
                .x_axis_label("t")
                .y_axis_label("I(t)")
                .x_axis_formatter(|gm, _r| fmt_si(gm.value * 1e-3, "s"))
                .y_axis_formatter(|gm, _r| fmt_si(gm.value, "A"))
                .label_formatter(|_name, p| {
                    format!(
                        "t = {}\nI(t) = {}",
                        fmt_si(p.x * 1e-3, "s"),
                        fmt_si(p.y, "A"),
                    )
                })
                .allow_zoom([true, false])
                .allow_drag([true, false])
                .allow_boxed_zoom(true)
                .show(ui, |pui| {
                    if let Some((xl, xh)) = pending {
                        let cur = pui.plot_bounds();
                        pui.set_plot_bounds(PlotBounds::from_min_max(
                            [xl, cur.min()[1]],
                            [xh, cur.max()[1]],
                        ));
                    }
                    let cur = pui.plot_bounds();
                    let xl = cur.min()[0];
                    let xh = cur.max()[0];
                    let (yl, yh) = data_y_range_in_x(&snapshot, xl, xh, SAMPLE_RATE);
                    pui.set_plot_bounds(PlotBounds::from_min_max([xl, yl], [xh, yh]));
                    pui.line(Line::new(time_points).name("signal"));
                    (xl, xh)
                });
            let (obs_lo, obs_hi) = time_response.inner;

            let plot_rect = time_response.response.rect;
            let overlay_w = 112.0;
            egui::Area::new(egui::Id::new("zoom-overlay"))
                .fixed_pos(plot_rect.right_top() + egui::vec2(-overlay_w - 8.0, 8.0))
                .order(egui::Order::Foreground)
                .interactable(true)
                .show(ui.ctx(), |aui| {
                    egui::Frame::default()
                        .fill(egui::Color32::from_black_alpha(180))
                        .stroke(egui::Stroke::new(1.0, egui::Color32::from_gray(70)))
                        .rounding(6.0)
                        .inner_margin(egui::Margin::same(3.0))
                        .show(aui, |aui| {
                            aui.horizontal(|aui| {
                                let mk = |glyph: &str| {
                                    egui::Button::new(egui::RichText::new(glyph).size(15.0))
                                        .min_size(egui::vec2(26.0, 22.0))
                                };
                                if aui
                                    .add_enabled(can_undo, mk("↶"))
                                    .on_hover_text("Undo zoom  (← / ⌘Z)")
                                    .clicked()
                                {
                                    overlay_undo = true;
                                }
                                if aui
                                    .add_enabled(can_redo, mk("↷"))
                                    .on_hover_text("Redo zoom  (→ / ⇧⌘Z)")
                                    .clicked()
                                {
                                    overlay_redo = true;
                                }
                                if aui
                                    .add_enabled(can_reset_now, mk("⟲"))
                                    .on_hover_text("Reset to full view  (r)")
                                    .clicked()
                                {
                                    overlay_reset = true;
                                }
                            });
                        });
                });

            let need_update = match self.view_x {
                Some((vx_lo, vx_hi)) => {
                    let span = (vx_hi - vx_lo).abs().max(1.0);
                    let dev = ((obs_lo - vx_lo).abs() + (obs_hi - vx_hi).abs()) / span;
                    dev > 1e-3
                }
                None => true,
            };
            if need_update && !pending_was_set {
                let now = Instant::now();
                let new_gesture = self.last_view_change_at.map_or(true, |t| {
                    now.duration_since(t) > Duration::from_millis(300)
                });
                if new_gesture {
                    if let Some(prev) = self.view_x {
                        self.push_undo(prev);
                    }
                }
                self.view_x = Some((obs_lo, obs_hi));
                self.last_view_change_at = Some(now);
            } else if pending_was_set {
                self.view_x = Some((obs_lo, obs_hi));
            }

            ui.add_space(6.0);

            let nyq = (SAMPLE_RATE as f64) * 0.5;
            let freq_title = if self.show_details {
                match &self.latest_result {
                    Some(r) => format!(
                        "Frequency domain — |I|(f)   f \u{2208} [{}, {}]   slice = {} pts",
                        fmt_si(0.0, "Hz"),
                        fmt_si(nyq, "Hz"),
                        r.n,
                    ),
                    None => format!(
                        "Frequency domain — |I|(f)   f \u{2208} [{}, {}]   (computing\u{2026})",
                        fmt_si(0.0, "Hz"),
                        fmt_si(nyq, "Hz"),
                    ),
                }
            } else {
                format!(
                    "Frequency domain — |I|(f)   f \u{2208} [{}, {}]",
                    fmt_si(0.0, "Hz"),
                    fmt_si(nyq, "Hz"),
                )
            };
            ui.label(egui::RichText::new(freq_title).size(15.0).strong());

            Plot::new("freq")
                .height(pane_h - 24.0)
                .x_axis_label("f")
                .y_axis_label("|I|(f)")
                .x_axis_formatter(|gm, _r| fmt_si(gm.value, "Hz"))
                .y_axis_formatter(|gm, _r| fmt_si(gm.value, "A"))
                .label_formatter(|_name, p| {
                    format!(
                        "f = {}\n|I|(f) = {}",
                        fmt_si(p.x, "Hz"),
                        fmt_si(p.y, "A"),
                    )
                })
                .include_x(0.0)
                .include_x(nyq)
                .show(ui, |pui| {
                    if let Some(r) = &self.latest_result {
                        let pts: PlotPoints = r
                            .freqs
                            .iter()
                            .zip(r.mags.iter())
                            .map(|(&f, &m)| [f as f64, m as f64])
                            .collect();
                        pui.line(Line::new(pts).name("|I|(f)"));
                    }
                });
        });

        if overlay_undo {
            self.act_undo();
        }
        if overlay_redo {
            self.act_redo();
        }
        if overlay_reset {
            self.act_reset(buf_t_lo_ms, buf_t_hi_ms);
        }

        ctx.request_repaint_after(Duration::from_millis(40));
    }
}

// ============================================================================
// Entry points
// ============================================================================

#[cfg(not(target_arch = "wasm32"))]
#[derive(Parser, Debug)]
#[command(about = "Flicker Scope — host UI for the Pico flicker detector.")]
struct CliArgs {
    /// Use the built-in synthetic signal instead of a real Pico.
    #[arg(long)]
    demo: bool,
    /// Explicit serial port (e.g. /dev/cu.usbmodem14101). Auto-detected if omitted.
    #[arg(long)]
    port: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
fn main() -> eframe::Result<()> {
    let args = CliArgs::parse();

    let shared = Arc::new(Mutex::new(SharedSamples {
        samples: VecDeque::with_capacity(BUF_LEN),
    }));
    let running = Arc::new(AtomicBool::new(true));
    let emitted = Arc::new(AtomicU64::new(0));

    if args.demo {
        eprintln!("[source] demo — synthetic signal");
        spawn_synth_source(shared.clone(), running.clone(), emitted.clone());
    } else {
        let port = args.port.clone().or_else(auto_detect_pico_port);
        match port {
            Some(p) => {
                eprintln!("[source] pico on {} @ {} Hz", p, SAMPLE_RATE as u32);
                if let Err(e) = spawn_pico_source(
                    p,
                    shared.clone(),
                    running.clone(),
                    emitted.clone(),
                ) {
                    eprintln!("[source] failed to open Pico: {e}; falling back to demo.");
                    spawn_synth_source(shared.clone(), running.clone(), emitted.clone());
                }
            }
            None => {
                eprintln!("[source] no Pico detected; falling back to demo. \
                           Pass --port <path> to override.");
                spawn_synth_source(shared.clone(), running.clone(), emitted.clone());
            }
        }
    }

    let (job_tx, job_rx) = bounded::<FftJob>(2);
    let (result_tx, result_rx) = bounded::<FftResult>(4);
    let _worker = spawn_fft_worker(job_rx, result_tx);

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_fullscreen(true),
        ..Default::default()
    };
    eframe::run_native(
        "Flicker Scope",
        options,
        Box::new(move |cc| {
            cc.egui_ctx.set_pixels_per_point(1.4);
            Ok(Box::new(App::new(
                shared, running, emitted, job_tx, result_rx,
            )))
        }),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    use wasm_bindgen::JsCast;

    console_error_panic_hook::set_once();

    wasm_bindgen_futures::spawn_local(async {
        let document = web_sys::window().unwrap().document().unwrap();
        let canvas = document
            .get_element_by_id("the-canvas")
            .expect("missing canvas with id=the-canvas")
            .dyn_into::<web_sys::HtmlCanvasElement>()
            .expect("element #the-canvas is not a canvas");

        eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(|cc| {
                    cc.egui_ctx.set_pixels_per_point(1.2);
                    Ok(Box::new(App::new_wasm()))
                }),
            )
            .await
            .expect("failed to start eframe web runner");
    });
}

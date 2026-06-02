//! Pitch + formant DSP primitives — `Stretch48k`, `SmoothedVoiceParams`, `Gate`, `Yin48k`.
//!
//! Populated by Plan 02-01 as a TYPE-CONTRACT SKELETON. Wave 1 plans (02-04 through 02-07)
//! fill in the bodies; this plan locks every public signature so downstream tasks land
//! against a known surface. Mirrors the Phase 1 `resampler.rs` pattern verbatim — every
//! struct here is constructed OFF the audio thread (during worker spawn or via the
//! off-RT preset-rebuild path) and exposes a single per-block hot-path method callable
//! from inside `assert_no_alloc(|| { ... })` (RESEARCH §Pattern 1).
//!
//! ## What lives here
//! - [`Preset`]: three-variant quality enum (Low / Balanced / Quality) per D-26 + RESEARCH §Q2.
//!   `Preset::window_hop()` returns the STFT `(block_length, interval)` pair that fits each
//!   latency budget. Starting points; execute-time A/B may tighten Quality (D-25).
//! - [`Stretch48k`]: wrapper around `signalsmith_stretch::Stretch` with the preset
//!   stashed. Constructed off-RT; `process(&[f32], &mut [f32])` is the per-block hot path.
//!   `set_transpose(m)` / `set_formant(m)` adopt D-24's locked `compensate_pitch = true` so
//!   callers cannot accidentally disable independent pitch + formant control.
//! - [`SmoothedVoiceParams`]: pure-Rust per-block exponential interpolator (RESEARCH
//!   §Pattern 3 + Example B). 30 ms time-constant per D-35; `step(target_pitch, target_formant)`
//!   is the per-block call between `triple_buffer<VoiceParams>::Output::read()` and the
//!   `Stretch48k::set_transpose` / `set_formant` setters. Without this, slider drags produce
//!   zipper noise (CONTEXT Pitfall #7).
//! - [`Gate`]: RMS gate with hysteresis (open at −45 dBFS, close at −50 dBFS, 50 ms hold-open)
//!   per D-30. `update(raw_input_rms)` returns the gate-open boolean; gate-closed → worker
//!   emits true digital silence (D-29).
//! - [`Yin48k`]: wraps `pitch_detection::detector::yin::YINDetector<f32>` with a 512-sample
//!   window per D-32. `BufferPool` pre-allocated at construction so `get_pitch(&[f32])` is
//!   alloc-free on the hot path (verified against 0.3.0 source, RESEARCH §Q4).
//! - [`rms_simd`]: free function — `wide::f32x8` SIMD RMS over a sample slice. Hot-path
//!   helper for D-34 SIMD acceleration; replaces the scalar `.map(|s| s*s).sum()` in
//!   `cpal_io::capture` and the Gate's per-block RMS read.
//!
//! ## No per-call allocation invariant
//! Every per-block method below performs ONLY:
//! - slice reads / writes against caller-supplied buffers,
//! - stack-local f32 arithmetic against `self`'s preexisting fields,
//! - calls into upstream crates (`signalsmith_stretch::Stretch::process` / setters,
//!   `pitch_detection::YINDetector::get_pitch`, `wide::f32x8` ops) whose docs guarantee
//!   no heap allocation.
//!
//! No `Vec::push`, no `Vec::extend`, no `Vec::with_capacity`, no `Box::new`. The
//! `dsp_assert_no_alloc_loop` integration test (Plan 02-09) verifies the contract under
//! the global `AllocDisabler`.
//!
//! ## Construction discipline
//! All `new()` calls live on the DSP worker spawn path OR on the engine event-loop thread
//! (preset rebuild via `EngineCommand::SetPreset`, Plan 02-08). NEVER inside the worker's
//! `loop { ... }` body, NEVER inside the cpal callback.

use pitch_detection::detector::yin::YINDetector;
use signalsmith_stretch::Stretch;

use crate::cpal_io::{BLOCK, SAMPLE_RATE_HZ};

/// Engine-wide sample rate constant re-exported for callers who want a single import. Equal
/// to [`SAMPLE_RATE_HZ`] from `cpal_io` — 48 kHz, fixed (D-05). The duplicate lives here so
/// dsp.rs is self-contained as a module surface; both constants resolve to the same value.
pub const ENGINE_SR: u32 = SAMPLE_RATE_HZ;

/// Three quality presets exposed in the Ready shell's segmented row (D-26). Each maps to a
/// distinct STFT `(block_length, interval)` pair that fits the latency budget for its tier
/// (D-25 — Low <32 ms, Balanced <40 ms, Quality <50 ms total round-trip).
///
/// Derived `Copy + Clone + Debug + Eq + PartialEq` so the future `EngineCommand::SetPreset(Preset)`
/// variant (Plan 02-08) inherits its `#[derive]`s without friction.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Preset {
    /// <32 ms total round-trip target. STFT (1024, 256) starting point per RESEARCH §Q2.
    Low,
    /// <40 ms total round-trip target. STFT (2048, 512) starting point per RESEARCH §Q2.
    Balanced,
    /// <50 ms total round-trip target. STFT (3072, 768) starting point per RESEARCH §Q2.
    Quality,
}

impl Preset {
    /// Return the `(block_length, interval)` STFT window/hop pair for this preset.
    ///
    /// 4:1 block-to-hop ratio matches the upstream `presetDefault` overlap and is the
    /// phase-vocoder sweet spot for quality. These are STARTING POINTS — the execute-time
    /// A/B sprint in Plan 02-04 may tighten Quality (D-25 — quality-validate after the
    /// latency budget is met).
    pub fn window_hop(self) -> (usize, usize) {
        match self {
            Preset::Low => (1024, 256),
            Preset::Balanced => (2048, 512),
            Preset::Quality => (3072, 768),
        }
    }
}

/// Allocation-free wrapper around `signalsmith_stretch::Stretch` mirroring the Phase 1
/// `Resampler48k` per-block pattern (RESEARCH §Pattern 1 + Example A).
///
/// ## Lifecycle
/// - Constructed OFF the audio thread (DSP worker spawn, or engine event-loop thread on
///   preset rebuild via `EngineCommand::SetPreset` — Plan 02-08).
/// - Owned exclusively by the DSP worker thread; never wrapped in `Mutex` or shared
///   across threads. Preset switches hand a fresh instance via crossbeam-channel.
/// - `process(&scratch, &mut processed)` is called every audio block. Per upstream docs
///   the call passes raw pointers to C++ and performs zero Rust-side allocation, so the
///   worker's `assert_no_alloc(|| { ... })` wrap holds.
///
/// ## D-28 warm contract
/// During Bypass the worker STILL calls `process()` every block so the internal
/// phase-vocoder state stays continuous; only the buffer pushed to `vo_tx` differs
/// (raw scratch vs processed). Toggling Bypass off must not glitch.
pub struct Stretch48k {
    /// The wrapped signalsmith Stretch instance. Owns the C++ allocations; built with
    /// channel_count=1 (mono engine end-to-end per `cpal_io::INPUT_CHANNELS`).
    inner: Stretch,
    /// Preset this instance was constructed for. Preserved so the UI can read it back
    /// from `Stretch48k::preset()` for the segmented row highlight.
    preset: Preset,
}

impl Stretch48k {
    /// Construct a new Stretch instance for the given preset. Calls
    /// `signalsmith_stretch::Stretch::new(1, block_length, interval)` with the
    /// preset's STFT pair from [`Preset::window_hop`].
    ///
    /// CRITICAL: MUST be called off the audio thread. The upstream constructor performs
    /// the one-time C++ buffer allocation; calling it from inside `assert_no_alloc(|| ...)`
    /// would trip the debug-build allocation counter.
    pub fn new(preset: Preset) -> Self {
        let (block_length, interval) = preset.window_hop();
        let inner = Stretch::new(1u32, block_length, interval);
        Self { inner, preset }
    }

    /// Total Stretch latency contribution in samples. Read at plan-time + per-preset by
    /// the Plan 02-04 latency-budget test:
    ///
    /// ```ignore
    /// let s = Stretch48k::new(preset);
    /// let latency_ms = s.latency_samples() as f32 / 48.0;
    /// assert!(latency_ms < (preset_budget_ms - 12.7));
    /// ```
    ///
    /// 12.7 ms is the cpal capture+playback in-flight + scheduling slack overhead per
    /// RESEARCH §Q2.
    pub fn latency_samples(&self) -> usize {
        self.inner.input_latency() + self.inner.output_latency()
    }

    /// Read back the preset this instance was constructed for. Used by the Ready shell
    /// segmented row to highlight the active preset.
    pub fn preset(&self) -> Preset {
        self.preset
    }

    /// Per-block DSP. Filled in by Plan 02-04 — body becomes `self.inner.process(input, output);`.
    /// Upstream docs: zero allocation; raw pointer pass to C++.
    pub fn process(&mut self, _input: &[f32], _output: &mut [f32]) {
        unimplemented!("filled in by Plan 02-04 — body is self.inner.process(_input, _output)")
    }

    /// Set the per-block pitch transpose multiplier. Wraps `set_transpose_factor(m, None)`.
    /// Plan 02-04 fills in: `self.inner.set_transpose_factor(multiplier, None);`
    pub fn set_transpose(&mut self, _multiplier: f32) {
        unimplemented!(
            "filled in by Plan 02-04 — body is self.inner.set_transpose_factor(_multiplier, None)"
        )
    }

    /// Set the per-block formant multiplier with `compensate_pitch = true` LOCKED per D-24.
    /// The boolean is intentionally not exposed on this surface so callers cannot defeat
    /// independent pitch + formant control.
    /// Plan 02-04 fills in: `self.inner.set_formant_factor(multiplier, true);`
    pub fn set_formant(&mut self, _multiplier: f32) {
        unimplemented!(
            "filled in by Plan 02-04 — body is self.inner.set_formant_factor(_multiplier, true) (D-24 locks compensate_pitch=true)"
        )
    }
}

/// Per-block exponential interpolator that smooths raw slider values before they reach
/// `Stretch48k::set_transpose` / `set_formant`. Without this, slider drags produce zipper
/// noise (CONTEXT Pitfall #7). 30 ms time-constant per D-35.
///
/// ## Math (RESEARCH §Pattern 3 + Example B)
/// - `tau_samples = (tau_ms / 1000) * 48_000` → 1440 for 30 ms @ 48 kHz.
/// - `alpha = 1.0 - exp(-block_samples / tau_samples)` → ≈ 0.163 for BLOCK=256, 30 ms.
/// - Per block: `current += alpha * (target - current)` for each of pitch and formant.
///
/// `alpha` is precomputed at construction (a const for fixed BLOCK + tau).
// Fields are written by `new()` and read by `pitch()` / `formant()` accessors, but `step()`
// (the only place that mutates them per block) is stubbed out until Plan 02-05 fills in the
// body. The `alpha` field is read only by the future `step()` body. `#[allow(dead_code)]` is
// scoped narrowly to this stub-phase struct and will become a no-op once Plan 02-05 lands.
#[allow(dead_code)]
#[derive(Copy, Clone, Debug)]
pub struct SmoothedVoiceParams {
    /// Current smoothed pitch multiplier. Initialized to `initial_pitch` from `new()`.
    pitch_current: f32,
    /// Current smoothed formant multiplier. Initialized to `initial_formant` from `new()`.
    formant_current: f32,
    /// One-pole filter coefficient `1.0 - exp(-block_samples / tau_samples)`. Precomputed
    /// once at construction; same value applies to both pitch and formant smoothing.
    alpha: f32,
}

impl SmoothedVoiceParams {
    /// Construct with the initial target values and the smoothing time-constant. Called
    /// once at DSP worker spawn; `initial_pitch` / `initial_formant` come from the default
    /// VoiceParams (D-22 — pitch 1.65×, formant 1.18×).
    ///
    /// `block_samples` is [`BLOCK`] (256); `tau_ms` is 30.0 (D-35). Both are passed
    /// explicitly so test code can drive alternative time-constants without going through
    /// crate constants.
    pub fn new(
        initial_pitch: f32,
        initial_formant: f32,
        block_samples: usize,
        tau_ms: f32,
    ) -> Self {
        let tau_samples = (tau_ms / 1000.0) * ENGINE_SR as f32;
        let alpha = 1.0 - (-(block_samples as f32) / tau_samples).exp();
        Self {
            pitch_current: initial_pitch,
            formant_current: initial_formant,
            alpha,
        }
    }

    /// Per-block step. Called by the DSP worker AFTER reading the latest VoiceParams
    /// snapshot from `triple_buffer<VoiceParams>::Output::read()`. Plan 02-05 fills in:
    /// ```ignore
    /// self.pitch_current   += self.alpha * (target_pitch   - self.pitch_current);
    /// self.formant_current += self.alpha * (target_formant - self.formant_current);
    /// ```
    pub fn step(&mut self, _target_pitch: f32, _target_formant: f32) {
        unimplemented!(
            "filled in by Plan 02-05 — body is two `current += alpha * (target - current)` lines"
        )
    }

    /// Read the current smoothed pitch multiplier. Plan 02-05 wires this to
    /// `Stretch48k::set_transpose(self.smoothed.pitch())` per block.
    pub fn pitch(&self) -> f32 {
        self.pitch_current
    }

    /// Read the current smoothed formant multiplier. Plan 02-05 wires this to
    /// `Stretch48k::set_formant(self.smoothed.formant())` per block.
    pub fn formant(&self) -> f32 {
        self.formant_current
    }
}

/// RMS silence gate with hysteresis and 50 ms hold-open (D-30 — hardcoded thresholds).
/// Operates on input RMS read via `Telemetry::input_rms.load(Relaxed)`; the worker calls
/// `gate.update(raw_rms)` once per block and emits true digital silence to `vo_tx` when
/// the gate is closed (D-29).
///
/// ## Threshold math (D-30, RESEARCH §Q5 + Example C)
/// - `open_threshold  = 10^(-45/20) ≈ 0.005623` (open at −45 dBFS)
/// - `close_threshold = 10^(-50/20) ≈ 0.003162` (close at −50 dBFS)
/// - `hold_open_samples = 0.050 * 48_000 = 2400` (50 ms)
/// - `alpha_attack  = 1 - exp(-BLOCK / 480)`  (10 ms attack)
/// - `alpha_release = 1 - exp(-BLOCK / 2400)` (50 ms release)
///
/// The 5 dB hysteresis gap prevents chattering — a level hovering between the two
/// thresholds cannot toggle the state.
///
/// All fields are written by `new()` and consumed by `update()`, which is stubbed until
/// Plan 02-05 fills in the body per RESEARCH §Example C. `#[allow(dead_code)]` is scoped
/// narrowly to this stub-phase struct and becomes a no-op once Plan 02-05 lands.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Gate {
    /// Current open/closed state. `false` at construction → gate starts closed; the first
    /// block of audio above `open_threshold` will open it.
    is_open: bool,
    /// One-pole envelope-follower output, smoothed `raw_input_rms` via the attack/release
    /// coefficients. Used to drive the threshold comparisons.
    smoothed_rms: f32,
    /// Count of consecutive samples since `smoothed_rms` last went below `close_threshold`,
    /// in BLOCK-sized increments. When this reaches `hold_open_samples`, the gate closes.
    samples_since_below: usize,
    /// Open threshold in linear amplitude (−45 dBFS ≈ 0.005623).
    open_threshold: f32,
    /// Close threshold in linear amplitude (−50 dBFS ≈ 0.003162). Strictly less than
    /// `open_threshold` — the gap is the hysteresis band.
    close_threshold: f32,
    /// 50 ms of audio in samples at 48 kHz = 2400. After the smoothed RMS sits below
    /// `close_threshold` for this many samples, the gate closes.
    hold_open_samples: usize,
    /// One-pole attack coefficient (rising-level smoothing). 10 ms time-constant.
    alpha_attack: f32,
    /// One-pole release coefficient (falling-level smoothing). 50 ms time-constant.
    alpha_release: f32,
}

impl Gate {
    /// Construct a closed gate with the D-30 hardcoded thresholds and time-constants.
    /// All coefficients computed from BLOCK; nothing runtime-tunable in Phase 2 (the
    /// user-facing threshold slider is Phase 4 / VOICE-03).
    pub fn new() -> Self {
        Self {
            is_open: false,
            smoothed_rms: 0.0,
            samples_since_below: 0,
            open_threshold: 0.005623,
            close_threshold: 0.003162,
            hold_open_samples: 2400,
            alpha_attack: 1.0 - (-(BLOCK as f32) / 480.0).exp(),
            alpha_release: 1.0 - (-(BLOCK as f32) / 2400.0).exp(),
        }
    }

    /// Per-block update. Plan 02-05 fills in the envelope-follower + hysteresis state
    /// machine per RESEARCH §Example C. Returns `true` when the gate is open (worker
    /// pushes processed audio) or `false` when closed (worker emits zeros — D-29).
    pub fn update(&mut self, _raw_input_rms: f32) -> bool {
        unimplemented!("filled in by Plan 02-05 — body is RESEARCH §Example C verbatim (envelope follower + hysteresis state machine)")
    }
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

/// YIN F0 estimator (D-32) wrapping `pitch_detection::detector::yin::YINDetector<f32>`.
/// 512-sample window per D-32 (~10 ms @ 48 kHz); evaluated at ~30 Hz from the DSP worker
/// via a subsample counter (RESEARCH §Pitfall 5).
///
/// ## Allocation profile
/// `YINDetector::new(512, 0)` allocates a `BufferPool<f32>` at construction; subsequent
/// `get_pitch` calls borrow from the pool via `RefCell` so the hot path is alloc-free
/// (verified against pitch-detection 0.3.0 source per RESEARCH §Q4). The `padding=0`
/// argument disables rustfft zero-padding, keeping the hot path tighter.
///
/// `detector` is consumed by the stubbed `get_pitch()` body Plan 02-06 fills in.
/// `#[allow(dead_code)]` is scoped narrowly to this stub-phase struct and becomes a no-op
/// once Plan 02-06 lands.
#[allow(dead_code)]
pub struct Yin48k {
    /// The wrapped YIN detector. Owns the pre-allocated BufferPool scratch.
    detector: YINDetector<f32>,
}

impl Yin48k {
    /// Construct with a 512-sample window and zero padding (D-32 + RESEARCH §Q4). Called
    /// once at DSP worker spawn; the BufferPool allocation lives off the audio path.
    pub fn new() -> Self {
        Self {
            detector: YINDetector::new(512, 0),
        }
    }

    /// Estimate F0 of a 512-sample window. Returns `Some(hz)` when voiced (clarity above
    /// threshold), `None` when unvoiced — the UI renders "—" on the unvoiced branch (D-32).
    ///
    /// Plan 02-06 fills in:
    /// ```ignore
    /// use pitch_detection::detector::PitchDetector;
    /// const POWER_THRESHOLD: f32 = 0.0;
    /// const CLARITY_THRESHOLD: f32 = 0.93;
    /// self.detector
    ///     .get_pitch(signal, ENGINE_SR as usize, POWER_THRESHOLD, CLARITY_THRESHOLD)
    ///     .map(|p| p.frequency)
    /// ```
    pub fn get_pitch(&mut self, _signal: &[f32]) -> Option<f32> {
        unimplemented!(
            "filled in by Plan 02-06 — body wraps `self.detector.get_pitch(_signal, ENGINE_SR as usize, 0.0, 0.93).map(|p| p.frequency)`"
        )
    }
}

impl Default for Yin48k {
    fn default() -> Self {
        Self::new()
    }
}

/// SIMD-accelerated RMS over a sample slice using `wide::f32x8` (D-34, RESEARCH §Q7).
/// Hot-path replacement for the scalar `.map(|s| s*s).sum()` pattern in `cpal_io::capture`
/// and the Gate's per-block RMS computation; the `dsp_simd_rms_parity` test (Plan 02-07)
/// asserts byte-equivalence with the scalar version within 1e-6.
///
/// Returns `sqrt(sum_of_squares / len)` — the standard linear-amplitude RMS. Returns
/// `0.0` for an empty slice (matches scalar behavior — `0/1` for the `len.max(1)` divisor
/// pattern used in `cpal_io`). Plan 02-07 fills in the actual SIMD body.
pub fn rms_simd(_samples: &[f32]) -> f32 {
    unimplemented!(
        "filled in by Plan 02-07 — body chunks into wide::f32x8, accumulates squares, sqrt(sum / len)"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test that the locked Preset → (window, hop) pairs match RESEARCH §Q2.
    /// If Plan 02-04's A/B sprint tightens any of these, update this assertion in
    /// lock-step with the `Preset::window_hop` body.
    #[test]
    fn preset_window_hop_pairs_match_research() {
        assert_eq!(Preset::Low.window_hop(), (1024, 256));
        assert_eq!(Preset::Balanced.window_hop(), (2048, 512));
        assert_eq!(Preset::Quality.window_hop(), (3072, 768));
    }
}

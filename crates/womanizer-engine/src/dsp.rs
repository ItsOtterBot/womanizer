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
//!   Defined in `womanizer-core::primitives` as of Plan 02-02 (so `EngineCommand::SetPreset(Preset)`
//!   can reference it without a circular crate dep — Pattern G); re-exported here for ergonomic
//!   `use crate::dsp::Preset`. [`preset_window_hop`] returns the STFT `(block_length, interval)`
//!   pair that fits each latency budget. Starting points; execute-time A/B may tighten Quality (D-25).
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
use pitch_detection::detector::PitchDetector;
use signalsmith_stretch::Stretch;

use crate::cpal_io::{BLOCK, SAMPLE_RATE_HZ};

// Phase 2 Plan 02-02 moved the canonical `Preset` definition into `womanizer-core::primitives`
// so the new `EngineCommand::SetPreset(Preset)` variant can reference it without a circular
// crate dep (Pattern G / PATTERNS.md decision (a)). Inherent impls must live on the
// defining crate's type, so `Preset::window_hop` becomes the free function
// `preset_window_hop` below.
pub use womanizer_core::Preset;

/// Engine-wide sample rate constant re-exported for callers who want a single import. Equal
/// to [`SAMPLE_RATE_HZ`] from `cpal_io` — 48 kHz, fixed (D-05). The duplicate lives here so
/// dsp.rs is self-contained as a module surface; both constants resolve to the same value.
pub const ENGINE_SR: u32 = SAMPLE_RATE_HZ;

/// Return the `(block_length, interval)` STFT window/hop pair for the given preset.
///
/// 4:1 block-to-hop ratio matches the upstream `presetDefault` overlap and is the
/// phase-vocoder sweet spot for quality. These are STARTING POINTS — the execute-time
/// A/B sprint in Plan 02-04 may tighten Quality (D-25 — quality-validate after the
/// latency budget is met).
///
/// Free function rather than `Preset::window_hop` because [`Preset`] is defined in
/// `womanizer-core::primitives` (Plan 02-02; Pattern G — fields/types that cross thread
/// boundaries live there so [`EngineCommand::SetPreset`] can reference Preset without a
/// circular crate dep). Rust requires inherent impls to live on the defining crate's type.
pub fn preset_window_hop(preset: Preset) -> (usize, usize) {
    match preset {
        Preset::Low => (1024, 256),
        Preset::Balanced => (2048, 512),
        Preset::Quality => (3072, 768),
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
    /// preset's STFT pair from [`preset_window_hop`].
    ///
    /// CRITICAL: MUST be called off the audio thread. The upstream constructor performs
    /// the one-time C++ buffer allocation; calling it from inside `assert_no_alloc(|| ...)`
    /// would trip the debug-build allocation counter.
    pub fn new(preset: Preset) -> Self {
        let (block_length, interval) = preset_window_hop(preset);
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

    /// Per-block DSP — delegates to the upstream phase-vocoder.
    ///
    /// Zero allocation per upstream — the wrapper passes raw pointers to the C function.
    /// Verified safe inside `assert_no_alloc(|| { … })` by `tests/dsp_assert_no_alloc_loop.rs`
    /// (Plan 02-09). The call IS the per-block DSP hot path; the worker calls this every
    /// block regardless of bypass state (D-28 warm contract).
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        self.inner.process(input, output);
    }

    /// Set the per-block pitch transpose multiplier. Wraps
    /// `Stretch::set_transpose_factor(m, None)` — the second `None` argument disables the
    /// upstream tonality-tilt feature (we do not expose tonality on this Phase 2 surface).
    ///
    /// `debug_assert!` guards against the future Phase 4 import path passing a non-positive
    /// ratio (V5 input validation per RESEARCH §Security Domain). UI slider ranges (D-23)
    /// already clamp to `[1.20, 2.00]`; the assert is defense-in-depth.
    pub fn set_transpose(&mut self, multiplier: f32) {
        debug_assert!(
            multiplier > 0.0,
            "Stretch transpose multiplier must be > 0 (got {multiplier})"
        );
        self.inner.set_transpose_factor(multiplier, None);
    }

    /// Set the per-block formant multiplier with `compensate_pitch = true` LOCKED per D-24.
    ///
    /// `compensate_pitch=true` is LOCKED per CONTEXT.md D-24 — exposing it as a parameter
    /// would defeat DSP-01's independent-control contract. The boolean is intentionally
    /// not exposed on this surface so callers cannot defeat independent pitch + formant
    /// control. `debug_assert!` guards against a non-positive ratio (V5 input validation).
    pub fn set_formant(&mut self, multiplier: f32) {
        debug_assert!(
            multiplier > 0.0,
            "Stretch formant multiplier must be > 0 (got {multiplier})"
        );
        self.inner.set_formant_factor(multiplier, true);
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
// Fields are written by `new()`, read by `pitch()` / `formant()` accessors, and mutated by
// `step()` (Plan 02-05).
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
    /// snapshot from `triple_buffer<VoiceParams>::Output::read()`. Body is the textbook
    /// one-pole exponential interpolator: `current += alpha * (target - current)` for
    /// each of pitch and formant. Two lines, zero allocation, ~6 f32 ops per block.
    /// Plan 02-05.
    #[inline]
    pub fn step(&mut self, target_pitch: f32, target_formant: f32) {
        self.pitch_current += self.alpha * (target_pitch - self.pitch_current);
        self.formant_current += self.alpha * (target_formant - self.formant_current);
    }

    /// Read the current smoothed pitch multiplier. Wired by Plan 02-05 to
    /// `Stretch48k::set_transpose(self.smoothed.pitch())` per block.
    #[inline]
    pub fn pitch(&self) -> f32 {
        self.pitch_current
    }

    /// Read the current smoothed formant multiplier. Wired by Plan 02-05 to
    /// `Stretch48k::set_formant(self.smoothed.formant())` per block.
    #[inline]
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
/// All fields are written by `new()` and consumed by `update()` (Plan 02-05) per
/// RESEARCH §Example C.
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

    /// Per-block update — envelope-follower + hysteresis state machine per RESEARCH
    /// §Example C. Returns `true` when the gate is open (worker pushes processed audio)
    /// or `false` when closed (worker emits zeros — D-29). Plan 02-05.
    ///
    /// One-pole envelope follower picks `alpha_attack` on a rising raw RMS, `alpha_release`
    /// on a falling raw RMS — standard attack/release smoothing. Then the hysteresis
    /// state machine: while open, only close after `smoothed_rms` has stayed below
    /// `close_threshold` for `hold_open_samples` consecutive samples (BLOCK-quantized).
    /// While closed, open only when `smoothed_rms` crosses `open_threshold`. The 5 dB gap
    /// between the two thresholds prevents chatter when the level hovers in the dead zone.
    #[inline]
    pub fn update(&mut self, raw_input_rms: f32) -> bool {
        let alpha = if raw_input_rms > self.smoothed_rms {
            self.alpha_attack
        } else {
            self.alpha_release
        };
        self.smoothed_rms += alpha * (raw_input_rms - self.smoothed_rms);

        if self.is_open {
            if self.smoothed_rms < self.close_threshold {
                self.samples_since_below = self.samples_since_below.saturating_add(BLOCK);
                if self.samples_since_below >= self.hold_open_samples {
                    self.is_open = false;
                }
            } else {
                self.samples_since_below = 0;
            }
        } else if self.smoothed_rms > self.open_threshold {
            self.is_open = true;
            self.samples_since_below = 0;
        }
        self.is_open
    }

    /// Read the current gate state. Wired by the worker so the post-process `processed.fill(0.0)`
    /// (D-29 true digital silence) branch can be taken outside the assert_no_alloc closure if
    /// needed; in Plan 02-05 the worker reads the return value of `update()` directly to avoid
    /// a second read of self. Plan 02-05.
    #[inline]
    pub fn is_open(&self) -> bool {
        self.is_open
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
/// `detector` is consumed by `get_pitch()` (Plan 02-06 landed the body); fields are live.
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
    /// 0.85), `None` when unvoiced — the UI renders "—" on the unvoiced branch (D-32).
    ///
    /// Per RESEARCH §Q4 — the lower-than-default 0.85 clarity threshold (vs the crate's
    /// default 0.93) prevents false-unvoiced on low-volume mic input; tune at execute-time
    /// A/B if false-voiced becomes a problem. `POWER_THRESHOLD = 0.0` disables YIN's
    /// internal power gate (Phase 2 owns gating via the separate `Gate` state machine —
    /// D-30; YIN should not also gate).
    ///
    /// Allocation profile: `YINDetector::new(512, 0)` pre-allocates a `BufferPool` at
    /// construction (verified against pitch-detection 0.3.0 source); subsequent `get_pitch`
    /// calls borrow scratch via `RefCell::borrow_mut()` on the hot path, no heap allocation.
    /// The worker calls this inside `assert_no_alloc(|| { ... })`.
    pub fn get_pitch(&mut self, signal: &[f32]) -> Option<f32> {
        const POWER_THRESHOLD: f32 = 0.0;
        const CLARITY_THRESHOLD: f32 = 0.85;
        self.detector
            .get_pitch(
                signal,
                ENGINE_SR as usize,
                POWER_THRESHOLD,
                CLARITY_THRESHOLD,
            )
            .map(|p| p.frequency)
    }
}

impl Default for Yin48k {
    fn default() -> Self {
        Self::new()
    }
}

/// SIMD-accelerated RMS over a sample slice using `wide::f32x8` (D-34, RESEARCH §Q7).
/// Hot-path replacement for the scalar `.map(|s| s*s).sum()` pattern in `cpal_io::capture`;
/// the lib tests (`rms_simd_*` in this file's `tests` mod) and the
/// `dsp_simd_rms_parity` integration test (Plan 02-07) both assert byte-equivalence with the
/// scalar `sqrt(sum_sq / len.max(1))` reference within 1e-6 across silence / sine / noise /
/// constant / remainder-path inputs.
///
/// Returns `sqrt(sum_of_squares / len)` — the standard linear-amplitude RMS. Returns
/// `0.0` for an empty slice (matches scalar behavior via the `len.max(1)` divisor pattern
/// used in `cpal_io`).
///
/// ## Implementation (RESEARCH §Q7 Pattern (a))
/// - `chunks_exact(8)` over the input → contiguous 8-lane chunks. Each chunk loads into an
///   `f32x8` via `f32x8::new(arr)`; the running square accumulator is `acc + v * v`. The
///   lane-wise multiply + add is the f32x8 Add/Mul impls (verified against wide 1.4.0
///   `src/f32x8_.rs`). At end, `acc.to_array()` exposes the eight lane sums.
/// - The scalar `chunks.remainder()` slice handles the 0–7 leftover samples that do not
///   fit an f32x8 — required for non-multiple-of-8 inputs (e.g. the `cpal_io` capture
///   callback in shared-mode where the device hands us non-power-of-2 frame counts).
/// - Final reduction: sum of the 8 lanes plus the scalar remainder squares, divided by
///   `len.max(1)`, sqrt.
///
/// ## Allocation profile
/// `f32x8` is a `#[repr(C)]` value type; all loads / arithmetic happen on stack. No
/// `Vec::push`, no `Box::new`, no heap. Safe to call inside `assert_no_alloc(|| { ... })`
/// — `cpal_io::capture` does exactly this (Plan 02-07 Task 2 swap).
#[inline]
pub fn rms_simd(samples: &[f32]) -> f32 {
    use wide::f32x8;

    let mut acc = f32x8::ZERO;
    let chunks = samples.chunks_exact(8);
    let remainder = chunks.remainder();
    for c in chunks {
        // `chunks_exact(8)` guarantees `c.len() == 8`; the `try_into` is therefore
        // infallible. Constructing the array literal is a stack-local copy of 8 f32s into
        // an aligned `[f32; 8]`, which `f32x8::new` accepts directly.
        let arr: [f32; 8] = c
            .try_into()
            .expect("chunks_exact(8) yields exactly 8 elements");
        let v = f32x8::new(arr);
        acc += v * v;
    }
    let lane_sums = acc.to_array();
    let mut sum_sq: f32 = lane_sums.iter().copied().sum();
    for s in remainder {
        sum_sq += s * s;
    }
    (sum_sq / samples.len().max(1) as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test that the locked Preset → (window, hop) pairs match RESEARCH §Q2.
    /// If Plan 02-04's A/B sprint tightens any of these, update this assertion in
    /// lock-step with the [`preset_window_hop`] body.
    #[test]
    fn preset_window_hop_pairs_match_research() {
        assert_eq!(preset_window_hop(Preset::Low), (1024, 256));
        assert_eq!(preset_window_hop(Preset::Balanced), (2048, 512));
        assert_eq!(preset_window_hop(Preset::Quality), (3072, 768));
    }

    /// Construction smoke test for all three presets — verifies the upstream
    /// `Stretch::new(1, block, interval)` constructor succeeds for each Preset's STFT
    /// pair AND that the reported total latency is non-zero (the vocoder must take some
    /// internal latency) and bounded below 4000 samples (~83 ms @ 48 kHz — well above any
    /// preset's budget; a regression where preset_default-style construction creeps in
    /// would trip this).
    ///
    /// Protects against:
    /// - Future signalsmith version drift where construction parameters are silently
    ///   ignored or mis-applied (e.g., `preset_default` being used instead of explicit
    ///   `new(1, block, interval)`).
    /// - A planner accidentally re-introducing a preset whose internal window exceeds the
    ///   product's hard latency ceiling (80 ms — CLAUDE.md).
    #[test]
    fn stretch48k_constructs_for_all_presets() {
        for preset in [Preset::Low, Preset::Balanced, Preset::Quality] {
            let s = Stretch48k::new(preset);
            assert_eq!(s.preset(), preset);
            let lat = s.latency_samples();
            assert!(
                lat > 0,
                "Stretch48k::new({preset:?}) reported zero latency_samples — \
                 upstream construction may be a no-op or a future API change zeroed it out"
            );
            assert!(
                lat < 4000,
                "Stretch48k::new({preset:?}) reported latency_samples={lat} which exceeds \
                 the 4000-sample (~83 ms @ 48 kHz) ceiling — any preset above this blows the \
                 80 ms hard latency cap (CLAUDE.md); reject"
            );
        }
    }

    /// Source-level invariant gate: every `set_formant_factor(...)` CODE call in this file
    /// MUST pass `true` as the second argument (D-24 — compensate_pitch LOCKED).
    ///
    /// This is not a behavior test (the upstream Rust API exposes no getter for
    /// `compensate_pitch` state); it is a structural gate that catches a future planner
    /// or refactor accidentally swapping the `true` for `false` (which would defeat
    /// DSP-01's independent-control contract).
    ///
    /// Reads its own source file via `CARGO_MANIFEST_DIR`. Skips comment lines (lines whose
    /// trimmed prefix starts with `//`) — doc-comments naturally mention the symbol as a
    /// reference. Skips its own needle-literal line by constructing the needle at runtime
    /// from two string fragments so the whole needle never appears literally on one source
    /// line. For each remaining CODE line containing the needle, asserts the next 30 chars
    /// after the open paren contain `, true)`.
    #[test]
    fn stretch48k_set_formant_uses_compensate_pitch_true_grep() {
        let src_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/dsp.rs");
        let src = std::fs::read_to_string(&src_path)
            .expect("must be able to read src/dsp.rs from CARGO_MANIFEST_DIR");
        // Construct the needle at runtime from two fragments so the assembled string
        // `set_formant_factor(` does not appear as a literal on any source line of this
        // test — otherwise the literal here would match itself and trip the assertion.
        let needle = {
            let a = "set_formant_";
            let b = "factor(";
            format!("{a}{b}")
        };
        let mut found_any = false;
        for (lineno, line) in src.lines().enumerate() {
            // Skip comment lines (//, ///, //!) — doc-comments may name the symbol as
            // a reference without being an actual call site.
            if line.trim_start().starts_with("//") {
                continue;
            }
            let Some(idx) = line.find(needle.as_str()) else {
                continue;
            };
            let tail_start = idx + needle.len();
            // 30 chars is enough to span `multiplier, true)` (~17 chars) plus headroom.
            let tail_end = (tail_start + 30).min(line.len());
            let tail = &line[tail_start..tail_end];
            assert!(
                tail.contains(", true)"),
                "set_formant_factor code call on line {} does not pass `, true)` for \
                 compensate_pitch within the next 30 chars — D-24 LOCK violated. \
                 Found tail: {tail:?}",
                lineno + 1
            );
            found_any = true;
        }
        assert!(
            found_any,
            "no set_formant_factor code call found in src/dsp.rs — Stretch48k::set_formant \
             body has regressed away from delegating to the upstream setter"
        );
    }

    /// Helper: convert a dBFS level to its linear-amplitude equivalent. Used by the gate
    /// tests below to express their RMS levels in the same units as D-30's thresholds
    /// (open at −45 dBFS ≈ 0.005623, close at −50 dBFS ≈ 0.003162).
    fn dbfs_linear(db: f32) -> f32 {
        10f32.powf(db / 20.0)
    }

    /// SmoothedVoiceParams converges to its target within 5% after 20 blocks of constant
    /// drive (20 * 256 = 5120 samples ≈ 106.6 ms ≈ 3.5 time-constants of the 30 ms decay).
    ///
    /// Verifies the one-pole exponential math is wired correctly — without `step()` doing
    /// anything, the values would stay at their initial 1.0 and the assertion would fire.
    /// 5% is a generous tolerance for 3.5 τ; the textbook exponential math gives ≈ 3%
    /// residual at exactly 3.5 τ.
    #[test]
    fn smoothed_step_converges_to_target() {
        let mut s = SmoothedVoiceParams::new(1.0, 1.0, BLOCK, 30.0);
        let target_pitch = 1.65;
        let target_formant = 1.18;
        for _ in 0..20 {
            s.step(target_pitch, target_formant);
        }
        let pitch_err = (s.pitch() - target_pitch).abs() / target_pitch;
        let formant_err = (s.formant() - target_formant).abs() / target_formant;
        assert!(
            pitch_err < 0.05,
            "SmoothedVoiceParams pitch did not converge within 5% after 20 blocks: \
             current={}, target={target_pitch}, err={pitch_err}",
            s.pitch()
        );
        assert!(
            formant_err < 0.05,
            "SmoothedVoiceParams formant did not converge within 5% after 20 blocks: \
             current={}, target={target_formant}, err={formant_err}",
            s.formant()
        );
    }

    /// SmoothedVoiceParams precomputes alpha ≈ 0.163 for BLOCK=256 and tau=30 ms at 48 kHz
    /// per RESEARCH §Q6: `1.0 - exp(-256 / 1440) ≈ 0.16297`. Reads the alpha field via a
    /// behavioral probe — one `step(1.0, 0.0)` call from `current = 0.0` yields
    /// `current = alpha * (1.0 - 0.0) = alpha`. This indirectly verifies the constructor
    /// math without exposing the private field.
    #[test]
    fn smoothed_alpha_matches_30ms_tau() {
        let mut s = SmoothedVoiceParams::new(0.0, 0.0, BLOCK, 30.0);
        s.step(1.0, 1.0);
        let observed_alpha = s.pitch();
        let expected_alpha = 1.0 - (-(BLOCK as f32) / 1440.0).exp();
        let diff = (observed_alpha - expected_alpha).abs();
        assert!(
            diff < 0.001,
            "SmoothedVoiceParams alpha drift: observed={observed_alpha}, expected={expected_alpha}, diff={diff}"
        );
        // And the round number for RESEARCH §Q6 sanity:
        assert!(
            (observed_alpha - 0.163).abs() < 0.01,
            "SmoothedVoiceParams alpha at BLOCK=256, tau=30 ms diverged from the RESEARCH §Q6 \
             ≈ 0.163 figure: observed={observed_alpha}"
        );
    }

    /// Gate opens when smoothed RMS crosses the −45 dBFS open threshold (≈ 0.005623). Feed
    /// raw RMS at −40 dBFS (= 0.01, well above the threshold) for enough blocks for the
    /// 10 ms attack envelope to converge; assert `is_open() == true`.
    #[test]
    fn gate_opens_above_neg45_dbfs() {
        let mut g = Gate::new();
        let raw_rms = dbfs_linear(-40.0);
        // 40 blocks * 256 samples = 10240 samples ≈ 213 ms — well past 10 ms attack tau
        // (≈ 4 time-constants of the envelope). Smoothed RMS will sit just under raw_rms.
        for _ in 0..40 {
            g.update(raw_rms);
        }
        assert!(
            g.is_open(),
            "Gate failed to open after sustained raw_rms = {raw_rms} (−40 dBFS, above the −45 dBFS open threshold)"
        );
    }

    /// Gate closes when smoothed RMS sits below the −50 dBFS close threshold for the full
    /// hold-open window (2400 samples ≈ 50 ms). Open the gate first, then sweep to −60 dBFS
    /// (well below close) for enough blocks for both the release envelope AND the hold-open
    /// counter to elapse; assert `is_open() == false`.
    #[test]
    fn gate_closes_below_neg50_dbfs_with_hold() {
        let mut g = Gate::new();
        // Phase 1: open the gate via a loud signal.
        for _ in 0..40 {
            g.update(dbfs_linear(-40.0));
        }
        assert!(
            g.is_open(),
            "precondition: gate must be open before close test"
        );
        // Phase 2: switch to a very quiet signal; gate must close after hold-open elapses.
        // 50 blocks * 256 = 12800 samples — > 2400 hold-open AND > 50 ms release tau.
        for _ in 0..50 {
            g.update(dbfs_linear(-60.0));
        }
        assert!(
            !g.is_open(),
            "Gate failed to close after sustained −60 dBFS for 50 blocks (12800 samples)"
        );
    }

    /// Gate hysteresis: a level inside the 5 dB dead zone (between −50 dBFS close and
    /// −45 dBFS open) must NOT toggle the gate state. Open the gate with a loud signal,
    /// then sweep to −47 dBFS (between close and open) for ~1 second; assert the gate
    /// stays open the whole time.
    #[test]
    fn gate_hysteresis_intermediate_holds_open() {
        let mut g = Gate::new();
        // Phase 1: open the gate.
        for _ in 0..40 {
            g.update(dbfs_linear(-40.0));
        }
        assert!(g.is_open(), "precondition: gate must be open");
        // Phase 2: sweep to −47 dBFS (in the hysteresis band) for ~1 second of audio.
        // 200 blocks * 256 = 51200 samples ≈ 1.067 s — far longer than the 50 ms hold-open;
        // if the gate were going to close due to hysteresis-band drift it would have already.
        let mid = dbfs_linear(-47.0);
        for _ in 0..200 {
            g.update(mid);
            assert!(
                g.is_open(),
                "Gate closed mid-flight in the hysteresis band (−47 dBFS, between −45 open and −50 close)"
            );
        }
    }

    /// Helper: generate a `len`-sample window of a pure sine at `f_hz` and `amplitude`
    /// at the engine's 48 kHz sample rate. Mirrors the sine-generator pattern in
    /// `resampler.rs::tests` (RESEARCH §Q12 / PATTERNS.md identifies the resampler as the
    /// canonical analog).
    fn sine_window(f_hz: f32, amplitude: f32, len: usize) -> Vec<f32> {
        let phase_step = 2.0 * std::f32::consts::PI * f_hz / ENGINE_SR as f32;
        let mut phase = 0.0f32;
        let mut out = vec![0f32; len];
        for s in out.iter_mut() {
            *s = amplitude * phase.sin();
            phase += phase_step;
            if phase > 2.0 * std::f32::consts::PI {
                phase -= 2.0 * std::f32::consts::PI;
            }
        }
        out
    }

    /// Helper: deterministic linear congruential PRNG → uniform white noise in [-1, 1].
    /// Per Plan 02-06 action — use the classic glibc LCG (a=1103515245, c=12345) seeded at
    /// 12345 so the test is fully reproducible without bringing in a `rand` dep.
    fn lcg_noise(len: usize) -> Vec<f32> {
        let mut state: u32 = 12345;
        let mut out = Vec::with_capacity(len);
        for _ in 0..len {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            out.push((state as i32 as f32) / (i32::MAX as f32));
        }
        out
    }

    /// DSP-04 lib test 1: 220 Hz pure sine over 512 samples at 48 kHz → Yin48k returns
    /// Some(f) with |f - 220| < 2.0 Hz. The 2 Hz tolerance accommodates YIN's parabolic
    /// interpolation quantization (RESEARCH §Q4) without masking a clarity / power
    /// threshold regression in the wrapper.
    #[test]
    fn yin48k_returns_some_for_220hz_sine() {
        let window = sine_window(220.0, 0.5, 512);
        let mut yin = Yin48k::new();
        let result = yin.get_pitch(&window);
        let f = result.expect(
            "Yin48k::get_pitch must return Some for a clean 220 Hz sine — \
             None here would indicate the clarity threshold is too strict (RESEARCH §Q4)",
        );
        let err = (f - 220.0).abs();
        assert!(
            err < 2.0,
            "Yin48k::get_pitch returned {f} Hz for a 220 Hz sine; \
             error {err} Hz exceeds the 2 Hz YIN-interpolation tolerance"
        );
    }

    /// DSP-04 lib test 2: 512 samples of deterministic LCG white noise → Yin48k returns
    /// None (clarity below 0.85 threshold). Validates the unvoiced branch; without this,
    /// the worker would falsely store garbage F0 readings for whisper / breath / silence
    /// and the UI's `.is_nan()` "—" rendering (D-32) would never trigger.
    #[test]
    fn yin48k_returns_none_for_white_noise() {
        let window = lcg_noise(512);
        let mut yin = Yin48k::new();
        let result = yin.get_pitch(&window);
        assert!(
            result.is_none(),
            "Yin48k::get_pitch must return None for white noise (no periodic structure); \
             got {result:?} — clarity threshold may be too lenient or wrapper is misconfigured"
        );
    }

    /// Scalar reference RMS — byte-equivalent of the loop currently in `cpal_io.rs` at
    /// lines 477-479 (`let sum_sq: f32 = mono_native.iter().map(|s| s*s).sum(); (sum_sq /
    /// mono_native.len().max(1) as f32).sqrt()`). Plan 02-07's parity tests assert that
    /// `rms_simd` matches this within 1e-6 across all input shapes.
    fn scalar_rms(samples: &[f32]) -> f32 {
        let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
        (sum_sq / samples.len().max(1) as f32).sqrt()
    }

    /// DSP-06 lib test 1: zero buffer of 256 samples → rms_simd returns 0.0 exactly.
    /// Smallest possible signal; if rms_simd accumulates any garbage from f32x8 lane
    /// init or chunk remainder handling, this catches it cheaply.
    #[test]
    fn rms_simd_zero_buffer() {
        let input = [0.0f32; 256];
        let observed = rms_simd(&input);
        assert!(
            observed.abs() < 1e-6,
            "rms_simd([0.0; 256]) returned {observed}, expected 0.0"
        );
    }

    /// DSP-06 lib test 2: constant 0.5 buffer of 256 samples → rms_simd returns 0.5
    /// (RMS of a constant signal is the constant itself). Exercises every f32x8 lane with
    /// the same finite non-zero value; catches accumulator wiring bugs and divides-by-zero
    /// in the final `sqrt(sum / len)` step.
    #[test]
    fn rms_simd_constant_buffer() {
        let input = [0.5f32; 256];
        let observed = rms_simd(&input);
        assert!(
            (observed - 0.5).abs() < 1e-6,
            "rms_simd([0.5; 256]) returned {observed}, expected 0.5 (RMS of constant = constant)"
        );
    }

    /// DSP-06 lib test 3: 220 Hz sine over 256 samples at amplitude 0.5 → rms_simd matches
    /// the cpal_io.rs:477-479 scalar form within 1e-6. The CRITICAL parity gate — proves
    /// the SIMD implementation reproduces the exact arithmetic of the existing capture path,
    /// not a subtly different (e.g. Kahan-summed or population-vs-sample-RMS) variant.
    /// Six orders of magnitude tighter than the UI's three-decimal RMS display so any
    /// drift the user could perceive would trip this gate first.
    #[test]
    fn rms_simd_matches_scalar_220_sine() {
        let input = sine_window(220.0, 0.5, 256);
        let simd = rms_simd(&input);
        let scalar = scalar_rms(&input);
        assert!(
            (simd - scalar).abs() < 1e-6,
            "rms_simd vs scalar_rms parity broken on 220 Hz sine: simd={simd}, scalar={scalar}, diff={}",
            (simd - scalar).abs()
        );
    }

    /// DSP-06 lib test 4: 257-sample buffer (NOT a multiple of 8) — exercises the
    /// `chunks_exact(8)` scalar-tail remainder path. If the remainder loop is omitted or
    /// computes the wrong indices, the last sample is silently dropped and parity breaks
    /// at the 1/257 ≈ 0.4 % level — well above the 1e-6 tolerance.
    #[test]
    fn rms_simd_handles_remainder() {
        let mut input = sine_window(440.0, 0.7, 257);
        // Make the lone tail sample distinctly non-zero so its presence/absence visibly
        // moves the result: 257 % 8 = 1 → exactly one remainder lane.
        input[256] = 0.9;
        let simd = rms_simd(&input);
        let scalar = scalar_rms(&input);
        assert!(
            (simd - scalar).abs() < 1e-6,
            "rms_simd remainder-path parity broken at len=257: simd={simd}, scalar={scalar}, diff={}",
            (simd - scalar).abs()
        );
    }

    /// Smoke test that `Yin48k::get_pitch` is allocation-free on the hot path. Wraps 100
    /// consecutive `get_pitch` calls (50 sine, 50 noise — exercises both voiced + unvoiced
    /// branches and their internal BufferPool::borrow_mut() paths) inside
    /// `assert_no_alloc(|| { ... })` and asserts the violation counter delta is zero.
    ///
    /// This is the lib-side mitigation for T-2-yin-allocation: pitch-detection 0.3.0's
    /// `YINDetector::new(512, 0)` pre-allocates the BufferPool at construction; the hot
    /// path borrows from it via `RefCell`. A future crate version that lazily allocates
    /// on first call would trip this gate. Plan 02-09's `dsp_assert_no_alloc_loop`
    /// integration test runs the FULL Phase 2 worker body (Stretch + Smoothed + Gate +
    /// YIN) for 10 s under the global AllocDisabler; this lib smoke is a tighter,
    /// fast-feedback gate that fires on the YIN call site specifically.
    #[cfg(debug_assertions)]
    #[test]
    #[serial_test::serial(no_alloc_violation_counter)]
    fn yin48k_get_pitch_alloc_free_smoke() {
        use assert_no_alloc::{assert_no_alloc, violation_count};
        let voiced = sine_window(220.0, 0.5, 512);
        let unvoiced = lcg_noise(512);
        let mut yin = Yin48k::new();
        let before = violation_count();
        assert_no_alloc(|| {
            for _ in 0..50 {
                let _ = yin.get_pitch(&voiced);
            }
            for _ in 0..50 {
                let _ = yin.get_pitch(&unvoiced);
            }
        });
        let after = violation_count();
        assert_eq!(
            after, before,
            "Yin48k::get_pitch tripped the assert_no_alloc violation counter over 100 \
             calls — pitch-detection 0.3.x may have introduced lazy allocation on the hot \
             path (T-2-yin-allocation regression). Pin the exact version in workspace deps."
        );
    }
}

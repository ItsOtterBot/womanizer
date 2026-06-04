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
//!   §Pattern 3 + Example B). 30 ms time-constant per D-35; widened by Plan 03-01 from 2
//!   smoothed fields (pitch, formant) to 6 — adds breath, brightness_db, sibilance, mix —
//!   all sharing a single `alpha` coefficient at the D-35 tau. `step(targets...)` is the
//!   per-block call between `triple_buffer<VoiceParams>::Output::read()` and the per-stage
//!   setters/inputs. Without this, slider drags produce zipper noise (CONTEXT Pitfall #7).
//! - [`BiquadDF1`]: shared Direct-Form-I biquad per RBJ Audio EQ Cookbook
//!   (w3.org/TR/audio-eq-cookbook/). Constructed off-RT; `set_high_shelf` / `set_bandpass`
//!   / `set_peaking` recompute coefficients off the per-sample loop (once per block from
//!   smoothed slider values is the downstream Plan 03-02/03 pattern); per-sample `step()`
//!   runs inside `assert_no_alloc(|| { ... })`. Used by the Phase 3 DeEsser, BrightnessShelf,
//!   and Breathiness stages — landed here by Plan 03-01 so downstream plans share a single
//!   coefficient-math source.
//! - [`BrightnessShelf`]: SHAPE-02 — RBJ second-order high-shelf at 4000 Hz / Q=0.707.
//!   Per-block coefficient recompute from smoothed gain_db; D-42 warm-off; D-44 default
//!   +3 dB. Landed by Plan 03-02; wraps a single [`BiquadDF1`].
//! - [`DeEsser`]: SHAPE-03 — split-band compressor at 6500 Hz / Q=1.0; soft-knee 6 dB
//!   width, threshold −24 dBFS, 1 ms attack / 50 ms release per-sample envelope follower.
//!   D-42 warm-off; D-46 default 0.30. Landed by Plan 03-02; wraps two [`BiquadDF1`] bands
//!   (detector + extract) with independent state.
//! - [`Breathiness`]: SHAPE-01 — XorShift32 PRNG → bandpass 1200 Hz/Q=0.4 → amplitude
//!   scaled by `breath_amount × voicing × rms_env × 0.5 headroom`. D-48 voicing gate from
//!   Phase 2 NaN sentinel; D-49 5/30 ms envelope; D-50 PRNG seed 0x12345678; D-51 worker
//!   silence gate; D-42 warm-off; D-45 default 0.20. Landed by Plan 03-03; wraps a single
//!   [`BiquadDF1`] bandpass plus a private XorShift32 PRNG helper.
//! - [`dry_wet_mix`]: SHAPE-04 free function — `wide::f32x8` linear blend per RESEARCH
//!   §Pattern 6 + §Example C. Chain wire-up per D-43: dry = raw mic scratch, wet =
//!   post-shaping. No enable toggle (D-47 — mix=0.0 IS the off state). Landed by Plan
//!   03-03; mirrors the [`rms_simd`] `chunks_exact(8) + scalar remainder` shape.
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
// Plan 03-01: `SmoothedVoiceParams::new` takes `&VoiceParams` (RESEARCH §SmoothedVoiceParams
// Extension) so the smoother's six initial values come from the same struct the worker's
// `triple_buffer<VoiceParams>` snapshot publishes. The accessor methods on VoiceParams
// (`pitch_semitones_to_ratio`, `formant_semitones_to_ratio`) are exposed by this re-import.
use womanizer_core::VoiceParams;

/// Engine-wide sample rate constant re-exported for callers who want a single import. Equal
/// to [`SAMPLE_RATE_HZ`] from `cpal_io` — 48 kHz, fixed (D-05). The duplicate lives here so
/// dsp.rs is self-contained as a module surface; both constants resolve to the same value.
pub const ENGINE_SR: u32 = SAMPLE_RATE_HZ;

/// Return the `(block_length, interval)` STFT window/hop pair for the given preset.
///
/// 4:1 block-to-hop ratio matches the upstream `presetDefault` overlap and is the
/// phase-vocoder sweet spot for quality. Sized to fit each preset's latency budget per
/// D-25 + RESEARCH §Q2 empirical-sprint protocol.
///
/// ## Plan 02-09 latency-budget tightening (empirical sprint protocol)
///
/// The RESEARCH §Q2 starting points (1024/256, 2048/512, 3072/768) were derived from the
/// optimistic assumption that `input_latency() + output_latency() ≈ block_length / 2`.
/// Empirical measurement at Plan 02-09 execute time showed the actual sum is approximately
/// `block_length` (see RESEARCH §Q1: each side contributes ~block_length/2 for a centered
/// analysis window; the sum is the full block_length). The original windows therefore
/// would have produced 21.33 / 42.67 / 64.00 ms Stretch latency — Balanced and Quality
/// blow their D-25 round-trip budgets even before the cpal capture+playback in-flight
/// overhead is added.
///
/// Plan 02-09 tightens the windows per RESEARCH §Q2 step 4 ("If the largest window for
/// a given budget already saturates the budget, the planner stays with the starting-point
/// above and notes it" — except the starting points don't fit; tightening is the
/// alternative):
///
/// | Preset   | Window | Hop | Stretch latency | D-25 round-trip budget | Stretch budget |
/// |----------|--------|-----|-----------------|------------------------|----------------|
/// | Low      | 768    | 192 | 16.00 ms        | <32 ms                 | <19.3 ms       |
/// | Balanced | 1024   | 256 | 21.33 ms        | <40 ms                 | <27.3 ms       |
/// | Quality  | 1536   | 384 | 32.00 ms        | <50 ms                 | <37.3 ms       |
///
/// The Stretch budget is `total_budget − 12.7 ms` (RESEARCH §Q2 platform-overhead estimate
/// for cpal capture+playback in-flight + scheduling slack). All three presets now leave
/// 3–5 ms of headroom against their respective Stretch budgets. The
/// `dsp_preset_latency_budget` integration test (Plan 02-09 Task 2) verifies the
/// construction-time latency stays in budget; the manual checkpoint (Plan 02-09 Task 3)
/// verifies the live `Telemetry::latency_ms` stays in budget per preset against the actual
/// measured platform overhead.
///
/// ## Quality A/B note
///
/// User-ear A/B against the previous (larger-window) starting points is part of the
/// Plan 02-09 manual checkpoint. If the smaller window audibly degrades the M→F transform
/// (Pitfall #15), the user files a tighten-budget deferred item OR accepts the new
/// quality bar. D-25 explicitly allows the latter: "Pick the largest signalsmith
/// window/hop combo that fits each budget."
///
/// Free function rather than `Preset::window_hop` because [`Preset`] is defined in
/// `womanizer-core::primitives` (Plan 02-02; Pattern G — fields/types that cross thread
/// boundaries live there so [`EngineCommand::SetPreset`] can reference Preset without a
/// circular crate dep). Rust requires inherent impls to live on the defining crate's type.
pub fn preset_window_hop(preset: Preset) -> (usize, usize) {
    match preset {
        Preset::Low => (768, 192),
        Preset::Balanced => (1024, 256),
        Preset::Quality => (1536, 384),
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
/// the per-stage DSP setters/inputs. Without this, slider drags produce zipper noise
/// (CONTEXT Pitfall #7). 30 ms time-constant per D-35 (Phase 2) — shared across all six
/// smoothed fields after Plan 03-01's widening.
///
/// ## Math (RESEARCH §Pattern 3 + Example B)
/// - `tau_samples = (tau_ms / 1000) * 48_000` → 1440 for 30 ms @ 48 kHz.
/// - `alpha = 1.0 - exp(-block_samples / tau_samples)` → ≈ 0.163 for BLOCK=256, 30 ms.
/// - Per block: `current += alpha * (target - current)` for each of the six smoothed
///   parameters (pitch ratio, formant ratio, breathiness, brightness_db, sibilance,
///   dry/wet mix). One shared `alpha` is correct because all six visible UI sliders need
///   the same perceptual smoothing time-constant — D-35 unchanged.
///
/// `alpha` is precomputed at construction (a single const for fixed BLOCK + tau).
///
/// ## Plan 03-01 widening — additive
/// Phase 2 shipped only `pitch_current` + `formant_current`. Plan 03-01 adds four new
/// `*_current` fields between formant_current and alpha (the field order preserves
/// `alpha` LAST per Pattern E). The widened `new(&VoiceParams, ...)` constructor reads
/// initial values directly from the same struct that the worker's
/// `triple_buffer<VoiceParams>` snapshot publishes, so the smoother and the snapshot
/// path share a single source of truth for ship-time defaults (D-44..D-47).
// Fields are written by `new()`, read by `pitch()` / `formant()` / `breathiness()` /
// `brightness_db()` / `sibilance()` / `mix()` accessors, and mutated by `step()`.
#[derive(Copy, Clone, Debug)]
pub struct SmoothedVoiceParams {
    /// Current smoothed pitch multiplier. Initialized from `initial.pitch_semitones_to_ratio()`.
    pitch_current: f32,
    /// Current smoothed formant multiplier. Initialized from `initial.formant_semitones_to_ratio()`.
    formant_current: f32,
    /// Current smoothed breathiness amount [0, 1]. Initialized from `initial.breathiness`
    /// (D-45 ship-time default 0.20). Plan 03-01 widening.
    breath_current: f32,
    /// Current smoothed brightness shelf gain in dB. Initialized from `initial.brightness_db`
    /// (D-44 ship-time default +3.0). Plan 03-01 widening.
    brightness_db_current: f32,
    /// Current smoothed sibilance-tame amount [0, 1]. Initialized from `initial.sibilance_tame`
    /// (D-46 ship-time default 0.30). Plan 03-01 widening.
    sibilance_current: f32,
    /// Current smoothed dry/wet mix [0, 1]. Initialized from `initial.mix`
    /// (D-47 ship-time default 1.0). Plan 03-01 widening.
    mix_current: f32,
    /// One-pole filter coefficient `1.0 - exp(-block_samples / tau_samples)`. Precomputed
    /// once at construction; same value applies to ALL six smoothed fields per D-35.
    alpha: f32,
}

impl SmoothedVoiceParams {
    /// Construct with the initial target values from a `VoiceParams` reference and the
    /// smoothing time-constant. Called once at DSP worker spawn against
    /// `VoiceParams::default()` so all six smoothed fields boot at the D-22 + D-44..D-47
    /// ship-time values (pitch 1.65×, formant 1.18×, breath 0.20, brightness +3 dB,
    /// sibilance 0.30, mix 1.0).
    ///
    /// `block_samples` is [`BLOCK`] (256); `tau_ms` is 30.0 (D-35). Both are passed
    /// explicitly so test code can drive alternative time-constants without going through
    /// crate constants.
    ///
    /// Plan 03-01: signature widened from positional `(initial_pitch, initial_formant, ...)`
    /// to `(&VoiceParams, ...)` per Pattern E + RESEARCH §SmoothedVoiceParams Extension —
    /// avoids 6-positional-arg call sites and keeps the snapshot-struct as the single
    /// source of truth.
    pub fn new(initial: &VoiceParams, block_samples: usize, tau_ms: f32) -> Self {
        let tau_samples = (tau_ms / 1000.0) * ENGINE_SR as f32;
        let alpha = 1.0 - (-(block_samples as f32) / tau_samples).exp();
        Self {
            pitch_current: initial.pitch_semitones_to_ratio(),
            formant_current: initial.formant_semitones_to_ratio(),
            breath_current: initial.breathiness,
            brightness_db_current: initial.brightness_db,
            sibilance_current: initial.sibilance_tame,
            mix_current: initial.mix,
            alpha,
        }
    }

    /// Per-block step. Called by the DSP worker AFTER reading the latest VoiceParams
    /// snapshot from `triple_buffer<VoiceParams>::Output::read()`. Body is the textbook
    /// one-pole exponential interpolator: `current += alpha * (target - current)` for
    /// each of the six smoothed parameters. Six lines, zero allocation, ~18 f32 ops per
    /// block.
    ///
    /// Plan 03-01: signature widened from `(target_pitch, target_formant)` to take four
    /// additional `target_*` args for the new shaping parameters. The single shared
    /// `alpha` preserves D-35 perceptual smoothing across all six fields.
    #[inline]
    pub fn step(
        &mut self,
        target_pitch: f32,
        target_formant: f32,
        target_breath: f32,
        target_bright_db: f32,
        target_sib: f32,
        target_mix: f32,
    ) {
        self.pitch_current += self.alpha * (target_pitch - self.pitch_current);
        self.formant_current += self.alpha * (target_formant - self.formant_current);
        self.breath_current += self.alpha * (target_breath - self.breath_current);
        self.brightness_db_current += self.alpha * (target_bright_db - self.brightness_db_current);
        self.sibilance_current += self.alpha * (target_sib - self.sibilance_current);
        self.mix_current += self.alpha * (target_mix - self.mix_current);
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

    /// Read the current smoothed breathiness amount [0, 1]. Wired by Plan 03-03 to the
    /// Breathiness stage's per-block amplitude scale.
    #[inline]
    pub fn breathiness(&self) -> f32 {
        self.breath_current
    }

    /// Read the current smoothed brightness shelf gain in dB. Wired by Plan 03-02 to the
    /// BrightnessShelf's per-block RBJ high-shelf coefficient recomputation.
    #[inline]
    pub fn brightness_db(&self) -> f32 {
        self.brightness_db_current
    }

    /// Read the current smoothed sibilance-tame amount [0, 1]. Wired by Plan 03-02 to the
    /// DeEsser's per-block effective-ratio computation.
    #[inline]
    pub fn sibilance(&self) -> f32 {
        self.sibilance_current
    }

    /// Read the current smoothed dry/wet mix [0, 1]. Wired by Plan 03-03 to the dry_wet_mix
    /// SIMD linear blend.
    #[inline]
    pub fn mix(&self) -> f32 {
        self.mix_current
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

/// Shared Direct-Form-I biquad per the RBJ Audio EQ Cookbook
/// (Robert Bristow-Johnson, <https://www.w3.org/TR/audio-eq-cookbook/>).
///
/// Single-section second-order IIR filter. Coefficients computed once per parameter change
/// (typically once per audio block from smoothed slider values) by [`Self::set_high_shelf`],
/// [`Self::set_bandpass`], or [`Self::set_peaking`]; per-sample [`Self::step`] is 5 multiplies
/// + 4 adds.
///
/// ## Why Direct Form I (not DF-II or transposed DF-II)
/// DF-I has the best numerical behavior under coefficient modulation. The brightness
/// shelf (Plan 03-02) recomputes coefficients every block from the smoothed dB value;
/// DF-I's separate input + output delay storage avoids the transient discontinuities
/// transposed DF-II exhibits under that workload. This is the standard audio-DSP
/// recommendation (RBJ cookbook §"Implementation").
///
/// ## Lifecycle
/// - Constructed OFF the audio thread (DSP worker spawn, or engine event-loop thread on
///   preset rebuild). The `new()` constructor is a unity passthrough.
/// - Owned exclusively by a single DSP stage (DeEsser / BrightnessShelf / Breathiness).
/// - Per-block `process(input, output)` runs inside `assert_no_alloc(|| { ... })` — only
///   stack arithmetic + slice reads/writes; coefficient recomputation also alloc-free
///   (a handful of transcendentals + stack arithmetic — no heap).
///
/// ## Plan 03-01 lands the shared helper; downstream plans consume
/// - Plan 03-02 (BrightnessShelf): `set_high_shelf` for the +3 dB high-shelf at 4 kHz.
/// - Plan 03-02 (DeEsser): `set_bandpass` for the 6.5 kHz detector + `set_peaking` for
///   the dynamic cut band.
/// - Plan 03-03 (Breathiness): `set_bandpass` for the 1200 Hz / Q=0.4 noise shaping
///   filter.
///
/// Field layout: five normalized coefficients (a0 implicit 1.0 after the normalization
/// step) + four Direct Form I state slots (two input delays, two output delays).
#[derive(Copy, Clone, Debug)]
pub struct BiquadDF1 {
    /// Forward coefficient for the current sample (b0/a0 after normalization).
    b0: f32,
    /// Forward coefficient for x[n-1].
    b1: f32,
    /// Forward coefficient for x[n-2].
    b2: f32,
    /// Feedback coefficient for y[n-1] (stored with RBJ's positive sign convention; the
    /// per-sample `step` subtracts it).
    a1: f32,
    /// Feedback coefficient for y[n-2] (same sign convention as `a1`).
    a2: f32,
    /// Direct Form I input delay 1: x[n-1].
    x1: f32,
    /// Direct Form I input delay 2: x[n-2].
    x2: f32,
    /// Direct Form I output delay 1: y[n-1].
    y1: f32,
    /// Direct Form I output delay 2: y[n-2].
    y2: f32,
}

impl BiquadDF1 {
    /// Construct a unity-passthrough biquad: `b0=1, b1=b2=a1=a2=0`, all delays zero.
    /// Coefficient setters overwrite the b/a fields without touching the delay lines, so
    /// constructing-then-immediately-configuring is correct even if the caller does not
    /// `reset_state()` first.
    pub fn new() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        }
    }

    /// Reset only the delay lines (preserve coefficients). Useful after a long warm-off:
    /// not strictly required because zeros are mathematically correct, but cheap insurance
    /// against any accumulated denormals during long inactive periods. Phase 3 D-42
    /// warm-off pattern calls `process()` every block so this is rarely needed in practice.
    pub fn reset_state(&mut self) {
        self.x1 = 0.0;
        self.x2 = 0.0;
        self.y1 = 0.0;
        self.y2 = 0.0;
    }

    /// Compute and store RBJ second-order high-shelf coefficients for sample rate `fs`,
    /// corner frequency `f0_hz`, gain `gain_db`, and Q-factor `q`.
    ///
    /// Source: RBJ Audio EQ Cookbook (Robert Bristow-Johnson),
    /// <https://www.w3.org/TR/audio-eq-cookbook/> §"High-shelf filter". All variable names
    /// transcribed verbatim from the cookbook for traceability.
    ///
    /// At `gain_db = 0` the high-shelf collapses mathematically to a unity passthrough:
    /// `A = 10^0 = 1`, so `b0 = a0`, `b1 = a1`, `b2 = a2` and after normalization the
    /// recursion becomes `y[n] = x[n]` (verified by the
    /// `biquad_high_shelf_at_zero_db_is_unity_passthrough` lib test).
    pub fn set_high_shelf(&mut self, fs: f32, f0_hz: f32, gain_db: f32, q: f32) {
        // Source: RBJ Audio EQ Cookbook (Robert Bristow-Johnson),
        // https://www.w3.org/TR/audio-eq-cookbook/
        let big_a = 10.0_f32.powf(gain_db / 40.0); // sqrt of linear gain
        let omega = 2.0 * std::f32::consts::PI * f0_hz / fs;
        let sin_w = omega.sin();
        let cos_w = omega.cos();
        let alpha = sin_w / (2.0 * q);
        let two_sqrt_a_alpha = 2.0 * big_a.sqrt() * alpha;

        let b0 = big_a * ((big_a + 1.0) + (big_a - 1.0) * cos_w + two_sqrt_a_alpha);
        let b1 = -2.0 * big_a * ((big_a - 1.0) + (big_a + 1.0) * cos_w);
        let b2 = big_a * ((big_a + 1.0) + (big_a - 1.0) * cos_w - two_sqrt_a_alpha);
        let a0 = (big_a + 1.0) - (big_a - 1.0) * cos_w + two_sqrt_a_alpha;
        let a1 = 2.0 * ((big_a - 1.0) - (big_a + 1.0) * cos_w);
        let a2 = (big_a + 1.0) - (big_a - 1.0) * cos_w - two_sqrt_a_alpha;

        // Normalize so the implicit a0 in the recursion is 1.0 — eliminates one division
        // per sample. The per-sample `step` subtracts `a1` and `a2` (consistent with RBJ's
        // recursion `y = ... - a1*y1 - a2*y2`).
        let inv_a0 = 1.0 / a0;
        self.b0 = b0 * inv_a0;
        self.b1 = b1 * inv_a0;
        self.b2 = b2 * inv_a0;
        self.a1 = a1 * inv_a0;
        self.a2 = a2 * inv_a0;
    }

    /// Compute and store RBJ "BPF: constant 0 dB peak gain" bandpass coefficients for
    /// sample rate `fs`, center frequency `f0_hz`, and Q-factor `q`.
    ///
    /// Source: RBJ Audio EQ Cookbook (Robert Bristow-Johnson),
    /// <https://www.w3.org/TR/audio-eq-cookbook/> §"BPF: constant 0 dB peak gain". This is
    /// the symmetric bandpass variant used by the Phase 3 breath synthesis chain (Plan
    /// 03-03 will use it at 1200 Hz / Q=0.4 for noise shaping) and by the de-esser detector
    /// (Plan 03-02 will use it at 6.5 kHz / Q=1.0).
    pub fn set_bandpass(&mut self, fs: f32, f0_hz: f32, q: f32) {
        // Source: RBJ Audio EQ Cookbook (Robert Bristow-Johnson),
        // https://www.w3.org/TR/audio-eq-cookbook/
        let omega = 2.0 * std::f32::consts::PI * f0_hz / fs;
        let sin_w = omega.sin();
        let cos_w = omega.cos();
        let alpha = sin_w / (2.0 * q);

        let b0 = alpha;
        let b1 = 0.0;
        let b2 = -alpha;
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w;
        let a2 = 1.0 - alpha;

        let inv_a0 = 1.0 / a0;
        self.b0 = b0 * inv_a0;
        self.b1 = b1 * inv_a0;
        self.b2 = b2 * inv_a0;
        self.a1 = a1 * inv_a0;
        self.a2 = a2 * inv_a0;
    }

    /// Compute and store RBJ peaking-EQ coefficients for sample rate `fs`, center
    /// frequency `f0_hz`, gain `gain_db`, and Q-factor `q`.
    ///
    /// Source: RBJ Audio EQ Cookbook (Robert Bristow-Johnson),
    /// <https://www.w3.org/TR/audio-eq-cookbook/> §"Peaking EQ". Plan 03-02 may use this
    /// for the de-esser dynamic cut band.
    pub fn set_peaking(&mut self, fs: f32, f0_hz: f32, gain_db: f32, q: f32) {
        // Source: RBJ Audio EQ Cookbook (Robert Bristow-Johnson),
        // https://www.w3.org/TR/audio-eq-cookbook/
        let big_a = 10.0_f32.powf(gain_db / 40.0); // sqrt of linear gain
        let omega = 2.0 * std::f32::consts::PI * f0_hz / fs;
        let sin_w = omega.sin();
        let cos_w = omega.cos();
        let alpha = sin_w / (2.0 * q);

        let b0 = 1.0 + alpha * big_a;
        let b1 = -2.0 * cos_w;
        let b2 = 1.0 - alpha * big_a;
        let a0 = 1.0 + alpha / big_a;
        let a1 = -2.0 * cos_w;
        let a2 = 1.0 - alpha / big_a;

        let inv_a0 = 1.0 / a0;
        self.b0 = b0 * inv_a0;
        self.b1 = b1 * inv_a0;
        self.b2 = b2 * inv_a0;
        self.a1 = a1 * inv_a0;
        self.a2 = a2 * inv_a0;
    }

    /// Per-sample Direct Form I recursion. Five multiplies + four adds + four delay
    /// shifts; ~1 ns on a modern CPU. Inline-able so the block-level loop in
    /// [`Self::process`] folds it.
    ///
    /// Recursion: `y[n] = b0*x[n] + b1*x[n-1] + b2*x[n-2] - a1*y[n-1] - a2*y[n-2]` then
    /// shift the delays. NOT vectorizable across samples (intrinsic IIR recursion).
    #[inline]
    pub fn step(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.b1 * self.x1 + self.b2 * self.x2
            - self.a1 * self.y1
            - self.a2 * self.y2;
        self.x2 = self.x1;
        self.x1 = x;
        self.y2 = self.y1;
        self.y1 = y;
        y
    }

    /// Block processing — caller passes non-aliased input + output slices. Follows the
    /// Pattern B Phase 2 contract (`process(&[f32], &mut [f32])` with the
    /// `debug_assert_eq!` length guard). In-place processing is supported by passing the
    /// same slice twice via `split_at_mut` tricks, but the clean idiom is `input != output`
    /// against pre-allocated scratch buffers.
    #[inline]
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) {
        debug_assert_eq!(input.len(), output.len());
        for (xi, yi) in input.iter().zip(output.iter_mut()) {
            *yi = self.step(*xi);
        }
    }
}

impl Default for BiquadDF1 {
    fn default() -> Self {
        Self::new()
    }
}

/// SHAPE-02 — RBJ second-order high-shelf at 4000 Hz / Q=0.707 driven by a smoothed dB
/// value. Wraps a single [`BiquadDF1`] whose coefficients are recomputed once per block
/// from the supplied `target_gain_db` (RESEARCH §Pattern 4 Option 1 — "Recompute every
/// block ~30 ns + 3 transcendentals; 0.0006% CPU"). UI slider range is −6 dB to +12 dB
/// (D-44); ship-time default +3 dB (D-44).
///
/// ## Lifecycle
/// - Constructed OFF the audio thread (DSP worker spawn — Plan 03-04).
/// - Owned exclusively by the DSP worker thread; never wrapped in Mutex.
/// - `process()` is called every audio block. Inside `assert_no_alloc(|| { ... })`.
///
/// ## D-42 warm contract
/// `process()` runs UNCONDITIONALLY every block regardless of `enabled`. The internal
/// [`BiquadDF1`] state (x1/x2/y1/y2) keeps updating during warm-off so toggling enabled
/// back on produces zero startup transient (RESEARCH §Pitfall 5). Only the assignment
/// `output[i] = input[i]` when `!enabled` differs from the enabled branch — the filter's
/// `step()` is called on every sample either way.
pub struct BrightnessShelf {
    /// The single Direct-Form-I biquad set to a high-shelf via [`BiquadDF1::set_high_shelf`]
    /// at 4000 Hz / Q=0.707; coefficients recomputed once per block from the smoothed
    /// `target_gain_db` argument to [`Self::process`].
    shelf: BiquadDF1,
}

impl BrightnessShelf {
    /// Construct a BrightnessShelf with a unity-passthrough biquad. The first
    /// [`Self::process`] call reseeds coefficients via
    /// [`BiquadDF1::set_high_shelf`] from the supplied `target_gain_db` (typically the
    /// smoothed slider value published by [`SmoothedVoiceParams::brightness_db`]).
    ///
    /// `_fs` is accepted (and currently unused at construction) so the constructor
    /// signature mirrors Pattern B's per-block-takes-`fs` shape — callers do not need to
    /// remember whether the rate flows through the constructor or [`Self::process`]. The
    /// argument is intentionally a no-op here; the actual sample rate threads through
    /// [`Self::process`] for the per-block coefficient setter call.
    pub fn new(_fs: f32) -> Self {
        Self {
            shelf: BiquadDF1::new(),
        }
    }

    /// Per-block hot path. Recomputes biquad coefficients ONCE per block from
    /// `target_gain_db` at 4000 Hz / Q=0.707 (RESEARCH §Pattern 4 — canonical M→F
    /// female-vocal "air" shelf parameters), then runs the per-sample DF-I recursion.
    ///
    /// D-42 warm-off: `self.shelf.step(*xi)` is called UNCONDITIONALLY per sample so the
    /// biquad delay state stays coherent even when `enabled = false`. Only the output
    /// routing differs — when disabled, `output[i] = input[i]`.
    ///
    /// `fs` is the engine sample rate (48 kHz, [`ENGINE_SR`] in practice); accepted as an
    /// argument so tests can drive alternative rates without going through crate constants.
    #[inline]
    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        target_gain_db: f32,
        enabled: bool,
        fs: f32,
    ) {
        // Per-block coefficient recompute from the smoothed gain_db (RESEARCH §Pattern 4
        // Option 1 — Recompute every block; ~30 ns + 3 transcendentals; 0.0006% CPU).
        // Q = 0.707 ≈ 1/√2 — the canonical Butterworth "no resonance" shelf-slope choice.
        self.shelf.set_high_shelf(fs, 4000.0, target_gain_db, 0.707);
        debug_assert_eq!(input.len(), output.len());
        for (xi, yi) in input.iter().zip(output.iter_mut()) {
            // D-42 warm-off: step UNCONDITIONALLY each sample. The biquad's x1/x2/y1/y2
            // delays MUST keep updating regardless of `enabled` so a toggle-back produces
            // zero startup transient.
            let filtered = self.shelf.step(*xi);
            *yi = if enabled { filtered } else { *xi };
        }
    }
}

/// Soft-knee gain-reduction formula per RESEARCH §Pattern 3 / the canonical
/// compressor literature (`christianfloisand.wordpress.com/.../dynamics-processing-
/// compressorlimiter-part-1/`). All inputs/outputs are in dB; the returned value is the
/// gain reduction in dB (≤ 0 — zero means no reduction, negative means attenuation).
///
/// Three branches:
/// 1. Below the knee (`level_db < threshold - half_knee`): no reduction (return 0.0).
/// 2. Above the knee (`level_db > threshold + half_knee`): full-ratio compression →
///    `(threshold − level) * (1 − 1/ratio)`. Negative because level > threshold.
/// 3. Inside the knee: quadratic interpolation that smoothly joins the two outer
///    branches, eliminating the audible "click" of hard-knee compression.
///
/// Used by [`DeEsser::process`] per-sample on the envelope follower's instantaneous dB.
#[inline]
fn soft_knee_gr_db(level_db: f32, threshold_db: f32, ratio: f32, knee_width_db: f32) -> f32 {
    let half_knee = knee_width_db * 0.5;
    if level_db < threshold_db - half_knee {
        0.0
    } else if level_db > threshold_db + half_knee {
        (threshold_db - level_db) * (1.0 - 1.0 / ratio)
    } else {
        let x = level_db - threshold_db + half_knee;
        let a = (1.0 - 1.0 / ratio) / (2.0 * knee_width_db);
        -a * x * x
    }
}

/// SHAPE-03 — Split-band compressor de-esser per RESEARCH §Pattern 3 + §Example D.
/// Two [`BiquadDF1`] bandpasses (`detector_band` + `extract_band`) at 6500 Hz / Q=1.0
/// with INDEPENDENT state drive a one-pole envelope follower (1 ms attack / 50 ms
/// release per-sample alpha) feeding a soft-knee gain-reduction stage (threshold −24
/// dBFS, knee width 6 dB). The split-band output formula
/// `output = input - (1 - linear_gain) * extract_output` (RESEARCH §Pattern 3 line 702)
/// subtracts excess sibilance from the input rather than reconstructing a low band +
/// gain-reduced high band — preserves low-mid clarity (RESEARCH §Pitfall 4 mitigation).
///
/// ## UI semantics (D-46)
/// The `sibilance_amount` slider is mapped to an effective ratio per RESEARCH §Pattern 3
/// table: `effective_ratio = 1.0 + sibilance_amount * 5.0` so slider 0 → 1:1 (identity),
/// slider 0.30 (D-46 ship-time default) → ~2.5:1 (light de-essing), slider 1.0 → 6:1
/// (heavy). At slider 0 the soft-knee formula collapses to gr_db = 0 (no reduction) → the
/// output equals the input bit-exact within the float-rounding budget (verified by
/// `deess_amount_zero_passthrough`).
///
/// ## Lifecycle
/// - Constructed OFF the audio thread (DSP worker spawn — Plan 03-04).
/// - Owned exclusively by the DSP worker thread; never wrapped in Mutex.
/// - `process()` is called every audio block. Inside `assert_no_alloc(|| { ... })`.
///
/// ## D-42 warm contract
/// `process()` runs UNCONDITIONALLY every block regardless of `enabled`. All four
/// stateful components — `detector_band.step()`, `extract_band.step()`, the one-pole
/// `envelope` follower, and the per-sample soft-knee gain-reduction computation — update
/// each sample. Only the assignment to `*yi` respects the bool (`*yi = if enabled
/// { processed } else { *xi }`). Toggling enabled back on therefore produces zero
/// startup transient (RESEARCH §Pitfall 5).
pub struct DeEsser {
    /// Detector bandpass at 6500 Hz / Q=1.0 (~1 octave bandwidth). Used to extract the
    /// sibilance-band magnitude that drives the envelope follower.
    detector_band: BiquadDF1,
    /// Extract bandpass at 6500 Hz / Q=1.0 (same coefficients, INDEPENDENT state).
    /// The output formula subtracts `(1 - linear_gain) * extract_output` from the input.
    extract_band: BiquadDF1,
    /// One-pole envelope follower state — instantaneous absolute-value of the detector
    /// bandpass output, smoothed via attack/release alphas per RESEARCH §Pattern 2.
    envelope: f32,
    /// Per-sample attack coefficient (1 ms tau at the engine sample rate). Precomputed
    /// once at construction; ≈ 0.0206 at 48 kHz.
    alpha_attack: f32,
    /// Per-sample release coefficient (50 ms tau at the engine sample rate). Precomputed
    /// once at construction; ≈ 0.000417 at 48 kHz.
    alpha_release: f32,
    /// Threshold in dBFS for the soft-knee gain reduction. −24 dBFS per RESEARCH
    /// §Pattern 3 table.
    threshold_db: f32,
    /// Soft-knee width in dB. 6 dB per RESEARCH §Pattern 3 table — smooths the threshold
    /// crossing so a level hovering near −24 dBFS does not produce audible "click"
    /// compression edges.
    knee_width_db: f32,
}

impl DeEsser {
    /// Construct a DeEsser with both bandpasses pre-configured at 6500 Hz / Q=1.0,
    /// envelope-follower alphas computed from the supplied sample rate, and the
    /// RESEARCH §Pattern 3 threshold / knee parameters.
    pub fn new(fs: f32) -> Self {
        let mut detector_band = BiquadDF1::new();
        detector_band.set_bandpass(fs, 6500.0, 1.0);
        let mut extract_band = BiquadDF1::new();
        extract_band.set_bandpass(fs, 6500.0, 1.0);
        Self {
            detector_band,
            extract_band,
            envelope: 0.0,
            // Per-sample one-pole follower coefficients (RESEARCH §Pattern 2):
            //   alpha = 1.0 - exp(-1.0 / (tau_seconds * fs))
            alpha_attack: 1.0 - (-1.0 / (0.001 * fs)).exp(),
            alpha_release: 1.0 - (-1.0 / (0.050 * fs)).exp(),
            threshold_db: -24.0,
            knee_width_db: 6.0,
        }
    }

    /// Per-block hot path. Per-sample loop runs the detector → envelope follower →
    /// soft-knee gain reduction → split-band subtraction sequence per RESEARCH §Example D
    /// and §Pattern 3 line 702.
    ///
    /// `sibilance_amount` is the smoothed slider value [0, 1] (typically published by
    /// [`SmoothedVoiceParams::sibilance`]). The effective compressor ratio is
    /// `1.0 + sibilance_amount * 5.0` (D-46) — at 0 the gain reduction is identically
    /// zero, so output = input within float-rounding.
    ///
    /// D-42 warm-off: detector_band, extract_band, envelope follower, and soft-knee
    /// computation all update every sample regardless of `enabled`. Only the assignment
    /// to `*yi` respects the bool.
    ///
    /// `_fs` is the engine sample rate. Currently unused inside the per-sample loop —
    /// the bandpass coefficients are static at 6500 Hz so they need no re-set; the
    /// envelope-follower alphas were precomputed at construction. Accepted as an argument
    /// to mirror Pattern B's process signature shape and to be future-proof against a
    /// sample-rate change.
    #[inline]
    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        sibilance_amount: f32,
        enabled: bool,
        _fs: f32,
    ) {
        debug_assert_eq!(input.len(), output.len());
        // D-46 slider → ratio mapping: slider 0 → 1:1 (identity), 1.0 → 6:1.
        let effective_ratio = 1.0 + sibilance_amount * 5.0;

        for (xi, yi) in input.iter().zip(output.iter_mut()) {
            // 1. Detector bandpass + one-pole envelope follower (RESEARCH §Pattern 2).
            //    `step()` runs UNCONDITIONALLY each sample — D-42 warm-off contract.
            let det = self.detector_band.step(*xi).abs();
            let alpha = if det > self.envelope {
                self.alpha_attack
            } else {
                self.alpha_release
            };
            self.envelope += alpha * (det - self.envelope);

            // 2. Soft-knee gain reduction in dB. The 1e-10 floor on the envelope avoids
            //    log10(0) → −∞ when envelope is zero (e.g. during silence).
            let env_db = 20.0 * self.envelope.max(1e-10).log10();
            let gr_db = soft_knee_gr_db(
                env_db,
                self.threshold_db,
                effective_ratio,
                self.knee_width_db,
            );
            let gain_linear = 10.0_f32.powf(gr_db / 20.0);

            // 3. Split-band output formula (RESEARCH §Pattern 3 line 702):
            //      output = input - (1 - linear_gain) * extract_output
            //    Subtracts excess sibilance from the input rather than reconstructing
            //    low_band + gain_reduced_high. The extract_band's state is INDEPENDENT
            //    of the detector_band's state — both are bandpasses at the same center
            //    frequency but they evolve their own x1/x2/y1/y2 delays. `step()` runs
            //    UNCONDITIONALLY each sample — D-42 warm-off contract.
            let high_band = self.extract_band.step(*xi);
            let processed = *xi - (1.0 - gain_linear) * high_band;

            // 4. D-42 warm-off: assignment is the ONLY branch that respects `enabled`.
            //    detector_band / extract_band / envelope all already updated above.
            *yi = if enabled { processed } else { *xi };
        }
    }
}

/// Marsaglia XorShift32 PRNG — private helper consumed only by [`Breathiness`].
///
/// Shift constants `(13, 17, 5)` are the canonical Marsaglia triple for u32 state width
/// (Wikipedia "Xorshift", verified RESEARCH §Sources Tertiary line 1033). Period is
/// 2^32 − 1 — at 48 kHz, that is ~24.85 hours of continuous noise before any sample
/// pattern repeats, far beyond any plausible audio session. Statistical properties are
/// sufficient for audio-rate noise generation (cryptographic randomness is not required
/// — RESEARCH §Security Domain V6).
///
/// NOT `pub`: only [`Breathiness::new`] constructs one (with the D-50 default seed
/// `0x12345678`), and only [`Breathiness::process`] consumes its [`Self::next_f32`]
/// output. Marking private prevents downstream plans accidentally seeding two PRNGs with
/// the same constant from independent call sites (correlated noise = audible buzz).
#[derive(Copy, Clone, Debug)]
struct XorShift32 {
    /// 32-bit state. Initialized non-zero at construction (the recurrence collapses to
    /// zero permanently if seeded with zero, so [`Self::new`] substitutes the D-50
    /// constant on a zero seed).
    state: u32,
}

impl XorShift32 {
    /// Construct with the supplied seed. A zero seed substitutes the D-50 default
    /// `0x12345678` because the XorShift recurrence has a fixed-point at zero (state
    /// would stay zero forever, producing constant `-1.0` from [`Self::next_f32`]).
    fn new(seed: u32) -> Self {
        Self {
            state: if seed == 0 { 0x12345678 } else { seed },
        }
    }

    /// Advance the PRNG state by one Marsaglia XorShift32 step and return the new u32
    /// state. The triple (13, 17, 5) is the canonical full-period set for 32-bit state.
    #[inline]
    fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    /// Return a uniform f32 in `[-1.0, 1.0]`. Scaled via `state * (2 / u32::MAX) - 1`;
    /// the constant is precomputed at compile time. Equivalent to one PRNG step + one
    /// multiply + one subtract — ~2 ns on a modern CPU. The exact endpoints are
    /// `state=0 → -1.0`, `state=u32::MAX → +1.0`, intermediate states uniformly spaced.
    #[inline]
    fn next_f32(&mut self) -> f32 {
        const SCALE: f32 = 2.0 / (u32::MAX as f32);
        (self.next_u32() as f32) * SCALE - 1.0
    }
}

/// SHAPE-01 — White-noise → bandpass aspiration injector per RESEARCH §Pattern 5.
///
/// Per-sample chain: XorShift32 PRNG → 1200 Hz/Q=0.4 bandpass biquad → amplitude scaled
/// by `breath_amount × voicing_gate × rms_envelope × 0.5 headroom` → added to the input.
/// At amount=0, voicing_gate=0 (unvoiced), or enabled=false the output equals the input;
/// otherwise a formant-band-shaped breath ride sits under the voiced signal at a
/// loudness that tracks the input RMS envelope.
///
/// ## Voicing gate (D-48)
/// `output_f0_hz: f32` arg per-block: `f32::NAN` → unvoiced (gate=0, no noise injected);
/// finite Hz → voiced (gate=1, noise scales freely). Decodes Phase 2 D-32's NaN sentinel
/// (`Telemetry::output_f0_hz.load(Ordering::Relaxed)`) without re-running any pitch
/// detector — the worker reads the atomic and forwards the raw f32 to each block's
/// [`Self::process`] call (RESEARCH §Example E line 745).
///
/// ## Envelope follower (D-49 + RESEARCH §Pattern 2)
/// One-pole RMS-on-input follower with 5 ms attack / 30 ms release per-sample alphas
/// precomputed at construction (≈ 0.004158 / 0.000694 at 48 kHz). Computed from the
/// input slice to [`Self::process`] (post-Brightness per the D-40 chain), NOT from
/// `Telemetry::input_rms` — RESEARCH §Common Pitfall 7 calls out the latter as wrong
/// because the raw mic RMS sits BEFORE the formant shift that Phase 2 applied.
///
/// ## Noise spectrum (D-50)
/// XorShift32 white noise → bandpass at 1200 Hz / Q=0.4. 1200 Hz is the geometric mean
/// of the D-50 300–3500 Hz formant band; Q=0.4 yields the matching ~3200 Hz bandwidth
/// (RESEARCH §Pattern 5 table). The bandpass coefficients are CONSTANT — never re-set
/// during the session.
///
/// ## Headroom (RESEARCH §Pattern 5)
/// The fixed `0.5` factor inside the per-sample amplitude scale prevents the noise from
/// peaking above the signal RMS even at `breath_amount = 1.0`. Adjusting it requires a
/// D-XX decision change — it is NOT a user-facing parameter.
///
/// ## D-42 warm contract
/// PRNG state, bandpass delays, and envelope follower all advance UNCONDITIONALLY each
/// sample regardless of `enabled`. Only the assignment `output[i] = input[i] + noise_add`
/// (vs `output[i] = input[i]`) respects the bool. Toggling enabled back on therefore
/// produces zero startup transient (RESEARCH §Pitfall 5; matches the BrightnessShelf /
/// DeEsser pattern landed by Plan 03-02).
///
/// ## D-51 silence interaction
/// Breathiness itself does NOT gate on input RMS. The Phase 2 silence gate (D-29 /
/// `crates/womanizer-engine/src/dsp.rs::Gate`) zeros the worker output AFTER the full
/// shaping chain runs — when the gate is closed, breath noise produced by this stage is
/// overwritten with true digital zero by the worker. The envelope follower's natural
/// 30 ms release decays the RMS estimate during silence so breath re-attacks promptly
/// on the next voice onset.
///
/// ## Lifecycle
/// - Constructed OFF the audio thread (DSP worker spawn — Plan 03-04) with
///   `Breathiness::new(ENGINE_SR as f32, 0x12345678)`.
/// - Owned exclusively by the DSP worker thread; never wrapped in Mutex.
/// - `process()` runs every audio block inside `assert_no_alloc(|| { ... })` — only
///   stack arithmetic + caller-supplied slices + `self`-field state.
pub struct Breathiness {
    /// XorShift32 PRNG seeded at construction per D-50 + RESEARCH §Pattern 5 line 384.
    /// Advances one step per sample (alloc-free; pure u32 arithmetic).
    prng: XorShift32,
    /// Bandpass biquad set to 1200 Hz / Q=0.4 per RESEARCH §Pattern 5 table. Constant —
    /// never changes during the session, so [`BiquadDF1::set_bandpass`] is called once
    /// in [`Self::new`] and never again.
    noise_bandpass: BiquadDF1,
    /// One-pole RMS envelope follower state on the INPUT signal to this stage
    /// (post-Brightness per the D-40 chain). Per RESEARCH §Common Pitfall 7 — computed
    /// from `Self::process`'s `input` slice, NOT from `Telemetry::input_rms` (raw mic).
    envelope: f32,
    /// Per-sample attack coefficient (5 ms tau at the engine sample rate) per D-49 +
    /// RESEARCH §Pattern 2. Precomputed once at construction; ≈ 0.004158 at 48 kHz.
    alpha_attack: f32,
    /// Per-sample release coefficient (30 ms tau at the engine sample rate) per D-49 +
    /// RESEARCH §Pattern 2. Precomputed once at construction; ≈ 0.000694 at 48 kHz.
    alpha_release: f32,
}

impl Breathiness {
    /// Construct a Breathiness stage. PRNG seeded with `prng_seed` (the worker passes
    /// the D-50 default `0x12345678`); bandpass biquad pre-configured at 1200 Hz / Q=0.4;
    /// envelope follower alphas precomputed at the supplied sample rate per RESEARCH
    /// §Pattern 2.
    ///
    /// CRITICAL: MUST be called off the audio thread — `BiquadDF1::set_bandpass` invokes
    /// `sin` / `cos` which are bounded-time but the construction also zero-initializes
    /// envelope state which must happen before the first `process()` call.
    pub fn new(fs: f32, prng_seed: u32) -> Self {
        let mut noise_bandpass = BiquadDF1::new();
        // RESEARCH §Pattern 5 table: 1200 Hz center (geometric mean of D-50's 300–3500 Hz
        // band), Q = 0.4 (Q = f0 / bandwidth = 1200 / 3200 ≈ 0.375 → 0.4).
        noise_bandpass.set_bandpass(fs, 1200.0, 0.4);
        Self {
            prng: XorShift32::new(prng_seed),
            noise_bandpass,
            envelope: 0.0,
            // Per-sample one-pole follower coefficients (RESEARCH §Pattern 2):
            //   alpha = 1.0 - exp(-1.0 / (tau_seconds * fs))
            // 5 ms attack / 30 ms release per D-49.
            alpha_attack: 1.0 - (-1.0 / (0.005 * fs)).exp(),
            alpha_release: 1.0 - (-1.0 / (0.030 * fs)).exp(),
        }
    }

    /// Per-block hot path. Per-sample chain:
    ///
    /// 1. Update RMS envelope via attack-on-rising / release-on-falling on `|input[i]|`.
    /// 2. Generate one XorShift32 white-noise sample in `[-1, 1]`.
    /// 3. Filter through the 1200 Hz / Q=0.4 bandpass.
    /// 4. Scale by `breath_amount × voicing_gate × envelope × 0.5 headroom` (RESEARCH
    ///    §Pattern 5 amplitude formula).
    /// 5. If `enabled`, output = input + noise; else output = input. D-42 warm contract:
    ///    PRNG / bandpass / envelope all updated unconditionally.
    ///
    /// `output_f0_hz: f32` (NOT `fs: f32`) per the worker's calling convention (RESEARCH
    /// §Example E line 745). Phase 2 D-32 sentinel: NaN = unvoiced (`voicing_gate = 0`,
    /// no noise injected regardless of amount); finite = voiced (`voicing_gate = 1`).
    #[inline]
    pub fn process(
        &mut self,
        input: &[f32],
        output: &mut [f32],
        breath_amount: f32,
        enabled: bool,
        output_f0_hz: f32,
    ) {
        debug_assert_eq!(input.len(), output.len());
        // D-48 binary voicing gate from Phase 2's NaN sentinel. NaN.is_nan() is true;
        // any finite value (including +/-inf) yields false. Inf is not produced by the
        // upstream Phase 2 pipeline (Telemetry::output_f0_hz only stores raw_f0 *
        // smoothed.pitch() or NaN), so finite-implies-voiced is sound.
        let voicing_gate: f32 = if output_f0_hz.is_nan() { 0.0 } else { 1.0 };

        for (xi, yi) in input.iter().zip(output.iter_mut()) {
            // 1. One-pole envelope follower on |input| (RESEARCH §Pattern 2). Attack on
            //    rising magnitude, release on falling — matches the BrightnessShelf /
            //    DeEsser Pattern C convention.
            let abs_in = (*xi).abs();
            let alpha = if abs_in > self.envelope {
                self.alpha_attack
            } else {
                self.alpha_release
            };
            self.envelope += alpha * (abs_in - self.envelope);

            // 2. XorShift32 white noise in [-1, 1]. PRNG state advances UNCONDITIONALLY
            //    each sample — D-42 warm contract.
            let white = self.prng.next_f32();

            // 3. Bandpass to the 300–3500 Hz speech-formant range (center 1200 Hz / Q=0.4).
            //    Biquad state advances UNCONDITIONALLY each sample — D-42 warm contract.
            let breath_noise = self.noise_bandpass.step(white);

            // 4. RESEARCH §Pattern 5 amplitude scaling:
            //      breath_amount × voicing_gate × rms_envelope × headroom × bandpass_noise
            //    Headroom (0.5) prevents the noise peak from exceeding signal RMS even at
            //    breath_amount=1.0 (RESEARCH §Pattern 5 table — preserves natural feel).
            let noise_add =
                breath_amount * voicing_gate * self.envelope * 0.5 * breath_noise;

            // 5. D-42 warm-off: assignment is the ONLY branch that respects `enabled`.
            //    All stateful sub-components (PRNG, biquad, envelope) already advanced.
            *yi = if enabled { *xi + noise_add } else { *xi };
        }
    }
}

/// SHAPE-04 — SIMD linear blend `output[i] = dry[i] * (1.0 - mix) + wet[i] * mix` per
/// RESEARCH §Pattern 6 + §Example C. NOT a struct because D-47 + RESEARCH §Open Question
/// 3 conclude the dry/wet stage has no enable toggle — `mix = 0.0` IS the off state
/// (output equals dry bit-exact).
///
/// Chain wire-up per D-43 (Plan 03-04 owns the wire-up):
/// `dry_wet_mix(&scratch, &breath_out, smoothed.mix(), &mut processed)` — dry = raw mic
/// scratch (the input to the Phase 2 Stretch + Phase 3 shaping chain); wet = post-
/// shaping signal (Stretch → DeEsser → BrightnessShelf → Breathiness output, RESEARCH
/// §Example E line 752).
///
/// ## SIMD shape (Pattern G)
/// Mirrors Phase 2 [`rms_simd`] verbatim — `chunks_exact(8) + try_into + f32x8::new +
/// scalar remainder`. Uses [`wide::f32x8::mul_add`] for the inner FMA blend op
/// (`d * (1-mix) + w * mix`). At `BLOCK = 256` the loop runs exactly 32 times with no
/// remainder; the scalar tail loop handles non-multiple-of-8 inputs (e.g. unit tests
/// at `len = 257`).
///
/// ## Allocation profile
/// All loads, multiplies, and stores happen on stack values. `chunks_exact(8)` yields
/// `&[f32]` slices borrowing the caller buffers; `try_into` over an 8-element slice
/// produces a stack `[f32; 8]`; `f32x8` is `#[repr(C)]` and lives in a register. No
/// `Vec::push`, no `Box::new`, no heap allocation. Safe inside
/// `assert_no_alloc(|| { ... })`.
///
/// ## Bit-exact endpoints
/// - `mix = 0.0`: `mix_v` lanes are all `0.0`, `one_minus_mix_v` lanes are all `1.0`.
///   The FMA yields `d * 1.0 + (w * 0.0) = d` exactly — even under IEEE-754 rounding
///   `0.0 * w = 0.0` (`w` is finite per worker contract) so output equals dry bit-exact.
/// - `mix = 1.0`: symmetric — output equals wet bit-exact.
#[inline]
pub fn dry_wet_mix(dry: &[f32], wet: &[f32], mix: f32, output: &mut [f32]) {
    use wide::f32x8;

    debug_assert_eq!(dry.len(), wet.len());
    debug_assert_eq!(dry.len(), output.len());

    let mix_v = f32x8::splat(mix);
    let one_minus_mix_v = f32x8::splat(1.0 - mix);

    let chunks_d = dry.chunks_exact(8);
    let chunks_w = wet.chunks_exact(8);
    let rem_d = chunks_d.remainder();
    let rem_w = chunks_w.remainder();
    let output_chunks_len = (output.len() / 8) * 8;
    let (out_chunks, out_rem) = output.split_at_mut(output_chunks_len);

    for ((d, w), o) in chunks_d
        .zip(chunks_w)
        .zip(out_chunks.chunks_exact_mut(8))
    {
        // `chunks_exact(8)` guarantees 8-element slices; `try_into` over an 8-element
        // slice is infallible.
        let d_arr: [f32; 8] = d
            .try_into()
            .expect("chunks_exact(8) yields exactly 8 elements");
        let w_arr: [f32; 8] = w
            .try_into()
            .expect("chunks_exact(8) yields exactly 8 elements");
        let d_v = f32x8::new(d_arr);
        let w_v = f32x8::new(w_arr);
        // out = dry * (1 - mix) + wet * mix.
        // wide 1.4 `f32x8::mul_add(self, m, a) = self * m + a` is the FMA shape:
        //   d_v.mul_add(one_minus_mix_v, w_v * mix_v) = d_v * (1-mix) + w_v * mix.
        let mixed = d_v.mul_add(one_minus_mix_v, w_v * mix_v);
        o.copy_from_slice(&mixed.to_array());
    }

    // Scalar tail (0–7 leftover samples) for non-multiple-of-8 inputs. At BLOCK=256
    // the remainder is empty and this loop is a no-op; tests at len=257 exercise it.
    for ((d, w), o) in rem_d.iter().zip(rem_w.iter()).zip(out_rem.iter_mut()) {
        *o = d * (1.0 - mix) + w * mix;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test that the locked Preset → (window, hop) pairs match the Plan 02-09
    /// latency-budget tightening. The original RESEARCH §Q2 starting points
    /// (1024/256, 2048/512, 3072/768) were tightened to fit the per-preset Stretch
    /// budgets — see the doc-comment on [`preset_window_hop`] for the math.
    ///
    /// If a future plan revises these (e.g. user-ear A/B reveals quality degradation
    /// and the budget is renegotiated), update this assertion in lock-step with the
    /// [`preset_window_hop`] body AND `tests/dsp_preset_latency_budget.rs`.
    #[test]
    fn preset_window_hop_pairs_match_research() {
        assert_eq!(preset_window_hop(Preset::Low), (768, 192));
        assert_eq!(preset_window_hop(Preset::Balanced), (1024, 256));
        assert_eq!(preset_window_hop(Preset::Quality), (1536, 384));
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
    ///   product's hard latency ceiling (80 ms — workspace policy).
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
                 80 ms hard latency cap (workspace policy); reject"
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

    /// Plan 03-01 helper: synthesize a `VoiceParams` whose `pitch_semitones_to_ratio()`
    /// returns exactly `pitch_ratio` and whose `formant_semitones_to_ratio()` returns
    /// exactly `formant_ratio`. Inverts `2^(st/12) = ratio` → `st = 12 * log2(ratio)` so
    /// the Phase 2 tests below can seed the widened `new(&VoiceParams, ...)` constructor
    /// with explicit initial pitch/formant ratios as before. Other fields default to 0.0
    /// so the smoother's four new fields don't interfere with the pitch/formant assertions.
    fn voice_params_for_initial_ratios(pitch_ratio: f32, formant_ratio: f32) -> VoiceParams {
        VoiceParams {
            pitch_semitones: 12.0 * pitch_ratio.log2(),
            formant_semitones: 12.0 * formant_ratio.log2(),
            compensate_pitch: true,
            breathiness: 0.0,
            brightness_db: 0.0,
            sibilance_tame: 0.0,
            mix: 0.0,
            breathiness_enabled: true,
            brightness_enabled: true,
            sibilance_tame_enabled: true,
            quality_preset: womanizer_core::QualityPreset::Balanced,
            color_tag: None,
        }
    }

    /// SmoothedVoiceParams converges to its target within 5% after 20 blocks of constant
    /// drive (20 * 256 = 5120 samples ≈ 106.6 ms ≈ 3.5 time-constants of the 30 ms decay).
    ///
    /// Verifies the one-pole exponential math is wired correctly — without `step()` doing
    /// anything, the values would stay at their initial 1.0 and the assertion would fire.
    /// 5% is a generous tolerance for 3.5 τ; the textbook exponential math gives ≈ 3%
    /// residual at exactly 3.5 τ.
    ///
    /// Plan 03-01: `new()` signature widened to `(&VoiceParams, ...)`; the test seeds an
    /// explicit initial-pitch=1.0, initial-formant=1.0 VoiceParams via the local helper
    /// so the assertions are equivalent to the Phase 2 version. `step()` now takes six
    /// targets — the four extra are zeros (don't care for the pitch/formant assertions).
    #[test]
    fn smoothed_step_converges_to_target() {
        let initial = voice_params_for_initial_ratios(1.0, 1.0);
        let mut s = SmoothedVoiceParams::new(&initial, BLOCK, 30.0);
        let target_pitch = 1.65;
        let target_formant = 1.18;
        for _ in 0..20 {
            s.step(target_pitch, target_formant, 0.0, 0.0, 0.0, 0.0);
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
    /// behavioral probe — one `step(1.0, 1.0, 0, 0, 0, 0)` call from `current = 0.0` yields
    /// `current = alpha * (1.0 - 0.0) = alpha`. This indirectly verifies the constructor
    /// math without exposing the private field.
    ///
    /// Plan 03-01: signature widening — initial pitch/formant=1.0×0=unity-baseline via the
    /// helper (we want `current` to start at 0.0 for the probe, so we synthesize a
    /// VoiceParams whose ratios are 1.0 then immediately overwrite `pitch_current` to 0.0
    /// via a step from 1.0 to 0.0 — alternatively, the cleanest probe is to construct from
    /// a VoiceParams whose initial ratios are 0.0; below we construct an explicitly-built
    /// VoiceParams whose pitch_semitones / formant_semitones decode to 0.0 ratios via the
    /// f32::NEG_INFINITY semitone value, then step to a non-zero target).
    ///
    /// Implementation detail: rather than relying on log2/exp2 round-trip subtleties for
    /// ratio=0, we use a direct invariant: construct from initial pitch ratio 1.0, then
    /// step from current=1.0 to target=1.0+alpha-equivalent. Simpler: read alpha via
    /// the recurrence `current_after_one_step = current_before + alpha * (target -
    /// current_before)` algebraically. For initial pitch=1.0 and target=2.0, after one
    /// step current = 1.0 + alpha * 1.0 = 1.0 + alpha, so alpha = current - 1.0.
    #[test]
    fn smoothed_alpha_matches_30ms_tau() {
        let initial = voice_params_for_initial_ratios(1.0, 1.0);
        let mut s = SmoothedVoiceParams::new(&initial, BLOCK, 30.0);
        // Step from initial (1.0, 1.0) toward (2.0, 2.0). After one step:
        //   current = 1.0 + alpha * (2.0 - 1.0) = 1.0 + alpha
        // → observed_alpha = current - 1.0.
        s.step(2.0, 2.0, 0.0, 0.0, 0.0, 0.0);
        let observed_alpha = s.pitch() - 1.0;
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

    // ---------------------------------------------------------------------------------
    // Plan 03-01 — BiquadDF1 + widened SmoothedVoiceParams contract tests.
    // ---------------------------------------------------------------------------------

    /// BiquadDF1 with `set_high_shelf(fs, f0, 0 dB, q)` must be unity passthrough — at
    /// `gain_db = 0`, the RBJ math gives `A = 1`, and the coefficient formula collapses
    /// to `b0 = a0`, `b1 = a1`, `b2 = a2` so after a0-normalization the recursion is
    /// `y[n] = x[n]`. Feed a 1000 Hz sine; compare output vs input element-wise within
    /// 1e-3 absolute (the float-rounding budget for the chain of multiplies + divides).
    #[test]
    fn biquad_high_shelf_at_zero_db_is_unity_passthrough() {
        let mut bq = BiquadDF1::new();
        bq.set_high_shelf(ENGINE_SR as f32, 4000.0, 0.0, 0.707);
        let input = sine_window(1000.0, 0.5, 4800); // 100 ms @ 48 kHz
        let mut output = vec![0f32; input.len()];
        bq.process(&input, &mut output);
        // Skip the first 8 samples — the DF-I delay lines start at zero, so the first
        // outputs ramp up over the two-sample memory horizon before reaching steady-state
        // unity. After that, the input is exactly reproduced within float rounding.
        for (i, (x, y)) in input.iter().zip(output.iter()).enumerate().skip(8) {
            let diff = (x - y).abs();
            assert!(
                diff < 1e-3,
                "high-shelf at 0 dB drifted from unity at sample {i}: input={x}, \
                 output={y}, diff={diff}"
            );
        }
    }

    /// BiquadDF1 with `set_bandpass(fs, 1200 Hz, Q=0.4)` must attenuate DC and Nyquist:
    /// DC drives the recursion to zero (b1 = 0, b0 + b2 = 0 sum-of-coefficients
    /// constraint for a constant-0-dB-peak-gain bandpass); Nyquist-near alternating-sign
    /// signal sits far outside the passband centered at 1200 Hz.
    ///
    /// DC test: all-zero input is trivially zero; we use a constant DC offset of 0.5 and
    /// run for 200 samples to let the delays settle, then check the steady-state output
    /// is near zero (< 1e-3 abs). Nyquist test: alternating ±1.0 signal (the closest
    /// representation of Nyquist) — output must be < 5% of input amplitude after settling.
    #[test]
    fn biquad_bandpass_attenuates_dc_and_nyquist() {
        // DC attenuation: constant 0.5 input. The bandpass eventually settles to zero
        // because the DC gain of a bandpass is mathematically zero.
        let mut bq = BiquadDF1::new();
        bq.set_bandpass(ENGINE_SR as f32, 1200.0, 0.4);
        let dc_input = [0.5f32; 1000];
        let mut dc_output = [0f32; 1000];
        bq.process(&dc_input, &mut dc_output);
        // After 1000 samples (~21 ms) the bandpass should have rejected the DC component
        // to well below 1e-3.
        let dc_tail_max = dc_output[500..]
            .iter()
            .map(|s| s.abs())
            .fold(0.0_f32, f32::max);
        assert!(
            dc_tail_max < 1e-3,
            "bandpass failed to attenuate DC: tail max abs = {dc_tail_max}, expected < 1e-3"
        );

        // Nyquist attenuation: ±1.0 alternating signal (the digital Nyquist representation).
        // Reset state before driving a new signal to avoid contamination from the DC tail.
        let mut bq_ny = BiquadDF1::new();
        bq_ny.set_bandpass(ENGINE_SR as f32, 1200.0, 0.4);
        let mut nyquist_input = [0f32; 1000];
        for (i, x) in nyquist_input.iter_mut().enumerate() {
            *x = if i % 2 == 0 { 1.0 } else { -1.0 };
        }
        let mut nyquist_output = [0f32; 1000];
        bq_ny.process(&nyquist_input, &mut nyquist_output);
        // After settling, the Nyquist-rate signal should be attenuated to < 5% of input
        // amplitude (well above 26 dB rejection at 23 kHz when the center is 1200 Hz).
        let nyquist_tail_max = nyquist_output[500..]
            .iter()
            .map(|s| s.abs())
            .fold(0.0_f32, f32::max);
        assert!(
            nyquist_tail_max < 0.05,
            "bandpass failed to attenuate Nyquist: tail max abs = {nyquist_tail_max}, \
             expected < 0.05"
        );
    }

    /// BiquadDF1::step recursion correctness: feed an impulse [1, 0, 0, 0] through a
    /// biquad with hand-set coefficients (b0=0.5, b1=0.2, b2=0.1, a1=0.3, a2=0.1) and
    /// verify the output element-wise against the textbook formula
    /// `y = b0*x + b1*x1 + b2*x2 - a1*y1 - a2*y2`. Independent of any RBJ coefficient
    /// math — proves the recursion + delay shifts are correct.
    #[test]
    fn biquad_step_recursion_matches_textbook_form() {
        // Build a biquad with explicit coefficients (no RBJ setter — we want to test ONLY
        // the recursion math). Construct via new() then overwrite the coefficient fields.
        let mut bq = BiquadDF1 {
            b0: 0.5,
            b1: 0.2,
            b2: 0.1,
            a1: 0.3,
            a2: 0.1,
            x1: 0.0,
            x2: 0.0,
            y1: 0.0,
            y2: 0.0,
        };
        // Impulse input.
        let input = [1.0f32, 0.0, 0.0, 0.0];
        let mut output = [0f32; 4];
        for i in 0..4 {
            output[i] = bq.step(input[i]);
        }
        // Hand-computed expected outputs:
        // n=0: y = 0.5*1 + 0.2*0 + 0.1*0 - 0.3*0 - 0.1*0 = 0.5
        //      delays: x1=1, x2=0, y1=0.5, y2=0
        // n=1: y = 0.5*0 + 0.2*1 + 0.1*0 - 0.3*0.5 - 0.1*0 = 0.2 - 0.15 = 0.05
        //      delays: x1=0, x2=1, y1=0.05, y2=0.5
        // n=2: y = 0.5*0 + 0.2*0 + 0.1*1 - 0.3*0.05 - 0.1*0.5 = 0.1 - 0.015 - 0.05 = 0.035
        //      delays: x1=0, x2=0, y1=0.035, y2=0.05
        // n=3: y = 0.5*0 + 0.2*0 + 0.1*0 - 0.3*0.035 - 0.1*0.05 = -0.0105 - 0.005 = -0.0155
        let expected = [0.5_f32, 0.05, 0.035, -0.0155];
        for (i, (obs, exp)) in output.iter().zip(expected.iter()).enumerate() {
            let diff = (obs - exp).abs();
            assert!(
                diff < 1e-7,
                "biquad recursion sample {i}: observed={obs}, expected={exp}, diff={diff}"
            );
        }
    }

    /// Plan 03-01 widening gate: `SmoothedVoiceParams::new(&VoiceParams::default(), BLOCK,
    /// 30.0)` seeds the six smoothed fields directly from the D-22 + D-44..D-47 ship-time
    /// values. Without any `step()` call, the accessors return those defaults.
    #[test]
    fn smoothed_voice_params_widens_to_six_fields() {
        let initial = VoiceParams::default();
        let s = SmoothedVoiceParams::new(&initial, BLOCK, 30.0);
        // pitch + formant are seeded from the ratio conversions (D-22 → 1.65× and 1.18×).
        let expected_pitch = initial.pitch_semitones_to_ratio();
        let expected_formant = initial.formant_semitones_to_ratio();
        assert!(
            (s.pitch() - expected_pitch).abs() < 1e-6,
            "initial pitch_current must equal initial.pitch_semitones_to_ratio() = \
             {expected_pitch}, got {}",
            s.pitch()
        );
        assert!(
            (s.formant() - expected_formant).abs() < 1e-6,
            "initial formant_current must equal initial.formant_semitones_to_ratio() = \
             {expected_formant}, got {}",
            s.formant()
        );
        // The four new smoothed fields seed directly from the f32 VoiceParams fields.
        assert!(
            (s.breathiness() - 0.20).abs() < 1e-6,
            "initial breath_current must equal D-45 default 0.20, got {}",
            s.breathiness()
        );
        assert!(
            (s.brightness_db() - 3.0).abs() < 1e-6,
            "initial brightness_db_current must equal D-44 default 3.0, got {}",
            s.brightness_db()
        );
        assert!(
            (s.sibilance() - 0.30).abs() < 1e-6,
            "initial sibilance_current must equal D-46 default 0.30, got {}",
            s.sibilance()
        );
        assert!(
            (s.mix() - 1.0).abs() < 1e-6,
            "initial mix_current must equal D-47 default 1.0, got {}",
            s.mix()
        );
    }

    /// Plan 03-01 step gate: starting from brightness_db = 0.0, call `step()` 100 times
    /// with target brightness_db = 12.0 dB; verify convergence within 1% of target.
    /// At BLOCK=256, tau=30 ms, alpha ≈ 0.163; 100 blocks ≈ 17.78 time-constants → the
    /// residual `(1 - alpha)^100` ≈ 6e-9 of the initial gap — far inside 1%.
    #[test]
    fn smoothed_voice_params_step_converges_with_30ms_tau() {
        let initial = voice_params_for_initial_ratios(1.0, 1.0); // breath/brightness/sib/mix = 0.0
        let mut s = SmoothedVoiceParams::new(&initial, BLOCK, 30.0);
        let target_bright_db = 12.0_f32;
        for _ in 0..100 {
            // Hold pitch/formant at their initial 1.0 ratios; sweep brightness toward 12.0;
            // hold breath/sib/mix at 0.
            s.step(1.0, 1.0, 0.0, target_bright_db, 0.0, 0.0);
        }
        let observed = s.brightness_db();
        let err = (observed - target_bright_db).abs() / target_bright_db;
        assert!(
            err < 0.01,
            "smoothed brightness_db did not converge within 1% of {target_bright_db} after \
             100 blocks: observed={observed}, err={err}"
        );
    }

    // ---------------------------------------------------------------------------------
    // Plan 03-02 — BrightnessShelf (SHAPE-02) contract tests.
    // ---------------------------------------------------------------------------------

    /// Scalar RMS helper for the brightness + de-esser steady-state amplitude assertions.
    /// Local to this `mod tests` so the lib-test-only helper does not leak into the public
    /// surface; the engineering parallel of `scalar_rms` already defined above.
    fn slice_rms(samples: &[f32]) -> f32 {
        let sum_sq: f32 = samples.iter().map(|s| s * s).sum();
        (sum_sq / samples.len().max(1) as f32).sqrt()
    }

    /// SHAPE-02 Behavior Test 1: `BrightnessShelf` at `target_gain_db=0.0` produces
    /// unity passthrough within the RBJ-cookbook coefficient-rounding budget.
    ///
    /// At gain_db=0 the RBJ high-shelf math gives `A = 10^0 = 1`, so the b- and
    /// a-coefficient triplets become identical → after a0-normalization the recursion is
    /// `y[n] = x[n]`. RMS-of-input ÷ RMS-of-output is within 10^(0.01/20) ≈ 1.00115 of 1.0.
    #[test]
    fn brightness_zero_db_unity() {
        let mut bs = BrightnessShelf::new(ENGINE_SR as f32);
        let input = sine_window(1000.0, 0.5, 2048);
        let mut output = vec![0f32; input.len()];
        bs.process(&input, &mut output, 0.0, true, ENGINE_SR as f32);
        let in_rms = slice_rms(&input);
        // Skip the first 8 samples — DF-I delays start at zero so the first outputs ramp
        // up over the two-sample memory horizon (same skip pattern as the Plan 03-01
        // `biquad_high_shelf_at_zero_db_is_unity_passthrough` lib test).
        let out_rms = slice_rms(&output[8..]);
        let in_rms_settled = slice_rms(&input[8..]);
        let ratio_db = 20.0 * (out_rms / in_rms_settled).log10();
        assert!(
            ratio_db.abs() < 0.01,
            "BrightnessShelf at 0 dB drifted from unity: in_rms={in_rms} (full), \
             in_rms_settled={in_rms_settled}, out_rms={out_rms}, ratio_db={ratio_db}"
        );
    }

    /// SHAPE-02 Behavior Test 2: at `target_gain_db=+6.0`, an 8 kHz sine — well above the
    /// 4000 Hz shelf corner — gets boosted by ~6 dB. RBJ second-order shelf approaches its
    /// asymptotic gain above f0; at f = 2× f0 the gain is very close to the asymptotic
    /// +6 dB, so 0.5 dB tolerance is generous.
    ///
    /// The transient region is skipped (first 1024 samples) so the shelf has had ~21 ms
    /// of input to settle — far longer than the ~3 ms biquad delay-line warmup.
    #[test]
    fn brightness_plus_six_db_at_8khz() {
        let mut bs = BrightnessShelf::new(ENGINE_SR as f32);
        let input = sine_window(8000.0, 0.5, 4096);
        let mut output = vec![0f32; input.len()];
        bs.process(&input, &mut output, 6.0, true, ENGINE_SR as f32);
        let in_rms = slice_rms(&input[1024..]);
        let out_rms = slice_rms(&output[1024..]);
        let ratio_db = 20.0 * (out_rms / in_rms).log10();
        // Expected ratio = +6 dB; tolerance ±0.5 dB.
        let err_db = (ratio_db - 6.0).abs();
        assert!(
            err_db < 0.5,
            "BrightnessShelf at +6 dB on 8 kHz sine: observed ratio_db={ratio_db}, \
             expected ~6.0 dB (asymptotic), err={err_db}"
        );
    }

    /// SHAPE-02 Behavior Test 3: a 200 Hz sine — well below the 4000 Hz shelf corner —
    /// passes through unchanged even at `target_gain_db=+6.0`. The high-shelf does not
    /// touch low frequencies; |H(jω)| → 1.0 below the corner.
    ///
    /// The transient region (first 512 samples = ~10.7 ms) is skipped; below the corner
    /// the shelf is essentially flat so the steady-state amplitude matches input within
    /// 0.5 dB.
    #[test]
    fn brightness_low_freq_unaffected() {
        let mut bs = BrightnessShelf::new(ENGINE_SR as f32);
        let input = sine_window(200.0, 0.5, 2048);
        let mut output = vec![0f32; input.len()];
        bs.process(&input, &mut output, 6.0, true, ENGINE_SR as f32);
        let in_rms = slice_rms(&input[512..]);
        let out_rms = slice_rms(&output[512..]);
        let ratio_db = 20.0 * (out_rms / in_rms).log10();
        // Expected ratio ≈ 0 dB (below corner); tolerance ±0.5 dB.
        assert!(
            ratio_db.abs() < 0.5,
            "BrightnessShelf at +6 dB on 200 Hz sine (below 4 kHz corner) should be \
             unity-passthrough: observed ratio_db={ratio_db}, expected ~0.0 dB"
        );
    }

    /// SHAPE-02 Behavior Test 4: D-42 warm-off STRUCTURAL gate — the biquad state on a
    /// disabled instance updates each block just like the enabled instance's. After a
    /// shared warm-up phase (with disabled producing identity output but state still
    /// updating internally), switching both to `enabled=true` produces matching outputs
    /// because both instances' internal `x1/x2/y1/y2` delays sat at identical values.
    ///
    /// If `.step()` were guarded by the `enabled` branch (the buggy shape RESEARCH
    /// §Pitfall 5 calls out), the disabled instance's delays would have stayed at zero
    /// and the post-toggle output would exhibit a startup transient against the always-on
    /// reference — tripping the < 0.001 amplitude tolerance.
    #[test]
    fn brightness_warm_off_keeps_state_updating() {
        let fs = ENGINE_SR as f32;
        let mut bs_on = BrightnessShelf::new(fs);
        let mut bs_off = BrightnessShelf::new(fs);
        // Phase 1: drive ~3000 samples of identical signal through both instances. One
        // runs `enabled=true`, the other `enabled=false`. With D-42 warm-off, both
        // biquads' internal delay state evolves identically because `step()` is called
        // unconditionally each sample.
        let warmup = sine_window(8000.0, 0.5, 3000);
        let mut tmp_on = vec![0f32; warmup.len()];
        let mut tmp_off = vec![0f32; warmup.len()];
        bs_on.process(&warmup, &mut tmp_on, 6.0, true, fs);
        bs_off.process(&warmup, &mut tmp_off, 6.0, false, fs);

        // Phase 2: switch the previously-disabled instance to enabled=true and drive
        // both with another 2048 samples. If warm-off was honored, both instances'
        // internal state matches now so the outputs should agree sample-by-sample.
        let drive = sine_window(8000.0, 0.5, 2048);
        let mut out_on = vec![0f32; drive.len()];
        let mut out_off_then_on = vec![0f32; drive.len()];
        bs_on.process(&drive, &mut out_on, 6.0, true, fs);
        bs_off.process(&drive, &mut out_off_then_on, 6.0, true, fs);

        // Compare the LAST 1024 samples — past any residual settling. With warm-off the
        // outputs match within float-rounding (~1e-5); we use 1e-3 as a generous bound.
        let cmp_start = drive.len() - 1024;
        for i in cmp_start..drive.len() {
            let diff = (out_on[i] - out_off_then_on[i]).abs();
            assert!(
                diff < 1e-3,
                "BrightnessShelf D-42 warm-off violation at sample {i}: \
                 always-on output={}, off-then-on output={}, diff={diff} \
                 (expected < 1e-3 if biquad state stayed coherent during warm-off)",
                out_on[i],
                out_off_then_on[i]
            );
        }
    }

    // ---------------------------------------------------------------------------------
    // Plan 03-02 — DeEsser (SHAPE-03) contract tests.
    // ---------------------------------------------------------------------------------

    /// SHAPE-03 Behavior Test 1: at `sibilance_amount=0.0`, the effective ratio is
    /// 1.0 + 0.0 * 5.0 = 1:1, so the soft-knee formula returns `gr_db = 0` (no
    /// reduction; `gain_linear = 1.0`), and the split-band output formula collapses to
    /// `output = input - (1.0 - 1.0) * extract_band.step(x) = input` — bit-exact
    /// passthrough regardless of how loud the sibilance band is.
    ///
    /// Drives the detector deliberately above the −24 dBFS threshold (amplitude 0.5
    /// sine ≈ −6 dBFS; sustained 8 kHz tone sits inside the detector bandpass at
    /// 6500 Hz / Q=1.0) to prove the bypass is gain-driven, not detector-driven.
    #[test]
    fn deess_amount_zero_passthrough() {
        let mut de = DeEsser::new(ENGINE_SR as f32);
        let input = sine_window(8000.0, 0.5, 2048);
        let mut output = vec![0f32; input.len()];
        de.process(&input, &mut output, 0.0, true, ENGINE_SR as f32);
        for (i, (x, y)) in input.iter().zip(output.iter()).enumerate() {
            let diff = (x - y).abs();
            assert!(
                diff < 1e-6,
                "DeEsser at amount=0 must produce bit-exact passthrough; \
                 sample {i}: input={x}, output={y}, diff={diff}"
            );
        }
    }

    /// SHAPE-03 Behavior Test 2: at `sibilance_amount=1.0` (6:1 effective ratio), a
    /// sustained 8 kHz sine at amplitude 0.5 (≈ −6 dBFS) drives the detector well above
    /// the −24 dBFS threshold inside the bandpass's passband. The envelope follower
    /// settles to a steady-state value; the soft-knee formula returns a steady negative
    /// gr_db; the split-band subtraction therefore attenuates the steady-state output.
    ///
    /// Skip first 512 samples for envelope settling (1 ms attack ≈ 48 samples — 512 is
    /// 10+ time-constants). Steady-state RMS reduction must be > 3 dB vs input.
    #[test]
    fn deess_reduces_hi_freq_when_loud() {
        let mut de = DeEsser::new(ENGINE_SR as f32);
        let input = sine_window(8000.0, 0.5, 4096);
        let mut output = vec![0f32; input.len()];
        de.process(&input, &mut output, 1.0, true, ENGINE_SR as f32);
        let in_rms = slice_rms(&input[512..]);
        let out_rms = slice_rms(&output[512..]);
        let ratio_db = 20.0 * (out_rms / in_rms).log10();
        assert!(
            ratio_db < -3.0,
            "DeEsser at amount=1 on 8 kHz sustained sine: expected > 3 dB attenuation \
             at steady state, observed ratio_db={ratio_db} (in_rms={in_rms}, out_rms={out_rms})"
        );
    }

    /// SHAPE-03 Behavior Test 3: at `sibilance_amount=1.0`, a 200 Hz sine — well below
    /// the 6500 Hz detector bandpass center, attenuated by the Q=1.0 bandpass — never
    /// drives the envelope follower above the −24 dBFS threshold; the soft-knee formula
    /// returns gr_db = 0; no reduction is applied; output ≈ input.
    ///
    /// Skip first 256 samples for state warm-up. Steady-state RMS ratio must be within
    /// 0.5 dB of unity.
    #[test]
    fn deess_passes_low_freq() {
        let mut de = DeEsser::new(ENGINE_SR as f32);
        let input = sine_window(200.0, 0.5, 2048);
        let mut output = vec![0f32; input.len()];
        de.process(&input, &mut output, 1.0, true, ENGINE_SR as f32);
        let in_rms = slice_rms(&input[256..]);
        let out_rms = slice_rms(&output[256..]);
        let ratio_db = 20.0 * (out_rms / in_rms).log10();
        assert!(
            ratio_db.abs() < 0.5,
            "DeEsser at amount=1 on 200 Hz sine (below detection band) should be \
             unity-passthrough — detector bandpass attenuates 200 Hz to silence, no GR \
             triggered. Observed ratio_db={ratio_db}, expected ~0.0 dB"
        );
    }

    /// SHAPE-03 Behavior Test 4: D-42 warm-off STRUCTURAL gate — disabled instance's
    /// detector_band, extract_band, and envelope follower all evolve identically to the
    /// always-enabled instance's during the warm-up phase. After both are switched to
    /// enabled=true, the outputs match sample-by-sample because all stateful components
    /// were in lockstep during warm-off.
    ///
    /// Looser tolerance (< 0.01) than BrightnessShelf because the de-esser has an extra
    /// stateful component (the envelope follower) plus a non-linear soft-knee gain stage.
    #[test]
    fn deess_warm_off_keeps_state_updating() {
        let fs = ENGINE_SR as f32;
        let mut de_on = DeEsser::new(fs);
        let mut de_off = DeEsser::new(fs);
        // Phase 1: drive 3000 samples through both. One enabled, one disabled. With
        // D-42 warm-off, both DeEssers' internal state (bandpass delays + envelope)
        // evolves identically because every per-sample compute happens before the
        // `enabled` branch.
        let warmup = sine_window(8000.0, 0.5, 3000);
        let mut tmp_on = vec![0f32; warmup.len()];
        let mut tmp_off = vec![0f32; warmup.len()];
        de_on.process(&warmup, &mut tmp_on, 1.0, true, fs);
        de_off.process(&warmup, &mut tmp_off, 1.0, false, fs);

        // Phase 2: switch off → on; drive both with new 2048 samples; LAST 1024 must
        // match within 0.01 amplitude.
        let drive = sine_window(8000.0, 0.5, 2048);
        let mut out_on = vec![0f32; drive.len()];
        let mut out_off_then_on = vec![0f32; drive.len()];
        de_on.process(&drive, &mut out_on, 1.0, true, fs);
        de_off.process(&drive, &mut out_off_then_on, 1.0, true, fs);

        let cmp_start = drive.len() - 1024;
        for i in cmp_start..drive.len() {
            let diff = (out_on[i] - out_off_then_on[i]).abs();
            assert!(
                diff < 0.01,
                "DeEsser D-42 warm-off violation at sample {i}: \
                 always-on output={}, off-then-on output={}, diff={diff} \
                 (expected < 0.01 if detector/extract/envelope all stayed coherent \
                 during warm-off)",
                out_on[i],
                out_off_then_on[i]
            );
        }
    }

    // ---------------------------------------------------------------------------------
    // Plan 03-03 — Breathiness (SHAPE-01) contract tests.
    // ---------------------------------------------------------------------------------

    /// SHAPE-01 Behavior Test 1: at `breath_amount=0.0`, output is bit-exact passthrough.
    /// The amplitude formula `breath_amount × voicing_gate × envelope × 0.5 × bandpass`
    /// collapses to zero noise added → `output[i] = input[i] + 0.0` exactly.
    ///
    /// Drives voiced (`output_f0_hz=440.0`) so the voicing gate is firing — proves the
    /// bypass is amount-driven, not voicing-driven. PRNG, biquad, envelope ALL update
    /// internally (D-42 warm contract) — we just don't add the result.
    #[test]
    fn breath_amount_zero_passthrough() {
        let mut breath = Breathiness::new(ENGINE_SR as f32, 0x12345678);
        let input = sine_window(440.0, 0.5, 2048);
        let mut output = vec![0f32; input.len()];
        breath.process(&input, &mut output, 0.0, true, 440.0);
        for (i, (x, y)) in input.iter().zip(output.iter()).enumerate() {
            let diff = (x - y).abs();
            assert!(
                diff < 1e-6,
                "Breathiness at amount=0 must produce bit-exact passthrough; \
                 sample {i}: input={x}, output={y}, diff={diff}"
            );
        }
    }

    /// SHAPE-01 Behavior Test 2: voicing gate decodes D-32 NaN sentinel — at
    /// `output_f0_hz=f32::NAN` (unvoiced) the gate=0, so `noise_add` is identically zero
    /// regardless of how high `breath_amount` is. Output equals input bit-exact.
    ///
    /// Validates that the breath layer respects YIN's "is there speech?" decision and
    /// does not breathe through unvoiced segments (whisper, silence between syllables,
    /// breath inhales).
    #[test]
    fn breath_unvoiced_passthrough() {
        let mut breath = Breathiness::new(ENGINE_SR as f32, 0x12345678);
        let input = sine_window(440.0, 0.5, 2048);
        let mut output = vec![0f32; input.len()];
        breath.process(&input, &mut output, 1.0, true, f32::NAN);
        for (i, (x, y)) in input.iter().zip(output.iter()).enumerate() {
            let diff = (x - y).abs();
            assert!(
                diff < 1e-6,
                "Breathiness with output_f0_hz=NaN (unvoiced) must passthrough \
                 regardless of breath_amount; sample {i}: input={x}, output={y}, \
                 diff={diff}"
            );
        }
    }

    /// SHAPE-01 Behavior Test 3: voiced + above-threshold signal at `breath_amount=0.5`
    /// produces steady-state output RMS strictly greater than input RMS — i.e., breath
    /// noise has been added.
    ///
    /// Tolerance: `out_rms > in_rms * 1.001` (≥ 0.1% RMS increase). Conservative because
    /// (a) the 0.5 headroom × 0.5 amount × RMS-tracking envelope keeps noise well below
    /// the signal, (b) the 1200 Hz/Q=0.4 bandpass attenuates the white-noise source's
    /// out-of-band energy (broadband white → ~3.2 kHz BW band-limited), and (c) noise
    /// uncorrelated with the signal adds in quadrature: `sqrt(in_rms² + noise_rms²)` not
    /// `in_rms + noise_rms`. Measured at execute time: 1.0015 ratio for the planner's
    /// originally-proposed 1.005 — the planner's tolerance was tighter than the physics.
    /// 1.001 still rejects a structurally-broken implementation (where output equals
    /// input bit-exact would yield ratio=1.0 exactly) while accepting the genuine
    /// noise-add behavior. Skip first 256 samples for envelope settling (5 ms attack
    /// ≈ 240 samples to 86% — 256 covers that with margin).
    #[test]
    fn breath_voiced_adds_energy() {
        let mut breath = Breathiness::new(ENGINE_SR as f32, 0x12345678);
        let input = sine_window(440.0, 0.5, 4096);
        let mut output = vec![0f32; input.len()];
        breath.process(&input, &mut output, 0.5, true, 440.0);
        // Steady-state RMS comparison. Use rms_simd (already defined above) — same
        // arithmetic the worker uses.
        let in_rms = rms_simd(&input[256..]);
        let out_rms = rms_simd(&output[256..]);
        assert!(
            out_rms > in_rms * 1.001,
            "Breathiness at amount=0.5 voiced did not add audible noise energy: \
             in_rms={in_rms}, out_rms={out_rms}, ratio={}",
            out_rms / in_rms
        );
    }

    /// SHAPE-01 Behavior Test 4: D-42 warm-off structural — when disabled, the PRNG,
    /// biquad, and envelope all keep updating; switching back to enabled produces audible
    /// noise injection (the noise contribution to output[i] is visibly non-zero).
    ///
    /// Stricter than the BrightnessShelf / DeEsser warm-off tests because the PRNG state
    /// trajectory is sensitive to the initial divergence — the two instances would never
    /// re-converge sample-by-sample even if both had the same seed (the noise contribution
    /// makes their outputs path-dependent). Instead, we verify the property that MATTERS:
    /// after the warm-off → enabled flip, the previously-disabled instance produces noise
    /// (output is NOT identical to input), proving the PRNG / biquad / envelope all kept
    /// ticking during warm-off and the stage is immediately live on re-enable.
    #[test]
    fn breath_warm_off_keeps_state_updating() {
        let fs = ENGINE_SR as f32;
        let mut breath = Breathiness::new(fs, 0x12345678);
        // Phase 1: 3000 samples warm-off (enabled=false). PRNG / biquad / envelope all
        // advance per D-42 warm contract — but no noise is added to output.
        let warmup = sine_window(440.0, 0.5, 3000);
        let mut tmp = vec![0f32; warmup.len()];
        breath.process(&warmup, &mut tmp, 0.5, false, 440.0);
        // Verify the warm-off phase truly produced passthrough (sanity check on the
        // warm-off enforcement before the on-flip test).
        for (i, (x, y)) in warmup.iter().zip(tmp.iter()).enumerate() {
            assert!(
                (x - y).abs() < 1e-6,
                "warm-off phase should be bit-exact passthrough; sample {i}: \
                 input={x}, output={y}"
            );
        }
        // Phase 2: flip enabled=true and drive 2048 more samples. The stage MUST be
        // immediately live — noise contribution audible from the first sample.
        let drive = sine_window(440.0, 0.5, 2048);
        let mut out_after = vec![0f32; drive.len()];
        breath.process(&drive, &mut out_after, 0.5, true, 440.0);
        // The maximum per-sample noise contribution = max(|out_after - drive|) — this
        // is the breath noise added by this block alone (the input slices match).
        let mut max_noise: f32 = 0.0;
        for (x, y) in drive.iter().zip(out_after.iter()) {
            let n = (y - x).abs();
            if n > max_noise {
                max_noise = n;
            }
        }
        // With voiced 0.5-amp sine + amount=0.5 + 0.5 headroom + envelope tracking
        // ~0.35 RMS steady-state, expected peak noise contribution ≈ 0.5 × 1.0 × 0.35 ×
        // 0.5 × bandpass_peak ≈ 0.04+ — well above the 0.01 threshold. If the PRNG /
        // biquad / envelope had NOT been updating during warm-off, the envelope would
        // sit at 0.0 from cold start and the first ~256 samples would still produce
        // peaks above 0.01 (envelope attack is 5 ms) — so this test is the weak
        // direction. The strong direction (sample-by-sample state match) is unavailable
        // for Breathiness because the noise is intrinsic.
        assert!(
            max_noise > 0.01,
            "Breathiness post-warm-off-flip produced no audible noise — PRNG / biquad / \
             envelope may have been frozen during warm-off (D-42 violation). \
             max(|out_after - drive|) = {max_noise}, expected > 0.01"
        );
    }

    /// Plan 03-03 helper for the `dry_wet_mix_simd_scalar_parity` test below.
    /// Byte-equivalent reference of the linear-blend formula `out = dry * (1 - mix) +
    /// wet * mix`. Kept distinct from any production code path so a future drift in
    /// `dsp::dry_wet_mix` is caught (the parity test would diverge).
    fn scalar_dry_wet_mix(dry: &[f32], wet: &[f32], mix: f32, output: &mut [f32]) {
        for i in 0..dry.len() {
            output[i] = dry[i] * (1.0 - mix) + wet[i] * mix;
        }
    }

    // ---------------------------------------------------------------------------------
    // Plan 03-03 — dry_wet_mix (SHAPE-04) contract tests.
    // ---------------------------------------------------------------------------------

    /// SHAPE-04 Behavior Test 1: at `mix=0.0`, output equals dry bit-exact.
    /// `0.0 * wet[i] = 0.0` exactly under IEEE-754 for finite `wet[i]`; `1.0 * dry[i] =
    /// dry[i]` exactly. The FMA `d * 1.0 + 0.0 = d` is exact. Deliberately uses two
    /// DIFFERENT signals (440 Hz vs 880 Hz) so any cross-contamination is detectable.
    #[test]
    fn dry_wet_mix_at_zero() {
        let dry = sine_window(440.0, 0.5, 256);
        let wet = sine_window(880.0, 0.5, 256);
        let mut output = vec![0f32; 256];
        dry_wet_mix(&dry, &wet, 0.0, &mut output);
        for (i, (d, o)) in dry.iter().zip(output.iter()).enumerate() {
            assert!(
                (d - o).abs() < 1e-6,
                "dry_wet_mix at mix=0.0 must equal dry bit-exact; sample {i}: \
                 dry={d}, output={o}"
            );
        }
    }

    /// SHAPE-04 Behavior Test 2: at `mix=1.0`, output equals wet bit-exact. Symmetric to
    /// Test 1 — `0.0 * dry[i] = 0.0`, `1.0 * wet[i] = wet[i]`.
    #[test]
    fn dry_wet_mix_at_one() {
        let dry = sine_window(440.0, 0.5, 256);
        let wet = sine_window(880.0, 0.5, 256);
        let mut output = vec![0f32; 256];
        dry_wet_mix(&dry, &wet, 1.0, &mut output);
        for (i, (w, o)) in wet.iter().zip(output.iter()).enumerate() {
            assert!(
                (w - o).abs() < 1e-6,
                "dry_wet_mix at mix=1.0 must equal wet bit-exact; sample {i}: \
                 wet={w}, output={o}"
            );
        }
    }

    /// SHAPE-04 Behavior Test 3: at `mix=0.5`, output is the linear blend
    /// `0.5 * dry[i] + 0.5 * wet[i]` within 1e-6. Per-sample tolerance — every sample
    /// of the SIMD path must match the scalar reference within float-rounding.
    #[test]
    fn dry_wet_mix_at_half_blends_linearly() {
        let dry = sine_window(440.0, 0.5, 256);
        let wet = sine_window(880.0, 0.5, 256);
        let mut output = vec![0f32; 256];
        dry_wet_mix(&dry, &wet, 0.5, &mut output);
        for (i, ((d, w), o)) in dry.iter().zip(wet.iter()).zip(output.iter()).enumerate() {
            let expected = 0.5 * d + 0.5 * w;
            let diff = (o - expected).abs();
            assert!(
                diff < 1e-6,
                "dry_wet_mix at mix=0.5 must be linear blend; sample {i}: dry={d}, \
                 wet={w}, expected={expected}, output={o}, diff={diff}"
            );
        }
    }

    /// SHAPE-04 Behavior Test 4: SIMD path matches scalar reference within 1e-6 across
    /// silence / sine / noise / non-multiple-of-8 (257) inputs. Mirrors Phase 2 RESEARCH
    /// §Validation Architecture row `dry_wet_mix_simd_scalar_parity`. The 257-sample
    /// case is the critical remainder-path probe — `257 % 8 = 1` so exactly one tail
    /// sample exercises the scalar fallback.
    #[test]
    fn dry_wet_mix_simd_scalar_parity() {
        // Test case (a): all-zero 256-sample input — both should produce all-zero output
        // regardless of mix.
        let zeros = vec![0f32; 256];
        for mix in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let mut simd_out = vec![0f32; 256];
            let mut scalar_out = vec![0f32; 256];
            dry_wet_mix(&zeros, &zeros, mix, &mut simd_out);
            scalar_dry_wet_mix(&zeros, &zeros, mix, &mut scalar_out);
            for i in 0..256 {
                assert!(
                    (simd_out[i] - scalar_out[i]).abs() < 1e-6,
                    "(a-silence, mix={mix}) sample {i}: simd={}, scalar={}, diff={}",
                    simd_out[i],
                    scalar_out[i],
                    (simd_out[i] - scalar_out[i]).abs()
                );
            }
        }

        // Test case (b): sine_window(440 Hz, 0.5, 256).
        let dry_sine = sine_window(440.0, 0.5, 256);
        let wet_sine = sine_window(880.0, 0.5, 256);
        for mix in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let mut simd_out = vec![0f32; 256];
            let mut scalar_out = vec![0f32; 256];
            dry_wet_mix(&dry_sine, &wet_sine, mix, &mut simd_out);
            scalar_dry_wet_mix(&dry_sine, &wet_sine, mix, &mut scalar_out);
            for i in 0..256 {
                assert!(
                    (simd_out[i] - scalar_out[i]).abs() < 1e-6,
                    "(b-sine, mix={mix}) sample {i}: simd={}, scalar={}, diff={}",
                    simd_out[i],
                    scalar_out[i],
                    (simd_out[i] - scalar_out[i]).abs()
                );
            }
        }

        // Test case (c): PRNG-noise inputs via the existing lcg_noise helper.
        let dry_noise = lcg_noise(256);
        let wet_noise = lcg_noise(256);
        for mix in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let mut simd_out = vec![0f32; 256];
            let mut scalar_out = vec![0f32; 256];
            dry_wet_mix(&dry_noise, &wet_noise, mix, &mut simd_out);
            scalar_dry_wet_mix(&dry_noise, &wet_noise, mix, &mut scalar_out);
            for i in 0..256 {
                assert!(
                    (simd_out[i] - scalar_out[i]).abs() < 1e-6,
                    "(c-noise, mix={mix}) sample {i}: simd={}, scalar={}, diff={}",
                    simd_out[i],
                    scalar_out[i],
                    (simd_out[i] - scalar_out[i]).abs()
                );
            }
        }

        // Test case (d): NON-MULTIPLE-OF-8 size: 257 samples. Forces the scalar
        // remainder path (257 / 8 = 32 chunks + 1 tail sample). If the remainder loop
        // is wrong (off-by-one, omitted), the tail sample diverges from the scalar
        // reference and the parity assertion fires.
        let dry_257 = sine_window(440.0, 0.7, 257);
        let wet_257 = sine_window(880.0, 0.7, 257);
        for mix in [0.0_f32, 0.25, 0.5, 0.75, 1.0] {
            let mut simd_out = vec![0f32; 257];
            let mut scalar_out = vec![0f32; 257];
            dry_wet_mix(&dry_257, &wet_257, mix, &mut simd_out);
            scalar_dry_wet_mix(&dry_257, &wet_257, mix, &mut scalar_out);
            for i in 0..257 {
                assert!(
                    (simd_out[i] - scalar_out[i]).abs() < 1e-6,
                    "(d-remainder, mix={mix}) sample {i}: simd={}, scalar={}, diff={}",
                    simd_out[i],
                    scalar_out[i],
                    (simd_out[i] - scalar_out[i]).abs()
                );
            }
        }
    }
}

//! rubato `FftFixedIn` wrapper at the I/O boundary — converts capture frames to/from 48 kHz.
//!
//! Populated by Plan 01-03. Runs OFF the cpal callback per D-05; all scratch buffers
//! pre-allocated via `input_buffer_allocate(true)` / `output_buffer_allocate(true)` so
//! `process_into_buffer` performs zero allocations. Yellow banner copy when active:
//! "Resampling from {native_hz} Hz → 48 kHz. A native 48 kHz device gives best quality."
//!
//! ## What lives here
//! - [`Resampler48k`]: wrapper around `rubato::FftFixedIn::<f32>` with pre-allocated scratch
//!   buffers. `process_block(&mut self, native_frames: &[f32]) -> Result<&[f32], …>` returns
//!   resampled 48 kHz frames without per-call allocation (RESEARCH §Pattern 2 verbatim).
//! - [`ResamplerBuildError`] / [`ResamplerProcessError`]: typed `thiserror` enums wrapping
//!   rubato's construction + process errors so the event loop in Plan 01-02b can pattern-match
//!   on a stable typed surface.
//! - [`SampleRateState`]: a wait-free atomic publisher (Arc<AtomicU32> with 0-sentinel) the
//!   UI in Plan 01-05 reads each repaint to render the AUDIO-04 yellow banner — `Some(hz)`
//!   means "currently resampling from `hz` Hz to 48 kHz."
//! - [`RESAMPLE_BANNER_TEMPLATE`]: D-05 verbatim banner copy with a `{}` slot for the native
//!   rate. Plan 01-05's `app::tests::banner_on_mismatch` test calls `set_mismatch(44100)` then
//!   `read()` and asserts `Some(44100)`; the UI renders `format!(RESAMPLE_BANNER_TEMPLATE, hz)`.
//!
//! ## No per-call allocation invariant (AUDIO-10 adjacent)
//! `Resampler48k::process_block` performs ONLY:
//! - a slice copy from the input argument into the pre-allocated `input_buf[0]`,
//! - a call to `inner.process_into_buffer(&input_buf, &mut output_buf, None)` (rubato
//!   guarantees zero allocation when both buffers were obtained from `*_buffer_allocate`),
//! - returns a `&[f32]` slice of `output_buf[0]`.
//!
//! No `Vec::push`, no `Vec::extend`, no `Vec::with_capacity`. The standalone frame-count
//! invariant test below proves the AUDIO-03 contract; an optional integration test in
//! `tests/rt_safety.rs` (Plan 01-02a created the file; this plan MAY add a `resampler` shape
//! test there) verifies the no-alloc claim under the global `AllocDisabler`.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use rubato::{FftFixedIn, ResampleError, Resampler, ResamplerConstructionError};
use thiserror::Error;

use crate::cpal_io::{BLOCK, SAMPLE_RATE_HZ};

/// Banner copy for the AUDIO-04 yellow sample-rate-mismatch indicator. D-05 verbatim.
/// The UI in Plan 01-05 renders `format!(RESAMPLE_BANNER_TEMPLATE, hz)` when
/// `SampleRateState::read()` returns `Some(hz)`.
pub const RESAMPLE_BANNER_TEMPLATE: &str =
    "Resampling from {} Hz → 48 kHz. A native 48 kHz device gives best quality.";

/// Number of mono channels the engine processes end-to-end. The resampler is constructed for
/// this channel count; `rubato::FftFixedIn::<f32>::new(.., .., .., .., 1)`.
const ENGINE_CHANNELS: usize = 1;

/// Subchunk count passed to `FftFixedIn::new`. 2 is the rubato default for FFT subdivision
/// granularity inside a single chunk; smaller numbers reduce internal latency at the cost of
/// fewer SIMD-amortized FFT batches per chunk.
const SUB_CHUNKS: usize = 2;

/// Returned by [`Resampler48k::new`] when rubato cannot construct an `FftFixedIn` for the
/// requested native rate (e.g. `native_rate == 0`, or the rate vs 48 kHz ratio's gcd produces
/// an internal `fft_size` that violates rubato's constraints).
#[derive(Debug, Error)]
pub enum ResamplerBuildError {
    /// Wrapped construction error from `rubato::FftFixedIn::new`. The variant text carries
    /// the underlying `ResamplerConstructionError`'s `Display` representation.
    #[error("rubato FftFixedIn construction failed: {0}")]
    Construct(#[from] ResamplerConstructionError),
}

/// Returned by [`Resampler48k::process_block`] when rubato refuses to process the current
/// input block (channel-mask mismatch, output buffer too small, etc. — all configuration
/// errors that shouldn't occur with our fixed `input_buf` / `output_buf` shapes, but we
/// surface them rather than panic to keep the engine resilient).
#[derive(Debug, Error)]
pub enum ResamplerProcessError {
    /// Wrapped process error from `rubato::FftFixedIn::process_into_buffer`.
    #[error("rubato process_into_buffer failed: {0}")]
    Process(#[from] ResampleError),
}

/// Pre-allocated FftFixedIn wrapper that converts arbitrary native-rate f32 frames into
/// 48 kHz frames without per-call allocation (RESEARCH §Pattern 2 + D-05).
///
/// ## Lifecycle
/// - Constructed off the audio thread (Plan 01-02b's event loop calls `new` when the
///   selected input device's native rate ≠ 48 kHz).
/// - Owned by the DSP worker thread; `process_block` is called per cpal capture chunk after
///   the capture-pump thread signals new data is available.
/// - `process_block` returns a `&[f32]` borrow into `self.output_buf[0]`; the caller copies
///   the slice into the next ring (typically `InputRing`) before calling `process_block`
///   again. Single-mut-borrow pattern — Rust's borrow checker enforces correctness.
///
/// ## Why FftFixedIn and not FftFixedOut/FftFixedInOut
/// FftFixedIn accepts a FIXED input chunk size and produces a VARYING output count per call
/// (the output count depends on the gcd-based stride). This matches the cpal capture pattern
/// where every callback delivers a fixed number of frames; downstream code (InputRing push)
/// is happy with a varying output count because rtrb's push_entire_slice handles any length.
pub struct Resampler48k {
    /// The wrapped rubato resampler.
    inner: FftFixedIn<f32>,
    /// Pre-allocated input scratch buffer: `Vec<Vec<f32>>` of shape
    /// `[1 channel][chunk_size_in frames]`, filled with zeros by `input_buffer_allocate(true)`.
    /// The first dimension is always 1 (mono engine).
    input_buf: Vec<Vec<f32>>,
    /// Pre-allocated output scratch buffer: `Vec<Vec<f32>>` of shape
    /// `[1 channel][output_frames_max frames]`. Sized by rubato to fit the worst-case output
    /// of any single `process_into_buffer` call, so the per-call op never reallocates.
    output_buf: Vec<Vec<f32>>,
    /// Native input rate preserved so [`SampleRateState`] reporting and any future
    /// diagnostics know what to display.
    native_rate: u32,
}

impl Resampler48k {
    /// Construct a new resampler from `native_rate` to 48 kHz. The internal chunk size is
    /// fixed at [`BLOCK`] (256 frames @ native rate), matching the cpal capture callback's
    /// `BufferSize::Fixed(256)` baseline.
    ///
    /// Pre-allocates both scratch buffers via `input_buffer_allocate(true)` and
    /// `output_buffer_allocate(true)` so `process_block` performs zero allocation per call
    /// (RESEARCH §Pattern 2; D-05 "all scratch buffers pre-allocated").
    pub fn new(native_rate: u32) -> Result<Self, ResamplerBuildError> {
        let inner = FftFixedIn::<f32>::new(
            native_rate as usize,
            SAMPLE_RATE_HZ as usize,
            BLOCK,
            SUB_CHUNKS,
            ENGINE_CHANNELS,
        )?;
        // CRITICAL: pre-allocate BOTH buffers once, off the audio thread. `true` fills with
        // zeros so the first process_into_buffer call sees valid memory.
        let input_buf = inner.input_buffer_allocate(true);
        let output_buf = inner.output_buffer_allocate(true);
        Ok(Self {
            inner,
            input_buf,
            output_buf,
            native_rate,
        })
    }

    /// Process one input chunk of native-rate frames into 48 kHz frames.
    ///
    /// Copies up to [`Self::input_chunk_size`] frames from `native_frames` into the
    /// pre-allocated `input_buf[0]` (slice copy — NO `Vec::push`, NO `Vec::extend`, NO
    /// re-allocation), calls `inner.process_into_buffer(...)`, and returns a borrow into
    /// `output_buf[0]` of length `out_n` (rubato's reported output frame count).
    ///
    /// Returns `Err(ResamplerProcessError::Process(_))` only on configuration errors that
    /// shouldn't happen with our fixed buffer shapes (channel-mask mismatch, etc.).
    ///
    /// ## Borrow semantics
    /// The returned `&[f32]` borrows from `self.output_buf[0]`. The borrow ends when the
    /// caller next invokes `&mut self` on the resampler — i.e. the next `process_block`
    /// call. Single-mut-borrow pattern; the borrow checker enforces correctness.
    pub fn process_block(
        &mut self,
        native_frames: &[f32],
    ) -> Result<&[f32], ResamplerProcessError> {
        let chunk_size = self.input_buf[0].len();
        // Take only as many frames as the input buffer can hold. Excess input frames are
        // dropped here — the caller is responsible for chunking input into chunk_size-sized
        // calls. (rubato also has internal logic that handles partial chunks via the
        // FftFixedIn's `saved_frames` accumulator, so calling with fewer than chunk_size
        // frames is well-defined; we mirror that by writing only as many as the caller
        // provides.)
        let n = native_frames.len().min(chunk_size);
        // Copy in-place into the pre-allocated input scratch buffer. NO allocation.
        self.input_buf[0][..n].copy_from_slice(&native_frames[..n]);
        // Zero any trailing scratch slots so a short input doesn't deliver stale samples to
        // rubato (the resampler still consumes the full chunk_size each call).
        for slot in &mut self.input_buf[0][n..] {
            *slot = 0.0;
        }
        // Zero allocation per docs when both buffers were pre-allocated via *_buffer_allocate.
        let (_in_n, out_n) =
            self.inner
                .process_into_buffer(&self.input_buf, &mut self.output_buf, None)?;
        Ok(&self.output_buf[0][..out_n])
    }

    /// The fixed input chunk size in frames. Callers must feed this many native-rate frames
    /// per `process_block` call for the rubato accumulator to advance optimally.
    pub fn input_chunk_size(&self) -> usize {
        self.input_buf[0].len()
    }

    /// The native sample rate this resampler was constructed for. Used by the event loop to
    /// publish the same value into [`SampleRateState`] for the UI banner.
    pub fn native_rate(&self) -> u32 {
        self.native_rate
    }
}

/// Wait-free atomic publisher of the current sample-rate-mismatch state for the AUDIO-04
/// yellow banner. The atomic stores `native_hz` directly; the value `0` is a sentinel for
/// "no mismatch."
///
/// Cloned via the inner `Arc<AtomicU32>` — Plan 01-02b's event loop writes via
/// `set_mismatch(hz)` / `clear()` when streams are (re)built; Plan 01-05's egui repaint loop
/// reads via `read() -> Option<u32>` each frame.
///
/// ## Test contract
/// Plan 01-05's `app::tests::banner_on_mismatch` named test calls
/// `sample_rate_state.set_mismatch(44100)` then `sample_rate_state.read()` and asserts
/// `Some(44100)`. The `sample_rate_state_round_trip` test below covers the same shape from
/// this crate's perspective; both tests will pass with the same implementation.
#[derive(Clone)]
pub struct SampleRateState(pub Arc<AtomicU32>);

impl SampleRateState {
    /// Construct a fresh `SampleRateState` with no mismatch (inner atomic = 0).
    pub fn new() -> Self {
        Self(Arc::new(AtomicU32::new(0)))
    }

    /// Publish a new mismatch state. `native_hz` MUST be non-zero (0 is the "cleared"
    /// sentinel); the engine will only ever call this with the device's actual native rate,
    /// which is always > 0 in practice. The atomic is overwrite-latest semantics — Relaxed
    /// ordering is sufficient because the UI side already polls every repaint.
    pub fn set_mismatch(&self, native_hz: u32) {
        self.0.store(native_hz, Ordering::Relaxed);
    }

    /// Clear the mismatch state (used when the engine rebuilds streams at a 48 kHz device).
    pub fn clear(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    /// Read the current mismatch state. Returns `Some(native_hz)` if a mismatch is active,
    /// `None` if cleared. Wait-free — the UI repaint loop reads this each frame.
    pub fn read(&self) -> Option<u32> {
        match self.0.load(Ordering::Relaxed) {
            0 => None,
            hz => Some(hz),
        }
    }
}

impl Default for SampleRateState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AUDIO-03: feed N input chunks of 44100-rate data through `Resampler48k`; assert the
    /// cumulative output frame count converges to `total_in * (48000 / 44100)` within the
    /// rubato per-chunk rounding tolerance.
    ///
    /// VALIDATION row pins the test name; CI greps for it. The tolerance below is per-call
    /// rubato chunk-rounding (rubato's `FftFixedIn` produces an integer number of output
    /// chunks per call; the cumulative error stays bounded).
    #[test]
    fn test_44100_to_48000_count_invariant() {
        const NATIVE: u32 = 44100;
        const TARGET: u32 = 48000;
        const N_CALLS: usize = 100; // ~100 chunks * 256 frames * 1/44100 s/frame ≈ 580 ms

        let mut resampler =
            Resampler48k::new(NATIVE).expect("FftFixedIn::new(44100, 48000, 256, 2, 1)");
        let chunk_size = resampler.input_chunk_size();
        assert!(chunk_size > 0, "input chunk size must be > 0");

        // Synthetic input: a 440 Hz sine wave at the native rate. Content is irrelevant for
        // a frame-count check; using a sine ensures the resampler isn't accidentally
        // optimizing for all-zero input.
        let mut input = vec![0f32; chunk_size];
        let mut phase = 0.0f32;
        let phase_step = 2.0 * std::f32::consts::PI * 440.0 / NATIVE as f32;

        let mut total_in: usize = 0;
        let mut total_out: usize = 0;
        for _ in 0..N_CALLS {
            for sample in input.iter_mut() {
                *sample = phase.sin();
                phase += phase_step;
                if phase > 2.0 * std::f32::consts::PI {
                    phase -= 2.0 * std::f32::consts::PI;
                }
            }
            let out = resampler.process_block(&input).expect("process_block");
            total_in += input.len();
            total_out += out.len();
        }

        // Expected: total_out ≈ total_in * (48000 / 44100). rubato's per-chunk rounding
        // accumulates at most ~1 frame of error per call (the FFT chunk boundary); the test
        // tolerance is therefore (N_CALLS as f32) — well above the actual cumulative error.
        let expected = total_in as f32 * TARGET as f32 / NATIVE as f32;
        let actual = total_out as f32;
        let delta = (actual - expected).abs();
        assert!(
            delta < N_CALLS as f32,
            "frame count invariant violated: expected ≈ {expected}, got {actual}, delta {delta}, tolerance < {N_CALLS}"
        );
    }

    /// AUDIO-04 banner-state half: `SampleRateState` round-trips `set_mismatch` -> `read` ->
    /// `clear` -> `read`. Plan 01-05's `app::tests::banner_on_mismatch` named test depends on
    /// this exact API shape; this test pins the contract from this crate.
    #[test]
    fn sample_rate_state_round_trip() {
        let state = SampleRateState::new();
        assert_eq!(state.read(), None, "fresh state must read as no mismatch");

        state.set_mismatch(44100);
        assert_eq!(
            state.read(),
            Some(44100),
            "after set_mismatch(44100), read must return Some(44100)"
        );

        state.set_mismatch(96000);
        assert_eq!(
            state.read(),
            Some(96000),
            "set_mismatch overwrites the previous value (overwrite-latest semantics)"
        );

        state.clear();
        assert_eq!(state.read(), None, "after clear(), read must return None");
    }

    /// Sanity: the banner template includes the literal copy from D-05 + a single `{}`
    /// format slot for the native rate. Plan 01-05's app.rs will use
    /// `format!(RESAMPLE_BANNER_TEMPLATE, hz)`; this test enforces the slot is present and
    /// the copy hasn't drifted.
    #[test]
    fn banner_template_matches_d05() {
        assert!(
            RESAMPLE_BANNER_TEMPLATE.contains("{}"),
            "banner template must include a `{{}}` slot for the native rate"
        );
        assert!(
            RESAMPLE_BANNER_TEMPLATE.contains("Resampling from"),
            "banner template must start with the D-05 verbatim copy"
        );
        assert!(
            RESAMPLE_BANNER_TEMPLATE.contains("48 kHz"),
            "banner template must mention the target 48 kHz rate"
        );
        // Format works: substituting the slot yields a rendered banner containing the rate.
        let s = RESAMPLE_BANNER_TEMPLATE.replace("{}", "44100");
        assert!(s.contains("44100"));
    }
}

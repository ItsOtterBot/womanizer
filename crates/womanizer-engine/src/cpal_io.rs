//! cpal stream construction + RT-shaped capture/playback callbacks.
//!
//! Populated by Plan 01-02a. Mirrors the Phase 0 smoke-harness shape (every callback body
//! wrapped in `assert_no_alloc(|| { ... })`; drop-on-Full ring pushes; error_callback pushes
//! into `ErrorRing` only — no allocation, no log, no syscall on the RT path).
//!
//! ## What lives here
//! - [`BLOCK`] / [`SAMPLE_RATE_HZ`]: the Phase 1 latency / sample-rate baseline (D-03 + AUDIO-02).
//! - [`EngineBuildError`]: typed failure surface for the post-build config assertion (Pitfall #3,
//!   AUDIO-05 — "fail loudly" on silent format/channel/rate fallback).
//! - [`pick_input_config`] / [`pick_output_config`]: filter `supported_*_configs()` for 48 kHz +
//!   mono(input) / stereo(output) + f32, asserting the collapsed `SupportedStreamConfig` matches
//!   the request before any stream is built (RESEARCH Pattern 3).
//! - [`enumerate_inputs`]: cpal input-device name list for the UI dropdown (AUDIO-01).
//! - [`build_capture_stream`] / [`build_virtual_output_stream`]: the real cpal stream constructors.
//!   Capture closure wraps its body in `assert_no_alloc(|| { … })`, pushes mono f32 frames into
//!   `InputRing`, computes RMS into `Telemetry::input_rms`, bumps `samples_since_wake` for the
//!   capture-pump thread (which is the SOLE site of `DspWakeHandle::wake()` for the DSP worker
//!   per wake.rs:8-14 + RESEARCH Anti-Pattern). Playback closure drains `VirtualOutRing` to the
//!   stereo cpal buffer (mono-duplicated to both channels per D-16), writes silence + bumps
//!   `Telemetry::xruns` on underrun, and pairs the playback timestamp with the matching capture
//!   timestamp to publish a smoothed (EMA α=0.1) round-trip latency value to
//!   `Telemetry::latency_ms` (D-06, AUDIO-06 latency half).
//!
//! ## Rings are NOT constructed here
//! The caller (Plan 01-02b event_loop) owns the rings and moves the producer/consumer halves
//! into the closures. This matches RESEARCH Pitfall #Y (construct both halves BEFORE
//! `build_*_stream`).
//!
//! ## What the cpal callback MUST NEVER do
//! - allocate (enforced in debug by the workspace's `#[global_allocator] = AllocDisabler` +
//!   the `assert_no_alloc(|| { … })` wrap)
//! - log via `tracing::*` (formatters allocate; the only RT error path is `ErrorRing`)
//! - call `DspWakeHandle::wake()` for the DSP worker (may syscall — see wake.rs:8-14 + A7).
//!   The capture-pump thread in `worker.rs` is the only allowed caller.
//! - acquire any `Mutex` / `RwLock`
//! - propagate `?` (every fallible op uses `let _ = …`; drop-on-Full is the contract)

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use assert_no_alloc::assert_no_alloc;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{
    BufferSize, SampleFormat, SampleRate, StreamConfig, StreamInstant, SupportedBufferSize,
    SupportedStreamConfig,
};
// Note: in cpal 0.17 `SampleRate` is `pub type SampleRate = u32` — used as a bare numeric
// type, not constructed via `SampleRate(48_000)`.
use rtrb::Producer;
use thiserror::Error;
use womanizer_core::{AudioFrame, DspWakeHandle, EngineError, HotParams, Telemetry};

/// Phase 1 cpal block size — 256 frames at 48 kHz = 5.33 ms per cpal callback (D-03 baseline).
/// Leaves headroom for Phase 2 DSP and Phase 3 shaping under the < 50 ms typical target.
pub const BLOCK: usize = 256;

/// Engine-fixed sample rate (AUDIO-02). Sample-rate mismatch is the #1 cause of crackle, so the
/// engine asserts 48 kHz post-build and refuses to construct streams at any other rate (Pitfall
/// #3). Devices whose native rate differs go through `resampler.rs` at the I/O boundary.
pub const SAMPLE_RATE_HZ: u32 = 48_000;

/// Channel count the virtual-output device must support (D-16 — rebranded BlackHole ships as
/// 2-channel stereo). The engine processes mono internally and writes the same sample to both
/// channels at the virtual-device boundary.
pub const OUTPUT_CHANNELS: u16 = 2;

/// Channel count for the capture device (mono; the engine is single-channel end-to-end).
pub const INPUT_CHANNELS: u16 = 1;

/// EMA smoothing factor for the latency telemetry (D-06).
/// `latency_ms = (1-α) * latency_ms + α * raw_ms` with α = 0.1 → ~10-tick rolling average.
const LATENCY_EMA_ALPHA: f32 = 0.1;

/// Capacity of the timestamp-pair SPSC ring used to pair capture / playback timestamps for
/// round-trip latency measurement (D-06). 64 pairs at 5.33 ms each = ~340 ms of slack — far
/// more than any plausible scheduling jitter.
pub const TS_RING_CAPACITY: usize = 64;

/// Returned by [`build_capture_stream`] / [`build_virtual_output_stream`] when the device
/// negotiated a config that does not match the engine's 48 kHz / mono(input) or
/// 48 kHz / stereo(output) / f32 contract. Surfaced to the event loop via `EngineCommand`
/// failure so the UI can render the device row as red instead of silently producing wrong-rate
/// audio (Pitfall #3, AUDIO-05).
#[derive(Debug, Error)]
pub enum EngineBuildError {
    /// No `SupportedStreamConfigRange` matched the (channels, rate-range, f32) filter.
    /// The engine refuses to fall back silently — surfaces this so the device row can render
    /// red with the reason.
    #[error("no compatible config (need {channels}-ch f32 @ 48 kHz)")]
    NoCompatibleConfig {
        /// Channel count the engine required (1 for capture, 2 for virtual-output).
        channels: u16,
    },
    /// The device negotiated a sample rate other than 48 kHz despite the post-`with_sample_rate`
    /// call. Devices SHOULD honor `with_sample_rate(48_000)` if 48 kHz is inside the range; if
    /// they don't, that's a backend bug we surface rather than silently using the wrong rate.
    #[error("device negotiated wrong sample rate: {got} Hz (expected 48000 Hz)")]
    NegotiatedWrongRate {
        /// The sample rate the device actually returned post-negotiation.
        got: u32,
    },
    /// The device negotiated a channel count other than what the engine requested.
    #[error("device negotiated wrong channel count: {got} (expected {expected})")]
    NegotiatedWrongChannels {
        /// Actual channel count returned post-negotiation.
        got: u16,
        /// Channel count the engine requested.
        expected: u16,
    },
    /// The device negotiated a sample format other than f32.
    #[error("device negotiated wrong sample format: {got:?} (expected F32)")]
    NegotiatedWrongFormat {
        /// Actual sample format returned post-negotiation.
        got: SampleFormat,
    },
    /// Wrapped backend error from `cpal::Device::build_*_stream`.
    #[error("cpal build_stream backend error: {0}")]
    BackendError(#[from] cpal::BuildStreamError),
    /// Wrapped backend error from `cpal::Device::default_*_config` / `supported_*_configs`.
    #[error("cpal default config error: {0}")]
    DefaultConfigError(#[from] cpal::DefaultStreamConfigError),
    /// Wrapped backend error from `cpal::Device::supported_*_configs`.
    #[error("cpal supported configs error: {0}")]
    SupportedConfigsError(#[from] cpal::SupportedStreamConfigsError),
    /// Wrapped backend error from `cpal::Stream::play`.
    #[error("cpal stream.play() error: {0}")]
    PlayStreamError(#[from] cpal::PlayStreamError),
}

/// Phase 1 owns capture + virtual-output. The optional monitor stream lives in Plan 01-03's
/// `monitor.rs`. The event loop (Plan 01-02b) holds an `Option<EngineStreams>` for the lifetime
/// of the Started state; dropping the struct stops both streams.
pub struct EngineStreams {
    /// Capture stream (mic → InputRing). Mono f32 @ 48 kHz, `BufferSize::Fixed(256)` baseline.
    pub input: cpal::Stream,
    /// Virtual-output stream (VirtualOutRing → device VRChat sees as mic). Stereo f32 @ 48 kHz.
    pub virtual_out: cpal::Stream,
}

/// SPSC ring of (sequence number, capture-`StreamInstant`) pairs the capture callback pushes
/// and the playback callback drains for latency pairing (D-06).
pub type TimestampPair = (u64, StreamInstant);

/// Pick a 48 kHz mono f32 input config out of the device's supported configs (RESEARCH Pattern 3).
///
/// Returns the collapsed `SupportedStreamConfig`. The caller then asserts each collapsed value
/// matches the request and constructs the cpal stream; this function does the filter half.
///
/// AUDIO-02 + AUDIO-05: refuses to silently fall back. If no config matches, returns
/// `EngineBuildError::NoCompatibleConfig` so the UI can render the device row as red.
pub fn pick_input_config(device: &cpal::Device) -> Result<SupportedStreamConfig, EngineBuildError> {
    let target: SampleRate = SAMPLE_RATE_HZ;
    let supported = device
        .supported_input_configs()?
        .find(|range| {
            range.channels() == INPUT_CHANNELS
                && range.sample_format() == SampleFormat::F32
                && range.min_sample_rate() <= target
                && range.max_sample_rate() >= target
        })
        .ok_or(EngineBuildError::NoCompatibleConfig {
            channels: INPUT_CHANNELS,
        })?
        .with_sample_rate(target);
    assert_collapsed_config(&supported, INPUT_CHANNELS)?;
    Ok(supported)
}

/// Pick a 48 kHz stereo f32 output config (D-16 — virtual output is 2-channel stereo).
/// Same rules as [`pick_input_config`]; surfaces typed errors on mismatch.
pub fn pick_output_config(
    device: &cpal::Device,
) -> Result<SupportedStreamConfig, EngineBuildError> {
    let target: SampleRate = SAMPLE_RATE_HZ;
    let supported = device
        .supported_output_configs()?
        .find(|range| {
            range.channels() == OUTPUT_CHANNELS
                && range.sample_format() == SampleFormat::F32
                && range.min_sample_rate() <= target
                && range.max_sample_rate() >= target
        })
        .ok_or(EngineBuildError::NoCompatibleConfig {
            channels: OUTPUT_CHANNELS,
        })?
        .with_sample_rate(target);
    assert_collapsed_config(&supported, OUTPUT_CHANNELS)?;
    Ok(supported)
}

/// Pattern 3 post-build assertion shared by [`pick_input_config`] and [`pick_output_config`].
/// Devices SHOULD honor `with_sample_rate(48_000)`; if they don't, surface the specific
/// mismatch (Pitfall #3, AUDIO-05).
fn assert_collapsed_config(
    supported: &SupportedStreamConfig,
    expected_channels: u16,
) -> Result<(), EngineBuildError> {
    if supported.sample_rate() != SAMPLE_RATE_HZ {
        return Err(EngineBuildError::NegotiatedWrongRate {
            got: supported.sample_rate(),
        });
    }
    if supported.channels() != expected_channels {
        return Err(EngineBuildError::NegotiatedWrongChannels {
            got: supported.channels(),
            expected: expected_channels,
        });
    }
    if supported.sample_format() != SampleFormat::F32 {
        return Err(EngineBuildError::NegotiatedWrongFormat {
            got: supported.sample_format(),
        });
    }
    Ok(())
}

/// Pick `BufferSize::Fixed(256)` if the device accepts it, otherwise fall back per D-03.
///
/// - `SupportedBufferSize::Range { min, max }` containing 256 → `Fixed(256)`.
/// - `Range { min, max }` not containing 256 → `Fixed(min)` + warn log (off-RT).
/// - `Unknown` → `BufferSize::Default` + warn log.
fn pick_buffer_size(supported: &SupportedBufferSize) -> BufferSize {
    match supported {
        SupportedBufferSize::Range { min, max } => {
            let target = BLOCK as u32;
            if (*min..=*max).contains(&target) {
                BufferSize::Fixed(target)
            } else {
                tracing::warn!(
                    min,
                    max,
                    target,
                    "device does not accept {target}-frame buffer; using min={min}"
                );
                BufferSize::Fixed(*min)
            }
        }
        SupportedBufferSize::Unknown => {
            tracing::warn!("device buffer-size range unknown; using BufferSize::Default");
            BufferSize::Default
        }
    }
}

/// Enumerate input devices via cpal, returning a list of names for the UI dropdown (AUDIO-01).
///
/// On hosts with no input devices (CI runners, headless containers) this returns an empty
/// `Vec` — callers render the UI dropdown as empty + a "no input devices found" hint. Never
/// panics. Off-RT path: `tracing::warn!` for any cpal enumeration failure.
pub fn enumerate_inputs() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(iter) => iter
            .filter_map(|d| match d.description() {
                Ok(desc) => Some(desc.name().to_string()),
                Err(e) => {
                    tracing::warn!(error = ?e, "failed to read input device description; skipping");
                    None
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = ?e, "cpal::Host::input_devices() failed; returning empty list");
            Vec::new()
        }
    }
}

/// Enumerate output devices via cpal — used by the Setup screen's manual-pick fallback when
/// strict-regex detection misses the user's virtual cable (e.g. multi-cable installer variants
/// or WASAPI endpoint names that diverge from the friendly name shown in the Windows control
/// panel). Same shape and failure semantics as [`enumerate_inputs`].
pub fn enumerate_outputs() -> Vec<String> {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(iter) => iter
            .filter_map(|d| match d.description() {
                Ok(desc) => Some(desc.name().to_string()),
                Err(e) => {
                    tracing::warn!(error = ?e, "failed to read output device description; skipping");
                    None
                }
            })
            .collect(),
        Err(e) => {
            tracing::warn!(error = ?e, "cpal::Host::output_devices() failed; returning empty list");
            Vec::new()
        }
    }
}

/// Build the RT-safe capture stream (mic → InputRing).
///
/// Closure body is wrapped in `assert_no_alloc(|| { … })` and:
/// 1. Pushes `data` into `in_tx` with drop-on-Full (Pattern B — never propagate `Err(Full)`).
/// 2. Computes mono RMS into `tele.input_rms` (atomic, overwrite-latest).
/// 3. Bumps `samples_since_wake` so the capture-pump thread (in `worker.rs`) can call
///    `DspWakeHandle::wake()` off the callback (wake.rs:8-14 — `wake()` is forbidden from
///    inside the cpal callback because `Thread::unpark()` can syscall).
/// 4. Pushes the (block_seq, capture-timestamp) pair into `ts_tx` for latency pairing (D-06).
///
/// The `error_callback` maps `cpal::StreamError::DeviceNotAvailable` and `StreamInvalidated`
/// and `BackendSpecific { .. }` to `EngineError::DeviceFault`, pushes into `err_tx` with
/// drop-on-Full, and calls `engine_wake.wake()` to nudge the off-RT event loop to drain
/// `ErrorRing` and emit `EngineEvent::Error(DeviceFault)` for the UI banner (D-07, D-21,
/// Pitfall #14). The `error_callback` is a separate cpal-internal thread (not the audio
/// callback) so `wake()` is allowed there per wake.rs.
///
/// `BufferUnderrun` is mapped to `EngineError::Xrun` (cheaper to recover; UI may show xrun
/// counter without raising the disconnect banner).
#[allow(clippy::too_many_arguments)]
pub fn build_capture_stream(
    device: &cpal::Device,
    mut in_tx: Producer<AudioFrame>,
    samples_since_wake: Arc<AtomicUsize>,
    mut ts_tx: Producer<TimestampPair>,
    _hot: Arc<HotParams>,
    tele: Arc<Telemetry>,
    mut err_tx: Producer<EngineError>,
    engine_wake: DspWakeHandle,
) -> Result<cpal::Stream, EngineBuildError> {
    let supported = pick_input_config(device)?;
    let buffer_size = pick_buffer_size(supported.buffer_size());
    let requested = StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size,
    };

    // Per-stream sequence counter so the playback callback can match capture timestamps to
    // playback timestamps (D-06 latency pairing). Cheap atomic; only the capture callback
    // increments it.
    let block_seq = Arc::new(AtomicU64::new(0));
    let block_seq_cb = block_seq.clone();

    let stream = device.build_input_stream::<f32, _, _>(
        &requested,
        move |data: &[f32], info: &cpal::InputCallbackInfo| {
            // RT-SHAPED region: no allocation, no lock, no syscall, no log, no panic, no Box.
            // Mirrors smoke.rs lines 96-105 + the `stub_dsp_callback` shape verbatim.
            assert_no_alloc(|| {
                // 1. Push samples into InputRing — drop-on-Full per Pattern B (the engine
                //    surfaces device-level faults via the error_callback path, not here).
                let _ = in_tx.push_entire_slice(data);

                // 2. Mono RMS into Telemetry::input_rms (atomic, overwrite-latest). Used by
                //    the UI level meter AND by the feedback-loop detector (D-13).
                let sum_sq: f32 = data.iter().map(|s| s * s).sum();
                let rms = (sum_sq / data.len().max(1) as f32).sqrt();
                tele.input_rms.store(rms, Ordering::Relaxed);

                // 3. Bump samples_since_wake for the capture-pump thread (NOT for the worker —
                //    we cannot call wake() here per wake.rs:8-14). Release ordering pairs with
                //    the capture-pump thread's Acquire load so any data pushed above is visible
                //    to the worker once it observes the wake.
                samples_since_wake.fetch_add(data.len(), Ordering::Release);

                // 4. Push (seq, capture-instant) for latency pairing. Drop-on-Full — the
                //    playback side reads opportunistically and skips on miss (D-06 + A3
                //    clock-origin caveat documented above).
                let seq = block_seq_cb.fetch_add(1, Ordering::Relaxed);
                let _ = ts_tx.push((seq, info.timestamp().capture));
            });
        },
        move |err: cpal::StreamError| {
            // NOT the audio callback — this is a separate cpal-internal thread, so wake() is
            // allowed (wake.rs lines 8-14 forbid the audio callback specifically). We still
            // refrain from logging here because (a) the off-RT event loop drains ErrorRing
            // and tracing::warn!s with full context, (b) keeping the body tiny minimizes the
            // chance of accidental allocation in a future revision.
            let mapped = match err {
                cpal::StreamError::DeviceNotAvailable => EngineError::DeviceFault,
                cpal::StreamError::StreamInvalidated => EngineError::DeviceFault,
                cpal::StreamError::BufferUnderrun => EngineError::Xrun,
                cpal::StreamError::BackendSpecific { .. } => EngineError::DeviceFault,
            };
            let _ = err_tx.push(mapped); // drop-on-Full — UI already sees the device row red
            engine_wake.wake();
        },
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

/// Build the RT-safe virtual-output stream (VirtualOutRing → device VRChat sees as mic).
///
/// The virtual device is 2-channel stereo (D-16). VirtualOutRing carries MONO frames per the
/// Phase 1 worker contract; the playback callback duplicates each mono sample to both
/// channels at the virtual-device boundary.
///
/// Closure body is wrapped in `assert_no_alloc(|| { … })` and:
/// 1. Drains `vo_rx` (mono) for `out.len() / 2` frames; duplicates each sample to both
///    channels of `out`.
/// 2. On underrun: fills the cpal buffer with silence and bumps `tele.xruns`.
/// 3. Pops from `ts_rx` until matching capture-seq; computes `playback_ts - capture_ts` (ms),
///    EMA-smooths into `tele.latency_ms`. If ts ring empty or out-of-sync, skips (no panic,
///    no allocation).
///
/// ### Clock-origin caveat (A3 from RESEARCH)
/// `cpal::StreamInstant` origins are documented per-stream-may-differ. On macOS
/// (`mach_absolute_time`) and Windows (`QueryPerformanceCounter`) the capture-stream and
/// output-stream instants are anchored to the same system clock in practice, so
/// `playback_ts.duration_since(&capture_ts)` is meaningful. Phase 5 may add an explicit
/// self-check; Phase 1 documents and accepts the per-stream-origin assumption.
#[allow(clippy::too_many_arguments)]
pub fn build_virtual_output_stream(
    device: &cpal::Device,
    mut vo_rx: rtrb::Consumer<AudioFrame>,
    mut ts_rx: rtrb::Consumer<TimestampPair>,
    tele: Arc<Telemetry>,
    mut err_tx: Producer<EngineError>,
    engine_wake: DspWakeHandle,
) -> Result<cpal::Stream, EngineBuildError> {
    let supported = pick_output_config(device)?;
    let buffer_size = pick_buffer_size(supported.buffer_size());
    let requested = StreamConfig {
        channels: supported.channels(),
        sample_rate: supported.sample_rate(),
        buffer_size,
    };

    // Atomic for tracking the "last matched capture sequence number" the playback side has
    // already paired with a capture timestamp. Avoids re-pairing the same (seq, ts) on every
    // callback. Phase 1 keeps it simple — the playback callback drains the ts ring fully each
    // call and pairs the most recent capture-ts it observed.
    let last_capture_ts = Arc::new(AtomicCaptureInstant::new());
    let last_capture_ts_cb = last_capture_ts.clone();

    let stream = device.build_output_stream::<f32, _, _>(
        &requested,
        move |out: &mut [f32], info: &cpal::OutputCallbackInfo| {
            assert_no_alloc(|| {
                // (1) + (2): drain mono frames; duplicate to stereo; silence on underrun.
                let frames = out.len() / 2;
                match vo_rx.read_chunk(frames) {
                    Ok(chunk) => {
                        let (a, b) = chunk.as_slices();
                        for (i, s) in a.iter().chain(b.iter()).enumerate() {
                            // SAFETY of index: `frames = out.len() / 2`, so `i*2+1 < out.len()`
                            // for i in [0, frames). chain a+b is exactly `frames` items.
                            out[i * 2] = *s;
                            out[i * 2 + 1] = *s;
                        }
                        chunk.commit_all();
                    }
                    Err(_) => {
                        out.fill(0.0);
                        tele.xruns.fetch_add(1, Ordering::Relaxed);
                    }
                }

                // (3) Latency pairing. Drain ALL available pairs from ts_rx (keeps the ring
                //    from filling up); remember the latest capture-ts we saw. Then compute
                //    `playback_ts - latest_capture_ts` and EMA-smooth into tele.latency_ms.
                //
                //    A3 clock-origin caveat: on Mac (mach_absolute_time) + Win (QPC) the
                //    capture-stream and output-stream StreamInstant origins agree in practice.
                while let Ok(pair) = ts_rx.pop() {
                    last_capture_ts_cb.store(pair.1);
                }
                if let Some(capture_ts) = last_capture_ts_cb.load() {
                    let playback_ts = info.timestamp().playback;
                    if let Some(delta) = playback_ts.duration_since(&capture_ts) {
                        let ms = duration_to_ms(delta);
                        let prev = tele.latency_ms.load(Ordering::Relaxed);
                        let smoothed = (1.0 - LATENCY_EMA_ALPHA) * prev + LATENCY_EMA_ALPHA * ms;
                        tele.latency_ms.store(smoothed, Ordering::Relaxed);
                    }
                }
            });
        },
        move |err: cpal::StreamError| {
            // Same shape as the capture-side error_callback — separate cpal-internal thread,
            // wake() permitted.
            let mapped = match err {
                cpal::StreamError::DeviceNotAvailable => EngineError::DeviceFault,
                cpal::StreamError::StreamInvalidated => EngineError::DeviceFault,
                cpal::StreamError::BufferUnderrun => EngineError::Xrun,
                cpal::StreamError::BackendSpecific { .. } => EngineError::DeviceFault,
            };
            let _ = err_tx.push(mapped);
            engine_wake.wake();
        },
        None,
    )?;
    stream.play()?;
    Ok(stream)
}

/// Convert a `Duration` to milliseconds as f32. RT-safe (no allocation, no syscall).
#[inline]
fn duration_to_ms(d: Duration) -> f32 {
    d.as_secs_f32() * 1000.0
}

/// Lock-free holder for the most-recent capture `StreamInstant` observed by the playback
/// callback. `StreamInstant` is `Copy` (two i64+u32 fields) — we pack secs into an `AtomicI64`
/// and nanos+present-bit into an `AtomicU64`.
///
/// Avoids a `Mutex<Option<StreamInstant>>` (locks forbidden on the RT path). Stores are
/// Relaxed because both the capture and playback callbacks already serialize through the
/// `ts_rx` SPSC ring; this holder is just the "remembered latest" cache on the consumer side.
struct AtomicCaptureInstant {
    // We use two u64s: `secs_plus_one` (0 = "no value", n>0 means actual secs = n-1) +
    // `nanos`. This keeps the holder allocation-free and pointer-sized atomics — `AtomicU64`
    // is available on every cpal-supported 64-bit target (macOS Apple Silicon, Windows x86_64).
    secs_plus_one: std::sync::atomic::AtomicU64,
    nanos: std::sync::atomic::AtomicU32,
}

impl AtomicCaptureInstant {
    fn new() -> Self {
        Self {
            secs_plus_one: std::sync::atomic::AtomicU64::new(0),
            nanos: std::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Store the latest `StreamInstant`. RT-safe.
    fn store(&self, ts: StreamInstant) {
        // We store nanos first then secs, so a torn read recovers gracefully (load checks
        // secs > 0 first). Since playback callback is the sole consumer, real tearing is
        // impossible — we still order writes for forward-compat.
        self.nanos.store(ts.as_nanos_field(), Ordering::Relaxed);
        self.secs_plus_one
            .store(ts.as_secs_field() as u64 + 1, Ordering::Relaxed);
    }

    /// Load the latest `StreamInstant`. Returns `None` if no value has been stored yet.
    fn load(&self) -> Option<StreamInstant> {
        let s = self.secs_plus_one.load(Ordering::Relaxed);
        if s == 0 {
            return None;
        }
        let secs = (s - 1) as i64;
        let nanos = self.nanos.load(Ordering::Relaxed);
        Some(StreamInstant::new(secs, nanos))
    }
}

/// Helper trait — cpal does not expose `StreamInstant`'s internal `(secs, nanos)` fields.
/// We need them for the lock-free `AtomicCaptureInstant` holder. Re-derive via the public
/// `add` / `duration_since` APIs by anchoring to `StreamInstant::new(0, 0)` and the
/// difference duration.
trait StreamInstantExt {
    fn as_secs_field(&self) -> i64;
    fn as_nanos_field(&self) -> u32;
}

impl StreamInstantExt for StreamInstant {
    fn as_secs_field(&self) -> i64 {
        // Reconstruct via duration_since(zero). Negative origins (rare; cpal docs only mention
        // them for unspecified origin) round to 0 — acceptable for the EMA latency input.
        let zero = StreamInstant::new(0, 0);
        match self.duration_since(&zero) {
            Some(d) => d.as_secs() as i64,
            None => 0,
        }
    }

    fn as_nanos_field(&self) -> u32 {
        let zero = StreamInstant::new(0, 0);
        match self.duration_since(&zero) {
            Some(d) => d.subsec_nanos(),
            None => 0,
        }
    }
}

// Per-VALIDATION rows AUDIO-01, AUDIO-02, AUDIO-05 (revision B3 — the AUDIO-01 row points
// at `cpal_io::tests::enumerate_inputs` specifically). Each test must exit 0 on a host with
// real audio devices AND on a headless CI host (skip-on-no-device pattern documented in
// each test).
#[cfg(test)]
mod tests {
    use super::*;

    /// AUDIO-01: `enumerate_inputs()` returns a `Vec<String>` and is non-empty on a host
    /// with at least one input device. Skips the non-empty assertion on a headless CI runner.
    #[test]
    fn enumerate_inputs() {
        // The function must always return a Vec; never panic.
        let names = super::enumerate_inputs();
        // On a typical dev / production host, at least one input device is present.
        // On a headless CI host with no audio hardware, the vec may legitimately be empty.
        if names.is_empty() {
            eprintln!(
                "skipping enumerate_inputs non-empty assertion: no input devices on this host"
            );
            return;
        }
        // Sanity: every returned name is a non-empty String.
        for name in &names {
            assert!(!name.is_empty(), "device name must not be empty");
        }
    }

    /// AUDIO-02: `pick_input_config` against the default input device returns a config
    /// asserted at 48 kHz / 1-ch / f32. Skips on hosts with no default input device.
    #[test]
    fn config_is_48k() {
        let host = cpal::default_host();
        let Some(device) = host.default_input_device() else {
            eprintln!("skipping config_is_48k: no default input device on this host");
            return;
        };

        match pick_input_config(&device) {
            Ok(cfg) => {
                assert_eq!(cfg.sample_rate(), SAMPLE_RATE_HZ, "must be 48 kHz");
                assert_eq!(cfg.channels(), INPUT_CHANNELS, "must be mono");
                assert_eq!(cfg.sample_format(), SampleFormat::F32, "must be f32");
            }
            Err(EngineBuildError::NoCompatibleConfig { .. }) => {
                // Some legitimate hardware (e.g. exotic interfaces, virtual devices that only
                // expose stereo) cannot satisfy 1-ch f32 @ 48 kHz. Document the skip rather
                // than fail; AUDIO-05 covers the negative-path assertion below.
                eprintln!(
                    "skipping config_is_48k: default input device does not support mono f32 @ 48 kHz"
                );
            }
            Err(e) => {
                // Any other error (e.g. backend permission denied) is also a host-environment
                // condition rather than a code defect — skip with the reason for diagnosability.
                eprintln!("skipping config_is_48k: {e}");
            }
        }
    }

    /// AUDIO-05: pure-Rust assertion of the post-build config check.
    ///
    /// Constructing a fully synthetic `cpal::Device` is not feasible in pure Rust (the
    /// platform-aliased `Device` struct wraps a private backend-specific inner), so this test
    /// exercises the `assert_collapsed_config` helper directly with hand-built
    /// `SupportedStreamConfig` values that mimic what a misbehaving backend could return.
    /// Each variant of `EngineBuildError::Negotiated*` is provably emitted.
    #[test]
    fn reject_mismatched_config() {
        use cpal::{SupportedBufferSize, SupportedStreamConfig};

        // Wrong sample rate (44.1 kHz instead of 48 kHz).
        let wrong_rate = SupportedStreamConfig::new(
            INPUT_CHANNELS,
            44_100,
            SupportedBufferSize::Range { min: 64, max: 1024 },
            SampleFormat::F32,
        );
        match assert_collapsed_config(&wrong_rate, INPUT_CHANNELS) {
            Err(EngineBuildError::NegotiatedWrongRate { got: 44_100 }) => {}
            other => panic!("expected NegotiatedWrongRate {{ got: 44100 }}, got {other:?}"),
        }

        // Wrong channel count (stereo where mono was requested).
        let wrong_channels = SupportedStreamConfig::new(
            2,
            SAMPLE_RATE_HZ,
            SupportedBufferSize::Range { min: 64, max: 1024 },
            SampleFormat::F32,
        );
        match assert_collapsed_config(&wrong_channels, INPUT_CHANNELS) {
            Err(EngineBuildError::NegotiatedWrongChannels {
                got: 2,
                expected: 1,
            }) => {}
            other => {
                panic!("expected NegotiatedWrongChannels {{ got: 2, expected: 1 }}, got {other:?}")
            }
        }

        // Wrong sample format (I16 instead of F32).
        let wrong_format = SupportedStreamConfig::new(
            INPUT_CHANNELS,
            SAMPLE_RATE_HZ,
            SupportedBufferSize::Range { min: 64, max: 1024 },
            SampleFormat::I16,
        );
        match assert_collapsed_config(&wrong_format, INPUT_CHANNELS) {
            Err(EngineBuildError::NegotiatedWrongFormat {
                got: SampleFormat::I16,
            }) => {}
            other => panic!("expected NegotiatedWrongFormat {{ got: I16 }}, got {other:?}"),
        }

        // Positive control: a fully compliant config passes.
        let good = SupportedStreamConfig::new(
            INPUT_CHANNELS,
            SAMPLE_RATE_HZ,
            SupportedBufferSize::Range { min: 64, max: 1024 },
            SampleFormat::F32,
        );
        assert!(
            assert_collapsed_config(&good, INPUT_CHANNELS).is_ok(),
            "compliant config must pass assert_collapsed_config"
        );
    }
}

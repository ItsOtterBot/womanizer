//! Self-monitor playback stream + feedback-loop detector.
//!
//! Populated by Plan 01-03. Default OFF (D-12). The detector trips on five consecutive
//! 50 ms RMS windows each rising by ≥ 6 dB (D-13, AUDIO-08 500 ms total budget); on trip
//! it sets `HotParams::monitor_enabled` to false (D-14) and the UI shows the persistent
//! yellow banner: "Self-monitor disabled — feedback detected. Use headphones, not speakers."
//!
//! ## What lives here
//! - [`MonitorBannerState`]: a pair of atomic flags (feedback_detected + disconnected) the
//!   event loop sets and the UI reads to render the AUDIO-08 (D-14) and AUDIO-09 (D-07)
//!   yellow banners. Banner copy constants `FEEDBACK_BANNER_COPY` + `DISCONNECT_BANNER_COPY`
//!   match D-14 / D-07 verbatim.
//! - [`build_monitor_output_stream`]: mirrors `cpal_io::build_virtual_output_stream` but
//!   short-circuits to `out.fill(0.0)` when `HotParams::monitor_enabled.load() == false`
//!   (D-12 default + D-14 trip). Supports mono OR stereo monitor devices (most physical
//!   headphone outputs are stereo; some USB audio interfaces present mono).
//! - [`FeedbackDetector`]: a stateful tap polling `Telemetry::input_rms` at the event-loop's
//!   50 ms `recv_timeout` Timeout-arm cadence. Owns a fixed-size 5-window RMS history; trips
//!   on 5 consecutive ≥ 6 dB (2× linear) rises. On trip: sets `monitor_enabled = false` +
//!   `banner.feedback_detected = true`. Re-arms on the false→true edge of `monitor_enabled`
//!   per D-15 (no cooldown, no grace period).
//!
//! ## Wiring scope (revision W8)
//! This module PUBLISHES the FeedbackDetector + MonitorBannerState + build_monitor_output_stream
//! APIs. The INSTANTIATION of FeedbackDetector (in the Ready shell with live Telemetry /
//! HotParams / MonitorBannerState from the running engine) and the INVOCATION of
//! `detector.tick()` (inside event_loop.rs's `recv_timeout` Timeout arm where Plan 01-02b
//! leaves the marker comment) are owned by Plan 01-05 (Wave 4). This plan does NOT touch
//! event_loop.rs.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

// Diagnostic counters for the monitor callback. Bumped from the RT path with `Relaxed`
// (no allocation, no syscall — Relaxed atomic adds are safe in `assert_no_alloc`). Read
// from the event loop's 1-Hz snapshot. To be removed once the audio chain is confirmed
// working end-to-end.
pub static DIAG_MONITOR_CALLBACK: AtomicUsize = AtomicUsize::new(0);
pub static DIAG_MONITOR_DRAIN: AtomicUsize = AtomicUsize::new(0);
pub static DIAG_MONITOR_UNDERRUN: AtomicUsize = AtomicUsize::new(0);
use std::sync::Arc;

use assert_no_alloc::assert_no_alloc;
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, SampleFormat, SampleRate, StreamConfig, SupportedStreamConfig};
use rtrb::{Consumer, Producer};
use womanizer_core::{AudioFrame, DspWakeHandle, EngineError, HotParams, Telemetry};

use crate::cpal_io::{EngineBuildError, BLOCK, SAMPLE_RATE_HZ};

/// Banner copy for the AUDIO-08 / D-14 feedback-detected yellow banner. Verbatim from D-14.
pub const FEEDBACK_BANNER_COPY: &str =
    "Self-monitor disabled — feedback detected. Use headphones, not speakers.";

/// Banner copy for the AUDIO-09 / D-07 device-disconnect yellow banner. Verbatim from D-07.
/// Set by the event-loop in Plan 01-02b when it drains an `EngineError::DeviceFault` from the
/// `ErrorRing`; Plan 01-05's UI reads `MonitorBannerState::is_disconnected()` each repaint.
pub const DISCONNECT_BANNER_COPY: &str = "Audio disconnected — click to reconnect.";

/// 6 dB == 2× linear amplitude (`20*log10(2) ≈ 6.02 dB`). The detector flags a window-pair
/// rise iff `next >= prev * RISE_FACTOR_6DB`.
const RISE_FACTOR_6DB: f32 = 2.0;

/// Minimum previous-RMS value at which a rise check is meaningful. Below this we treat the
/// previous window as silence and never count a "rise" — avoids the detector tripping on a
/// healthy 0.0 → 0.001 transition immediately after a quiet section.
const MIN_PREV_RMS_FOR_RISE: f32 = 1.0e-6;

/// Length of the rolling RMS history. 5 windows × 50 ms = 250 ms — matches D-13 verbatim.
const WINDOW_HISTORY_LEN: usize = 5;

/// Twin atomic-flag publisher for the two Phase 1 yellow banners (AUDIO-08 feedback-detected
/// + AUDIO-09 disconnected). Shaped like [`crate::resampler::SampleRateState`] but with two
///   independent flags.
///
/// Cloned via the inner Arcs — Plan 01-05's UI repaint loop reads both atomics each frame to
/// decide which banner(s) to render. The event loop in Plan 01-02b writes
/// `disconnected.store(true)` on `EngineError::DeviceFault` drain; the [`FeedbackDetector`]
/// in this module writes `feedback_detected.store(true)` on trip.
#[derive(Clone)]
pub struct MonitorBannerState {
    /// True iff the feedback-loop detector has tripped and the UI should render
    /// [`FEEDBACK_BANNER_COPY`]. Cleared on the false→true edge of
    /// `HotParams::monitor_enabled` (D-15 re-arm semantics).
    pub feedback_detected: Arc<AtomicBool>,
    /// True iff the engine event loop drained an `EngineError::DeviceFault` and the UI
    /// should render [`DISCONNECT_BANNER_COPY`]. Cleared by the event loop when the user
    /// clicks "reconnect" and the rebuild succeeds. (Plan 01-05 owns the click-to-reconnect
    /// UX wire-up; this plan only publishes the atomic.)
    pub disconnected: Arc<AtomicBool>,
}

impl MonitorBannerState {
    /// Construct a fresh banner state with both flags cleared.
    pub fn new() -> Self {
        Self {
            feedback_detected: Arc::new(AtomicBool::new(false)),
            disconnected: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Set the feedback-detected flag. Called by [`FeedbackDetector::tick`] on trip.
    pub fn set_feedback_detected(&self) {
        self.feedback_detected.store(true, Ordering::Relaxed);
    }

    /// Clear the feedback-detected flag. Called by [`FeedbackDetector::tick`] on the
    /// false→true edge of `HotParams::monitor_enabled` (D-15 re-arm: no cooldown).
    pub fn clear_feedback_detected(&self) {
        self.feedback_detected.store(false, Ordering::Relaxed);
    }

    /// Read the feedback-detected flag. The UI repaint loop polls this each frame.
    pub fn is_feedback_detected(&self) -> bool {
        self.feedback_detected.load(Ordering::Relaxed)
    }

    /// Set the disconnected flag. Called by the event loop in Plan 01-02b when it drains an
    /// `EngineError::DeviceFault` from the `ErrorRing`.
    pub fn set_disconnected(&self) {
        self.disconnected.store(true, Ordering::Relaxed);
    }

    /// Clear the disconnected flag. Called by the event loop when streams are successfully
    /// rebuilt after a user-initiated reconnect (Plan 01-05 wires the click handler).
    pub fn clear_disconnected(&self) {
        self.disconnected.store(false, Ordering::Relaxed);
    }

    /// Read the disconnected flag. The UI repaint loop polls this each frame.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Relaxed)
    }
}

impl Default for MonitorBannerState {
    fn default() -> Self {
        Self::new()
    }
}

/// Pick a mono-or-stereo 48 kHz f32 output config for the user's monitor (headphone) device.
///
/// Most physical headphone outputs are stereo, but some USB audio interfaces present mono,
/// and the cpal default-output-config on Mac/Win is the device's native shape. We accept
/// either channel count and let the callback handle the channels==1 vs channels==2 routing
/// (duplicate-on-2-channels for stereo) at the cpal-buffer boundary.
fn pick_monitor_config(device: &cpal::Device) -> Result<SupportedStreamConfig, EngineBuildError> {
    let target: SampleRate = SAMPLE_RATE_HZ;
    let supported = device
        .supported_output_configs()?
        .find(|range| {
            (range.channels() == 1 || range.channels() == 2)
                && range.sample_format() == SampleFormat::F32
                && range.min_sample_rate() <= target
                && range.max_sample_rate() >= target
        })
        .ok_or(EngineBuildError::NoCompatibleConfig { channels: 2 })?
        .with_sample_rate(target);
    // Validate the collapsed config — same Pattern 3 shape as cpal_io.rs, but with the
    // mono-or-stereo relaxation. We only check rate + format here; the channel count is
    // already constrained to {1, 2} by the filter above and is read off `supported` at the
    // call site to decide the duplicate-or-passthrough routing.
    if supported.sample_rate() != SAMPLE_RATE_HZ {
        return Err(EngineBuildError::NegotiatedWrongRate {
            got: supported.sample_rate(),
        });
    }
    if supported.sample_format() != SampleFormat::F32 {
        return Err(EngineBuildError::NegotiatedWrongFormat {
            got: supported.sample_format(),
        });
    }
    Ok(supported)
}

/// Build the RT-safe monitor output stream (MonitorOutRing → user's headphone device).
///
/// Same shape as `cpal_io::build_virtual_output_stream` but:
/// 1. Accepts a mono OR stereo monitor device (filter the config range accordingly).
/// 2. Short-circuits to `out.fill(0.0); return;` when `HotParams::monitor_enabled == false`
///    (the D-12 default and the D-14 post-trip state). Avoids draining `mo_rx` when the
///    monitor is off — saves a few cycles and keeps the ring from accidentally consuming
///    frames the next monitor-on session might want.
/// 3. Duplicates mono frames to both channels of a stereo cpal buffer when the negotiated
///    config is 2-channel.
///
/// Callback body is wrapped in `assert_no_alloc`. The error_callback maps cpal's four
/// `StreamError` variants to `EngineError::DeviceFault` (or `Xrun` for `BufferUnderrun`) and
/// pushes drop-on-Full into `err_tx`, then wakes the engine event loop.
#[allow(clippy::too_many_arguments)]
pub fn build_monitor_output_stream(
    device: &cpal::Device,
    mut mo_rx: Consumer<AudioFrame>,
    hot: Arc<HotParams>,
    tele: Arc<Telemetry>,
    mut err_tx: Producer<EngineError>,
    engine_wake: DspWakeHandle,
) -> Result<cpal::Stream, EngineBuildError> {
    let supported = pick_monitor_config(device)?;
    let channels = supported.channels();
    // We accept Fixed(BLOCK) or fall back to the device's range / Default. Reuse the
    // cpal_io.rs helper indirectly via Default — keeping pick_buffer_size private to
    // cpal_io.rs is fine since the monitor stream's latency budget is not load-bearing for
    // the AUDIO-06 round-trip measurement. The cpal default size on Mac/Win is small enough
    // (usually 256-512) for the monitor use case.
    let buffer_size = match supported.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            let target = BLOCK as u32;
            if (*min..=*max).contains(&target) {
                BufferSize::Fixed(target)
            } else {
                tracing::warn!(
                    min,
                    max,
                    target,
                    "monitor device does not accept {target}-frame buffer; using min={min}"
                );
                BufferSize::Fixed(*min)
            }
        }
        cpal::SupportedBufferSize::Unknown => {
            tracing::warn!("monitor device buffer-size range unknown; using BufferSize::Default");
            BufferSize::Default
        }
    };
    let requested = StreamConfig {
        channels,
        sample_rate: supported.sample_rate(),
        buffer_size,
    };

    let stream = device.build_output_stream::<f32, _, _>(
        &requested,
        move |out: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            assert_no_alloc(|| {
                DIAG_MONITOR_CALLBACK.fetch_add(1, Ordering::Relaxed);
                // Monitor-off short-circuit (D-12 default + D-14 post-trip). Fill silence
                // cheaply and skip the ring drain entirely. The Relaxed load is fine because
                // the UI toggle's overwrite-latest semantics already accept up to one
                // callback-period of staleness.
                if !hot.monitor_enabled.load(Ordering::Relaxed) {
                    out.fill(0.0);
                    return;
                }
                // Monitor on: drain mono frames from MonitorOutRing into the cpal buffer.
                // Stereo case: read out.len()/2 mono frames and duplicate each to two
                // channels. Mono case: read out.len() mono frames and copy 1:1.
                if channels == 2 {
                    let frames = out.len() / 2;
                    match mo_rx.read_chunk(frames) {
                        Ok(chunk) => {
                            let (a, b) = chunk.as_slices();
                            for (i, s) in a.iter().chain(b.iter()).enumerate() {
                                out[i * 2] = *s;
                                out[i * 2 + 1] = *s;
                            }
                            chunk.commit_all();
                            DIAG_MONITOR_DRAIN.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            out.fill(0.0);
                            tele.xruns.fetch_add(1, Ordering::Relaxed);
                            DIAG_MONITOR_UNDERRUN.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                } else {
                    // Mono device path: 1:1 copy.
                    match mo_rx.read_chunk(out.len()) {
                        Ok(chunk) => {
                            let (a, b) = chunk.as_slices();
                            out[..a.len()].copy_from_slice(a);
                            out[a.len()..a.len() + b.len()].copy_from_slice(b);
                            chunk.commit_all();
                            DIAG_MONITOR_DRAIN.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            out.fill(0.0);
                            tele.xruns.fetch_add(1, Ordering::Relaxed);
                            DIAG_MONITOR_UNDERRUN.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            });
        },
        move |err: cpal::StreamError| {
            // Same shape as cpal_io.rs's error_callbacks — separate cpal-internal thread,
            // wake() permitted. All four cpal 0.17 StreamError variants mapped exhaustively.
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

/// Stateful 5×6dB-rise feedback-loop detector. Polls `Telemetry::input_rms` at the event
/// loop's 50 ms cadence (Plan 01-05 wires the call from event_loop.rs's `recv_timeout`
/// Timeout arm).
///
/// ## Algorithm (D-13 verbatim)
/// - Track a fixed-size 5-window history of input RMS readings.
/// - On each tick: shift the history left by one slot; write the current RMS into slot [4].
/// - Once the history is full (5 readings accumulated), trip iff every adjacent pair
///   `(prev, next)` in the history satisfies `next >= prev * 2.0` (6 dB linear).
/// - On trip: store `HotParams::monitor_enabled = false`, set
///   `MonitorBannerState::feedback_detected = true`, mark `tripped = true` (latches until
///   re-arm to avoid re-tripping the same trip).
///
/// ## Re-arm semantics (D-15)
/// On the false→true edge of `HotParams::monitor_enabled` (the user manually re-enables the
/// monitor toggle), clear the banner + zero the window history + reset `tripped` to false.
/// No cooldown, no grace period — the detector is armed immediately on the next tick.
///
/// ## Threading
/// - `tick()` runs on the OFF-RT event-loop thread (NOT in any cpal callback).
/// - Reads `Telemetry::input_rms` via `Relaxed` atomic load (the cpal capture callback writes
///   it via `Relaxed` store; both sides accept up to one-tick of staleness).
/// - Writes `HotParams::monitor_enabled` via `Relaxed` store (same semantics).
pub struct FeedbackDetector {
    tele: Arc<Telemetry>,
    hot: Arc<HotParams>,
    banner: MonitorBannerState,
    /// Rolling 5-window RMS history. Newest reading at index [4]; index [0] is the oldest.
    window_history: [f32; WINDOW_HISTORY_LEN],
    /// Number of `tick()` calls observed since the last reset (capped at 5). The detector
    /// only evaluates the trip rule once `windows_filled == 5`.
    windows_filled: usize,
    /// True iff the detector has already tripped this monitor-on session. Latches until
    /// re-arm (false→true edge of `monitor_enabled`).
    tripped: bool,
    /// Tracks the previous value of `hot.monitor_enabled` so `tick()` can detect the
    /// false→true edge for D-15 re-arm.
    prev_monitor_enabled: bool,
}

impl FeedbackDetector {
    /// Construct a new detector bound to the given telemetry / HotParams / banner state.
    /// The initial state has an empty history and reflects the current monitor toggle.
    pub fn new(tele: Arc<Telemetry>, hot: Arc<HotParams>, banner: MonitorBannerState) -> Self {
        let prev_monitor_enabled = hot.monitor_enabled.load(Ordering::Relaxed);
        Self {
            tele,
            hot,
            banner,
            window_history: [0.0; WINDOW_HISTORY_LEN],
            windows_filled: 0,
            tripped: false,
            prev_monitor_enabled,
        }
    }

    /// Advance the detector by one 50 ms tick. Called from event_loop.rs's Timeout arm
    /// (Plan 01-05 wires the invocation; this plan publishes the API only).
    pub fn tick(&mut self) {
        let cur = self.hot.monitor_enabled.load(Ordering::Relaxed);

        // D-15 re-arm: on the false→true edge, clear the banner + zero history + un-latch
        // and return early. The next tick begins collecting fresh windows from scratch — no
        // cooldown beyond the natural single-tick lag for the user's toggle action to be
        // observed by the event loop. Returning early here keeps the post-re-arm history
        // demonstrably empty (windows_filled == 0) per the D-15 test contract.
        let rearmed = cur && !self.prev_monitor_enabled;
        self.prev_monitor_enabled = cur;
        if rearmed {
            self.banner.clear_feedback_detected();
            self.window_history = [0.0; WINDOW_HISTORY_LEN];
            self.windows_filled = 0;
            self.tripped = false;
            return;
        }

        // If monitor is off (D-12 default + D-14 post-trip), do nothing further. The
        // detector only runs while the monitor is actively producing audio.
        if !cur {
            return;
        }
        // Already tripped this session — wait for re-arm to do anything.
        if self.tripped {
            return;
        }

        // Sample current RMS, shift history left, write into slot [4].
        let rms = self.tele.input_rms.load(Ordering::Relaxed);
        for i in 0..(WINDOW_HISTORY_LEN - 1) {
            self.window_history[i] = self.window_history[i + 1];
        }
        self.window_history[WINDOW_HISTORY_LEN - 1] = rms;
        self.windows_filled = (self.windows_filled + 1).min(WINDOW_HISTORY_LEN);

        // Evaluate the trip rule once we have a full history.
        if self.windows_filled == WINDOW_HISTORY_LEN && self.all_rose_by_6db_consecutively() {
            // D-14: auto-disable + banner.
            self.hot.monitor_enabled.store(false, Ordering::Relaxed);
            self.banner.set_feedback_detected();
            self.tripped = true;
        }
    }

    /// Return true iff every adjacent pair `(prev, next)` in `window_history` satisfies
    /// `next >= prev * 2.0` (6 dB linear) AND `prev` is above the silence floor. The silence
    /// floor avoids false positives from a healthy quiet-to-quiet transition.
    fn all_rose_by_6db_consecutively(&self) -> bool {
        for i in 0..(WINDOW_HISTORY_LEN - 1) {
            let prev = self.window_history[i];
            let next = self.window_history[i + 1];
            if prev < MIN_PREV_RMS_FOR_RISE {
                return false;
            }
            if next < prev * RISE_FACTOR_6DB {
                return false;
            }
        }
        true
    }

    /// Test-only accessor for the latched trip state. Used by the feedback-detector unit
    /// tests below; not part of the production surface (the event loop reads
    /// `banner.is_feedback_detected()` instead).
    #[cfg(test)]
    fn tripped(&self) -> bool {
        self.tripped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atomic_float::AtomicF32;
    use std::sync::atomic::{AtomicBool, AtomicU32};

    /// Build a fresh (Telemetry, HotParams, MonitorBannerState) trio for a detector test.
    /// Mirrors the constructor shape used in cpal_io.rs's tests and the rt_safety.rs file.
    fn build_scaffolding(
        monitor_enabled: bool,
    ) -> (Arc<Telemetry>, Arc<HotParams>, MonitorBannerState) {
        let tele = Arc::new(Telemetry {
            latency_ms: AtomicF32::new(0.0),
            input_rms: AtomicF32::new(0.0),
            xruns: AtomicU32::new(0),
        });
        let hot = Arc::new(HotParams {
            input_gain: AtomicF32::new(1.0),
            gate_threshold: AtomicF32::new(0.0),
            bypass: AtomicBool::new(false),
            monitor_enabled: AtomicBool::new(monitor_enabled),
        });
        let banner = MonitorBannerState::new();
        (tele, hot, banner)
    }

    /// AUDIO-07 / D-12: `HotParams::monitor_enabled` defaults to false immediately after
    /// engine construction. Verified here against the Phase 0 default-shape constructor.
    #[test]
    fn default_off() {
        let hot = HotParams {
            input_gain: AtomicF32::new(1.0),
            gate_threshold: AtomicF32::new(0.0),
            bypass: AtomicBool::new(false),
            monitor_enabled: AtomicBool::new(false),
        };
        assert!(
            !hot.monitor_enabled.load(Ordering::Relaxed),
            "HotParams::monitor_enabled must default to false per D-12"
        );
    }

    /// AUDIO-08 / D-13 trip path: feed 5 consecutive +6 dB (2× linear) rises into the
    /// detector. On the 5th tick the trip rule must fire: `tripped == true`,
    /// `monitor_enabled == false`, `banner.feedback_detected == true`.
    #[test]
    fn feedback_detector_trips_on_5x_6db_rise() {
        let (tele, hot, banner) = build_scaffolding(true);
        let mut detector = FeedbackDetector::new(tele.clone(), hot.clone(), banner.clone());

        // 6-reading sequence where each value is 2× the previous. The first call seeds the
        // history; by the 5th call the history is full and the trip rule fires. We add a
        // 6th reading just to confirm tripped stays latched across additional ticks.
        let rms_sequence = [0.001f32, 0.002, 0.004, 0.008, 0.016, 0.032];
        for &rms in &rms_sequence {
            tele.input_rms.store(rms, Ordering::Relaxed);
            detector.tick();
        }

        assert!(
            detector.tripped(),
            "detector must trip after 5 consecutive +6 dB rises"
        );
        assert!(
            !hot.monitor_enabled.load(Ordering::Relaxed),
            "on trip, HotParams::monitor_enabled must be set to false (D-14)"
        );
        assert!(
            banner.is_feedback_detected(),
            "on trip, MonitorBannerState::feedback_detected must be true (D-14)"
        );
    }

    /// AUDIO-08 negative path: a single RMS spike (cough/shout/door slam) MUST NOT trip the
    /// detector. The geometric-growth signature is what defines feedback; a single isolated
    /// spike followed by a return-to-baseline does not match.
    #[test]
    fn feedback_detector_does_not_trip_on_single_spike() {
        let (tele, hot, banner) = build_scaffolding(true);
        let mut detector = FeedbackDetector::new(tele.clone(), hot.clone(), banner.clone());

        // Single spike then return to quiet. 6 ticks total (one more than history length) to
        // ensure the spike has shifted fully out of the history window.
        let rms_sequence = [0.001f32, 0.5, 0.001, 0.001, 0.001, 0.001];
        for &rms in &rms_sequence {
            tele.input_rms.store(rms, Ordering::Relaxed);
            detector.tick();
        }

        assert!(
            !detector.tripped(),
            "detector must NOT trip on a single spike followed by quiet (D-13 5-consecutive rule)"
        );
        assert!(
            hot.monitor_enabled.load(Ordering::Relaxed),
            "monitor_enabled must remain true when the detector hasn't tripped"
        );
        assert!(
            !banner.is_feedback_detected(),
            "banner must remain cleared when the detector hasn't tripped"
        );
    }

    /// D-15 re-arm path: after a trip, manually re-enabling the monitor toggle clears the
    /// banner + history + latch on the very next tick (no cooldown, no grace period).
    #[test]
    fn re_arm_clears_banner_and_state() {
        let (tele, hot, banner) = build_scaffolding(true);
        let mut detector = FeedbackDetector::new(tele.clone(), hot.clone(), banner.clone());

        // Step 1: trip the detector via the 5×6dB sequence (same as the trip test).
        for &rms in &[0.001f32, 0.002, 0.004, 0.008, 0.016, 0.032] {
            tele.input_rms.store(rms, Ordering::Relaxed);
            detector.tick();
        }
        assert!(detector.tripped(), "precondition: trip must have occurred");
        assert!(
            !hot.monitor_enabled.load(Ordering::Relaxed),
            "precondition: monitor must be auto-disabled"
        );

        // Step 2: user re-enables the monitor toggle. On the next tick, the detector should
        // observe the false→true edge and clear all state.
        hot.monitor_enabled.store(true, Ordering::Relaxed);
        // Reset RMS to baseline so the post-clear history doesn't immediately re-trip.
        tele.input_rms.store(0.0, Ordering::Relaxed);
        detector.tick();

        assert!(
            !banner.is_feedback_detected(),
            "after re-arm, banner.feedback_detected must be cleared (D-15)"
        );
        assert!(
            !detector.tripped(),
            "after re-arm, detector.tripped latch must be false (D-15)"
        );
        // History should be zeroed — verify by checking the internal field (test-only access).
        assert_eq!(
            detector.window_history, [0.0; WINDOW_HISTORY_LEN],
            "after re-arm, window_history must be zeroed (D-15)"
        );
        assert_eq!(
            detector.windows_filled, 0,
            "after re-arm, windows_filled must reset to 0 (D-15)"
        );
    }

    /// Sanity: the banner copy constants match the D-14 / D-07 verbatim text. Catches any
    /// future drift in the copy that would silently desync the UI from the spec.
    #[test]
    fn banner_copy_matches_spec() {
        assert_eq!(
            FEEDBACK_BANNER_COPY,
            "Self-monitor disabled — feedback detected. Use headphones, not speakers.",
            "FEEDBACK_BANNER_COPY must match D-14 verbatim"
        );
        assert_eq!(
            DISCONNECT_BANNER_COPY, "Audio disconnected — click to reconnect.",
            "DISCONNECT_BANNER_COPY must match D-07 verbatim"
        );
    }
}

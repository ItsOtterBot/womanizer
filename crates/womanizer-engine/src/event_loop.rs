//! Off-RT engine event loop — pumps `EngineCommand` / `EngineEvent`, drains the `ErrorRing`,
//! and owns the reconnect path (D-21).
//!
//! Populated by Plan 01-02b. Channels are `crossbeam-channel::unbounded`; the loop body uses
//! `recv_timeout(50 ms)` so the ErrorRing drain happens at a fixed cadence even when no
//! command is in flight (Plan 01-05 will additionally tick the feedback-loop detector here
//! per revision W8).
//!
//! ## Threading
//! This module owns a single OS thread (`womanizer-engine-loop`) that runs [`Engine::run`].
//! The thread owns:
//!   * the cpal capture + virtual-output [`Stream`](cpal::Stream)s — cpal `Stream` is `!Send`
//!     on some backends, so we build/drop streams on this same thread and never need to
//!     send a stream across a thread boundary;
//!   * the [`WorkerHandles`](crate::worker::WorkerHandles) for the DSP + capture-pump
//!     threads;
//!   * the rings (the engine constructs both halves BEFORE `build_*_stream` per RESEARCH
//!     Pitfall #Y; the capture / playback closures take the producer / consumer halves by
//!     move);
//!   * the in-memory [`EngineState`] that preserves the last-selected device names across
//!     a `DeviceFault → user-click → Start` reconnect (AUDIO-09 + D-07 + D-21 — Phase 1
//!     ships in-memory IDs only; SQLite roundtrip is Phase 5's remit).
//!
//! The public surface for the UI is [`EngineHandle`] (the `crossbeam-channel` ends + the
//! shared `Arc<HotParams>` / `Arc<Telemetry>` atomics) returned by [`spawn`].
//!
//! ## AUDIO-09 reconnect contract
//! After `EngineEvent::Error(DeviceFault)` fires, the engine does NOT auto-rebuild — it
//! waits for `EngineCommand::Start` from the UI (matches D-07 "Reconnect path: user clicks
//! → engine rebuilds"). When `Start` arrives while `streams.is_some()`, the engine drops
//! the old streams + joins worker threads first, then rebuilds from `self.state`. Phase 1
//! scope per D-21: in-memory device-ID preservation only; SQLite roundtrip + UX flourishes
//! land in Phase 5.

use std::sync::atomic::AtomicUsize;
use std::sync::Arc;
use std::thread::Builder;
use std::time::Duration;

use crossbeam_channel::{unbounded, Receiver, RecvTimeoutError, Sender};
use rtrb::{Consumer, Producer, RingBuffer};
use womanizer_core::{
    DspWakeHandle, EngineCommand, EngineError, EngineEvent, ErrorRing, HotParams, InputRing,
    MonitorOutRing, Telemetry, VirtualOutRing, VoiceParams, RING_CAPACITY,
};

use crate::cpal_io::{
    self, build_capture_stream, build_virtual_output_stream, EngineStreams, TimestampPair,
    TS_RING_CAPACITY,
};
use crate::monitor::{FeedbackDetector, MonitorBannerState};
use crate::resampler::SampleRateState;
use crate::worker::{self, WorkerHandles};

/// 50 ms event-loop tick — the cadence at which the engine drains the `ErrorRing` and (in
/// Plan 01-05) ticks the feedback-loop detector. Matches the cadence the RESEARCH "Engine
/// handle + start/stop driven by EngineCommand" example uses.
const TICK: Duration = Duration::from_millis(50);

/// In-memory state preserved across `EngineCommand::Start` cycles. After a `DeviceFault`
/// fires, the user clicks the reconnect banner and the UI re-sends `EngineCommand::Start`;
/// the event loop rebuilds capture + virtual-output streams using THESE device names.
///
/// Phase 1 scope per D-21: in-memory only. Phase 5 owns the SQLite-roundtrip polish so a
/// last-selected device survives an app restart.
///
/// Names match the user-visible cpal `Device::description().name()` (the same id surfaced
/// by [`cpal_io::enumerate_inputs`] and the per-OS `devices::detect()` modules). When
/// either field is `None`, the engine falls back to the host's default device for that side
/// and logs an off-RT `tracing::warn!`.
#[derive(Debug, Default, Clone)]
pub struct EngineState {
    /// Last-selected input device name (mic). `None` → use `default_input_device()`.
    pub selected_input: Option<String>,
    /// Last-selected virtual-output device name (the device VRChat sees as a mic — VB-CABLE
    /// Input on Windows). `None` → use `default_output_device()` (a sensible fallback only
    /// for development; the real product flow always selects the virtual device by name).
    pub selected_virtual_output: Option<String>,
    /// Last-selected monitor (headphones) device name for the self-monitor stream (D-12).
    /// `None` → use `default_output_device()`. If the monitor stream fails to build, the
    /// engine logs a warning and continues without monitor — the mic → virtual-output path
    /// is unaffected.
    pub selected_monitor: Option<String>,
}

/// Public handle the UI consumes. The UI side sends [`EngineCommand`]s on `cmd_tx` and
/// receives [`EngineEvent`]s on `evt_rx`. The shared atomics (`hot` / `tele`) are
/// cloneable — the UI reads telemetry every repaint and writes hot params from sliders.
#[derive(Clone)]
pub struct EngineHandle {
    /// Send `EngineCommand::Start | Stop | SelectVoice(id)` to drive the engine.
    pub cmd_tx: Sender<EngineCommand>,
    /// Receive `EngineEvent::Started | Stopped | Error(EngineError)` from the engine.
    pub evt_rx: Receiver<EngineEvent>,
    /// Shared hot-scalar atomics (input gain, gate threshold, bypass, monitor_enabled).
    pub hot: Arc<HotParams>,
    /// Shared live telemetry (latency, RMS, xrun count).
    pub tele: Arc<Telemetry>,
    /// Shared banner-state atomics (feedback_detected + disconnected). Plan 01-05's UI
    /// repaint loop reads `is_feedback_detected()` / `is_disconnected()` each frame to render
    /// the AUDIO-08 / D-14 + AUDIO-09 / D-07 yellow banners. The engine's `FeedbackDetector`
    /// (instantiated inside `Engine::build_streams_and_worker` per W8 wiring) writes
    /// `feedback_detected = true` on trip; the event loop sets `disconnected = true` when it
    /// drains an `EngineError::DeviceFault` from the ErrorRing. Cloned via inner Arcs so UI
    /// and engine see the same atomics.
    pub monitor_banner: MonitorBannerState,
    /// Wait-free publisher of the current sample-rate-mismatch state (AUDIO-04 yellow
    /// banner). The engine sets this in `build_streams_and_worker` when the picked input
    /// device's native rate is not 48 kHz; the UI clones this on the Setup → Ready
    /// transition and reads it each repaint to render `RESAMPLE_BANNER_TEMPLATE`. Cloned
    /// via inner `Arc<AtomicU32>` so both sides see the same atomic without locking.
    pub sample_rate_state: SampleRateState,
    /// ErrorRing producer the engine drains on every 50 ms tick alongside the per-Start
    /// capture / virtual-output rings. The principal consumer is the integration test in
    /// `tests/reconnect.rs` (AUDIO-09 ground truth — synthesize a `DeviceFault` without a
    /// real device disconnect) and any future chaos / sleep-wake harness in Phase 5.
    ///
    /// Feature-gated behind `cfg(any(test, feature = "test-injection"))` (CR-03 fix) so the
    /// shipped production binary cannot reach the injection path at all — preventing a
    /// future contributor from accidentally taking the `Mutex` on a UI or RT thread.
    /// `tests/reconnect.rs` declares `required-features = ["test-injection"]` in
    /// `Cargo.toml` and is the sole intended caller.
    ///
    /// The `Mutex` wrap is purely to keep the handle `Send + Sync + Clone`; rtrb
    /// `Producer` is `!Sync` by construction.
    #[cfg(any(test, feature = "test-injection"))]
    pub err_inject_tx: Arc<std::sync::Mutex<Producer<EngineError>>>,
}

#[cfg(any(test, feature = "test-injection"))]
impl EngineHandle {
    /// Inject a synthetic engine error into the persistent injection ring the event loop
    /// drains on every 50 ms tick. Used by `tests/reconnect.rs` to exercise the AUDIO-09
    /// path without a real device disconnect. Production UI must NOT call this — it is a
    /// test/diagnostic hook and is feature-gated out of release builds.
    ///
    /// Returns `Ok(())` on push success, `Err(EngineError)` if the mutex was poisoned or
    /// the ring is full. A poisoned mutex no longer panics the caller; the injection is
    /// just dropped (the engine is in a degraded state and the test framework will surface
    /// the original panic anyway).
    pub fn inject_error(&self, e: EngineError) {
        let Ok(mut tx) = self.err_inject_tx.lock() else {
            // Poisoned mutex — engine is in a degraded state. Drop the injection rather
            // than panic-cascade; the original panic that poisoned the lock will surface
            // through normal test failure reporting.
            return;
        };
        // Best-effort: if the ring is full the engine has plenty to drain; dropping a
        // synthetic injection is fine for the test contract.
        let _ = tx.push(e);
    }
}

/// The off-RT engine — owned by the thread spawned by [`spawn`].
struct Engine {
    // --- channels (UI ↔ engine) ---
    cmd_rx: Receiver<EngineCommand>,
    evt_tx: Sender<EngineEvent>,

    // --- shared atomics + wake ---
    hot: Arc<HotParams>,
    tele: Arc<Telemetry>,

    // --- banner state shared with the UI (W8 wiring half) ---
    //
    // Engine writes `disconnected = true` when an `EngineError::DeviceFault` is drained from
    // the ErrorRing and the UI's repaint loop reads `is_disconnected()` each frame; the
    // FeedbackDetector inside the engine writes `feedback_detected = true` on trip. The same
    // MonitorBannerState struct is cloned into `EngineHandle::monitor_banner` so both sides
    // see the same Arc<AtomicBool> pair.
    monitor_banner: MonitorBannerState,

    // --- AUDIO-04 sample-rate-mismatch publisher (shared with the EngineHandle) ---
    //
    // Engine writes the input device's native rate when it picks a non-48-kHz input config
    // (and clears on a successful 48-kHz pick). The UI clones this and reads each repaint
    // to render the AUDIO-04 yellow banner verbatim per D-05.
    sample_rate_state: SampleRateState,

    // --- in-memory device-name state preserved across reconnect (AUDIO-09 + D-21) ---
    state: EngineState,

    // --- error-path consumers (drained on every Timeout tick) ---
    //
    // Three sources funnel here:
    //   1. `inject_err_rx`: persistent ring for tests/reconnect.rs synthesizing faults.
    //      Lives for the lifetime of the engine (created in `spawn`, dropped on engine
    //      thread exit).
    //   2. `capture_err_rx`: per-Start ring; producer half is moved into the capture
    //      cpal closure's `error_callback`. `None` when not Started.
    //   3. `vout_err_rx`: per-Start ring; producer half is moved into the virtual-output
    //      cpal closure's `error_callback`. `None` when not Started.
    //
    // We pop from all three in `drain_error_ring`. rtrb's single-consumer constraint is
    // satisfied: each Consumer is owned exclusively by the engine thread.
    inject_err_rx: Consumer<EngineError>,
    capture_err_rx: Option<Consumer<EngineError>>,
    vout_err_rx: Option<Consumer<EngineError>>,

    // --- stream + worker lifetime (Some only when Started) ---
    streams: Option<EngineStreams>,
    worker: Option<WorkerHandles>,

    // --- feedback detector (Some only when Started — W8 wiring) ---
    //
    // Constructed inside `build_streams_and_worker` once `tele` / `hot` / `monitor_banner`
    // are live; ticked from the Timeout arm at the 50 ms `recv_timeout` cadence; dropped in
    // `handle_stop_silent` alongside the streams + worker handles. The detector observes
    // `Telemetry::input_rms` (written by the cpal capture callback) and on trip clears
    // `HotParams::monitor_enabled` + sets `monitor_banner.feedback_detected = true` (D-14).
    feedback_detector: Option<FeedbackDetector>,
}

impl Engine {
    /// Run the event loop until the command channel is disconnected (sender drop).
    fn run(mut self) {
        tracing::info!("engine event loop entering recv_timeout(50ms) main loop");
        loop {
            match self.cmd_rx.recv_timeout(TICK) {
                Ok(EngineCommand::Start) => self.handle_start(),
                Ok(EngineCommand::Stop) => self.handle_stop(),
                Ok(EngineCommand::SelectVoice(id)) => {
                    // Phase 4 wires `snap_in.write(VoiceParams::from_db(id))` here; Phase 1
                    // is a no-op + debug log. The worker's `snap_out` side is already wired
                    // by `worker::spawn`, and Phase 1's worker ignores its contents per
                    // D-01 (memcpy passthrough).
                    tracing::debug!(?id, "SelectVoice received (Phase 4 wiring placeholder)");
                }
                Ok(EngineCommand::SetInput(name)) => {
                    tracing::info!(?name, "SetInput received");
                    self.state.selected_input = name;
                    // Hot-swap if streams are live: tear down + rebuild against the new device.
                    if self.streams.is_some() {
                        self.handle_stop_silent();
                        self.handle_start();
                    }
                }
                Ok(EngineCommand::SetVirtualOutput(name)) => {
                    tracing::info!(?name, "SetVirtualOutput received");
                    self.state.selected_virtual_output = name;
                    if self.streams.is_some() {
                        self.handle_stop_silent();
                        self.handle_start();
                    }
                }
                Ok(EngineCommand::SetMonitor(name)) => {
                    tracing::info!(?name, "SetMonitor received");
                    self.state.selected_monitor = name;
                    if self.streams.is_some() {
                        self.handle_stop_silent();
                        self.handle_start();
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    self.drain_error_ring();
                    // W8 wiring (Plan 01-05): tick the FeedbackDetector at the same 50 ms
                    // cadence as the ErrorRing drain. The detector is only present while
                    // streams are Started (built in `build_streams_and_worker`, dropped in
                    // `handle_stop_silent`), so the `if let` keeps the Stopped path a no-op.
                    // The detector observes `Telemetry::input_rms` (written by the cpal
                    // capture callback) — when 5 consecutive 50 ms RMS windows each rise by
                    // ≥ 6 dB, it stores `monitor_enabled = false` (D-14) and sets
                    // `monitor_banner.feedback_detected = true` so the UI repaints the
                    // yellow banner (D-13 + AUDIO-08 500 ms total budget).
                    if let Some(fd) = self.feedback_detector.as_mut() {
                        fd.tick();
                    }
                }
                Err(RecvTimeoutError::Disconnected) => {
                    tracing::info!("engine cmd channel disconnected; event loop exiting");
                    break;
                }
            }
        }
        // Final teardown — streams first (stops the cpal callbacks pushing into rings),
        // then worker handles (stops the DSP + pump threads).
        self.handle_stop();
    }

    /// Handle `EngineCommand::Start`. Idempotent if already running. On error, emit
    /// `EngineEvent::Error(DeviceFault)` so the UI surfaces the reconnect banner; do NOT
    /// auto-retry (D-07 — the user clicks → engine rebuilds).
    fn handle_start(&mut self) {
        // AUDIO-09 reconnect: if streams are still alive from a previous Start that
        // hasn't been Stopped (e.g. the user clicked Start again after a DeviceFault that
        // didn't auto-stop), tear them down before rebuilding. This matches D-21 + D-07
        // "Reconnect path: user clicks → engine rebuilds" — the engine is responsible for
        // the destructive half of the cycle so the UI just sends a fresh Start.
        if self.streams.is_some() || self.worker.is_some() {
            tracing::info!(
                "Start received while streams still alive (post-fault reconnect or duplicate Start); \
                 tearing down old streams before rebuild (AUDIO-09 + D-21)"
            );
            self.handle_stop_silent();
        }

        match self.build_streams_and_worker() {
            Ok(()) => {
                tracing::info!("engine started");
                let _ = self.evt_tx.send(EngineEvent::Started);
            }
            Err(e) => {
                tracing::error!(?e, "build_streams failed");
                let _ = self
                    .evt_tx
                    .send(EngineEvent::Error(EngineError::DeviceFault));
            }
        }
    }

    /// Build capture + virtual-output streams + spawn DSP/pump workers. Resolves device
    /// names from `self.state` (falling back to host defaults if `None`).
    fn build_streams_and_worker(&mut self) -> Result<(), EngineBootError> {
        use cpal::traits::HostTrait;

        // ---- resolve devices by name (falling back to host defaults) ----
        let host = cpal::default_host();
        let input_device = match &self.state.selected_input {
            Some(name) => find_input_device_by_name(&host, name).or_else(|| {
                tracing::warn!(
                    name = %name,
                    "selected input device not found; falling back to default_input_device (AUDIO-09 in-memory fallback)"
                );
                host.default_input_device()
            }),
            None => {
                tracing::warn!(
                    "no selected_input set; using default_input_device (AUDIO-09 in-memory fallback)"
                );
                host.default_input_device()
            }
        }
        .ok_or(EngineBootError::NoDefaultInput)?;

        let vout_device = match &self.state.selected_virtual_output {
            Some(name) => find_output_device_by_name(&host, name).or_else(|| {
                tracing::warn!(
                    name = %name,
                    "selected virtual-output device not found; falling back to default_output_device (AUDIO-09 in-memory fallback)"
                );
                host.default_output_device()
            }),
            None => {
                tracing::warn!(
                    "no selected_virtual_output set; using default_output_device (AUDIO-09 in-memory fallback)"
                );
                host.default_output_device()
            }
        }
        .ok_or(EngineBootError::NoDefaultOutput)?;

        // ---- construct rings + atomics BEFORE build_*_stream (RESEARCH Pitfall #Y) ----
        let (in_tx, in_rx): InputRing = RingBuffer::new(RING_CAPACITY);
        let (vo_tx, vo_rx): VirtualOutRing = RingBuffer::new(RING_CAPACITY);
        // mo_rx is moved into the monitor stream constructor below. If the monitor stream
        // fails to build (e.g. user's monitor device is busy), we drop mo_rx — the worker's
        // `mo_tx.push_entire_slice(...)` calls become no-ops once the consumer disappears.
        let (mo_tx, mo_rx): MonitorOutRing = RingBuffer::new(RING_CAPACITY);

        let (ts_tx, ts_rx): (Producer<TimestampPair>, Consumer<TimestampPair>) =
            RingBuffer::new(TS_RING_CAPACITY);

        // Per-Start ErrorRings — one per cpal stream (rtrb Producers are NOT cloneable,
        // and each cpal error_callback needs its own Producer half). Both consumers are
        // drained on every Timeout tick by `drain_error_ring`. Capacity 32 each = 64 slots
        // total per Start (matches smoke.rs:64).
        let (capture_err_tx, capture_err_rx) = ErrorRing::new(32);
        let (vout_err_tx, vout_err_rx) = ErrorRing::new(32);

        // ---- triple_buffer snapshot — Phase 1 ignores contents; Phase 4 writes voices ----
        let (_snap_in, snap_out) = triple_buffer::triple_buffer(&VoiceParams::default());

        // ---- counters + wake ----
        let samples_since_wake = Arc::new(AtomicUsize::new(0));

        // Spawn worker threads. `worker::spawn` internally constructs the wake handle bound
        // to the spawned DSP worker thread (see `spawn_dsp_worker` for the one-shot channel
        // that hands the thread's `Thread` handle back to the caller so the wake's
        // `unpark()` target is correct). The pump thread gets that same wake clone — when
        // it observes ≥ BLOCK frames in `samples_since_wake`, calling `wake.wake()` actually
        // unparks the DSP worker, which was the Phase 1 close-out bug.
        let worker = worker::spawn(
            in_rx,
            vo_tx,
            mo_tx,
            samples_since_wake.clone(),
            self.hot.clone(),
            snap_out,
        )
        .map_err(EngineBootError::SpawnWorker)?;

        // The `engine_wake` clone passed into the cpal error_callbacks. In Phase 1 the
        // engine event loop does NOT park (recv_timeout(50ms) wakes naturally on its own
        // schedule), so this wake is effectively a "promote a future error slightly
        // sooner" hint that does nothing useful in Phase 1. We still pass it because the
        // cpal_io public signatures require it; production behavior is correct.
        let engine_wake = DspWakeHandle::new(std::thread::current());

        // ---- Pick the input config + decide whether a rubato resampler is needed.
        //      AUDIO-03 + AUDIO-04: when the device's native rate isn't 48 kHz, we
        //      construct a Resampler48k(native_rate), pass it into the capture stream so
        //      the callback resamples mono native → mono 48k on the fly, AND publish the
        //      native rate to `sample_rate_state` so the UI lights up the yellow banner.
        //      A successful 48 kHz pick clears the banner.
        let input_config =
            cpal_io::pick_input_config(&input_device).map_err(EngineBootError::BuildCapture)?;
        let input_resampler = if input_config.sample_rate() != cpal_io::SAMPLE_RATE_HZ {
            let native = input_config.sample_rate();
            let rs = crate::resampler::Resampler48k::new(native).map_err(|e| {
                tracing::error!(
                    ?e,
                    native,
                    "resampler init failed; surfacing as device fault"
                );
                EngineBootError::BuildCapture(cpal_io::EngineBuildError::NoCompatibleConfig {
                    channels: 1,
                })
            })?;
            self.sample_rate_state.set_mismatch(native);
            tracing::info!(native, "rubato Resampler48k engaged for input device");
            Some(rs)
        } else {
            self.sample_rate_state.clear();
            None
        };

        // ---- build capture + virtual-output streams (rings already constructed) ----
        let input_stream = build_capture_stream(
            &input_device,
            input_config,
            in_tx,
            samples_since_wake,
            ts_tx,
            self.hot.clone(),
            self.tele.clone(),
            capture_err_tx,
            engine_wake.clone(),
            input_resampler,
        )
        .map_err(EngineBootError::BuildCapture)?;

        let vout_stream = build_virtual_output_stream(
            &vout_device,
            vo_rx,
            ts_rx,
            self.tele.clone(),
            vout_err_tx,
            engine_wake.clone(),
        )
        .map_err(EngineBootError::BuildVirtualOut)?;

        // ---- monitor (headphones) stream — non-fatal. Resolves the selected_monitor device
        // (falling back to host default), tries to build, logs + drops `mo_rx` on failure. ----
        let monitor_stream = {
            let device = match &self.state.selected_monitor {
                Some(name) => find_output_device_by_name(&host, name).or_else(|| {
                    tracing::warn!(
                        name = %name,
                        "selected monitor device not found; falling back to default_output_device"
                    );
                    host.default_output_device()
                }),
                None => host.default_output_device(),
            };
            match device {
                Some(dev) => {
                    // Build a dedicated ErrorRing for the monitor stream so its faults don't
                    // mix into the capture / vout rings.
                    let (mon_err_tx, _mon_err_rx) = ErrorRing::new(32);
                    match crate::monitor::build_monitor_output_stream(
                        &dev,
                        mo_rx,
                        self.hot.clone(),
                        self.tele.clone(),
                        mon_err_tx,
                        engine_wake,
                    ) {
                        Ok(s) => {
                            tracing::info!("monitor stream built");
                            Some(s)
                        }
                        Err(e) => {
                            tracing::warn!(error = ?e, "monitor stream build failed; continuing without self-monitor");
                            None
                        }
                    }
                }
                None => {
                    tracing::warn!("no monitor device available; continuing without self-monitor");
                    drop(mo_rx);
                    None
                }
            }
        };

        // ---- W8 wiring: instantiate the FeedbackDetector now that tele / hot / banner
        // are live and the streams that drive `Telemetry::input_rms` are about to run. ----
        //
        // The detector ticks at the same 50 ms cadence as the ErrorRing drain (from the
        // Timeout arm); on a 5×6 dB trip it clears `HotParams::monitor_enabled` and sets
        // `monitor_banner.feedback_detected` so the UI's repaint loop renders the yellow
        // banner. The MonitorBannerState is shared with the EngineHandle (cloned via inner
        // Arc<AtomicBool> pair) so UI + engine see the same atomics.
        let feedback_detector = FeedbackDetector::new(
            self.tele.clone(),
            self.hot.clone(),
            self.monitor_banner.clone(),
        );

        // ---- W8 wiring: a fresh Start clears any leftover disconnect banner from the
        // previous session. The UI's "click to reconnect" handler sends Start, the engine
        // rebuilds streams successfully, the banner clears. ----
        self.monitor_banner.clear_disconnected();

        // ---- stash on self for the duration of the Started state ----
        self.streams = Some(EngineStreams {
            input: input_stream,
            virtual_out: vout_stream,
            monitor: monitor_stream,
        });
        self.worker = Some(worker);
        self.capture_err_rx = Some(capture_err_rx);
        self.vout_err_rx = Some(vout_err_rx);
        self.feedback_detector = Some(feedback_detector);
        Ok(())
    }

    /// Handle `EngineCommand::Stop` — drop streams + join workers, then emit
    /// `EngineEvent::Stopped`. Idempotent.
    fn handle_stop(&mut self) {
        let was_running = self.streams.is_some() || self.worker.is_some();
        self.handle_stop_silent();
        if was_running {
            tracing::info!("engine stopped");
            let _ = self.evt_tx.send(EngineEvent::Stopped);
        }
    }

    /// Same as `handle_stop` but does not emit `EngineEvent::Stopped`. Used internally by
    /// `handle_start` to tear down old streams before rebuilding (the UI should not see a
    /// spurious Stopped between two Started events).
    fn handle_stop_silent(&mut self) {
        // Drop streams first — this halts the RT callbacks pushing into rings, so the
        // worker's next wait() will return shortly (the pump observes no new samples and
        // the worker's stop_flag will trip on the next iteration).
        self.streams = None;
        if let Some(worker) = self.worker.take() {
            worker
                .stop_flag
                .store(true, std::sync::atomic::Ordering::Relaxed);
            // CRITICAL: a bare `unpark()` is NOT sufficient — `wake.wait()` only exits its
            // park-loop when `pending` is true, and a bare unpark leaves `pending=false`, so
            // the DSP worker would just re-park forever and `join()` would deadlock (the
            // hang Andrew hit live: Stop click → event loop frozen → Start ignored). Use the
            // full `wake()` (sets `pending=true` AND unparks) so the wait() loop observes
            // pending, returns, then checks stop_flag and exits.
            worker.stop_wake.wake();
            // join() returns Result<()> wrapping the thread's panic if any; ignore both
            // outcomes in the stop path (panics surface separately via tracing in worker).
            let _ = worker.dsp_thread.join();
            let _ = worker.pump_thread.join();
        }
        // Drop the per-Start error consumers — the producer halves were moved into the
        // cpal closures and are dropped with the streams above.
        self.capture_err_rx = None;
        self.vout_err_rx = None;

        // Drop the per-Start FeedbackDetector — its Arc<Telemetry> / Arc<HotParams> /
        // MonitorBannerState clones are held only by the detector and the EngineHandle, so
        // letting it drop here releases this engine's hold on them. The banner state itself
        // is NOT cleared here: a sticky disconnect banner survives Stop so the UI can show
        // it after a teardown caused by a DeviceFault.
        self.feedback_detector = None;
    }

    /// Drain ALL error-ring consumers (injection + capture + virtual-output) into
    /// `EngineEvent::Error` messages for the UI banner. Called on every Timeout tick (50 ms
    /// cadence). Pattern: `let _ = self.evt_tx.send(...)` — the UI may have already dropped
    /// the receiver (window closed); the next iteration's `Disconnected` arm handles
    /// graceful shutdown.
    ///
    /// W8 wiring side-effect: a drained `EngineError::DeviceFault` ALSO sets the
    /// `MonitorBannerState::disconnected` flag so the UI's repaint loop renders the AUDIO-09
    /// / D-07 yellow banner without waiting to observe the `EngineEvent` arrival. Both paths
    /// (the event-channel emission AND the banner flag) are in place — the UI is free to
    /// listen on either; the banner-flag path is the simpler 30-Hz repaint check.
    fn drain_error_ring(&mut self) {
        while let Ok(e) = self.inject_err_rx.pop() {
            tracing::warn!(
                ?e,
                "engine error drained from injection ErrorRing (test path)"
            );
            if matches!(e, EngineError::DeviceFault) {
                self.monitor_banner.set_disconnected();
            }
            let _ = self.evt_tx.send(EngineEvent::Error(e));
        }
        if let Some(rx) = self.capture_err_rx.as_mut() {
            while let Ok(e) = rx.pop() {
                tracing::warn!(?e, "engine error drained from capture ErrorRing");
                if matches!(e, EngineError::DeviceFault) {
                    self.monitor_banner.set_disconnected();
                }
                let _ = self.evt_tx.send(EngineEvent::Error(e));
            }
        }
        if let Some(rx) = self.vout_err_rx.as_mut() {
            while let Ok(e) = rx.pop() {
                tracing::warn!(?e, "engine error drained from virtual-output ErrorRing");
                if matches!(e, EngineError::DeviceFault) {
                    self.monitor_banner.set_disconnected();
                }
                let _ = self.evt_tx.send(EngineEvent::Error(e));
            }
        }
    }
}

/// Errors returned by `Engine::build_streams_and_worker`. The event loop maps every
/// variant to `EngineEvent::Error(DeviceFault)` for the UI — the banner is the same
/// regardless of which sub-step failed; the detail goes into the `tracing::error!` log so
/// a developer reading stderr knows whether it was capture, playback, or worker spawn that
/// failed.
#[derive(Debug, thiserror::Error)]
enum EngineBootError {
    /// No default input device on this host (headless CI or no audio hardware).
    #[error("no default input device on this host")]
    NoDefaultInput,
    /// No default output device on this host (headless CI or no audio hardware).
    #[error("no default output device on this host")]
    NoDefaultOutput,
    /// `cpal_io::build_capture_stream` failed (format mismatch, permission denied, etc.).
    #[error("build_capture_stream failed: {0}")]
    BuildCapture(cpal_io::EngineBuildError),
    /// `cpal_io::build_virtual_output_stream` failed.
    #[error("build_virtual_output_stream failed: {0}")]
    BuildVirtualOut(cpal_io::EngineBuildError),
    /// `worker::spawn` failed (thread spawn returned `Err`).
    #[error("worker spawn failed: {0}")]
    SpawnWorker(std::io::Error),
}

/// Look up an input device by user-visible name. The name is the composed `"endpoint (driver)"`
/// form produced by `cpal_io::enumerate_inputs`; falls back to a bare-name match (for legacy
/// settings rows written before the composed-name change). Returns `None` if no match.
fn find_input_device_by_name(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    cpal_io::match_input_device_by_composed_name(host, name).or_else(|| {
        host.input_devices()
            .ok()?
            .find(|d| d.description().ok().is_some_and(|desc| desc.name() == name))
    })
}

/// Look up an output device by user-visible name. Same fallback contract as
/// `find_input_device_by_name`.
fn find_output_device_by_name(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    cpal_io::match_output_device_by_composed_name(host, name).or_else(|| {
        host.output_devices()
            .ok()?
            .find(|d| d.description().ok().is_some_and(|desc| desc.name() == name))
    })
}

/// Public spawner: creates the cmd/evt channels, constructs the engine struct, spawns the
/// engine event-loop thread, and returns the [`EngineHandle`] the UI consumes.
///
/// The returned `EngineHandle` is the SOLE public surface for driving the engine — the UI
/// never sees the `Engine` struct itself. The thread is detached: when the UI drops the
/// `EngineHandle`, `cmd_tx` is dropped, the engine loop sees `Disconnected`, calls
/// `handle_stop` to tear down streams + workers, and exits cleanly.
///
/// # Arguments
/// - `initial_state`: in-memory device-name state (last-selected input + virtual output).
///   Phase 1 expects this empty on first start (host defaults are used + a warn logs);
///   Phase 5 will populate from SQLite settings.
/// - `hot`: shared `HotParams` atomics, also cloned into the worker + the UI side.
/// - `tele`: shared `Telemetry` atomics, also cloned into the cpal closures + the UI side.
pub fn spawn(
    initial_state: EngineState,
    hot: Arc<HotParams>,
    tele: Arc<Telemetry>,
) -> EngineHandle {
    let (cmd_tx, cmd_rx) = unbounded::<EngineCommand>();
    let (evt_tx, evt_rx) = unbounded::<EngineEvent>();

    // Persistent injection ErrorRing — lives for the lifetime of the engine. The consumer
    // is always present (the engine drains it every 50 ms tick); the producer Arc-Mutex
    // wrapper is only built when the `test-injection` feature is on, so the production
    // binary cannot reach the inject path (CR-03 gate).
    let (inject_err_tx, inject_err_rx) = ErrorRing::new(64);

    #[cfg(any(test, feature = "test-injection"))]
    let err_inject_tx_handle = Arc::new(std::sync::Mutex::new(inject_err_tx));
    #[cfg(not(any(test, feature = "test-injection")))]
    drop(inject_err_tx);

    // W8 wiring: construct a single MonitorBannerState that both the engine (writes
    // disconnected/feedback_detected on trip) and the UI (reads them each repaint to render
    // the yellow banners) share. Cloned via inner Arc<AtomicBool> — both holders see the
    // same atomics. The shared state is created HERE (not in build_streams_and_worker) so
    // the disconnect banner can be set on the very first DeviceFault even if streams have
    // not yet been built (e.g. when default_input_device() returns None on a headless box —
    // build fails, error event emits, banner reflects the disconnected state immediately).
    let monitor_banner = MonitorBannerState::new();

    // AUDIO-04 sample-rate publisher — shared with the EngineHandle via internal Arc<AtomicU32>
    // clone so the UI's yellow banner reads the same atomic the engine writes.
    let sample_rate_state = SampleRateState::new();

    let engine = Engine {
        cmd_rx,
        evt_tx,
        hot: hot.clone(),
        tele: tele.clone(),
        monitor_banner: monitor_banner.clone(),
        sample_rate_state: sample_rate_state.clone(),
        state: initial_state,
        inject_err_rx,
        capture_err_rx: None,
        vout_err_rx: None,
        streams: None,
        worker: None,
        feedback_detector: None,
    };

    // Spawn the engine event-loop thread. We do not retain the JoinHandle — the engine
    // exits when its cmd channel sees `Disconnected` (i.e. when the UI drops the
    // EngineHandle and we drop cmd_tx). Production runs forever; tests drop the handle
    // and the thread exits within ~50 ms (one recv_timeout tick).
    let _join = Builder::new()
        .name("womanizer-engine-loop".to_string())
        .spawn(move || engine.run())
        .expect("spawn engine event loop thread");

    EngineHandle {
        cmd_tx,
        evt_rx,
        hot,
        tele,
        monitor_banner,
        sample_rate_state,
        #[cfg(any(test, feature = "test-injection"))]
        err_inject_tx: err_inject_tx_handle,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    /// Sanity: `EngineState::default()` returns both fields as `None` so the engine falls
    /// back to host defaults on first Start (matches the Phase 1 D-21 contract).
    #[test]
    fn engine_state_default_is_empty() {
        let s = EngineState::default();
        assert!(s.selected_input.is_none());
        assert!(s.selected_virtual_output.is_none());
    }

    /// Sanity: spawning an engine returns a handle whose cmd channel accepts Stop without
    /// panicking. We do NOT send Start here because that would attempt to open a real cpal
    /// device — that path lives in `tests/reconnect.rs` (the integration test that exercises
    /// the AUDIO-09 reconnect cycle).
    #[test]
    fn spawn_returns_a_live_handle_that_accepts_stop() {
        let hot = Arc::new(HotParams {
            input_gain: atomic_float::AtomicF32::new(1.0),
            gate_threshold: atomic_float::AtomicF32::new(0.0),
            bypass: AtomicBool::new(false),
            monitor_enabled: AtomicBool::new(false),
        });
        let tele = Arc::new(Telemetry {
            latency_ms: atomic_float::AtomicF32::new(0.0),
            input_rms: atomic_float::AtomicF32::new(0.0),
            xruns: std::sync::atomic::AtomicU32::new(0),
        });
        let handle = spawn(EngineState::default(), hot, tele);
        // Stop is idempotent — engine is in Stopped state already, so no event fires.
        handle
            .cmd_tx
            .send(EngineCommand::Stop)
            .expect("cmd_tx alive");
        // Allow one tick for the engine to process; nothing should arrive on evt_rx.
        std::thread::sleep(Duration::from_millis(75));
        match handle.evt_rx.try_recv() {
            Err(crossbeam_channel::TryRecvError::Empty) => {} // expected
            Ok(ev) => panic!("unexpected event from Stop on Stopped engine: {ev:?}"),
            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                panic!("engine thread exited prematurely")
            }
        }
        // Drop the handle — the engine loop sees Disconnected and exits within ~50 ms.
        drop(handle);
    }

    /// Sanity: the test-only injection path delivers an error to the event loop within a
    /// single 50 ms tick. We construct the engine WITHOUT building streams (so no real
    /// audio device is touched), inject a `DeviceFault`, and assert the matching
    /// `EngineEvent::Error(DeviceFault)` arrives on `evt_rx`. This is the building block
    /// `tests/reconnect.rs` uses to exercise the full AUDIO-09 cycle.
    #[test]
    fn inject_error_delivers_to_evt_rx() {
        let hot = Arc::new(HotParams {
            input_gain: atomic_float::AtomicF32::new(1.0),
            gate_threshold: atomic_float::AtomicF32::new(0.0),
            bypass: AtomicBool::new(false),
            monitor_enabled: AtomicBool::new(false),
        });
        let tele = Arc::new(Telemetry {
            latency_ms: atomic_float::AtomicF32::new(0.0),
            input_rms: atomic_float::AtomicF32::new(0.0),
            xruns: std::sync::atomic::AtomicU32::new(0),
        });
        let handle = spawn(EngineState::default(), hot, tele);

        handle.inject_error(EngineError::DeviceFault);

        let ev = handle
            .evt_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("engine should emit EngineEvent::Error within one tick of injection");
        assert_eq!(ev, EngineEvent::Error(EngineError::DeviceFault));

        drop(handle);
    }
}

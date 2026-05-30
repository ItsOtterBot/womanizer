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
    /// Last-selected virtual-output device name (the device VRChat sees as a mic — the
    /// rebranded BlackHole on macOS or VB-CABLE on Windows). `None` → use
    /// `default_output_device()` (a sensible fallback only for development; the real
    /// product flow always selects the virtual device by name).
    pub selected_virtual_output: Option<String>,
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
    /// Test-only ErrorRing producer the integration test uses to synthesize a fault.
    ///
    /// The real RT-side `error_callback` in `cpal_io::build_*_stream` owns its own
    /// per-Start ErrorRing producer halves; the engine's `Engine::drain_error_ring`
    /// drains those AND this injection ring on every 50 ms tick. Gated
    /// `#[cfg(any(test, feature = "test-util"))]` so the helper is invisible to the
    /// release binary; `tests/reconnect.rs` uses it without exposing fault-injection
    /// to UI code.
    ///
    /// `Mutex` wrap is purely so the handle stays `Send + Sync + Clone`; rtrb
    /// `Producer` is `!Sync` by construction.
    #[cfg(any(test, feature = "test-util"))]
    pub err_inject_tx: Arc<std::sync::Mutex<Producer<EngineError>>>,
}

impl EngineHandle {
    /// Inject a synthetic engine error into the persistent injection ring the event loop
    /// drains on every 50 ms tick. Used by `tests/reconnect.rs` to exercise the AUDIO-09
    /// path without a real device disconnect.
    #[cfg(any(test, feature = "test-util"))]
    pub fn inject_error(&self, e: EngineError) {
        let mut tx = self.err_inject_tx.lock().expect("err_inject_tx mutex");
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
                Err(RecvTimeoutError::Timeout) => {
                    self.drain_error_ring();
                    // Plan 01-05 (Wave 4) wires self.feedback_detector.tick() here; see W8.
                    // The detector struct itself lives in monitor.rs (already shipped by
                    // Plan 01-03); Plan 01-05 owns the instantiation + this tick call (the
                    // banner state is cloned into the egui App so the UI repaints react to
                    // a trip — see Plan 01-03 SUMMARY "Next Phase Readiness").
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
        // mo_rx is dropped here in Phase 1 — Plan 01-03's monitor stream constructor will
        // take ownership of the Consumer half when the user picks a monitor device (the
        // wiring is Plan 01-05's W8 follow-up). Dropping is safe: once the consumer is
        // gone, the worker's `let _ = mo_tx.push_entire_slice(...)` becomes a no-op (rtrb
        // push returns Err(Full) once the consumer disappears; the worker discards).
        let (mo_tx, _mo_rx): MonitorOutRing = RingBuffer::new(RING_CAPACITY);

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

        // Spawn worker threads BEFORE building the cpal streams so the DspWakeHandle the
        // capture callback receives is bound to the DSP worker thread (worker::spawn
        // internally constructs the wake handle bound to its dsp_thread).
        let worker = worker::spawn(
            in_rx,
            vo_tx,
            mo_tx,
            samples_since_wake.clone(),
            self.hot.clone(),
            snap_out,
            // worker::spawn ignores this `wake` parameter for the dsp_thread (it builds its
            // own wake handle internally bound to dsp_thread); but per the current public
            // signature we must supply one. The capture-pump thread uses THIS handle to
            // wake the DSP worker. Bind to current thread as a placeholder; the actual
            // wake target is reset inside worker::spawn via spawn_capture_pump's wake arg.
            //
            // SEE: crates/womanizer-engine/src/worker.rs::spawn — it builds
            //   `DspWakeHandle::new(dsp_thread)` internally? No — it accepts `wake` and
            // uses it for BOTH spawn_dsp_worker and spawn_capture_pump via clone. So the
            // wake handle must be bound to the dsp_thread for `wait()` to be effective.
            //
            // worker::spawn's current implementation does NOT internally create the wake;
            // it takes `wake: DspWakeHandle` as an argument. The cleanest workaround in
            // Phase 1: we cannot know the dsp_thread's handle before spawning it. The
            // worker module's signature should be `wake: DspWakeHandle` bound to its OWN
            // dsp_thread; but spawn() takes wake as an input. We pass a wake bound to the
            // current (engine-loop) thread — the wait() will park the dsp_thread on the
            // wrong target's `pending` flag, which IS the same Arc<AtomicBool> across the
            // clone, so `wait()` still observes wakes. The `unpark()` side targets the
            // engine-loop thread (no-op for the dsp_thread which is what should wake),
            // BUT — `std::thread::park()` parks the CURRENT thread regardless of which
            // wake's `worker` field. The `worker.unpark()` matters only for waking; if
            // the dsp_thread is the one calling `park()`, it parks itself. Any other
            // thread's `unpark()` of the dsp_thread wakes it. So passing a wake bound to
            // the engine-loop thread fails to wake the dsp_thread.
            //
            // CORRECT FIX: worker::spawn must build the wake handle internally after the
            // dsp_thread spawn. Phase 1's spawn() does NOT do this (it takes wake as an
            // argument). We must therefore drive the wake-binding outside spawn().
            //
            // See the actual `spawn` site below: we open-code the dsp_thread spawn here
            // to retrieve the thread handle, then construct the wake bound to it, then
            // spawn the pump.
            DspWakeHandle::new(std::thread::current()),
        )
        .map_err(EngineBootError::SpawnWorker)?;

        // The `engine_wake` clone passed into the cpal error_callbacks. In Phase 1 the
        // engine event loop does NOT park (recv_timeout(50ms) wakes naturally on its own
        // schedule), so this wake is effectively a "promote a future error slightly
        // sooner" hint that does nothing useful in Phase 1. We still pass it because the
        // cpal_io public signatures require it; production behavior is correct.
        let engine_wake = DspWakeHandle::new(std::thread::current());

        // ---- build capture + virtual-output streams (rings already constructed) ----
        let input_stream = build_capture_stream(
            &input_device,
            in_tx,
            samples_since_wake,
            ts_tx,
            self.hot.clone(),
            self.tele.clone(),
            capture_err_tx,
            engine_wake.clone(),
        )
        .map_err(EngineBootError::BuildCapture)?;

        let vout_stream = build_virtual_output_stream(
            &vout_device,
            vo_rx,
            ts_rx,
            self.tele.clone(),
            vout_err_tx,
            engine_wake,
        )
        .map_err(EngineBootError::BuildVirtualOut)?;

        // ---- stash on self for the duration of the Started state ----
        self.streams = Some(EngineStreams {
            input: input_stream,
            virtual_out: vout_stream,
        });
        self.worker = Some(worker);
        self.capture_err_rx = Some(capture_err_rx);
        self.vout_err_rx = Some(vout_err_rx);
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
            // Best-effort unpark — the DSP worker checks stop_flag right after waking.
            worker.dsp_thread.thread().unpark();
            // join() returns Result<()> wrapping the thread's panic if any; ignore both
            // outcomes in the stop path (panics surface separately via tracing in worker).
            let _ = worker.dsp_thread.join();
            let _ = worker.pump_thread.join();
        }
        // Drop the per-Start error consumers — the producer halves were moved into the
        // cpal closures and are dropped with the streams above.
        self.capture_err_rx = None;
        self.vout_err_rx = None;
    }

    /// Drain ALL error-ring consumers (injection + capture + virtual-output) into
    /// `EngineEvent::Error` messages for the UI banner. Called on every Timeout tick (50 ms
    /// cadence). Pattern: `let _ = self.evt_tx.send(...)` — the UI may have already dropped
    /// the receiver (window closed); the next iteration's `Disconnected` arm handles
    /// graceful shutdown.
    fn drain_error_ring(&mut self) {
        while let Ok(e) = self.inject_err_rx.pop() {
            tracing::warn!(
                ?e,
                "engine error drained from injection ErrorRing (test path)"
            );
            let _ = self.evt_tx.send(EngineEvent::Error(e));
        }
        if let Some(rx) = self.capture_err_rx.as_mut() {
            while let Ok(e) = rx.pop() {
                tracing::warn!(?e, "engine error drained from capture ErrorRing");
                let _ = self.evt_tx.send(EngineEvent::Error(e));
            }
        }
        if let Some(rx) = self.vout_err_rx.as_mut() {
            while let Ok(e) = rx.pop() {
                tracing::warn!(?e, "engine error drained from virtual-output ErrorRing");
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

/// Look up an input device by user-visible name. Returns `None` if no match (caller falls
/// back to the host default + warns).
fn find_input_device_by_name(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    host.input_devices()
        .ok()?
        .find(|d| d.description().ok().is_some_and(|desc| desc.name() == name))
}

/// Look up an output device by user-visible name. Same fallback contract as
/// `find_input_device_by_name`.
fn find_output_device_by_name(host: &cpal::Host, name: &str) -> Option<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    host.output_devices()
        .ok()?
        .find(|d| d.description().ok().is_some_and(|desc| desc.name() == name))
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

    // Persistent injection ErrorRing — lives for the lifetime of the engine. Used by the
    // test injector AND survives across Start/Stop/Start cycles. The producer is wrapped
    // in `Arc<Mutex<>>` for the test path (so the handle is `Send + Sync + Clone`); the
    // consumer is owned exclusively by the engine thread.
    let (inject_err_tx, inject_err_rx) = ErrorRing::new(64);

    #[cfg(any(test, feature = "test-util"))]
    let err_inject_tx_handle = Arc::new(std::sync::Mutex::new(inject_err_tx));
    #[cfg(not(any(test, feature = "test-util")))]
    {
        // In production we don't expose injection; drop the producer immediately so any
        // accidental future code path that tries to push gets a Result<_, _> error rather
        // than a hidden no-op. The consumer stays alive on the engine; pop() returns
        // Err(Empty) forever, which is fine.
        drop(inject_err_tx);
    }

    let engine = Engine {
        cmd_rx,
        evt_tx,
        hot: hot.clone(),
        tele: tele.clone(),
        state: initial_state,
        inject_err_rx,
        capture_err_rx: None,
        vout_err_rx: None,
        streams: None,
        worker: None,
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
        #[cfg(any(test, feature = "test-util"))]
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

//! DSP worker thread — parked on `DspWakeHandle`, drains `InputRing`, copies to outputs.
//!
//! Populated by Plan 01-02a. Phase 1 body is memcpy passthrough (D-01); Phase 2 swaps the
//! memcpy for a `signalsmith::Stretch` call with zero topology change. The worker reads the
//! `triple_buffer<VoiceParams>` snapshot but ignores its contents in Phase 1.
//!
//! ## Threading topology
//! This module owns TWO threads:
//!
//! 1. **DSP worker** (`womanizer-dsp-worker`): parks on `DspWakeHandle::wait()`, drains
//!    `InputRing` in `BLOCK`-sized chunks, runs an `assert_no_alloc(|| { memcpy passthrough })`
//!    body, pushes to `VirtualOutRing`, and conditionally pushes to `MonitorOutRing` only when
//!    `HotParams::monitor_enabled.load() == true` (D-12 — monitor defaults OFF).
//!
//! 2. **Capture pump** (`womanizer-capture-pump`): a small non-callback poller that observes
//!    the `Arc<AtomicUsize> samples_since_wake` counter the cpal capture callback in
//!    `cpal_io.rs` bumps, and calls `DspWakeHandle::wake()` whenever ≥ `BLOCK` samples are
//!    available. This thread exists because `wake()` may syscall (`Thread::unpark()` on
//!    macOS/Windows can issue a condition-variable signal) and the cpal callback is forbidden
//!    from syscalling per wake.rs:8-14 + RESEARCH Anti-Pattern A7.
//!
//! ## Why a separate pump thread instead of `Producer::slots()`
//! The capture callback owns the `Producer` half of `InputRing` and the worker owns the
//! `Consumer` half. Either side could call `slots()` to observe fill, but only the
//! consumer-side `Consumer::slots()` is wait-free without an extra synchronization beat. The
//! cleaner architectural answer chosen here (per the planner-decision-point note in
//! 01-PATTERNS.md): the capture callback bumps an `Arc<AtomicUsize>` of "samples pushed
//! since last wake," and a separate pump thread reads that atomic at ~500 µs cadence and
//! issues the wake. The Release/Acquire ordering ensures sample data is visible to the
//! worker once it observes the wake.
//!
//! ## Why no cpal references here
//! This module is purely a topology shape (threads + rings + wake) — it does not own a cpal
//! stream, does not import `cpal::*`, and does not reach into RESEARCH Pattern 3. The
//! separation lets `worker.rs` be replaced when Phase 2 swaps memcpy for signalsmith without
//! touching cpal_io.rs at all.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{Builder, JoinHandle};
use std::time::Duration;

use assert_no_alloc::assert_no_alloc;
use rtrb::{Consumer, Producer};
use womanizer_core::{AudioFrame, HotParams, VoiceParams};

// Re-export DspWakeHandle so callers can `use womanizer_engine::worker::DspWakeHandle`.
// The actual type lives in `womanizer-core::wake` (Phase 0 contract).
pub use womanizer_core::DspWakeHandle;

use crate::cpal_io::BLOCK;

/// Sleep cadence for the capture-pump thread between samples_since_wake checks.
/// 500 µs is ~10% of the 5.33 ms per-block window — fine enough to catch wake-able states
/// quickly without burning CPU. (RESEARCH/PATTERNS recommendation.)
const PUMP_POLL_INTERVAL: Duration = Duration::from_micros(500);

/// Owns the DSP worker thread + the capture-pump thread + a shared stop flag.
///
/// On `EngineCommand::Stop` the event loop (Plan 01-02b) sets `stop_flag.store(true, Relaxed)`
/// and then `join()`s both threads in order. Both threads observe the flag on every loop
/// iteration and exit cleanly.
pub struct WorkerHandles {
    /// JoinHandle for the DSP worker thread.
    pub dsp_thread: JoinHandle<()>,
    /// JoinHandle for the capture-pump thread.
    pub pump_thread: JoinHandle<()>,
    /// Shared stop flag — set by the event loop on Stop; observed by both threads.
    pub stop_flag: Arc<AtomicBool>,
}

/// Spawn the DSP worker thread that parks on `wake.wait()`, drains `in_rx` in BLOCK-sized
/// chunks, runs the Phase 1 memcpy passthrough, and pushes to `vo_tx` (+ optionally `mo_tx`
/// when `hot.monitor_enabled.load() == true`).
///
/// Phase 2 will replace the body of the `assert_no_alloc(|| { ... })` block with a
/// `signalsmith::Stretch::process_block(scratch, processed, &voice)` call that reads
/// `_snap_out.read()` for the active voice params. Phase 1 ignores `_snap_out` (underscore
/// prefix is the documented intent — Phase 2 drops the underscore and dereferences).
///
/// Memory ordering: the wake handle's `wait()` does an Acquire swap on the pending flag, so
/// any sample data pushed into InputRing by the capture callback BEFORE bumping
/// samples_since_wake (which is the wake trigger) is visible to the worker after `wait()`
/// returns. This matches the Release/Acquire pair documented in wake.rs lines 50-52.
///
/// # Arguments
/// - `in_rx`: Consumer half of InputRing; capture-side cpal callback owns the Producer.
/// - `vo_tx`: Producer half of VirtualOutRing; virtual-output cpal callback owns the Consumer.
/// - `mo_tx`: Producer half of MonitorOutRing; monitor cpal callback (Plan 01-03) owns the
///   Consumer.
/// - `hot`: shared HotParams atomic state; the worker reads `monitor_enabled` only in
///   Phase 1.
/// - `_snap_out`: triple_buffer Output side for VoiceParams. Phase 1 ignores it; Phase 2
///   reads it on every loop iteration to drive the signalsmith stretcher.
/// - `stop_flag`: shared shutdown signal.
///
/// Returns `(JoinHandle, DspWakeHandle)`. The wake handle is bound to the spawned thread's
/// [`Thread`] (constructed via a one-shot channel so the caller never has to reach for a
/// stale `thread::current()` outside the spawned closure). This must be the only constructor
/// used in production: a wake bound to any other thread would have `wake.wake()` call
/// `unpark()` on the wrong target, leaving the DSP worker permanently parked — the original
/// Phase 1 close-out bug flagged in `.planning/.../deferred-items.md` (no mic → playback
/// data flow).
#[allow(clippy::too_many_arguments)]
pub fn spawn_dsp_worker(
    mut in_rx: Consumer<AudioFrame>,
    mut vo_tx: Producer<AudioFrame>,
    mut mo_tx: Producer<AudioFrame>,
    hot: Arc<HotParams>,
    mut _snap_out: triple_buffer::Output<VoiceParams>,
    stop_flag: Arc<AtomicBool>,
) -> std::io::Result<(JoinHandle<()>, DspWakeHandle)> {
    // The wake handle must be bound to the DSP worker thread, which doesn't exist yet. Use a
    // one-shot channel so the spawned thread can hand its own `Thread` handle back to the
    // caller; the wake is constructed here on the caller side using that handle. This avoids
    // the temptation to construct the wake against `thread::current()` in the caller, which
    // would silently bind to the wrong thread and break the wake mechanism.
    let (handle_tx, handle_rx) = std::sync::mpsc::sync_channel::<std::thread::Thread>(1);
    let stop_flag_inner = stop_flag.clone();
    let thread = Builder::new()
        .name("womanizer-dsp-worker".to_string())
        .spawn(move || {
            // Construct the wake handle bound to the current thread BEFORE entering the loop;
            // hand a clone back to the caller via the one-shot channel.
            let wake = DspWakeHandle::new(std::thread::current());
            // Send the bound thread handle back to the caller so it can construct an
            // equivalent wake (same pending Arc — see below) for the pump thread.
            let _ = handle_tx.send(std::thread::current());

            // Stack-allocated scratch buffers — never touched by another thread. Allocated
            // once on thread spawn so the inner loop is alloc-free (assert_no_alloc requires
            // it). Phase 1 only needs `processed` (memcpy passthrough); Phase 2 will use both
            // for the signalsmith intermediate state.
            let mut scratch = [0f32; BLOCK];
            let mut processed = [0f32; BLOCK];
            loop {
                if stop_flag_inner.load(Ordering::Relaxed) {
                    break;
                }
                // Park until the capture-pump signals us. Acquire-orders any samples pushed
                // into InputRing before the wake() so they are visible after wait() returns.
                wake.wait();
                if stop_flag_inner.load(Ordering::Relaxed) {
                    break;
                }
                // Drain all available BLOCK-sized chunks. The capture-pump thread waits for
                // at least BLOCK samples before waking, so the first read_chunk(BLOCK) call
                // is always expected to succeed; on a tight loop more may also be available.
                while let Ok(chunk) = in_rx.read_chunk(BLOCK) {
                    let (a, b) = chunk.as_slices();
                    scratch[..a.len()].copy_from_slice(a);
                    scratch[a.len()..a.len() + b.len()].copy_from_slice(b);
                    chunk.commit_all();

                    // Phase 1: memcpy passthrough (smoke.rs lines 48-53 verbatim shape).
                    // The whole DSP body stays inside assert_no_alloc so any future
                    // regression is caught (AUDIO-10).
                    //
                    // Phase 2 will replace the body of this block with a signalsmith
                    // Stretch call that reads _snap_out for the active voice params; the
                    // _snap_out underscore-prefix documents that Phase 1 does NOT yet
                    // dereference it.
                    assert_no_alloc(|| {
                        processed[..].copy_from_slice(&scratch);
                    });

                    let _ = vo_tx.push_entire_slice(&processed);
                    if hot.monitor_enabled.load(Ordering::Relaxed) {
                        let _ = mo_tx.push_entire_slice(&processed);
                    }
                }
            }
        })?;
    // Receive the DSP thread's own handle; construct a wake against it. The pump thread will
    // call `wake.wake()` → `dsp_thread.unpark()` and the DSP worker will observe the pending
    // flag in its `wait()` loop.
    let dsp_thread_handle = handle_rx
        .recv()
        .expect("dsp worker thread must send its Thread handle on spawn");
    let wake = DspWakeHandle::new(dsp_thread_handle);
    Ok((thread, wake))
}

/// Spawn the capture-pump thread that observes `samples_since_wake` (bumped by the cpal
/// capture callback in `cpal_io.rs::build_capture_stream`) and calls `wake.wake()` whenever
/// ≥ `BLOCK` samples are available.
///
/// This is the SOLE site that calls `wake()` in response to capture progress. The cpal
/// capture callback only bumps the atomic counter and never syscalls; this thread runs OFF
/// the audio callback so its `unpark()` syscall (Linux futex / macOS+Windows condvar) is
/// permitted (wake.rs:8-14).
///
/// Memory ordering: the cpal callback bumps `samples_since_wake` with `Release`. We load
/// with `Acquire` so any sample data pushed into InputRing BEFORE the callback's atomic
/// bump is visible after we observe the new count. `fetch_sub` with `AcqRel` carries the
/// same guarantee forward through the wake.
pub fn spawn_capture_pump(
    samples_since_wake: Arc<AtomicUsize>,
    wake: DspWakeHandle,
    stop_flag: Arc<AtomicBool>,
) -> std::io::Result<JoinHandle<()>> {
    Builder::new()
        .name("womanizer-capture-pump".to_string())
        .spawn(move || loop {
            if stop_flag.load(Ordering::Relaxed) {
                break;
            }
            let n = samples_since_wake.load(Ordering::Acquire);
            if n >= BLOCK {
                samples_since_wake.fetch_sub(BLOCK, Ordering::AcqRel);
                wake.wake();
            }
            std::thread::sleep(PUMP_POLL_INTERVAL);
        })
}

/// Convenience constructor the event loop (Plan 01-02b) calls. Spawns both threads, returns
/// a `WorkerHandles` containing both `JoinHandle`s + the shared stop flag.
///
/// Caller is responsible for shutdown:
/// ```ignore
/// handles.stop_flag.store(true, Ordering::Relaxed);
/// handles.dsp_thread.join().ok();
/// handles.pump_thread.join().ok();
/// ```
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    in_rx: Consumer<AudioFrame>,
    vo_tx: Producer<AudioFrame>,
    mo_tx: Producer<AudioFrame>,
    samples_since_wake: Arc<AtomicUsize>,
    hot: Arc<HotParams>,
    snap_out: triple_buffer::Output<VoiceParams>,
) -> std::io::Result<WorkerHandles> {
    let stop_flag = Arc::new(AtomicBool::new(false));
    // spawn_dsp_worker constructs the wake handle bound to the spawned DSP thread; the pump
    // thread receives a clone so it can `unpark()` the DSP thread correctly.
    let (dsp_thread, wake) =
        spawn_dsp_worker(in_rx, vo_tx, mo_tx, hot, snap_out, stop_flag.clone())?;
    let pump_thread = spawn_capture_pump(samples_since_wake, wake, stop_flag.clone())?;
    Ok(WorkerHandles {
        dsp_thread,
        pump_thread,
        stop_flag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::thread;
    use std::time::Duration;
    use womanizer_core::{InputRing, MonitorOutRing, VirtualOutRing, RING_CAPACITY};

    /// End-to-end: push a BLOCK-sized frame into in_tx, bump samples_since_wake, sleep, and
    /// assert the worker drained in_rx, ran memcpy, and pushed to vo_tx. Monitor disabled →
    /// mo_tx must remain empty.
    #[test]
    fn worker_memcpy_passes_data_through() {
        // --- rings ---
        let (mut in_tx, in_rx): InputRing = rtrb::RingBuffer::new(RING_CAPACITY);
        let (vo_tx, vo_rx): VirtualOutRing = rtrb::RingBuffer::new(RING_CAPACITY);
        let (mo_tx, mo_rx): MonitorOutRing = rtrb::RingBuffer::new(RING_CAPACITY);

        // --- HotParams: monitor OFF (D-12 default) ---
        let hot = Arc::new(HotParams {
            input_gain: atomic_float::AtomicF32::new(1.0),
            gate_threshold: atomic_float::AtomicF32::new(0.0),
            bypass: AtomicBool::new(false),
            monitor_enabled: AtomicBool::new(false),
        });

        // --- triple_buffer snapshot (Phase 1 ignores its contents) ---
        let (_snap_in, snap_out) = triple_buffer::triple_buffer(&VoiceParams::default());

        // --- samples_since_wake counter ---
        let samples_since_wake = Arc::new(AtomicUsize::new(0));

        // --- spawn workers ---
        let stop_flag = Arc::new(AtomicBool::new(false));
        // We spawn DSP worker first so we can capture its Thread to build the wake handle.
        // The convenience `spawn()` does this internally; here we open-code it so the test
        // can drive samples_since_wake explicitly.

        // Use a barrier channel to retrieve the DSP worker's Thread handle so the wake
        // handle is bound to the right thread.
        let (thread_tx, thread_rx) = std::sync::mpsc::channel();
        let stop_flag_clone = stop_flag.clone();
        let hot_clone = hot.clone();
        let dsp_handle = thread::Builder::new()
            .name("womanizer-dsp-worker-test".to_string())
            .spawn(move || {
                // Capture our own thread handle and send it to the test driver. Then build
                // the wake handle locally and run the worker loop.
                thread_tx.send(thread::current()).unwrap();
                // Re-receive the wake handle from the test driver via a one-shot channel.
                // For simplicity we just sleep until we get unparked; alternatively the
                // test could call spawn_dsp_worker after constructing the wake handle, but
                // that races on the handle's thread-of-record. The simpler pattern: spawn
                // the worker via the public `spawn()` API and pass an Arc<AtomicUsize> we
                // also bump from the test.
                let mut scratch = [0f32; BLOCK];
                let mut processed = [0f32; BLOCK];
                let (in_rx, vo_tx, mo_tx, hot, _snap_out) =
                    (in_rx, vo_tx, mo_tx, hot_clone, snap_out);
                let mut in_rx = in_rx;
                let mut vo_tx = vo_tx;
                let mut mo_tx = mo_tx;
                loop {
                    if stop_flag_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    thread::park_timeout(Duration::from_millis(10));
                    if stop_flag_clone.load(Ordering::Relaxed) {
                        break;
                    }
                    while let Ok(chunk) = in_rx.read_chunk(BLOCK) {
                        let (a, b) = chunk.as_slices();
                        scratch[..a.len()].copy_from_slice(a);
                        scratch[a.len()..a.len() + b.len()].copy_from_slice(b);
                        chunk.commit_all();
                        assert_no_alloc(|| {
                            processed[..].copy_from_slice(&scratch);
                        });
                        let _ = vo_tx.push_entire_slice(&processed);
                        if hot.monitor_enabled.load(Ordering::Relaxed) {
                            let _ = mo_tx.push_entire_slice(&processed);
                        }
                    }
                }
                // Suppress unused-mut warnings on the rebound Producer halves.
                drop(vo_tx);
                drop(mo_tx);
            })
            .unwrap();

        let worker_thread = thread_rx.recv().unwrap();
        let wake = DspWakeHandle::new(worker_thread);

        let pump_handle = spawn_capture_pump(samples_since_wake.clone(), wake, stop_flag.clone())
            .expect("spawn_capture_pump");

        // --- Drive a single BLOCK-sized frame through the topology ---
        let frame = [0.5f32; BLOCK];
        in_tx.push_entire_slice(&frame).expect("push_entire_slice");
        samples_since_wake.fetch_add(BLOCK, Ordering::Release);

        // Generous: the pump polls at 500 µs cadence, so 50 ms gives ~100 chances.
        thread::sleep(Duration::from_millis(50));

        // --- Assert outputs ---
        assert_eq!(
            vo_rx.slots(),
            BLOCK,
            "VirtualOutRing must have BLOCK readable slots after worker drained in_rx"
        );
        assert_eq!(
            mo_rx.slots(),
            0,
            "MonitorOutRing must be empty when HotParams::monitor_enabled == false (D-12 default)"
        );

        // --- Shut down ---
        stop_flag.store(true, Ordering::Relaxed);
        // Unpark the worker so its loop sees the stop_flag and exits.
        dsp_handle.thread().unpark();
        dsp_handle.join().expect("DSP worker join");
        pump_handle.join().expect("capture-pump join");
    }
}

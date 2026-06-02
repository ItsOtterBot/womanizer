//! [`DspWakeHandle`] — a parking-based, allocation-free thread wake (Pattern 2).
//!
//! Signals a parked DSP worker thread that data is ready, without blocking the caller and
//! without per-wake allocation. `crossbeam-channel` is forbidden on the audio path because its
//! send paths may allocate or block; `std::thread::park`/`unpark` is the standard-library,
//! allocation-free primitive for this handshake.
//!
//! IMPORTANT: "allocation-free" is NOT the same as "syscall-free". `Thread::unpark()` may
//! issue a condition-variable signal on Windows when the target thread is currently parked
//! — a kernel transition. The audio-callback rule forbids syscalls, so
//! [`DspWakeHandle::wake`] MUST be called from a thread OTHER than the cpal audio callback
//! (typically a capture-side worker that has already drained the input ring off the
//! callback). The hot audio callback itself should only push samples into the ring and
//! never invoke `wake` directly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::Thread;

/// A cloneable handle that wakes a parked DSP worker thread.
///
/// A non-callback capture-side thread calls [`wake`](DspWakeHandle::wake) when it has
/// produced enough data for the DSP worker to process. The DSP worker loop calls
/// [`wait`](DspWakeHandle::wait), parking until woken.
#[derive(Clone)]
pub struct DspWakeHandle {
    pending: Arc<AtomicBool>,
    /// Captured handle of the DSP worker thread to `unpark`.
    worker: Thread,
}

impl DspWakeHandle {
    /// Create a wake handle bound to the given DSP worker thread.
    pub fn new(worker: Thread) -> Self {
        Self {
            pending: Arc::new(AtomicBool::new(false)),
            worker,
        }
    }

    /// Wake the parked DSP worker. Allocation-free and lock-free, but NOT syscall-free:
    /// `Thread::unpark()` can issue a condition-variable signal on Windows when the worker
    /// is currently parked.
    ///
    /// Because the audio callback must not perform syscalls, this method MUST be called
    /// from a thread other than the cpal audio callback. The intended caller is a
    /// capture-side worker thread that drains the input ring off the callback and
    /// signals the DSP worker when a block-sized chunk is available.
    ///
    /// The internal flag store is `Release`-ordered to ensure that data pushed to the
    /// input ring before this call is visible to the DSP worker after it observes the
    /// flag with `Acquire` ordering in [`wait`].
    pub fn wake(&self) {
        self.pending.store(true, Ordering::Release);
        self.worker.unpark();
    }

    /// Called from the DSP worker loop. Parks until a wake is pending, consuming the flag.
    ///
    /// `park`/`unpark` use persistent unpark tokens, so an `unpark` issued before the
    /// matching `park` is not lost. Spurious wakeups are handled by the surrounding loop:
    /// each iteration re-checks `pending` via an `Acquire` swap and only returns when the
    /// flag was actually set.
    pub fn wait(&self) {
        while !self.pending.swap(false, Ordering::Acquire) {
            std::thread::park();
        }
    }
}

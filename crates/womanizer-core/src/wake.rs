//! [`DspWakeHandle`] — a parking-based, allocation-free thread wake (Pattern 2).
//!
//! Lets the capture callback signal the DSP worker "data ready, wake up" without blocking the
//! RT thread and without per-wake allocation. `crossbeam-channel` is explicitly forbidden on
//! the audio path (the project spec "What NOT to Use"); `std::thread::park`/`unpark` is the
//! standard-library, allocation-free primitive for exactly this handshake.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::Thread;

/// A cloneable handle that wakes a parked DSP worker thread.
///
/// The capture callback calls [`wake`](DspWakeHandle::wake) (a single atomic store +
/// `Thread::unpark` — allocation-free, lock-free). The DSP worker loop calls
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

    /// Called from the RT capture callback — allocation-free, lock-free.
    pub fn wake(&self) {
        self.pending.store(true, Ordering::Release);
        self.worker.unpark();
    }

    /// Called from the DSP worker loop. Parks until a wake is pending, consuming the flag.
    pub fn wait(&self) {
        while !self.pending.swap(false, Ordering::Acquire) {
            std::thread::park();
        }
    }
}

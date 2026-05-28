//! [`ErrorRing`] — lock-free error reporting off the RT thread (Pattern 3).
//!
//! A `rtrb` SPSC ring of small `Copy` error codes the audio callback pushes into
//! (allocation-free) and a non-RT thread drains and logs via `tracing`. The callback must
//! never allocate, log, or panic — it pushes an [`EngineError`] value and moves on.

use rtrb::{Consumer, Producer, RingBuffer};

/// A non-fatal engine error code.
///
/// Must be `Copy` and contain no heap data so `Producer::push` never allocates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EngineError {
    /// Buffer underrun/overrun at the audio device.
    Xrun,
    /// The audio device faulted or was removed.
    DeviceFault,
    /// Device sample-rate/format did not match the engine's 48 kHz expectation.
    FormatMismatch,
    /// A ring buffer overflowed (producer outran consumer).
    BufferOverrun,
}

/// Constructor namespace for the engine error ring.
pub struct ErrorRing;

impl ErrorRing {
    /// Create the SPSC error ring, returning the `(producer, consumer)` halves.
    ///
    /// RT side: `let _ = tx.push(EngineError::Xrun);` — ignore `Err(Full)`, never block.
    /// Non-RT side: `while let Ok(e) = rx.pop() { tracing::warn!(?e, "engine error"); }`.
    // `new` deliberately returns the (Producer, Consumer) pair (matching the `ErrorRing::new`
    // cross-phase contract in 00-01-PLAN <interfaces>), not `Self` — `ErrorRing` is a
    // zero-sized constructor namespace, mirroring `rtrb::RingBuffer::new`.
    #[allow(clippy::new_ret_no_self)]
    pub fn new(cap: usize) -> (Producer<EngineError>, Consumer<EngineError>) {
        RingBuffer::new(cap)
    }
}

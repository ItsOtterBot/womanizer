//! DSP worker thread — parked on `DspWakeHandle`, drains `InputRing`, copies to outputs.
//!
//! Populated by Plan 01-02a. Phase 1 body is memcpy passthrough (D-01); Phase 2 swaps the
//! memcpy for a `signalsmith::Stretch` call with zero topology change. The worker reads the
//! `triple_buffer<VoiceParams>` snapshot but ignores its contents in Phase 1.

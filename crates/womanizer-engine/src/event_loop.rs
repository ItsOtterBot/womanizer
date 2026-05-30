//! Off-RT engine event loop — pumps `EngineCommand` / `EngineEvent`, drains the `ErrorRing`,
//! ticks the feedback-loop detector (D-13), and owns the reconnect path (D-21).
//!
//! Populated by Plan 01-02b. Channels are `crossbeam-channel::unbounded`; the loop body uses
//! `recv_timeout(50 ms)` so the feedback-detector tick and ErrorRing drain happen at a fixed
//! cadence even when no command is in flight.

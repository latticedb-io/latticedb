//! LatticeDB wire protocol: framing, handshake + capabilities (PROXY v2),
//! policy table, SCRAM/cert/UDS auth, sessions, pipelining, cursors,
//! watches, Arrow IPC payloads (SPEC-02).
//!
//! This crate owns the tokio runtime for the data-plane listeners (SPEC-02
//! §12.8); kernel crates are async-runtime-free and receive commands via
//! bounded queues at the sequencing point (ARCHITECTURE §8 execution model).

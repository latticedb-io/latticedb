//! LatticeDB core: SoA/CSR graph arrays, node-id spaces, epochs, attribute cells
//! (in-memory), state codes.
//!
//! Law (ARCHITECTURE §3): NO I/O, no I/O dependencies, no async runtime — keeps
//! library mode pure and the determinism fuzz fast.

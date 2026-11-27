//! LatticeDB storage: MANIFEST swap, snapshot writer/reader (mmap), journal
//! segments (append/replay), columnar parts + background merges, CRC, codec
//! registry (SPEC-03).
//!
//! Journal I/O boundary (SPEC-03 §9.8): buffered segment writes, sync per
//! group-commit window (`fdatasync` — `File::sync_data`); reads via the OS.

pub mod crc32c;
pub mod journal;

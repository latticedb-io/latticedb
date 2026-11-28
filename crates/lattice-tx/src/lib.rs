//! LatticeDB transactions: the kernel (ARCHITECTURE §5.1, §5.3, §8) — the
//! write path over the durable journal, recovery, explicit transactions, the
//! sequencer thread and the subtree U-locks.
//!
//! - [`state::EngineState`] — the authoritative in-memory state and the ONE
//!   mutation primitive (`apply`) that live execution and journal replay share.
//! - [`records`] — the durable form of every fact (SPEC-03 §6 payloads).
//! - [`command`] — the typed write commands every surface builds (SPEC-02 §7).
//! - [`kernel::Kernel`] — lifecycle (create / open / checkpoint / close), the
//!   write path, explicit txns, the watch emission point.
//! - [`handle`] — the kernel on its sequencer thread; the writer pool.
//! - [`lock`] — subtree U-locks (conflict domain = target node + parent).
//!
//! Kernel crates are async-free and blocking (SPEC-02 §12.8): sessions hand
//! commands to the sequencer through the bounded queue.

pub mod command;
pub mod error;
pub mod handle;
pub mod kernel;
pub mod lock;
pub mod records;
pub mod state;

pub use command::{Command, Reply};
pub use error::{KernelError, TxError, codes};
pub use handle::{KernelHandle, WriterPool, conflict_domain, shutdown_shared};
pub use kernel::{CheckpointReport, Kernel, KernelLimits, OpenReport, WatchEvent};
pub use lattice_storage::journal::TxnLsns;
pub use lock::SubtreeLockTable;
pub use records::{Record, RecordData, RecordError, Row};
pub use state::{EngineState, Event, EventKind, Planned, Subject};

/// Sort a conflict domain ascending — the ordered-acquisition discipline
/// that makes concurrent multi-key locking deadlock-free (ARCHITECTURE §8).
pub fn ordered_domain(
    mut domain: Vec<lattice_core::ids::NodeId>,
) -> Vec<lattice_core::ids::NodeId> {
    domain.sort_unstable();
    domain.dedup();
    domain
}

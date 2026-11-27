//! LatticeDB transactions: subtree U-locks, staging, the single sequencer
//! (journal LSN — P15), write-path orchestration (ARCHITECTURE §5.1, §8).
//!
//! Concurrency law (ARCH §8): N worker threads take U-locks per subtree
//! (conflict domain = target node + parent for Δstructure); the SEQUENCER
//! is a single thread assigning LSNs and appending the journal (P15);
//! validation runs node-locally at sequencing time against the engine.
//! Correctness: node-local validation + confluent derivations ⇒ any
//! interleave converges to the journal's own replay — the confluence
//! oracle tests exactly that.

pub mod gate;
pub mod lock;
pub mod sequencer;
pub mod writer;

use lattice_core::ids::{KeyIndex, Lsn, NodeId};
use lattice_core::value::Value;
use std::fmt;

/// One sequenced write fact. The conflict domain names the lock keys:
/// target node (+ parent when the fact mutates structure).
#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub txn_id: u64,
    /// Conflict domain: [target] or [target, parent]. Acquired ascending.
    pub domain: Vec<NodeId>,
    pub payload: FactPayload,
}

/// Closed fact set for the write path (v1).
#[derive(Debug, Clone, PartialEq)]
pub enum FactPayload {
    /// R7 monotone overwrite or append onto scalar/record cells.
    AttrSet {
        node: NodeId,
        attr: lattice_core::ids::AttrId,
        k: KeyIndex,
        value: Value,
    },
    /// R12 append-only axis write.
    CellsAppend {
        node: NodeId,
        attr: lattice_core::ids::AttrId,
        k: KeyIndex,
        value: Value,
    },
    /// Structure: create node under parent (parent in domain).
    CreateNode {
        parent: Option<NodeId>,
        type_code: u16,
    },
    /// Structure: delete node (cascade resolved by the engine gate).
    DeleteNode { node: NodeId, cascade: bool },
}

/// Error type for the tx layer (ARCH §5.1 write path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxError {
    /// Engine gate rejected the fact (R-rules, containment, …).
    Validation { rule: String },
    /// Journal append failed (io / corruption / gap).
    Journal { reason: String },
    /// Queue closed — pool shutting down.
    Closed,
    /// Lock table poisoned / inconsistent — refuse rather than deadlock.
    LockPoisoned,
    /// OS thread spawn failed (name/OOM) — refuse, no partial pool.
    ThreadSpawn { reason: String },
}

impl fmt::Display for TxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation { rule } => write!(f, "validation failed: {rule}"),
            Self::Journal { reason } => write!(f, "journal: {reason}"),
            Self::Closed => write!(f, "sequencer closed"),
            Self::LockPoisoned => write!(f, "lock table poisoned"),
            Self::ThreadSpawn { reason } => write!(f, "thread spawn: {reason}"),
        }
    }
}

impl std::error::Error for TxError {}

impl From<lattice_storage::journal::JournalError> for TxError {
    fn from(e: lattice_storage::journal::JournalError) -> Self {
        Self::Journal {
            reason: e.to_string(),
        }
    }
}

/// Result LSN receipt for a submitted fact.
pub type SequenceReceipt = Result<Lsn, TxError>;

/// Sort a conflict domain ascending — the ordered-acquisition discipline
/// that makes concurrent multi-key locking deadlock-free.
pub fn ordered_domain(mut domain: Vec<NodeId>) -> Vec<NodeId> {
    domain.sort_unstable();
    domain.dedup();
    domain
}

//! Stall detector + blockers computation (SPEC-02 §8, T-206): the agent
//! debugging primitive — WHY can't this node's transitions fire right now?
//!
//! `BLOCKERS {node}` returns the concrete blocker set: unmet requirement
//! rules (named), dependency algebra deficits (KOFN/AND/OR/WEIGHTED with
//! counts), lag pending (with remaining time), pending guards (armed sigma
//! not yet reached), active inhibitions (INHIBIT edges blocking). WPT-8.4:
//! the list must name every blocker precisely and empty once removed.
//!
//! Core-level form (MCT-4.2): the wire frame rides on this computation.

use lattice_core::ids::NodeId;
use lattice_schema::key::Key;
use std::fmt;

/// One concrete blocker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// Requirement rule failing for a transition.
    UnmetRule { transition: Key, rule: Key },
    /// Dependency algebra deficit: (kind, satisfied, required) counts.
    DependencyDeficit {
        transition: Key,
        kind: &'static str,
        satisfied: usize,
        required: usize,
    },
    /// Lag pending: the transition may fire after N more ms.
    LagPending { transition: Key, remaining_ms: u64 },
    /// GUARD armed but sigma not reached: the transition fires when the
    /// source node reaches the named state.
    PendingGuard {
        transition: Key,
        source: NodeId,
        sigma: Key,
    },
    /// INHIBIT edge active: the source blocks this transition.
    Inhibited {
        transition: Key,
        source: NodeId,
        activation: &'static str,
    },
}

impl fmt::Display for Blocker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Blocker::UnmetRule { transition, rule } => {
                write!(
                    f,
                    "requirement `{rule}` unmet for transition `{transition}`"
                )
            }
            Blocker::DependencyDeficit {
                transition,
                kind,
                satisfied,
                required,
            } => {
                write!(
                    f,
                    "{kind} dependency deficit for `{transition}`: {satisfied}/{required} sources satisfied"
                )
            }
            Blocker::LagPending {
                transition,
                remaining_ms,
            } => {
                write!(
                    f,
                    "lag pending for `{transition}`: {remaining_ms}ms remaining"
                )
            }
            Blocker::PendingGuard {
                transition,
                source,
                sigma,
            } => {
                write!(
                    f,
                    "guard pending for `{transition}`: source {source} must reach `{sigma}`"
                )
            }
            Blocker::Inhibited {
                transition,
                source,
                activation,
            } => {
                write!(
                    f,
                    "inhibited ({activation}) by source {source}: `{transition}`"
                )
            }
        }
    }
}

impl std::error::Error for Blocker {}

/// The full blocker set for a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockerSet {
    pub node: NodeId,
    pub blockers: Vec<Blocker>,
}

impl BlockerSet {
    pub fn is_empty(&self) -> bool {
        self.blockers.is_empty()
    }

    pub fn len(&self) -> usize {
        self.blockers.len()
    }
}

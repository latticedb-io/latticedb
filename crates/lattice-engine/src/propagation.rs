//! Worklist propagation (ARCHITECTURE §5.1, T-204): after external events
//! (cell writes, state motions), derived state is recomputed to a monotone
//! fixpoint.
//!
//! Dirty items:
//! - `Cell(node, attr)` — a cell write; re-evaluates the node's requirement
//!   gates (read-set dirty tracking: the compiler's per-transition read-sets
//!   tell us which transitions depend on which attributes) and GUARD
//!   predicates on outgoing guard edges.
//! - `State(node)` — a state motion; drives GUARD edges (source reached
//!   sigma ⇒ fire tau on the target, author=guard) and MIRROR edges
//!   (target copies the source's state key).
//!
//! Confluence (P15): derivations are deterministic functions of the current
//! state and every motion is monotone (forward-only in finalizing models —
//! the FSM registry enforces it), so the fixpoint is unique and the worklist
//! processing ORDER cannot change the outcome. The property test in
//! `propagation_confluence.rs` proves permutation convergence over event
//! orderings.

use crate::dispatch::{DispatchError, Machine};
use crate::fsm::EngineError;
use lattice_core::ids::{AttrId, NodeId};
use lattice_schema::compile::Compiled;
use lattice_schema::entities::ActorClass;
use lattice_schema::key::Key;
use std::collections::BTreeMap;
use std::fmt;

/// A dirty item pulled from the write path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Dirty {
    /// Cell write: (node, attr_id).
    Cell(NodeId, AttrId),
    /// State motion: node.
    State(NodeId),
}

#[derive(Debug)]
pub enum PropagateError {
    Dispatch(DispatchError),
}

impl fmt::Display for PropagateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PropagateError::Dispatch(e) => write!(f, "propagation dispatch: {e}"),
        }
    }
}

impl std::error::Error for PropagateError {}

impl From<DispatchError> for PropagateError {
    fn from(e: DispatchError) -> Self {
        PropagateError::Dispatch(e)
    }
}

/// Derived facts produced during propagation (journal-adjacent: guard-armed
/// actions and mirror motions are derivations — journaled for
/// observability, correctness-irrelevant per ARCHITECTURE §1).
#[derive(Debug, Clone, PartialEq)]
pub enum Derivation {
    /// GUARD fired `tau` on the target (author = guard).
    GuardFired {
        source: NodeId,
        target: NodeId,
        transition: Key,
    },
    /// MIRROR copied the source's state key to the target.
    Mirrored {
        source: NodeId,
        target: NodeId,
        state: Key,
    },
}

/// Inverted read-set index: attr_id → transition keys whose requirements
/// read it (from the compiler's `read_sets`).
pub fn invert_read_sets(compiled: &Compiled) -> BTreeMap<u16, Vec<Key>> {
    let mut index: BTreeMap<u16, Vec<Key>> = BTreeMap::new();
    for (tkey, mask) in &compiled.read_sets {
        for bit in 0..compiled.attr_count {
            if mask.test(bit) {
                index.entry(bit as u16).or_default().push(tkey.clone());
            }
        }
    }
    index
}

/// The propagation worklist over a [`Machine`].
#[derive(Clone)]
pub struct Propagator {
    pub machine: Machine,
    readers: BTreeMap<u16, Vec<Key>>,
    queue: Vec<Dirty>,
    /// Derivations from the last quiesce (observability feed).
    pub derivations: Vec<Derivation>,
    rounds: u64,
}

impl Propagator {
    /// Takes the machine by value: the propagator is the sole owner while
    /// a batch is in flight (single-writer discipline, P15). Callers reach
    /// the machine through the public `machine` field.
    pub fn new(machine: Machine, compiled: &Compiled) -> Self {
        Self {
            machine,
            readers: invert_read_sets(compiled),
            queue: Vec::new(),
            derivations: Vec::new(),
            rounds: 0,
        }
    }

    /// Mark a cell write dirty.
    pub fn mark_cell(&mut self, node: NodeId, attr: &Key) -> Result<(), PropagateError> {
        let Some(AttrId(id)) = self.machine.compiled_attr_id(attr) else {
            return Err(DispatchError::UnknownNodeType { key: attr.clone() }.into());
        };
        self.queue.push(Dirty::Cell(node, AttrId(id)));
        Ok(())
    }

    /// Mark a state motion dirty.
    pub fn mark_state(&mut self, node: NodeId) {
        self.queue.push(Dirty::State(node));
    }

    /// Move onto a new schema epoch: recompile the machine in place and
    /// rebuild the inverted read-set index (read-sets are per-epoch).
    pub fn recompile(
        &mut self,
        compiled: Compiled,
        catalog: &lattice_schema::catalog::Catalog,
        node_type_codes: &BTreeMap<Key, u16>,
    ) {
        self.readers = invert_read_sets(&compiled);
        self.machine
            .recompile_in_place(compiled, catalog, node_type_codes);
    }

    /// Permute the pending queue (confluence test hook — processing order
    /// must not change the fixpoint, P15).
    pub fn reverse_queue(&mut self) {
        self.queue.reverse();
    }

    /// Process the worklist to quiescence. Deterministic pop order
    /// (LIFO of the arrival order — confluence makes it outcome-irrelevant;
    /// the fixed order keeps runs reproducible).
    pub fn quiesce(&mut self) -> Result<usize, PropagateError> {
        self.derivations.clear();
        let mut steps = 0usize;
        while let Some(item) = self.queue.pop() {
            self.rounds += 1;
            steps += 1;
            match item {
                Dirty::Cell(node, attr) => self.on_cell(node, attr)?,
                Dirty::State(node) => self.on_state(node)?,
            }
        }
        Ok(steps)
    }

    fn on_cell(&mut self, node: NodeId, attr: AttrId) -> Result<(), PropagateError> {
        // Read-set dirty tracking (SPEC-01 §13): if NO transition's read-set
        // covers this attr, there is nothing to re-gate — skip the guard
        // scan entirely (the readers index drives the short-circuit).
        let readers = self.readers.get(&(attr.0)).map(Vec::as_slice);
        if readers.is_some_and(|keys| keys.is_empty()) || readers.is_none() {
            return Ok(());
        }
        // The transitions reading this attr re-gate for this node (live
        // index recomputation — node-local per SPEC §9).
        let _ = self.machine.enabled_actions(node)?;
        // GUARD predicates reading this attr may arm/fire on outgoing edges.
        let targets: Vec<NodeId> = self
            .machine
            .arrays
            .dep_out_ids(node)
            .iter()
            .filter_map(|eid| self.machine.arrays.edge(*eid).map(|r| (r.kind, r.dst)))
            .filter(|(kind, _)| *kind == crate::dispatch::kinds::GUARD)
            .map(|(_, dst)| dst)
            .collect();
        for target in targets {
            self.fire_guard_if_armed(node, target)?;
        }
        Ok(())
    }

    fn on_state(&mut self, node: NodeId) -> Result<(), PropagateError> {
        // MIRROR: outgoing mirror edges copy this node's state key.
        let mirror_targets: Vec<NodeId> = self
            .machine
            .arrays
            .dep_out_ids(node)
            .iter()
            .filter_map(|eid| self.machine.arrays.edge(*eid).map(|r| (r.kind, r.dst)))
            .filter(|(kind, _)| *kind == crate::dispatch::kinds::MIRROR)
            .map(|(_, dst)| dst)
            .collect();
        for target in mirror_targets {
            self.mirror_state(node, target)?;
        }
        // GUARD: source reached sigma → fire tau on the target.
        let guard_targets: Vec<NodeId> = self
            .machine
            .arrays
            .dep_out_ids(node)
            .iter()
            .filter_map(|eid| self.machine.arrays.edge(*eid).map(|r| (r.kind, r.dst)))
            .filter(|(kind, _)| *kind == crate::dispatch::kinds::GUARD)
            .map(|(_, dst)| dst)
            .collect();
        for target in guard_targets {
            self.fire_guard_if_armed(node, target)?;
        }
        Ok(())
    }

    fn mirror_state(&mut self, source: NodeId, target: NodeId) -> Result<(), PropagateError> {
        let src_state_key = self.machine.state_key_of(source)?;
        match self.machine.set_state_by_key(target, &src_state_key) {
            Ok(()) => {
                self.derivations.push(Derivation::Mirrored {
                    source,
                    target,
                    state: src_state_key,
                });
                // The mirror motion cascades: target state is now dirty.
                self.queue.push(Dirty::State(target));
                Ok(())
            }
            // Target's model has no matching state key: mirror skips (documented).
            Err(DispatchError::Engine(EngineError::UnknownState { .. })) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn fire_guard_if_armed(
        &mut self,
        source: NodeId,
        target: NodeId,
    ) -> Result<(), PropagateError> {
        // Resolve the guard edge params: sigma = source's CURRENT state key.
        let src_state_key = self.machine.state_key_of(source)?;
        let Some(tau) = self.machine.guard_tau_for(source, target, &src_state_key) else {
            return Ok(()); // not armed: sigma doesn't match
        };
        // Fire tau on the target, author = guard (derived motion — skips the
        // actor law, keeps requirement checks).
        match self
            .machine
            .fire_derived(target, &tau, ActorClass::Guard, String::from("guard"))
        {
            Ok(_) => {
                self.derivations.push(Derivation::GuardFired {
                    source,
                    target,
                    transition: tau,
                });
                self.queue.push(Dirty::State(target));
                Ok(())
            }
            // Unmet requirements mean the guard is armed but blocked — the
            // stall detector (T-206) reports it; not a propagation error.
            Err(DispatchError::RequirementsUnmet { .. }) => Ok(()),
            Err(DispatchError::StateMismatch { .. }) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

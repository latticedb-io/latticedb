//! Transition/action dispatch (SPEC-01 §9, T-202): the engine's write path
//! for state motion.
//!
//! - **Requirements**: a transition is enabled iff all its NamedRules hold
//!   at evaluation; `REQUIREMENTS_UNMET` lists the failing rule keys and
//!   the live enabled-action index flips (SGT-9.1/9.2).
//! - **Actors**: non-empty actor classes ⇒ invokable by those classes;
//!   machine classes (system/timer/guard) never user-invokable.
//! - **Effects v1** (micro-decision 8): `spawn {instance_of}` — engine-
//!   authored child creation (journal AddVertex in the same txn);
//!   `select_branch` — cancels XOR siblings; `cancel_siblings` reuses the
//!   machinery. Cancellation reasons are recorded (SGT-9.6).
//! - **Terminals are absorbing** (R5): an action targeting a terminal-state
//!   node is rejected `TERMINAL` (SGT-9.7).
//! - **Action journal record**: `{lsn, node, transition, actor_class,
//!   subject, payload?, from, to}` — the only way state motion enters
//!   history. The engine emits the record; the sequencer assigns the LSN.
//!
//! Cell store: uniform `(node, attr, k) → (value, lsn, written_us)` —
//! scalar = column with fixed k=0 (SPEC-03 §9.1). `logical_us` is the
//! journal-time domain (P8: no wall clock) — the timer wheel (T-205)
//! advances it; cell staleness (`epoch_gte`) reads it.

use crate::algebra::{Kind, evaluate_incoming};
use crate::blockers::{Blocker, BlockerSet};
use crate::fsm::{EngineError, FsmRegistry};
use lattice_core::arrays::GraphArrays;
use lattice_core::ids::{AttrId, KeyIndex, Lsn, NodeId};
use lattice_core::value::Value;
use lattice_schema::catalog::Catalog;
use lattice_schema::compile::Compiled;
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use std::collections::BTreeMap;
use std::fmt;

/// Edge-kind registry bytes (SPEC-01 §10 fixed order).
pub mod kinds {
    pub const AND: u8 = 1;
    pub const OR: u8 = 2;
    pub const KOFN: u8 = 3;
    pub const GUARD: u8 = 4;
    pub const INHIBIT: u8 = 5;
    pub const XOR: u8 = 6;
    pub const MIRROR: u8 = 7;
    pub const WEIGHTED: u8 = 8;
}

/// Journal-time domain (logical microseconds; timer wheel is the clock, P8).
type JournalUs = u64;

/// One stored cell: value + writing LSN + journal-time of the write.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredCell {
    pub value: Value,
    pub lsn: Lsn,
    pub written_us: JournalUs,
}

/// The action journal record content (the sequencer assigns `lsn`).
#[derive(Debug, Clone, PartialEq)]
pub struct ActionRecord {
    pub lsn: Option<Lsn>,
    pub node: NodeId,
    pub transition: Key,
    pub actor_class: ActorClass,
    pub subject: String,
    pub payload: Option<Value>,
    pub from: Key,
    pub to: Key,
}

/// Engine-authored facts to journal in the SAME txn as the action.
#[derive(Debug, Clone, PartialEq)]
pub enum EngineFact {
    /// spawn effect: AddVertex under the acting node, typed correctly.
    AddVertex {
        node: NodeId,
        parent: NodeId,
        node_type: Key,
    },
    /// select_branch/cancel_siblings: sibling cancelled with reason.
    CancelSibling { node: NodeId, reason: String },
}

/// Outcome of a dispatched action.
#[derive(Debug, Clone, PartialEq)]
pub struct ActionOutcome {
    pub from: Key,
    pub to: Key,
    pub record: ActionRecord,
    pub engine_facts: Vec<EngineFact>,
    pub spawned: Option<NodeId>,
    pub cancelled: Vec<(NodeId, String)>,
}

/// Dispatch errors — wire code mapping happens at the protocol layer (M3).
#[derive(Debug, Clone, PartialEq)]
pub enum DispatchError {
    Engine(EngineError),
    UnknownTransition {
        key: Key,
    },
    UnknownNodeType {
        key: Key,
    },
    UnknownRule {
        key: Key,
    },
    /// Node's current state ≠ the transition's `from` (ORDER_CONFLICT family).
    StateMismatch {
        expected_from: Key,
        actual: Key,
    },
    /// Actor class not in the transition's invokable set.
    NotInvokable {
        transition: Key,
        actor: ActorClass,
    },
    /// R5: action targeting a terminal-state node.
    Terminal {
        state: Key,
    },
    /// SGT-9.1: requirements unmet — machine-readable failing list.
    RequirementsUnmet {
        failing: Vec<Key>,
    },
    /// select_branch: the selected branch is not XOR-connected to the node.
    NotABranch {
        node: NodeId,
        selected: NodeId,
    },
    /// select_branch: payload must carry the selected branch as NodeRef.
    MissingBranchSelection,
    /// SGT-9.4: the transition's lag has not elapsed since from-entry.
    LagNotElapsed {
        transition: Key,
        lag_ms: u64,
    },
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DispatchError::Engine(e) => write!(f, "{e}"),
            DispatchError::UnknownTransition { key } => write!(f, "unknown transition `{key}`"),
            DispatchError::UnknownNodeType { key } => write!(f, "unknown node type `{key}`"),
            DispatchError::UnknownRule { key } => write!(f, "unknown rule `{key}`"),
            DispatchError::StateMismatch {
                expected_from,
                actual,
            } => {
                write!(
                    f,
                    "state mismatch: transition fires from `{expected_from}`, node is `{actual}`"
                )
            }
            DispatchError::NotInvokable { transition, actor } => {
                write!(
                    f,
                    "transition `{transition}` is not invokable by actor {actor:?}"
                )
            }
            DispatchError::Terminal { state } => {
                write!(
                    f,
                    "TERMINAL: node is in terminal state `{state}` — absorbing"
                )
            }
            DispatchError::RequirementsUnmet { failing } => {
                write!(
                    f,
                    "REQUIREMENTS_UNMET: {}",
                    failing
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            DispatchError::NotABranch { node, selected } => {
                write!(
                    f,
                    "node {selected} is not an XOR branch of decision node {node}"
                )
            }
            DispatchError::LagNotElapsed { transition, lag_ms } => {
                write!(
                    f,
                    "transition `{transition}` lag {lag_ms}ms not elapsed since state entry"
                )
            }
            DispatchError::MissingBranchSelection => {
                write!(
                    f,
                    "select_branch effect requires the selected branch as payload NodeRef"
                )
            }
        }
    }
}

impl std::error::Error for DispatchError {}

impl From<EngineError> for DispatchError {
    fn from(e: EngineError) -> Self {
        DispatchError::Engine(e)
    }
}

/// ArrayError → EngineError bridging (structural ops inside effects).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EngineErrorBridge {}

/// The semantics-engine runtime: arrays + FSM + cells + compiled epoch.
/// `Clone` supports building fresh fixtures per property-test run.
#[derive(Clone)]
pub struct Machine {
    pub arrays: GraphArrays,
    pub fsm: FsmRegistry,
    compiled: Compiled,
    /// catalog.transitions (dispatch table).
    transitions: BTreeMap<Key, Transition>,
    /// catalog.rules (requirement predicates).
    rules: BTreeMap<Key, PredicateForm>,
    /// node_type key → u16 code (BTreeMap declaration order = deterministic).
    node_type_codes: BTreeMap<Key, u16>,
    /// node_type key → state model key.
    node_type_model: BTreeMap<Key, Key>,
    /// cells: (node, attr, k) → (value, lsn, written_us).
    cells: BTreeMap<(u32, u16, u64), StoredCell>,
    /// journal time (P8: only the timer wheel advances this).
    logical_us: JournalUs,
    /// cancellation reasons (observability; journal-adjacent record).
    cancellations: BTreeMap<u32, String>,
    /// GUARD edge params: edge_id → (sigma, tau). The edge rides in the
    /// arrays (kind = GUARD, aux = this map's key); params are schema-level
    /// references resolved at edge creation (SGT-10.3).
    pub guard_params: BTreeMap<u64, (Key, Key)>,
    next_guard_param_id: u64,
    /// The timer wheel (P8: the only clock). Armed on state entry for
    /// transitions with timing; fired by `advance_time`.
    pub wheel: crate::timer::TimerWheel,
}

impl Machine {
    pub fn new(arrays: GraphArrays, compiled: Compiled, catalog: &Catalog) -> Self {
        let fsm = FsmRegistry::from_compiled(&compiled);
        let transitions: BTreeMap<Key, Transition> = catalog.transitions.clone();
        let rules: BTreeMap<Key, PredicateForm> = catalog
            .rules
            .iter()
            .map(|(k, r)| (k.clone(), r.form.clone()))
            .collect();
        let mut node_type_codes = BTreeMap::new();
        let mut node_type_model = BTreeMap::new();
        for (i, (ntk, nt)) in catalog.node_types.iter().enumerate() {
            node_type_codes.insert(ntk.clone(), i as u16);
            node_type_model.insert(ntk.clone(), nt.state_model.clone());
        }
        Self {
            arrays,
            fsm,
            compiled,
            transitions,
            rules,
            node_type_codes,
            node_type_model,
            cells: BTreeMap::new(),
            logical_us: 0,
            cancellations: BTreeMap::new(),
            guard_params: BTreeMap::new(),
            next_guard_param_id: 0,
            wheel: crate::timer::TimerWheel::new(),
        }
    }

    /// Advance journal time and fire due timers (T-205 — P8: the wheel is
    /// the only clock).
    pub fn advance_time(&mut self, to_us: JournalUs) -> Vec<crate::timer::Due> {
        let due = self.wheel.advance(to_us);
        if to_us > self.logical_us {
            self.logical_us = to_us;
        }
        let mut fired = Vec::new();
        for d in due {
            match d.kind.clone() {
                crate::timer::TimerKind::Deadline { on_deadline } => {
                    // State mismatch (node already moved) drops silently;
                    // REQUIREMENTS_UNMET is observable, not fatal. Anything
                    // else is a schema/model bug surfaced loudly.
                    match self.fire_derived(
                        d.node,
                        &on_deadline,
                        ActorClass::Timer,
                        String::from("timer"),
                    ) {
                        Ok(_) => fired.push(d),
                        Err(DispatchError::StateMismatch { .. }) => {}
                        Err(DispatchError::RequirementsUnmet { .. }) => {}
                        Err(e) => panic!("timer deadline dispatch failed: {e}"),
                    }
                }
                other => fired.push(crate::timer::Due {
                    node: d.node,
                    transition: d.transition,
                    kind: other,
                    armed_us: d.armed_us,
                }),
            }
        }
        fired
    }

    pub fn logical_us(&self) -> JournalUs {
        self.logical_us
    }

    pub fn node_type_code(&self, key: &Key) -> Option<u16> {
        self.node_type_codes.get(key).copied()
    }

    pub fn node_type_key(&self, code: u16) -> Option<&Key> {
        self.node_type_codes
            .iter()
            .find(|(_, c)| **c == code)
            .map(|(k, _)| k)
    }

    /// Write one cell (R6/R7 validations land in T-207's validator suite).
    pub fn set_cell(
        &mut self,
        node: NodeId,
        attr: &Key,
        k: KeyIndex,
        value: Value,
        lsn: Lsn,
    ) -> Result<(), DispatchError> {
        let id = self
            .compiled
            .attr_ids
            .get(attr)
            .ok_or_else(|| DispatchError::UnknownNodeType { key: attr.clone() })?;
        let AttrId(id) = id;
        self.cells.insert(
            (node.0, *id, k.0),
            StoredCell {
                value,
                lsn,
                written_us: self.logical_us,
            },
        );
        Ok(())
    }

    /// Latest cell of an attr on a node (greatest k).
    pub fn latest_cell(&self, node: NodeId, attr: &Key) -> Option<&StoredCell> {
        let id = self.compiled.attr_ids.get(attr)?;
        let AttrId(id) = id;
        self.cells
            .range((node.0, *id, 0)..=(node.0, *id, u64::MAX))
            .next_back()
            .map(|(_, cell)| cell)
    }

    /// Cells of an attr on a node, k-ascending.
    fn cells_of(&self, node: NodeId, attr: &Key) -> Vec<(u64, &StoredCell)> {
        let Some(AttrId(id)) = self.compiled.attr_ids.get(attr) else {
            return Vec::new();
        };
        self.cells
            .range((node.0, *id, 0)..=(node.0, *id, u64::MAX))
            .filter(|((_, a, _), _)| a == id)
            .map(|((_, _, k), cell)| (*k, cell))
            .collect()
    }

    /// Live enabled-action index for a node: static action_index filtered by
    /// requirements actually holding right now (SGT-9.1/9.2).
    pub fn enabled_actions(&self, node: NodeId) -> Result<Vec<Key>, DispatchError> {
        let type_code = self.arrays.type_of(node);
        let ntk = self
            .node_type_key(type_code)
            .ok_or_else(|| DispatchError::UnknownNodeType {
                key: key_of_code(type_code),
            })?
            .clone();
        let model_key = self.node_type_model[&ntk].clone();
        let state_code = self.arrays.state_of(node);
        let model =
            self.compiled
                .models
                .get(&model_key)
                .ok_or_else(|| EngineError::UnknownModel {
                    model: model_key.clone(),
                })?;
        let Some((state_key, _, _)) = model.states.get(state_code as usize) else {
            return Err(corrupt_state_code(model_key.clone(), state_code));
        };
        let static_index = self
            .compiled
            .action_index
            .get(&(ntk.clone(), state_key.clone()))
            .cloned()
            .unwrap_or_default();
        let mut enabled = Vec::new();
        for tk in static_index {
            let t = &self.transitions[&tk];
            if t.requirements.iter().all(|r| self.eval_rule(r, node)) {
                enabled.push(tk);
            }
        }
        Ok(enabled)
    }

    /// Evaluate a NamedRule against a node's live state (SPEC-01 §13 forms).
    pub fn eval_rule(&self, rule: &Key, node: NodeId) -> bool {
        match self.rules.get(rule) {
            Some(form) => self.eval_predicate(form, node),
            None => false,
        }
    }

    fn eval_predicate(&self, form: &PredicateForm, node: NodeId) -> bool {
        match form {
            PredicateForm::AttrPresent { attr } => self.latest_cell(node, attr).is_some(),
            PredicateForm::AttrCmp { attr, op, value }
            | PredicateForm::LatestCmp { attr, op, value } => match self.latest_cell(node, attr) {
                Some(cell) => value_cmp(&cell.value, op, value),
                None => false,
            },
            PredicateForm::CellCountGte { attr, k } => {
                self.cells_of(node, attr).len() >= *k as usize
            }
            PredicateForm::CellFieldCountGte {
                attr,
                field,
                value,
                min,
            } => {
                // count occurrences of `value` at record field index `field`
                // across the attr's cells; field index = position in field
                // name order is schema-level — at engine level the field is
                // addressed by its declared index (compiled order). We treat
                // `field` as the field INDEX key parsed from the name for v1
                // (record fields carry their index in the field name grammar).
                let target = field_index(field);
                let mut count = 0usize;
                for (_, cell) in self.cells_of(node, attr) {
                    if let Value::Record(rec) = &cell.value
                        && let Some(v) = rec.get(target)
                        && v == value
                    {
                        count += 1;
                    }
                }
                count >= *min as usize
            }
            PredicateForm::EpochGte { attr, age_us } => match self.latest_cell(node, attr) {
                Some(cell) => self.logical_us.saturating_sub(cell.written_us) >= *age_us as u64,
                None => false,
            },
            PredicateForm::ChildrenState {
                success,
                expect_all,
            } => {
                let kids = self.arrays.children_of(node);
                if kids.is_empty() {
                    return false;
                }
                let mut hits = 0usize;
                for child in kids {
                    if self.child_in_terminal_class(*child, *success) {
                        hits += 1;
                    }
                }
                if *expect_all {
                    hits == kids.len()
                } else {
                    hits > 0
                }
            }
            PredicateForm::SetMember { attr, values } => match self.latest_cell(node, attr) {
                Some(cell) => values.iter().any(|v| v == &cell.value),
                None => false,
            },
            PredicateForm::StrEq { attr, value } => match self.latest_cell(node, attr) {
                Some(cell) => matches!(&cell.value, Value::Str(s) if s.as_ref() == value),
                None => false,
            },
            PredicateForm::StrPrefix { attr, value } => match self.latest_cell(node, attr) {
                Some(cell) => matches!(&cell.value, Value::Str(s) if s.as_ref().starts_with(value)),
                None => false,
            },
        }
    }

    /// Is `node`'s current state in the given terminal class?
    fn child_in_terminal_class(&self, node: NodeId, success: bool) -> bool {
        let type_code = self.arrays.type_of(node);
        let Some(ntk) = self.node_type_key(type_code) else {
            return false;
        };
        let Some(model_key) = self.node_type_model.get(ntk) else {
            return false;
        };
        let Some(model) = self.compiled.models.get(model_key) else {
            return false;
        };
        let code = self.arrays.state_of(node) as usize;
        let Some((_, success_terminal, failure_terminal)) = model.states.get(code) else {
            return false;
        };
        if success {
            *success_terminal
        } else {
            *failure_terminal
        }
    }

    /// Dispatch one action (SPEC-01 §9): validate → gate → move → effects.
    pub fn apply_action(
        &mut self,
        node: NodeId,
        transition_key: &Key,
        actor: ActorClass,
        subject: impl Into<String>,
        payload: Option<Value>,
    ) -> Result<ActionOutcome, DispatchError> {
        let t = self
            .transitions
            .get(transition_key)
            .ok_or_else(|| DispatchError::UnknownTransition {
                key: transition_key.clone(),
            })?
            .clone();

        // Actor law (SPEC-01 §9): invokable iff actor ∈ actors (non-empty).
        if !t.actors.contains(&actor) {
            return Err(DispatchError::NotInvokable {
                transition: transition_key.clone(),
                actor,
            });
        }

        // Node type/model resolution.
        let type_code = self.arrays.type_of(node);
        let ntk = self
            .node_type_key(type_code)
            .ok_or_else(|| DispatchError::UnknownNodeType {
                key: key_of_code(type_code),
            })?
            .clone();
        let model_key = self.node_type_model[&ntk].clone();
        if model_key != t.model {
            return Err(DispatchError::UnknownTransition {
                key: transition_key.clone(),
            });
        }

        // Current state (SoA u8) → key; R5 terminal absorption; from-match.
        let state_code = self.arrays.state_of(node);
        let model =
            self.compiled
                .models
                .get(&model_key)
                .ok_or_else(|| EngineError::UnknownModel {
                    model: model_key.clone(),
                })?;
        let Some((actual, success_terminal, failure_terminal)) =
            model.states.get(state_code as usize)
        else {
            return Err(corrupt_state_code(model_key.clone(), state_code));
        };
        if &t.from != actual {
            return Err(DispatchError::StateMismatch {
                expected_from: t.from.clone(),
                actual: actual.clone(),
            });
        }
        if *success_terminal || *failure_terminal {
            return Err(DispatchError::Terminal {
                state: actual.clone(),
            });
        }

        // Requirements (SGT-9.1): failing list is machine-readable.
        let failing: Vec<Key> = t
            .requirements
            .iter()
            .filter(|r| !self.eval_rule(r, node))
            .cloned()
            .collect();
        if !failing.is_empty() {
            return Err(DispatchError::RequirementsUnmet { failing });
        }

        // Lag gate (SGT-9.4): the transition MAY fire no earlier than
        // from-entry + lag_ms.
        if let Some(lag) = t.timing.lag_ms
            && !self.wheel.lag_ok(node, transition_key, self.logical_us)
        {
            return Err(DispatchError::LagNotElapsed {
                transition: transition_key.clone(),
                lag_ms: lag,
            });
        }
        // FSM motion (declared-only, ranks, absorbing — T-201).
        let allowed = self.fsm.check_motion(&model_key, &t.from, &t.to)?;
        let to_code = allowed.to.code;

        // Effects.
        let mut engine_facts = Vec::new();
        let mut spawned = None;
        let mut cancelled = Vec::new();
        for effect in &t.effects {
            match effect {
                Effect::Spawn { instance_of } => {
                    let child_type_code = self.node_type_code(instance_of).ok_or_else(|| {
                        DispatchError::UnknownNodeType {
                            key: instance_of.clone(),
                        }
                    })?;
                    let child = self
                        .arrays
                        .add_child(node, child_type_code)
                        .map_err(|e| DispatchError::Engine(EngineError::Arrays(e)))?;
                    engine_facts.push(EngineFact::AddVertex {
                        node: child,
                        parent: node,
                        node_type: instance_of.clone(),
                    });
                    spawned = Some(child);
                }
                Effect::SelectBranch => {
                    // The selected branch rides in the payload as NodeRef.
                    let Some(Value::NodeRef(selected)) = payload.as_ref() else {
                        return Err(DispatchError::MissingBranchSelection);
                    };
                    let selected = *selected;
                    let (kept, others) = self.xor_branches_of(node, selected)?;
                    let _ = kept;
                    for sibling in others {
                        let reason = format!("select_branch: {selected:?} chosen over {sibling:?}");
                        if let Some(_moved_to) = self.cancel_via_declared_motion(sibling)? {
                            cancelled.push((sibling, reason.clone()));
                            engine_facts.push(EngineFact::CancelSibling {
                                node: sibling,
                                reason,
                            });
                        } else {
                            // No declared cancel path: record-only (observable).
                            cancelled
                                .push((sibling, format!("{reason} (no declared cancel motion)")));
                            engine_facts.push(EngineFact::CancelSibling {
                                node: sibling,
                                reason: format!("{reason} (no declared cancel motion)"),
                            });
                        }
                    }
                }
                Effect::CancelSiblings => {
                    // Cancel ALL XOR siblings of the acting node's decision
                    // parent (reuses select_branch machinery, micro-dec 8).
                    for sibling in self.xor_siblings_via_parents(node) {
                        let reason = format!("cancel_siblings: superseded by {node:?}");
                        if self.cancel_via_declared_motion(sibling)?.is_some() {
                            cancelled.push((sibling, reason.clone()));
                            engine_facts.push(EngineFact::CancelSibling {
                                node: sibling,
                                reason,
                            });
                        }
                    }
                }
            }
        }

        // State motion (the ONLY state write — P2).
        self.arrays
            .set_state(node, to_code)
            .map_err(|e| DispatchError::Engine(EngineError::Arrays(e)))?;
        self.rearm_timers(node, &allowed.to.key)?;

        let record = ActionRecord {
            lsn: None, // sequencer assigns (tx layer)
            node,
            transition: transition_key.clone(),
            actor_class: actor,
            subject: subject.into(),
            payload,
            from: allowed.from.key.clone(),
            to: allowed.to.key.clone(),
        };
        Ok(ActionOutcome {
            from: allowed.from.key,
            to: allowed.to.key,
            record,
            engine_facts,
            spawned,
            cancelled,
        })
    }

    /// XOR branches of a decision node: dsts of outgoing XOR edges.
    fn xor_branches_of(
        &self,
        decision: NodeId,
        selected: NodeId,
    ) -> Result<(bool, Vec<NodeId>), DispatchError> {
        let mut targets = Vec::new();
        for eid in self.arrays.dep_out_ids(decision) {
            if let Some(rec) = self.arrays.edge(*eid)
                && rec.kind == kinds::XOR
            {
                targets.push(rec.dst);
            }
        }
        if !targets.contains(&selected) {
            return Err(DispatchError::NotABranch {
                node: decision,
                selected,
            });
        }
        let others: Vec<NodeId> = targets.into_iter().filter(|t| *t != selected).collect();
        Ok((true, others))
    }

    /// Siblings connected to `node` through a shared decision parent via XOR.
    fn xor_siblings_via_parents(&self, node: NodeId) -> Vec<NodeId> {
        let mut siblings = Vec::new();
        for eid in self.arrays.dep_in_ids(node) {
            if let Some(rec) = self.arrays.edge(*eid)
                && rec.kind == kinds::XOR
            {
                let parent = rec.src;
                for eid2 in self.arrays.dep_out_ids(parent) {
                    if let Some(rec2) = self.arrays.edge(*eid2)
                        && rec2.kind == kinds::XOR
                        && rec2.dst != node
                        && !siblings.contains(&rec2.dst)
                    {
                        siblings.push(rec2.dst);
                    }
                }
            }
        }
        siblings
    }

    /// Cancel a sibling: find a declared motion from its current state to a
    /// failure terminal; apply through the FSM. Returns the moved-to state.
    fn cancel_via_declared_motion(
        &mut self,
        sibling: NodeId,
    ) -> Result<Option<Key>, DispatchError> {
        let type_code = self.arrays.type_of(sibling);
        let Some(ntk) = self.node_type_key(type_code) else {
            return Ok(None);
        };
        let model_key = self.node_type_model[ntk].clone();
        let state_code = self.arrays.state_of(sibling);
        let Some(model) = self.compiled.models.get(&model_key) else {
            return Ok(None);
        };
        let Some((cur, _, _)) = model.states.get(state_code as usize) else {
            return Ok(None); // stale state code after schema evolution: nothing to cancel
        };
        // any declared motion from current state to a failure terminal
        let mut cancel_to: Option<Key> = None;
        for (from, to) in model.motions.keys() {
            if from == cur
                && let Some((_, _, failure_terminal)) =
                    model.states.iter().find(|(k, _, _)| k == to)
                && *failure_terminal
            {
                cancel_to = Some(to.clone());
                break;
            }
        }
        let Some(to) = cancel_to else { return Ok(None) };
        let allowed = self.fsm.check_motion(&model_key, cur, &to)?;
        self.arrays
            .set_state(sibling, allowed.to.code)
            .map_err(|e| DispatchError::Engine(EngineError::Arrays(e)))?;
        Ok(Some(to))
    }

    pub fn cancellation_reason(&self, node: NodeId) -> Option<&String> {
        self.cancellations.get(&node.0)
    }

    /// Compiled attr id lookup (propagation read-set indexing).
    pub fn compiled_attr_id(&self, key: &Key) -> Option<AttrId> {
        self.compiled.attr_ids.get(key).copied()
    }

    /// Read-only access to the compiled epoch (read-set inversion etc.).
    pub fn compiled_ref(&self) -> &Compiled {
        &self.compiled
    }

    /// Rebuild against a NEW compiled epoch, preserving live runtime state
    /// (arrays, cells, journal time, timers, guard params). This is the
    /// per-epoch recompile path (ARCHITECTURE §7): the machine is
    /// reconstructed, live facts ride across untouched.
    pub fn recompile(mut self, compiled: Compiled, catalog: &Catalog) -> Self {
        let arrays = std::mem::replace(
            &mut self.arrays,
            GraphArrays::new(lattice_core::ids::NodeIdSpace::new()),
        );
        let cells = std::mem::take(&mut self.cells);
        let logical_us = self.logical_us;
        let wheel = std::mem::take(&mut self.wheel);
        let guard_params = std::mem::take(&mut self.guard_params);
        let next_guard_param_id = self.next_guard_param_id;
        let mut machine = Machine::new(arrays, compiled, catalog);
        machine.cells = cells;
        machine.logical_us = logical_us;
        machine.wheel = wheel;
        machine.guard_params = guard_params;
        machine.next_guard_param_id = next_guard_param_id;
        machine
    }

    /// A node's current state KEY (SoA u8 → model declaration order).
    pub fn state_key_of(&self, node: NodeId) -> Result<Key, DispatchError> {
        let type_code = self.arrays.type_of(node);
        let ntk = self
            .node_type_key(type_code)
            .ok_or_else(|| DispatchError::UnknownNodeType {
                key: key_of_code(type_code),
            })?
            .clone();
        let model_key = self.node_type_model[&ntk].clone();
        let model = self.compiled.models.get(&model_key).ok_or_else(|| {
            DispatchError::Engine(EngineError::UnknownModel {
                model: model_key.clone(),
            })
        })?;
        let code = self.arrays.state_of(node) as usize;
        let Some((skey, _, _)) = model.states.get(code) else {
            return Err(corrupt_state_code(model_key.clone(), code as u8));
        };
        Ok(skey.clone())
    }

    /// Set a node's state by KEY (MIRROR: copies a state across models —
    /// skips when the target model has no matching state key).
    pub fn set_state_by_key(&mut self, node: NodeId, state_key: &Key) -> Result<(), DispatchError> {
        let type_code = self.arrays.type_of(node);
        let ntk = self
            .node_type_key(type_code)
            .ok_or_else(|| DispatchError::UnknownNodeType {
                key: key_of_code(type_code),
            })?
            .clone();
        let model_key = self.node_type_model[&ntk].clone();
        let code = self
            .compiled
            .models
            .get(&model_key)
            .and_then(|m| {
                m.states
                    .iter()
                    .position(|(k, _, _)| k == state_key)
                    .map(|i| i as u8)
            })
            .ok_or_else(|| EngineError::UnknownState {
                model: model_key,
                state: state_key.clone(),
            })?;
        self.arrays
            .set_state(node, code)
            .map_err(|e| DispatchError::Engine(EngineError::Arrays(e)))
    }

    /// GUARD edge params resolved: does this guard edge (source → target)
    /// have sigma == source's current state? Returns tau if armed.
    pub fn guard_tau_for(&self, source: NodeId, target: NodeId, source_state: &Key) -> Option<Key> {
        for eid in self.arrays.dep_out_ids(source) {
            let Some(rec) = self.arrays.edge(*eid) else {
                continue;
            };
            if rec.kind != crate::dispatch::kinds::GUARD || rec.dst != target {
                continue;
            }
            if let Some((sigma, tau)) = self.guard_params.get(&(rec.aux as u64))
                && sigma == source_state
            {
                return Some(tau.clone());
            }
        }
        None
    }

    /// Derived motion (author = guard/timer): skips the actor law (engine
    /// authors are not invokers), keeps requirement checks and FSM motion.
    pub fn fire_derived(
        &mut self,
        node: NodeId,
        transition_key: &Key,
        author: ActorClass,
        subject: impl Into<String>,
    ) -> Result<ActionOutcome, DispatchError> {
        let t = self
            .transitions
            .get(transition_key)
            .ok_or_else(|| DispatchError::UnknownTransition {
                key: transition_key.clone(),
            })?
            .clone();
        if !t.actors.is_empty() {
            // Derived motion fires ONLY engine-armed (empty-actor) transitions.
            return Err(DispatchError::NotInvokable {
                transition: transition_key.clone(),
                actor: author,
            });
        }
        // State match + terminal + requirements + FSM (same as apply_action
        // minus the actor gate).
        let type_code = self.arrays.type_of(node);
        let ntk = self
            .node_type_key(type_code)
            .ok_or_else(|| DispatchError::UnknownNodeType {
                key: key_of_code(type_code),
            })?
            .clone();
        let model_key = self.node_type_model[&ntk].clone();
        if model_key != t.model {
            return Err(DispatchError::UnknownTransition {
                key: transition_key.clone(),
            });
        }
        let state_code = self.arrays.state_of(node);
        let model =
            self.compiled
                .models
                .get(&model_key)
                .ok_or_else(|| EngineError::UnknownModel {
                    model: model_key.clone(),
                })?;
        let Some((actual, success_terminal, failure_terminal)) =
            model.states.get(state_code as usize)
        else {
            return Err(corrupt_state_code(model_key.clone(), state_code));
        };
        if &t.from != actual {
            return Err(DispatchError::StateMismatch {
                expected_from: t.from.clone(),
                actual: actual.clone(),
            });
        }
        if *success_terminal || *failure_terminal {
            return Err(DispatchError::Terminal {
                state: actual.clone(),
            });
        }
        let failing: Vec<Key> = t
            .requirements
            .iter()
            .filter(|r| !self.eval_rule(r, node))
            .cloned()
            .collect();
        if !failing.is_empty() {
            return Err(DispatchError::RequirementsUnmet { failing });
        }
        // Lag gate for derived motions too (timer fires respect lags).
        if let Some(lag) = t.timing.lag_ms
            && !self.wheel.lag_ok(node, transition_key, self.logical_us)
        {
            return Err(DispatchError::LagNotElapsed {
                transition: transition_key.clone(),
                lag_ms: lag,
            });
        }
        let allowed = self.fsm.check_motion(&model_key, &t.from, &t.to)?;
        self.arrays
            .set_state(node, allowed.to.code)
            .map_err(|e| DispatchError::Engine(EngineError::Arrays(e)))?;
        self.rearm_timers(node, &allowed.to.key)?;
        let record = ActionRecord {
            lsn: None,
            node,
            transition: transition_key.clone(),
            actor_class: author,
            subject: subject.into(),
            payload: None,
            from: allowed.from.key.clone(),
            to: allowed.to.key.clone(),
        };
        Ok(ActionOutcome {
            from: allowed.from.key,
            to: allowed.to.key,
            record,
            engine_facts: Vec::new(),
            spawned: None,
            cancelled: Vec::new(),
        })
    }

    /// After a state motion to `new_state_key`: cancel timers from the old
    /// state, arm timers for transitions leaving `new_state_key` that carry
    /// timing (SPEC-01 §9: arming = entering the from-state).
    fn rearm_timers(&mut self, node: NodeId, new_state_key: &Key) -> Result<(), DispatchError> {
        self.wheel.cancel_node(node);
        let type_code = self.arrays.type_of(node);
        let ntk = self
            .node_type_key(type_code)
            .ok_or_else(|| DispatchError::UnknownNodeType {
                key: key_of_code(type_code),
            })?
            .clone();
        let model_key = self.node_type_model[&ntk].clone();
        let now = self.logical_us;
        for (tk, t) in &self.transitions {
            if t.model == model_key && &t.from == new_state_key {
                if let Some(lag) = t.timing.lag_ms {
                    self.wheel
                        .arm_lag(node, tk, lag, now)
                        .map_err(|e| DispatchError::Engine(EngineError::Timers(e)))?;
                }
                if let (Some(deadline), Some(on_deadline)) =
                    (t.timing.deadline_ms, t.timing.on_deadline.clone())
                {
                    self.wheel
                        .arm_deadline(node, tk, deadline, &on_deadline, now)
                        .map_err(|e| DispatchError::Engine(EngineError::Timers(e)))?;
                }
            }
        }
        Ok(())
    }

    /// Register a GUARD edge with resolved params: creates the dependency
    /// edge (kind GUARD) and stores (sigma, tau). Returns the aux id.
    pub fn add_guard_edge(
        &mut self,
        source: NodeId,
        target: NodeId,
        sigma: &Key,
        tau: &Key,
    ) -> Result<u32, DispatchError> {
        let aux = self.next_guard_param_id as u32;
        self.next_guard_param_id += 1;
        self.guard_params
            .insert(aux as u64, (sigma.clone(), tau.clone()));
        let eid = self
            .arrays
            .add_edge(source, target, crate::dispatch::kinds::GUARD, aux)
            .map_err(|e| DispatchError::Engine(EngineError::Arrays(e)))?;
        let _ = eid;
        Ok(aux)
    }

    /// Register a MIRROR edge.
    pub fn add_mirror_edge(&mut self, source: NodeId, target: NodeId) -> Result<(), DispatchError> {
        self.arrays
            .add_edge(source, target, crate::dispatch::kinds::MIRROR, 0)
            .map_err(|e| DispatchError::Engine(EngineError::Arrays(e)))?;
        Ok(())
    }
}

/// A state code outside the model's range after schema evolution — an
/// invariant violation surfaced loudly (never silently mapped).
fn corrupt_state_code(model: Key, code: u8) -> DispatchError {
    DispatchError::Engine(EngineError::Arrays(
        lattice_core::arrays::ArrayError::MalformedRaw {
            reason: format!("model `{model}`: state code {code} out of range"),
        },
    ))
}

fn unreachable_key() -> ! {
    panic!("neither fallback key parses — programmer error")
}

fn key_of_code(_code: u16) -> Key {
    Key::parse("__unknown_node_type__")
        .unwrap_or_else(|_| Key::parse("unknown_node_type").unwrap_or_else(|_| unreachable_key()))
}

fn field_index(field: &Key) -> usize {
    // v1: record fields are addressed by their declared index encoded in the
    // field key grammar as a decimal suffix (field grammar is schema-level;
    // the engine needs only a stable index). Keys are snake_case names — the
    // trailing numeric suffix IS the index by bundle convention.
    let name = field.as_str();
    let idx: String = name
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    idx.parse().unwrap_or(usize::MAX)
}

/// Order-sensitive comparison for predicate forms (same-variant only; a
/// variant mismatch makes the comparison false — typed columns guarantee
/// homogeneity in practice).
fn value_cmp(actual: &Value, op: &CmpOp, expected: &Value) -> bool {
    use std::cmp::Ordering;
    let ord = match (actual, expected) {
        (Value::I64(a), Value::I64(b))
        | (Value::Timestamp(a), Value::Timestamp(b))
        | (Value::Duration(a), Value::Duration(b)) => a.cmp(b),
        (Value::F64(a), Value::F64(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Value::Str(a), Value::Str(b)) => a.as_ref().cmp(b.as_ref()),
        (Value::Enum(a), Value::Enum(b)) => a.cmp(b),
        (Value::Decimal(a), Value::Decimal(b)) => a.unscaled.cmp(&b.unscaled),
        (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
        _ => return false,
    };
    match op {
        CmpOp::Eq => ord == Ordering::Equal,
        CmpOp::Ne => ord != Ordering::Equal,
        CmpOp::Gt => ord == Ordering::Greater,
        CmpOp::Gte => ord != Ordering::Less,
        CmpOp::Lt => ord == Ordering::Less,
        CmpOp::Lte => ord != Ordering::Greater,
    }
}

// silence: KeyIndex/Lsn used in cells API
#[allow(unused)]
fn _touches(k: KeyIndex, l: Lsn) {}

impl Machine {
    /// Compute the blocker set for one node: for each transition declared
    /// from the node's current state (invokable actions + guard-armed
    /// transitions), classify every reason it can't fire right now.
    /// Empty set = quiescent node (at least one action ready).
    pub fn blockers(&self, node: NodeId) -> Result<BlockerSet, DispatchError> {
        if !self.arrays.is_alive(node) {
            return Err(DispatchError::UnknownNodeType {
                key: Key::parse("unknown").unwrap_or_else(|_| unreachable_key()),
            });
        }
        let type_code = self.arrays.type_of(node);
        let ntk = self
            .node_type_key(type_code)
            .ok_or_else(|| DispatchError::UnknownNodeType {
                key: key_of_code(type_code),
            })?
            .clone();
        let model_key = self.node_type_model[&ntk].clone();
        let model = self.compiled.models.get(&model_key).ok_or_else(|| {
            DispatchError::Engine(crate::fsm::EngineError::UnknownModel {
                model: model_key.clone(),
            })
        })?;
        let code = self.arrays.state_of(node) as usize;
        let Some((cur_state, succ_t, fail_t)) = model.states.get(code) else {
            return Err(corrupt_state_code(model_key.clone(), code as u8));
        };
        let mut blockers = Vec::new();
        let _ = (succ_t, fail_t);

        // Every transition declared from the current state.
        for (tk, t) in &self.transitions {
            if t.model != model_key || &t.from != cur_state {
                continue;
            }
            // 1. Requirements (named failing rules).
            for r in &t.requirements {
                if !self.eval_rule(r, node) {
                    blockers.push(Blocker::UnmetRule {
                        transition: tk.clone(),
                        rule: r.clone(),
                    });
                }
            }
            // 2. Lag pending.
            if t.timing.lag_ms.is_some() && !self.wheel.lag_ok(node, tk, self.logical_us) {
                let fire_at = self.wheel.fire_at(node, tk).unwrap_or(0);
                let remaining = fire_at.saturating_sub(self.logical_us) / 1_000;
                blockers.push(Blocker::LagPending {
                    transition: tk.clone(),
                    remaining_ms: remaining,
                });
            }
            // 3. Deadline armed (informational — it fires on_deadline at expiry).
            if self.wheel.deadline_armed(node, tk) {
                // not a blocker per se; the deadline is the transition's own
                // SLA. Recorded only via PendingGuard below for guards.
            }
            // 4. Dependency algebra over incoming dependency edges:
            //    incoming edges of kinds AND/OR/KOFN/WEIGHTED from other
            //    nodes whose satisfaction feeds this transition's target...
            //    In the LatticeDB model, dependency edges feed a node's
            //    *satisfaction*, which guards read. For dispatch-level
            //    blockers we compute the node's own satisfaction deficits.
            let incoming = self.incoming_dependency_edges(node);
            if !incoming.is_empty() {
                let verdict = evaluate_incoming(&incoming, None);
                if verdict == crate::algebra::Verdict::NotSatisfied {
                    let (satisfied, total) = incoming
                        .iter()
                        .fold((0usize, 0usize), |(s, t), (_, sat, _)| {
                            (s + usize::from(*sat), t + 1)
                        });
                    let kind_name = incoming[0].0.name();
                    blockers.push(Blocker::DependencyDeficit {
                        transition: tk.clone(),
                        kind: kind_name,
                        satisfied,
                        required: total,
                    });
                }
            }
            // 5. GUARD edges targeting this node: pending while sigma unreached.
            for eid in self.arrays.dep_in_ids(node) {
                let Some(rec) = self.arrays.edge(*eid) else {
                    continue;
                };
                if rec.kind != kinds::GUARD {
                    continue;
                }
                if let Some((sigma, tau)) = self.guard_params.get(&(rec.aux as u64))
                    && tau == tk
                {
                    let src_state = self.state_key_of(rec.src).ok();
                    if src_state.as_ref() != Some(sigma) {
                        blockers.push(Blocker::PendingGuard {
                            transition: tk.clone(),
                            source: rec.src,
                            sigma: sigma.clone(),
                        });
                    }
                }
            }
            // 6. INHIBIT edges targeting this node (active inhibition).
            for eid in self.arrays.dep_in_ids(node) {
                let Some(rec) = self.arrays.edge(*eid) else {
                    continue;
                };
                if rec.kind != kinds::INHIBIT {
                    continue;
                }
                // activation: on_source_final | while_source_active — v1
                // core level: active when the source is NOT in a success
                // terminal (on_source_final) or alive at all (while_active).
                let src_state = self.state_key_of(rec.src).ok();
                let active = match src_state.as_ref() {
                    None => true,
                    Some(skey) => {
                        // success-terminal source stops inhibiting
                        !self.is_success_terminal(rec.src, skey)
                    }
                };
                if active {
                    blockers.push(Blocker::Inhibited {
                        transition: tk.clone(),
                        source: rec.src,
                        activation: "while_source_active",
                    });
                }
            }
        }
        Ok(BlockerSet { node, blockers })
    }

    fn incoming_dependency_edges(&self, node: NodeId) -> Vec<(Kind, bool, i128)> {
        let mut out = Vec::new();
        for eid in self.arrays.dep_in_ids(node) {
            let Some(rec) = self.arrays.edge(*eid) else {
                continue;
            };
            let Some(kind) = Kind::from_byte(rec.kind) else {
                continue;
            };
            // Satisfaction kinds ONLY (§10 rollup): AND/OR/KOFN/WEIGHTED.
            // GUARD/INHIBIT/XOR/MIRROR carry separate semantics and have
            // their own blocker classes — mixing them into the satisfaction
            // count would corrupt the deficit report (found by the
            // blockers test: guard+inhibit edges inflated required 2→4).
            if !matches!(kind, Kind::And | Kind::Or | Kind::Kofn | Kind::Weighted) {
                continue;
            }
            let satisfied = self
                .state_key_of(rec.src)
                .ok()
                .map(|s| self.is_success_terminal(rec.src, &s))
                .unwrap_or(false);
            out.push((kind, satisfied, 10_000i128));
        }
        out
    }

    fn is_success_terminal(&self, node: NodeId, state: &Key) -> bool {
        let type_code = self.arrays.type_of(node);
        let Some(ntk) = self.node_type_key(type_code) else {
            return false;
        };
        let Some(model_key) = self.node_type_model.get(ntk) else {
            return false;
        };
        let Some(model) = self.compiled.models.get(model_key) else {
            return false;
        };
        model
            .states
            .iter()
            .find(|(k, _, _)| k == state)
            .map(|(_, s, _)| *s)
            .unwrap_or(false)
    }
}

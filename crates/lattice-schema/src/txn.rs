//! Schema transactions (SPEC-01 §16): the closed op registry, R13 guarded
//! evolution with reference counting, atomic commit, and dry-run planning.
//!
//! Laws implemented here:
//! - **Op registry is closed** (SPEC-01 §16): exactly the 28 ops; keys are
//!   immutable (no rename op exists anywhere — SGT-2.3 is structural).
//! - **Atomicity**: a schema txn is all-or-nothing (the journal records one
//!   SchemaTxn per txn — a partial commit would be unreplayable).
//! - **Every accepted epoch compiles** (P3): after each accepted op the
//!   staged catalog must `compile()` cleanly — the compiler is the semantic
//!   gate (ranks, homogeneity, monotonicity, unresolved references).
//! - **R13 drain discipline** (deprecate → drain → remove): destructive ops
//!   against live references are rejected with a rule id and a live-reference
//!   path (SGT-16.1). Deprecate-first is enforced where the registry offers a
//!   Deprecate op (enum values, states, node types).
//! - **Dry-run fidelity** (SGT-16.3): `plan` and `commit` run the identical
//!   validation pass over a staged copy.
//! - **Enum index stability**: cells store dict indices, so removing a value
//!   is only legal for the LAST value of the enum (shrinking never
//!   re-indexes; SPEC-01 §4 + SPEC-03 codec dict).

use crate::catalog::Catalog;
use crate::entities::*;
use crate::key::Key;
use lattice_core::value::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// Deprecation registries — catalog-level by design (ED-06): entity shapes
/// stay exactly as SPEC-01 §2–§15 froze them. Serialized with the catalog
/// snapshot (T-106).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deprecations {
    pub enum_values: BTreeSet<(Key, Key)>,
    pub states: BTreeSet<(Key, Key)>,
    pub node_types: BTreeSet<Key>,
}

/// The closed op registry (SPEC-01 §16), in registry order.
#[derive(Debug, Clone, PartialEq)]
pub enum SchemaOp {
    CreateGraph(GraphSchema),
    DropGraph {
        key: Key,
    },
    CreateEnum(EnumType),
    AddEnumValue {
        enum_key: Key,
        value: Key,
    },
    DeprecateEnumValue {
        enum_key: Key,
        value: Key,
    },
    RemoveEnumValue {
        enum_key: Key,
        value: Key,
    },
    CreateRecord(RecordType),
    AddRecordField {
        record_key: Key,
        name: Key,
        elem: ElemType,
    },
    RemoveRecordField {
        record_key: Key,
        field: Key,
    },
    CreateKeyType(KeyTypeDef),
    CreateAttribute(AttributeDef),
    AlterAttributeCodec {
        attr_key: Key,
        codec: String,
    },
    CreateAttributeSet(AttributeSet),
    CreateStateModel(StateModel),
    AddState {
        model_key: Key,
        state: State,
    },
    AddTransition(Transition),
    AlterTransitionTiming {
        transition_key: Key,
        timing: TransitionTiming,
    },
    DeprecateState {
        model_key: Key,
        state_key: Key,
    },
    RemoveState {
        model_key: Key,
        state_key: Key,
    },
    CreateNodeType(NodeType),
    DeprecateNodeType {
        type_key: Key,
    },
    CreateIndex(IndexDef),
    DropIndex {
        index_key: Key,
    },
    CreateRule(NamedRule),
    RemoveRule {
        rule_key: Key,
    },
    CreateRole(Role),
    GrantOp(Grant),
    Revoke {
        role: Key,
        subject: String,
        target: GrantTarget,
    },
}

/// Per-op outcome of a plan/commit validation pass.
#[derive(Debug, Clone, PartialEq)]
pub struct OpResult {
    pub op_index: usize,
    pub accepted: bool,
    pub error: Option<SchemaError>,
    /// R13 rule id + live-reference path (guarded-evolution rejections).
    pub r13: Option<R13Violation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct R13Violation {
    pub rule: &'static str,
    pub path: String,
}

impl fmt::Display for R13Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.rule, self.path)
    }
}

/// Atomic commit rejection: the per-op outcomes and nothing applied.
#[derive(Debug, Clone, PartialEq)]
pub struct TxnError {
    pub outcomes: Vec<OpResult>,
}

impl fmt::Display for TxnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rejected: Vec<String> = self
            .outcomes
            .iter()
            .filter(|o| !o.accepted)
            .map(|o| match (&o.error, &o.r13) {
                (Some(e), Some(r13)) => format!("op[{}]: {e} ({r13})", o.op_index),
                (Some(e), None) => format!("op[{}]: {e}", o.op_index),
                (None, Some(r13)) => format!("op[{}]: {r13}", o.op_index),
                (None, None) => format!("op[{}]: rejected", o.op_index),
            })
            .collect();
        write!(
            f,
            "schema txn rejected (atomic, nothing applied): {}",
            rejected.join("; ")
        )
    }
}

impl std::error::Error for TxnError {}

/// Catalog + deprecation registries: the evolvable schema state of one
/// Database. Epoch bumps exactly once per committed txn (SGT-1.2).
#[derive(Debug, Clone, Default)]
pub struct SchemaState {
    pub catalog: Catalog,
    pub deprecations: Deprecations,
    /// Committed txn history in commit order — the catalog blob's source
    /// (SPEC-03 §3 op-log equivalent) and the journal SCHEMA_TXN payloads.
    /// GROUPED per txn: replay must bump epochs per txn, not per op (the
    /// journal records one SchemaTxn per committed txn).
    pub ops_log: Vec<Vec<SchemaOp>>,
}

impl SchemaState {
    pub fn new(catalog: Catalog) -> Self {
        Self {
            catalog,
            deprecations: Deprecations::default(),
            ops_log: Vec::new(),
        }
    }

    /// Dry-run (SPEC-01 §16): full validation, no commit, per-op verdicts.
    pub fn plan(&self, ops: &[SchemaOp]) -> Vec<OpResult> {
        let mut staged = self.clone();
        validate_ops(&mut staged, ops)
    }

    /// Atomic commit: all ops accepted or nothing applied; epoch +1.
    pub fn commit(&mut self, ops: Vec<SchemaOp>) -> Result<u64, TxnError> {
        let mut scratch = self.clone();
        scratch.catalog.schema_epoch += 1; // exactly one bump per committed txn
        let outcomes = validate_ops(&mut scratch, &ops);
        if outcomes.iter().any(|o| !o.accepted) {
            return Err(TxnError { outcomes });
        }
        let epoch = scratch.catalog.schema_epoch;
        scratch.ops_log.push(ops.clone());
        *self = scratch;
        Ok(epoch)
    }

    /// Lineage-stable node-type codes: a type's code is the index of its
    /// FIRST appearance in the committed op history — append-only, never
    /// reused, the attr_id discipline of ARCHITECTURE §4. Codes are persisted
    /// per node in snapshots, so they must not depend on per-epoch catalog
    /// order (adding a type must never re-type existing nodes). Types that
    /// entered the catalog without a `CreateNodeType` op (fixtures built
    /// directly on the catalog) follow in key order.
    pub fn node_type_codes(&self) -> BTreeMap<Key, u16> {
        let present: Vec<Key> = self.catalog.node_types.keys().cloned().collect();
        lineage_codes(&self.ops_log, &present, |op| match op {
            SchemaOp::CreateNodeType(n) => Some(n.key.clone()),
            _ => None,
        })
    }

    /// Graph ids by the same lineage rule (`CreateGraph` order).
    pub fn graph_ids(&self) -> BTreeMap<Key, u16> {
        let present: Vec<Key> = self.catalog.graphs.keys().cloned().collect();
        lineage_codes(&self.ops_log, &present, |op| match op {
            SchemaOp::CreateGraph(g) => Some(g.key.clone()),
            _ => None,
        })
    }

    /// Attribute-id lineage by the same rule (`CreateAttribute` order), in
    /// the shape [`crate::compile::compile_with_lineage`] takes as
    /// `prev_attr_ids`. Cells and parts are keyed by attr id, so a fresh
    /// open must rebuild the SAME assignment the live process used — a
    /// per-open sort would silently re-key every cell.
    pub fn attr_id_lineage(&self) -> BTreeMap<Key, lattice_core::ids::AttrId> {
        let present: Vec<Key> = self.catalog.attributes.keys().cloned().collect();
        lineage_codes(&self.ops_log, &present, |op| match op {
            SchemaOp::CreateAttribute(a) => Some(a.key.clone()),
            _ => None,
        })
        .into_iter()
        .map(|(k, id)| (k, lattice_core::ids::AttrId(id)))
        .collect()
    }
}

/// `code = index of first appearance` over the op history, then the
/// remaining `present` keys in key order. A catalog with more than
/// `u16::MAX` entities leaves the excess WITHOUT a code — any use surfaces
/// as "unknown" downstream (loud), never as a silently wrapped id.
fn lineage_codes(
    ops_log: &[Vec<SchemaOp>],
    present: &[Key],
    first: impl Fn(&SchemaOp) -> Option<Key>,
) -> BTreeMap<Key, u16> {
    let mut codes: BTreeMap<Key, u16> = BTreeMap::new();
    let mut next: u32 = 0;
    let mut assign = |codes: &mut BTreeMap<Key, u16>, key: &Key| {
        if !codes.contains_key(key) {
            if let Ok(code) = u16::try_from(next) {
                codes.insert(key.clone(), code);
            }
            next += 1;
        }
    };
    for txn in ops_log {
        for op in txn {
            if let Some(k) = first(op) {
                assign(&mut codes, &k);
            }
        }
    }
    for key in present {
        assign(&mut codes, key);
    }
    codes
}

/// Shared validation pass for plan and commit (SGT-16.3 fidelity): accepted
/// ops are applied INTO `staged` (the caller decides whether to keep it).
///
/// The P3 compile gate is TXN-level: one schema txn = one epoch = one
/// compile. Intermediate ops may transiently produce non-compilable states
/// (a state model precedes its transitions in valid bundles); only the
/// final post-txn catalog must compile. Failure rejects the whole atomic
/// txn (nothing applied) with the compile error on the synthetic trailing
/// outcome.
fn validate_ops(staged: &mut SchemaState, ops: &[SchemaOp]) -> Vec<OpResult> {
    let mut outcomes = Vec::with_capacity(ops.len() + 1);
    for (i, op) in ops.iter().enumerate() {
        match apply_op(staged, op) {
            Ok(()) => outcomes.push(OpResult {
                op_index: i,
                accepted: true,
                error: None,
                r13: None,
            }),
            Err((error, r13)) => outcomes.push(OpResult {
                op_index: i,
                accepted: false,
                error: Some(error),
                r13,
            }),
        }
    }
    // Structural/R13 rejections already gate per-op; a rejected op leaves
    // the staged catalog in whatever state the accepted prefix produced —
    // the txn-level compile still runs on that prefix (bundle authors get
    // one precise compile verdict for the txn as a whole).
    if let Err(e) = crate::compile::compile(&staged.catalog) {
        outcomes.push(OpResult {
            op_index: ops.len(),
            accepted: false,
            error: Some(e),
            r13: None,
        });
    }
    outcomes
}

type OpError = (SchemaError, Option<R13Violation>);

/// One op = structural/R13 checks on a trial clone + the P3 compile gate;
/// the op lands in `staged` only when both pass (a rejected op must never
/// pollute validation of subsequent ops). Schema txns are rare — the per-op
/// clone is deliberate simplicity (ED-07).
fn apply_op(staged: &mut SchemaState, op: &SchemaOp) -> Result<(), OpError> {
    let mut trial = staged.clone();
    apply_op_inner(&mut trial, op)?;
    *staged = trial;
    Ok(())
}

fn r13(rule: &'static str, path: impl Into<String>) -> Result<(), OpError> {
    Err((
        SchemaError::R13InUse,
        Some(R13Violation {
            rule,
            path: path.into(),
        }),
    ))
}

fn apply_op_inner(staged: &mut SchemaState, op: &SchemaOp) -> Result<(), OpError> {
    let catalog = &mut staged.catalog;
    match op {
        SchemaOp::CreateGraph(g) => catalog.create_graph(g.clone()).map_err(|e| (e, None)),
        SchemaOp::DropGraph { key } => {
            if !catalog.graphs.contains_key(key) {
                return Err((
                    SchemaError::NotFound {
                        namespace: "graph",
                        key: key.clone(),
                    },
                    None,
                ));
            }
            let mut refs = Vec::new();
            for g in &catalog.grants {
                if let GrantTarget::Graph(gk) = &g.target
                    && gk == key
                {
                    refs.push(format!("grant role=`{}` subject=`{}`", g.role, g.subject));
                }
            }
            if !refs.is_empty() {
                return r13(
                    "R13_IN_USE",
                    format!("graph `{key}` referenced by {}", refs.join(", ")),
                );
            }
            catalog.graphs.remove(key);
            Ok(())
        }
        SchemaOp::CreateEnum(e) => catalog.create_enum(e.clone()).map_err(|e2| (e2, None)),
        SchemaOp::AddEnumValue { enum_key, value } => {
            let e = catalog.enums.get_mut(enum_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "enum",
                        key: enum_key.clone(),
                    },
                    None,
                )
            })?;
            if e.values.contains(value) {
                return Err((
                    SchemaError::AlreadyExists {
                        namespace: "enum_value",
                        key: value.clone(),
                    },
                    None,
                ));
            }
            if e.values.len() >= 256 {
                return Err((
                    SchemaError::EnumBounds {
                        key: enum_key.clone(),
                        count: e.values.len() + 1,
                    },
                    None,
                ));
            }
            e.values.push(value.clone());
            staged
                .deprecations
                .enum_values
                .remove(&(enum_key.clone(), value.clone()));
            Ok(())
        }
        SchemaOp::DeprecateEnumValue { enum_key, value } => {
            let e = catalog.enums.get(enum_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "enum",
                        key: enum_key.clone(),
                    },
                    None,
                )
            })?;
            if !e.values.contains(value) {
                return Err((
                    SchemaError::NotFound {
                        namespace: "enum_value",
                        key: value.clone(),
                    },
                    None,
                ));
            }
            staged
                .deprecations
                .enum_values
                .insert((enum_key.clone(), value.clone()));
            Ok(())
        }
        SchemaOp::RemoveEnumValue { enum_key, value } => {
            let e = catalog.enums.get(enum_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "enum",
                        key: enum_key.clone(),
                    },
                    None,
                )
            })?;
            let idx = e.values.iter().position(|v| v == value).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "enum_value",
                        key: value.clone(),
                    },
                    None,
                )
            })?;
            if !staged
                .deprecations
                .enum_values
                .contains(&(enum_key.clone(), value.clone()))
            {
                return r13(
                    "R13_DRAIN",
                    format!("enum `{enum_key}` value `{value}` must be deprecated before removal"),
                );
            }
            // Report live references first (drain is the operator's next
            // step); index stability becomes the final blocker after drain.
            let refs = enum_value_refs(catalog, enum_key, idx);
            if !refs.is_empty() {
                return r13(
                    "R13_IN_USE",
                    format!(
                        "enum `{enum_key}` value `{value}` referenced by {}",
                        refs.join(", ")
                    ),
                );
            }
            if idx != e.values.len() - 1 {
                return r13(
                    "R13_INDEX_STABILITY",
                    format!(
                        "enum `{enum_key}` value `{value}` is not the last dict entry; removal would re-index live cells"
                    ),
                );
            }
            let e = catalog.enums.get_mut(enum_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "enum",
                        key: enum_key.clone(),
                    },
                    None,
                )
            })?;
            e.values.pop();
            staged
                .deprecations
                .enum_values
                .remove(&(enum_key.clone(), value.clone()));
            Ok(())
        }
        SchemaOp::CreateRecord(r) => catalog.create_record(r.clone()).map_err(|e| (e, None)),
        SchemaOp::AddRecordField {
            record_key,
            name,
            elem,
        } => {
            let r = catalog.records.get_mut(record_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "record",
                        key: record_key.clone(),
                    },
                    None,
                )
            })?;
            if r.fields.len() >= lattice_core::value::MAX_RECORD_FIELDS {
                return Err((
                    SchemaError::RecordBounds {
                        key: record_key.clone(),
                        count: r.fields.len() + 1,
                    },
                    None,
                ));
            }
            if r.fields.iter().any(|(f, _)| f == name) {
                return Err((
                    SchemaError::RecordFieldDuplicate {
                        key: record_key.clone(),
                        field: name.clone(),
                    },
                    None,
                ));
            }
            r.fields.push((name.clone(), elem.clone()));
            Ok(())
        }
        SchemaOp::RemoveRecordField { record_key, field } => {
            let r = catalog.records.get(record_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "record",
                        key: record_key.clone(),
                    },
                    None,
                )
            })?;
            if !r.fields.iter().any(|(f, _)| f == field) {
                return Err((
                    SchemaError::NotFound {
                        namespace: "record_field",
                        key: field.clone(),
                    },
                    None,
                ));
            }
            // Live references: rules counting this field over attributes of
            // this record type (SPEC-01 §5).
            let mut ref_attrs: Vec<String> = Vec::new();
            for (ak, def) in &catalog.attributes {
                if def.attr_type == AttrType::Record(record_key.clone()) {
                    ref_attrs.push(format!("{ak}"));
                }
            }
            let mut refs = Vec::new();
            for (rk, rule) in &catalog.rules {
                if let PredicateForm::CellFieldCountGte { attr, field: f, .. } = &rule.form {
                    let attr_name = attr.as_str().to_string();
                    if ref_attrs.contains(&attr_name) && f == field {
                        refs.push(format!(
                            "rule `{rk}` (CellFieldCountGte over `{attr_name}`.{field})"
                        ));
                    }
                }
            }
            if !refs.is_empty() {
                return r13(
                    "R13_IN_USE",
                    format!(
                        "record `{record_key}` field `{field}` referenced by {}",
                        refs.join(", ")
                    ),
                );
            }
            let r = catalog.records.get_mut(record_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "record",
                        key: record_key.clone(),
                    },
                    None,
                )
            })?;
            r.fields.retain(|(f, _)| f != field);
            Ok(())
        }
        SchemaOp::CreateKeyType(k) => catalog.create_key_type(k.clone()).map_err(|e| (e, None)),
        SchemaOp::CreateAttribute(a) => catalog.create_attribute(a.clone()).map_err(|e| (e, None)),
        SchemaOp::AlterAttributeCodec { attr_key, codec } => {
            let a = catalog.attributes.get_mut(attr_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "attribute",
                        key: attr_key.clone(),
                    },
                    None,
                )
            })?;
            a.codec_override = Some(codec.clone());
            Ok(())
        }
        SchemaOp::CreateAttributeSet(s) => {
            catalog.create_attr_set(s.clone()).map_err(|e| (e, None))
        }
        SchemaOp::CreateStateModel(m) => {
            catalog.create_state_model(m.clone()).map_err(|e| (e, None))
        }
        SchemaOp::AddState { model_key, state } => {
            let m = catalog.state_models.get_mut(model_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "state_model",
                        key: model_key.clone(),
                    },
                    None,
                )
            })?;
            if m.state(&state.key).is_some() {
                return Err((
                    SchemaError::AlreadyExists {
                        namespace: "state",
                        key: state.key.clone(),
                    },
                    None,
                ));
            }
            if m.class == ModelClass::Endless && (state.success_terminal || state.failure_terminal)
            {
                return Err((
                    SchemaError::EndlessHasTerminal {
                        key: model_key.clone(),
                    },
                    None,
                ));
            }
            m.states.push(state.clone());
            Ok(())
        }
        SchemaOp::AddTransition(t) => catalog.create_transition(t.clone()).map_err(|e| (e, None)),
        SchemaOp::AlterTransitionTiming {
            transition_key,
            timing,
        } => {
            let t = catalog.transitions.get_mut(transition_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "transition",
                        key: transition_key.clone(),
                    },
                    None,
                )
            })?;
            t.timing = timing.clone();
            Ok(())
        }
        SchemaOp::DeprecateState {
            model_key,
            state_key,
        } => {
            let m = catalog.state_models.get(model_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "state_model",
                        key: model_key.clone(),
                    },
                    None,
                )
            })?;
            if m.state(state_key).is_none() {
                return Err((
                    SchemaError::NotFound {
                        namespace: "state",
                        key: state_key.clone(),
                    },
                    None,
                ));
            }
            staged
                .deprecations
                .states
                .insert((model_key.clone(), state_key.clone()));
            Ok(())
        }
        SchemaOp::RemoveState {
            model_key,
            state_key,
        } => {
            let m = catalog.state_models.get(model_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "state_model",
                        key: model_key.clone(),
                    },
                    None,
                )
            })?;
            if m.state(state_key).is_none() {
                return Err((
                    SchemaError::NotFound {
                        namespace: "state",
                        key: state_key.clone(),
                    },
                    None,
                ));
            }
            // Uniform order: drain discipline first (the operator's next
            // step is always deprecate), then structural blockers.
            if !staged
                .deprecations
                .states
                .contains(&(model_key.clone(), state_key.clone()))
            {
                return r13(
                    "R13_DRAIN",
                    format!("state `{state_key}` must be deprecated before removal"),
                );
            }
            if &m.initial == state_key {
                return r13(
                    "R13_IN_USE",
                    format!("state `{state_key}` is the initial state of model `{model_key}`"),
                );
            }
            let mut refs = Vec::new();
            for (tk, t) in &catalog.transitions {
                if &t.model == model_key && (&t.from == state_key || &t.to == state_key) {
                    refs.push(format!("transition `{tk}` ({}`→{}`)", t.from, t.to));
                }
            }
            for (nk, nt) in &catalog.node_types {
                if &nt.state_model == model_key {
                    for lock in &nt.state_locks {
                        if &lock.state == state_key {
                            refs.push(format!("node type `{nk}` state lock"));
                        }
                    }
                }
            }
            if !refs.is_empty() {
                return r13(
                    "R13_IN_USE",
                    format!("state `{state_key}` referenced by {}", refs.join(", ")),
                );
            }
            let m = catalog.state_models.get_mut(model_key).ok_or_else(|| {
                (
                    SchemaError::NotFound {
                        namespace: "state_model",
                        key: model_key.clone(),
                    },
                    None,
                )
            })?;
            m.states.retain(|s| &s.key != state_key);
            staged
                .deprecations
                .states
                .remove(&(model_key.clone(), state_key.clone()));
            Ok(())
        }
        SchemaOp::CreateNodeType(n) => catalog.create_node_type(n.clone()).map_err(|e| (e, None)),
        SchemaOp::DeprecateNodeType { type_key } => {
            if !catalog.node_types.contains_key(type_key) {
                return Err((
                    SchemaError::NotFound {
                        namespace: "node_type",
                        key: type_key.clone(),
                    },
                    None,
                ));
            }
            staged.deprecations.node_types.insert(type_key.clone());
            Ok(())
        }
        SchemaOp::CreateIndex(i) => catalog.create_index(i.clone()).map_err(|e| (e, None)),
        SchemaOp::DropIndex { index_key } => {
            if catalog.indices.remove(index_key).is_none() {
                return Err((
                    SchemaError::NotFound {
                        namespace: "index",
                        key: index_key.clone(),
                    },
                    None,
                ));
            }
            Ok(())
        }
        SchemaOp::CreateRule(r) => catalog.create_rule(r.clone()).map_err(|e| (e, None)),
        SchemaOp::RemoveRule { rule_key } => {
            if !catalog.rules.contains_key(rule_key) {
                return Err((
                    SchemaError::NotFound {
                        namespace: "rule",
                        key: rule_key.clone(),
                    },
                    None,
                ));
            }
            let mut refs = Vec::new();
            for (tk, t) in &catalog.transitions {
                if t.requirements.contains(rule_key) {
                    refs.push(format!("transition `{tk}`"));
                }
            }
            if !refs.is_empty() {
                return r13(
                    "R13_IN_USE",
                    format!("rule `{rule_key}` referenced by {}", refs.join(", ")),
                );
            }
            catalog.rules.remove(rule_key);
            Ok(())
        }
        SchemaOp::CreateRole(r) => catalog.create_role(r.clone()).map_err(|e| (e, None)),
        SchemaOp::GrantOp(g) => {
            catalog.grant(g.clone());
            Ok(())
        }
        SchemaOp::Revoke {
            role,
            subject,
            target,
        } => {
            catalog.revoke(role, subject, target);
            Ok(())
        }
    }
}

/// Paths of catalog-level live references to enum value at dict index `idx`:
/// attribute defaults and rule parameters carrying `Value::Enum(idx)`.
fn enum_value_refs(catalog: &Catalog, enum_key: &Key, idx: usize) -> Vec<String> {
    let mut refs = Vec::new();
    let target = Value::Enum(idx as u8);
    for (ak, def) in &catalog.attributes {
        let is_enum_attr = matches!(&def.attr_type, AttrType::Enum(e) if e == enum_key);
        if is_enum_attr
            && let Some(d) = &def.default
            && d == &target
        {
            refs.push(format!("attribute `{ak}` default"));
        }
    }
    for (rk, rule) in &catalog.rules {
        let hits = predicate_enum_refs(&rule.form, enum_key, &target);
        if !hits.is_empty() {
            refs.push(format!("rule `{rk}` ({hits})"));
        }
    }
    refs
}

fn predicate_enum_refs(form: &PredicateForm, enum_key: &Key, target: &Value) -> String {
    let mut hit = Vec::new();
    let check = |values: &[Value], what: &str, hit: &mut Vec<String>| {
        if values.iter().any(|v| v == target) {
            hit.push(what.to_string());
        }
    };
    match form {
        PredicateForm::SetMember { attr, values } => {
            let mut h = Vec::new();
            check(values, &format!("SetMember over `{attr}`"), &mut h);
            hit.extend(h);
        }
        PredicateForm::AttrCmp { attr, value, .. }
        | PredicateForm::LatestCmp { attr, value, .. } => {
            if value == target {
                hit.push(format!(
                    "comparison over `{attr}` (attr of enum `{enum_key}`)"
                ));
            }
        }
        PredicateForm::CellFieldCountGte { attr, value, .. } if value == target => {
            hit.push(format!("CellFieldCountGte over `{attr}`"));
        }
        _ => {}
    }
    hit.join(", ")
}

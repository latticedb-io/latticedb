//! The Catalog: the meta-schema graph container (SPEC-01 §1). Every typed
//! entity exists only as catalog data, referenced by `key` — no inline
//! definitions. Inserts perform structural validation; cross-reference
//! resolution is the compiler's job (T-103) and op validation R13's (T-104).
//!
//! Epoch law (SPEC-01 §1): `schema_epoch` bumps exactly once per committed
//! schema transaction — the txn layer (T-104) owns it; raw inserts here never
//! touch it.
//!
//! Determinism: all namespaces are `BTreeMap` — iteration and serialization
//! order are total and stable (SPEC-03 D-invariants).

use crate::entities::*;
use crate::key::{Key, validate_display_name};
use std::collections::BTreeMap;

/// The built-in scalar set, in registry order (SPEC-01 §3 — non-deletable,
/// present in every fresh Database; SGT-17.1).
pub const BUILTIN_SCALARS: &[ScalarType] = &[
    ScalarType::Bool,
    ScalarType::I64,
    ScalarType::F64,
    ScalarType::Decimal {
        precision: 38,
        scale: 0,
    },
    ScalarType::String,
    ScalarType::Bytes,
    ScalarType::Timestamp,
    ScalarType::Duration,
    ScalarType::NodeRef,
];

/// Edge-kind registry (SPEC-01 §10): engine constants, not catalog entities.
pub const EDGE_KINDS: &[&str] = &[
    "AND", "OR", "KOFN", "GUARD", "INHIBIT", "XOR", "MIRROR", "WEIGHTED",
];

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Catalog {
    pub schema_epoch: u64,
    pub graphs: BTreeMap<Key, GraphSchema>,
    pub enums: BTreeMap<Key, EnumType>,
    pub records: BTreeMap<Key, RecordType>,
    pub key_types: BTreeMap<Key, KeyTypeDef>,
    pub attributes: BTreeMap<Key, AttributeDef>,
    pub attr_sets: BTreeMap<Key, AttributeSet>,
    pub attr_groups: BTreeMap<Key, AttributeGroup>,
    pub state_models: BTreeMap<Key, StateModel>,
    pub transitions: BTreeMap<Key, Transition>,
    pub node_types: BTreeMap<Key, NodeType>,
    pub indices: BTreeMap<Key, IndexDef>,
    pub rules: BTreeMap<Key, NamedRule>,
    pub roles: BTreeMap<Key, Role>,
    pub grants: Vec<Grant>,
}

impl Catalog {
    /// Fresh Database (SPEC-01 §17): built-in scalars + fixed edge registry
    /// (code-level constants), empty user catalog, zero graphs, epoch 0.
    pub fn fresh() -> Self {
        Self::default()
    }

    fn put<T>(
        namespace: &'static str,
        map: &mut BTreeMap<Key, T>,
        entity: T,
        key: Key,
    ) -> Result<(), SchemaError> {
        if map.contains_key(&key) {
            return Err(SchemaError::AlreadyExists { namespace, key });
        }
        map.insert(key, entity);
        Ok(())
    }

    pub fn create_graph(&mut self, g: GraphSchema) -> Result<(), SchemaError> {
        validate_display_name(&g.display_name).map_err(SchemaError::DisplayName)?;
        let key = g.key.clone();
        Self::put("graph", &mut self.graphs, g, key)
    }

    pub fn create_enum(&mut self, e: EnumType) -> Result<(), SchemaError> {
        let key = e.key.clone();
        Self::put("enum", &mut self.enums, e, key)
    }

    pub fn create_record(&mut self, r: RecordType) -> Result<(), SchemaError> {
        let key = r.key.clone();
        Self::put("record", &mut self.records, r, key)
    }

    pub fn create_key_type(&mut self, k: KeyTypeDef) -> Result<(), SchemaError> {
        let key = k.key.clone();
        Self::put("key_type", &mut self.key_types, k, key)
    }

    pub fn create_attribute(&mut self, a: AttributeDef) -> Result<(), SchemaError> {
        let key = a.key.clone();
        Self::put("attribute", &mut self.attributes, a, key)
    }

    pub fn create_attr_set(&mut self, s: AttributeSet) -> Result<(), SchemaError> {
        let key = s.key.clone();
        Self::put("attr_set", &mut self.attr_sets, s, key)
    }

    pub fn create_attr_group(&mut self, g: AttributeGroup) -> Result<(), SchemaError> {
        validate_display_name(&g.display_name).map_err(SchemaError::DisplayName)?;
        let key = g.key.clone();
        Self::put("attr_group", &mut self.attr_groups, g, key)
    }

    pub fn create_state_model(&mut self, m: StateModel) -> Result<(), SchemaError> {
        let key = m.key.clone();
        Self::put("state_model", &mut self.state_models, m, key)
    }

    pub fn create_transition(&mut self, t: Transition) -> Result<(), SchemaError> {
        let key = t.key.clone();
        Self::put("transition", &mut self.transitions, t, key)
    }

    pub fn create_node_type(&mut self, n: NodeType) -> Result<(), SchemaError> {
        validate_display_name(&n.display_name).map_err(SchemaError::DisplayName)?;
        let key = n.key.clone();
        Self::put("node_type", &mut self.node_types, n, key)
    }

    pub fn create_index(&mut self, i: IndexDef) -> Result<(), SchemaError> {
        let key = i.key.clone();
        Self::put("index", &mut self.indices, i, key)
    }

    pub fn create_rule(&mut self, r: NamedRule) -> Result<(), SchemaError> {
        let key = r.key.clone();
        Self::put("rule", &mut self.rules, r, key)
    }

    pub fn create_role(&mut self, r: Role) -> Result<(), SchemaError> {
        let key = r.key.clone();
        Self::put("role", &mut self.roles, r, key)
    }

    /// Grant: journaled binding (SPEC-01 §14); duplicates are idempotent-noops
    /// at catalog level (the journal layer decides whether to record).
    pub fn grant(&mut self, g: Grant) {
        let duplicate = self.grants.iter().any(|existing| {
            existing.role == g.role && existing.subject == g.subject && existing.target == g.target
        });
        if !duplicate {
            self.grants.push(g);
        }
    }

    /// Revoke: removes all bindings matching (role, subject, target); returns
    /// the number of revoked bindings.
    pub fn revoke(&mut self, role: &Key, subject: &str, target: &GrantTarget) -> usize {
        let before = self.grants.len();
        self.grants
            .retain(|g| !(g.role == *role && g.subject == subject && g.target == *target));
        before - self.grants.len()
    }
}

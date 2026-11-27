//! The schema compiler (ARCHITECTURE §7, SPEC-01 §8/§13): turns the catalog
//! graph into dispatch masks, requirement read-sets, the enabled-action index
//! and index plans — the compile step of P3 "schema is a graph, compiled".
//!
//! Determinism: attribute ids are assigned in `BTreeMap` key order; every
//! output map is a `BTreeMap`; masks are [`BitSet`]s compared by bytes.
//! `compile(fresh_catalog + entities)` is a pure function — same input ⇒
//! equal [`Compiled`] (recompile identity is acceptance-tested).
//!
//! Compile-time validations here:
//! - reference resolution for every cross-entity pointer (`NotFound`),
//! - AttributeSet homogeneity (SPEC-01 §7, `MixedSet`),
//! - parent_type cycle check (SGT-11.3, `TypeCycle`),
//! - finalizing models: DAG + ranks = longest-path depth from `initial`
//!   (SGT-8.5); backward transitions rejected (`BackwardRank`, SGT-8.4);
//!   cycles rejected (`FinalizingCycle`); unreachable states rejected,
//! - monotonicity pairing for comparison predicates (SGT-13.2,
//!   `UnresolvedMonotonicity`).

use crate::catalog::Catalog;
use crate::entities::*;
use crate::key::Key;
use lattice_core::bitset::BitSet;
use lattice_core::ids::AttrId;
use std::collections::{BTreeMap, BTreeSet};

/// Per-model runtime view (SPEC-01 §8): class, states with terminals,
/// initial, ranks (finalizing only), and the declared motion table.
#[derive(Debug, Clone, PartialEq)]
pub struct CompiledModel {
    pub class: ModelClass,
    /// (state key, success_terminal, failure_terminal) in declaration order.
    pub states: Vec<(Key, bool, bool)>,
    pub initial: Key,
    /// Finalizing only: state → longest-path depth from initial.
    pub ranks: BTreeMap<Key, u64>,
    /// Declared motions: (from, to) → transition keys.
    pub motions: BTreeMap<(Key, Key), Vec<Key>>,
}

/// Compiled dispatch structures for one schema epoch.
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    pub epoch: u64,
    /// Deterministic attr_id assignment (sorted key order).
    pub attr_ids: BTreeMap<Key, AttrId>,
    pub attr_count: usize,
    /// Effective attribute mask per node type (own + inherited sets).
    pub effective_attrs: BTreeMap<Key, BitSet>,
    /// (node_type, state) → writable attribute mask (state locks; absence of
    /// a lock for a state = all effective attributes writable — absence
    /// semantics, SPEC-01 §7).
    pub write_masks: BTreeMap<(Key, Key), BitSet>,
    /// (node_type, from_state) → invokable transition keys (non-empty actor
    /// classes). Requirement gates flip bits at runtime (M2).
    pub action_index: BTreeMap<(Key, Key), Vec<Key>>,
    /// Transition → union of requirement-rule attribute read-sets.
    pub read_sets: BTreeMap<Key, BitSet>,
    /// Rule → resolved attribute references.
    pub rule_attrs: BTreeMap<Key, Vec<AttrId>>,
    /// (model, state) → rank (longest-path depth from initial); finalizing
    /// models only.
    pub ranks: BTreeMap<(Key, Key), u64>,
    /// Resolved index definitions (rebuild plans for T-107).
    pub index_plans: BTreeMap<Key, IndexKind>,
    /// Runtime FSM view per state model (T-201 registry input).
    pub models: BTreeMap<Key, CompiledModel>,
}

/// Compile the full catalog for its current epoch.
pub fn compile(catalog: &Catalog) -> Result<Compiled, SchemaError> {
    // -- A: deterministic attribute-id assignment ---------------------------
    let attr_ids: BTreeMap<Key, AttrId> = catalog
        .attributes
        .keys()
        .enumerate()
        .map(|(i, k)| (k.clone(), AttrId(i as u16)))
        .collect();
    let attr_count = catalog.attributes.len();
    let id_of = |k: &Key| -> Result<AttrId, SchemaError> {
        attr_ids
            .get(k)
            .copied()
            .ok_or_else(|| SchemaError::NotFound {
                namespace: "attribute",
                key: k.clone(),
            })
    };

    // -- B: rules — reference resolution + monotonicity pairing -------------
    let mut rule_attrs: BTreeMap<Key, Vec<AttrId>> = BTreeMap::new();
    for (key, rule) in &catalog.rules {
        let mut refs = BTreeSet::new();
        collect_rule_attrs(&rule.form, |attr_key| {
            let id = id_of(attr_key)?;
            // attr_cmp / latest_cmp read attributes directionally: the attr
            // must declare monotonicity (SGT-13.2 — rejected at creation).
            let directional = matches!(
                rule.form,
                PredicateForm::AttrCmp { .. } | PredicateForm::LatestCmp { .. }
            );
            if directional {
                let def =
                    catalog
                        .attributes
                        .get(attr_key)
                        .ok_or_else(|| SchemaError::NotFound {
                            namespace: "attribute",
                            key: attr_key.clone(),
                        })?;
                if def.monotonicity == Monotonicity::None {
                    return Err(SchemaError::UnresolvedMonotonicity {
                        rule: key.clone(),
                        attr: attr_key.clone(),
                    });
                }
            }
            refs.insert(id);
            Ok(())
        })?;
        rule_attrs.insert(key.clone(), refs.into_iter().collect());
    }

    // -- C: attribute sets — resolution + homogeneity ------------------------
    let mut set_attrs: BTreeMap<Key, Vec<AttrId>> = BTreeMap::new();
    for (key, set) in &catalog.attr_sets {
        let mut ids = Vec::new();
        let mut saw_vector = false;
        let mut saw_scalar_family = false;
        for m in &set.members {
            let def =
                catalog
                    .attributes
                    .get(m)
                    .ok_or_else(|| SchemaError::UnresolvedSetMember {
                        set: key.clone(),
                        attr: m.clone(),
                    })?;
            if matches!(def.attr_type, AttrType::Vector { .. }) {
                saw_vector = true;
            } else {
                saw_scalar_family = true;
            }
            ids.push(id_of(m)?);
        }
        if saw_vector && saw_scalar_family {
            return Err(SchemaError::MixedSet { key: key.clone() });
        }
        let homogenous = matches!(
            (set.kind, saw_vector),
            (SetKind::ScalarSet, false) | (SetKind::TableSet, true)
        );
        if !homogenous {
            return Err(SchemaError::MixedSet { key: key.clone() });
        }
        set_attrs.insert(key.clone(), ids);
    }

    // -- D: element-type reference resolution --------------------------------
    for def in catalog.attributes.values() {
        resolve_attr_type(&def.attr_type, catalog)?;
    }
    for rec in catalog.records.values() {
        for (_, elem) in &rec.fields {
            if let ElemType::Enum(e) = elem
                && !catalog.enums.contains_key(e)
            {
                return Err(SchemaError::NotFound {
                    namespace: "enum",
                    key: e.clone(),
                });
            }
        }
    }
    for t in catalog.transitions.values() {
        if let Some(pt) = &t.payload_type
            && !catalog.records.contains_key(pt)
        {
            return Err(SchemaError::NotFound {
                namespace: "record",
                key: pt.clone(),
            });
        }
        if let Some(od) = &t.timing.on_deadline
            && !catalog.transitions.contains_key(od)
        {
            return Err(SchemaError::NotFound {
                namespace: "transition",
                key: od.clone(),
            });
        }
        for e in &t.effects {
            if let Effect::Spawn { instance_of } = e
                && !catalog.node_types.contains_key(instance_of)
            {
                return Err(SchemaError::NotFound {
                    namespace: "node_type",
                    key: instance_of.clone(),
                });
            }
        }
        for r in &t.requirements {
            if !catalog.rules.contains_key(r) {
                return Err(SchemaError::NotFound {
                    namespace: "rule",
                    key: r.clone(),
                });
            }
        }
    }
    for idx in catalog.indices.values() {
        match &idx.kind {
            IndexKind::ByState { type_key } => {
                if !catalog.node_types.contains_key(type_key) {
                    return Err(SchemaError::NotFound {
                        namespace: "node_type",
                        key: type_key.clone(),
                    });
                }
            }
            IndexKind::AttrPresence { attr } => {
                if !catalog.attributes.contains_key(attr) {
                    return Err(SchemaError::NotFound {
                        namespace: "attribute",
                        key: attr.clone(),
                    });
                }
            }
            IndexKind::SetPresence { set_key } => {
                if !catalog.attr_sets.contains_key(set_key) {
                    return Err(SchemaError::NotFound {
                        namespace: "attr_set",
                        key: set_key.clone(),
                    });
                }
            }
        }
    }

    // -- E: type forest — parent resolution + cycle check + inheritance -----
    let mut type_parent: BTreeMap<Key, Option<Key>> = BTreeMap::new();
    for (key, nt) in &catalog.node_types {
        if let Some(p) = &nt.parent_type
            && !catalog.node_types.contains_key(p)
        {
            return Err(SchemaError::NotFound {
                namespace: "node_type",
                key: p.clone(),
            });
        }
        if !catalog.state_models.contains_key(&nt.state_model) {
            return Err(SchemaError::NotFound {
                namespace: "state_model",
                key: nt.state_model.clone(),
            });
        }
        if !catalog.key_types.contains_key(&nt.key_type) {
            return Err(SchemaError::NotFound {
                namespace: "key_type",
                key: nt.key_type.clone(),
            });
        }
        for s in &nt.attribute_sets {
            if !catalog.attr_sets.contains_key(s) {
                return Err(SchemaError::NotFound {
                    namespace: "attr_set",
                    key: s.clone(),
                });
            }
        }
        type_parent.insert(key.clone(), nt.parent_type.clone());
    }
    for key in catalog.node_types.keys() {
        // walk to root; a repeated key = cycle (SGT-11.3)
        let mut seen = BTreeSet::new();
        let mut cur = Some(key.clone());
        while let Some(k) = cur {
            if !seen.insert(k.clone()) {
                return Err(SchemaError::TypeCycle { key: key.clone() });
            }
            cur = type_parent.get(&k).cloned().flatten();
        }
    }

    // Effective attribute sets per type: own sets first (wins conflicts),
    // then ancestor sets walking toward the root (SPEC-01 §11).
    let mut effective_attrs: BTreeMap<Key, BitSet> = BTreeMap::new();
    for key in catalog.node_types.keys() {
        let mut ordered_sets: Vec<Key> = Vec::new();
        let mut cur = Some(key.clone());
        while let Some(k) = cur {
            if let Some(nt) = catalog.node_types.get(&k) {
                for s in &nt.attribute_sets {
                    if !ordered_sets.contains(s) {
                        ordered_sets.push(s.clone());
                    }
                }
            }
            cur = type_parent.get(&k).cloned().flatten();
        }
        let mut mask = BitSet::new(attr_count);
        for s in &ordered_sets {
            for id in &set_attrs[s] {
                mask.set(id.0 as usize);
            }
        }
        effective_attrs.insert(key.clone(), mask);
    }

    // -- F: ranks per finalizing model ---------------------------------------
    let mut ranks: BTreeMap<(Key, Key), u64> = BTreeMap::new();
    for (mkey, model) in &catalog.state_models {
        // transitions of this model: (from, to)
        let mut edges: Vec<(Key, Key)> = Vec::new();
        for t in catalog.transitions.values() {
            if &t.model == mkey {
                let from_ok = model.state(&t.from).is_some();
                let to_ok = model.state(&t.to).is_some();
                if !from_ok {
                    return Err(SchemaError::NotFound {
                        namespace: "state",
                        key: t.from.clone(),
                    });
                }
                if !to_ok {
                    return Err(SchemaError::NotFound {
                        namespace: "state",
                        key: t.to.clone(),
                    });
                }
                edges.push((t.from.clone(), t.to.clone()));
            }
        }
        if model.class == ModelClass::Endless {
            continue; // cycles legal; no ranks
        }
        // Cycle detection: DFS from `initial` with colors (gray hit = cycle).
        // Reachability: only states reachable from `initial` get ranks.
        let mut color: BTreeMap<Key, u8> =
            model.states.iter().map(|s| (s.key.clone(), 0u8)).collect();
        let mut adj: BTreeMap<Key, Vec<Key>> = BTreeMap::new();
        for (from, to) in &edges {
            adj.entry(from.clone()).or_default().push(to.clone());
        }
        let mut post: Vec<Key> = Vec::new();
        let mut stack: Vec<(Key, usize)> = vec![(model.initial.clone(), 0)];
        color.insert(model.initial.clone(), 1);
        while let Some((node, idx)) = stack.pop() {
            let children = adj.get(&node).cloned().unwrap_or_default();
            if idx < children.len() {
                stack.push((node.clone(), idx + 1));
                let child = &children[idx];
                match color.get(child).copied().unwrap_or(0) {
                    0 => {
                        color.insert(child.clone(), 1);
                        stack.push((child.clone(), 0));
                    }
                    1 => {
                        return Err(SchemaError::FinalizingCycle {
                            model: mkey.clone(),
                        });
                    }
                    _ => {}
                }
            } else {
                color.insert(node.clone(), 2);
                post.push(node);
            }
        }

        // Longest-path ranks from initial (SGT-8.5): |V| relaxation passes
        // suffice on a DAG — and the DFS above proved reachability acyclic.
        let mut rank: BTreeMap<Key, u64> = BTreeMap::new();
        rank.insert(model.initial.clone(), 0);
        for _ in 0..model.states.len() {
            let mut changed = false;
            for (from, to) in &edges {
                if let Some(rf) = rank.get(from).copied() {
                    let cand = rf + 1;
                    if cand > rank.get(to).copied().unwrap_or(0) {
                        rank.insert(to.clone(), cand);
                        changed = true;
                    }
                }
            }
            if !changed {
                break;
            }
        }
        for s in &model.states {
            let r = rank
                .get(&s.key)
                .copied()
                .ok_or_else(|| SchemaError::UnreachableState {
                    model: mkey.clone(),
                    state: s.key.clone(),
                })?;
            ranks.insert((mkey.clone(), s.key.clone()), r);
        }
        // Backward prohibition (SGT-8.4): each transition must go strictly
        // forward, ranked against the model graph WITHOUT itself (the
        // spec's incremental semantics — an offending edge must not absorb
        // itself into the longest path it is judged on). Full-graph ranks
        // (above) remain the published epoch ranks.
        for e_idx in 0..edges.len() {
            let mut red: BTreeMap<Key, u64> = BTreeMap::new();
            red.insert(model.initial.clone(), 0);
            for _ in 0..model.states.len() {
                let mut changed = false;
                for (i, (from, to)) in edges.iter().enumerate() {
                    if i == e_idx {
                        continue;
                    }
                    if let Some(rf) = red.get(from).copied() {
                        let cand = rf + 1;
                        if cand > red.get(to).copied().unwrap_or(0) {
                            red.insert(to.clone(), cand);
                            changed = true;
                        }
                    }
                }
                if !changed {
                    break;
                }
            }
            let (from, to) = &edges[e_idx];
            if let (Some(rf), Some(rt)) = (red.get(from).copied(), red.get(to).copied())
                && rt <= rf
            {
                return Err(SchemaError::BackwardRank {
                    model: mkey.clone(),
                    from: from.clone(),
                    to: to.clone(),
                    from_rank: rf,
                    to_rank: rt,
                });
            }
        }
    }

    // -- G: write masks per (type, state) ------------------------------------
    let mut write_masks: BTreeMap<(Key, Key), BitSet> = BTreeMap::new();
    for (tkey, nt) in &catalog.node_types {
        let model = &catalog.state_models[&nt.state_model];
        let eff = &effective_attrs[tkey];
        for s in &model.states {
            let mut mask = eff.clone();
            if let Some(lock) = nt.state_locks.iter().find(|l| l.state == s.key) {
                let mut allowed = BitSet::new(attr_count);
                for ws in &lock.writable_sets {
                    if let Some(ids) = set_attrs.get(ws) {
                        for id in ids {
                            allowed.set(id.0 as usize);
                        }
                    } else {
                        return Err(SchemaError::NotFound {
                            namespace: "attr_set",
                            key: ws.clone(),
                        });
                    }
                }
                mask.intersect_with(&allowed);
            }
            write_masks.insert((tkey.clone(), s.key.clone()), mask);
        }
    }

    // -- H: action index + read sets -----------------------------------------
    let mut action_index: BTreeMap<(Key, Key), Vec<Key>> = BTreeMap::new();
    let mut read_sets: BTreeMap<Key, BitSet> = BTreeMap::new();
    for (tkey, nt) in &catalog.node_types {
        let model = &catalog.state_models[&nt.state_model];
        for s in &model.states {
            let mut actions = Vec::new();
            for (tk, t) in &catalog.transitions {
                if t.model == nt.state_model && t.from == s.key && t.is_action() {
                    actions.push(tk.clone());
                }
            }
            if !actions.is_empty() {
                action_index.insert((tkey.clone(), s.key.clone()), actions);
            }
        }
    }
    for (tk, t) in &catalog.transitions {
        let mut mask = BitSet::new(attr_count);
        for r in &t.requirements {
            // Missing rules were rejected in the D pass; every key resolves.
            if let Some(ids) = rule_attrs.get(r) {
                for id in ids {
                    mask.set(id.0 as usize);
                }
            }
        }
        read_sets.insert(tk.clone(), mask);
    }

    // Transition from/to states must exist (models may be shared by types).
    for (tk, t) in &catalog.transitions {
        let model = catalog
            .state_models
            .get(&t.model)
            .ok_or_else(|| SchemaError::NotFound {
                namespace: "state_model",
                key: t.model.clone(),
            })?;
        if model.state(&t.from).is_none() {
            return Err(SchemaError::NotFound {
                namespace: "state",
                key: t.from.clone(),
            });
        }
        if model.state(&t.to).is_none() {
            return Err(SchemaError::NotFound {
                namespace: "state",
                key: t.to.clone(),
            });
        }
        let _ = tk;
    }

    let index_plans = catalog
        .indices
        .iter()
        .map(|(k, i)| (k.clone(), i.kind.clone()))
        .collect();

    // -- I: runtime FSM view (T-201) ----------------------------------------
    let mut models: BTreeMap<Key, CompiledModel> = BTreeMap::new();
    for (mkey, model) in &catalog.state_models {
        let mut motions: BTreeMap<(Key, Key), Vec<Key>> = BTreeMap::new();
        for (tk, t) in &catalog.transitions {
            if &t.model == mkey {
                motions
                    .entry((t.from.clone(), t.to.clone()))
                    .or_default()
                    .push(tk.clone());
            }
        }
        let model_ranks: BTreeMap<Key, u64> = ranks
            .iter()
            .filter(|((m, _), _)| m == mkey)
            .map(|((_, s), r)| (s.clone(), *r))
            .collect();
        models.insert(
            mkey.clone(),
            CompiledModel {
                class: model.class,
                states: model
                    .states
                    .iter()
                    .map(|s| (s.key.clone(), s.success_terminal, s.failure_terminal))
                    .collect(),
                initial: model.initial.clone(),
                ranks: model_ranks,
                motions,
            },
        );
    }

    Ok(Compiled {
        epoch: catalog.schema_epoch,
        attr_ids,
        attr_count,
        effective_attrs,
        write_masks,
        action_index,
        read_sets,
        rule_attrs,
        ranks,
        index_plans,
        models,
    })
}

/// Resolve an attribute type's enum/record references.
fn resolve_attr_type(at: &AttrType, catalog: &Catalog) -> Result<(), SchemaError> {
    match at {
        AttrType::Scalar(_) => Ok(()),
        AttrType::Enum(e) => {
            if catalog.enums.contains_key(e) {
                Ok(())
            } else {
                Err(SchemaError::NotFound {
                    namespace: "enum",
                    key: e.clone(),
                })
            }
        }
        AttrType::Record(r) => {
            if catalog.records.contains_key(r) {
                Ok(())
            } else {
                Err(SchemaError::NotFound {
                    namespace: "record",
                    key: r.clone(),
                })
            }
        }
        AttrType::Vector { elem } => match elem {
            ElemType::Scalar(_) => Ok(()),
            ElemType::Enum(e) => {
                if catalog.enums.contains_key(e) {
                    Ok(())
                } else {
                    Err(SchemaError::NotFound {
                        namespace: "enum",
                        key: e.clone(),
                    })
                }
            }
        },
    }
}

/// Visit every attribute the predicate form references.
fn collect_rule_attrs(
    form: &PredicateForm,
    mut f: impl FnMut(&Key) -> Result<(), SchemaError>,
) -> Result<(), SchemaError> {
    match form {
        PredicateForm::AttrCmp { attr, .. }
        | PredicateForm::AttrPresent { attr }
        | PredicateForm::CellCountGte { attr, .. }
        | PredicateForm::CellFieldCountGte { attr, .. }
        | PredicateForm::LatestCmp { attr, .. }
        | PredicateForm::EpochGte { attr, .. }
        | PredicateForm::SetMember { attr, .. }
        | PredicateForm::StrEq { attr, .. }
        | PredicateForm::StrPrefix { attr, .. } => f(attr),
        PredicateForm::ChildrenState { .. } => Ok(()),
    }
}

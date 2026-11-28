//! The authoritative in-memory state of one database and the single mutation
//! primitive that live execution and journal replay share.
//!
//! Law of the assembly (T-313): a command is PLANNED read-only against the
//! current state into journal records; the records are then journaled and
//! APPLIED. Replay applies the same records through the same [`EngineState::apply`],
//! so "live = replay(journal)" holds by construction (ARCHITECTURE §5.1, §5.3).
//!
//! - `plan` validates: R2 containment, R3 acyclicity, R6 schema conformance and
//!   state locks, R7 monotonicity, R12 append-only axis, action dispatch checks.
//!   It never mutates; ids of vertices it will create are PEEKED so the record
//!   is complete before it is journaled.
//! - `apply` mutates. It trusts the journal: any error here is an invariant
//!   failure after the sequencing point ([`TxError::Invariant`]).

use crate::command::{Command, Reply};
use crate::error::{KernelError, TxError, codes, invariant};
use crate::records::{Author, Record, RecordData};
use lattice_core::arrays::GraphArrays;
use lattice_core::ids::{EdgeId, GraphId, KeyIndex, Lsn, NodeId, NodeIdSpace};
use lattice_core::value::Value;
use lattice_engine::algebra::validate_guard_params;
use lattice_engine::dispatch::{DispatchError, EngineFact, kinds};
use lattice_engine::propagation::{Derivation, Propagator};
use lattice_schema::catalog::Catalog;
use lattice_schema::compile::compile_with_lineage;
use lattice_schema::entities::{ActorClass, AttrType, SetKind};
use lattice_schema::key::Key;
use lattice_schema::txn::{SchemaOp, SchemaState};
use lattice_validate::{RequiredScope, Validators};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// The acting principal. Library mode uses [`Subject::trusted`] (the host
/// process holds every scope, ARCHITECTURE §10). The subject rides into
/// ACTION records as the journal author; scope ENFORCEMENT against catalog
/// grants is R11 (T-318).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub id: String,
    pub scopes: Vec<RequiredScope>,
}

impl Subject {
    /// Every scope — the trusted host-process subject.
    pub fn trusted(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            scopes: vec![
                RequiredScope::ReadSubtree,
                RequiredScope::WriteStructure,
                RequiredScope::WriteAttribute,
                RequiredScope::InvokeAction,
                RequiredScope::Administer,
            ],
        }
    }
}

/// Watch event kinds (SPEC-02 §9 closed set; values = wire bit positions).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventKind {
    State = 0,
    Attr = 1,
    Struct = 2,
    Action = 3,
    Enabled = 4,
}

/// One mutation event of a committed unit, in journal order (P16: egress is
/// a projection of the journal from a single emission point).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub kind: EventKind,
    pub node: NodeId,
    /// Transition an `Action` event fired.
    pub transition: Option<Key>,
    /// `Enabled` events: did the transition become enabled (`true`) or
    /// disabled; `transition` names it.
    pub enabled: Option<bool>,
}

impl Event {
    fn simple(kind: EventKind, node: NodeId) -> Self {
        Self {
            kind,
            node,
            transition: None,
            enabled: None,
        }
    }
}

/// A validated command: the records it will journal and the reply it earns.
#[derive(Debug, Clone, PartialEq)]
pub struct Planned {
    pub records: Vec<Record>,
    pub reply: Reply,
}

/// Which key a cell write targets — the R12 rule differs per case.
enum CellK<'a> {
    /// `ATTR_SET`: k = 0 on a scalar / enum / record attribute.
    Scalar,
    /// `CELLS_APPEND`: explicit `k`, must sit above `after` (the axis max so far).
    Append { k: u64, after: Option<&'a u64> },
    /// `ROW_APPEND`: k is assigned at apply.
    Row,
}

/// Node context of a cell write. `node` is `None` for the initial attributes
/// of a vertex that is still being planned.
struct WriteCtx<'a> {
    node: Option<NodeId>,
    type_key: &'a Key,
    state_key: &'a Key,
}

/// The staging tail: cells written since the last checkpoint, by
/// `(node, attr id)` → `k → (lsn, value)`. Parts hold everything up to the
/// checkpoint; a column read is parts ⊕ staging (ARCHITECTURE §8 "staging").
pub type Staging = BTreeMap<(u32, u16), BTreeMap<u64, (u64, Value)>>;

/// The in-memory database: schema + engine (structure, cells, FSM, timers).
#[derive(Clone)]
pub struct EngineState {
    pub schema: SchemaState,
    prop: Propagator,
    /// Committed units applied so far — the explicit-txn conflict stamp.
    pub write_seq: u64,
    /// Cells written since the last checkpoint (cleared by the kernel's
    /// checkpoint once they live in parts).
    staging: Staging,
    pending: Vec<Event>,
    /// Enabled-action sets of the nodes a unit touches, taken before it
    /// applied; the post-quiesce diff yields `action_enabled` / `disabled`.
    enabled_before: BTreeMap<NodeId, BTreeSet<Key>>,
    /// Vertices the last applied ACTION creates, in order — the
    /// engine-authored `ADD_VERTEX` records that follow it must match exactly.
    engine_spawns: VecDeque<(Option<u32>, Key)>,
}

impl EngineState {
    /// A fresh, empty database state over the given schema.
    pub fn fresh(schema: SchemaState) -> Result<Self, TxError> {
        Self::build(schema, GraphArrays::new(NodeIdSpace::new()))
    }

    /// State over a snapshot's structural arrays; cells are loaded after.
    pub fn restore(schema: SchemaState, arrays: GraphArrays) -> Result<Self, TxError> {
        Self::build(schema, arrays)
    }

    fn build(schema: SchemaState, arrays: GraphArrays) -> Result<Self, TxError> {
        let compiled = compile_with_lineage(&schema.catalog, Some(&schema.attr_id_lineage()))?;
        let type_codes = schema.node_type_codes();
        let machine = lattice_engine::dispatch::Machine::with_type_codes(
            arrays,
            compiled.clone(),
            &schema.catalog,
            &type_codes,
        );
        Ok(Self {
            prop: Propagator::new(machine, &compiled),
            schema,
            write_seq: 0,
            staging: Staging::new(),
            pending: Vec::new(),
            enabled_before: BTreeMap::new(),
            engine_spawns: VecDeque::new(),
        })
    }

    pub fn catalog(&self) -> &Catalog {
        &self.schema.catalog
    }

    pub fn machine(&self) -> &lattice_engine::dispatch::Machine {
        &self.prop.machine
    }

    pub fn arrays(&self) -> &GraphArrays {
        &self.prop.machine.arrays
    }

    /// The staging tail (read-only view for column reads).
    pub fn staging(&self) -> &Staging {
        &self.staging
    }

    /// Checkpoint done: everything staged now lives in parts.
    pub fn clear_staging(&mut self) {
        self.staging.clear();
    }

    /// Events of the last committed unit, in journal order (drained by the
    /// kernel after each unit — the single emission point).
    pub fn take_events(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.pending)
    }

    // -- planning ---------------------------------------------------------------

    /// Validate a command against the current state and produce the records
    /// it would journal. Read-only: a refused command changes nothing.
    pub fn plan(&self, cmd: &Command, subject: &Subject) -> Result<Planned, KernelError> {
        match cmd {
            Command::CreateNode {
                graph,
                node_type,
                parent,
                attrs,
            } => self.plan_create_node(graph, node_type, *parent, attrs),
            Command::DeleteNode { node, cascade } => self.plan_delete_node(*node, *cascade),
            Command::Link {
                src,
                dst,
                kind,
                aux,
                guard,
            } => self.plan_link(*src, *dst, *kind, *aux, guard.as_ref()),
            Command::Unlink { edge } => self.plan_unlink(*edge),
            Command::AttrSet { node, attr, value } => {
                let (type_key, state_key) = self.node_ctx(*node)?;
                self.check_cell(
                    &WriteCtx {
                        node: Some(*node),
                        type_key: &type_key,
                        state_key: &state_key,
                    },
                    attr,
                    CellK::Scalar,
                    value,
                )?;
                Ok(Planned {
                    records: vec![Record::new(RecordData::SetAttr {
                        node: node.0,
                        attr: attr.clone(),
                        value: value.clone(),
                    })],
                    reply: Reply::Attr { node: *node },
                })
            }
            Command::CellsAppend { node, attr, cells } => {
                if cells.is_empty() {
                    return Err(KernelError::new(
                        codes::INVALID_KEY,
                        "",
                        "CELLS_APPEND without cells",
                    ));
                }
                let (type_key, state_key) = self.node_ctx(*node)?;
                let ctx = WriteCtx {
                    node: Some(*node),
                    type_key: &type_key,
                    state_key: &state_key,
                };
                // The axis max so far: the first appended k must clear it.
                let current = self.machine().max_k(*node, attr);
                let mut after: Option<&u64> = current.as_ref();
                for (k, value) in cells {
                    self.check_cell(&ctx, attr, CellK::Append { k: *k, after }, value)?;
                    after = Some(k);
                }
                Ok(Planned {
                    records: vec![Record::new(RecordData::AppendCells {
                        node: node.0,
                        attr: attr.clone(),
                        cells: cells.clone(),
                    })],
                    reply: Reply::Attr { node: *node },
                })
            }
            Command::RowAppend { node, set, rows } => self.plan_row_append(*node, set, rows),
            Command::Action {
                node,
                transition,
                actor,
                payload,
            } => self.plan_action(*node, transition, *actor, payload.as_ref(), subject),
            Command::SchemaTxn { ops } => {
                let outcomes = self.schema.plan(ops);
                if let Some(bad) = outcomes.iter().find(|o| !o.accepted) {
                    return Err(schema_rejection(bad));
                }
                Ok(Planned {
                    records: vec![Record::new(RecordData::SchemaTxn { ops: ops.clone() })],
                    reply: Reply::Schema {
                        epoch: self.schema.catalog.schema_epoch + 1,
                    },
                })
            }
            Command::Grant { grant } => {
                if !self.schema.catalog.roles.contains_key(&grant.role) {
                    return Err(KernelError::new(
                        codes::NOT_FOUND,
                        KernelError::name_hint(
                            "role",
                            self.schema
                                .catalog
                                .roles
                                .keys()
                                .map(|k| k.as_str().to_string()),
                        ),
                        format!("role `{}` is not declared", grant.role),
                    ));
                }
                Ok(Planned {
                    records: vec![Record::new(RecordData::Grant {
                        grant: grant.clone(),
                    })],
                    reply: Reply::Ok,
                })
            }
            Command::Revoke {
                role,
                subject: grantee,
                target,
            } => {
                let known = self
                    .schema
                    .catalog
                    .grants
                    .iter()
                    .any(|g| &g.role == role && &g.subject == grantee && &g.target == target);
                if !known {
                    return Err(KernelError::new(
                        codes::NOT_FOUND,
                        "",
                        format!("no grant of role `{role}` to subject `{grantee}` at this target"),
                    ));
                }
                Ok(Planned {
                    records: vec![Record::new(RecordData::Revoke {
                        role: role.clone(),
                        subject: grantee.clone(),
                        target: target.clone(),
                    })],
                    reply: Reply::Ok,
                })
            }
        }
    }

    fn plan_create_node(
        &self,
        graph: &Key,
        node_type: &Key,
        parent: Option<NodeId>,
        attrs: &[(Key, Value)],
    ) -> Result<Planned, KernelError> {
        let graphs = self.schema.graph_ids();
        let Some(&graph_id) = graphs.get(graph) else {
            return Err(KernelError::new(
                codes::NOT_FOUND,
                KernelError::name_hint("graph", graphs.keys().map(|k| k.as_str().to_string())),
                format!("graph `{graph}` is not declared"),
            ));
        };
        let Some(nt) = self.schema.catalog.node_types.get(node_type).cloned() else {
            return Err(KernelError::new(
                codes::NOT_FOUND,
                KernelError::name_hint(
                    "type",
                    self.schema
                        .catalog
                        .node_types
                        .keys()
                        .map(|k| k.as_str().to_string()),
                ),
                format!("node type `{node_type}` is not declared"),
            ));
        };
        if let Some(p) = parent {
            let parent_type = self.type_key_of(p)?;
            Self::containment(node_type, nt.parent_type.as_ref(), &parent_type)?;
        } else if let Some(required) = nt.parent_type.as_ref() {
            return Err(KernelError::new(
                codes::CONTAINMENT,
                format!("type `{node_type}` requires a parent of type `{required}`"),
                format!("`{node_type}` created without a parent"),
            ));
        }
        let initial = self.initial_state_key(node_type)?;
        let id = self.machine().peek_next_node_id();
        let mut records = vec![Record::new(RecordData::AddVertex {
            node: id.0,
            parent: parent.map(|p| p.0),
            graph: graph_id,
            node_type: node_type.clone(),
            authored: Author::User,
        })];
        for (attr, value) in attrs {
            self.check_cell(
                &WriteCtx {
                    node: None,
                    type_key: node_type,
                    state_key: &initial,
                },
                attr,
                CellK::Scalar,
                value,
            )?;
            records.push(Record::new(RecordData::SetAttr {
                node: id.0,
                attr: attr.clone(),
                value: value.clone(),
            }));
        }
        Ok(Planned {
            records,
            reply: Reply::Created(id),
        })
    }

    /// Hint law: an unknown name lists the alternatives; CONTAINMENT names
    /// the expected parent type.
    fn containment(
        child_type: &Key,
        declared: Option<&Key>,
        actual: &Key,
    ) -> Result<(), KernelError> {
        Validators::r2_containment(child_type, declared, actual).map_err(|v| {
            let hint = match declared {
                Some(d) => format!("type `{child_type}` requires parent type `{d}`"),
                None => format!("type `{child_type}` is a root type — create it without a parent"),
            };
            KernelError::new(codes::CONTAINMENT, hint, v.reason)
        })
    }

    fn plan_delete_node(&self, node: NodeId, cascade: bool) -> Result<Planned, KernelError> {
        self.type_key_of(node)?;
        let subtree = self.subtree_of(node);
        if !cascade {
            let (children, edges) = self.dependents_of(node);
            if children > 0 || edges > 0 {
                return Err(KernelError::new(
                    codes::HAS_DEPENDENTS,
                    format!(
                        "{children} children, {edges} dependency edges — use cascade or drain first"
                    ),
                    format!("node {node} has dependents"),
                ));
            }
        }
        // node_ref integrity (SPEC-01 §3): a reference from OUTSIDE the
        // subtree keeps the delete refused even with cascade.
        let targets: BTreeSet<NodeId> = subtree.iter().copied().collect();
        if let Some(holder) = self.machine().node_ref_holder(&targets) {
            return Err(KernelError::new(
                codes::HAS_DEPENDENTS,
                format!(
                    "node {holder} holds a node_ref into the subtree — clear the reference first"
                ),
                format!("node {node} is referenced"),
            ));
        }
        let mut records = Vec::new();
        if cascade {
            let mut edges = BTreeSet::new();
            for n in &subtree {
                for eid in self
                    .arrays()
                    .dep_out_ids(*n)
                    .iter()
                    .chain(self.arrays().dep_in_ids(*n))
                {
                    edges.insert(*eid);
                }
            }
            records.extend(edges.into_iter().map(|eid| {
                Record::new(RecordData::DelEdge {
                    edge: u64::from(eid.0),
                })
            }));
        }
        // Children before parents (reversed pre-order).
        records.extend(
            subtree
                .iter()
                .rev()
                .map(|n| Record::new(RecordData::DelVertex { node: n.0 })),
        );
        Ok(Planned {
            records,
            reply: Reply::Ok,
        })
    }

    fn plan_link(
        &self,
        src: NodeId,
        dst: NodeId,
        kind: u8,
        aux: u32,
        guard: Option<&(Key, Key)>,
    ) -> Result<Planned, KernelError> {
        let src_type = self.type_key_of(src)?;
        let dst_type = self.type_key_of(dst)?;
        if src == dst {
            return Err(KernelError::new(
                codes::INVALID_KIND_PARAMS,
                "",
                format!("self-loop {src}→{src} is not a dependency edge"),
            ));
        }
        if !(1..=8).contains(&kind) {
            return Err(KernelError::new(
                codes::INVALID_KIND_PARAMS,
                "known kinds: AND=1, OR=2, KOFN=3, GUARD=4, INHIBIT=5, XOR=6, MIRROR=7, WEIGHTED=8",
                format!("edge kind {kind} is not in the registry"),
            ));
        }
        // R3: the dependency DAG is acyclic per Database (global, cross-graph).
        let dep_out = |n: NodeId| -> Vec<NodeId> {
            self.arrays()
                .dep_out_ids(n)
                .iter()
                .filter_map(|e| self.arrays().edge(*e))
                .filter(|r| r.alive)
                .map(|r| r.dst)
                .collect()
        };
        if Validators::r3_acyclicity(self.arrays(), &dep_out, src, dst).is_err() {
            // SGT-15.3: the hint walks the offending path dst → … → src.
            let mut path = self.path(dst, src).unwrap_or_else(|| vec![dst]);
            if path.last() != Some(&src) {
                path.push(src);
            }
            let path: Vec<String> = path.iter().map(|n| n.to_string()).collect();
            return Err(KernelError::new(
                codes::CYCLE,
                format!("offending path: {}", path.join(" → ")),
                format!("edge {src}→{dst} closes a dependency cycle"),
            ));
        }
        let (aux, guard) = match (kind, guard) {
            (kinds::GUARD, Some((sigma, tau))) => {
                let catalog = &self.schema.catalog;
                let src_model = &self.schema.catalog.node_types[&src_type].state_model;
                let dst_model = &self.schema.catalog.node_types[&dst_type].state_model;
                let sigma_states: BTreeMap<Key, ()> = catalog.state_models[src_model]
                    .states
                    .iter()
                    .map(|s| (s.key.clone(), ()))
                    .collect();
                // Derived motion only fires engine-armed (empty-actor) transitions.
                let tau_transitions: BTreeMap<Key, ()> = catalog
                    .transitions
                    .values()
                    .filter(|t| &t.model == dst_model && t.actors.is_empty())
                    .map(|t| (t.key.clone(), ()))
                    .collect();
                let rules: BTreeMap<Key, ()> =
                    catalog.rules.keys().map(|k| (k.clone(), ())).collect();
                validate_guard_params(sigma, tau, &None, &sigma_states, &tau_transitions, &rules)
                    .map_err(|e| {
                        KernelError::new(
                            codes::SCHEMA,
                            format!(
                                "sigma must be a state of `{src_model}` ({}); tau must be an engine-armed transition of `{dst_model}` ({})",
                                sigma_states.keys().map(|k| k.as_str().to_string()).collect::<Vec<_>>().join(", "),
                                tau_transitions.keys().map(|k| k.as_str().to_string()).collect::<Vec<_>>().join(", ")
                            ),
                            e.to_string(),
                        )
                    })?;
                (
                    self.machine().peek_next_guard_aux(),
                    Some((sigma.clone(), tau.clone())),
                )
            }
            (kinds::GUARD, None) => {
                return Err(KernelError::new(
                    codes::INVALID_KIND_PARAMS,
                    "GUARD edges carry (sigma, tau)",
                    "GUARD edge without params",
                ));
            }
            (_, Some(_)) => {
                return Err(KernelError::new(
                    codes::INVALID_KIND_PARAMS,
                    "only GUARD edges carry (sigma, tau)",
                    format!("guard params given for edge kind {kind}"),
                ));
            }
            (_, None) => (aux, None),
        };
        let edge = self.arrays().edge_slots() as u64;
        Ok(Planned {
            records: vec![Record::new(RecordData::AddEdge {
                edge,
                src: src.0,
                dst: dst.0,
                kind,
                aux,
                guard,
            })],
            reply: Reply::Edge(edge),
        })
    }

    fn plan_unlink(&self, edge: u64) -> Result<Planned, KernelError> {
        let eid = EdgeId(u32::try_from(edge).map_err(|_| {
            KernelError::new(codes::NOT_FOUND, "", format!("edge {edge} is not live"))
        })?);
        match self.arrays().edge(eid) {
            Some(rec) if rec.alive => {}
            _ => {
                return Err(KernelError::new(
                    codes::NOT_FOUND,
                    "",
                    format!("edge {edge} is not live"),
                ));
            }
        }
        Ok(Planned {
            records: vec![Record::new(RecordData::DelEdge { edge })],
            reply: Reply::Ok,
        })
    }

    fn plan_row_append(
        &self,
        node: NodeId,
        set: &Key,
        rows: &[crate::records::Row],
    ) -> Result<Planned, KernelError> {
        let catalog = &self.schema.catalog;
        let Some(attr_set) = catalog.attr_sets.get(set) else {
            return Err(KernelError::new(
                codes::NOT_FOUND,
                KernelError::name_hint(
                    "attribute set",
                    catalog.attr_sets.keys().map(|k| k.as_str().to_string()),
                ),
                format!("attribute set `{set}` is not declared"),
            ));
        };
        if attr_set.kind != SetKind::TableSet {
            return Err(KernelError::new(
                codes::SCHEMA,
                "ROW_APPEND targets table_sets only (SPEC-01 §7)",
                format!("`{set}` is a scalar_set"),
            ));
        }
        if rows.is_empty() {
            return Err(KernelError::new(
                codes::INVALID_KEY,
                "",
                "ROW_APPEND without rows",
            ));
        }
        let (type_key, state_key) = self.node_ctx(node)?;
        let ctx = WriteCtx {
            node: Some(node),
            type_key: &type_key,
            state_key: &state_key,
        };
        for row in rows {
            let mut seen = BTreeSet::new();
            for (attr, value) in &row.cells {
                if !attr_set.members.contains(attr) {
                    return Err(KernelError::new(
                        codes::SCHEMA,
                        format!(
                            "members of `{set}`: {}",
                            attr_set
                                .members
                                .iter()
                                .map(|k| k.as_str().to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        format!("attribute `{attr}` is not a member of set `{set}`"),
                    ));
                }
                if !seen.insert(attr) {
                    return Err(KernelError::new(
                        codes::INVALID_KEY,
                        "",
                        format!("attribute `{attr}` written twice in one row"),
                    ));
                }
                self.check_cell(&ctx, attr, CellK::Row, value)?;
            }
        }
        let ks = self.row_ks(node, &attr_set.members, rows);
        Ok(Planned {
            records: vec![Record::new(RecordData::RowAppend {
                node: node.0,
                set: set.clone(),
                rows: rows.to_vec(),
            })],
            reply: Reply::Rows {
                node,
                first_k: ks[0],
                count: u32::try_from(rows.len()).map_err(|_| {
                    KernelError::new(codes::LIMIT_EXCEEDED, "", "too many rows in one batch")
                })?,
            },
        })
    }

    fn plan_action(
        &self,
        node: NodeId,
        transition: &Key,
        actor: ActorClass,
        payload: Option<&Value>,
        subject: &Subject,
    ) -> Result<Planned, KernelError> {
        if !matches!(actor, ActorClass::Human | ActorClass::Agent) {
            return Err(KernelError::new(
                codes::INVALID_KEY,
                "actor_hint must be human or agent",
                "system/timer/guard actors are engine-internal",
            ));
        }
        self.type_key_of(node)?;
        let check = self
            .machine()
            .check_action(node, transition, actor, payload)
            .map_err(|e| self.dispatch_err(e))?;
        let graph = self.arrays().graph_of(node).0;
        let first = self.machine().peek_next_node_id().0;
        let spawned: Vec<NodeId> = (0..check.spawns.len())
            .map(|i| NodeId(first + i as u32))
            .collect();
        let mut records = Vec::with_capacity(1 + check.spawns.len());
        records.push(Record::new(RecordData::Action {
            node: node.0,
            transition: transition.clone(),
            actor,
            subject: subject.id.clone(),
            payload: payload.cloned(),
            from: check.from.clone(),
            to: check.to.clone(),
        }));
        for (i, (instance_of, _)) in check.spawns.iter().enumerate() {
            records.push(Record::new(RecordData::AddVertex {
                node: first + i as u32,
                parent: Some(node.0),
                graph,
                node_type: instance_of.clone(),
                authored: Author::Engine,
            }));
        }
        Ok(Planned {
            records,
            reply: Reply::Action {
                node,
                from: check.from.clone(),
                to: check.to.clone(),
                spawned,
                cancelled: check
                    .cancels
                    .iter()
                    .map(|(s, reason, _)| (*s, reason.clone()))
                    .collect(),
            },
        })
    }

    /// Engine dispatch error → wire code (SPEC-02 §7 registry).
    fn dispatch_err(&self, e: DispatchError) -> KernelError {
        use DispatchError::*;
        match &e {
            UnknownTransition { key } => KernelError::new(
                codes::NOT_FOUND,
                KernelError::name_hint(
                    "transition",
                    self.schema
                        .catalog
                        .transitions
                        .keys()
                        .map(|k| k.as_str().to_string()),
                ),
                format!("transition `{key}` is not declared"),
            ),
            UnknownNodeType { key } => KernelError::new(
                codes::NOT_FOUND,
                "",
                format!("node type `{key}` is not declared"),
            ),
            UnknownRule { key } => KernelError::new(
                codes::NOT_FOUND,
                "",
                format!("rule `{key}` is not declared"),
            ),
            StateMismatch {
                expected_from,
                actual,
            } => KernelError::new(
                codes::ORDER_CONFLICT,
                "",
                format!("transition fires from `{expected_from}`, node is in `{actual}`"),
            ),
            NotInvokable { transition, actor } => KernelError::new(
                codes::FORBIDDEN,
                "",
                format!("transition `{transition}` is not invokable by actor {actor:?}"),
            ),
            Terminal { state } => KernelError::new(
                codes::TERMINAL,
                "terminal states are absorbing — create a re-run instance",
                format!("node is in terminal state `{state}`"),
            ),
            RequirementsUnmet { failing } => KernelError::new(
                codes::REQUIREMENTS_UNMET,
                format!(
                    "failing rules: {}",
                    failing
                        .iter()
                        .map(|k| k.as_str().to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "action requirements unmet",
            ),
            NotABranch { node, selected } => KernelError::new(
                codes::INVALID_KIND_PARAMS,
                "",
                format!("node {selected} is not an XOR branch of decision node {node}"),
            ),
            MissingBranchSelection => KernelError::new(
                codes::INVALID_KEY,
                "select_branch takes the selected branch as a NodeRef payload",
                "select_branch without a branch selection",
            ),
            LagNotElapsed { transition, lag_ms } => KernelError::new(
                codes::ORDER_CONFLICT,
                "",
                format!("transition `{transition}` lag {lag_ms} ms not elapsed since state entry"),
            ),
            Engine(e) => KernelError::new(codes::INTERNAL, "", e.to_string()),
        }
    }

    // -- shared validation -----------------------------------------------------

    /// Type + state keys of a live node.
    fn node_ctx(&self, node: NodeId) -> Result<(Key, Key), KernelError> {
        let type_key = self.type_key_of(node)?;
        let state_key = self
            .machine()
            .state_key_of(node)
            .map_err(|e| KernelError::new(codes::INTERNAL, "", e.to_string()))?;
        Ok((type_key, state_key))
    }

    fn type_key_of(&self, node: NodeId) -> Result<Key, KernelError> {
        let arrays = self.arrays();
        if !arrays.is_alive(node) {
            return Err(KernelError::new(
                codes::NOT_FOUND,
                "node must exist",
                format!("node {node} not found"),
            ));
        }
        let code = arrays.type_of(node);
        self.machine().node_type_key(code).cloned().ok_or_else(|| {
            KernelError::new(
                codes::INTERNAL,
                "",
                format!("node {node} carries unknown type code {code}"),
            )
        })
    }

    fn initial_state_key(&self, type_key: &Key) -> Result<Key, KernelError> {
        let catalog = &self.schema.catalog;
        let model_key = &catalog.node_types[type_key].state_model;
        Ok(catalog.state_models[model_key].initial.clone())
    }

    /// R6 / R7 / R12 and the state locks for one cell write.
    fn check_cell(
        &self,
        ctx: &WriteCtx<'_>,
        attr: &Key,
        k: CellK<'_>,
        value: &Value,
    ) -> Result<(), KernelError> {
        let catalog = &self.schema.catalog;
        let compiled = self.machine().compiled_ref();
        let Some(def) = catalog.attributes.get(attr) else {
            return Err(KernelError::new(
                codes::SCHEMA,
                KernelError::name_hint(
                    "attribute",
                    catalog.attributes.keys().map(|k| k.as_str().to_string()),
                ),
                format!("attribute `{attr}` is not declared in the catalog (R6)"),
            ));
        };
        let Some(attr_id) = compiled.attr_ids.get(attr) else {
            return Err(KernelError::new(
                codes::INTERNAL,
                "",
                format!("attribute `{attr}` has no compiled id"),
            ));
        };
        let bit = usize::from(attr_id.0);
        let declared_for_type = compiled
            .effective_attrs
            .get(ctx.type_key)
            .is_some_and(|mask| mask.test(bit));
        if !declared_for_type {
            return Err(KernelError::new(
                codes::SCHEMA,
                "",
                format!("attribute `{attr}` is not part of type `{}`", ctx.type_key),
            ));
        }
        // Attribute locking is a consequence of state (SPEC-01 §7/§11): a
        // lock for (type, state) narrows the writable set; no lock = all.
        if let Some(mask) = compiled
            .write_masks
            .get(&(ctx.type_key.clone(), ctx.state_key.clone()))
            && !mask.test(bit)
        {
            return Err(KernelError::new(
                codes::FORBIDDEN,
                format!(
                    "state lock: `{attr}` is not writable while the node is in state `{}`",
                    ctx.state_key
                ),
                format!("write to `{attr}` refused by the state lock"),
            ));
        }
        let vector = matches!(def.attr_type, AttrType::Vector { .. });
        match k {
            CellK::Scalar if vector => {
                return Err(KernelError::new(
                    codes::NOT_APPENDABLE,
                    "ATTR_SET targets scalar/enum/record attributes; vector columns take CELLS_APPEND or ROW_APPEND",
                    format!("`{attr}` is a vector attribute"),
                ));
            }
            CellK::Append { .. } | CellK::Row => {
                let AttrType::Vector { elem } = &def.attr_type else {
                    return Err(KernelError::new(
                        codes::SCHEMA,
                        "scalar cells are written with ATTR_SET at k = 0",
                        format!("`{attr}` is not a vector attribute"),
                    ));
                };
                if let Err(v) = Validators::r6_elem_type(elem, value, &catalog.enums) {
                    return Err(KernelError::new(codes::SCHEMA, "", v.reason));
                }
            }
            CellK::Scalar => {
                if let Err(v) =
                    Validators::r6_attr_type(def, value, &catalog.enums, &catalog.records)
                {
                    return Err(KernelError::new(codes::SCHEMA, "", v.reason));
                }
            }
        }
        if let Value::NodeRef(target) = value
            && !self.arrays().is_alive(*target)
        {
            return Err(KernelError::new(
                codes::SCHEMA,
                "",
                format!("node_ref target {target} does not exist (or is deleted)"),
            ));
        }
        if let CellK::Append {
            k,
            after: Some(max),
        } = k
            && k <= *max
        {
            return Err(KernelError::new(
                codes::NOT_APPENDABLE,
                format!(
                    "current max k is {max} — append above it, or use a re-run instance for corrections"
                ),
                format!("k {k} is not above current max {max}"),
            ));
        }
        let current = ctx
            .node
            .and_then(|n| self.machine().latest_cell(n, attr))
            .map(|cell| &cell.value);
        if let Err(v) = Validators::r7_monotone_write(def.monotonicity, current, value) {
            return Err(KernelError::new(codes::NOT_MONOTONE, "", v.reason));
        }
        Ok(())
    }

    /// The `k` each row of a row-append lands on: the node's key axis
    /// advances once per row; `k_skip` leaves a hole (SPEC-01 §6).
    fn row_ks(&self, node: NodeId, members: &[Key], rows: &[crate::records::Row]) -> Vec<u64> {
        let machine = self.machine();
        let mut k = members
            .iter()
            .filter_map(|m| machine.max_k(node, m))
            .max()
            .map_or(0, |max| max + 1);
        rows.iter()
            .map(|row| {
                k += u64::from(row.k_skip);
                let at = k;
                k += 1;
                at
            })
            .collect()
    }

    /// The node and every live descendant, pre-order (children ascending).
    fn subtree_of(&self, root: NodeId) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        while let Some(n) = stack.pop() {
            out.push(n);
            for child in self.arrays().children_of(n).iter().rev() {
                stack.push(*child);
            }
        }
        out
    }

    fn dependents_of(&self, node: NodeId) -> (usize, usize) {
        let arrays = self.arrays();
        (
            arrays.children_of(node).len(),
            arrays.dep_out_ids(node).len() + arrays.dep_in_ids(node).len(),
        )
    }

    /// First path `from → … → to` over live dependency edges (cycle hints).
    fn path(&self, from: NodeId, to: NodeId) -> Option<Vec<NodeId>> {
        let mut stack = vec![vec![from]];
        let mut seen = BTreeSet::new();
        while let Some(path) = stack.pop() {
            let last = *path.last()?;
            if last == to {
                return Some(path);
            }
            if !seen.insert(last) {
                continue;
            }
            for eid in self.arrays().dep_out_ids(last).iter().rev() {
                if let Some(rec) = self.arrays().edge(*eid)
                    && rec.alive
                {
                    let mut next = path.clone();
                    next.push(rec.dst);
                    stack.push(next);
                }
            }
        }
        None
    }

    // -- applying ---------------------------------------------------------------

    /// Before a unit applies: remember the enabled actions of every live node
    /// it touches (the post-quiesce diff yields `action_enabled/disabled`).
    pub fn begin_unit(&mut self, records: &[Record]) -> Result<(), TxError> {
        self.enabled_before.clear();
        for node in self.unit_nodes(records) {
            if !self.arrays().is_alive(node) {
                continue;
            }
            let enabled = self
                .prop
                .machine
                .enabled_actions(node)
                .map_err(|e| invariant(format!("enabled-action index failed: {e}")))?;
            self.enabled_before
                .insert(node, enabled.into_iter().collect());
        }
        Ok(())
    }

    /// After quiesce: emit `Enabled` events for every transition whose
    /// enabled-ness flipped on a touched node.
    pub fn end_unit(&mut self) -> Result<(), TxError> {
        let mut nodes: BTreeSet<NodeId> = self.enabled_before.keys().copied().collect();
        nodes.extend(
            self.pending
                .iter()
                .filter(|e| e.kind == EventKind::State)
                .map(|e| e.node),
        );
        for node in nodes {
            if !self.arrays().is_alive(node) {
                continue;
            }
            let now: BTreeSet<Key> = self
                .prop
                .machine
                .enabled_actions(node)
                .map_err(|e| invariant(format!("enabled-action index failed: {e}")))?
                .into_iter()
                .collect();
            let empty = BTreeSet::new();
            let before = self.enabled_before.get(&node).unwrap_or(&empty);
            for transition in now.difference(before) {
                self.pending.push(Event {
                    kind: EventKind::Enabled,
                    node,
                    transition: Some(transition.clone()),
                    enabled: Some(true),
                });
            }
            for transition in before.difference(&now) {
                self.pending.push(Event {
                    kind: EventKind::Enabled,
                    node,
                    transition: Some(transition.clone()),
                    enabled: Some(false),
                });
            }
        }
        self.enabled_before.clear();
        Ok(())
    }

    /// Run the propagation worklist to quiescence (derived state) and collect
    /// the derivations as events. Deterministic: replay quiesces identically.
    pub fn quiesce(&mut self) -> Result<usize, TxError> {
        let steps = self
            .prop
            .quiesce()
            .map_err(|e| invariant(format!("propagation failed: {e}")))?;
        for d in self.prop.derivations.clone() {
            if let Derivation::GuardFired {
                target, transition, ..
            } = d
            {
                self.pending.push(Event {
                    kind: EventKind::Action,
                    node: target,
                    transition: Some(transition),
                    enabled: None,
                });
                self.pending.push(Event::simple(EventKind::State, target));
            } else if let Derivation::Mirrored { target, .. } = d {
                self.pending.push(Event::simple(EventKind::State, target));
            }
        }
        Ok(steps)
    }

    /// The single mutation primitive. Live execution and journal replay both
    /// go through it; `lsn` is the record's journal LSN.
    pub fn apply(&mut self, rec: &Record, lsn: u64) -> Result<(), TxError> {
        match &rec.data {
            RecordData::AddVertex {
                node,
                parent,
                graph,
                node_type,
                authored,
            } => {
                if *authored == Author::Engine {
                    let expected = self.engine_spawns.pop_front().ok_or_else(|| {
                        invariant("engine-authored ADD_VERTEX without a pending ACTION spawn")
                    })?;
                    let got = (*parent, node_type.clone());
                    if expected != got {
                        return Err(invariant(format!(
                            "engine-authored ADD_VERTEX {parent:?}/{node_type} does not match the ACTION's spawn {expected:?}"
                        )));
                    }
                    self.mark_state(NodeId(*node));
                    return Ok(());
                }
                let Some(code) = self.machine().node_type_code(node_type) else {
                    return Err(invariant(format!(
                        "ADD_VERTEX `{node_type}`: no node-type code in this epoch"
                    )));
                };
                let made = match parent {
                    None => self.prop.machine.arrays.add_root(GraphId(*graph), code),
                    Some(p) => self.prop.machine.arrays.add_child(NodeId(*p), code),
                }
                .map_err(|e| invariant(format!("vertex creation failed after sequencing: {e}")))?;
                if made.0 != *node {
                    return Err(invariant(format!(
                        "vertex id drift: journal says {node}, allocation gave {}",
                        made.0
                    )));
                }
                self.prop
                    .machine
                    .initialize_node(made)
                    .map_err(|e| invariant(format!("vertex initialization failed: {e}")))?;
                self.mark_state(made);
                self.pending.push(Event::simple(EventKind::Struct, made));
            }
            RecordData::DelVertex { node } => {
                let node = NodeId(*node);
                self.prop
                    .machine
                    .arrays
                    .delete_node(node)
                    .map_err(|e| invariant(format!("DEL_VERTEX {} failed: {e}", node.0)))?;
                // Cells die with their node — in the store and in staging.
                self.prop.machine.drop_cells(node);
                self.staging.retain(|(n, _), _| *n != node.0);
                self.pending.push(Event::simple(EventKind::Struct, node));
            }
            RecordData::AddEdge {
                edge,
                src,
                dst,
                kind,
                aux,
                guard,
            } => {
                let (src, dst) = (NodeId(*src), NodeId(*dst));
                let made = match guard {
                    Some((sigma, tau)) => {
                        let made_aux = self
                            .prop
                            .machine
                            .add_guard_edge(src, dst, sigma, tau)
                            .map_err(|e| {
                                invariant(format!("ADD_EDGE failed after sequencing: {e}"))
                            })?;
                        if made_aux != *aux {
                            return Err(invariant(format!(
                                "guard aux drift: journal says {aux}, allocation gave {made_aux}"
                            )));
                        }
                        EdgeId(self.prop.machine.arrays.edge_slots() as u32 - 1)
                    }
                    None => self
                        .prop
                        .machine
                        .arrays
                        .add_edge(src, dst, *kind, *aux)
                        .map_err(|e| invariant(format!("ADD_EDGE failed after sequencing: {e}")))?,
                };
                if u64::from(made.0) != *edge {
                    return Err(invariant(format!(
                        "edge id drift: journal says {edge}, allocation gave {}",
                        made.0
                    )));
                }
                self.pending.push(Event::simple(EventKind::Struct, src));
            }
            RecordData::DelEdge { edge } => {
                let index = u32::try_from(*edge)
                    .map_err(|_| invariant(format!("DEL_EDGE {edge}: id out of range")))?;
                let eid = EdgeId(index);
                let endpoints = self
                    .arrays()
                    .edge(eid)
                    .map(|r| (r.src, r.dst))
                    .ok_or_else(|| invariant(format!("DEL_EDGE {edge}: no such edge")))?;
                self.prop
                    .machine
                    .remove_edge(eid)
                    .map_err(|e| invariant(format!("DEL_EDGE {edge} failed: {e}")))?;
                self.pending
                    .push(Event::simple(EventKind::Struct, endpoints.0));
            }
            RecordData::SetAttr { node, attr, value } => {
                self.write_cell(*node, attr, 0, value.clone(), lsn)?;
            }
            RecordData::AppendCells { node, attr, cells } => {
                for (k, value) in cells {
                    self.write_cell(*node, attr, *k, value.clone(), lsn)?;
                }
            }
            RecordData::RowAppend { node, set, rows } => {
                let members: Vec<Key> = self
                    .schema
                    .catalog
                    .attr_sets
                    .get(set)
                    .map(|s| s.members.clone())
                    .ok_or_else(|| invariant(format!("ROW_APPEND set `{set}` is not declared")))?;
                let ks = self.row_ks(NodeId(*node), &members, rows);
                for (row, k) in rows.iter().zip(ks) {
                    for (attr, value) in &row.cells {
                        self.write_cell(*node, attr, k, value.clone(), lsn)?;
                    }
                }
            }
            RecordData::Action {
                node,
                transition,
                actor,
                subject,
                payload,
                from,
                to,
            } => {
                let node = NodeId(*node);
                let outcome = self
                    .prop
                    .machine
                    .apply_action(node, transition, *actor, subject.clone(), payload.clone())
                    .map_err(|e| {
                        invariant(format!(
                            "journaled ACTION `{transition}` failed to apply: {e}"
                        ))
                    })?;
                if outcome.from != *from || outcome.to != *to {
                    return Err(invariant(format!(
                        "journaled ACTION `{transition}` moved {}→{} but the record says {from}→{to}",
                        outcome.from, outcome.to
                    )));
                }
                self.engine_spawns = outcome
                    .engine_facts
                    .iter()
                    .filter_map(|f| match f {
                        EngineFact::AddVertex {
                            parent, node_type, ..
                        } => Some((Some(parent.0), node_type.clone())),
                        _ => None,
                    })
                    .collect();
                self.mark_state(node);
                self.pending.push(Event {
                    kind: EventKind::Action,
                    node,
                    transition: Some(transition.clone()),
                    enabled: None,
                });
                for (sibling, _) in &outcome.cancelled {
                    self.mark_state(*sibling);
                }
            }
            RecordData::SchemaTxn { ops } => {
                self.apply_schema_ops(ops.clone())?;
            }
            RecordData::Grant { grant } => {
                self.apply_schema_ops(vec![SchemaOp::GrantOp(grant.clone())])?;
            }
            RecordData::Revoke {
                role,
                subject,
                target,
            } => {
                self.apply_schema_ops(vec![SchemaOp::Revoke {
                    role: role.clone(),
                    subject: subject.clone(),
                    target: target.clone(),
                }])?;
            }
        }
        Ok(())
    }

    fn mark_state(&mut self, node: NodeId) {
        self.prop.mark_state(node);
        self.pending.push(Event::simple(EventKind::State, node));
    }

    /// A live cell write: the cell, the node's epoch, its attribute count,
    /// and the propagation dirty mark.
    fn write_cell(
        &mut self,
        node: u32,
        attr: &Key,
        k: u64,
        value: Value,
        lsn: u64,
    ) -> Result<(), TxError> {
        let node = NodeId(node);
        let first = self.prop.machine.max_k(node, attr).is_none();
        self.prop
            .machine
            .set_cell(node, attr, KeyIndex(k), value.clone(), Lsn(lsn))
            .map_err(|e| invariant(format!("cell write rejected after sequencing: {e}")))?;
        // Stage the cell for column reads until the next checkpoint.
        let attr_id = self
            .prop
            .machine
            .compiled_attr_id(attr)
            .ok_or_else(|| invariant(format!("cell write `{attr}`: no compiled id")))?;
        self.staging
            .entry((node.0, attr_id.0))
            .or_default()
            .insert(k, (lsn, value));
        if first {
            let count = self
                .prop
                .machine
                .arrays
                .attr_count_of(node)
                .saturating_add(1);
            self.prop
                .machine
                .arrays
                .set_attr_count(node, count)
                .map_err(|e| invariant(format!("attr count update failed: {e}")))?;
        } else {
            self.prop
                .machine
                .arrays
                .touch_node(node)
                .map_err(|e| invariant(format!("epoch bump failed: {e}")))?;
        }
        self.prop
            .mark_cell(node, attr)
            .map_err(|e| invariant(format!("dirty mark failed: {e}")))?;
        self.pending.push(Event::simple(EventKind::Attr, node));
        Ok(())
    }

    /// Restore a cell from a part at open: the cell only — the node's epoch
    /// and attribute count already sit in the snapshot.
    pub fn load_cell(
        &mut self,
        node: NodeId,
        attr: lattice_core::ids::AttrId,
        k: u64,
        value: Value,
        lsn: u64,
    ) -> Result<(), TxError> {
        let key = self
            .machine()
            .compiled_ref()
            .attr_ids
            .iter()
            .find(|(_, id)| **id == attr)
            .map(|(k, _)| k.clone())
            .ok_or_else(|| {
                invariant(format!(
                    "part cell names attr id {}, not in this epoch",
                    attr.0
                ))
            })?;
        self.prop
            .machine
            .set_cell(node, &key, KeyIndex(k), value, Lsn(lsn))
            .map_err(|e| invariant(format!("part cell load failed: {e}")))
    }

    fn apply_schema_ops(&mut self, ops: Vec<SchemaOp>) -> Result<u64, TxError> {
        let epoch = self
            .schema
            .commit(ops)
            .map_err(|e| invariant(format!("committed schema txn rejected on apply: {e}")))?;
        self.recompile()?;
        Ok(epoch)
    }

    fn recompile(&mut self) -> Result<(), TxError> {
        let compiled =
            compile_with_lineage(&self.schema.catalog, Some(&self.schema.attr_id_lineage()))?;
        let type_codes = self.schema.node_type_codes();
        self.prop
            .recompile(compiled, &self.schema.catalog, &type_codes);
        Ok(())
    }

    /// Recovery: restore a GUARD edge's params (they are outside the
    /// snapshot format — BACKLOG ED-15).
    pub fn restore_guard_param(&mut self, aux: u32, sigma: Key, tau: Key) {
        self.prop.machine.restore_guard_param(aux, sigma, tau);
    }

    /// Live nodes a unit's records name (for the enabled-action diff).
    fn unit_nodes(&self, records: &[Record]) -> BTreeSet<NodeId> {
        let mut nodes = BTreeSet::new();
        for rec in records {
            match &rec.data {
                RecordData::AddVertex { node, .. } | RecordData::DelVertex { node } => {
                    nodes.insert(NodeId(*node));
                }
                RecordData::AddEdge { src, dst, .. } => {
                    nodes.insert(NodeId(*src));
                    nodes.insert(NodeId(*dst));
                }
                RecordData::DelEdge { edge } => {
                    let eid = u32::try_from(*edge).map(EdgeId).ok();
                    if let Some(rec) = eid.and_then(|eid| self.arrays().edge(eid)) {
                        nodes.insert(rec.src);
                        nodes.insert(rec.dst);
                    }
                }
                RecordData::SetAttr { node, .. }
                | RecordData::AppendCells { node, .. }
                | RecordData::RowAppend { node, .. }
                | RecordData::Action { node, .. } => {
                    nodes.insert(NodeId(*node));
                }
                RecordData::SchemaTxn { .. }
                | RecordData::Grant { .. }
                | RecordData::Revoke { .. } => {}
            }
        }
        nodes
    }
}

/// A rejected schema op → wire code (`R13_IN_USE` for guarded-evolution
/// rejections, `SCHEMA` otherwise), with the R13 path as the hint.
fn schema_rejection(bad: &lattice_schema::txn::OpResult) -> KernelError {
    let r13 = bad.r13.as_ref().map(|r| r.to_string());
    let error = bad.error.as_ref().map_or_else(
        || "rejected without an error message".to_string(),
        |e| e.to_string(),
    );
    match r13 {
        Some(path) => KernelError::new(
            codes::R13_IN_USE,
            path,
            format!("op[{}]: {error}", bad.op_index),
        ),
        None => KernelError::new(
            codes::SCHEMA,
            String::new(),
            format!("op[{}]: {error}", bad.op_index),
        ),
    }
}

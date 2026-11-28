//! The kernel read path (ARCHITECTURE §5.2; SPEC-02 §8): snapshot, node
//! read, cursors, blockers, history — typed, for every surface to build on
//! (the wire layer maps TLV onto these; the CLI calls them directly).
//!
//! - A read sees exactly ONE committed epoch, never a torn mix (WPT-8.1):
//!   reads run between committed units on the sequencer thread. The epoch
//!   marker is the COMMIT LSN of the last unit the read saw.
//! - Cursors read COLUMNAR DATA from the parts plus the staging tail (cells
//!   written since the last checkpoint). A run that a filter excludes by the
//!   run directory or by the zone maps is never decoded (zone-map pushdown).
//!   The engine keeps its own in-memory cell store for predicates; parity
//!   between the two is a test invariant, not an assumption.
//! - `at_epoch` is a time-travel read (WPT-8.2): the view is rebuilt from
//!   the retained snapshot + parts + journal prefix up to that LSN in a
//!   throwaway state. The live state is untouched.
//! - Selectors pick a NODE SET by scan in v1; index-backed selection and the
//!   drop/rebuild equivalence land with T-315.
//!
//! Codes: the closed registry (SPEC-02 §7) has no cursor codes. An expired
//! or unknown cursor is `NOT_FOUND` with a telling hint, a malformed cursor
//! request is `INVALID_KEY` — never a code outside the registry (SQ-12).

use crate::TxError;
use crate::error::{KernelError, codes};
use crate::state::{EngineState, Staging};
use lattice_core::ids::{AttrId, EdgeId, NodeId};
use lattice_core::value::Value;
use lattice_engine::blockers::Blocker;
use lattice_schema::entities::{AttrType, ElemType, ScalarType};
use lattice_schema::key::Key;
use lattice_storage::part::Part;
use std::collections::{BTreeMap, BTreeSet};

/// Which nodes a cursor covers.
#[derive(Debug, Clone, PartialEq)]
pub enum Selector {
    All,
    Subtree(NodeId),
    Graph(Key),
    TypeScan {
        node_type: Key,
    },
    ByState {
        node_type: Key,
        state: Key,
    },
    AttrPresence {
        attr: Key,
    },
    /// Nodes whose scalar `attr` equals `value`.
    SharedKey {
        attr: Key,
        value: Value,
    },
}

/// Cursor filters. `k` narrows the key axis; `value` is an inclusive range
/// on the column's own values. Both are pushed down to the run directory and
/// the zone maps before anything is decoded.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CursorFilter {
    pub k: Option<(u64, u64)>,
    pub value: Option<(Value, Value)>,
}

/// A read request.
#[derive(Debug, Clone, PartialEq)]
pub enum Read {
    /// `SNAPSHOT {graph?, subtree?, at_epoch?}` — nodes, states, edges and
    /// scalar cells as of one epoch.
    Snapshot {
        graph: Option<Key>,
        subtree: Option<NodeId>,
        at_epoch: Option<u64>,
    },
    /// `NODE_READ {node}` — scalars, record cells and a head/tail preview of
    /// every vector.
    NodeRead {
        node: NodeId,
    },
    CursorOpen {
        attr: Key,
        selector: Selector,
        filter: CursorFilter,
    },
    CursorFetch {
        cursor: u64,
        budget: Budget,
    },
    CursorClose {
        cursor: u64,
    },
    /// `BLOCKERS {node}` — why none of the node's transitions can fire.
    Blockers {
        node: NodeId,
    },
    /// `HISTORY {node, range}` — the node's journal-derived event list.
    History {
        node: NodeId,
        range: Option<(u64, u64)>,
    },
}

/// A fetch budget (SPEC-02 §8: "`budget` bytes/rows").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    pub rows: usize,
    pub bytes: usize,
}

/// A read's result.
#[derive(Debug, Clone, PartialEq)]
pub enum ReadReply {
    Snapshot(GraphView),
    Node(NodeView),
    Cursor(u64),
    Batch(Batch),
    Closed,
    Blockers(Vec<BlockerEntry>),
    History(Vec<HistoryEvent>),
}

/// One node of a snapshot: the structural row plus type and state by key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub node: u32,
    pub graph: u16,
    pub type_code: u16,
    pub state: u8,
    pub epoch: u64,
    pub parent: Option<u32>,
    pub type_key: Option<Key>,
    pub state_key: Option<Key>,
}

/// One live dependency edge of a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRow {
    pub edge_id: u64,
    pub src: u32,
    pub dst: u32,
    pub kind: u8,
    pub aux: u32,
}

/// One cell: the attribute value at key `k`, with the LSN that wrote it.
#[derive(Debug, Clone, PartialEq)]
pub struct CellRow {
    pub node: u32,
    pub attr: AttrId,
    pub k: u64,
    pub lsn: u64,
    pub value: Value,
}

/// The graph as of one epoch. Edges are listed when their SOURCE is in scope.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphView {
    /// COMMIT LSN of the last unit this view saw.
    pub epoch: u64,
    pub nodes: Vec<NodeRow>,
    pub edges: Vec<EdgeRow>,
    /// Every in-scope cell at k = 0, by `(node, attr)`.
    pub scalars: Vec<CellRow>,
}

/// What one cursor column holds — identical for every batch of a cursor
/// (WPT-8.3: "Arrow schema stable across batches").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnSchema {
    pub attr: Key,
    pub value_type: String,
    pub vector: bool,
}

impl ColumnSchema {
    /// Stable identity of the schema across batches.
    pub fn fingerprint(&self) -> String {
        format!(
            "{}|{}|{}",
            self.attr,
            if self.vector { "vector" } else { "scalar" },
            self.value_type
        )
    }
}

/// One page of a cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub schema: ColumnSchema,
    /// `(node, k)`-ascending.
    pub rows: Vec<CellRow>,
    /// More rows are waiting behind this batch.
    pub remaining: bool,
}

/// A node's detail (NODE_READ).
#[derive(Debug, Clone, PartialEq)]
pub struct NodeView {
    pub node: NodeRow,
    /// `(attr, value)` at k = 0, attribute-ascending.
    pub scalars: Vec<(Key, Value)>,
    pub vectors: Vec<VectorPreview>,
}

/// Head / tail preview of one vector attribute.
#[derive(Debug, Clone, PartialEq)]
pub struct VectorPreview {
    pub attr: Key,
    pub count: u64,
    pub first_k: Option<u64>,
    pub last_k: Option<u64>,
    pub head: Vec<(u64, Value)>,
    pub tail: Vec<(u64, Value)>,
}

/// One concrete blocker (WPT-8.4: precise and named).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockerEntry {
    pub kind: &'static str,
    pub transition: Option<Key>,
    pub detail: String,
}

impl From<&Blocker> for BlockerEntry {
    fn from(b: &Blocker) -> Self {
        let (kind, transition) = match b {
            Blocker::UnmetRule { transition, .. } => ("unmet_rule", Some(transition)),
            Blocker::DependencyDeficit { transition, .. } => {
                ("dependency_deficit", Some(transition))
            }
            Blocker::LagPending { transition, .. } => ("lag_pending", Some(transition)),
            Blocker::PendingGuard { transition, .. } => ("pending_guard", Some(transition)),
            Blocker::Inhibited { transition, .. } => ("inhibited", Some(transition)),
        };
        Self {
            kind,
            transition: transition.cloned(),
            detail: b.to_string(),
        }
    }
}

/// One journal-derived event of a node (HISTORY). `author` is set for
/// actions — the only record class that carries one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEvent {
    pub lsn: u64,
    pub kind: &'static str,
    pub detail: String,
    pub author: Option<String>,
}

/// Nodes a selector picks; `None` = every live node.
pub fn select_nodes(
    state: &EngineState,
    selector: &Selector,
) -> Result<Option<BTreeSet<u32>>, KernelError> {
    let arrays = state.arrays();
    let machine = state.machine();
    let all = |pred: &dyn Fn(u32) -> bool| -> BTreeSet<u32> {
        (0..arrays.len() as u32)
            .filter(|n| arrays.is_alive(NodeId(*n)) && pred(*n))
            .collect()
    };
    Ok(match selector {
        Selector::All => None,
        Selector::Subtree(root) => {
            if !arrays.is_alive(*root) {
                return Err(KernelError::new(
                    codes::NOT_FOUND,
                    "node must exist",
                    format!("subtree root {root} not found"),
                ));
            }
            let mut set = BTreeSet::new();
            let mut stack = vec![*root];
            while let Some(n) = stack.pop() {
                set.insert(n.0);
                stack.extend(arrays.children_of(n));
            }
            Some(set)
        }
        Selector::Graph(g) => {
            let id = graph_id(state, g)?;
            Some(all(&|n| arrays.graph_of(NodeId(n)) == id))
        }
        Selector::TypeScan { node_type } | Selector::ByState { node_type, .. } => {
            let code = type_code(state, node_type)?;
            if let Selector::ByState { state: want, .. } = selector {
                let code_state = state_code(state, node_type, want)?;
                Some(all(&|n| {
                    arrays.type_of(NodeId(n)) == code && arrays.state_of(NodeId(n)) == code_state
                }))
            } else {
                Some(all(&|n| arrays.type_of(NodeId(n)) == code))
            }
        }
        Selector::AttrPresence { attr } | Selector::SharedKey { attr, .. } => {
            let id = attr_id(state, attr)?;
            let equal = match selector {
                Selector::SharedKey { value, .. } => Some(value.clone()),
                _ => None,
            };
            let mut set = BTreeSet::new();
            for ((node, a, k), cell) in machine.cells() {
                let matches =
                    a == id && equal.as_ref().is_none_or(|v| k.0 == 0 && &cell.value == v);
                if matches && arrays.is_alive(node) {
                    set.insert(node.0);
                }
            }
            Some(set)
        }
    })
}

fn graph_id(state: &EngineState, key: &Key) -> Result<lattice_core::ids::GraphId, KernelError> {
    state
        .schema
        .graph_ids()
        .get(key)
        .copied()
        .map(lattice_core::ids::GraphId)
        .ok_or_else(|| {
            KernelError::new(
                codes::NOT_FOUND,
                KernelError::name_hint(
                    "graph",
                    state
                        .schema
                        .graph_ids()
                        .keys()
                        .map(|k| k.as_str().to_string()),
                ),
                format!("graph `{key}` is not declared"),
            )
        })
}

fn type_code(state: &EngineState, key: &Key) -> Result<u16, KernelError> {
    state.machine().node_type_code(key).ok_or_else(|| {
        KernelError::new(
            codes::NOT_FOUND,
            KernelError::name_hint(
                "type",
                state
                    .catalog()
                    .node_types
                    .keys()
                    .map(|k| k.as_str().to_string()),
            ),
            format!("node type `{key}` is not declared"),
        )
    })
}

fn state_code(state: &EngineState, node_type: &Key, want: &Key) -> Result<u8, KernelError> {
    let catalog = state.catalog();
    let model = &catalog.node_types[node_type].state_model;
    let known = catalog.state_models[model]
        .states
        .iter()
        .map(|s| s.key.as_str().to_string())
        .collect::<Vec<_>>();
    catalog.state_models[model]
        .states
        .iter()
        .position(|s| &s.key == want)
        .and_then(|i| u8::try_from(i).ok())
        .ok_or_else(|| {
            KernelError::new(
                codes::NOT_FOUND,
                KernelError::name_hint("state", known),
                format!("state `{want}` is not declared by model `{model}`"),
            )
        })
}

fn attr_id(state: &EngineState, key: &Key) -> Result<AttrId, KernelError> {
    state.machine().compiled_attr_id(key).ok_or_else(|| {
        KernelError::new(
            codes::SCHEMA,
            KernelError::name_hint(
                "attribute",
                state
                    .catalog()
                    .attributes
                    .keys()
                    .map(|k| k.as_str().to_string()),
            ),
            format!("attribute `{key}` is not declared in the catalog (R6)"),
        )
    })
}

fn node_row(state: &EngineState, node: NodeId) -> NodeRow {
    let arrays = state.arrays();
    let machine = state.machine();
    NodeRow {
        node: node.0,
        graph: arrays.graph_of(node).0,
        type_code: arrays.type_of(node),
        state: arrays.state_of(node),
        epoch: arrays.epoch_of(node).0,
        parent: arrays.parent_of(node).map(|p| p.0),
        type_key: machine.node_type_key(arrays.type_of(node)).cloned(),
        state_key: machine.state_key_of(node).ok(),
    }
}

/// The graph over one state as of `epoch`. `graph` and `subtree` narrow the
/// scope; with neither, every live node is in scope.
pub fn graph_view(
    state: &EngineState,
    epoch: u64,
    graph: Option<&Key>,
    subtree: Option<NodeId>,
) -> Result<GraphView, KernelError> {
    let arrays = state.arrays();
    let scope = match (graph, subtree) {
        (None, None) => None,
        (Some(g), _) => select_nodes(state, &Selector::Graph(g.clone()))?,
        (_, Some(root)) => select_nodes(state, &Selector::Subtree(root))?,
    };
    let in_scope = |n: u32| scope.as_ref().is_none_or(|s| s.contains(&n));
    let nodes: Vec<NodeRow> = (0..arrays.len() as u32)
        .filter(|n| arrays.is_alive(NodeId(*n)) && in_scope(*n))
        .map(|n| node_row(state, NodeId(n)))
        .collect();
    let edges: Vec<EdgeRow> = (0..arrays.edge_slots())
        .filter_map(|i| arrays.edge(EdgeId(i as u32)).map(|r| (i as u64, r)))
        .filter(|(_, r)| in_scope(r.src.0))
        .map(|(slot, r)| EdgeRow {
            // Edge ids are table slots — `aux` is a parameter field.
            edge_id: slot,
            src: r.src.0,
            dst: r.dst.0,
            kind: r.kind,
            aux: r.aux,
        })
        .collect();
    let mut scalars: Vec<CellRow> = state
        .machine()
        .cells()
        .filter(|((n, _, k), _)| k.0 == 0 && arrays.is_alive(*n) && in_scope(n.0))
        .map(|((n, a, k), cell)| CellRow {
            node: n.0,
            attr: a,
            k: k.0,
            lsn: cell.lsn.0,
            value: cell.value.clone(),
        })
        .collect();
    scalars.sort_by_key(|c| (c.node, c.attr.0));
    Ok(GraphView {
        epoch,
        nodes,
        edges,
        scalars,
    })
}

/// A node's scalars and vector previews, from the state's cells.
pub fn node_view(
    state: &EngineState,
    node: NodeId,
    preview: usize,
) -> Result<NodeView, KernelError> {
    if !state.arrays().is_alive(node) {
        return Err(KernelError::new(
            codes::NOT_FOUND,
            "node must exist",
            format!("node {node} not found"),
        ));
    }
    // attr id → key: bijective with the catalog, so a miss is an invariant.
    let key_of: BTreeMap<AttrId, Key> = state
        .machine()
        .compiled_ref()
        .attr_ids
        .iter()
        .map(|(k, id)| (*id, k.clone()))
        .collect();
    let mut by_key: BTreeMap<Key, Vec<(u64, Value)>> = BTreeMap::new();
    for ((n, attr, k), cell) in state.machine().cells() {
        if n != node {
            continue;
        }
        let key = key_of.get(&attr).ok_or_else(|| {
            KernelError::new(
                codes::INTERNAL,
                "",
                format!(
                    "cell names attr id {}, which this epoch does not compile",
                    attr.0
                ),
            )
        })?;
        by_key
            .entry(key.clone())
            .or_default()
            .push((k.0, cell.value.clone()));
    }
    let mut scalars = Vec::new();
    let mut vectors = Vec::new();
    for (key, mut cells) in by_key {
        cells.sort_by_key(|(k, _)| *k);
        let vector = state
            .catalog()
            .attributes
            .get(&key)
            .is_some_and(|d| matches!(d.attr_type, AttrType::Vector { .. }));
        if vector {
            let split = cells.len().saturating_sub(preview);
            let tail = cells[split..].to_vec();
            let head = cells.iter().take(preview).cloned().collect();
            vectors.push(VectorPreview {
                attr: key,
                count: cells.len() as u64,
                first_k: cells.first().map(|(k, _)| *k),
                last_k: cells.last().map(|(k, _)| *k),
                head,
                tail,
            });
        } else {
            let (k, value) = cells.remove(cells.len() - 1);
            if k != 0 {
                return Err(KernelError::new(
                    codes::INTERNAL,
                    "",
                    format!("scalar attribute `{key}` holds a cell at k {k}"),
                ));
            }
            scalars.push((key, value));
        }
    }
    Ok(NodeView {
        node: node_row(state, node),
        scalars,
        vectors,
    })
}

/// Why none of the node's transitions can fire right now.
pub fn blockers_of(state: &EngineState, node: NodeId) -> Result<Vec<BlockerEntry>, KernelError> {
    if !state.arrays().is_alive(node) {
        return Err(KernelError::new(
            codes::NOT_FOUND,
            "node must exist",
            format!("node {node} not found"),
        ));
    }
    Ok(state
        .machine()
        .blockers(node)
        .map_err(|e| KernelError::new(codes::INTERNAL, "", e.to_string()))?
        .blockers
        .iter()
        .map(BlockerEntry::from)
        .collect())
}

/// Journal events of one node, from decoded records in journal order.
/// `edge_endpoints` resolves an edge id for `DEL_EDGE` records (the record
/// carries only the id; a dead edge keeps its table slot).
pub fn history_events(
    node: NodeId,
    decoded: &[(u64, crate::Record)],
    edge_endpoints: &dyn Fn(u64) -> Option<(u32, u32)>,
) -> Vec<HistoryEvent> {
    use crate::records::{Author, RecordData as D};
    let node = node.0;
    let mut out = Vec::new();
    for (lsn, rec) in decoded {
        let event = |kind, detail: String, author: Option<String>| HistoryEvent {
            lsn: *lsn,
            kind,
            detail,
            author,
        };
        match &rec.data {
            D::AddVertex {
                node: n,
                parent,
                authored,
                ..
            } if *n == node => out.push(event(
                "node_added",
                match (parent, authored) {
                    (Some(p), Author::Engine) => format!("spawned under node {p} by the engine"),
                    (Some(p), Author::User) => format!("created under node {p}"),
                    (None, _) => "created as a root".to_string(),
                },
                None,
            )),
            D::DelVertex { node: n } if *n == node => {
                out.push(event("node_removed", "node deleted".to_string(), None))
            }
            D::AddEdge {
                edge,
                src,
                dst,
                kind,
                ..
            } if *src == node || *dst == node => out.push(event(
                "edge_added",
                format!(
                    "edge {edge} (kind {kind}) {}",
                    if *src == node {
                        format!("to node {dst}")
                    } else {
                        format!("from node {src}")
                    }
                ),
                None,
            )),
            D::DelEdge { edge } => {
                if let Some((src, dst)) = edge_endpoints(*edge)
                    && (src == node || dst == node)
                {
                    out.push(event(
                        "edge_removed",
                        format!("edge {edge} (kind {}) removed", "see edge_added"),
                        None,
                    ));
                }
            }
            D::SetAttr {
                node: n,
                attr,
                value,
            } if *n == node => out.push(event("attr_set", format!("`{attr}` = {value:?}"), None)),
            D::AppendCells {
                node: n,
                attr,
                cells,
            } if *n == node => out.push(event(
                "cells_appended",
                format!(
                    "`{attr}`: {} cells, k {}..{}",
                    cells.len(),
                    cells.first().map_or(0, |c| c.0),
                    cells.last().map_or(0, |c| c.0)
                ),
                None,
            )),
            D::RowAppend { node: n, set, rows } if *n == node => {
                let cells: usize = rows.iter().map(|r| r.cells.len()).sum();
                out.push(event(
                    "rows_appended",
                    format!("table `{set}`: {} rows, {cells} cells", rows.len()),
                    None,
                ))
            }
            D::Action {
                node: n,
                transition,
                actor,
                subject,
                from,
                to,
                payload,
            } if *n == node => out.push(event(
                "action",
                format!(
                    "`{transition}` {from} → {to} ({actor:?}){}",
                    payload
                        .as_ref()
                        .map_or_else(String::new, |p| format!(" with payload {p:?}"))
                ),
                Some(subject.clone()),
            )),
            _ => {}
        }
    }
    out
}

/// A human-readable name for an attribute's value type (schema identity).
pub fn type_name(ty: &AttrType) -> String {
    let scalar = |t: &ScalarType| match t {
        ScalarType::Bool => "bool".to_string(),
        ScalarType::I64 => "i64".to_string(),
        ScalarType::F64 => "f64".to_string(),
        ScalarType::String => "string".to_string(),
        ScalarType::Bytes => "bytes".to_string(),
        ScalarType::Timestamp => "timestamp".to_string(),
        ScalarType::Duration => "duration".to_string(),
        ScalarType::NodeRef => "node_ref".to_string(),
        ScalarType::Decimal { precision, scale } => format!("decimal({precision},{scale})"),
    };
    let elem = |e: &ElemType| match e {
        ElemType::Scalar(t) => scalar(t),
        ElemType::Enum(k) => format!("enum:{k}"),
    };
    match ty {
        AttrType::Scalar(t) => scalar(t),
        AttrType::Enum(k) => format!("enum:{k}"),
        AttrType::Record(k) => format!("record:{k}"),
        AttrType::Vector { elem: e } => format!("vector<{}>", elem(e)),
    }
}

/// The schema of one attribute's column.
pub fn column_schema(
    state: &EngineState,
    attr: &Key,
) -> Result<(AttrId, ColumnSchema), KernelError> {
    let id = attr_id(state, attr)?;
    let def = state.catalog().attributes.get(attr).ok_or_else(|| {
        KernelError::new(
            codes::SCHEMA,
            "",
            format!("attribute `{attr}` is not declared"),
        )
    })?;
    Ok((
        id,
        ColumnSchema {
            attr: attr.clone(),
            vector: matches!(def.attr_type, AttrType::Vector { .. }),
            value_type: type_name(&def.attr_type),
        },
    ))
}

/// A conservative in-memory size of a value — the `bytes` half of a budget.
pub fn value_bytes(v: &Value) -> usize {
    let extra = match v {
        Value::Str(s) => s.len(),
        Value::Bytes(b) => b.len(),
        Value::Record(r) => r.field_count() * 16,
        _ => 0,
    };
    std::mem::size_of::<Value>() + extra
}

/// One node's merged cells of a column: `(k, lsn, value)`, k-ascending.
pub type RunCells = Vec<(u64, u64, Value)>;

/// A node's staged cells of one column: `(node id, k → (lsn, value))`.
type StagedRun<'a> = (u32, &'a BTreeMap<u64, (u64, Value)>);

/// A column as node-ascending runs: the part's runs merged with the staging
/// tail (staged cells win — they are newer). Built per fetch; cheap.
pub struct ColumnReader<'a> {
    part: Option<&'a Part>,
    run_idx: usize,
    staged: Vec<StagedRun<'a>>,
    staged_pos: usize,
    /// Resume point: nothing before this node is read — nor decoded.
    after: Option<u32>,
    /// The column's declared element type: part cells of I64 storage are
    /// re-typed to it. `None` for record columns (they decode whole).
    elem: Option<&'a ElemType>,
}

impl<'a> ColumnReader<'a> {
    /// `after_node`: start at the first node ≥ it (a resumed cursor).
    pub fn new(
        part: Option<&'a Part>,
        staging: &'a Staging,
        attr: AttrId,
        after_node: Option<u32>,
        elem: Option<&'a ElemType>,
    ) -> Self {
        let staged = staging
            .iter()
            .filter(|((n, a), _)| *a == attr.0 && after_node.is_none_or(|after| *n >= after))
            .map(|((n, _), cells)| (*n, cells))
            .collect();
        Self {
            part,
            run_idx: 0,
            staged,
            staged_pos: 0,
            after: after_node,
            elem,
        }
    }

    /// The next node and its merged cells. `skip_part_run` is the pushdown
    /// hook: it sees the part and the run index and returns whether the run
    /// may be left UNdecoded (the node's staged cells are still merged in).
    pub fn next_run(
        &mut self,
        mut skip_part_run: impl FnMut(&Part, usize) -> bool,
    ) -> Result<Option<(u32, RunCells)>, TxError> {
        loop {
            let part_node = self
                .part
                .filter(|p| self.run_idx < p.run_count())
                .map(|p| p.run_node(self.run_idx).0);
            let staged_node = self.staged.get(self.staged_pos).map(|(n, _)| *n);
            let node = match (part_node, staged_node) {
                (None, None) => return Ok(None),
                (Some(p), Some(s)) => p.min(s),
                (Some(p), None) | (None, Some(p)) => p,
            };
            // Before the resume point: step over the node, decoding nothing.
            if self.after.is_some_and(|after| node < after) {
                if part_node == Some(node) {
                    self.run_idx += 1;
                }
                if staged_node == Some(node) {
                    self.staged_pos += 1;
                }
                continue;
            }
            let mut cells: RunCells = Vec::new();
            if part_node == Some(node) {
                let part = self
                    .part
                    .ok_or_else(|| crate::error::invariant("part run without a part"))?;
                if !skip_part_run(part, self.run_idx) {
                    for cell in part.run_cells(self.run_idx)? {
                        let Some(value) = cell.value else {
                            continue;
                        };
                        let value = match self.elem {
                            Some(elem) => crate::kernel::retype(elem, value)?,
                            None => value,
                        };
                        cells.push((cell.k.0, cell.lsn.0, value));
                    }
                }
                self.run_idx += 1;
            }
            if staged_node == Some(node) {
                for (k, (lsn, value)) in self.staged[self.staged_pos].1 {
                    match cells.iter_mut().find(|(ck, _, _)| ck == k) {
                        Some(slot) => *slot = (*k, *lsn, value.clone()),
                        None => cells.push((*k, *lsn, value.clone())),
                    }
                }
                self.staged_pos += 1;
            }
            cells.sort_by_key(|(k, _, _)| *k);
            return Ok(Some((node, cells)));
        }
    }
}

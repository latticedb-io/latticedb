//! Structural SoA/CSR arrays (ARCHITECTURE §4): the composition forest and
//! the dependency DAG over one node-id space.
//!
//! Design invariants (all asserted by [`GraphArrays::assert_invariants`],
//! exercised in tests):
//! - `node_id` == array index; ids are allocated monotonically and never
//!   reused; deletion is a tombstone (`alive = false`) — logical, replayable.
//! - parents are allocated before children (`parent < id`) and composition
//!   edges stay inside their graph (ARCHITECTURE §4).
//! - dependency edges MAY cross graphs; self-loops are structurally banned.
//! - every mutation of a node (add child, delete, edge attach/detach, state
//!   change) bumps that node's epoch by exactly 1.
//! - CSR target lists are strictly ascending (binary-searchable); edge CSR
//!   entries reference live edges only.

use crate::ids::{EdgeId, Epoch, GraphId, IdSpaceError, NO_PARENT, NodeId, NodeIdSpace};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArrayError {
    IdSpace(IdSpaceError),
    NodeNotFound(NodeId),
    NodeDead(NodeId),
    ParentNotFound(NodeId),
    ParentDead(NodeId),
    /// Composition edges must stay inside their graph (ARCH §4).
    CrossGraphComposition {
        parent: GraphId,
        child: GraphId,
    },
    /// Structural HAS_DEPENDENTS guard: children or dependency edges exist.
    HasDependents {
        children: usize,
        dep_edges: usize,
    },
    /// Snapshot import rejected: raw vectors are structurally inconsistent.
    MalformedRaw {
        reason: String,
    },
    EdgeNotFound(EdgeId),
    /// Dependency self-loop (cycle detection beyond this is R4, engine).
    SelfLoop {
        node: NodeId,
    },
    EdgeAlreadyDead(EdgeId),
}

impl std::fmt::Display for ArrayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArrayError::IdSpace(e) => write!(f, "{e}"),
            ArrayError::NodeNotFound(n) => write!(f, "node {n} not found"),
            ArrayError::NodeDead(n) => write!(f, "node {n} is deleted"),
            ArrayError::ParentNotFound(n) => write!(f, "parent {n} not found"),
            ArrayError::ParentDead(n) => write!(f, "parent {n} is deleted"),
            ArrayError::CrossGraphComposition { parent, child } => {
                write!(
                    f,
                    "composition edge crosses graphs: parent graph {parent}, child graph {child}"
                )
            }
            ArrayError::HasDependents {
                children,
                dep_edges,
            } => {
                write!(
                    f,
                    "node has dependents: {children} children, {dep_edges} dependency edges"
                )
            }
            ArrayError::MalformedRaw { reason } => {
                write!(f, "malformed raw arrays: {reason}")
            }
            ArrayError::EdgeNotFound(e) => write!(f, "edge {e} not found"),
            ArrayError::SelfLoop { node } => write!(f, "self-loop on node {node}"),
            ArrayError::EdgeAlreadyDead(e) => write!(f, "edge {e} already removed"),
        }
    }
}

impl std::error::Error for ArrayError {}

impl From<IdSpaceError> for ArrayError {
    fn from(e: IdSpaceError) -> Self {
        ArrayError::IdSpace(e)
    }
}

/// One dependency edge record (ARCH §4 edge table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeRec {
    pub src: NodeId,
    pub dst: NodeId,
    /// EdgeKind registry byte (SPEC-01 §10; validated at schema level).
    pub kind: u8,
    pub aux: u32,
    pub alive: bool,
}

/// Raw SoA snapshot of the arrays (SPEC-03 §3 structural_arrays block).
/// Field widths and ordering are fixed by the spec.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RawArrays {
    pub alive: Vec<u8>,
    pub parent: Vec<u32>,
    pub graph: Vec<u16>,
    pub node_type: Vec<u16>,
    pub state: Vec<u8>,
    pub epoch: Vec<u64>,
    pub attr_count: Vec<u16>,
    pub child_off: Vec<u64>,
    pub child_tgt: Vec<u32>,
    pub dep_out_off: Vec<u64>,
    pub dep_out_tgt: Vec<u32>,
    pub dep_in_off: Vec<u64>,
    pub dep_in_tgt: Vec<u32>,
    /// `(src, dst, kind, aux, alive)` per edge, edge_id == index.
    pub edges: Vec<(u32, u32, u8, u32, u8)>,
}

/// Structural SoA/CSR arrays. `V` = id-space watermark; all per-node vectors
/// are indexed by `node_id`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphArrays {
    space: NodeIdSpace,
    alive: Vec<bool>,
    parent: Vec<Option<NodeId>>,
    graph: Vec<GraphId>,
    node_type: Vec<u16>,
    state: Vec<u8>,
    epoch: Vec<Epoch>,
    attr_count: Vec<u16>,
    // Composition CSR: children sorted strictly ascending.
    child_off: Vec<u64>,
    child_tgt: Vec<NodeId>,
    // Dependency CSR: edge ids sorted ascending (both directions).
    dep_out_off: Vec<u64>,
    dep_out_tgt: Vec<EdgeId>,
    dep_in_off: Vec<u64>,
    dep_in_tgt: Vec<EdgeId>,
    // Edge table; edge_id == index.
    edges: Vec<EdgeRec>,
}

impl GraphArrays {
    pub fn new(space: NodeIdSpace) -> Self {
        let v = space.watermark() as usize;
        Self {
            space,
            alive: vec![false; v],
            parent: vec![None; v],
            graph: vec![GraphId(0); v],
            node_type: vec![0; v],
            state: vec![0; v],
            epoch: vec![Epoch(0); v],
            attr_count: vec![0; v],
            child_off: vec![0; v + 1],
            child_tgt: Vec::new(),
            dep_out_off: vec![0; v + 1],
            dep_out_tgt: Vec::new(),
            dep_in_off: vec![0; v + 1],
            dep_in_tgt: Vec::new(),
            edges: Vec::new(),
        }
    }

    pub fn id_space(&self) -> &NodeIdSpace {
        &self.space
    }

    pub fn len(&self) -> usize {
        self.space.watermark() as usize
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn touch(&mut self, id: NodeId) {
        self.epoch[id.0 as usize].0 += 1;
    }

    /// Grow the SoA vectors for a freshly allocated id: per-node slots plus
    /// a new CSR sentinel (each offset run's end == current target length).
    fn grow_for(&mut self) {
        self.alive.push(false);
        self.parent.push(None);
        self.graph.push(GraphId(0));
        self.node_type.push(0);
        self.state.push(0);
        self.epoch.push(Epoch(0));
        self.attr_count.push(0);
        self.child_off.push(self.child_tgt.len() as u64);
        self.dep_out_off.push(self.dep_out_tgt.len() as u64);
        self.dep_in_off.push(self.dep_in_tgt.len() as u64);
    }

    fn alive_node(&self, id: NodeId) -> Result<usize, ArrayError> {
        let idx = id.0 as usize;
        if idx >= self.alive.len() {
            return Err(ArrayError::NodeNotFound(id));
        }
        if !self.alive[idx] {
            return Err(ArrayError::NodeDead(id));
        }
        Ok(idx)
    }

    /// Insert `item` into the ascending run `tgt[lo..hi]`.
    fn csr_insert<T: Ord + Copy>(tgt: &mut Vec<T>, lo: usize, hi: usize, item: T) {
        let pos = lo + tgt[lo..hi].partition_point(|x| *x < item);
        tgt.insert(pos, item);
    }

    /// Remove `item` from the ascending run `tgt[lo..hi]` (must exist).
    fn csr_remove<T: Ord + Copy + std::fmt::Debug>(
        tgt: &mut Vec<T>,
        lo: usize,
        hi: usize,
        item: T,
    ) {
        let pos = lo + tgt[lo..hi].partition_point(|x| *x < item);
        debug_assert_eq!(tgt[pos], item, "csr item must exist");
        tgt.remove(pos);
    }

    // -- structural mutations ------------------------------------------------

    /// Add a root of the composition forest (its own graph subset).
    pub fn add_root(&mut self, graph: GraphId, node_type: u16) -> Result<NodeId, ArrayError> {
        let id = self.space.allocate()?;
        self.grow_for();
        let idx = id.0 as usize;
        self.alive[idx] = true;
        self.parent[idx] = None;
        self.graph[idx] = graph;
        self.node_type[idx] = node_type;
        self.epoch[idx] = Epoch(1); // creation is the first mutation
        Ok(id)
    }

    /// Add a child under `parent` (composition edge, same graph, epoch bumps).
    pub fn add_child(&mut self, parent: NodeId, node_type: u16) -> Result<NodeId, ArrayError> {
        let p = self.alive_node(parent).map_err(|e| match e {
            ArrayError::NodeNotFound(_) => ArrayError::ParentNotFound(parent),
            ArrayError::NodeDead(_) => ArrayError::ParentDead(parent),
            other => other,
        })?;
        let id = self.space.allocate()?;
        self.grow_for();
        let idx = id.0 as usize;
        self.alive[idx] = true;
        self.parent[idx] = Some(parent);
        self.graph[idx] = self.graph[p];
        self.node_type[idx] = node_type;
        self.epoch[idx] = Epoch(1);

        // Composition CSR insert for the parent.
        self.touch(parent);
        let lo = self.child_off[parent.0 as usize] as usize;
        let hi = self.child_off[parent.0 as usize + 1] as usize;
        Self::csr_insert(&mut self.child_tgt, lo, hi, id);
        for off in self.child_off[parent.0 as usize + 1..].iter_mut() {
            *off += 1;
        }
        Ok(id)
    }

    /// Structurally delete a node: rejected while alive children or live
    /// dependency edges reference it (the HAS_DEPENDENTS guard, SPEC-01 §3).
    pub fn delete_node(&mut self, id: NodeId) -> Result<(), ArrayError> {
        let idx = self.alive_node(id)?;
        let children = self.child_off[idx + 1] - self.child_off[idx];
        let dep_out = self.dep_out_off[idx + 1] - self.dep_out_off[idx];
        let dep_in = self.dep_in_off[idx + 1] - self.dep_in_off[idx];
        if children != 0 || dep_out != 0 || dep_in != 0 {
            return Err(ArrayError::HasDependents {
                children: children as usize,
                dep_edges: (dep_out + dep_in) as usize,
            });
        }
        if let Some(parent) = self.parent[idx] {
            let lo = self.child_off[parent.0 as usize] as usize;
            let hi = self.child_off[parent.0 as usize + 1] as usize;
            Self::csr_remove(&mut self.child_tgt, lo, hi, id);
            for off in self.child_off[parent.0 as usize + 1..].iter_mut() {
                *off -= 1;
            }
            self.touch(parent);
        }
        self.alive[idx] = false;
        self.parent[idx] = None;
        self.touch(id);
        Ok(())
    }

    /// Add a dependency edge `src --kind--> dst`. May cross graphs (ARCH §4);
    /// both endpoints must be alive; self-loops rejected structurally.
    pub fn add_edge(
        &mut self,
        src: NodeId,
        dst: NodeId,
        kind: u8,
        aux: u32,
    ) -> Result<EdgeId, ArrayError> {
        self.alive_node(src)?;
        self.alive_node(dst)?;
        if src == dst {
            return Err(ArrayError::SelfLoop { node: src });
        }
        let eid = EdgeId(self.edges.len() as u32);
        self.edges.push(EdgeRec {
            src,
            dst,
            kind,
            aux,
            alive: true,
        });

        let s = src.0 as usize;
        let d = dst.0 as usize;
        Self::csr_insert(
            &mut self.dep_out_tgt,
            self.dep_out_off[s] as usize,
            self.dep_out_off[s + 1] as usize,
            eid,
        );
        for off in self.dep_out_off[s + 1..].iter_mut() {
            *off += 1;
        }
        Self::csr_insert(
            &mut self.dep_in_tgt,
            self.dep_in_off[d] as usize,
            self.dep_in_off[d + 1] as usize,
            eid,
        );
        for off in self.dep_in_off[d + 1..].iter_mut() {
            *off += 1;
        }
        self.touch(src);
        self.touch(dst);
        Ok(eid)
    }

    /// Remove a live dependency edge; both endpoint epochs bump.
    pub fn remove_edge(&mut self, eid: EdgeId) -> Result<(), ArrayError> {
        let idx = eid.0 as usize;
        let rec = self
            .edges
            .get(idx)
            .copied()
            .ok_or(ArrayError::EdgeNotFound(eid))?;
        if !rec.alive {
            return Err(ArrayError::EdgeAlreadyDead(eid));
        }
        let s = rec.src.0 as usize;
        let d = rec.dst.0 as usize;
        Self::csr_remove(
            &mut self.dep_out_tgt,
            self.dep_out_off[s] as usize,
            self.dep_out_off[s + 1] as usize,
            eid,
        );
        for off in self.dep_out_off[s + 1..].iter_mut() {
            *off -= 1;
        }
        Self::csr_remove(
            &mut self.dep_in_tgt,
            self.dep_in_off[d] as usize,
            self.dep_in_off[d + 1] as usize,
            eid,
        );
        for off in self.dep_in_off[d + 1..].iter_mut() {
            *off -= 1;
        }
        self.edges[idx].alive = false;
        self.touch(rec.src);
        self.touch(rec.dst);
        Ok(())
    }

    /// Raw state write (engine guards transitions — R5/T-201); epoch bumps.
    pub fn set_state(&mut self, id: NodeId, state: u8) -> Result<(), ArrayError> {
        self.alive_node(id)?;
        self.state[id.0 as usize] = state;
        self.touch(id);
        Ok(())
    }

    /// Attribute-set cardinality update (engine/tx on cell writes); epoch bumps.
    pub fn set_attr_count(&mut self, id: NodeId, count: u16) -> Result<(), ArrayError> {
        self.alive_node(id)?;
        self.attr_count[id.0 as usize] = count;
        self.touch(id);
        Ok(())
    }

    /// Bump a node's epoch for a mutation the arrays do not carry
    /// themselves (cell writes live in the engine's cell store; the epoch is
    /// the per-node change counter, ARCHITECTURE §4).
    pub fn touch_node(&mut self, id: NodeId) -> Result<(), ArrayError> {
        self.alive_node(id)?;
        self.touch(id);
        Ok(())
    }

    // -- reads ---------------------------------------------------------------

    pub fn is_alive(&self, id: NodeId) -> bool {
        id.0 < self.alive.len() as u32 && self.alive[id.0 as usize]
    }

    pub fn parent_of(&self, id: NodeId) -> Option<NodeId> {
        self.parent[id.0 as usize]
    }

    pub fn graph_of(&self, id: NodeId) -> GraphId {
        self.graph[id.0 as usize]
    }

    pub fn type_of(&self, id: NodeId) -> u16 {
        self.node_type[id.0 as usize]
    }

    pub fn state_of(&self, id: NodeId) -> u8 {
        self.state[id.0 as usize]
    }

    pub fn epoch_of(&self, id: NodeId) -> Epoch {
        self.epoch[id.0 as usize]
    }

    pub fn attr_count_of(&self, id: NodeId) -> u16 {
        self.attr_count[id.0 as usize]
    }

    /// Children of `id`, strictly ascending (composition CSR run).
    pub fn children_of(&self, id: NodeId) -> &[NodeId] {
        let lo = self.child_off[id.0 as usize] as usize;
        let hi = self.child_off[id.0 as usize + 1] as usize;
        &self.child_tgt[lo..hi]
    }

    pub fn dep_out_ids(&self, id: NodeId) -> &[EdgeId] {
        let lo = self.dep_out_off[id.0 as usize] as usize;
        let hi = self.dep_out_off[id.0 as usize + 1] as usize;
        &self.dep_out_tgt[lo..hi]
    }

    pub fn dep_in_ids(&self, id: NodeId) -> &[EdgeId] {
        let lo = self.dep_in_off[id.0 as usize] as usize;
        let hi = self.dep_in_off[id.0 as usize + 1] as usize;
        &self.dep_in_tgt[lo..hi]
    }

    pub fn edge(&self, eid: EdgeId) -> Option<&EdgeRec> {
        self.edges.get(eid.0 as usize).filter(|e| e.alive)
    }

    /// The edge-table slot, live OR dead (`edge` serves live edges only).
    pub fn edge_slot(&self, eid: EdgeId) -> Option<&EdgeRec> {
        self.edges.get(eid.0 as usize)
    }

    pub fn edge_count(&self) -> usize {
        self.edges.iter().filter(|e| e.alive).count()
    }

    /// Edge-table slots — live AND dead. Edge ids are slot indexes and dead
    /// slots are never reused, so this is also the id the next edge takes.
    pub fn edge_slots(&self) -> usize {
        self.edges.len()
    }

    pub fn alive_count(&self) -> usize {
        self.alive.iter().filter(|a| **a).count()
    }

    /// Export to the snapshot encoding (SPEC-03 §3 structural_arrays).
    /// Roots and dead nodes carry `NO_PARENT`; dead nodes carry `alive = 0`.
    pub fn to_raw(&self) -> RawArrays {
        RawArrays {
            alive: self.alive.iter().map(|a| u8::from(*a)).collect(),
            parent: self
                .parent
                .iter()
                .map(|p| p.map_or(NO_PARENT, |n| n.0))
                .collect(),
            graph: self.graph.iter().map(|g| g.0).collect(),
            node_type: self.node_type.clone(),
            state: self.state.clone(),
            epoch: self.epoch.iter().map(|e| e.0).collect(),
            attr_count: self.attr_count.clone(),
            child_off: self.child_off.clone(),
            child_tgt: self.child_tgt.iter().map(|n| n.0).collect(),
            dep_out_off: self.dep_out_off.clone(),
            dep_out_tgt: self.dep_out_tgt.iter().map(|e| e.0).collect(),
            dep_in_off: self.dep_in_off.clone(),
            dep_in_tgt: self.dep_in_tgt.iter().map(|e| e.0).collect(),
            edges: self
                .edges
                .iter()
                .map(|e| (e.src.0, e.dst.0, e.kind, e.aux, u8::from(e.alive)))
                .collect(),
        }
    }

    /// Rebuild from a snapshot export; the id-space watermark derives from
    /// the per-node vector length. Callers verify invariants after import.
    pub fn from_raw(raw: &RawArrays) -> Result<Self, ArrayError> {
        let v = raw.alive.len();
        if raw.parent.len() != v
            || raw.graph.len() != v
            || raw.node_type.len() != v
            || raw.state.len() != v
            || raw.epoch.len() != v
            || raw.attr_count.len() != v
            || raw.child_off.len() != v + 1
            || raw.dep_out_off.len() != v + 1
            || raw.dep_in_off.len() != v + 1
        {
            return Err(ArrayError::MalformedRaw {
                reason: "per-node vector length mismatch".into(),
            });
        }
        let mut arrays = Self::new(NodeIdSpace::restore(v as u32));
        arrays.alive = raw.alive.iter().map(|a| *a != 0).collect();
        arrays.parent = raw
            .parent
            .iter()
            .map(|p| {
                if *p == NO_PARENT {
                    None
                } else {
                    Some(NodeId(*p))
                }
            })
            .collect();
        arrays.graph = raw.graph.iter().map(|g| GraphId(*g)).collect();
        arrays.node_type = raw.node_type.clone();
        arrays.state = raw.state.clone();
        arrays.epoch = raw.epoch.iter().map(|e| Epoch(*e)).collect();
        arrays.attr_count = raw.attr_count.clone();
        arrays.child_off = raw.child_off.clone();
        arrays.child_tgt = raw.child_tgt.iter().map(|n| NodeId(*n)).collect();
        arrays.dep_out_off = raw.dep_out_off.clone();
        arrays.dep_out_tgt = raw.dep_out_tgt.iter().map(|e| EdgeId(*e)).collect();
        arrays.dep_in_off = raw.dep_in_off.clone();
        arrays.dep_in_tgt = raw.dep_in_tgt.iter().map(|e| EdgeId(*e)).collect();
        arrays.edges = raw
            .edges
            .iter()
            .map(|(src, dst, kind, aux, alive)| EdgeRec {
                src: NodeId(*src),
                dst: NodeId(*dst),
                kind: *kind,
                aux: *aux,
                alive: *alive != 0,
            })
            .collect();
        Ok(arrays)
    }

    /// Full structural invariant check (tests + debug builds).
    pub fn assert_invariants(&self) {
        let v = self.len();
        assert_eq!(self.alive.len(), v);
        assert_eq!(
            self.child_off.len(),
            v + 1,
            "composition CSR offsets are V+1"
        );
        assert_eq!(self.dep_out_off.len(), v + 1, "dep-out CSR offsets are V+1");
        assert_eq!(self.dep_in_off.len(), v + 1, "dep-in CSR offsets are V+1");

        let check_off = |off: &Vec<u64>, what: &str| {
            assert!(
                off.windows(2).all(|w| w[0] <= w[1]),
                "{what} offsets must be monotone"
            );
            assert_eq!(off[0], 0, "{what} offsets start at 0");
        };
        check_off(&self.child_off, "composition");
        check_off(&self.dep_out_off, "dep-out");
        check_off(&self.dep_in_off, "dep-in");

        for id in 0..v as u32 {
            let nid = NodeId(id);
            if !self.alive[id as usize] {
                continue;
            }
            if let Some(p) = self.parent[id as usize] {
                assert!(p.0 < id, "parent must be allocated before child");
                assert!(self.alive[p.0 as usize], "parent must be alive");
                assert_eq!(
                    self.graph[p.0 as usize], self.graph[id as usize],
                    "composition stays in-graph"
                );
            }
            let kids = self.children_of(nid);
            assert!(
                kids.windows(2).all(|w| w[0].0 < w[1].0),
                "children must be strictly ascending"
            );
            for run in [self.dep_out_ids(nid), self.dep_in_ids(nid)] {
                assert!(
                    run.windows(2).all(|w| w[0].0 < w[1].0),
                    "edge-id runs ascending"
                );
                for &e in run {
                    match self.edges.get(e.0 as usize) {
                        Some(rec) => assert!(rec.alive, "csr references only live edges"),
                        None => panic!("csr edge id {e} missing from edge table"),
                    }
                }
            }
        }
        for (i, rec) in self.edges.iter().enumerate() {
            if rec.alive {
                assert!(rec.src.0 != rec.dst.0, "no self-loops");
                assert!(self.alive[rec.src.0 as usize], "live edge src alive");
                assert!(self.alive[rec.dst.0 as usize], "live edge dst alive");
            } else {
                assert!(self.edges[i].src.0 < v as u32 && self.edges[i].dst.0 < v as u32);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(id: u16) -> GraphId {
        GraphId(id)
    }

    // Composition forest basics + invariants + epoch discipline.
    #[test]
    fn forest_build_delete_and_invariants() {
        let mut a = GraphArrays::new(NodeIdSpace::new());
        let root = a.add_root(g(1), 10).unwrap();
        let c1 = a.add_child(root, 10).unwrap();
        let c2 = a.add_child(root, 11).unwrap();
        let gc = a.add_child(c1, 11).unwrap();
        a.assert_invariants();

        assert_eq!(a.parent_of(c1), Some(root));
        assert_eq!(a.graph_of(gc), g(1), "child inherits parent's graph");
        assert_eq!(a.children_of(root), &[c1, c2], "children ascending");
        assert_eq!(a.epoch_of(root), Epoch(3), "creation + two children");
        assert_eq!(a.epoch_of(c1), Epoch(2), "creation + one grandchild");

        // Grandchild blocks deletion (HAS_DEPENDENTS guard).
        let e = a.delete_node(c1).unwrap_err();
        assert!(
            matches!(
                e,
                ArrayError::HasDependents {
                    children: 1,
                    dep_edges: 0
                }
            ),
            "got: {e}"
        );

        a.delete_node(gc).unwrap();
        assert_eq!(a.epoch_of(c1), Epoch(3), "losing a child bumps the parent");
        assert!(!a.is_alive(gc));
        a.delete_node(c1).unwrap();
        a.assert_invariants();
        assert_eq!(a.alive_count(), 2);
    }

    #[test]
    fn composition_never_crosses_graphs() {
        let mut a = GraphArrays::new(NodeIdSpace::new());
        let r1 = a.add_root(g(1), 0).unwrap();
        let r2 = a.add_root(g(2), 0).unwrap();
        // Composition is same-graph by construction (child inherits parent's
        // graph); the cross-graph case is structurally impossible: assert the
        // two roots own disjoint graph subsets.
        assert_ne!(a.graph_of(r1), a.graph_of(r2));
        a.assert_invariants();
    }

    // Dependency edges: attach, cross-graph legal, remove, epoch bumps.
    #[test]
    fn dependency_edges_attach_remove_epoch_bumps() {
        let mut a = GraphArrays::new(NodeIdSpace::new());
        let x = a.add_root(g(1), 0).unwrap();
        let y = a.add_root(g(2), 0).unwrap();
        let ex = a.epoch_of(x);
        let ey = a.epoch_of(y);

        let e1 = a.add_edge(x, y, 3, 0).unwrap(); // cross-graph dep edge: legal
        assert_eq!(a.epoch_of(x), ex + Epoch(1));
        assert_eq!(a.epoch_of(y), ey + Epoch(1));
        assert_eq!(a.dep_out_ids(x), &[e1]);
        assert_eq!(a.dep_in_ids(y), &[e1]);
        assert_eq!(a.edge(e1).unwrap().dst, y);
        a.assert_invariants();

        // Endpoint with a live edge cannot be deleted.
        let e = a.delete_node(x).unwrap_err();
        assert!(
            matches!(
                e,
                ArrayError::HasDependents {
                    children: 0,
                    dep_edges: 1
                }
            ),
            "got: {e}"
        );

        a.remove_edge(e1).unwrap();
        assert!(a.edge(e1).is_none(), "removed edge reads as absent");
        assert!(a.dep_out_ids(x).is_empty());
        assert!(a.dep_in_ids(y).is_empty());
        assert_eq!(a.epoch_of(x), ex + Epoch(2), "attach + detach = two bumps");
        a.assert_invariants();
        a.delete_node(x).unwrap();
    }

    #[test]
    fn self_loop_and_unknown_targets_rejected() {
        let mut a = GraphArrays::new(NodeIdSpace::new());
        let x = a.add_root(g(1), 0).unwrap();
        assert!(matches!(
            a.add_edge(x, x, 0, 0),
            Err(ArrayError::SelfLoop { node: _ })
        ));
        assert!(matches!(
            a.add_edge(x, NodeId(999), 0, 0),
            Err(ArrayError::NodeNotFound(_))
        ));
        assert!(matches!(
            a.remove_edge(EdgeId(0)),
            Err(ArrayError::EdgeNotFound(_))
        ));
    }

    // State and attr-count writes are epoch-bumping mutations.
    #[test]
    fn state_and_attr_count_bump_epoch() {
        let mut a = GraphArrays::new(NodeIdSpace::new());
        let n = a.add_root(g(0), 5).unwrap();
        assert_eq!(a.state_of(n), 0);
        assert_eq!(a.type_of(n), 5);
        a.set_state(n, 7).unwrap();
        assert_eq!(a.state_of(n), 7);
        a.set_attr_count(n, 12).unwrap();
        assert_eq!(a.attr_count_of(n), 12);
        assert_eq!(a.epoch_of(n), Epoch(3));
        assert!(a.set_state(NodeId(42), 1).is_err(), "unknown node rejected");
    }

    // Deleting twice / dead-node reads fail loudly.
    #[test]
    fn dead_nodes_are_rejected_on_mutation() {
        let mut a = GraphArrays::new(NodeIdSpace::new());
        let n = a.add_root(g(0), 0).unwrap();
        a.delete_node(n).unwrap();
        assert!(matches!(a.delete_node(n), Err(ArrayError::NodeDead(_))));
        assert!(matches!(a.set_state(n, 1), Err(ArrayError::NodeDead(_))));
        assert!(matches!(a.add_child(n, 0), Err(ArrayError::ParentDead(_))));
    }

    // Larger forest keeps CSR invariants under interleaved deletes.
    #[test]
    fn interleaved_structure_stays_consistent() {
        let mut a = GraphArrays::new(NodeIdSpace::new());
        let r = a.add_root(g(3), 0).unwrap();
        let mut kids = Vec::new();
        for _ in 0..8 {
            kids.push(a.add_child(r, 0).unwrap());
        }
        // Remove every second child, then re-add: CSR runs stay sorted.
        for k in kids.iter().step_by(2).copied().collect::<Vec<_>>() {
            a.delete_node(k).unwrap();
        }
        let later = a.add_child(r, 0).unwrap();
        a.assert_invariants();
        let children = a.children_of(r);
        assert!(children.windows(2).all(|w| w[0].0 < w[1].0));
        assert_eq!(children.len(), 5, "4 survivors + 1 late child");
        assert!(children.contains(&later));
    }
}

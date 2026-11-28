//! Typed write commands (SPEC-02 §7) and their replies — the kernel's
//! library-mode surface.
//!
//! Every surface builds THESE: the wire layer decodes TLV frames into them,
//! the CLI builds them directly. One command semantics for library calls,
//! UDS and TCP (SPEC-02 §1, WPT-1.1). Schema transactions and grants ride
//! the same surface — a command is whatever becomes one journal txn.

use crate::records::{Cell, Row};
use lattice_core::ids::NodeId;
use lattice_core::value::Value;
use lattice_schema::entities::{ActorClass, Grant, GrantTarget};
use lattice_schema::key::Key;
use lattice_schema::txn::SchemaOp;

/// A write command. `Δstructure` · `Δattributes` · `εactions`, plus schema
/// and authorization mutations (SPEC-02 §7).
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// `CREATE_NODE {graph, type_key, parent?, initial_attrs}`.
    CreateNode {
        graph: Key,
        node_type: Key,
        parent: Option<NodeId>,
        attrs: Vec<(Key, Value)>,
    },
    /// `DELETE_NODE {node, cascade}`.
    DeleteNode { node: NodeId, cascade: bool },
    /// `LINK {src, dst, kind, params}`. `guard` = GUARD `(sigma, tau)`; other
    /// kinds carry their params in `aux` (v1 command surface).
    Link {
        src: NodeId,
        dst: NodeId,
        kind: u8,
        aux: u32,
        guard: Option<(Key, Key)>,
    },
    /// `UNLINK {edge}`.
    Unlink { edge: u64 },
    /// `ATTR_SET {node, attr, value}` — scalar / enum / record cells (k = 0).
    AttrSet {
        node: NodeId,
        attr: Key,
        value: Value,
    },
    /// `CELLS_APPEND {node, attr, cells}` — point appends to one column.
    CellsAppend {
        node: NodeId,
        attr: Key,
        cells: Vec<Cell>,
    },
    /// `ROW_APPEND {node, table_set, rows}`.
    RowAppend {
        node: NodeId,
        set: Key,
        rows: Vec<Row>,
    },
    /// `ACTION {node, transition_key, payload?, actor_hint}`.
    Action {
        node: NodeId,
        transition: Key,
        actor: ActorClass,
        payload: Option<Value>,
    },
    /// `SCHEMA_TXN {ops}` — atomic; one schema epoch per commit.
    SchemaTxn { ops: Vec<SchemaOp> },
    /// `GRANT {role, subject, target}`.
    Grant { grant: Grant },
    /// `REVOKE {role, subject, target}`.
    Revoke {
        role: Key,
        subject: String,
        target: GrantTarget,
    },
}

/// The reply to an accepted command (SPEC-04 §3 "Returns" column).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Created(NodeId),
    Edge(u64),
    Attr {
        node: NodeId,
    },
    Rows {
        node: NodeId,
        first_k: u64,
        count: u32,
    },
    Action {
        node: NodeId,
        from: Key,
        to: Key,
        spawned: Vec<NodeId>,
        cancelled: Vec<(NodeId, String)>,
    },
    Schema {
        epoch: u64,
    },
    Ok,
}

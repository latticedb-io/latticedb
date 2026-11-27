//! Read commands, cursors, time travel (SPEC-02 §8, T-308).
//!
//! Read surface over the [`ReadAdapter`] trait — the engine implements it;
//! wire tests run against fakes. Result payloads are **Arrow IPC
//! record-batch streams** (capability ARROW, SPEC-02 §2, §12.1): NODES /
//! EDGES / CELLS batches. One DATA_ARROW frame carries three
//! length-prefixed IPC sub-streams (one per schema); every batch of a
//! given kind has an identical schema (WPT-8.3 stability).
//!
//! Reads see a consistent view as of one epoch (WPT-8.1): the adapter
//! resolves `at_epoch` BEFORE row extraction — the wire layer takes a
//! single row snapshot per dispatch and never interleaves mutations.
//! Time travel (WPT-8.2) is adapter-side: `at_epoch ≤ current` returns
//! the exact older view and leaves live state untouched.
//!
//! Cursors (idle TTL, default 60 s, `CURSOR_EXPIRED` after expiry) page
//! CELLS batches by row budget with no dup/loss (WPT-8.3).

use crate::commands::{CommandError, codes};
use crate::frame::FrameType;
use arrow::array::{
    Array, BooleanArray, Float64Array, Int64Array, LargeBinaryArray, StringArray, UInt8Array,
    UInt16Array, UInt32Array, UInt64Array,
};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::ipc::reader::StreamReader;
use arrow::ipc::writer::StreamWriter;
use arrow::record_batch::RecordBatch;
use lattice_core::value::Value;
use std::collections::BTreeMap;
use std::io::Cursor;

/// Read-side TLV tags (SPEC-02 §8; disjoint from §7 write tags).
pub mod read_tags {
    pub const CURSOR_ID: u16 = 0x0061;
    pub const BUDGET: u16 = 0x0062;
    pub const INDEX_KIND: u16 = 0x0063;
    pub const PARAMS: u16 = 0x0064;
    pub const STATE: u16 = 0x0065;
    pub const TYPE_CODE: u16 = 0x0066;
    pub const ATTR: u16 = 0x0067;
    pub const SHARED_KEY: u16 = 0x0068;
    pub const RANGE_FROM: u16 = 0x0069;
    pub const RANGE_TO: u16 = 0x006A;
    pub const OPS: u16 = 0x006B;
    pub const AT_EPOCH: u16 = 0x0060;
}

/// Read error codes (0x03xx registry extension).
pub mod read_codes {
    pub const CURSOR_EXPIRED: u16 = 0x0313;
    pub const CURSOR_NOT_FOUND: u16 = 0x0314;
    pub const BAD_CURSOR_PARAMS: u16 = 0x0315;
    pub const NOT_READABLE: u16 = 0x0316;
}

/// One node row of a snapshot read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRow {
    pub node: u32,
    pub graph: u16,
    pub type_code: u16,
    pub state: u8,
    pub epoch: u64,
    pub parent: Option<u32>,
    pub alive: bool,
}

/// One edge row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRow {
    pub edge_id: u64,
    pub src: u32,
    pub dst: u32,
    pub kind: u8,
    pub aux: u32,
    pub alive: bool,
}

/// One cell row (attribute value at key k).
#[derive(Debug, Clone, PartialEq)]
pub struct CellRow {
    pub node: u32,
    pub attr: u16,
    pub k: u64,
    pub value: Value,
}

/// Blocker entry (WPT-8.4: precise, named).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockerEntry {
    pub kind: String,
    pub detail: String,
}

/// History event for HISTORY reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEvent {
    pub lsn: u64,
    pub kind: String,
    pub detail: String,
    pub author: String,
}

/// SCHEMA_PLAN dry-run outcome (SPEC-01 §16).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanOutcome {
    pub op_index: usize,
    pub accepted: bool,
    pub error: Option<String>,
}

/// Cursor index kinds (SPEC-02 §8 CURSOR_OPEN).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorKind {
    ByState { graph: u16, state: u8 },
    AttrPresence { attr: u16 },
    SharedKey { key: String },
    TypeScan { type_code: u16 },
}

impl CursorKind {
    pub const BY_STATE: u8 = 0;
    pub const ATTR_PRESENCE: u8 = 1;
    pub const SHARED_KEY: u8 = 2;
    pub const TYPE_SCAN: u8 = 3;

    /// Decode from TLV params. Unknown kind ⇒ BAD_CURSOR_PARAMS with the
    /// known-kinds hint (hint law, §7 semantics carried to §8 params).
    fn from_wire(kind: u8, params: &BTreeMap<u16, Vec<u8>>) -> Result<Self, CommandError> {
        let le16 = |tag: u16| -> Option<u16> {
            params.get(&tag).and_then(|b| {
                if b.len() == 2 {
                    Some(u16::from_le_bytes([b[0], b[1]]))
                } else {
                    None
                }
            })
        };
        match kind {
            Self::BY_STATE => {
                let state = params
                    .get(&read_tags::PARAMS)
                    .and_then(|b| b.first().copied())
                    .ok_or_else(|| missing_param("params.state"))?;
                Ok(Self::ByState {
                    graph: le16(read_tags::STATE).unwrap_or(0),
                    state,
                })
            }
            Self::ATTR_PRESENCE => Ok(Self::AttrPresence {
                attr: le16(read_tags::ATTR).ok_or_else(|| missing_param("attr"))?,
            }),
            Self::SHARED_KEY => Ok(Self::SharedKey {
                key: params
                    .get(&read_tags::SHARED_KEY)
                    .map(|b| String::from_utf8_lossy(b).into_owned())
                    .ok_or_else(|| missing_param("shared_key"))?,
            }),
            Self::TYPE_SCAN => Ok(Self::TypeScan {
                type_code: le16(read_tags::TYPE_CODE).ok_or_else(|| missing_param("type_code"))?,
            }),
            _ => Err(CommandError {
                code: read_codes::BAD_CURSOR_PARAMS,
                hint: "index kinds: 0=by_state 1=attr_presence 2=shared_key 3=type_scan".into(),
                detail: format!("unknown index kind {kind}"),
            }),
        }
    }
}

fn missing_param(what: &'static str) -> CommandError {
    CommandError {
        code: read_codes::BAD_CURSOR_PARAMS,
        hint: format!("tag for {what} is required"),
        detail: format!("CURSOR_OPEN missing {what}"),
    }
}

/// The engine read surface (mirror of the write-side CommandExecutor).
pub trait ReadAdapter {
    fn current_epoch(&self) -> u64;
    /// Pin subsequent row extraction to `at_epoch` (None = live view).
    /// WPT-8.2: `at_epoch ≤ current` returns the exact older view; the
    /// live state is untouched. Pins reset per dispatch.
    fn pin_epoch(&mut self, at_epoch: Option<u64>) -> Result<(), CommandError>;
    fn node_count(&self) -> usize;
    fn node_row(&self, idx: usize) -> Option<NodeRow>;
    fn edge_count(&self) -> usize;
    fn edge_row(&self, idx: usize) -> Option<EdgeRow>;
    fn cell_count(&self) -> usize;
    fn cell_row(&self, idx: usize) -> Option<CellRow>;
    fn node_alive(&self, node: u32) -> bool;
    /// Cells of one node ordered by (attr, k).
    fn cells_of(&self, node: u32) -> Vec<CellRow>;
    fn state_name(&self, node: u32) -> Option<String>;
    fn blockers_of(&self, node: u32) -> Vec<BlockerEntry>;
    fn history_of(&self, node: u32, from_lsn: u64, to_lsn: u64) -> Vec<HistoryEvent>;
    /// Dry-run: validate ops, no commit (SPEC-01 §16). Never advances the
    /// catalog epoch.
    fn schema_plan(&mut self, ops: &[u8]) -> Result<Vec<PlanOutcome>, CommandError>;
    /// Cursor row selection is engine-side: only the engine can filter by
    /// node state / type / shared key. Rows ordered by (node, attr, k).
    fn cursor_rows(&self, kind: &CursorKind) -> Vec<CellRow>;
}

/// Server-side cursor registry (idle TTL, default 60 s, SPEC-02 §8).
pub struct CursorTable {
    next_id: u64,
    open: BTreeMap<u64, OpenCursor>,
    pub ttl_s: u64,
}

struct OpenCursor {
    rows: Vec<CellRow>,
    position: usize,
    last_touch_us: u64,
}

impl CursorTable {
    pub fn new(ttl_s: u64) -> Self {
        Self {
            next_id: 1,
            open: BTreeMap::new(),
            ttl_s,
        }
    }

    /// Open on the adapter's CURRENT selection (consistent view at open;
    /// later writes don't inject into an open cursor — §8 snapshot
    /// semantics).
    pub fn open(&mut self, kind: CursorKind, adapter: &dyn ReadAdapter, now_us: u64) -> u64 {
        let rows = adapter.cursor_rows(&kind);
        let id = self.next_id;
        self.next_id += 1;
        self.open.insert(
            id,
            OpenCursor {
                rows,
                position: 0,
                last_touch_us: now_us,
            },
        );
        id
    }

    /// Fetch one page: `budget_rows` rows, no dup/loss, schema stable.
    pub fn fetch(
        &mut self,
        cursor_id: u64,
        budget_rows: usize,
        now_us: u64,
    ) -> Result<Vec<u8>, CommandError> {
        let Some(cur) = self.open.get_mut(&cursor_id) else {
            return Err(CommandError {
                code: read_codes::CURSOR_NOT_FOUND,
                hint: "open a cursor first (CURSOR_OPEN)".into(),
                detail: format!("cursor {cursor_id} not found"),
            });
        };
        if now_us.saturating_sub(cur.last_touch_us) > self.ttl_s * 1_000_000 {
            self.open.remove(&cursor_id);
            return Err(CommandError {
                code: read_codes::CURSOR_EXPIRED,
                hint: format!("idle TTL is {}s — reopen with CURSOR_OPEN", self.ttl_s),
                detail: format!("cursor {cursor_id} expired"),
            });
        }
        cur.last_touch_us = now_us;
        let end = (cur.position + budget_rows).min(cur.rows.len());
        let page: Vec<CellRow> = cur.rows[cur.position..end].to_vec();
        cur.position = end;
        encode_cell_batches(&page)
    }

    pub fn close(&mut self, cursor_id: u64) -> Result<(), CommandError> {
        self.open
            .remove(&cursor_id)
            .map(|_| ())
            .ok_or_else(|| CommandError {
                code: read_codes::CURSOR_NOT_FOUND,
                hint: String::new(),
                detail: format!("cursor {cursor_id} not found"),
            })
    }

    /// Server-side GC of idle cursors (returns dropped ids).
    pub fn gc_idle(&mut self, now_us: u64) -> Vec<u64> {
        let ttl = self.ttl_s * 1_000_000;
        let expired: Vec<u64> = self
            .open
            .iter()
            .filter(|(_, c)| now_us.saturating_sub(c.last_touch_us) > ttl)
            .map(|(id, _)| *id)
            .collect();
        for id in &expired {
            self.open.remove(id);
        }
        expired
    }

    pub fn is_open(&self, cursor_id: u64) -> bool {
        self.open.contains_key(&cursor_id)
    }
}

// ---------------------------------------------------------------------------
// Arrow IPC encoding (SPEC-02 §2 bulk reads, §12.1)
// ---------------------------------------------------------------------------

fn arrow_err(what: &'static str) -> impl Fn(arrow::error::ArrowError) -> CommandError {
    move |e| CommandError {
        code: codes::INTERNAL,
        hint: String::new(),
        detail: format!("arrow {what}: {e}"),
    }
}

fn cell_schema() -> std::sync::Arc<Schema> {
    std::sync::Arc::new(Schema::new(vec![
        Field::new("node", DataType::UInt32, false),
        Field::new("attr", DataType::UInt16, false),
        Field::new("k", DataType::UInt64, false),
        Field::new("v_bool", DataType::Boolean, true),
        Field::new("v_i64", DataType::Int64, true),
        Field::new("v_f64", DataType::Float64, true),
        Field::new("v_str", DataType::Utf8, true),
        Field::new("v_ts", DataType::Int64, true),
        Field::new("v_bytes", DataType::LargeBinary, true),
    ]))
}

fn nodes_schema() -> std::sync::Arc<Schema> {
    std::sync::Arc::new(Schema::new(vec![
        Field::new("node", DataType::UInt32, false),
        Field::new("graph", DataType::UInt16, false),
        Field::new("type", DataType::UInt16, false),
        Field::new("state", DataType::UInt8, false),
        Field::new("epoch", DataType::UInt64, false),
        Field::new("parent", DataType::UInt32, true),
        Field::new("alive", DataType::Boolean, false),
    ]))
}

fn edges_schema() -> std::sync::Arc<Schema> {
    std::sync::Arc::new(Schema::new(vec![
        Field::new("edge_id", DataType::UInt64, false),
        Field::new("src", DataType::UInt32, false),
        Field::new("dst", DataType::UInt32, false),
        Field::new("kind", DataType::UInt8, false),
        Field::new("aux", DataType::UInt32, false),
        Field::new("alive", DataType::Boolean, false),
    ]))
}

fn cell_batch(rows: &[CellRow]) -> Result<RecordBatch, CommandError> {
    let n = rows.len();
    let mut v_bool: Vec<Option<bool>> = Vec::with_capacity(n);
    let mut v_i64: Vec<Option<i64>> = Vec::with_capacity(n);
    let mut v_f64: Vec<Option<f64>> = Vec::with_capacity(n);
    let mut v_str: Vec<Option<&str>> = Vec::with_capacity(n);
    let mut v_ts: Vec<Option<i64>> = Vec::with_capacity(n);
    let mut v_bytes: Vec<Option<&[u8]>> = Vec::with_capacity(n);
    for c in rows {
        v_bool.push(match c.value {
            Value::Bool(b) => Some(b),
            _ => None,
        });
        v_i64.push(match c.value {
            Value::I64(v) => Some(v),
            _ => None,
        });
        v_f64.push(match c.value {
            Value::F64(v) => Some(v),
            _ => None,
        });
        v_str.push(match &c.value {
            Value::Str(s) => Some(s.as_ref()),
            _ => None,
        });
        v_ts.push(match c.value {
            Value::Timestamp(v) => Some(v),
            _ => None,
        });
        v_bytes.push(match &c.value {
            Value::Bytes(b) => Some(b.as_ref()),
            _ => None,
        });
    }
    RecordBatch::try_new(
        cell_schema(),
        vec![
            std::sync::Arc::new(UInt32Array::from(
                rows.iter().map(|c| c.node).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt16Array::from(
                rows.iter().map(|c| c.attr).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt64Array::from(
                rows.iter().map(|c| c.k).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(BooleanArray::from(v_bool)),
            std::sync::Arc::new(Int64Array::from(v_i64)),
            std::sync::Arc::new(Float64Array::from(v_f64)),
            std::sync::Arc::new(StringArray::from(v_str)),
            std::sync::Arc::new(Int64Array::from(v_ts)),
            std::sync::Arc::new(LargeBinaryArray::from(v_bytes)),
        ],
    )
    .map_err(arrow_err("cells batch"))
}

/// CELLS batches → one IPC stream.
pub fn encode_cell_batches(rows: &[CellRow]) -> Result<Vec<u8>, CommandError> {
    let batch = cell_batch(rows)?;
    let mut buf = Vec::new();
    {
        let mut w =
            StreamWriter::try_new(&mut buf, batch.schema().as_ref()).map_err(arrow_err("ipc"))?;
        w.write(&batch).map_err(arrow_err("ipc"))?;
        w.finish().map_err(arrow_err("ipc"))?;
    }
    Ok(buf)
}

/// Length-prefixed sub-stream container: [u32 len][bytes] × 3
/// (nodes, edges, cells). One DATA_ARROW frame.
fn pack3(nodes: Vec<u8>, edges: Vec<u8>, cells: Vec<u8>) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + nodes.len() + edges.len() + cells.len());
    for part in [nodes, edges, cells] {
        out.extend_from_slice(&(part.len() as u32).to_le_bytes());
        out.extend_from_slice(&part);
    }
    out
}

/// Unpack the 3-sub-stream container.
pub fn unpack3(bytes: &[u8]) -> Result<[&[u8]; 3], CommandError> {
    let mut parts = Vec::with_capacity(3);
    let mut pos = 0usize;
    for _ in 0..3 {
        if pos + 4 > bytes.len() {
            return Err(CommandError {
                code: codes::INTERNAL,
                hint: "DATA_ARROW framing".into(),
                detail: "truncated sub-stream length".into(),
            });
        }
        let len = u32::from_le_bytes([bytes[pos], bytes[pos + 1], bytes[pos + 2], bytes[pos + 3]])
            as usize;
        pos += 4;
        let end = pos + len;
        if end > bytes.len() {
            return Err(CommandError {
                code: codes::INTERNAL,
                hint: "DATA_ARROW framing".into(),
                detail: format!(
                    "sub-stream declares {len} bytes, frame has {}",
                    bytes.len() - pos
                ),
            });
        }
        parts.push(&bytes[pos..end]);
        pos = end;
    }
    let [a, b, c] = parts[..] else {
        return Err(CommandError {
            code: codes::INTERNAL,
            hint: "DATA_ARROW framing".into(),
            detail: "expected 3 sub-streams".into(),
        });
    };
    Ok([a, b, c])
}

/// Full snapshot read: nodes → edges → cells, consistent as of the
/// adapter's view at dispatch time (WPT-8.1).
pub fn encode_snapshot(
    nodes: &[NodeRow],
    edges: &[EdgeRow],
    cells: &[CellRow],
) -> Result<Vec<u8>, CommandError> {
    let node_batch = RecordBatch::try_new(
        nodes_schema(),
        vec![
            std::sync::Arc::new(UInt32Array::from(
                nodes.iter().map(|r| r.node).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt16Array::from(
                nodes.iter().map(|r| r.graph).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt16Array::from(
                nodes.iter().map(|r| r.type_code).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt8Array::from(
                nodes.iter().map(|r| r.state).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt64Array::from(
                nodes.iter().map(|r| r.epoch).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt32Array::from(
                nodes.iter().map(|r| r.parent).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(BooleanArray::from(
                nodes.iter().map(|r| r.alive).collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(arrow_err("nodes batch"))?;
    let edge_batch = RecordBatch::try_new(
        edges_schema(),
        vec![
            std::sync::Arc::new(UInt64Array::from(
                edges.iter().map(|r| r.edge_id).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt32Array::from(
                edges.iter().map(|r| r.src).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt32Array::from(
                edges.iter().map(|r| r.dst).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt8Array::from(
                edges.iter().map(|r| r.kind).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(UInt32Array::from(
                edges.iter().map(|r| r.aux).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(BooleanArray::from(
                edges.iter().map(|r| r.alive).collect::<Vec<_>>(),
            )),
        ],
    )
    .map_err(arrow_err("edges batch"))?;
    let cells_stream = encode_cell_batches(cells)?;
    let mut nodes_buf = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut nodes_buf, node_batch.schema().as_ref())
            .map_err(arrow_err("ipc"))?;
        w.write(&node_batch).map_err(arrow_err("ipc"))?;
        w.finish().map_err(arrow_err("ipc"))?;
    }
    let mut edges_buf = Vec::new();
    {
        let mut w = StreamWriter::try_new(&mut edges_buf, edge_batch.schema().as_ref())
            .map_err(arrow_err("ipc"))?;
        w.write(&edge_batch).map_err(arrow_err("ipc"))?;
        w.finish().map_err(arrow_err("ipc"))?;
    }
    Ok(pack3(nodes_buf, edges_buf, cells_stream))
}

/// Decode a CELLS IPC stream back into rows (tests + host glue).
pub fn decode_cell_stream(bytes: &[u8]) -> Result<Vec<CellRow>, CommandError> {
    let reader = StreamReader::try_new(Cursor::new(bytes), None).map_err(arrow_err("read"))?;
    let mut out = Vec::new();
    for batch in reader {
        let batch = batch.map_err(arrow_err("read"))?;
        push_batch_cells(&batch, &mut out)?;
    }
    Ok(out)
}

fn push_batch_cells(batch: &RecordBatch, out: &mut Vec<CellRow>) -> Result<(), CommandError> {
    let cols = batch.columns();
    if cols.len() != 9 {
        return Err(schema_drift(format!(
            "expected 9 columns, got {}",
            cols.len()
        )));
    }
    let (
        Some(node),
        Some(attr),
        Some(k),
        Some(vb),
        Some(vi),
        Some(vf),
        Some(vs),
        Some(vt),
        Some(vy),
    ) = (
        cols[0].as_any().downcast_ref::<UInt32Array>(),
        cols[1].as_any().downcast_ref::<UInt16Array>(),
        cols[2].as_any().downcast_ref::<UInt64Array>(),
        cols[3].as_any().downcast_ref::<BooleanArray>(),
        cols[4].as_any().downcast_ref::<Int64Array>(),
        cols[5].as_any().downcast_ref::<Float64Array>(),
        cols[6].as_any().downcast_ref::<StringArray>(),
        cols[7].as_any().downcast_ref::<Int64Array>(),
        cols[8].as_any().downcast_ref::<LargeBinaryArray>(),
    )
    else {
        return Err(schema_drift("column type mismatch".into()));
    };
    for i in 0..batch.num_rows() {
        let value = if vb.is_valid(i) {
            Value::Bool(vb.value(i))
        } else if vi.is_valid(i) {
            Value::I64(vi.value(i))
        } else if vf.is_valid(i) {
            Value::F64(vf.value(i))
        } else if vs.is_valid(i) {
            Value::Str(vs.value(i).into())
        } else if vt.is_valid(i) {
            Value::Timestamp(vt.value(i))
        } else if vy.is_valid(i) {
            Value::Bytes(vy.value(i).to_vec().into_boxed_slice())
        } else {
            Value::Bool(false)
        };
        out.push(CellRow {
            node: node.value(i),
            attr: attr.value(i),
            k: k.value(i),
            value,
        });
    }
    Ok(())
}

fn schema_drift(detail: String) -> CommandError {
    CommandError {
        code: codes::INTERNAL,
        hint: "cells schema drift".into(),
        detail,
    }
}

/// Per-stream schema fingerprint (WPT-8.3: stable across batches) —
/// `name:type;` per field, deterministic order.
pub fn schema_fingerprint(bytes: &[u8]) -> Result<Vec<u8>, CommandError> {
    let reader = StreamReader::try_new(Cursor::new(bytes), None).map_err(arrow_err("read"))?;
    let mut buf = Vec::new();
    for f in reader.schema().fields() {
        buf.extend_from_slice(format!("{}:{:?};", f.name(), f.data_type()).as_bytes());
    }
    Ok(buf)
}

/// Read dispatch outcome.
#[derive(Debug)]
pub enum ReadReply {
    /// ACK payload (TLV).
    Ack(Vec<u8>),
    /// DATA_ARROW payload (IPC bytes).
    Data(Vec<u8>),
}

/// Route one read frame to its handler (SPEC-02 §8).
pub struct ReadDispatch;

impl ReadDispatch {
    pub fn dispatch(
        adapter: &mut dyn ReadAdapter,
        cursors: &mut CursorTable,
        frame_type: FrameType,
        payload: &BTreeMap<u16, Vec<u8>>,
        now_us: u64,
    ) -> Result<ReadReply, CommandError> {
        let le16 = |tag: u16| -> Option<u16> {
            payload.get(&tag).and_then(|b| {
                if b.len() == 2 {
                    Some(u16::from_le_bytes([b[0], b[1]]))
                } else {
                    None
                }
            })
        };
        let le32 = |tag: u16| -> Option<u32> {
            payload.get(&tag).and_then(|b| {
                if b.len() == 4 {
                    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                } else {
                    None
                }
            })
        };
        let le64 = |tag: u16| -> Option<u64> {
            payload
                .get(&tag)
                .and_then(|b| b.first_chunk::<8>().copied().map(u64::from_le_bytes))
        };
        let need_node = || CommandError {
            code: codes::INVALID_KEY,
            hint: "tag 0x0020 (node) required".into(),
            detail: "read command without node".into(),
        };
        match frame_type {
            FrameType::Snapshot => {
                // graph (0x0010) + at_epoch (0x0060) → adapter-resolved view
                let _graph = le16(0x0010);
                let at_epoch = le64(read_tags::AT_EPOCH);
                adapter.pin_epoch(at_epoch)?;
                let nodes: Vec<NodeRow> = (0..adapter.node_count())
                    .filter_map(|i| adapter.node_row(i))
                    .collect();
                let edges: Vec<EdgeRow> = (0..adapter.edge_count())
                    .filter_map(|i| adapter.edge_row(i))
                    .collect();
                let cells: Vec<CellRow> = (0..adapter.cell_count())
                    .filter_map(|i| adapter.cell_row(i))
                    .collect();
                Ok(ReadReply::Data(encode_snapshot(&nodes, &edges, &cells)?))
            }
            FrameType::NodeRead => {
                let node = le32(0x0020).ok_or_else(need_node)?;
                if !adapter.node_alive(node) {
                    return Err(CommandError {
                        code: codes::NOT_FOUND,
                        hint: "node must exist".into(),
                        detail: format!("node {node} not found"),
                    });
                }
                let cells = adapter.cells_of(node);
                let mut ack = Vec::new();
                if let Some(state) = adapter.state_name(node) {
                    ack.extend_from_slice(&read_tags::STATE.to_le_bytes());
                    ack.extend_from_slice(&(state.len() as u32).to_le_bytes());
                    ack.extend_from_slice(state.as_bytes());
                }
                // Inline TLV: latest cells — scalars + record presence +
                // vector head/tail previews ride as plain (node, attr, k,
                // value) tuples at this layer.
                ack.extend_from_slice(&READ_CELL_COUNT_TAG.to_le_bytes());
                ack.extend_from_slice(&4u32.to_le_bytes());
                ack.extend_from_slice(&(cells.len() as u32).to_le_bytes());
                for c in &cells {
                    ack.extend_from_slice(&c.node.to_le_bytes());
                    ack.extend_from_slice(&c.attr.to_le_bytes());
                    ack.extend_from_slice(&c.k.to_le_bytes());
                    encode_value_tlv(&c.value, &mut ack);
                }
                Ok(ReadReply::Ack(ack))
            }
            FrameType::CursorOpen => {
                let kind_byte = payload
                    .get(&read_tags::INDEX_KIND)
                    .and_then(|b| b.first().copied())
                    .ok_or_else(|| missing_param("index_kind"))?;
                let kind = CursorKind::from_wire(kind_byte, payload)?;
                let id = cursors.open(kind, adapter, now_us);
                let mut ack = Vec::new();
                ack.extend_from_slice(&read_tags::CURSOR_ID.to_le_bytes());
                ack.extend_from_slice(&8u32.to_le_bytes());
                ack.extend_from_slice(&id.to_le_bytes());
                Ok(ReadReply::Ack(ack))
            }
            FrameType::CursorFetch => {
                let id = le64(read_tags::CURSOR_ID).ok_or_else(|| missing_param("cursor_id"))?;
                let budget = le64(read_tags::BUDGET).unwrap_or(4096) as usize;
                Ok(ReadReply::Data(cursors.fetch(id, budget, now_us)?))
            }
            FrameType::CursorClose => {
                let id = le64(read_tags::CURSOR_ID).ok_or_else(|| missing_param("cursor_id"))?;
                cursors.close(id)?;
                Ok(ReadReply::Ack(Vec::new()))
            }
            FrameType::Blockers => {
                let node = le32(0x0020).ok_or_else(need_node)?;
                let entries = adapter.blockers_of(node);
                let mut ack = Vec::new();
                ack.extend_from_slice(&READ_COUNT_TAG.to_le_bytes());
                ack.extend_from_slice(&4u32.to_le_bytes());
                ack.extend_from_slice(&(entries.len() as u32).to_le_bytes());
                for b in &entries {
                    ack.extend_from_slice(&(b.kind.len() as u32).to_le_bytes());
                    ack.extend_from_slice(b.kind.as_bytes());
                    ack.extend_from_slice(&(b.detail.len() as u32).to_le_bytes());
                    ack.extend_from_slice(b.detail.as_bytes());
                }
                Ok(ReadReply::Ack(ack))
            }
            FrameType::History => {
                let node = le32(0x0020).ok_or_else(need_node)?;
                let from = le64(read_tags::RANGE_FROM).unwrap_or(0);
                let to = le64(read_tags::RANGE_TO).unwrap_or(u64::MAX);
                let events = adapter.history_of(node, from, to);
                let mut ack = Vec::new();
                ack.extend_from_slice(&READ_COUNT_TAG.to_le_bytes());
                ack.extend_from_slice(&4u32.to_le_bytes());
                ack.extend_from_slice(&(events.len() as u32).to_le_bytes());
                for e in &events {
                    ack.extend_from_slice(&e.lsn.to_le_bytes());
                    ack.extend_from_slice(&(e.kind.len() as u32).to_le_bytes());
                    ack.extend_from_slice(e.kind.as_bytes());
                    ack.extend_from_slice(&(e.detail.len() as u32).to_le_bytes());
                    ack.extend_from_slice(e.detail.as_bytes());
                    ack.extend_from_slice(&(e.author.len() as u32).to_le_bytes());
                    ack.extend_from_slice(e.author.as_bytes());
                }
                Ok(ReadReply::Ack(ack))
            }
            FrameType::SchemaPlan => {
                let ops = payload
                    .get(&read_tags::OPS)
                    .map(|b| b.as_slice())
                    .unwrap_or(&[]);
                let outcomes = adapter.schema_plan(ops)?;
                let mut ack = Vec::new();
                ack.extend_from_slice(&READ_COUNT_TAG.to_le_bytes());
                ack.extend_from_slice(&4u32.to_le_bytes());
                ack.extend_from_slice(&(outcomes.len() as u32).to_le_bytes());
                for o in &outcomes {
                    ack.extend_from_slice(&(o.op_index as u32).to_le_bytes());
                    ack.push(u8::from(o.accepted));
                    match &o.error {
                        Some(e) => {
                            ack.extend_from_slice(&(e.len() as u32).to_le_bytes());
                            ack.extend_from_slice(e.as_bytes());
                        }
                        None => ack.extend_from_slice(&0u32.to_le_bytes()),
                    }
                }
                Ok(ReadReply::Ack(ack))
            }
            _ => Err(CommandError {
                code: read_codes::NOT_READABLE,
                hint: "read frames: SNAPSHOT NODE_READ CURSOR_* BLOCKERS HISTORY SCHEMA_PLAN"
                    .into(),
                detail: "frame is not a read command".into(),
            }),
        }
    }
}

/// Generic count tag for read ACKs (entry count follows as [len][u32]).
pub const READ_COUNT_TAG: u16 = 0x006C;
/// NODE_READ cell-count tag.
pub const READ_CELL_COUNT_TAG: u16 = 0x006D;

/// Value → inline TLV ([tag 1B][len 4B][bytes]) — §8 NODE_READ inline form.
/// Tags mirror the §7 inline-value convention (1=Bool 2=I64 3=F64 5=Str
/// 6=Timestamp 7=Bytes); record/enum values degrade to their presence
/// prefix at this layer.
fn encode_value_tlv(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Bool(b) => {
            out.push(1);
            out.extend_from_slice(&1u32.to_le_bytes());
            out.push(u8::from(*b));
        }
        Value::I64(v) => {
            out.push(2);
            out.extend_from_slice(&8u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::F64(v) => {
            out.push(3);
            out.extend_from_slice(&8u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Str(s) => {
            out.push(5);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Timestamp(v) => {
            out.push(6);
            out.extend_from_slice(&8u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::Bytes(b) => {
            out.push(7);
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        Value::Enum(v) => {
            out.push(10);
            out.extend_from_slice(&1u32.to_le_bytes());
            out.push(*v);
        }
        Value::Decimal(d) => {
            let s = d.to_fixed_string();
            out.push(4);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Duration(v) => {
            out.push(8);
            out.extend_from_slice(&8u32.to_le_bytes());
            out.extend_from_slice(&v.to_le_bytes());
        }
        Value::NodeRef(n) => {
            out.push(9);
            out.extend_from_slice(&4u32.to_le_bytes());
            out.extend_from_slice(&n.0.to_le_bytes());
        }
        Value::Record(r) => {
            out.push(11);
            let presence = r.presence_mask().to_le_bytes();
            out.extend_from_slice(&(presence.len() as u32).to_le_bytes());
            out.extend_from_slice(&presence);
        }
    }
}

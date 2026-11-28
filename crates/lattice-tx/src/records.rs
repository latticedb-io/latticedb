//! Journal record payloads (SPEC-03 §6): the durable form of every fact.
//!
//! The journal frames and sequences opaque payloads; their layout is
//! tx-engine territory (`lattice_storage::journal`). Laws:
//! - little-endian, fixed-width integers; keys / strings / values are
//!   length-prefixed (`u16` / `u32` / `u32`);
//! - node types, attributes, transitions and graphs ride by KEY, never by a
//!   position-dependent code — ids are resolved at apply time against the
//!   lineage of the epoch (ARCHITECTURE §4);
//! - deterministic: a record always encodes to the same bytes (SPEC-03 D1,
//!   D4 — nothing wall-clock, hostname or PID enters a payload).
//!
//! | kind          | payload                                                                                     |
//! |---------------|---------------------------------------------------------------------------------------------|
//! | `ADD_VERTEX`  | `[node u32][parent u32: 0xFFFFFFFF = root][graph u16][type key][author u8: 0 user, 1 engine]`  |
//! | `DEL_VERTEX`  | `[node u32]`                                                                                |
//! | `ADD_EDGE`    | `[edge u64][src u32][dst u32][kind u8][aux u32][guard u8: 0 none, 1 (sigma key)(tau key)]`  |
//! | `DEL_EDGE`    | `[edge u64]`                                                                                |
//! | `SET_ATTR`    | `[node u32][attr key][value]`                                                               |
//! | `APPEND_CELLS`| `[node u32][attr key][count u32]([k u64][value]) × count`                                   |
//! | `ROW_APPEND`  | `[node u32][set key][count u32]([k_skip u32][cells u32]([attr key][value]) × cells) × count`|
//! | `ACTION`      | `[node u32][transition key][actor u8][subject str][payload u8 (0 none, 1 value)][from key][to key]` |
//! | `SCHEMA_TXN`  | op-log encoding of the ops (`lattice_schema::opslog`)                                       |
//! | `GRANT`       | op-log encoding of the single `GrantOp`                                                     |
//! | `REVOKE`      | op-log encoding of the single `Revoke`                                                      |
//!
//! An engine-authored `ADD_VERTEX` (a spawn effect) follows its `ACTION` in
//! the same txn: replay verifies it against what the action produced and
//! never creates the vertex twice.

use lattice_core::value::Value;
use lattice_schema::entities::{ActorClass, Grant, GrantTarget};
use lattice_schema::key::Key;
use lattice_schema::txn::SchemaOp;
use lattice_storage::journal::RecordKind;
use std::fmt;

/// Who created a vertex: a client command, or the engine (spawn effect).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Author {
    User = 0,
    Engine = 1,
}

/// One vector cell: `(k, value)`.
pub type Cell = (u64, Value);

/// One row of a `ROW_APPEND`: advance the node's key axis by `k_skip`, then
/// write the addressed cells at that `k` (unaddressed attributes are absent).
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub k_skip: u32,
    pub cells: Vec<(Key, Value)>,
}

/// Typed content of one journal record.
#[derive(Debug, Clone, PartialEq)]
pub enum RecordData {
    AddVertex {
        node: u32,
        /// `None` = composition root.
        parent: Option<u32>,
        graph: u16,
        node_type: Key,
        authored: Author,
    },
    DelVertex {
        node: u32,
    },
    AddEdge {
        edge: u64,
        src: u32,
        dst: u32,
        kind: u8,
        aux: u32,
        /// GUARD params `(sigma, tau)`.
        guard: Option<(Key, Key)>,
    },
    DelEdge {
        edge: u64,
    },
    SetAttr {
        node: u32,
        attr: Key,
        value: Value,
    },
    AppendCells {
        node: u32,
        attr: Key,
        cells: Vec<Cell>,
    },
    RowAppend {
        node: u32,
        set: Key,
        rows: Vec<Row>,
    },
    Action {
        node: u32,
        transition: Key,
        actor: ActorClass,
        subject: String,
        payload: Option<Value>,
        from: Key,
        to: Key,
    },
    SchemaTxn {
        ops: Vec<SchemaOp>,
    },
    Grant {
        grant: Grant,
    },
    Revoke {
        role: Key,
        subject: String,
        target: GrantTarget,
    },
}

/// One journal record: its typed data (the kind follows from the data).
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub data: RecordData,
}

impl Record {
    pub fn new(data: RecordData) -> Self {
        Self { data }
    }

    pub fn kind(&self) -> RecordKind {
        use RecordData::*;
        match &self.data {
            AddVertex { .. } => RecordKind::AddVertex,
            DelVertex { .. } => RecordKind::DelVertex,
            AddEdge { .. } => RecordKind::AddEdge,
            DelEdge { .. } => RecordKind::DelEdge,
            SetAttr { .. } => RecordKind::SetAttr,
            AppendCells { .. } => RecordKind::AppendCells,
            RowAppend { .. } => RecordKind::RowAppendBatch,
            Action { .. } => RecordKind::Action,
            SchemaTxn { .. } => RecordKind::SchemaTxn,
            Grant { .. } => RecordKind::Grant,
            Revoke { .. } => RecordKind::Revoke,
        }
    }

    /// Deterministic encoding of the payload.
    pub fn encode(&self) -> Vec<u8> {
        use RecordData::*;
        let mut out = Vec::new();
        match &self.data {
            AddVertex {
                node,
                parent,
                graph,
                node_type,
                authored,
            } => {
                out.extend_from_slice(&node.to_le_bytes());
                out.extend_from_slice(
                    &parent.unwrap_or(lattice_core::ids::NO_PARENT).to_le_bytes(),
                );
                out.extend_from_slice(&graph.to_le_bytes());
                put_key(&mut out, node_type);
                out.push(*authored as u8);
            }
            DelVertex { node } => out.extend_from_slice(&node.to_le_bytes()),
            AddEdge {
                edge,
                src,
                dst,
                kind,
                aux,
                guard,
            } => {
                out.extend_from_slice(&edge.to_le_bytes());
                out.extend_from_slice(&src.to_le_bytes());
                out.extend_from_slice(&dst.to_le_bytes());
                out.push(*kind);
                out.extend_from_slice(&aux.to_le_bytes());
                match guard {
                    None => out.push(0),
                    Some((sigma, tau)) => {
                        out.push(1);
                        put_key(&mut out, sigma);
                        put_key(&mut out, tau);
                    }
                }
            }
            DelEdge { edge } => out.extend_from_slice(&edge.to_le_bytes()),
            SetAttr { node, attr, value } => {
                out.extend_from_slice(&node.to_le_bytes());
                put_key(&mut out, attr);
                put_value(&mut out, value);
            }
            AppendCells { node, attr, cells } => {
                out.extend_from_slice(&node.to_le_bytes());
                put_key(&mut out, attr);
                out.extend_from_slice(&(cells.len() as u32).to_le_bytes());
                for (k, value) in cells {
                    out.extend_from_slice(&k.to_le_bytes());
                    put_value(&mut out, value);
                }
            }
            RowAppend { node, set, rows } => {
                out.extend_from_slice(&node.to_le_bytes());
                put_key(&mut out, set);
                out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
                for row in rows {
                    out.extend_from_slice(&row.k_skip.to_le_bytes());
                    out.extend_from_slice(&(row.cells.len() as u32).to_le_bytes());
                    for (attr, value) in &row.cells {
                        put_key(&mut out, attr);
                        put_value(&mut out, value);
                    }
                }
            }
            Action {
                node,
                transition,
                actor,
                subject,
                payload,
                from,
                to,
            } => {
                out.extend_from_slice(&node.to_le_bytes());
                put_key(&mut out, transition);
                out.push(actor_code(*actor));
                put_text(&mut out, subject);
                match payload {
                    None => out.push(0),
                    Some(v) => {
                        out.push(1);
                        put_value(&mut out, v);
                    }
                }
                put_key(&mut out, from);
                put_key(&mut out, to);
            }
            SchemaTxn { ops } => out.extend_from_slice(&lattice_schema::opslog::encode_ops(ops)),
            Grant { grant } => {
                out.extend_from_slice(&lattice_schema::opslog::encode_ops(&[SchemaOp::GrantOp(
                    grant.clone(),
                )]))
            }
            Revoke {
                role,
                subject,
                target,
            } => out.extend_from_slice(&lattice_schema::opslog::encode_ops(&[SchemaOp::Revoke {
                role: role.clone(),
                subject: subject.clone(),
                target: target.clone(),
            }])),
        }
        out
    }

    /// Decode a payload of the given kind; every byte must be consumed.
    pub fn decode(kind: RecordKind, bytes: &[u8]) -> Result<Self, RecordError> {
        let mut r = Reader {
            data: bytes,
            pos: 0,
        };
        let data = match kind {
            RecordKind::AddVertex => {
                let node = r.u32()?;
                let parent = r.u32()?;
                let graph = r.u16()?;
                let node_type = r.key()?;
                let authored = Author::try_from(r.u8()?)?;
                RecordData::AddVertex {
                    node,
                    parent: (parent != lattice_core::ids::NO_PARENT).then_some(parent),
                    graph,
                    node_type,
                    authored,
                }
            }
            RecordKind::DelVertex => RecordData::DelVertex { node: r.u32()? },
            RecordKind::AddEdge => {
                let edge = r.u64()?;
                let src = r.u32()?;
                let dst = r.u32()?;
                let kind = r.u8()?;
                let aux = r.u32()?;
                let guard = match r.u8()? {
                    0 => None,
                    1 => Some((r.key()?, r.key()?)),
                    other => {
                        return Err(RecordError::BadTag {
                            field: "guard",
                            tag: other,
                        });
                    }
                };
                RecordData::AddEdge {
                    edge,
                    src,
                    dst,
                    kind,
                    aux,
                    guard,
                }
            }
            RecordKind::DelEdge => RecordData::DelEdge { edge: r.u64()? },
            RecordKind::SetAttr => RecordData::SetAttr {
                node: r.u32()?,
                attr: r.key()?,
                value: r.value()?,
            },
            RecordKind::AppendCells => {
                let node = r.u32()?;
                let attr = r.key()?;
                let count = r.u32()? as usize;
                let mut cells = Vec::with_capacity(count.min(1 << 20));
                for _ in 0..count {
                    let k = r.u64()?;
                    let value = r.value()?;
                    cells.push((k, value));
                }
                RecordData::AppendCells { node, attr, cells }
            }
            RecordKind::RowAppendBatch => {
                let node = r.u32()?;
                let set = r.key()?;
                let count = r.u32()? as usize;
                let mut rows = Vec::with_capacity(count.min(1 << 16));
                for _ in 0..count {
                    let k_skip = r.u32()?;
                    let ncells = r.u32()? as usize;
                    let mut cells = Vec::with_capacity(ncells.min(1 << 16));
                    for _ in 0..ncells {
                        let attr = r.key()?;
                        let value = r.value()?;
                        cells.push((attr, value));
                    }
                    rows.push(Row { k_skip, cells });
                }
                RecordData::RowAppend { node, set, rows }
            }
            RecordKind::Action => {
                let node = r.u32()?;
                let transition = r.key()?;
                let actor = actor_from(r.u8()?)?;
                let subject = r.text()?;
                let payload = match r.u8()? {
                    0 => None,
                    1 => Some(r.value()?),
                    other => {
                        return Err(RecordError::BadTag {
                            field: "payload",
                            tag: other,
                        });
                    }
                };
                let from = r.key()?;
                let to = r.key()?;
                RecordData::Action {
                    node,
                    transition,
                    actor,
                    subject,
                    payload,
                    from,
                    to,
                }
            }
            RecordKind::SchemaTxn => RecordData::SchemaTxn {
                ops: decode_ops(&mut r)?,
            },
            RecordKind::Grant => {
                let ops = decode_ops(&mut r)?;
                match ops.as_slice() {
                    [SchemaOp::GrantOp(grant)] => RecordData::Grant {
                        grant: grant.clone(),
                    },
                    other => {
                        return Err(RecordError::BadOps {
                            reason: format!(
                                "GRANT record must hold exactly one GrantOp, holds {other:?}"
                            ),
                        });
                    }
                }
            }
            RecordKind::Revoke => {
                let ops = decode_ops(&mut r)?;
                match ops.as_slice() {
                    [
                        SchemaOp::Revoke {
                            role,
                            subject,
                            target,
                        },
                    ] => RecordData::Revoke {
                        role: role.clone(),
                        subject: subject.clone(),
                        target: target.clone(),
                    },
                    other => {
                        return Err(RecordError::BadOps {
                            reason: format!(
                                "REVOKE record must hold exactly one Revoke, holds {other:?}"
                            ),
                        });
                    }
                }
            }
            // Framed by the journal itself; never carried as record content.
            RecordKind::BeginTxn | RecordKind::Commit => {
                return Err(RecordError::BadTag {
                    field: "kind",
                    tag: kind as u8,
                });
            }
        };
        r.done()?;
        Ok(Self { data })
    }
}

impl std::convert::TryFrom<u8> for Author {
    type Error = RecordError;

    fn try_from(byte: u8) -> Result<Self, Self::Error> {
        match byte {
            0 => Ok(Self::User),
            1 => Ok(Self::Engine),
            other => Err(RecordError::BadTag {
                field: "author",
                tag: other,
            }),
        }
    }
}

fn actor_code(a: ActorClass) -> u8 {
    match a {
        ActorClass::Human => 0,
        ActorClass::Agent => 1,
        ActorClass::System => 2,
        ActorClass::Timer => 3,
        ActorClass::Guard => 4,
    }
}

fn actor_from(byte: u8) -> Result<ActorClass, RecordError> {
    Ok(match byte {
        0 => ActorClass::Human,
        1 => ActorClass::Agent,
        2 => ActorClass::System,
        3 => ActorClass::Timer,
        4 => ActorClass::Guard,
        other => {
            return Err(RecordError::BadTag {
                field: "actor",
                tag: other,
            });
        }
    })
}

fn put_key(out: &mut Vec<u8>, key: &Key) {
    out.extend_from_slice(&(key.as_str().len() as u16).to_le_bytes());
    out.extend_from_slice(key.as_str().as_bytes());
}

/// LE array from a slice whose length `take` already verified.
fn le<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(src);
    out
}

fn put_text(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
}

fn put_value(out: &mut Vec<u8>, value: &Value) {
    let mut buf = Vec::new();
    lattice_core::value_codec::encode_value(value, &mut buf);
    out.extend_from_slice(&(buf.len() as u32).to_le_bytes());
    out.extend_from_slice(&buf);
}

fn decode_ops(r: &mut Reader<'_>) -> Result<Vec<SchemaOp>, RecordError> {
    let bytes = r.take(r.data.len() - r.pos)?;
    lattice_schema::opslog::decode_ops(bytes).map_err(|e| RecordError::BadOps {
        reason: e.to_string(),
    })
}

/// Record payload decode failure — a corrupt or foreign payload. The journal
/// is the authority: the caller refuses rather than guesses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordError {
    Truncated { need: usize, have: usize },
    Trailing { bytes: usize },
    BadText { reason: String },
    BadValue { reason: String },
    BadOps { reason: String },
    BadTag { field: &'static str, tag: u8 },
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { need, have } => {
                write!(f, "record truncated: need {need}, have {have}")
            }
            Self::Trailing { bytes } => write!(f, "record has {bytes} trailing bytes"),
            Self::BadText { reason } => write!(f, "record text invalid: {reason}"),
            Self::BadValue { reason } => write!(f, "record value invalid: {reason}"),
            Self::BadOps { reason } => write!(f, "record op-log invalid: {reason}"),
            Self::BadTag { field, tag } => {
                write!(f, "record field `{field}` has unknown tag {tag:#04x}")
            }
        }
    }
}

impl std::error::Error for RecordError {}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], RecordError> {
        let need = self.pos + n;
        if need > self.data.len() {
            return Err(RecordError::Truncated {
                need,
                have: self.data.len(),
            });
        }
        let out = &self.data[self.pos..need];
        self.pos = need;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, RecordError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, RecordError> {
        Ok(u16::from_le_bytes(le(self.take(2)?)))
    }

    fn u32(&mut self) -> Result<u32, RecordError> {
        Ok(u32::from_le_bytes(le(self.take(4)?)))
    }

    fn u64(&mut self) -> Result<u64, RecordError> {
        Ok(u64::from_le_bytes(le(self.take(8)?)))
    }

    fn key(&mut self) -> Result<Key, RecordError> {
        let len = self.u16()? as usize;
        let bytes = self.take(len)?;
        let text = std::str::from_utf8(bytes).map_err(|e| RecordError::BadText {
            reason: format!("key is not utf-8: {e}"),
        })?;
        Key::parse(text).map_err(|e| RecordError::BadText {
            reason: format!("key `{text}`: {e}"),
        })
    }

    fn text(&mut self) -> Result<String, RecordError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        String::from_utf8(bytes.to_vec()).map_err(|e| RecordError::BadText {
            reason: format!("text is not utf-8: {e}"),
        })
    }

    fn value(&mut self) -> Result<Value, RecordError> {
        let len = self.u32()? as usize;
        let bytes = self.take(len)?;
        let (value, used) =
            lattice_core::value_codec::decode_value(bytes).map_err(|e| RecordError::BadValue {
                reason: e.to_string(),
            })?;
        if used != bytes.len() {
            return Err(RecordError::BadValue {
                reason: format!("value used {used} of {} bytes", bytes.len()),
            });
        }
        Ok(value)
    }

    fn done(&self) -> Result<(), RecordError> {
        let left = self.data.len() - self.pos;
        if left == 0 {
            Ok(())
        } else {
            Err(RecordError::Trailing { bytes: left })
        }
    }
}

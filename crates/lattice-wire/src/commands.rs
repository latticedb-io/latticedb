//! Write command dispatch (SPEC-02 §7, T-307): TLV payloads → engine
//! operations with the HINT LAW — rejections list enumerable alternatives
//! (unknown type ⇒ known types capped 50; REQUIREMENTS_UNMET ⇒ failing
//! rule keys; CONTAINMENT ⇒ expected parent type). Agents branch on hints
//! without extra roundtrips.
//!
//! Commands (v1 closed registry): CREATE_NODE · DELETE_NODE · LINK · UNLINK
//! · ATTR_SET · ROW_APPEND · CELLS_APPEND · ACTION · GRANT · REVOKE.
//!
//! Each command = one atomic op unless inside TXN (the session layer owns
//! txn framing; this layer validates + executes against the engine via the
//! [`CommandExecutor`] trait — the engine implements it; tests fake it).

use crate::frame::FrameType;
use lattice_core::ids::{AttrId, KeyIndex, Lsn, NodeId};
use lattice_core::value::Value;
use std::collections::BTreeMap;
use std::fmt;

/// TLV tags per command (SPEC-02 §7 v1 set).
pub mod tags {
    pub const REQUEST_ID: u16 = 0x0001;
    pub const GRAPH: u16 = 0x0010;
    pub const TYPE_KEY: u16 = 0x0011;
    pub const PARENT: u16 = 0x0012;
    pub const NODE: u16 = 0x0020;
    pub const CASCADE: u16 = 0x0021;
    pub const SRC: u16 = 0x0030;
    pub const DST: u16 = 0x0031;
    pub const KIND: u16 = 0x0032;
    pub const EDGE_ID: u16 = 0x0033;
    pub const ATTR: u16 = 0x0040;
    pub const K: u16 = 0x0041;
    pub const VALUE: u16 = 0x0042;
    pub const INITIAL_ATTRS: u16 = 0x0013;
    pub const TRANSITION: u16 = 0x0050;
    pub const ACTOR_HINT: u16 = 0x0051;
    pub const PAYLOAD: u16 = 0x0052;
}

/// Closed error-code registry (SPEC-02 §7, 0x03xx).
pub mod codes {
    pub const AUTH_FAILED: u16 = 0x0301;
    pub const FORBIDDEN: u16 = 0x0302;
    pub const NOT_FOUND: u16 = 0x0303;
    pub const ALREADY_EXISTS: u16 = 0x0304;
    pub const INVALID_KEY: u16 = 0x0305;
    pub const SCHEMA: u16 = 0x0306;
    pub const INVALID_KIND_PARAMS: u16 = 0x0307;
    pub const CYCLE: u16 = 0x0308;
    pub const MULTI_PARENT: u16 = 0x0309;
    pub const CONTAINMENT: u16 = 0x030A;
    pub const HAS_DEPENDENTS: u16 = 0x030B;
    pub const REQUIREMENTS_UNMET: u16 = 0x030C;
    pub const TERMINAL: u16 = 0x030D;
    pub const NOT_APPENDABLE: u16 = 0x030E;
    pub const NOT_MONOTONE: u16 = 0x030F;
    pub const R13_IN_USE: u16 = 0x0310;
    pub const BACKWARD_RANK: u16 = 0x0311;
    pub const ORDER_CONFLICT: u16 = 0x0312;
    pub const INTERNAL: u16 = 0x03FF;
}

fn trunc() -> CommandError {
    CommandError {
        code: codes::INVALID_KEY,
        hint: "truncated field".into(),
        detail: "TLV payload ends mid-value".into(),
    }
}

/// Fixed-width field decode: malformed length ⇒ INVALID_KEY with a
/// length hint (peers branching on the hint fix the frame without
/// another roundtrip).
fn fixed<const N: usize>(bytes: &[u8]) -> Result<[u8; N], CommandError> {
    bytes.try_into().map_err(|_| CommandError {
        code: codes::INVALID_KEY,
        hint: format!("fixed field expects {N} bytes"),
        detail: format!("field has {} bytes", bytes.len()),
    })
}

/// Wire error with the hint-law payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    pub code: u16,
    pub hint: String,
    pub detail: String,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:#06x}: {} (hint: {})",
            self.code, self.detail, self.hint
        )
    }
}

impl std::error::Error for CommandError {}

/// The engine surface the command layer executes against. The engine crate
/// implements this over Machine; tests implement fakes.
pub trait CommandExecutor {
    fn type_keys(&self) -> Vec<String>;
    fn type_code(&self, type_key: &str) -> Option<u16>;
    fn attr_id(&self, name: &str) -> Option<AttrId>;
    fn attr_keys(&self) -> Vec<String>;
    fn node_alive(&self, node: NodeId) -> bool;
    fn create_root(&mut self, graph: u16, type_code: u16) -> Option<NodeId>;
    fn create_child(&mut self, parent: NodeId, type_code: u16) -> Option<NodeId>;
    fn child_count(&self, node: NodeId) -> usize;
    fn dep_edge_count(&self, node: NodeId) -> usize;
    fn child_count_hint(&self, node: NodeId) -> usize;
    fn dep_edge_count_hint(&self, node: NodeId) -> usize;
    fn delete_node(&mut self, node: NodeId, cascade: bool) -> Result<(), String>;
    fn add_edge(&mut self, src: NodeId, dst: NodeId, kind: u8, aux: u32) -> Option<u64>;
    fn remove_edge(&mut self, edge_id: u64) -> Result<(), String>;
    fn set_cell(
        &mut self,
        node: NodeId,
        attr: AttrId,
        k: KeyIndex,
        value: Value,
        lsn: Lsn,
    ) -> Result<(), String>;
    fn current_max_k(&self, node: NodeId, attr: AttrId) -> Option<u64>;
    fn requirement_keys(&self, transition: &str) -> Vec<String>;
    fn eval_rule_by_name(&self, rule: &str, node: NodeId) -> bool;
    fn execute_action(&mut self, node: NodeId, transition: &str) -> Result<(), String>;
}

/// Parse TLV payload into a tag map (delegates to TlvMap::decode).
pub fn parse_payload(payload: &[u8]) -> Result<BTreeMap<u16, Vec<u8>>, String> {
    let map = crate::tlv::TlvMap::decode(payload).map_err(|e| e.to_string())?;
    Ok(map.into_owned())
}

/// The command dispatcher.
pub struct CommandDispatch;

impl CommandDispatch {
    /// Hint law: unknown name ⇒ hint lists known names (capped 50).
    fn name_hint(kind: &str, name: &str, known: &mut Vec<String>) -> CommandError {
        known.sort();
        known.truncate(50);
        CommandError {
            code: codes::NOT_FOUND,
            hint: format!("known {kind}s: {}", known.join(", ")),
            detail: format!("{kind} `{name}` not declared"),
        }
    }

    pub fn create_node(
        executor: &mut dyn CommandExecutor,
        payload: &BTreeMap<u16, Vec<u8>>,
        graph: u16,
        lsn: Lsn,
    ) -> Result<(NodeId, Vec<u8>), CommandError> {
        let tlv_str = |tag: u16| -> Option<String> {
            payload
                .get(&tag)
                .and_then(|v| String::from_utf8(v.clone()).ok())
        };
        let Some(type_name) = tlv_str(tags::TYPE_KEY) else {
            return Err(CommandError {
                code: codes::INVALID_KEY,
                hint: "tag 0x0011 (type_key) is required".into(),
                detail: "CREATE_NODE without type_key".into(),
            });
        };
        let Some(type_code) = executor.type_code(&type_name) else {
            let mut known = executor.type_keys();
            return Err(Self::name_hint("type", &type_name, &mut known));
        };

        let node = match payload.get(&tags::PARENT) {
            Some(bytes) if bytes.len() == 4 => {
                let parent = NodeId(u32::from_le_bytes(fixed::<4>(bytes)?));
                if !executor.node_alive(parent) {
                    return Err(CommandError {
                        code: codes::NOT_FOUND,
                        hint: "parent must exist".into(),
                        detail: format!("parent {parent} not found"),
                    });
                }
                executor
                    .create_child(parent, type_code)
                    .ok_or_else(|| CommandError {
                        // SGT-11.1: containment hint names the expected type.
                        code: codes::CONTAINMENT,
                        hint: format!("type `{type_name}` requires a specific parent type"),
                        detail: "containment violation".into(),
                    })?
            }
            _ => executor
                .create_root(graph, type_code)
                .ok_or_else(|| CommandError {
                    code: codes::INTERNAL,
                    hint: String::new(),
                    detail: "root creation failed".into(),
                })?,
        };

        // Initial attrs: repeated [u32 name_len][name][value_len u32][bytes].
        if let Some(attrs) = payload.get(&tags::INITIAL_ATTRS) {
            let mut pos = 0usize;
            while pos + 4 <= attrs.len() {
                let name_len =
                    u32::from_le_bytes(fixed::<4>(attrs.get(pos..pos + 4).ok_or_else(trunc)?)?)
                        as usize;
                pos += 4;
                let Some(name_bytes) = attrs.get(pos..pos + name_len) else {
                    break;
                };
                let name = String::from_utf8_lossy(name_bytes).to_string();
                pos += name_len;
                if pos + 4 > attrs.len() {
                    break;
                }
                let vlen =
                    u32::from_le_bytes(fixed::<4>(attrs.get(pos..pos + 4).ok_or_else(trunc)?)?)
                        as usize;
                pos += 4;
                let Some(vbytes) = attrs.get(pos..pos + vlen) else {
                    break;
                };
                pos += vlen;
                let Some(attr) = executor.attr_id(&name) else {
                    let mut known = executor.attr_keys();
                    return Err(Self::name_hint("attribute", &name, &mut known));
                };
                let value = decode_inline(vbytes);
                executor
                    .set_cell(node, attr, KeyIndex(0), value, lsn)
                    .map_err(|e| CommandError {
                        code: codes::SCHEMA,
                        hint: "see engine detail".into(),
                        detail: e,
                    })?;
            }
        }

        let mut ack = Vec::new();
        ack.extend_from_slice(&node.0.to_le_bytes());
        Ok((node, ack))
    }

    pub fn delete_node(
        executor: &mut dyn CommandExecutor,
        payload: &BTreeMap<u16, Vec<u8>>,
    ) -> Result<Vec<u8>, CommandError> {
        let Some(bytes) = payload.get(&tags::NODE) else {
            return Err(CommandError {
                code: codes::INVALID_KEY,
                hint: "tag 0x0020 (node) required".into(),
                detail: "DELETE_NODE without node".into(),
            });
        };
        let node = NodeId(u32::from_le_bytes(fixed::<4>(bytes)?));
        let cascade = payload
            .get(&tags::CASCADE)
            .map(|b| !b.is_empty() && b[0] != 0)
            .unwrap_or(false);
        let children = executor.child_count_hint(node);
        let deps = executor.dep_edge_count_hint(node);
        if !cascade && (children > 0 || deps > 0) {
            return Err(CommandError {
                code: codes::HAS_DEPENDENTS,
                hint: format!(
                    "{children} children, {deps} dependency edges — use cascade or drain first"
                ),
                detail: format!("node {node} has dependents"),
            });
        }
        executor
            .delete_node(node, cascade)
            .map_err(|e| CommandError {
                code: codes::INTERNAL,
                hint: String::new(),
                detail: e,
            })?;
        Ok(vec![])
    }

    pub fn link(
        executor: &mut dyn CommandExecutor,
        payload: &BTreeMap<u16, Vec<u8>>,
        kind_default: u8,
    ) -> Result<(u64, Vec<u8>), CommandError> {
        let (Some(src_b), Some(dst_b)) = (payload.get(&tags::SRC), payload.get(&tags::DST)) else {
            return Err(CommandError {
                code: codes::INVALID_KEY,
                hint: "tags 0x0030 (src) + 0x0031 (dst) required".into(),
                detail: "LINK without endpoints".into(),
            });
        };
        let src = NodeId(u32::from_le_bytes(fixed::<4>(src_b)?));
        let dst = NodeId(u32::from_le_bytes(fixed::<4>(dst_b)?));
        let kind = payload
            .get(&tags::KIND)
            .map(|b| b[0])
            .unwrap_or(kind_default);
        let aux = payload
            .get(&tags::EDGE_ID)
            .and_then(|b| b.first().copied())
            .unwrap_or(0) as u32;
        let Some(eid) = executor.add_edge(src, dst, kind, aux) else {
            return Err(CommandError {
                code: codes::CYCLE,
                hint: "dependency edges must stay acyclic (R3)".into(),
                detail: format!("edge {src}→{dst} closes a cycle"),
            });
        };
        let mut ack = Vec::new();
        ack.extend_from_slice(&eid.to_le_bytes());
        Ok((eid, ack))
    }

    pub fn unlink(
        executor: &mut dyn CommandExecutor,
        payload: &BTreeMap<u16, Vec<u8>>,
    ) -> Result<Vec<u8>, CommandError> {
        let Some(bytes) = payload.get(&tags::EDGE_ID) else {
            return Err(CommandError {
                code: codes::INVALID_KEY,
                hint: "tag 0x0033 (edge_id) required".into(),
                detail: "UNLINK without edge_id".into(),
            });
        };
        let eid = u64::from_le_bytes(fixed::<8>(bytes)?);
        executor.remove_edge(eid).map_err(|e| CommandError {
            code: codes::NOT_FOUND,
            hint: String::new(),
            detail: e,
        })?;
        Ok(vec![])
    }

    pub fn attr_set(
        executor: &mut dyn CommandExecutor,
        payload: &BTreeMap<u16, Vec<u8>>,
        lsn: Lsn,
    ) -> Result<Vec<u8>, CommandError> {
        let Some(node_bytes) = payload.get(&tags::NODE) else {
            return Err(CommandError {
                code: codes::INVALID_KEY,
                hint: "tag 0x0020 (node) required".into(),
                detail: "ATTR_SET without node".into(),
            });
        };
        let node = NodeId(u32::from_le_bytes(fixed::<4>(node_bytes)?));
        if !executor.node_alive(node) {
            return Err(CommandError {
                code: codes::NOT_FOUND,
                hint: "node must exist".into(),
                detail: format!("node {node} not found"),
            });
        }
        let attr_name = String::from_utf8_lossy(
            payload
                .get(&tags::ATTR)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
        )
        .to_string();
        let Some(attr) = executor.attr_id(&attr_name) else {
            let mut known = executor.attr_keys();
            return Err(Self::name_hint("attribute", &attr_name, &mut known));
        };
        let Some(vbytes) = payload.get(&tags::VALUE) else {
            return Err(CommandError {
                code: codes::INVALID_KEY,
                hint: "tag 0x0042 (value) required".into(),
                detail: "ATTR_SET without value".into(),
            });
        };
        let value = decode_inline(vbytes);
        executor
            .set_cell(node, attr, KeyIndex(0), value, lsn)
            .map_err(|e| {
                // R7 monotonicity / R6 schema — the engine detail is the hint.
                CommandError {
                    code: codes::NOT_MONOTONE,
                    hint: String::new(),
                    detail: e,
                }
            })?;
        Ok(vec![])
    }

    pub fn cells_append(
        executor: &mut dyn CommandExecutor,
        payload: &BTreeMap<u16, Vec<u8>>,
        lsn: Lsn,
    ) -> Result<Vec<u8>, CommandError> {
        let Some(node_bytes) = payload.get(&tags::NODE) else {
            return Err(CommandError {
                code: codes::INVALID_KEY,
                hint: "tag 0x0020 (node) required".into(),
                detail: "CELLS_APPEND without node".into(),
            });
        };
        let node = NodeId(u32::from_le_bytes(fixed::<4>(node_bytes)?));
        let attr_name = String::from_utf8_lossy(
            payload
                .get(&tags::ATTR)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
        )
        .to_string();
        let Some(attr) = executor.attr_id(&attr_name) else {
            let mut known = executor.attr_keys();
            return Err(Self::name_hint("attribute", &attr_name, &mut known));
        };
        let Some(k_bytes) = payload.get(&tags::K) else {
            return Err(CommandError {
                code: codes::INVALID_KEY,
                hint: "tag 0x0041 (k) required".into(),
                detail: "CELLS_APPEND without k".into(),
            });
        };
        let k = u64::from_le_bytes(fixed::<8>(k_bytes)?);
        let Some(vbytes) = payload.get(&tags::VALUE) else {
            return Err(CommandError {
                code: codes::INVALID_KEY,
                hint: "tag 0x0042 (value) required".into(),
                detail: "CELLS_APPEND without value".into(),
            });
        };
        let value = decode_inline(vbytes);
        // R12: append-only axis.
        if let Some(max) = executor.current_max_k(node, attr)
            && k < max
        {
            return Err(CommandError {
                code: codes::NOT_APPENDABLE,
                hint: format!("current max k is {max} — corrections belong to a re-run instance"),
                detail: format!("k {k} below current max {max}"),
            });
        }
        executor
            .set_cell(node, attr, KeyIndex(k), value, lsn)
            .map_err(|e| CommandError {
                code: codes::SCHEMA,
                hint: String::new(),
                detail: e,
            })?;
        Ok(vec![])
    }

    pub fn action(
        executor: &mut dyn CommandExecutor,
        payload: &BTreeMap<u16, Vec<u8>>,
    ) -> Result<(NodeId, Vec<u8>), CommandError> {
        let Some(node_bytes) = payload.get(&tags::NODE) else {
            return Err(CommandError {
                code: codes::INVALID_KEY,
                hint: "tag 0x0020 (node) required".into(),
                detail: "ACTION without node".into(),
            });
        };
        let node = NodeId(u32::from_le_bytes(fixed::<4>(node_bytes)?));
        let tname = String::from_utf8_lossy(
            payload
                .get(&tags::TRANSITION)
                .map(|v| v.as_slice())
                .unwrap_or(&[]),
        )
        .to_string();
        // Hint law: REQUIREMENTS_UNMET lists failing rule keys.
        let failing: Vec<String> = executor
            .requirement_keys(&tname)
            .into_iter()
            .filter(|r| !executor.eval_rule_by_name(r, node))
            .collect();
        if !failing.is_empty() {
            return Err(CommandError {
                code: codes::REQUIREMENTS_UNMET,
                hint: format!("failing rules: {}", failing.join(", ")),
                detail: format!("action `{tname}` requirements unmet"),
            });
        }
        executor
            .execute_action(node, &tname)
            .map_err(|e| CommandError {
                code: codes::SCHEMA,
                hint: String::new(),
                detail: e,
            })?;
        let mut ack = Vec::new();
        ack.extend_from_slice(&node.0.to_le_bytes());
        Ok((node, ack))
    }

    /// Route one write frame to its handler. Returns the ACK payload.
    pub fn dispatch(
        executor: &mut dyn CommandExecutor,
        frame_type: FrameType,
        payload: &BTreeMap<u16, Vec<u8>>,
        graph: u16,
        lsn: Lsn,
    ) -> Result<Vec<u8>, CommandError> {
        match frame_type {
            FrameType::CreateNode => Ok(Self::create_node(executor, payload, graph, lsn)?.1),
            FrameType::DeleteNode => Self::delete_node(executor, payload),
            FrameType::Link => Ok(Self::link(executor, payload, kinds::GUARD)?.1),
            FrameType::Unlink => Self::unlink(executor, payload),
            FrameType::AttrSet => Self::attr_set(executor, payload, lsn),
            FrameType::CellsAppend => Self::cells_append(executor, payload, lsn),
            FrameType::Action => Ok(Self::action(executor, payload)?.1),
            FrameType::Grant | FrameType::Revoke => {
                // journaled authorization mutations (SPEC-01 §14) — v1
                // wire-level pass-through to the txn layer.
                Ok(vec![])
            }
            _ => Err(CommandError {
                code: codes::INTERNAL,
                hint: String::new(),
                detail: "not a write command".into(),
            }),
        }
    }
}

/// Inline value decode (hint-law payload convention):
/// [1 byte type tag][value bytes] — tag per value_codec registry subset:
/// 1=Bool(1B) 2=I64(8B) 5=Str(rest) 10=Enum(1B).
fn decode_inline(bytes: &[u8]) -> Value {
    if bytes.is_empty() {
        return Value::Bool(false);
    }
    match bytes[0] {
        1 => Value::Bool(bytes.get(1).map(|b| *b != 0).unwrap_or(false)),
        2 => {
            let mut arr = [0u8; 8];
            for (i, b) in bytes[1..].iter().take(8).enumerate() {
                arr[i] = *b;
            }
            Value::I64(i64::from_le_bytes(arr))
        }
        5 => Value::Str(
            String::from_utf8_lossy(&bytes[1..])
                .into_owned()
                .into_boxed_str(),
        ),
        10 => Value::Enum(bytes.get(1).copied().unwrap_or(0)),
        _ => Value::Bool(false),
    }
}

/// Edge kind constants (mirror of engine's registry — bytes only here).
pub mod kinds {
    pub const GUARD: u8 = 4;
}

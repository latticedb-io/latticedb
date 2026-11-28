//! Deterministic binary encoding of schema transactions (SPEC-01 §16 ops).
//!
//! This is the payload format of journal `SCHEMA_TXN` records and the
//! snapshot's catalog blob ("op-log equivalent, deterministic order" —
//! SPEC-03 §3): the catalog is fully reproduced by replaying the ops in
//! order through [`SchemaState::commit`], so the snapshot never stores
//! catalog entities directly.
//!
//! Encoding laws (SPEC-03 D-invariants):
//! - closed op registry, fixed u16 tags, fixed field order per op;
//! - little-endian fixed-width integers, LEB128 only where variable-length
//!   framing demands it (none here — lengths are u32);
//! - keys and strings: `[u32 len][utf8 bytes]`;
//! - no optional garbage: absent fields are a single 0x00 flag byte.

use crate::catalog::Catalog;
use crate::entities::*;
use crate::key::{Key, validate_display_name};
use crate::txn::{Deprecations, SchemaOp, SchemaState};
use lattice_core::value::{RecordValue, Value};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpsError {
    Truncated {
        need: usize,
        have: usize,
    },
    UnknownOpTag {
        tag: u16,
    },
    UnknownValueTag {
        tag: u8,
    },
    UnknownScalarTag {
        tag: u8,
    },
    InvalidKey(String),
    InvalidUtf8,
    /// Decoded ops failed structural validation (entity constructors).
    InvalidEntity(String),
}

impl fmt::Display for OpsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpsError::Truncated { need, have } => {
                write!(f, "op-log truncated: need {need}, have {have}")
            }
            OpsError::UnknownOpTag { tag } => write!(f, "unknown op tag {tag}"),
            OpsError::UnknownValueTag { tag } => write!(f, "unknown value tag {tag}"),
            OpsError::UnknownScalarTag { tag } => write!(f, "unknown scalar tag {tag}"),
            OpsError::InvalidKey(s) => write!(f, "invalid key in op-log: {s}"),
            OpsError::InvalidUtf8 => write!(f, "invalid utf8 in op-log"),
            OpsError::InvalidEntity(s) => write!(f, "decoded entity failed validation: {s}"),
        }
    }
}

/// LE array from a slice of statically verified length (callers bound-check
/// before slicing; `copy_from_slice` panics loudly on any mismatch — an
/// explicit invariant, never a silent default).
fn le_bytes<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(src);
    out
}

impl std::error::Error for OpsError {}

// -- writer -----------------------------------------------------------------

struct W {
    buf: Vec<u8>,
}

impl W {
    fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn i128(&mut self, v: i128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }
    fn str(&mut self, s: &str) {
        self.u32(s.len() as u32);
        self.buf.extend_from_slice(s.as_bytes());
    }
    fn key(&mut self, k: &Key) {
        self.str(k.as_str());
    }
    fn opt_key(&mut self, k: &Option<Key>) {
        match k {
            Some(k) => {
                self.bool(true);
                self.key(k);
            }
            None => self.bool(false),
        }
    }
    fn scalar(&mut self, s: &ScalarType) {
        let tag: u8 = match s {
            ScalarType::Bool => 1,
            ScalarType::I64 => 2,
            ScalarType::F64 => 3,
            ScalarType::Decimal { .. } => 4,
            ScalarType::String => 5,
            ScalarType::Bytes => 6,
            ScalarType::Timestamp => 7,
            ScalarType::Duration => 8,
            ScalarType::NodeRef => 9,
        };
        self.u8(tag);
        if let ScalarType::Decimal { precision, scale } = s {
            self.u8(*precision);
            self.u8(*scale);
        }
    }
    fn elem(&mut self, e: &ElemType) {
        match e {
            ElemType::Scalar(s) => {
                self.u8(1);
                self.scalar(s);
            }
            ElemType::Enum(k) => {
                self.u8(2);
                self.key(k);
            }
        }
    }
    fn value(&mut self, v: &Value) {
        match v {
            Value::Bool(b) => {
                self.u8(1);
                self.bool(*b);
            }
            Value::I64(x) => {
                self.u8(2);
                self.i64(*x);
            }
            Value::F64(x) => {
                self.u8(3);
                self.u64(x.to_bits());
            }
            Value::Decimal(d) => {
                self.u8(4);
                self.i128(d.unscaled);
                self.u8(d.precision);
                self.u8(d.scale);
            }
            Value::Str(s) => {
                self.u8(5);
                self.str(s);
            }
            Value::Bytes(b) => {
                self.u8(6);
                self.u32(b.len() as u32);
                self.buf.extend_from_slice(b);
            }
            Value::Timestamp(x) => {
                self.u8(7);
                self.i64(*x);
            }
            Value::Duration(x) => {
                self.u8(8);
                self.i64(*x);
            }
            Value::NodeRef(n) => {
                self.u8(9);
                self.u32(n.0);
            }
            Value::Enum(x) => {
                self.u8(10);
                self.u8(*x);
            }
            Value::Record(rec) => {
                self.u8(11);
                self.u64(rec.presence_mask());
                self.u32(rec.field_count() as u32);
                for i in 0..rec.field_count() {
                    match rec.get(i) {
                        Some(v) => self.value(v),
                        None => self.value(&Value::Bool(false)), // masked absent
                    }
                }
            }
        }
    }
    fn attr_type(&mut self, t: &AttrType) {
        match t {
            AttrType::Scalar(s) => {
                self.u8(1);
                self.scalar(s);
            }
            AttrType::Enum(k) => {
                self.u8(2);
                self.key(k);
            }
            AttrType::Record(k) => {
                self.u8(3);
                self.key(k);
            }
            AttrType::Vector { elem } => {
                self.u8(4);
                self.elem(elem);
            }
        }
    }
    fn state_model(&mut self, m: &StateModel) {
        self.key(&m.key);
        self.bool(m.class == ModelClass::Endless);
        self.u32(m.states.len() as u32);
        for s in &m.states {
            self.key(&s.key);
            self.str(&s.name);
            self.bool(s.success_terminal);
            self.bool(s.failure_terminal);
        }
        self.key(&m.initial);
    }
}

/// Encode a batch of schema ops, in order, deterministically.
pub fn encode_ops(ops: &[SchemaOp]) -> Vec<u8> {
    let mut w = W { buf: Vec::new() };
    w.u32(ops.len() as u32);
    for op in ops {
        encode_one(&mut w, op);
    }
    w.buf
}

fn encode_one(w: &mut W, op: &SchemaOp) {
    use SchemaOp::*;
    match op {
        CreateGraph(g) => {
            w.u16(1);
            w.key(&g.key);
            w.str(&g.display_name);
            w.i64(g.created_at_us);
        }
        DropGraph { key } => {
            w.u16(2);
            w.key(key);
        }
        CreateEnum(e) => {
            w.u16(3);
            w.key(&e.key);
            w.u32(e.values.len() as u32);
            for v in &e.values {
                w.key(v);
            }
        }
        AddEnumValue { enum_key, value } => {
            w.u16(4);
            w.key(enum_key);
            w.key(value);
        }
        DeprecateEnumValue { enum_key, value } => {
            w.u16(5);
            w.key(enum_key);
            w.key(value);
        }
        RemoveEnumValue { enum_key, value } => {
            w.u16(6);
            w.key(enum_key);
            w.key(value);
        }
        CreateRecord(r) => {
            w.u16(7);
            w.key(&r.key);
            w.u32(r.fields.len() as u32);
            for (name, elem) in &r.fields {
                w.key(name);
                w.elem(elem);
            }
        }
        AddRecordField {
            record_key,
            name,
            elem,
        } => {
            w.u16(8);
            w.key(record_key);
            w.key(name);
            w.elem(elem);
        }
        RemoveRecordField { record_key, field } => {
            w.u16(9);
            w.key(record_key);
            w.key(field);
        }
        CreateKeyType(k) => {
            w.u16(10);
            w.key(&k.key);
            w.bool(k.density == Density::Sparse);
            w.bool(k.axis == AxisKind::Temporal);
        }
        CreateAttribute(a) => {
            w.u16(11);
            w.key(&a.key);
            w.attr_type(&a.attr_type);
            w.bool(a.required);
            match &a.default {
                Some(v) => {
                    w.bool(true);
                    w.value(v);
                }
                None => w.bool(false),
            }
            match &a.codec_override {
                Some(c) => {
                    w.bool(true);
                    w.str(c);
                }
                None => w.bool(false),
            }
            let mono: u8 = match a.monotonicity {
                Monotonicity::None => 0,
                Monotonicity::Increasing => 1,
                Monotonicity::Decreasing => 2,
            };
            w.u8(mono);
            match &a.write_scope {
                Some(s) => {
                    w.bool(true);
                    w.str(s);
                }
                None => w.bool(false),
            }
        }
        AlterAttributeCodec { attr_key, codec } => {
            w.u16(12);
            w.key(attr_key);
            w.str(codec);
        }
        CreateAttributeSet(s) => {
            w.u16(13);
            w.key(&s.key);
            w.bool(s.kind == SetKind::TableSet);
            w.u32(s.members.len() as u32);
            for m in &s.members {
                w.key(m);
            }
            match &s.write_scope {
                Some(x) => {
                    w.bool(true);
                    w.str(x);
                }
                None => w.bool(false),
            }
        }
        CreateStateModel(m) => {
            w.u16(14);
            w.state_model(m);
        }
        AddState { model_key, state } => {
            w.u16(15);
            w.key(model_key);
            w.key(&state.key);
            w.str(&state.name);
            w.bool(state.success_terminal);
            w.bool(state.failure_terminal);
        }
        AddTransition(t) => {
            w.u16(16);
            encode_transition(w, t);
        }
        AlterTransitionTiming {
            transition_key,
            timing,
        } => {
            w.u16(17);
            w.key(transition_key);
            encode_timing(w, timing);
        }
        DeprecateState {
            model_key,
            state_key,
        } => {
            w.u16(18);
            w.key(model_key);
            w.key(state_key);
        }
        RemoveState {
            model_key,
            state_key,
        } => {
            w.u16(19);
            w.key(model_key);
            w.key(state_key);
        }
        CreateNodeType(n) => {
            w.u16(20);
            w.key(&n.key);
            w.str(&n.display_name);
            w.opt_key(&n.parent_type);
            w.key(&n.state_model);
            w.u32(n.attribute_sets.len() as u32);
            for s in &n.attribute_sets {
                w.key(s);
            }
            w.key(&n.key_type);
            w.u32(n.state_locks.len() as u32);
            for l in &n.state_locks {
                w.key(&l.state);
                w.u32(l.writable_sets.len() as u32);
                for s in &l.writable_sets {
                    w.key(s);
                }
            }
        }
        DeprecateNodeType { type_key } => {
            w.u16(21);
            w.key(type_key);
        }
        CreateIndex(i) => {
            w.u16(22);
            w.key(&i.key);
            match &i.kind {
                IndexKind::ByState { type_key } => {
                    w.u8(1);
                    w.key(type_key);
                }
                IndexKind::AttrPresence { attr } => {
                    w.u8(2);
                    w.key(attr);
                }
                IndexKind::SetPresence { set_key } => {
                    w.u8(3);
                    w.key(set_key);
                }
            }
        }
        DropIndex { index_key } => {
            w.u16(23);
            w.key(index_key);
        }
        CreateRule(r) => {
            w.u16(24);
            w.key(&r.key);
            encode_predicate(w, &r.form);
        }
        RemoveRule { rule_key } => {
            w.u16(25);
            w.key(rule_key);
        }
        CreateRole(r) => {
            w.u16(26);
            w.key(&r.key);
            w.u32(r.scopes.len() as u32);
            for s in &r.scopes {
                let tag: u8 = match s {
                    Scope::ReadSubtree => 1,
                    Scope::WriteStructure => 2,
                    Scope::WriteAttribute => 3,
                    Scope::InvokeAction => 4,
                    Scope::Administer => 5,
                };
                w.u8(tag);
            }
        }
        GrantOp(g) => {
            w.u16(27);
            w.key(&g.role);
            w.str(&g.subject);
            encode_grant_target(w, &g.target);
        }
        Revoke {
            role,
            subject,
            target,
        } => {
            w.u16(28);
            w.key(role);
            w.str(subject);
            encode_grant_target(w, target);
        }
    }
}

fn encode_transition(w: &mut W, t: &Transition) {
    w.key(&t.key);
    w.key(&t.model);
    w.key(&t.from);
    w.key(&t.to);
    w.u32(t.requirements.len() as u32);
    for r in &t.requirements {
        w.key(r);
    }
    w.u32(t.actors.len() as u32);
    for a in &t.actors {
        let tag: u8 = match a {
            ActorClass::Human => 1,
            ActorClass::Agent => 2,
            ActorClass::System => 3,
            ActorClass::Timer => 4,
            ActorClass::Guard => 5,
        };
        w.u8(tag);
    }
    match &t.payload_type {
        Some(k) => {
            w.bool(true);
            w.key(k);
        }
        None => w.bool(false),
    }
    encode_timing(w, &t.timing);
    w.u32(t.effects.len() as u32);
    for e in &t.effects {
        match e {
            Effect::Spawn { instance_of } => {
                w.u8(1);
                w.key(instance_of);
            }
            Effect::SelectBranch => w.u8(2),
            Effect::CancelSiblings => w.u8(3),
        }
    }
}

fn encode_timing(w: &mut W, t: &TransitionTiming) {
    match t.lag_ms {
        Some(v) => {
            w.bool(true);
            w.u64(v);
        }
        None => w.bool(false),
    }
    match t.deadline_ms {
        Some(v) => {
            w.bool(true);
            w.u64(v);
        }
        None => w.bool(false),
    }
    match &t.on_deadline {
        Some(k) => {
            w.bool(true);
            w.key(k);
        }
        None => w.bool(false),
    }
}

fn encode_grant_target(w: &mut W, t: &GrantTarget) {
    match t {
        GrantTarget::Graph(k) => {
            w.u8(1);
            w.key(k);
        }
        GrantTarget::Node(n) => {
            w.u8(2);
            w.u32(n.0);
        }
    }
}

fn encode_predicate(w: &mut W, p: &PredicateForm) {
    let (tag, attr): (u8, Option<&Key>) = match p {
        PredicateForm::AttrCmp { attr, .. } => (1, Some(attr)),
        PredicateForm::AttrPresent { attr } => (2, Some(attr)),
        PredicateForm::CellCountGte { attr, .. } => (3, Some(attr)),
        PredicateForm::CellFieldCountGte { attr, .. } => (4, Some(attr)),
        PredicateForm::LatestCmp { attr, .. } => (5, Some(attr)),
        PredicateForm::EpochGte { attr, .. } => (6, Some(attr)),
        PredicateForm::ChildrenState { .. } => (7, None),
        PredicateForm::SetMember { attr, .. } => (8, Some(attr)),
        PredicateForm::StrEq { attr, .. } => (9, Some(attr)),
        PredicateForm::StrPrefix { attr, .. } => (10, Some(attr)),
    };
    w.u8(tag);
    if let Some(k) = attr {
        w.key(k);
    }
    let op_tag = |o: &CmpOp| match o {
        CmpOp::Eq => 1u8,
        CmpOp::Ne => 2,
        CmpOp::Gt => 3,
        CmpOp::Gte => 4,
        CmpOp::Lt => 5,
        CmpOp::Lte => 6,
    };
    match p {
        PredicateForm::AttrCmp { op, value, .. } | PredicateForm::LatestCmp { op, value, .. } => {
            w.u8(op_tag(op));
            w.value(value);
        }
        PredicateForm::CellCountGte { k, .. } => w.u64(*k),
        PredicateForm::CellFieldCountGte {
            field, value, min, ..
        } => {
            w.key(field);
            w.value(value);
            w.u64(*min);
        }
        PredicateForm::EpochGte { age_us, .. } => w.i64(*age_us),
        PredicateForm::ChildrenState {
            success,
            expect_all,
        } => {
            w.bool(*success);
            w.bool(*expect_all);
        }
        PredicateForm::SetMember { values, .. } => {
            w.u32(values.len() as u32);
            for v in values {
                w.value(v);
            }
        }
        PredicateForm::StrEq { value, .. } | PredicateForm::StrPrefix { value, .. } => {
            w.str(value);
        }
        _ => {}
    }
}

// -- reader -----------------------------------------------------------------

struct R<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> R<'a> {
    fn need(&self, n: usize) -> Result<(), OpsError> {
        if self.pos + n > self.data.len() {
            return Err(OpsError::Truncated {
                need: self.pos + n,
                have: self.data.len(),
            });
        }
        Ok(())
    }
    fn u8(&mut self) -> Result<u8, OpsError> {
        self.need(1)?;
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }
    fn u16(&mut self) -> Result<u16, OpsError> {
        self.need(2)?;
        let v = u16::from_le_bytes(le_bytes(&self.data[self.pos..self.pos + 2]));
        self.pos += 2;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32, OpsError> {
        self.need(4)?;
        let v = u32::from_le_bytes(le_bytes(&self.data[self.pos..self.pos + 4]));
        self.pos += 4;
        Ok(v)
    }
    fn u64(&mut self) -> Result<u64, OpsError> {
        self.need(8)?;
        let v = u64::from_le_bytes(le_bytes(&self.data[self.pos..self.pos + 8]));
        self.pos += 8;
        Ok(v)
    }
    fn i64(&mut self) -> Result<i64, OpsError> {
        Ok(self.u64()? as i64)
    }
    fn i128(&mut self) -> Result<i128, OpsError> {
        self.need(16)?;
        let v = i128::from_le_bytes(le_bytes(&self.data[self.pos..self.pos + 16]));
        self.pos += 16;
        Ok(v)
    }
    fn bool(&mut self) -> Result<bool, OpsError> {
        Ok(self.u8()? != 0)
    }
    fn str(&mut self) -> Result<String, OpsError> {
        let len = self.u32()? as usize;
        self.need(len)?;
        let s = String::from_utf8(self.data[self.pos..self.pos + len].to_vec())
            .map_err(|_| OpsError::InvalidUtf8)?;
        self.pos += len;
        Ok(s)
    }
    fn key(&mut self) -> Result<Key, OpsError> {
        let s = self.str()?;
        Key::parse(s.clone()).map_err(|_| OpsError::InvalidKey(s))
    }
    fn opt_key(&mut self) -> Result<Option<Key>, OpsError> {
        if self.bool()? {
            Ok(Some(self.key()?))
        } else {
            Ok(None)
        }
    }
    fn opt_str(&mut self) -> Result<Option<String>, OpsError> {
        if self.bool()? {
            Ok(Some(self.str()?))
        } else {
            Ok(None)
        }
    }
    fn scalar(&mut self) -> Result<ScalarType, OpsError> {
        let tag = self.u8()?;
        Ok(match tag {
            1 => ScalarType::Bool,
            2 => ScalarType::I64,
            3 => ScalarType::F64,
            4 => {
                let precision = self.u8()?;
                let scale = self.u8()?;
                ScalarType::Decimal { precision, scale }
            }
            5 => ScalarType::String,
            6 => ScalarType::Bytes,
            7 => ScalarType::Timestamp,
            8 => ScalarType::Duration,
            9 => ScalarType::NodeRef,
            other => return Err(OpsError::UnknownScalarTag { tag: other }),
        })
    }
    fn elem(&mut self) -> Result<ElemType, OpsError> {
        match self.u8()? {
            1 => Ok(ElemType::Scalar(self.scalar()?)),
            2 => Ok(ElemType::Enum(self.key()?)),
            other => Err(OpsError::UnknownValueTag { tag: other }),
        }
    }
    fn value(&mut self) -> Result<Value, OpsError> {
        match self.u8()? {
            1 => Ok(Value::Bool(self.bool()?)),
            2 => Ok(Value::I64(self.i64()?)),
            3 => Ok(Value::F64(f64::from_bits(self.u64()?))),
            4 => {
                let unscaled = self.i128()?;
                let precision = self.u8()?;
                let scale = self.u8()?;
                let d = lattice_core::value::Decimal::new(unscaled, precision, scale)
                    .map_err(|e| OpsError::InvalidEntity(format!("decimal: {e}")))?;
                Ok(Value::Decimal(d))
            }
            5 => Ok(Value::Str(self.str()?.into_boxed_str())),
            6 => {
                let len = self.u32()? as usize;
                self.need(len)?;
                let b = self.data[self.pos..self.pos + len].to_vec();
                self.pos += len;
                Ok(Value::Bytes(b.into_boxed_slice()))
            }
            7 => Ok(Value::Timestamp(self.i64()?)),
            8 => Ok(Value::Duration(self.i64()?)),
            9 => Ok(Value::NodeRef(lattice_core::ids::NodeId(self.u32()?))),
            10 => Ok(Value::Enum(self.u8()?)),
            11 => {
                let presence = self.u64()?;
                let count = self.u32()? as usize;
                let mut rv = RecordValue::default();
                for i in 0..count {
                    let v = self.value()?;
                    if (presence >> i) & 1 == 1 {
                        rv.set(i, v)
                            .map_err(|e| OpsError::InvalidEntity(e.to_string()))?;
                    }
                }
                Ok(Value::Record(rv))
            }
            other => Err(OpsError::UnknownValueTag { tag: other }),
        }
    }
    fn attr_type(&mut self) -> Result<AttrType, OpsError> {
        match self.u8()? {
            1 => Ok(AttrType::Scalar(self.scalar()?)),
            2 => Ok(AttrType::Enum(self.key()?)),
            3 => Ok(AttrType::Record(self.key()?)),
            4 => Ok(AttrType::Vector { elem: self.elem()? }),
            other => Err(OpsError::UnknownValueTag { tag: other }),
        }
    }
    fn state_model(&mut self) -> Result<StateModel, OpsError> {
        let key = self.key()?;
        let endless = self.bool()?;
        let count = self.u32()? as usize;
        let mut states = Vec::with_capacity(count);
        for _ in 0..count {
            let skey = self.key()?;
            let name = self.str()?;
            let success = self.bool()?;
            let failure = self.bool()?;
            states.push(State {
                key: skey,
                name,
                success_terminal: success,
                failure_terminal: failure,
            });
        }
        let initial = self.key()?;
        StateModel::new(
            key,
            if endless {
                ModelClass::Endless
            } else {
                ModelClass::Finalizing
            },
            states,
            initial,
        )
        .map_err(|e| OpsError::InvalidEntity(e.to_string()))
    }
}

/// Decode a batch of ops (order preserved).
pub fn decode_ops(data: &[u8]) -> Result<Vec<SchemaOp>, OpsError> {
    let mut r = R { data, pos: 0 };
    let count = r.u32()? as usize;
    let mut ops = Vec::with_capacity(count);
    for _ in 0..count {
        ops.push(decode_one(&mut r)?);
    }
    Ok(ops)
}

fn decode_timing(r: &mut R) -> Result<TransitionTiming, OpsError> {
    let lag_ms = if r.bool()? { Some(r.u64()?) } else { None };
    let deadline_ms = if r.bool()? { Some(r.u64()?) } else { None };
    let on_deadline = r.opt_key()?;
    Ok(TransitionTiming {
        lag_ms,
        deadline_ms,
        on_deadline,
    })
}

fn decode_grant_target(r: &mut R) -> Result<GrantTarget, OpsError> {
    match r.u8()? {
        1 => Ok(GrantTarget::Graph(r.key()?)),
        2 => Ok(GrantTarget::Node(lattice_core::ids::NodeId(r.u32()?))),
        other => Err(OpsError::UnknownValueTag { tag: other }),
    }
}

fn decode_predicate(r: &mut R) -> Result<PredicateForm, OpsError> {
    let tag = r.u8()?;
    let mut attr = || r.key();
    let cmp_op = |t: u8| -> Result<CmpOp, OpsError> {
        Ok(match t {
            1 => CmpOp::Eq,
            2 => CmpOp::Ne,
            3 => CmpOp::Gt,
            4 => CmpOp::Gte,
            5 => CmpOp::Lt,
            6 => CmpOp::Lte,
            other => return Err(OpsError::UnknownValueTag { tag: other }),
        })
    };
    Ok(match tag {
        1 => {
            let a = attr()?;
            let op = cmp_op(r.u8()?)?;
            let value = r.value()?;
            PredicateForm::AttrCmp { attr: a, op, value }
        }
        2 => PredicateForm::AttrPresent { attr: attr()? },
        3 => {
            let a = attr()?;
            let k = r.u64()?;
            PredicateForm::CellCountGte { attr: a, k }
        }
        4 => {
            let a = attr()?;
            let field = r.key()?;
            let value = r.value()?;
            let min = r.u64()?;
            PredicateForm::CellFieldCountGte {
                attr: a,
                field,
                value,
                min,
            }
        }
        5 => {
            let a = attr()?;
            let op = cmp_op(r.u8()?)?;
            let value = r.value()?;
            PredicateForm::LatestCmp { attr: a, op, value }
        }
        6 => {
            let a = attr()?;
            let age_us = r.i64()?;
            PredicateForm::EpochGte { attr: a, age_us }
        }
        7 => {
            let success = r.bool()?;
            let expect_all = r.bool()?;
            PredicateForm::ChildrenState {
                success,
                expect_all,
            }
        }
        8 => {
            let a = attr()?;
            let n = r.u32()? as usize;
            let mut values = Vec::with_capacity(n);
            for _ in 0..n {
                values.push(r.value()?);
            }
            PredicateForm::SetMember { attr: a, values }
        }
        9 => {
            let a = attr()?;
            let value = r.str()?;
            PredicateForm::StrEq { attr: a, value }
        }
        10 => {
            let a = attr()?;
            let value = r.str()?;
            PredicateForm::StrPrefix { attr: a, value }
        }
        other => return Err(OpsError::UnknownValueTag { tag: other }),
    })
}

fn decode_one(r: &mut R) -> Result<SchemaOp, OpsError> {
    let tag = r.u16()?;
    Ok(match tag {
        1 => {
            let key = r.key()?;
            let display_name = r.str()?;
            validate_display_name(&display_name)
                .map_err(|e| OpsError::InvalidEntity(e.to_string()))?;
            let created_at_us = r.i64()?;
            SchemaOp::CreateGraph(GraphSchema {
                key,
                display_name,
                created_at_us,
            })
        }
        2 => SchemaOp::DropGraph { key: r.key()? },
        3 => {
            let key = r.key()?;
            let n = r.u32()? as usize;
            let mut values = Vec::with_capacity(n);
            for _ in 0..n {
                values.push(r.key()?);
            }
            SchemaOp::CreateEnum(
                EnumType::new(key, values).map_err(|e| OpsError::InvalidEntity(e.to_string()))?,
            )
        }
        4 => SchemaOp::AddEnumValue {
            enum_key: r.key()?,
            value: r.key()?,
        },
        5 => SchemaOp::DeprecateEnumValue {
            enum_key: r.key()?,
            value: r.key()?,
        },
        6 => SchemaOp::RemoveEnumValue {
            enum_key: r.key()?,
            value: r.key()?,
        },
        7 => {
            let key = r.key()?;
            let n = r.u32()? as usize;
            let mut fields = Vec::with_capacity(n);
            for _ in 0..n {
                let name = r.key()?;
                let elem = r.elem()?;
                fields.push((name, elem));
            }
            SchemaOp::CreateRecord(
                RecordType::new(key, fields).map_err(|e| OpsError::InvalidEntity(e.to_string()))?,
            )
        }
        8 => SchemaOp::AddRecordField {
            record_key: r.key()?,
            name: r.key()?,
            elem: r.elem()?,
        },
        9 => SchemaOp::RemoveRecordField {
            record_key: r.key()?,
            field: r.key()?,
        },
        10 => {
            let key = r.key()?;
            let sparse = r.bool()?;
            let temporal = r.bool()?;
            SchemaOp::CreateKeyType(KeyTypeDef {
                key,
                density: if sparse {
                    Density::Sparse
                } else {
                    Density::Dense
                },
                axis: if temporal {
                    AxisKind::Temporal
                } else {
                    AxisKind::Ordinal
                },
            })
        }
        11 => {
            let key = r.key()?;
            let attr_type = r.attr_type()?;
            let required = r.bool()?;
            let default = if r.bool()? { Some(r.value()?) } else { None };
            let codec_override = r.opt_str()?;
            let monotonicity = match r.u8()? {
                0 => Monotonicity::None,
                1 => Monotonicity::Increasing,
                2 => Monotonicity::Decreasing,
                other => return Err(OpsError::UnknownValueTag { tag: other }),
            };
            let write_scope = r.opt_str()?;
            SchemaOp::CreateAttribute(AttributeDef {
                key,
                attr_type,
                required,
                default,
                codec_override,
                monotonicity,
                write_scope,
            })
        }
        12 => SchemaOp::AlterAttributeCodec {
            attr_key: r.key()?,
            codec: r.str()?,
        },
        13 => {
            let key = r.key()?;
            let table = r.bool()?;
            let n = r.u32()? as usize;
            let mut members = Vec::with_capacity(n);
            for _ in 0..n {
                members.push(r.key()?);
            }
            let write_scope = r.opt_str()?;
            SchemaOp::CreateAttributeSet(AttributeSet {
                key,
                kind: if table {
                    SetKind::TableSet
                } else {
                    SetKind::ScalarSet
                },
                members,
                write_scope,
            })
        }
        14 => SchemaOp::CreateStateModel(r.state_model()?),
        15 => {
            let model_key = r.key()?;
            let skey = r.key()?;
            let name = r.str()?;
            let success = r.bool()?;
            let failure = r.bool()?;
            SchemaOp::AddState {
                model_key,
                state: State {
                    key: skey,
                    name,
                    success_terminal: success,
                    failure_terminal: failure,
                },
            }
        }
        16 => {
            let key = r.key()?;
            let model = r.key()?;
            let from = r.key()?;
            let to = r.key()?;
            let nreq = r.u32()? as usize;
            let mut requirements = Vec::with_capacity(nreq);
            for _ in 0..nreq {
                requirements.push(r.key()?);
            }
            let nact = r.u32()? as usize;
            let mut actors = Vec::with_capacity(nact);
            for _ in 0..nact {
                actors.push(match r.u8()? {
                    1 => ActorClass::Human,
                    2 => ActorClass::Agent,
                    3 => ActorClass::System,
                    4 => ActorClass::Timer,
                    5 => ActorClass::Guard,
                    other => return Err(OpsError::UnknownValueTag { tag: other }),
                });
            }
            let payload_type = r.opt_key()?;
            let timing = decode_timing(r)?;
            let nfx = r.u32()? as usize;
            let mut effects = Vec::with_capacity(nfx);
            for _ in 0..nfx {
                effects.push(match r.u8()? {
                    1 => Effect::Spawn {
                        instance_of: r.key()?,
                    },
                    2 => Effect::SelectBranch,
                    3 => Effect::CancelSiblings,
                    other => return Err(OpsError::UnknownValueTag { tag: other }),
                });
            }
            SchemaOp::AddTransition(
                Transition::new(
                    key,
                    model,
                    from,
                    to,
                    requirements,
                    actors,
                    payload_type,
                    timing,
                    effects,
                )
                .map_err(|e| OpsError::InvalidEntity(e.to_string()))?,
            )
        }
        17 => SchemaOp::AlterTransitionTiming {
            transition_key: r.key()?,
            timing: decode_timing(r)?,
        },
        18 => SchemaOp::DeprecateState {
            model_key: r.key()?,
            state_key: r.key()?,
        },
        19 => SchemaOp::RemoveState {
            model_key: r.key()?,
            state_key: r.key()?,
        },
        20 => {
            let key = r.key()?;
            let display_name = r.str()?;
            validate_display_name(&display_name)
                .map_err(|e| OpsError::InvalidEntity(e.to_string()))?;
            let parent_type = r.opt_key()?;
            let state_model = r.key()?;
            let nsets = r.u32()? as usize;
            let mut attribute_sets = Vec::with_capacity(nsets);
            for _ in 0..nsets {
                attribute_sets.push(r.key()?);
            }
            let key_type = r.key()?;
            let nlocks = r.u32()? as usize;
            let mut state_locks = Vec::with_capacity(nlocks);
            for _ in 0..nlocks {
                let state = r.key()?;
                let ns = r.u32()? as usize;
                let mut writable_sets = Vec::with_capacity(ns);
                for _ in 0..ns {
                    writable_sets.push(r.key()?);
                }
                state_locks.push(StateLock {
                    state,
                    writable_sets,
                });
            }
            SchemaOp::CreateNodeType(NodeType {
                key,
                display_name,
                parent_type,
                state_model,
                attribute_sets,
                key_type,
                state_locks,
            })
        }
        21 => SchemaOp::DeprecateNodeType { type_key: r.key()? },
        22 => {
            let key = r.key()?;
            let kind = match r.u8()? {
                1 => IndexKind::ByState { type_key: r.key()? },
                2 => IndexKind::AttrPresence { attr: r.key()? },
                3 => IndexKind::SetPresence { set_key: r.key()? },
                other => return Err(OpsError::UnknownValueTag { tag: other }),
            };
            SchemaOp::CreateIndex(IndexDef { key, kind })
        }
        23 => SchemaOp::DropIndex {
            index_key: r.key()?,
        },
        24 => {
            let key = r.key()?;
            let form = decode_predicate(r)?;
            SchemaOp::CreateRule(NamedRule { key, form })
        }
        25 => SchemaOp::RemoveRule { rule_key: r.key()? },
        26 => {
            let key = r.key()?;
            let n = r.u32()? as usize;
            let mut scopes = Vec::with_capacity(n);
            for _ in 0..n {
                scopes.push(match r.u8()? {
                    1 => Scope::ReadSubtree,
                    2 => Scope::WriteStructure,
                    3 => Scope::WriteAttribute,
                    4 => Scope::InvokeAction,
                    5 => Scope::Administer,
                    other => return Err(OpsError::UnknownValueTag { tag: other }),
                });
            }
            SchemaOp::CreateRole(Role { key, scopes })
        }
        27 => {
            let role = r.key()?;
            let subject = r.str()?;
            let target = decode_grant_target(r)?;
            SchemaOp::GrantOp(Grant {
                role,
                subject,
                target,
            })
        }
        28 => {
            let role = r.key()?;
            let subject = r.str()?;
            let target = decode_grant_target(r)?;
            SchemaOp::Revoke {
                role,
                subject,
                target,
            }
        }
        other => return Err(OpsError::UnknownOpTag { tag: other }),
    })
}

/// Replay decoded ops into a fresh [`SchemaState`], returning it together
/// with the per-op verdicts (catalog reproduction — SFT-3.2 recompile path).
pub fn replay_ops(ops: &[SchemaOp]) -> Result<(SchemaState, Vec<crate::txn::OpResult>), OpsError> {
    let mut state = SchemaState {
        catalog: Catalog::fresh(),
        deprecations: Default::default(),
        ops_log: Vec::new(),
    };
    // No ops = a fresh catalog. Committing an empty list would record a
    // schema txn (and bump the epoch) that never happened.
    if ops.is_empty() {
        return Ok((state, Vec::new()));
    }
    let outcomes = state.plan(ops);
    for o in &outcomes {
        if !o.accepted {
            return Err(OpsError::InvalidEntity(format!(
                "replay rejected op[{}]: {} ({})",
                o.op_index,
                o.error.as_ref().map(|e| e.to_string()).unwrap_or_default(),
                o.r13.as_ref().map(|r| r.to_string()).unwrap_or_default()
            )));
        }
    }
    // Apply through commit for the atomic epoch bookkeeping.
    let epoch = state
        .commit(ops.to_vec())
        .map_err(|e| OpsError::InvalidEntity(format!("replay commit failed: {e}")))?;
    let _ = epoch;
    Ok((state, outcomes))
}

/// Deprecations ride beside the catalog (ED-06): deterministic encoding.
pub fn encode_deprecations(d: &Deprecations) -> Vec<u8> {
    let mut w = W { buf: Vec::new() };
    w.u32(d.enum_values.len() as u32);
    for (e, v) in &d.enum_values {
        w.key(e);
        w.key(v);
    }
    w.u32(d.states.len() as u32);
    for (m, s) in &d.states {
        w.key(m);
        w.key(s);
    }
    w.u32(d.node_types.len() as u32);
    for t in &d.node_types {
        w.key(t);
    }
    w.buf
}

pub fn decode_deprecations(data: &[u8]) -> Result<Deprecations, OpsError> {
    let mut r = R { data, pos: 0 };
    let mut d = Deprecations::default();
    let n = r.u32()? as usize;
    for _ in 0..n {
        d.enum_values.insert((r.key()?, r.key()?));
    }
    let n = r.u32()? as usize;
    for _ in 0..n {
        d.states.insert((r.key()?, r.key()?));
    }
    let n = r.u32()? as usize;
    for _ in 0..n {
        d.node_types.insert(r.key()?);
    }
    Ok(d)
}

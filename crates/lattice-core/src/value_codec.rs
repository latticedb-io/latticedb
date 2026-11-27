//! Deterministic binary encoding of [`Value`] cells — the canonical cell
//! wire format reused by the journal (SET_ATTR payloads), the schema op-log
//! (defaults, rule params), and part storage (record cells).
//!
//! Tags: 1 Bool | 2 I64 | 3 F64(bits) | 4 Decimal(i128,p,s) | 5 Str |
//! 6 Bytes | 7 Timestamp | 8 Duration | 9 NodeRef | 10 Enum(u8) |
//! 11 Record(presence u64 + field values, absent fields skipped).
//!
//! Determinism (SPEC-03 D1): fixed field order, LE integers, byte-exact
//! round-trip — `decode(encode(x)) == x`.

use crate::ids::NodeId;
use crate::value::{Decimal, RecordValue, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValueError {
    Truncated,
    UnknownTag(u8),
    InvalidUtf8,
    InvalidDecimal(String),
}

impl std::fmt::Display for ValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ValueError::Truncated => write!(f, "value bytes truncated"),
            ValueError::UnknownTag(t) => write!(f, "unknown value tag {t}"),
            ValueError::InvalidUtf8 => write!(f, "invalid utf8 in string value"),
            ValueError::InvalidDecimal(s) => write!(f, "invalid decimal: {s}"),
        }
    }
}

impl std::error::Error for ValueError {}

/// LE array from a statically verified slice (`arr` guarantees the length;
/// `copy_from_slice` panics loudly on any mismatch — an explicit invariant,
/// never a silent default).
fn le<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(src);
    out
}

/// Encode one value.
pub fn encode_value(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Bool(b) => {
            out.push(1);
            out.push(u8::from(*b));
        }
        Value::I64(x) => {
            out.push(2);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::F64(x) => {
            out.push(3);
            out.extend_from_slice(&x.to_bits().to_le_bytes());
        }
        Value::Decimal(d) => {
            out.push(4);
            out.extend_from_slice(&d.unscaled.to_le_bytes());
            out.push(d.precision);
            out.push(d.scale);
        }
        Value::Str(s) => {
            out.push(5);
            out.extend_from_slice(&(s.len() as u32).to_le_bytes());
            out.extend_from_slice(s.as_bytes());
        }
        Value::Bytes(b) => {
            out.push(6);
            out.extend_from_slice(&(b.len() as u32).to_le_bytes());
            out.extend_from_slice(b);
        }
        Value::Timestamp(x) => {
            out.push(7);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::Duration(x) => {
            out.push(8);
            out.extend_from_slice(&x.to_le_bytes());
        }
        Value::NodeRef(n) => {
            out.push(9);
            out.extend_from_slice(&n.0.to_le_bytes());
        }
        Value::Enum(x) => {
            out.push(10);
            out.push(*x);
        }
        Value::Record(rec) => {
            out.push(11);
            out.extend_from_slice(&rec.presence_mask().to_le_bytes());
            out.extend_from_slice(&(rec.field_count() as u32).to_le_bytes());
            for i in 0..rec.field_count() {
                match rec.get(i) {
                    Some(v) => encode_value(v, out),
                    // Placeholder for absent fields: the presence bit says
                    // everything (decode skips setting it), but the byte is
                    // consumed — symmetric with the reader.
                    None => encode_value(&Value::Bool(false), out),
                }
            }
        }
    }
}

/// Decode one value from the front of `data`; returns the value and the
/// number of bytes consumed.
pub fn decode_value(data: &[u8]) -> Result<(Value, usize), ValueError> {
    if data.is_empty() {
        return Err(ValueError::Truncated);
    }
    let tag = data[0];
    let mut pos = 1usize;
    fn need(data: &[u8], pos: usize, n: usize) -> Result<(), ValueError> {
        if pos + n > data.len() {
            Err(ValueError::Truncated)
        } else {
            Ok(())
        }
    }
    fn arr(data: &[u8], pos: &mut usize, n: usize) -> Result<[u8; 16], ValueError> {
        need(data, *pos, n)?;
        let mut out = [0u8; 16];
        out[..n].copy_from_slice(&data[*pos..*pos + n]);
        *pos += n;
        Ok(out)
    }
    Ok(match tag {
        1 => {
            need(data, pos, 1)?;
            pos += 1;
            (Value::Bool(data[pos - 1] != 0), pos)
        }
        2 => {
            let a = arr(data, &mut pos, 8)?;
            (Value::I64(i64::from_le_bytes(le(&a[(8)..]))), pos)
        }
        3 => {
            let a = arr(data, &mut pos, 8)?;
            (
                Value::F64(f64::from_bits(u64::from_le_bytes(le(&a[(8)..])))),
                pos,
            )
        }
        4 => {
            let a = arr(data, &mut pos, 16)?;
            let unscaled = i128::from_le_bytes(a);
            pos += 16;
            need(data, pos, 2)?;
            let precision = data[pos];
            let scale = data[pos + 1];
            pos += 2;
            let d = Decimal::new(unscaled, precision, scale)
                .map_err(|e| ValueError::InvalidDecimal(e.to_string()))?;
            (Value::Decimal(d), pos)
        }
        5 => {
            need(data, pos, 4)?;
            let len = u32::from_le_bytes(le(&data[pos..pos + 4])) as usize;
            pos += 4;
            need(data, pos, len)?;
            let s = String::from_utf8(data[pos..pos + len].to_vec())
                .map_err(|_| ValueError::InvalidUtf8)?;
            pos += len;
            (Value::Str(s.into_boxed_str()), pos)
        }
        6 => {
            need(data, pos, 4)?;
            let len = u32::from_le_bytes(le(&data[pos..pos + 4])) as usize;
            pos += 4;
            need(data, pos, len)?;
            let b = data[pos..pos + len].to_vec();
            pos += len;
            (Value::Bytes(b.into_boxed_slice()), pos)
        }
        7 => {
            let a = arr(data, &mut pos, 8)?;
            (Value::Timestamp(i64::from_le_bytes(le(&a[(8)..]))), pos)
        }
        8 => {
            let a = arr(data, &mut pos, 8)?;
            (Value::Duration(i64::from_le_bytes(le(&a[(8)..]))), pos)
        }
        9 => {
            let a = arr(data, &mut pos, 4)?;
            (
                Value::NodeRef(NodeId(u32::from_le_bytes(le(&a[(4)..])))),
                pos,
            )
        }
        10 => {
            need(data, pos, 1)?;
            pos += 1;
            (Value::Enum(data[pos - 1]), pos)
        }
        11 => {
            need(data, pos, 12)?;
            let presence = u64::from_le_bytes(le(&data[pos..pos + 8]));
            let count = u32::from_le_bytes(le(&data[pos + 8..pos + 12])) as usize;
            pos += 12;
            let mut rv = RecordValue::default();
            for i in 0..count {
                let (v, used) = decode_value(&data[pos..])?;
                pos += used;
                if (presence >> i) & 1 == 1 {
                    rv.set(i, v)
                        .map_err(|e| ValueError::InvalidDecimal(e.to_string()))?;
                }
            }
            (Value::Record(rv), pos)
        }
        other => return Err(ValueError::UnknownTag(other)),
    })
}

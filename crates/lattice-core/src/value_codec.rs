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
/// number of bytes consumed (callers with more data after the value advance
/// by it).
pub fn decode_value(data: &[u8]) -> Result<(Value, usize), ValueError> {
    /// Read `n` little-endian bytes at `pos` as a fixed array, advancing.
    fn take<const N: usize>(data: &[u8], pos: &mut usize) -> Result<[u8; N], ValueError> {
        let end = *pos + N;
        if end > data.len() {
            return Err(ValueError::Truncated);
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&data[*pos..end]);
        *pos = end;
        Ok(out)
    }
    /// Read a `u32`-length byte blob at `pos`, advancing.
    fn blob<'a>(data: &'a [u8], pos: &mut usize) -> Result<&'a [u8], ValueError> {
        let len = u32::from_le_bytes(take::<4>(data, pos)?) as usize;
        let end = *pos + len;
        if end > data.len() {
            return Err(ValueError::Truncated);
        }
        let out = &data[*pos..end];
        *pos = end;
        Ok(out)
    }
    if data.is_empty() {
        return Err(ValueError::Truncated);
    }
    let mut pos = 1usize;
    let value = match data[0] {
        1 => {
            let b = take::<1>(data, &mut pos)?[0];
            Value::Bool(b != 0)
        }
        2 => Value::I64(i64::from_le_bytes(take::<8>(data, &mut pos)?)),
        3 => Value::F64(f64::from_bits(u64::from_le_bytes(take::<8>(
            data, &mut pos,
        )?))),
        4 => {
            let unscaled = i128::from_le_bytes(take::<16>(data, &mut pos)?);
            let tail = take::<2>(data, &mut pos)?;
            let d = Decimal::new(unscaled, tail[0], tail[1])
                .map_err(|e| ValueError::InvalidDecimal(e.to_string()))?;
            Value::Decimal(d)
        }
        5 => {
            let s =
                std::str::from_utf8(blob(data, &mut pos)?).map_err(|_| ValueError::InvalidUtf8)?;
            Value::Str(s.to_string().into_boxed_str())
        }
        6 => Value::Bytes(blob(data, &mut pos)?.to_vec().into_boxed_slice()),
        7 => Value::Timestamp(i64::from_le_bytes(take::<8>(data, &mut pos)?)),
        8 => Value::Duration(i64::from_le_bytes(take::<8>(data, &mut pos)?)),
        9 => Value::NodeRef(NodeId(u32::from_le_bytes(take::<4>(data, &mut pos)?))),
        10 => Value::Enum(take::<1>(data, &mut pos)?[0]),
        11 => {
            let head = take::<12>(data, &mut pos)?;
            let presence = u64::from_le_bytes(head[..8].try_into().expect("8 bytes"));
            let count = u32::from_le_bytes(head[8..].try_into().expect("4 bytes")) as usize;
            let mut rv = RecordValue::default();
            for i in 0..count {
                let (v, used) = decode_value(&data[pos..])?;
                pos += used;
                if (presence >> i) & 1 == 1 {
                    rv.set(i, v)
                        .map_err(|e| ValueError::InvalidDecimal(e.to_string()))?;
                }
            }
            Value::Record(rv)
        }
        other => return Err(ValueError::UnknownTag(other)),
    };
    Ok((value, pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(v: Value) {
        let mut buf = Vec::new();
        encode_value(&v, &mut buf);
        let (back, used) = decode_value(&buf).expect("decodes");
        assert_eq!(used, buf.len(), "one value consumes exactly its bytes");
        assert_eq!(back, v, "decode(encode(x)) == x");
    }

    /// Every fixed-width tag round-trips with its REAL value — the reader
    /// used to decode I64/F64/Timestamp/Duration as 0 (it read the padding of
    /// its scratch buffer), panic on NodeRef and skip 16 bytes past Decimal.
    #[test]
    fn fixed_width_tags_round_trip_with_their_values() {
        round_trip(Value::I64(-123_456_789_012));
        round_trip(Value::F64(3.25));
        round_trip(Value::F64(-0.0));
        round_trip(Value::Timestamp(1_700_000_000_000_123));
        round_trip(Value::Duration(-42));
        round_trip(Value::NodeRef(NodeId(0xDEAD_BEEF)));
        round_trip(Value::Bool(true));
        round_trip(Value::Enum(200));
    }

    #[test]
    fn variable_width_tags_round_trip() {
        round_trip(Value::str("hello").unwrap());
        round_trip(Value::bytes(vec![1u8, 2, 3]).unwrap());
        round_trip(Value::Decimal(Decimal::new(-305, 10, 2).expect("decimal")));
    }

    /// A value followed by more data: `used` lands exactly on the next byte.
    #[test]
    fn used_points_at_the_next_value_in_a_sequence() {
        let values = [
            Value::I64(7),
            Value::Decimal(Decimal::new(305, 10, 2).unwrap()),
            Value::NodeRef(NodeId(77)),
            Value::str("next").unwrap(),
        ];
        let mut buf = Vec::new();
        for v in &values {
            encode_value(v, &mut buf);
        }
        let mut pos = 0usize;
        for v in &values {
            let (back, used) = decode_value(&buf[pos..]).expect("decodes");
            pos += used;
            assert_eq!(&back, v);
        }
        assert_eq!(pos, buf.len());
    }

    #[test]
    fn record_with_present_and_absent_fields_round_trips() {
        let rec = RecordValue::from_pairs(&[
            (0, Value::I64(1_000)),
            (2, Value::str("note").unwrap()),
            (3, Value::Decimal(Decimal::new(42, 8, 2).unwrap())),
        ])
        .expect("record");
        round_trip(Value::Record(rec));
    }

    #[test]
    fn truncated_and_unknown_inputs_are_errors() {
        assert_eq!(decode_value(&[]), Err(ValueError::Truncated));
        // I64 needs 8 payload bytes.
        assert_eq!(decode_value(&[2, 0, 0]), Err(ValueError::Truncated));
        // NodeRef needs 4.
        assert_eq!(decode_value(&[9, 0, 0, 0]), Err(ValueError::Truncated));
        // Str length beyond the buffer.
        assert_eq!(
            decode_value(&[5, 9, 0, 0, 0, b'a']),
            Err(ValueError::Truncated)
        );
        assert_eq!(decode_value(&[99]), Err(ValueError::UnknownTag(99)));
    }
}

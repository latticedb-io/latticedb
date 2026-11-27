//! Attribute cell values (SPEC-01 §3–§5): built-in scalars, enum cells
//! (u8 dict index), flat record cells with a u64 presence bitmask.
//!
//! Absence semantics (SPEC-01 §5): a record cell carries one presence bit per
//! declared field position; fields added after a cell was written read as
//! absent (bit unset). Defaults evaluate at read time — they are schema-level
//! (T-102) and never backfill bytes.

use crate::ids::NodeId;
use std::fmt;

/// Maximum record field count — presence bitmask is u64 (SPEC-01 §5).
pub const MAX_RECORD_FIELDS: usize = 64;
/// Max string value bytes (SPEC-01 §3).
pub const MAX_STRING_BYTES: usize = 65_536;
/// Max bytes value (SPEC-01 §3; quota system scales later).
pub const MAX_BYTES_VALUE: usize = 1_048_576;
/// Max decimal precision (SPEC-01 §3: decimal(p,s), p ≤ 38, i128 backing).
pub const MAX_DECIMAL_PRECISION: u8 = 38;

/// Cell-level error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellError {
    StringTooLong {
        len: usize,
    },
    BytesTooLong {
        len: usize,
    },
    /// decimal(p,s): p must be ≤ 38 and s ∈ [0, p].
    DecimalScaleOutOfRange {
        precision: u8,
        scale: u8,
    },
    DecimalPrecisionTooHigh {
        precision: u8,
    },
    /// |unscaled| must fit within p digits.
    DecimalUnscaledOutOfRange {
        unscaled: i128,
        precision: u8,
    },
    RecordTooWide {
        count: usize,
    },
    FieldIndexOutOfRange {
        index: usize,
        count: usize,
    },
}

impl fmt::Display for CellError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CellError::StringTooLong { len } => {
                write!(f, "string value is {len} bytes; cap is {MAX_STRING_BYTES}")
            }
            CellError::BytesTooLong { len } => {
                write!(f, "bytes value is {len} bytes; cap is {MAX_BYTES_VALUE}")
            }
            CellError::DecimalScaleOutOfRange { precision, scale } => {
                write!(f, "decimal scale {scale} out of range [0, {precision}]")
            }
            CellError::DecimalPrecisionTooHigh { precision } => {
                write!(
                    f,
                    "decimal precision {precision} exceeds cap {MAX_DECIMAL_PRECISION}"
                )
            }
            CellError::DecimalUnscaledOutOfRange {
                unscaled,
                precision,
            } => {
                write!(
                    f,
                    "decimal unscaled value {unscaled} exceeds {precision} digits"
                )
            }
            CellError::RecordTooWide { count } => {
                write!(f, "record has {count} fields; cap is {MAX_RECORD_FIELDS}")
            }
            CellError::FieldIndexOutOfRange { index, count } => {
                write!(f, "record field index {index} out of range [0, {count})")
            }
        }
    }
}

impl std::error::Error for CellError {}

/// Fixed-point decimal: `unscaled × 10^-scale`, bounded to `precision` digits
/// (SPEC-01 §3: `decimal(p,s)`, p ∈ [1,38], s ∈ [0,p]; exact arithmetic only —
/// the money type; f64 MUST NOT be used for monetary values).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Decimal {
    pub unscaled: i128,
    pub scale: u8,
    pub precision: u8,
}

impl Decimal {
    pub fn new(unscaled: i128, precision: u8, scale: u8) -> Result<Self, CellError> {
        if precision == 0 || precision > MAX_DECIMAL_PRECISION {
            return Err(CellError::DecimalPrecisionTooHigh { precision });
        }
        if scale > precision {
            return Err(CellError::DecimalScaleOutOfRange { precision, scale });
        }
        let limit = 10i128.pow(u32::from(precision));
        if unscaled.abs() >= limit {
            return Err(CellError::DecimalUnscaledOutOfRange {
                unscaled,
                precision,
            });
        }
        Ok(Self {
            unscaled,
            scale,
            precision,
        })
    }

    /// Exact addition (money arithmetic): aligns scales on the finer one.
    /// Overflow of the backing i128 is impossible for in-range operands with
    /// p ≤ 38 (worst case 10^38 + 10^38 < i128::MAX ≈ 1.7×10^38? No — it can
    /// overflow at extreme range, so it is checked and reported).
    pub fn checked_add(&self, other: &Decimal) -> Result<Decimal, CellError> {
        let (scale, a, b) = if self.scale >= other.scale {
            (
                self.scale,
                self.unscaled,
                other.unscaled * 10i128.pow(u32::from(self.scale - other.scale)),
            )
        } else {
            (
                other.scale,
                self.unscaled * 10i128.pow(u32::from(other.scale - self.scale)),
                other.unscaled,
            )
        };
        let sum = a
            .checked_add(b)
            .ok_or(CellError::DecimalUnscaledOutOfRange {
                unscaled: 0,
                precision: MAX_DECIMAL_PRECISION,
            })?;
        // Result keeps the wider of the two declared precisions.
        let precision = self.precision.max(other.precision);
        Decimal::new(sum, precision, scale)
    }

    /// Canonical display: fixed-point with `scale` fraction digits.
    pub fn to_fixed_string(&self) -> String {
        let neg = self.unscaled < 0;
        let abs = self.unscaled.unsigned_abs();
        let div = 10u128.pow(u32::from(self.scale));
        let int_part = abs / div;
        let frac_part = abs % div;
        if self.scale == 0 {
            format!("{}{}", if neg { "-" } else { "" }, int_part)
        } else {
            format!(
                "{}{}.{:0width$}",
                if neg { "-" } else { "" },
                int_part,
                frac_part,
                width = self.scale as usize
            )
        }
    }
}

/// One attribute cell value. Scalar set is closed (SPEC-01 §3); enum cells
/// carry the u8 dict index (dict lives in the EnumType, T-102).
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    I64(i64),
    /// IEEE-754 double — never for money (SPEC-01 §3).
    F64(f64),
    Decimal(Decimal),
    Str(Box<str>),
    Bytes(Box<[u8]>),
    /// Microseconds since Unix epoch, UTC.
    Timestamp(i64),
    /// Microseconds.
    Duration(i64),
    /// References an existing node in the same Database (R6 checks at write).
    NodeRef(NodeId),
    /// Enum dict index (u8-backed, ≤ 256 values).
    Enum(u8),
    /// Flat record cell: field values + presence bitmask (SPEC-01 §5).
    Record(RecordValue),
}

impl Value {
    pub fn str(s: impl Into<Box<str>>) -> Result<Self, CellError> {
        let s = s.into();
        if s.len() > MAX_STRING_BYTES {
            return Err(CellError::StringTooLong { len: s.len() });
        }
        Ok(Value::Str(s))
    }

    pub fn bytes(b: impl Into<Box<[u8]>>) -> Result<Self, CellError> {
        let b = b.into();
        if b.len() > MAX_BYTES_VALUE {
            return Err(CellError::BytesTooLong { len: b.len() });
        }
        Ok(Value::Bytes(b))
    }
}

/// Flat record cell (depth 1): `field_count ≤ 64`, one presence bit per field
/// position. `values` is the dense storage of *present* positions? No —
/// values is indexed by declared field position; absent positions hold no
/// meaningful bytes, so they are skipped in storage and reconstructed here as
/// `Value::Bool(false)` placeholders? For the in-memory representation we
/// keep the declared-shape vector sparse-free: `values[idx]` is meaningful
/// iff presence bit `idx` is set (SPEC-01 §5); readers must consult
/// [`RecordValue::get`].
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RecordValue {
    values: Vec<Value>,
    presence: u64,
}

impl RecordValue {
    /// Build a record cell from (position, value) pairs; positions are
    /// declared field positions, need not be dense.
    pub fn from_pairs(pairs: &[(usize, Value)]) -> Result<Self, CellError> {
        let declared = pairs.iter().map(|(i, _)| *i).max().map_or(0, |m| m + 1);
        if declared > MAX_RECORD_FIELDS {
            return Err(CellError::RecordTooWide { count: declared });
        }
        let mut rv = Self {
            values: vec![Value::Bool(false); declared],
            presence: 0,
        };
        for (idx, v) in pairs {
            rv.set(*idx, v.clone())?;
        }
        Ok(rv)
    }

    pub fn field_count(&self) -> usize {
        self.values.len()
    }

    /// Write one field: value + presence bit. Structurally bounded by u64.
    pub fn set(&mut self, idx: usize, v: Value) -> Result<(), CellError> {
        if idx >= MAX_RECORD_FIELDS {
            return Err(CellError::RecordTooWide { count: idx + 1 });
        }
        if idx >= self.values.len() {
            self.values.resize(idx + 1, Value::Bool(false));
        }
        self.values[idx] = v;
        self.presence |= 1u64 << idx;
        Ok(())
    }

    /// Absence semantics: `None` when the presence bit is unset — including
    /// fields added to the record type after this cell was written.
    pub fn get(&self, idx: usize) -> Option<&Value> {
        if idx >= self.values.len() || (self.presence >> idx) & 1 == 0 {
            return None;
        }
        self.values.get(idx)
    }

    pub fn presence_mask(&self) -> u64 {
        self.presence
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // SGT-3.1 (representation level): decimal exactness — 0.1 + 0.2 = 0.30.
    #[test]
    fn decimal_addition_is_exact() {
        let a = Decimal::new(10, 10, 2).unwrap(); // 0.10
        let b = Decimal::new(20, 10, 2).unwrap(); // 0.20
        let sum = a.checked_add(&b).unwrap();
        assert_eq!(sum.unscaled, 30, "0.1 + 0.2 must be exactly 0.30 (i128 30)");
        assert_eq!(sum.to_fixed_string(), "0.30");
    }

    // SGT-3.2 (representation level): range guard.
    #[test]
    fn decimal_rejects_precision_violations() {
        assert!(matches!(
            Decimal::new(1, 39, 0),
            Err(CellError::DecimalPrecisionTooHigh { precision: 39 })
        ));
        assert!(matches!(
            Decimal::new(1, 10, 11),
            Err(CellError::DecimalScaleOutOfRange {
                precision: 10,
                scale: 11
            })
        ));
        assert!(matches!(
            Decimal::new(100, 2, 2), // 3 digits > p=2
            Err(CellError::DecimalUnscaledOutOfRange {
                unscaled: 100,
                precision: 2
            })
        ));
    }

    #[test]
    fn decimal_negative_fixed_string() {
        let d = Decimal::new(-12345, 12, 3).unwrap();
        assert_eq!(d.to_fixed_string(), "-12.345");
    }

    // SGT-3.4 (representation level): string cap.
    #[test]
    fn string_cap_enforced() {
        let ok = Value::str("x".repeat(65_536)).unwrap();
        assert_eq!(ok, Value::Str("x".repeat(65_536).into_boxed_str()));
        let err = Value::str("x".repeat(65_537)).unwrap_err();
        assert_eq!(err, CellError::StringTooLong { len: 65_537 });
    }

    #[test]
    fn bytes_cap_enforced() {
        let err = Value::bytes(vec![0u8; 1_048_577]).unwrap_err();
        assert_eq!(err, CellError::BytesTooLong { len: 1_048_577 });
        let ok = Value::bytes(vec![7u8; 1_048_576]).unwrap();
        assert!(matches!(ok, Value::Bytes(_)));
    }

    // SGT-5.1 (representation level): absence semantics on later fields.
    #[test]
    fn record_presence_bitmask_absence_semantics() {
        let mut rec =
            RecordValue::from_pairs(&[(0, Value::I64(42)), (1, Value::Str("hello".into()))])
                .unwrap();
        assert_eq!(rec.field_count(), 2);
        assert_eq!(rec.presence_mask(), 0b11, "both written fields present");
        assert_eq!(rec.get(0), Some(&Value::I64(42)));
        assert_eq!(rec.get(1), Some(&Value::Str("hello".into())));

        // A field added after the cell was written: bit unset → reads absent.
        assert_eq!(rec.get(2), None, "field 2 not yet written must read absent");
        rec.set(2, Value::F64(0.5)).unwrap();
        assert_eq!(rec.presence_mask(), 0b111);
        assert_eq!(rec.get(2), Some(&Value::F64(0.5)));
    }

    #[test]
    fn record_width_bounded_by_u64_mask() {
        let err = RecordValue::from_pairs(&[(64, Value::I64(1))]).unwrap_err();
        assert_eq!(err, CellError::RecordTooWide { count: 65 });
    }

    #[test]
    fn value_equality_is_data_equality() {
        let a = Value::Timestamp(1_700_000_000_000_001);
        let b = Value::Timestamp(1_700_000_000_000_001);
        let c = Value::Duration(1);
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(Value::NodeRef(NodeId(9)), Value::NodeRef(NodeId(9)));
        assert_eq!(Value::Enum(3), Value::Enum(3));
    }
}

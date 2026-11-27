//! Codec registry (SPEC-03 §5): deterministic column codecs + chains.
//!
//! | id | codec | applies to |
//! |----|-------|------------|
//! | 0x00 | identity | all |
//! | 0x01 | RLE | bool, enum (low-cardinality) |
//! | 0x02 | dictionary | enum, string |
//! | 0x03 | delta (zig-zag varint) | i64 |
//! | 0x04 | delta-of-delta | i64 (temporal keys, timestamps) |
//! | 0x05 | Gorilla | f64 series |
//! | 0x06 | bitpack | bool |
//! | 0x07 | zstd (block) | any — last stage only (feature `zstd`, ED-09) |
//!
//! Determinism (SPEC-03 D1): `decode(encode(x)) == x` byte-exact; encoders
//! are pure functions of the input column — no randomness, no wall clock,
//! stable iteration order (dictionary entries are sorted).
//!
//! Codec chains `[value_codec, frame?]` are recorded in part headers;
//! decoders honor the header and never assume defaults (SPEC-03 §5). With
//! the `zstd` feature off, default chains ship `frame = None` — deterministic
//! per build (ED-09); a header carrying zstd still decodes when the feature
//! is enabled.

use std::fmt;

// -- column model -----------------------------------------------------------

/// Logical type of one encoded column (drives codec applicability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Bool,
    I64,
    F64,
    Str,
    Bytes,
    /// Enum cells: u8 dict index (dict lives in the EnumType).
    Enum,
    /// Decimal unscaled i128 (precision/scale ride in the attribute def).
    Dec,
}

/// One column of homogeneous cells (pre-encoding).
#[derive(Debug, Clone, PartialEq)]
pub enum ColumnData {
    Bool(Vec<bool>),
    I64(Vec<i64>),
    F64(Vec<f64>),
    Str(Vec<Box<str>>),
    Bytes(Vec<Vec<u8>>),
    Enum(Vec<u8>),
    Dec(Vec<i128>),
}

impl ColumnData {
    pub fn ty(&self) -> ColumnType {
        match self {
            ColumnData::Bool(_) => ColumnType::Bool,
            ColumnData::I64(_) => ColumnType::I64,
            ColumnData::F64(_) => ColumnType::F64,
            ColumnData::Str(_) => ColumnType::Str,
            ColumnData::Bytes(_) => ColumnType::Bytes,
            ColumnData::Enum(_) => ColumnType::Enum,
            ColumnData::Dec(_) => ColumnType::Dec,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            ColumnData::Bool(v) => v.len(),
            ColumnData::I64(v) => v.len(),
            ColumnData::F64(v) => v.len(),
            ColumnData::Str(v) => v.len(),
            ColumnData::Bytes(v) => v.len(),
            ColumnData::Enum(v) => v.len(),
            ColumnData::Dec(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// f64 comparison for D1 checks: bit equality (NaN-safe).
pub fn f64_bits_eq(a: &[f64], b: &[f64]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b.iter())
            .all(|(x, y)| x.to_bits() == y.to_bits())
}

// -- errors (per-module error struct — AGENTS Rust rules) -------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// Column type not applicable to this codec.
    TypeMismatch {
        codec: u8,
        got: ColumnType,
    },
    UnknownCodec {
        id: u8,
        stage: &'static str,
    },
    /// Encoded bytes are structurally invalid for the codec.
    Corrupt {
        codec: u8,
        reason: String,
    },
    /// Decoded cell count differs from the requested count.
    CountMismatch {
        codec: u8,
        expected: usize,
        got: usize,
    },
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::TypeMismatch { codec, got } => {
                write!(
                    f,
                    "codec 0x{codec:02X} does not apply to column type {got:?}"
                )
            }
            CodecError::UnknownCodec { id, stage } => {
                write!(f, "unknown codec id 0x{id:02X} at stage {stage}")
            }
            CodecError::Corrupt { codec, reason } => {
                write!(f, "codec 0x{codec:02X}: corrupt input: {reason}")
            }
            CodecError::CountMismatch {
                codec,
                expected,
                got,
            } => {
                write!(
                    f,
                    "codec 0x{codec:02X}: decoded {got} cells, expected {expected}"
                )
            }
        }
    }
}

impl std::error::Error for CodecError {}

// -- varint primitives (LEB128 + zig-zag) -----------------------------------

fn put_uvarint(buf: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            buf.push(b);
            break;
        }
        buf.push(b | 0x80);
    }
}

fn read_uvarint(data: &[u8], pos: &mut usize, codec: u8) -> Result<u64, CodecError> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    loop {
        let b = data.get(*pos).copied().ok_or_else(|| CodecError::Corrupt {
            codec,
            reason: "varint runs past end of input".into(),
        })?;
        *pos += 1;
        result |= u64::from(b & 0x7F) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 64 {
            return Err(CodecError::Corrupt {
                codec,
                reason: "varint too long".into(),
            });
        }
    }
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(u: u64) -> i64 {
    ((u >> 1) as i64) ^ -((u & 1) as i64)
}

// -- codec ids --------------------------------------------------------------

pub const IDENTITY: u8 = 0x00;
pub const RLE: u8 = 0x01;
pub const DICTIONARY: u8 = 0x02;
pub const DELTA: u8 = 0x03;
pub const DELTA_OF_DELTA: u8 = 0x04;
pub const GORILLA: u8 = 0x05;
pub const BITPACK: u8 = 0x06;
pub const ZSTD: u8 = 0x07;
pub const NO_FRAME: u8 = 0xFF;

// -- identity (0x00) --------------------------------------------------------

fn encode_identity(col: &ColumnData) -> Vec<u8> {
    let mut out = vec![col.ty() as u8];
    match col {
        ColumnData::Bool(v) => {
            out.extend_from_slice(&(v.len() as u64).to_le_bytes());
            for b in v {
                out.push(u8::from(*b));
            }
        }
        ColumnData::I64(v) => {
            out.extend_from_slice(&(v.len() as u64).to_le_bytes());
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
        ColumnData::F64(v) => {
            out.extend_from_slice(&(v.len() as u64).to_le_bytes());
            for x in v {
                out.extend_from_slice(&x.to_bits().to_le_bytes());
            }
        }
        ColumnData::Str(v) => {
            out.extend_from_slice(&(v.len() as u64).to_le_bytes());
            for s in v {
                out.extend_from_slice(&(s.len() as u32).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
        ColumnData::Bytes(v) => {
            out.extend_from_slice(&(v.len() as u64).to_le_bytes());
            for b in v {
                out.extend_from_slice(&(b.len() as u32).to_le_bytes());
                out.extend_from_slice(b);
            }
        }
        ColumnData::Enum(v) => {
            out.extend_from_slice(&(v.len() as u64).to_le_bytes());
            out.extend_from_slice(v);
        }
        ColumnData::Dec(v) => {
            out.extend_from_slice(&(v.len() as u64).to_le_bytes());
            for x in v {
                out.extend_from_slice(&x.to_le_bytes());
            }
        }
    }
    out
}

fn corrupt(codec: u8, reason: impl Into<String>) -> CodecError {
    CodecError::Corrupt {
        codec,
        reason: reason.into(),
    }
}

fn decode_identity(ty: ColumnType, data: &[u8]) -> Result<ColumnData, CodecError> {
    const C: u8 = IDENTITY;
    let got = data.first().copied().unwrap_or(0xFF);
    if got != ty as u8 {
        return Err(corrupt(
            C,
            format!("type tag {got:#04x} != header type {:#04x}", ty as u8),
        ));
    }
    let mut pos = 1usize;
    let mut take = |n: usize| -> Result<&[u8], CodecError> {
        if pos + n > data.len() {
            return Err(corrupt(C, "input truncated"));
        }
        let s = &data[pos..pos + n];
        pos += n;
        Ok(s)
    };
    let count = u64::from_le_bytes(le_bytes(take(8)?)) as usize;
    match ty {
        ColumnType::Bool => {
            let raw = take(count)?;
            Ok(ColumnData::Bool(raw.iter().map(|b| *b != 0).collect()))
        }
        ColumnType::I64 => {
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                v.push(i64::from_le_bytes(le_bytes(take(8)?)));
            }
            Ok(ColumnData::I64(v))
        }
        ColumnType::F64 => {
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                v.push(f64::from_bits(u64::from_le_bytes(le_bytes(take(8)?))));
            }
            Ok(ColumnData::F64(v))
        }
        ColumnType::Str => {
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                let len = u32::from_le_bytes(le_bytes(take(4)?)) as usize;
                let raw = take(len)?;
                let s = String::from_utf8(raw.to_vec())
                    .map_err(|_| corrupt(C, "invalid utf8"))?
                    .into_boxed_str();
                v.push(s);
            }
            Ok(ColumnData::Str(v))
        }
        ColumnType::Bytes => {
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                let len = u32::from_le_bytes(le_bytes(take(4)?)) as usize;
                v.push(take(len)?.to_vec());
            }
            Ok(ColumnData::Bytes(v))
        }
        ColumnType::Enum => Ok(ColumnData::Enum(take(count)?.to_vec())),
        ColumnType::Dec => {
            let mut v = Vec::with_capacity(count);
            for _ in 0..count {
                v.push(i128::from_le_bytes(le_bytes(take(16)?)));
            }
            Ok(ColumnData::Dec(v))
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

// -- RLE (0x01): bool / enum — runs of (varint count, raw value) ------------

fn encode_rle(col: &ColumnData) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    match col {
        ColumnData::Bool(v) => {
            let mut i = 0usize;
            while i < v.len() {
                let cur = v[i];
                let mut j = i + 1;
                while j < v.len() && v[j] == cur {
                    j += 1;
                }
                put_uvarint(&mut out, (j - i) as u64);
                out.push(u8::from(cur));
                i = j;
            }
        }
        ColumnData::Enum(v) => {
            let mut i = 0usize;
            while i < v.len() {
                let cur = v[i];
                let mut j = i + 1;
                while j < v.len() && v[j] == cur {
                    j += 1;
                }
                put_uvarint(&mut out, (j - i) as u64);
                out.push(cur);
                i = j;
            }
        }
        _ => {
            return Err(CodecError::TypeMismatch {
                codec: RLE,
                got: col.ty(),
            });
        }
    }
    Ok(out)
}

fn decode_rle(ty: ColumnType, data: &[u8], count: usize) -> Result<ColumnData, CodecError> {
    const C: u8 = RLE;
    if !matches!(ty, ColumnType::Bool | ColumnType::Enum) {
        return Err(CodecError::TypeMismatch { codec: C, got: ty });
    }
    let mut pos = 0usize;
    let mut decoded = 0usize;
    let mut bools: Vec<bool> = Vec::with_capacity(count);
    let mut enums: Vec<u8> = Vec::with_capacity(count);
    while decoded < count {
        let run = read_uvarint(data, &mut pos, C)? as usize;
        let v = data
            .get(pos)
            .copied()
            .ok_or_else(|| corrupt(C, "truncated run"))?;
        pos += 1;
        if decoded + run > count {
            return Err(corrupt(C, "runs exceed declared count"));
        }
        match ty {
            ColumnType::Bool => {
                for _ in 0..run {
                    bools.push(v != 0);
                }
            }
            ColumnType::Enum => {
                for _ in 0..run {
                    enums.push(v);
                }
            }
            _ => unreachable!("checked above"),
        }
        decoded += run;
    }
    if pos != data.len() {
        return Err(corrupt(C, "trailing bytes"));
    }
    Ok(match ty {
        ColumnType::Bool => ColumnData::Bool(bools),
        ColumnType::Enum => ColumnData::Enum(enums),
        _ => unreachable!("checked above"),
    })
}

// -- dictionary (0x02): enum / string — sorted dict + varint indices --------

fn encode_dictionary(col: &ColumnData) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    match col {
        ColumnData::Enum(v) => {
            let mut dict: Vec<u8> = v.clone();
            dict.sort_unstable();
            dict.dedup();
            put_uvarint(&mut out, dict.len() as u64);
            out.extend_from_slice(&dict);
            let index_of: std::collections::BTreeMap<u8, u64> = dict
                .iter()
                .enumerate()
                .map(|(i, d)| (*d, i as u64))
                .collect();
            for cell in v {
                put_uvarint(&mut out, index_of[cell]);
            }
        }
        ColumnData::Str(v) => {
            let mut dict: Vec<&str> = v.iter().map(|s| &**s).collect();
            dict.sort_unstable();
            dict.dedup();
            put_uvarint(&mut out, dict.len() as u64);
            for d in &dict {
                out.extend_from_slice(&(d.len() as u32).to_le_bytes());
                out.extend_from_slice(d.as_bytes());
            }
            let index_of: std::collections::BTreeMap<&str, u64> = dict
                .iter()
                .enumerate()
                .map(|(i, d)| (*d, i as u64))
                .collect();
            for cell in v {
                put_uvarint(&mut out, index_of[&**cell]);
            }
        }
        _ => {
            return Err(CodecError::TypeMismatch {
                codec: DICTIONARY,
                got: col.ty(),
            });
        }
    }
    Ok(out)
}

fn decode_dictionary(ty: ColumnType, data: &[u8], count: usize) -> Result<ColumnData, CodecError> {
    const C: u8 = DICTIONARY;
    let mut pos = 0usize;
    match ty {
        ColumnType::Enum => {
            let dict_len = read_uvarint(data, &mut pos, C)? as usize;
            if pos + dict_len > data.len() {
                return Err(corrupt(C, "dict truncated"));
            }
            let dict = data[pos..pos + dict_len].to_vec();
            pos += dict_len;
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                let i = read_uvarint(data, &mut pos, C)? as usize;
                let v = dict
                    .get(i)
                    .copied()
                    .ok_or_else(|| corrupt(C, "index out of dict"))?;
                out.push(v);
            }
            if pos != data.len() {
                return Err(corrupt(C, "trailing bytes"));
            }
            Ok(ColumnData::Enum(out))
        }
        ColumnType::Str => {
            let dict_len = read_uvarint(data, &mut pos, C)? as usize;
            let mut dict: Vec<Box<str>> = Vec::with_capacity(dict_len);
            for _ in 0..dict_len {
                if pos + 4 > data.len() {
                    return Err(corrupt(C, "dict entry truncated"));
                }
                let len = u32::from_le_bytes(le_bytes(&data[pos..pos + 4])) as usize;
                pos += 4;
                if pos + len > data.len() {
                    return Err(corrupt(C, "dict string truncated"));
                }
                let s = String::from_utf8(data[pos..pos + len].to_vec())
                    .map_err(|_| corrupt(C, "invalid utf8"))?;
                pos += len;
                dict.push(s.into_boxed_str());
            }
            let mut out = Vec::with_capacity(count);
            for _ in 0..count {
                let i = read_uvarint(data, &mut pos, C)? as usize;
                let v = dict
                    .get(i)
                    .cloned()
                    .ok_or_else(|| corrupt(C, "index out of dict"))?;
                out.push(v);
            }
            if pos != data.len() {
                return Err(corrupt(C, "trailing bytes"));
            }
            Ok(ColumnData::Str(out))
        }
        _ => Err(CodecError::TypeMismatch { codec: C, got: ty }),
    }
}

// -- delta (0x03) / delta-of-delta (0x04): i64, zig-zag varint, wrapping ----

fn encode_delta(col: &ColumnData) -> Result<Vec<u8>, CodecError> {
    let ColumnData::I64(v) = col else {
        return Err(CodecError::TypeMismatch {
            codec: DELTA,
            got: col.ty(),
        });
    };
    let mut out = Vec::new();
    let Some(first) = v.first() else {
        return Ok(out); // empty column: zero bytes (count rides in the part header)
    };
    out.extend_from_slice(&first.to_le_bytes());
    for pair in v.windows(2) {
        let d = pair[1].wrapping_sub(pair[0]);
        put_uvarint(&mut out, zigzag(d));
    }
    Ok(out)
}

fn decode_delta(data: &[u8], count: usize) -> Result<ColumnData, CodecError> {
    const C: u8 = DELTA;
    let mut pos = 0usize;
    let mut out = Vec::with_capacity(count);
    if count > 0 {
        if data.len() < 8 {
            return Err(corrupt(C, "first value truncated"));
        }
        out.push(i64::from_le_bytes(le_bytes(&data[..8])));
        pos = 8;
    }
    while out.len() < count {
        let d = unzigzag(read_uvarint(data, &mut pos, C)?);
        let prev = out[out.len() - 1];
        out.push(prev.wrapping_add(d));
    }
    if pos != data.len() {
        return Err(corrupt(C, "trailing bytes"));
    }
    Ok(ColumnData::I64(out))
}

fn encode_delta_of_delta(col: &ColumnData) -> Result<Vec<u8>, CodecError> {
    let ColumnData::I64(v) = col else {
        return Err(CodecError::TypeMismatch {
            codec: DELTA_OF_DELTA,
            got: col.ty(),
        });
    };
    let mut out = Vec::new();
    if let Some(first) = v.first() {
        out.extend_from_slice(&first.to_le_bytes());
    }
    if v.len() > 1 {
        let d1 = v[1].wrapping_sub(v[0]);
        put_uvarint(&mut out, zigzag(d1));
    }
    for i in 2..v.len() {
        let d = v[i].wrapping_sub(v[i - 1]);
        let dd = d.wrapping_sub(v[i - 1].wrapping_sub(v[i - 2]));
        put_uvarint(&mut out, zigzag(dd));
    }
    Ok(out)
}

fn decode_delta_of_delta(data: &[u8], count: usize) -> Result<ColumnData, CodecError> {
    const C: u8 = DELTA_OF_DELTA;
    let mut pos = 0usize;
    let mut out: Vec<i64> = Vec::with_capacity(count);
    if count > 0 {
        if data.len() < 8 {
            return Err(corrupt(C, "first value truncated"));
        }
        out.push(i64::from_le_bytes(le_bytes(&data[..8])));
        pos = 8;
    }
    if out.len() < count {
        let d1 = unzigzag(read_uvarint(data, &mut pos, C)?);
        let prev = out[0];
        out.push(prev.wrapping_add(d1));
    }
    while out.len() < count {
        let n = out.len();
        let dd = unzigzag(read_uvarint(data, &mut pos, C)?);
        let d = out[n - 1].wrapping_sub(out[n - 2]).wrapping_add(dd);
        let prev = out[n - 1];
        out.push(prev.wrapping_add(d));
    }
    if pos != data.len() {
        return Err(corrupt(C, "trailing bytes"));
    }
    Ok(ColumnData::I64(out))
}

// -- bit stream (LSB-first) for Gorilla / bitpack ---------------------------

struct BitWriter {
    bytes: Vec<u8>,
    bitpos: usize,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            bitpos: 0,
        }
    }
    fn write_bit(&mut self, bit: u8) {
        let byte = self.bitpos / 8;
        if byte >= self.bytes.len() {
            self.bytes.push(0);
        }
        if bit != 0 {
            self.bytes[byte] |= 1 << (self.bitpos % 8);
        }
        self.bitpos += 1;
    }
    fn write_bits(&mut self, v: u64, n: usize) {
        for i in 0..n {
            let bit = ((v >> i) & 1) as u8;
            self.write_bit(bit);
        }
    }
    fn finish(self) -> Vec<u8> {
        self.bytes
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    bitpos: usize,
}

impl<'a> BitReader<'a> {
    fn read_bit(&mut self) -> Result<u8, CodecError> {
        let byte = self.bitpos / 8;
        let b = self
            .data
            .get(byte)
            .copied()
            .ok_or_else(|| corrupt(GORILLA, "bit stream truncated"))?;
        let bit = (b >> (self.bitpos % 8)) & 1;
        self.bitpos += 1;
        Ok(bit)
    }
    fn read_bits(&mut self, n: usize) -> Result<u64, CodecError> {
        let mut v = 0u64;
        for i in 0..n {
            let bit = self.read_bit()? as u64;
            v |= bit << i;
        }
        Ok(v)
    }
}

// -- Gorilla (0x05): f64 series ---------------------------------------------

fn encode_gorilla(col: &ColumnData) -> Result<Vec<u8>, CodecError> {
    let ColumnData::F64(v) = col else {
        return Err(CodecError::TypeMismatch {
            codec: GORILLA,
            got: col.ty(),
        });
    };
    let mut w = BitWriter::new();
    let mut prev_bits: u64 = 0;
    let mut prev_lead: i32 = -1;
    let mut prev_tail: i32 = 0;
    for (i, x) in v.iter().enumerate() {
        let bits = x.to_bits();
        if i == 0 {
            w.write_bits(bits, 64);
            prev_bits = bits;
            continue;
        }
        let xor = prev_bits ^ bits;
        if xor == 0 {
            w.write_bit(0);
            continue;
        }
        // Gorilla window clamp: the lead field is 5 bits (max 31); a larger
        // lead simply widens the meaningful window (the extra bits are zero).
        let lead = (xor.leading_zeros() as i32).min(31);
        let trail = xor.trailing_zeros() as i32;
        let mb = (64 - lead - trail) as usize;
        if prev_lead >= 0 && lead >= prev_lead && trail >= prev_tail {
            // reuse previous window: control '10'
            w.write_bit(1);
            w.write_bit(0);
            w.write_bits(xor >> prev_tail, (64 - prev_lead - prev_tail) as usize);
        } else {
            // new window: control '11', 5-bit lead, 6-bit (mb-1), mb payload
            w.write_bit(1);
            w.write_bit(1);
            w.write_bits(lead as u64, 5);
            w.write_bits(mb as u64 - 1, 6);
            w.write_bits(xor >> trail, mb);
            prev_lead = lead;
            prev_tail = trail;
        }
        prev_bits = bits;
    }
    Ok(w.finish())
}

fn decode_gorilla(data: &[u8], count: usize) -> Result<ColumnData, CodecError> {
    let mut r = BitReader { data, bitpos: 0 };
    let mut out = Vec::with_capacity(count);
    let mut prev_bits: u64 = 0;
    let mut prev_lead: i32 = -1;
    let mut prev_tail: i32 = 0;
    for i in 0..count {
        if i == 0 {
            let bits = r.read_bits(64)?;
            out.push(f64::from_bits(bits));
            prev_bits = bits;
            continue;
        }
        let ctl = r.read_bit()?;
        let bits = if ctl == 0 {
            prev_bits
        } else {
            let reuse = r.read_bit()?;
            let xor;
            if reuse == 0 {
                let mb = (64 - prev_lead - prev_tail) as usize;
                let payload = r.read_bits(mb)?;
                // payload < 2^mb: shifting into the window is exact — the
                // window's top bit sits below bit 64 (lead >= 0).
                xor = payload << prev_tail;
            } else {
                let lead = r.read_bits(5)? as i32;
                let mb = (r.read_bits(6)? + 1) as usize;
                let payload = r.read_bits(mb)?;
                let shift = 64 - lead - mb as i32;
                xor = payload << shift;
                prev_lead = lead;
                prev_tail = shift;
            }
            prev_bits ^ xor
        };
        out.push(f64::from_bits(bits));
        prev_bits = bits;
    }
    Ok(ColumnData::F64(out))
}

// -- bitpack (0x06): bool, LSB-first -----------------------------------------

fn encode_bitpack(col: &ColumnData) -> Result<Vec<u8>, CodecError> {
    let ColumnData::Bool(v) = col else {
        return Err(CodecError::TypeMismatch {
            codec: BITPACK,
            got: col.ty(),
        });
    };
    let mut out = vec![0u8; v.len().div_ceil(8)];
    for (i, b) in v.iter().enumerate() {
        if *b {
            out[i / 8] |= 1 << (i % 8);
        }
    }
    Ok(out)
}

fn decode_bitpack(data: &[u8], count: usize) -> Result<ColumnData, CodecError> {
    const C: u8 = BITPACK;
    let need = count.div_ceil(8);
    if data.len() < need {
        return Err(corrupt(C, "packed bits truncated"));
    }
    if data.len() > need {
        return Err(corrupt(C, "trailing bytes"));
    }
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push((data[i / 8] >> (i % 8)) & 1 != 0);
    }
    Ok(ColumnData::Bool(out))
}

// -- dispatch ----------------------------------------------------------------

/// Encode one column with a value codec.
pub fn encode_value(codec: u8, col: &ColumnData) -> Result<Vec<u8>, CodecError> {
    match codec {
        IDENTITY => Ok(encode_identity(col)),
        RLE => encode_rle(col),
        DICTIONARY => encode_dictionary(col),
        DELTA => encode_delta(col),
        DELTA_OF_DELTA => encode_delta_of_delta(col),
        GORILLA => encode_gorilla(col),
        BITPACK => encode_bitpack(col),
        other => Err(CodecError::UnknownCodec {
            id: other,
            stage: "value",
        }),
    }
}

/// Decode one column: `count` cells of `ty` from value-codec bytes.
pub fn decode_value(
    codec: u8,
    ty: ColumnType,
    data: &[u8],
    count: usize,
) -> Result<ColumnData, CodecError> {
    let col = match codec {
        IDENTITY => decode_identity(ty, data)?,
        RLE => decode_rle(ty, data, count)?,
        DICTIONARY => decode_dictionary(ty, data, count)?,
        DELTA => {
            let c = decode_delta(data, count)?;
            if c.ty() != ty {
                return Err(corrupt(DELTA, "decoded type mismatch"));
            }
            c
        }
        DELTA_OF_DELTA => {
            let c = decode_delta_of_delta(data, count)?;
            if c.ty() != ty {
                return Err(corrupt(DELTA_OF_DELTA, "decoded type mismatch"));
            }
            c
        }
        GORILLA => {
            let c = decode_gorilla(data, count)?;
            if c.ty() != ty {
                return Err(corrupt(GORILLA, "decoded type mismatch"));
            }
            c
        }
        BITPACK => {
            let c = decode_bitpack(data, count)?;
            if c.ty() != ty {
                return Err(corrupt(BITPACK, "decoded type mismatch"));
            }
            c
        }
        other => {
            return Err(CodecError::UnknownCodec {
                id: other,
                stage: "value",
            });
        }
    };
    if col.ty() != ty {
        return Err(corrupt(codec, "decoded type mismatch"));
    }
    if col.len() != count {
        return Err(CodecError::CountMismatch {
            codec,
            expected: count,
            got: col.len(),
        });
    }
    Ok(col)
}

// -- codec chains (SPEC-03 §5) ----------------------------------------------

/// `[value_codec, frame?]` — recorded in the part header; decoders honor it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodecChain {
    pub value: u8,
    /// Frame stage: `ZSTD` or `NO_FRAME`.
    pub frame: u8,
}

impl CodecChain {
    pub const fn new(value: u8, frame: u8) -> Self {
        Self { value, frame }
    }

    pub const fn pure(value: u8) -> Self {
        Self {
            value,
            frame: NO_FRAME,
        }
    }

    pub const fn identity() -> Self {
        Self::pure(IDENTITY)
    }

    /// Two-byte header encoding `[value][frame]`.
    pub fn to_bytes(&self) -> [u8; 2] {
        [self.value, self.frame]
    }

    pub fn from_bytes(b: &[u8]) -> Result<Self, CodecError> {
        if b.len() != 2 {
            return Err(corrupt(0xFF, "chain header must be exactly 2 bytes"));
        }
        Ok(Self {
            value: b[0],
            frame: b[1],
        })
    }
}

/// Default chain per scalar type (SPEC-03 §5). With the `zstd` feature off,
/// the frame stage ships as `NO_FRAME` (ED-09).
pub fn default_chain(ty: ColumnType) -> CodecChain {
    let frame = if cfg!(feature = "zstd") {
        ZSTD
    } else {
        NO_FRAME
    };
    let value = match ty {
        ColumnType::Bool => BITPACK,
        ColumnType::Enum | ColumnType::Str => DICTIONARY,
        ColumnType::I64 => DELTA,
        ColumnType::F64 => GORILLA,
        ColumnType::Bytes | ColumnType::Dec => IDENTITY,
    };
    CodecChain::new(value, frame)
}

/// Encode through the chain: value stage, then frame stage.
pub fn encode_chain(chain: CodecChain, col: &ColumnData) -> Result<Vec<u8>, CodecError> {
    let staged = encode_value(chain.value, col)?;
    match chain.frame {
        NO_FRAME => Ok(staged),
        ZSTD => zstd_compress(&staged),
        other => Err(CodecError::UnknownCodec {
            id: other,
            stage: "frame",
        }),
    }
}

/// Decode through the chain: frame stage first (owned staging), then value.
pub fn decode_chain(
    chain: CodecChain,
    ty: ColumnType,
    data: &[u8],
    count: usize,
) -> Result<ColumnData, CodecError> {
    let frame_decoded;
    let staged: &[u8] = match chain.frame {
        NO_FRAME => data,
        ZSTD => {
            frame_decoded = zstd_decompress(data)?;
            &frame_decoded
        }
        other => {
            return Err(CodecError::UnknownCodec {
                id: other,
                stage: "frame",
            });
        }
    };
    decode_value(chain.value, ty, staged, count)
}

#[cfg(feature = "zstd")]
fn zstd_compress(data: &[u8]) -> Result<Vec<u8>, CodecError> {
    let level = 3_i32; // documented default; the config-tunable lands with parts (T-107)
    zstd::encode_all(std::io::Cursor::new(data), level)
        .map_err(|e| corrupt(ZSTD, format!("compression failed: {e}")))
}

#[cfg(feature = "zstd")]
fn zstd_decompress(data: &[u8]) -> Result<Vec<u8>, CodecError> {
    let mut out = Vec::new();
    zstd::decode_all(std::io::Cursor::new(data), &mut out)
        .map_err(|e| corrupt(ZSTD, format!("decompression failed: {e}")))?;
    Ok(out)
}

#[cfg(not(feature = "zstd"))]
fn zstd_compress(_data: &[u8]) -> Result<Vec<u8>, CodecError> {
    Err(CodecError::UnknownCodec {
        id: ZSTD,
        stage: "frame (feature off)",
    })
}

#[cfg(not(feature = "zstd"))]
fn zstd_decompress(_data: &[u8]) -> Result<Vec<u8>, CodecError> {
    Err(CodecError::UnknownCodec {
        id: ZSTD,
        stage: "frame (feature off)",
    })
}

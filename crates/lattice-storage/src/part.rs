//! Parts — columnar attribute storage (SPEC-03 §4): the unit of attribute
//! truth. Immutable image per `(graph, attr_id, generation)`.
//!
//! - Cells strictly sorted by `(node_id, k)` (SFT-4.1); one node = one run.
//! - Dense keys: `k = first_k + i` per run; sparse keys: explicit k array
//!   riding at the end of each run blob.
//! - **Scalar = column with fixed k=0** (SPEC-03 §9.1): scalar overwrites
//!   are new cells in newer parts; [`merge`] keeps the greatest-LSN cell
//!   per `(node, k)` (SFT-4.2). Tombstones are LSN-stamped and suppress
//!   older cells; they survive merges until compaction purges them (M5).
//! - Zone maps per run (total-order min/max, null_count) let cursor filters
//!   skip runs without decoding them (SFT-4.3 — asserted via the decode
//!   counter).
//! - Absence: per-run validity bitmap; record cells carry the field-presence
//!   bitmask inside the value (SPEC-01 §5); merge preserves it exactly
//!   (SFT-4.4).
//!
//! Image: header(46) | run_directory(166/run) | data | presence | footer.
//! Run blob: `[u32 values_len][u32 lsns_len][values col][lsns col][ks?]`,
//! everything through the part's codec chain. Decimal parts store the
//! attribute's (precision, scale) in the header. Catalog bytes are never
//! here; value truth lives exclusively in parts (three-layer truth).

use crate::codecs::{CodecChain, CodecError, ColumnData, ColumnType, encode_value};
use crate::crc32c::crc32c_of;
use lattice_core::ids::{AttrId, GraphId, KeyIndex, Lsn, NodeId};
use lattice_core::value::Value;
use lattice_core::value_codec as vc;
use std::cell::Cell as Atomic64;
use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};
use std::path::Path;

pub const PART_MAGIC: &[u8; 7] = b"LPART01";
pub const PART_VERSION: u32 = 1;
const HEADER_LEN: usize = 45;
const BLOCK: usize = 64 * 1024;
/// Format framing constant (on-disk blocking), not a business tunable.
const ROW_GROUP: u32 = 65_536;
const DENSITY_SPARSE: u8 = 1;
const ZONE_MAX: usize = 64;
const RUN_ENTRY_LEN: usize = 4 + 8 + 4 + 4 + 8 + (1 + 2 + ZONE_MAX) * 2 + 4;

#[derive(Debug)]
pub enum PartError {
    NotAPart {
        reason: String,
    },
    Corrupt {
        reason: String,
    },
    TotalCrcMismatch {
        expected: u32,
        got: u32,
    },
    Truncated {
        need: usize,
        have: usize,
    },
    Codec(CodecError),
    Io(io::Error),
    /// Builder misuse: duplicate `(node, k)` within one part.
    DuplicateCell {
        node: u32,
        k: u64,
    },
    /// Merge misuse: identical LSNs for one `(node, k)` — journal LSNs are
    /// unique, so this is a determinism bug.
    MergeConflict {
        node: u32,
        k: u64,
    },
    /// Merge inputs disagree on value mode / density / codec chain.
    ShapeMismatch {
        reason: String,
    },
    /// Core value codec failure (record cells).
    ValueCodec(lattice_core::value_codec::ValueError),
}

impl fmt::Display for PartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PartError::NotAPart { reason } => write!(f, "not a part: {reason}"),
            PartError::Corrupt { reason } => write!(f, "part corrupt: {reason}"),
            PartError::TotalCrcMismatch { expected, got } => {
                write!(
                    f,
                    "part total CRC mismatch: expected {expected:#010x}, got {got:#010x}"
                )
            }
            PartError::Truncated { need, have } => {
                write!(f, "part truncated: need {need}, have {have}")
            }
            PartError::Codec(e) => write!(f, "part codec: {e}"),
            PartError::Io(e) => write!(f, "part i/o: {e}"),
            PartError::DuplicateCell { node, k } => {
                write!(f, "duplicate (node {node}, k {k}) within one part")
            }
            PartError::MergeConflict { node, k } => {
                write!(f, "merge conflict: identical LSNs for (node {node}, k {k})")
            }
            PartError::ShapeMismatch { reason } => write!(f, "merge shape mismatch: {reason}"),
            PartError::ValueCodec(e) => write!(f, "part value codec: {e}"),
        }
    }
}

impl std::error::Error for PartError {}

impl From<CodecError> for PartError {
    fn from(e: CodecError) -> Self {
        PartError::Codec(e)
    }
}
impl From<io::Error> for PartError {
    fn from(e: io::Error) -> Self {
        PartError::Io(e)
    }
}

fn corrupt(reason: impl Into<String>) -> PartError {
    PartError::Corrupt {
        reason: reason.into(),
    }
}

/// One attribute cell with its journal stamp.
#[derive(Debug, Clone, PartialEq)]
pub struct PartCell {
    pub node: NodeId,
    pub k: KeyIndex,
    pub lsn: Lsn,
    /// `None` = tombstone (LSN-stamped deletion marker, SPEC-03 §4).
    pub value: Option<Value>,
}

// -- zone maps ---------------------------------------------------------------

/// Total-order zone key over comparable values (None = not zoneable).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct ZoneBytes {
    tag: u8,
    bytes: Vec<u8>,
}

impl ZoneBytes {
    fn key(v: &Value) -> Option<Self> {
        let mut z = Self::default();
        match v {
            Value::Bool(b) => {
                z.tag = 1;
                z.bytes = vec![u8::from(*b)];
            }
            Value::I64(x) => {
                z.tag = 2;
                z.bytes = ((*x as u64) ^ (1 << 63)).to_be_bytes().to_vec();
            }
            Value::F64(x) => {
                z.tag = 3;
                let bits = x.to_bits();
                let key = if bits & (1 << 63) != 0 {
                    !bits
                } else {
                    bits | (1 << 63)
                };
                z.bytes = key.to_be_bytes().to_vec();
            }
            Value::Timestamp(x) => {
                z.tag = 4;
                z.bytes = ((*x as u64) ^ (1 << 63)).to_be_bytes().to_vec();
            }
            Value::Duration(x) => {
                z.tag = 5;
                z.bytes = ((*x as u64) ^ (1 << 63)).to_be_bytes().to_vec();
            }
            Value::NodeRef(n) => {
                z.tag = 6;
                z.bytes = n.0.to_be_bytes().to_vec();
            }
            Value::Enum(x) => {
                z.tag = 7;
                z.bytes = vec![*x];
            }
            Value::Decimal(d) => {
                z.tag = 8;
                z.bytes = d.scale.to_be_bytes().to_vec();
                z.bytes.extend_from_slice(&d.unscaled.to_be_bytes());
            }
            Value::Str(s) => {
                z.tag = 9;
                z.bytes = s.as_bytes().to_vec();
            }
            Value::Record(_) | Value::Bytes(_) => return None,
        }
        Some(z)
    }

    /// Fixed-slot encoding: [tag u8][len u16][≤64 bytes, truncated]. Prefix
    /// truncation keeps the order valid for prefix-comparable zones (it only
    /// widens the admitted range — filtering precision drops, correctness
    /// holds).
    fn encode_fixed(&self, slot: &mut [u8]) {
        let len = self.bytes.len().min(ZONE_MAX);
        slot[0] = self.tag;
        slot[1..3].copy_from_slice(&(len as u16).to_le_bytes());
        slot[3..3 + len].copy_from_slice(&self.bytes[..len]);
    }

    fn decode_fixed(slot: &[u8]) -> Self {
        let tag = slot[0];
        let len = u16::from_le_bytes(le_bytes(&slot[1..3])) as usize;
        Self {
            tag,
            bytes: slot[3..3 + len].to_vec(),
        }
    }

    fn is_empty_zone(&self) -> bool {
        self.tag == 0 && self.bytes.is_empty()
    }
}

// -- builder -----------------------------------------------------------------

/// Value mode of a part's column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PartValueType {
    Typed(ColumnType),
    /// Record cells: each present value is core-codec bytes in a Bytes column.
    Record,
}

impl PartValueType {
    fn column(&self) -> ColumnType {
        match self {
            PartValueType::Typed(t) => *t,
            PartValueType::Record => ColumnType::Bytes,
        }
    }
    fn tag(&self) -> u8 {
        match self {
            PartValueType::Typed(t) => t.to_u8(),
            PartValueType::Record => 0xFF,
        }
    }
    fn from_tag(tag: u8) -> Option<Self> {
        Some(match tag {
            0xFF => PartValueType::Record,
            t => PartValueType::Typed(ColumnType::from_u8(t)?),
        })
    }
}

/// Builder: cells may arrive in any order; `finish()` sorts, rejects
/// duplicate `(node, k)`, groups runs per node, and encodes the image.
pub struct PartBuilder {
    graph: GraphId,
    attr: AttrId,
    generation: u64,
    mode: PartValueType,
    sparse: bool,
    chain: CodecChain,
    dec_precision: u8,
    dec_scale: u8,
    cells: Vec<PartCell>,
}

impl PartBuilder {
    pub fn new(
        graph: GraphId,
        attr: AttrId,
        generation: u64,
        mode: PartValueType,
        sparse: bool,
        chain: CodecChain,
    ) -> Self {
        Self {
            graph,
            attr,
            generation,
            mode,
            sparse,
            chain,
            dec_precision: 38,
            dec_scale: 0,
            cells: Vec::new(),
        }
    }

    /// Decimal parts must declare the attribute's (p, s) — restored on read.
    pub fn decimal_params(&mut self, precision: u8, scale: u8) {
        self.dec_precision = precision;
        self.dec_scale = scale;
    }

    pub fn add(&mut self, cell: PartCell) {
        self.cells.push(cell);
    }

    pub fn finish(self) -> Result<PartImage, PartError> {
        let mut cells = self.cells;
        cells.sort_by_key(|a| (a.node.0, a.k.0));
        for w in cells.windows(2) {
            if w[0].node == w[1].node && w[0].k == w[1].k {
                return Err(PartError::DuplicateCell {
                    node: w[1].node.0,
                    k: w[1].k.0,
                });
            }
        }

        let mut runs: Vec<(NodeId, Vec<&PartCell>)> = Vec::new();
        for cell in &cells {
            match runs.last_mut() {
                Some((node, list)) if *node == cell.node => list.push(cell),
                _ => runs.push((cell.node, vec![cell])),
            }
        }

        let mut out = Vec::new();
        out.extend_from_slice(PART_MAGIC);
        out.extend_from_slice(&PART_VERSION.to_le_bytes());
        out.extend_from_slice(&self.graph.0.to_le_bytes());
        out.extend_from_slice(&self.attr.0.to_le_bytes());
        out.extend_from_slice(&self.generation.to_le_bytes());
        out.push(if self.sparse { DENSITY_SPARSE } else { 0 });
        let ch = self.chain.to_bytes();
        out.push(ch[0]);
        out.push(ch[1]);
        out.extend_from_slice(&(cells.len() as u64).to_le_bytes());
        out.extend_from_slice(&ROW_GROUP.to_le_bytes());
        out.extend_from_slice(&(runs.len() as u32).to_le_bytes());
        out.push(self.mode.tag());
        out.push(self.dec_precision);
        out.push(self.dec_scale);
        if out.len() != HEADER_LEN {
            panic!("header len {} != {}", out.len(), HEADER_LEN);
        }

        // run directory placeholder
        let dir_pos = out.len();
        for _ in &runs {
            out.extend_from_slice(&[0u8; RUN_ENTRY_LEN]);
        }

        // data section
        let data_start = out.len();
        let mut presence = Vec::new();
        let mut entries: Vec<(u32, u64, u32, u32, u64, ZoneBytes, ZoneBytes, u32)> = Vec::new();
        for (node, run_cells) in &runs {
            let cell_count = run_cells.len() as u32;
            let mut values: Vec<Value> = Vec::new();
            let mut lsns: Vec<i64> = Vec::new();
            let mut bitmap =
                vec![0u8; cell_count as usize / 8 + usize::from(!cell_count.is_multiple_of(8))];
            let mut lo: Option<ZoneBytes> = None;
            let mut hi: Option<ZoneBytes> = None;
            let mut nulls = 0u32;
            for (i, c) in run_cells.iter().enumerate() {
                // Tombstones stamp their LSN too (absence of VALUE, not of
                // history): the LSN column covers every cell slot.
                lsns.push(c.lsn.0 as i64);
                match &c.value {
                    Some(v) => {
                        bitmap[i / 8] |= 1 << (i % 8);
                        values.push(v.clone());
                        if let Some(z) = ZoneBytes::key(v) {
                            lo = Some(match lo {
                                Some(cur) if cur <= z => cur,
                                _ => z.clone(),
                            });
                            hi = Some(match hi {
                                Some(cur) if cur >= z => cur,
                                _ => z,
                            });
                        }
                    }
                    None => nulls += 1,
                }
            }
            let val_col: ColumnData = match &self.mode {
                PartValueType::Record => ColumnData::Bytes(
                    values
                        .iter()
                        .map(|v| {
                            let mut enc = Vec::new();
                            vc::encode_value(v, &mut enc);
                            enc
                        })
                        .collect(),
                ),
                PartValueType::Typed(t) => {
                    let bad = |v: &Value| corrupt(format!("typed part got value {v:?}"));
                    match t {
                        ColumnType::Bool => ColumnData::Bool(
                            values
                                .iter()
                                .map(|v| match v {
                                    Value::Bool(b) => Ok(*b),
                                    other => Err(bad(other)),
                                })
                                .collect::<Result<Vec<bool>, PartError>>()?,
                        ),
                        ColumnType::I64 => ColumnData::I64(
                            values
                                .iter()
                                .map(|v| match v {
                                    Value::I64(x) | Value::Timestamp(x) | Value::Duration(x) => {
                                        Ok(*x)
                                    }
                                    Value::NodeRef(n) => Ok(n.0 as i64),
                                    other => Err(bad(other)),
                                })
                                .collect::<Result<Vec<i64>, PartError>>()?,
                        ),
                        ColumnType::F64 => ColumnData::F64(
                            values
                                .iter()
                                .map(|v| match v {
                                    Value::F64(x) => Ok(*x),
                                    other => Err(bad(other)),
                                })
                                .collect::<Result<Vec<f64>, PartError>>()?,
                        ),
                        ColumnType::Str => ColumnData::Str(
                            values
                                .iter()
                                .map(|v| match v {
                                    Value::Str(s) => Ok(s.clone()),
                                    other => Err(bad(other)),
                                })
                                .collect::<Result<Vec<Box<str>>, PartError>>()?,
                        ),
                        ColumnType::Bytes => ColumnData::Bytes(
                            values
                                .iter()
                                .map(|v| match v {
                                    Value::Bytes(b) => Ok(b.to_vec()),
                                    other => Err(bad(other)),
                                })
                                .collect::<Result<Vec<Vec<u8>>, PartError>>()?,
                        ),
                        ColumnType::Enum => ColumnData::Enum(
                            values
                                .iter()
                                .map(|v| match v {
                                    Value::Enum(x) => Ok(*x),
                                    other => Err(bad(other)),
                                })
                                .collect::<Result<Vec<u8>, PartError>>()?,
                        ),
                        ColumnType::Dec => ColumnData::Dec(
                            values
                                .iter()
                                .map(|v| match v {
                                    Value::Decimal(d) => Ok(d.unscaled),
                                    other => Err(bad(other)),
                                })
                                .collect::<Result<Vec<i128>, PartError>>()?,
                        ),
                    }
                }
            };
            let values_enc = encode_value(self.chain.value, &val_col)?;
            let lsn_enc = encode_value(self.chain.value, &ColumnData::I64(lsns))?;
            let data_off = out.len() - data_start;
            out.extend_from_slice(&(values_enc.len() as u32).to_le_bytes());
            out.extend_from_slice(&(lsn_enc.len() as u32).to_le_bytes());
            out.extend_from_slice(&values_enc);
            out.extend_from_slice(&lsn_enc);
            if self.sparse {
                for c in run_cells {
                    out.extend_from_slice(&c.k.0.to_le_bytes());
                }
            }
            presence.extend_from_slice(&bitmap);
            let first_k = run_cells.first().ok_or_else(|| corrupt("empty run"))?.k.0;
            entries.push((
                node.0,
                first_k,
                cell_count,
                values.len() as u32,
                data_off as u64,
                lo.unwrap_or_default(),
                hi.unwrap_or_default(),
                nulls,
            ));
        }
        let data_len = out.len() - data_start;

        // presence section
        let _presence_pos = out.len() - data_start;
        out.extend_from_slice(&presence);

        // patch run directory
        for (i, (node, first_k, cell_count, present, data_off, lo, hi, nulls)) in
            entries.iter().enumerate()
        {
            let p = dir_pos + i * RUN_ENTRY_LEN;
            out[p..p + 4].copy_from_slice(&node.to_le_bytes());
            out[p + 4..p + 12].copy_from_slice(&first_k.to_le_bytes());
            out[p + 12..p + 16].copy_from_slice(&cell_count.to_le_bytes());
            out[p + 16..p + 20].copy_from_slice(&present.to_le_bytes());
            out[p + 20..p + 28].copy_from_slice(&data_off.to_le_bytes());
            lo.encode_fixed(&mut out[p + 28..p + 28 + 3 + ZONE_MAX]);
            hi.encode_fixed(&mut out[p + 28 + 3 + ZONE_MAX..p + 28 + 2 * (3 + ZONE_MAX)]);
            out[p + 28 + 2 * (3 + ZONE_MAX)..p + RUN_ENTRY_LEN]
                .copy_from_slice(&nulls.to_le_bytes());
        }

        // footer: [data_len u64][crc × blocks][part total u32][magic 7];
        // the total covers the entire image before it (parts carry no
        // catalog, so full-file scope is correct here — unlike snapshots).
        let blocks = data_len.div_ceil(BLOCK);
        out.extend_from_slice(&(data_len as u64).to_le_bytes());
        for i in 0..blocks {
            let start = data_start + i * BLOCK;
            let end = data_start + ((i + 1) * BLOCK).min(data_len);
            out.extend_from_slice(&crc32c_of(&out[start..end]).to_le_bytes());
        }
        let total = crc32c_of(&out);
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(PART_MAGIC);
        Ok(PartImage { bytes: out })
    }
}

/// Encoded part image.
pub struct PartImage {
    pub bytes: Vec<u8>,
}

impl PartImage {
    pub fn write(&self, path: &Path) -> Result<(), PartError> {
        let mut f = std::fs::File::create(path)?;
        f.write_all(&self.bytes)?;
        f.sync_data()?;
        Ok(())
    }

    /// Raw bytes (tests, MANIFEST bookkeeping).
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

// -- reader ------------------------------------------------------------------

/// LE array from a statically verified slice (loud on mismatch — invariant).
fn le_bytes<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(src);
    out
}

/// Parsed part with lazy run decoding.
#[derive(Clone)]
pub struct Part {
    pub graph: GraphId,
    pub attr: AttrId,
    pub generation: u64,
    pub sparse: bool,
    pub mode: PartValueType,
    pub chain: CodecChain,
    pub dec_precision: u8,
    pub dec_scale: u8,
    pub cell_count: u64,
    data: Vec<u8>,
    data_start: usize,
    presence_pos: usize,
    runs: Vec<RunEntry>,
    decoded_runs: Atomic64<u64>,
}

#[derive(Debug, Clone)]
struct RunEntry {
    node: NodeId,
    first_k: KeyIndex,
    cell_count: u32,
    present_count: u32,
    data_off: u64,
    region_len: usize,
    zone_lo: ZoneBytes,
    zone_hi: ZoneBytes,
    zone_nulls: u32,
    presence_off: usize,
}

impl fmt::Debug for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Part")
            .field("graph", &self.graph.0)
            .field("attr", &self.attr.0)
            .field("generation", &self.generation)
            .field("cell_count", &self.cell_count)
            .field("runs", &self.runs.len())
            .finish()
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PartError> {
        if self.pos + n > self.data.len() {
            return Err(PartError::Truncated {
                need: self.pos + n,
                have: self.data.len(),
            });
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u32(&mut self) -> Result<u32, PartError> {
        Ok(u32::from_le_bytes(le_bytes(self.take(4)?)))
    }
    fn u64(&mut self) -> Result<u64, PartError> {
        Ok(u64::from_le_bytes(le_bytes(self.take(8)?)))
    }
}

impl Part {
    /// Runs decoded so far — the zone-pushdown instrumentation (SFT-4.3).
    pub fn decoded_runs(&self) -> u64 {
        self.decoded_runs.get()
    }

    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    pub fn run_node(&self, run_idx: usize) -> NodeId {
        self.runs[run_idx].node
    }

    /// Zone bounds of a run (`None` when the value type is not zoneable).
    pub fn run_zone(&self, run_idx: usize) -> Option<(ZoneBytes, ZoneBytes)> {
        let e = &self.runs[run_idx];
        if e.zone_lo.is_empty_zone() {
            None
        } else {
            Some((e.zone_lo.clone(), e.zone_hi.clone()))
        }
    }

    /// Parse + verify a part image.
    pub fn from_bytes(data: &[u8]) -> Result<Part, PartError> {
        if data.len() < HEADER_LEN + 12 {
            return Err(PartError::Truncated {
                need: HEADER_LEN + 12,
                have: data.len(),
            });
        }
        if data.len() < 7 || &data[..7] != PART_MAGIC {
            return Err(PartError::NotAPart {
                reason: "bad magic".into(),
            });
        }
        let tail = data.len() - 7;
        if &data[tail..] != PART_MAGIC {
            return Err(PartError::NotAPart {
                reason: "bad magic tail".into(),
            });
        }
        let total_expected = u32::from_le_bytes(le_bytes(&data[tail - 4..tail]));
        let total_got = crc32c_of(&data[..tail - 4]);
        if total_expected != total_got {
            return Err(PartError::TotalCrcMismatch {
                expected: total_expected,
                got: total_got,
            });
        }
        let version = u32::from_le_bytes(le_bytes(&data[7..11]));
        if version != PART_VERSION {
            return Err(PartError::NotAPart {
                reason: format!("version {version}"),
            });
        }
        let graph = GraphId(u16::from_le_bytes(le_bytes(&data[11..13])));
        let attr = AttrId(u16::from_le_bytes(le_bytes(&data[13..15])));
        let generation = u64::from_le_bytes(le_bytes(&data[15..23]));
        let sparse = data[23] == DENSITY_SPARSE;
        let chain = CodecChain::from_bytes(&data[24..26])?;
        let cell_count = u64::from_le_bytes(le_bytes(&data[26..34]));
        let _row_group = u32::from_le_bytes(le_bytes(&data[34..38]));
        let run_count = u32::from_le_bytes(le_bytes(&data[38..42])) as usize;
        let mode = PartValueType::from_tag(data[42]).ok_or_else(|| PartError::NotAPart {
            reason: format!("value type tag {:#04x}", data[42]),
        })?;
        let dec_precision = data[43];
        let dec_scale = data[44];

        // Parse the directory first — presence length is then known, and
        // data_len falls out of the image-size equation (no provisional
        // footer reads). image = dir | data | presence |
        // [data_len u64][crc × n][total u32][magic 7].
        let data_start = HEADER_LEN + run_count * RUN_ENTRY_LEN;

        // run directory
        let mut c = Cursor {
            data,
            pos: HEADER_LEN,
        };
        let mut runs = Vec::with_capacity(run_count);
        for _ in 0..run_count {
            let node = NodeId(c.u32()?);
            let first_k = KeyIndex(c.u64()?);
            let cell_count = c.u32()?;
            let present_count = c.u32()?;
            let data_off = c.u64()?;
            let zone_lo = ZoneBytes::decode_fixed(c.take(3 + ZONE_MAX)?);
            let zone_hi = ZoneBytes::decode_fixed(c.take(3 + ZONE_MAX)?);
            let zone_nulls = c.u32()?;
            runs.push(RunEntry {
                node,
                first_k,
                cell_count,
                present_count,
                data_off,
                region_len: 0,
                zone_lo,
                zone_hi,
                zone_nulls,
                presence_off: 0,
            });
        }
        let presence_len: usize = runs
            .iter()
            .map(|r| r.cell_count as usize / 8 + usize::from(r.cell_count % 8 != 0))
            .sum();
        let x = (tail - 4) - data_start - presence_len; // data_len + 8 + 4*crc
        let mut data_len = None;
        for crc_n in 0..=x.div_ceil(BLOCK) + 1 {
            let candidate = x.saturating_sub(8 + 4 * crc_n);
            if candidate.div_ceil(BLOCK) == crc_n {
                data_len = Some(candidate);
                break;
            }
        }
        let data_len = data_len.ok_or_else(|| corrupt("footer size equation unsolvable"))?;
        let presence_pos = data_start + data_len;
        for i in 0..runs.len() {
            let next_off = if i + 1 < runs.len() {
                runs[i + 1].data_off
            } else {
                (presence_pos - data_start) as u64
            };
            runs[i].region_len = (next_off - runs[i].data_off) as usize;
            runs[i].presence_off = runs[..i]
                .iter()
                .map(|r| r.cell_count as usize / 8 + usize::from(r.cell_count % 8 != 0))
                .sum();
        }

        Ok(Part {
            graph,
            attr,
            generation,
            sparse,
            mode,
            chain,
            dec_precision,
            dec_scale,
            cell_count,
            data: data.to_vec(),
            data_start,
            presence_pos,
            runs,
            decoded_runs: Atomic64::new(0),
        })
    }

    /// Decode one run's cells (lazy; increments the decode counter).
    pub fn run_cells(&self, run_idx: usize) -> Result<Vec<PartCell>, PartError> {
        self.decoded_runs.set(self.decoded_runs.get() + 1);
        let entry = &self.runs[run_idx];
        let region = &self.data[self.data_start + entry.data_off as usize
            ..self.data_start + entry.data_off as usize + entry.region_len];
        if region.len() < 8 {
            return Err(corrupt("run region truncated"));
        }
        let val_len = u32::from_le_bytes(le_bytes(&region[..4])) as usize;
        let lsn_len = u32::from_le_bytes(le_bytes(&region[4..8])) as usize;
        if region.len() < 8 + val_len + lsn_len {
            return Err(corrupt("run columns truncated"));
        }
        let values = crate::codecs::decode_value(
            self.chain.value,
            self.mode.column(),
            &region[8..8 + val_len],
            entry.present_count as usize,
        )?;
        let lsns = crate::codecs::decode_value(
            self.chain.value,
            ColumnType::I64,
            &region[8 + val_len..8 + val_len + lsn_len],
            entry.cell_count as usize,
        )?;
        let lsn_list = match &lsns {
            ColumnData::I64(v) => v,
            _ => return Err(corrupt("lsn column type")),
        };
        let ks: Vec<u64> = if self.sparse {
            let ks_off = 8 + val_len + lsn_len;
            (0..entry.cell_count as usize)
                .map(|i| u64::from_le_bytes(le_bytes(&region[ks_off + i * 8..ks_off + i * 8 + 8])))
                .collect()
        } else {
            Vec::new()
        };
        let mut cells = Vec::with_capacity(entry.cell_count as usize);
        let mut vi = 0usize;
        for i in 0..entry.cell_count as usize {
            let k = if self.sparse {
                ks[i]
            } else {
                entry.first_k.0 + i as u64
            };
            let present =
                (self.data[self.presence_pos + entry.presence_off + i / 8] >> (i % 8)) & 1 == 1;
            let lsn = Lsn(lsn_list[i] as u64);
            if present {
                let v = match &values {
                    ColumnData::Bool(v) => Value::Bool(v[vi]),
                    ColumnData::I64(v) => match self.mode.column() {
                        ColumnType::I64 => Value::I64(v[vi]),
                        _ => Value::Timestamp(v[vi]), // temporal parts: caller knows
                    },
                    ColumnData::F64(v) => Value::F64(v[vi]),
                    ColumnData::Str(v) => Value::Str(v[vi].clone()),
                    ColumnData::Bytes(v) => match self.mode {
                        PartValueType::Record => {
                            let (val, used) =
                                vc::decode_value(&v[vi]).map_err(PartError::ValueCodec)?;
                            debug_assert_eq!(used, v[vi].len(), "record cell decodes fully");
                            val
                        }
                        _ => Value::Bytes(v[vi].clone().into_boxed_slice()),
                    },
                    ColumnData::Enum(v) => Value::Enum(v[vi]),
                    ColumnData::Dec(v) => {
                        let d = lattice_core::value::Decimal::new(
                            v[vi],
                            self.dec_precision,
                            self.dec_scale,
                        )
                        .map_err(|e| corrupt(format!("decimal restore: {e}")))?;
                        Value::Decimal(d)
                    }
                };
                cells.push(PartCell {
                    node: entry.node,
                    k: KeyIndex(k),
                    lsn,
                    value: Some(v),
                });
                vi += 1;
            } else {
                cells.push(PartCell {
                    node: entry.node,
                    k: KeyIndex(k),
                    lsn,
                    value: None,
                });
            }
        }
        debug_assert_eq!(vi, entry.present_count as usize);
        Ok(cells)
    }

    /// Zone pushdown: does this run's zone map admit the inclusive value
    /// range `[lo, hi]`? Non-zoneable values always admit (must decode).
    pub fn run_admits_range(&self, run_idx: usize, lo: &Value, hi: &Value) -> bool {
        let entry = &self.runs[run_idx];
        if entry.zone_lo.is_empty_zone() {
            return true;
        }
        let (Some(zlo), Some(zhi)) = (ZoneBytes::key(lo), ZoneBytes::key(hi)) else {
            return true;
        };
        entry.zone_hi >= zlo && entry.zone_lo <= zhi
    }

    /// Binary-search the run for `node` (runs are node-ordered).
    pub fn run_of(&self, node: NodeId) -> Option<usize> {
        // runs are node-ascending and unique: partition_point lands ON the
        // candidate run.
        let i = self.runs.partition_point(|r| r.node.0 < node.0);
        if i < self.runs.len() && self.runs[i].node == node {
            Some(i)
        } else {
            None
        }
    }

    /// Exact `(node, k)` lookup (SFT-4.1).
    pub fn lookup(&self, node: NodeId, k: KeyIndex) -> Result<Option<PartCell>, PartError> {
        let Some(run) = self.run_of(node) else {
            return Ok(None);
        };
        let cells = self.run_cells(run)?;
        Ok(cells
            .binary_search_by(|c| c.k.0.cmp(&k.0))
            .ok()
            .map(|i| cells[i].clone()))
    }

    /// All cells of all runs, decoded eagerly (merge/replay use).
    pub fn all_cells(&self) -> Result<Vec<PartCell>, PartError> {
        let mut out = Vec::with_capacity(self.cell_count as usize);
        for i in 0..self.runs.len() {
            out.extend(self.run_cells(i)?);
        }
        Ok(out)
    }

    /// The zone key of a decoded cell value (for merge re-zoning).
    fn zone_of(v: &Value) -> Option<ZoneBytes> {
        ZoneBytes::key(v)
    }
}

// -- merge (SPEC-03 §7: deterministic sort-merge, greatest LSN wins) ---------

/// Deterministic k-way merge: for each `(node, k)` the cell with the
/// greatest LSN wins; identical LSNs on one key are a determinism bug
/// (`MergeConflict`). Tombstones win by LSN like any cell and survive
/// output (purge is compaction's job — M5). Same inputs ⇒ same output
/// bytes (SPEC-03 D3).
pub fn merge(parts: &[&Part]) -> Result<PartImage, PartError> {
    let first = parts
        .first()
        .ok_or_else(|| corrupt("merge of zero parts"))?;
    for p in parts {
        if p.mode != first.mode
            || p.sparse != first.sparse
            || p.chain != first.chain
            || p.graph != first.graph
            || p.attr != first.attr
        {
            return Err(PartError::ShapeMismatch {
                reason: "inputs must share graph, attr, mode, density and chain".into(),
            });
        }
    }

    // Decode everything (lazy parts → full cell streams), then sort-merge.
    let mut streams: Vec<Vec<PartCell>> = Vec::with_capacity(parts.len());
    for p in parts {
        streams.push(p.all_cells()?);
    }
    let mut merged: BTreeMap<(u32, u64), PartCell> = BTreeMap::new();
    for stream in &streams {
        for cell in stream {
            let key = (cell.node.0, cell.k.0);
            match merged.get_mut(&key) {
                None => {
                    merged.insert(key, cell.clone());
                }
                Some(cur) => {
                    if cur.lsn == cell.lsn {
                        return Err(PartError::MergeConflict {
                            node: key.0,
                            k: key.1,
                        });
                    }
                    if cell.lsn > cur.lsn {
                        *cur = cell.clone();
                    }
                }
            }
        }
    }

    let mut b = PartBuilder::new(
        first.graph,
        first.attr,
        0,
        first.mode,
        first.sparse,
        first.chain,
    );
    if matches!(first.mode, PartValueType::Typed(ColumnType::Dec)) {
        b.decimal_params(first.dec_precision, first.dec_scale);
    }
    for cell in merged.into_values() {
        b.add(cell);
    }
    b.finish()
}

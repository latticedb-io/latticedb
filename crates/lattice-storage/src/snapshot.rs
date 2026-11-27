//! Snapshot format (SPEC-03 §3): immutable `snapshot.v<N>` = header |
//! structural_arrays | catalog_blob | part_index | footer.
//!
//! - header: magic `LTSNAP01` | version u32 | database_id u32 | V u64 |
//!   E u64 | schema_epoch u64 | created_lsn u64 (64 bytes).
//! - structural arrays: the fixed SoA/CSR ordering per SPEC-03 §3 (see
//!   [`lattice_core::arrays::RawArrays`]). Cell data never appears here —
//!   values live exclusively in parts (three-layer truth).
//! - catalog blob: length-prefixed op-log (deterministic schema-txn
//!   encoding, `lattice_schema::opslog`) + deprecation registries + CRC32C.
//!   On CRC failure the caller MUST recompile from the journal (SFT-3.2).
//! - compiled-mask blob: intentionally absent in v1 — open recompiles
//!   deterministically (ED-10).
//! - part_index: count = 0 in M1 (extended by T-107).
//! - footer: CRC32C table per 64 KiB block of the payload, total CRC32C,
//!   magic tail.
//!
//! Law (SPEC-03 §3): a failed structural-array CRC ⇒ the whole snapshot is
//! unusable (`SnapshotError::StructureCorrupt`) — never half-opens. A failed
//! catalog CRC is survivable (`catalog_valid = false`) because the catalog
//! is rebuildable from the journal's op-log.

use crate::crc32c::crc32c_of;
use lattice_core::arrays::{GraphArrays, RawArrays};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

pub const MAGIC: &[u8; 8] = b"LTSNAP01";
pub const FORMAT_VERSION: u32 = 1;
const HEADER_LEN: usize = 48;
const BLOCK: usize = 64 * 1024;

#[derive(Debug)]
pub enum SnapshotError {
    Io(io::Error),
    /// Bad magic / version — not a LatticeDB snapshot.
    NotASnapshot {
        reason: String,
    },
    /// Structural-array corruption — refuse the whole snapshot (SFT-3.3).
    StructureCorrupt {
        block: u64,
        offset: u64,
        reason: String,
    },
    /// Whole-file integrity failure (total CRC mismatch).
    TotalCrcMismatch {
        expected: u32,
        got: u32,
    },
    Truncated {
        need: usize,
        have: usize,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::Io(e) => write!(f, "snapshot i/o: {e}"),
            SnapshotError::NotASnapshot { reason } => write!(f, "not a snapshot: {reason}"),
            SnapshotError::StructureCorrupt {
                block,
                offset,
                reason,
            } => write!(
                f,
                "SNAPSHOT_CORRUPT: structural arrays damaged (block {block}, offset {offset}: {reason}) — rebuild from earliest valid backup + journal; refusing to half-open"
            ),
            SnapshotError::TotalCrcMismatch { expected, got } => {
                write!(
                    f,
                    "snapshot total CRC mismatch: expected {expected:#010x}, got {got:#010x}"
                )
            }
            SnapshotError::Truncated { need, have } => {
                write!(f, "snapshot truncated: need {need}, have {have}")
            }
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

impl std::error::Error for SnapshotError {}

impl From<io::Error> for SnapshotError {
    fn from(e: io::Error) -> Self {
        SnapshotError::Io(e)
    }
}

/// A parsed snapshot (arrays + catalog blob + validity flags).
#[derive(Debug, Clone)]
pub struct SnapshotData {
    pub database_id: u32,
    pub v: u64,
    pub e: u64,
    pub schema_epoch: u64,
    pub created_lsn: u64,
    pub arrays: RawArrays,
    /// Decoded op-log bytes (catalog reproduction input).
    pub catalog_ops: Vec<u8>,
    pub catalog_deprecations: Vec<u8>,
    /// `false` ⇒ the catalog blob failed its CRC — caller recompiles from
    /// the journal (SFT-3.2); the structural data stays trusted.
    pub catalog_valid: bool,
}

fn corrupt(codec_or_block: u64, reason: impl Into<String>) -> SnapshotError {
    SnapshotError::StructureCorrupt {
        block: codec_or_block,
        offset: 0,
        reason: reason.into(),
    }
}

/// Encode the full snapshot payload (header + arrays + catalog + footer).
pub fn encode_snapshot(
    database_id: u32,
    schema_epoch: u64,
    created_lsn: u64,
    arrays: &GraphArrays,
    catalog_ops: &[u8],
    catalog_deprecations: &[u8],
) -> Vec<u8> {
    let raw = arrays.to_raw();
    let v = raw.alive.len() as u64;
    let e = raw.edges.len() as u64;

    let mut buf = Vec::new();
    // header (64 bytes)
    buf.extend_from_slice(MAGIC);
    buf.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&database_id.to_le_bytes());
    buf.extend_from_slice(&v.to_le_bytes());
    buf.extend_from_slice(&e.to_le_bytes());
    buf.extend_from_slice(&schema_epoch.to_le_bytes());
    buf.extend_from_slice(&created_lsn.to_le_bytes());
    debug_assert_eq!(buf.len(), HEADER_LEN);

    // structural arrays (SPEC-03 §3 order)
    for a in &raw.alive {
        buf.push(*a);
    }
    for p in &raw.parent {
        buf.extend_from_slice(&p.to_le_bytes());
    }
    for g in &raw.graph {
        buf.extend_from_slice(&g.to_le_bytes());
    }
    for t in &raw.node_type {
        buf.extend_from_slice(&t.to_le_bytes());
    }
    for s in &raw.state {
        buf.push(*s);
    }
    for ep in &raw.epoch {
        buf.extend_from_slice(&ep.to_le_bytes());
    }
    for ac in &raw.attr_count {
        buf.extend_from_slice(&ac.to_le_bytes());
    }
    for o in &raw.child_off {
        buf.extend_from_slice(&o.to_le_bytes());
    }
    for t in &raw.child_tgt {
        buf.extend_from_slice(&t.to_le_bytes());
    }
    for o in &raw.dep_out_off {
        buf.extend_from_slice(&o.to_le_bytes());
    }
    for t in &raw.dep_out_tgt {
        buf.extend_from_slice(&t.to_le_bytes());
    }
    for o in &raw.dep_in_off {
        buf.extend_from_slice(&o.to_le_bytes());
    }
    for t in &raw.dep_in_tgt {
        buf.extend_from_slice(&t.to_le_bytes());
    }
    for (src, dst, kind, aux, alive) in &raw.edges {
        buf.extend_from_slice(&src.to_le_bytes());
        buf.extend_from_slice(&dst.to_le_bytes());
        buf.push(*kind);
        buf.extend_from_slice(&aux.to_le_bytes());
        buf.push(*alive);
    }

    // Structural region ends here — its length drives the CRC scopes
    // (catalog + part_index live outside every structural scope).
    let struct_len = buf.len();

    // catalog blob: [ops len][ops][dep len][dep][crc32c]
    buf.extend_from_slice(&(catalog_ops.len() as u64).to_le_bytes());
    buf.extend_from_slice(catalog_ops);
    buf.extend_from_slice(&(catalog_deprecations.len() as u64).to_le_bytes());
    buf.extend_from_slice(catalog_deprecations);
    let cat_crc = crc32c_of(&cat_bytes(catalog_ops, catalog_deprecations));
    buf.extend_from_slice(&cat_crc.to_le_bytes());

    // part_index: count = 0 in M1 (T-107 extends the format additively).
    buf.extend_from_slice(&0u64.to_le_bytes());

    // footer: [count u64][block CRCs of the STRUCTURAL region][total CRC32C
    // over structural region + table][magic tail]. Catalog bytes are outside
    // every structural scope — their own CRC governs survivability (SFT-3.2).
    let blocks = struct_len.div_ceil(BLOCK);
    let mut table = Vec::with_capacity(8 + blocks * 4);
    table.extend_from_slice(&(blocks as u64).to_le_bytes());
    for i in 0..blocks {
        let start = i * BLOCK;
        let chunk = &buf[start..((i + 1) * BLOCK).min(struct_len)];
        table.extend_from_slice(&crc32c_of(chunk).to_le_bytes());
    }
    let mut scope = Vec::with_capacity(struct_len + table.len());
    scope.extend_from_slice(&buf[..struct_len]);
    scope.extend_from_slice(&table);
    let total = crc32c_of(&scope);
    buf.extend_from_slice(&table);
    buf.extend_from_slice(&total.to_le_bytes());
    buf.extend_from_slice(MAGIC);
    buf
}
fn cat_bytes(ops: &[u8], deps: &[u8]) -> Vec<u8> {
    let mut c = Vec::with_capacity(16 + ops.len() + deps.len());
    c.extend_from_slice(&(ops.len() as u64).to_le_bytes());
    c.extend_from_slice(ops);
    c.extend_from_slice(&(deps.len() as u64).to_le_bytes());
    c.extend_from_slice(deps);
    c
}

/// Write a snapshot to `path` (fsync'd — immutable once MANIFEST-listed).
pub fn write_snapshot(path: &Path, payload: &[u8]) -> Result<(), SnapshotError> {
    let mut f = File::create(path)?;
    f.write_all(payload)?;
    f.sync_data()?;
    Ok(())
}

/// Sequential reader over snapshot bytes (explicit bounds, explicit errors).
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], SnapshotError> {
        if self.pos + n > self.data.len() {
            return Err(SnapshotError::Truncated {
                need: self.pos + n,
                have: self.data.len(),
            });
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, SnapshotError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, SnapshotError> {
        Ok(u16::from_le_bytes(le_bytes(&self.take(2)?)))
    }
    fn u32(&mut self) -> Result<u32, SnapshotError> {
        Ok(u32::from_le_bytes(le_bytes(&self.take(4)?)))
    }
    fn u64(&mut self) -> Result<u64, SnapshotError> {
        Ok(u64::from_le_bytes(le_bytes(&self.take(8)?)))
    }
    fn u64_vec(&mut self, n: usize) -> Result<Vec<u64>, SnapshotError> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.u64()?);
        }
        Ok(out)
    }
    fn u32_vec(&mut self, n: usize) -> Result<Vec<u32>, SnapshotError> {
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(self.u32()?);
        }
        Ok(out)
    }
}

/// Read and verify a snapshot. Structural corruption refuses; catalog CRC
/// failure degrades to `catalog_valid = false` (SFT-3.2/3.3).
pub fn read_snapshot(path: &Path) -> Result<SnapshotData, SnapshotError> {
    let mut data = Vec::new();
    File::open(path)?.read_to_end(&mut data)?;

    if data.len() < HEADER_LEN + 8 + 4 + 8 {
        return Err(SnapshotError::Truncated {
            need: HEADER_LEN + 8 + 4 + 8,
            have: data.len(),
        });
    }
    if &data[..8] != MAGIC {
        return Err(SnapshotError::NotASnapshot {
            reason: "bad magic".into(),
        });
    }
    let tail = data.len() - 8;
    if &data[tail..] != MAGIC {
        return Err(SnapshotError::NotASnapshot {
            reason: "bad magic tail".into(),
        });
    }
    let _tail_check = data.len() - 8; // footer verified after the arrays parse

    // header fields
    let version = u32::from_le_bytes(le_bytes(&data[8..12]));
    if version != FORMAT_VERSION {
        return Err(SnapshotError::NotASnapshot {
            reason: format!("version {version}"),
        });
    }
    let database_id = u32::from_le_bytes(le_bytes(&data[12..16]));
    let v = u64::from_le_bytes(le_bytes(&data[16..24])) as usize;
    let e = u64::from_le_bytes(le_bytes(&data[24..32])) as usize;
    let schema_epoch = u64::from_le_bytes(le_bytes(&data[32..40]));
    let created_lsn = u64::from_le_bytes(le_bytes(&data[40..48]));

    // structural arrays (SPEC-03 §3 order)
    let mut c = Cursor {
        data: &data,
        pos: HEADER_LEN,
    };
    let mut alive = Vec::with_capacity(v);
    let mut parent = Vec::with_capacity(v);
    let mut graph = Vec::with_capacity(v);
    let mut node_type = Vec::with_capacity(v);
    let mut state = Vec::with_capacity(v);
    let mut epoch = Vec::with_capacity(v);
    let mut attr_count = Vec::with_capacity(v);
    for _ in 0..v {
        alive.push(c.u8()?);
    }
    for _ in 0..v {
        parent.push(c.u32()?);
    }
    for _ in 0..v {
        graph.push(c.u16()?);
    }
    for _ in 0..v {
        node_type.push(c.u16()?);
    }
    for _ in 0..v {
        state.push(c.u8()?);
    }
    for _ in 0..v {
        epoch.push(c.u64()?);
    }
    for _ in 0..v {
        attr_count.push(c.u16()?);
    }
    let child_off = c.u64_vec(v + 1)?;
    let child_tgt = c.u32_vec(child_off.last().copied().unwrap_or(0) as usize)?;
    let dep_out_off = c.u64_vec(v + 1)?;
    let dep_out_tgt = c.u32_vec(dep_out_off.last().copied().unwrap_or(0) as usize)?;
    let dep_in_off = c.u64_vec(v + 1)?;
    let dep_in_tgt = c.u32_vec(dep_in_off.last().copied().unwrap_or(0) as usize)?;
    let mut edges = Vec::with_capacity(e);
    for _ in 0..e {
        let src = c.u32()?;
        let dst = c.u32()?;
        let kind = c.u8()?;
        let aux = c.u32()?;
        let alive = c.u8()?;
        edges.push((src, dst, kind, aux, alive));
    }

    // The structural region ends here — its length drives the CRC scopes.
    let struct_len = c.pos;

    // catalog blob
    let ops_len = c.u64()? as usize;
    let ops = c.take(ops_len)?.to_vec();
    let dep_len = c.u64()? as usize;
    let deps = c.take(dep_len)?.to_vec();
    let cat_crc = c.u32()?;
    let catalog_valid = crc32c_of(&cat_bytes(&ops, &deps)) == cat_crc;

    // part_index (count only in M1)
    let part_count = c.u64()?;
    if part_count != 0 {
        return Err(SnapshotError::NotASnapshot {
            reason: format!("part entries present but this build reads 0 (got {part_count})"),
        });
    }

    // footer: [count u64][block CRCs — structural scope][total CRC32C]
    let tail = data.len() - 8;
    if &data[tail..] != MAGIC {
        return Err(SnapshotError::NotASnapshot {
            reason: "bad magic tail".into(),
        });
    }
    let total_pos = tail - 4;
    // footer = [count u64][crc × blocks][total u32][magic 8]; `blocks` is
    // known from the structural region — no provisional reads.
    let blocks = struct_len.div_ceil(BLOCK);
    // The CRC table ends exactly at `total_pos` — it starts at
    // `total_pos - 4*blocks`, and the u64 count sits right before it.
    let crc_table_start = total_pos - 4 * blocks;
    let count_pos = crc_table_start - 8;
    if count_pos < HEADER_LEN {
        return Err(SnapshotError::Truncated {
            need: HEADER_LEN,
            have: count_pos,
        });
    }
    let table_count = u64::from_le_bytes(le_bytes(&data[count_pos..count_pos + 8])) as usize;
    if table_count != blocks {
        return Err(SnapshotError::NotASnapshot {
            reason: format!(
                "block table count {table_count} disagrees with structural region ({blocks})"
            ),
        });
    }
    let table_pos = crc_table_start;
    let table_pos = count_pos + 8;
    if blocks != struct_len.div_ceil(BLOCK) {
        return Err(SnapshotError::NotASnapshot {
            reason: format!("block table count {blocks} disagrees with structural region"),
        });
    }
    if count_pos < HEADER_LEN {
        return Err(SnapshotError::Truncated {
            need: HEADER_LEN,
            have: count_pos,
        });
    }
    let mut scope = Vec::with_capacity(struct_len + (total_pos - count_pos));
    scope.extend_from_slice(&data[..struct_len]);
    // Scope = structural region + the whole table (count + CRCs), matching
    // the encoder exactly.
    scope.extend_from_slice(&data[count_pos..total_pos]);
    for i in 0..blocks {
        let start = i * BLOCK;
        let end = ((i + 1) * BLOCK).min(struct_len);
        let expected =
            u32::from_le_bytes(le_bytes(&data[table_pos + i * 4..table_pos + i * 4 + 4]));
        if crc32c_of(&data[start..end]) != expected {
            return Err(SnapshotError::StructureCorrupt {
                block: i as u64,
                offset: start as u64,
                reason: "block CRC mismatch".into(),
            });
        }

        let total_got = crc32c_of(&scope);
        let total_expected = u32::from_le_bytes(le_bytes(&data[total_pos..total_pos + 4]));
        if total_expected != total_got {
            return Err(SnapshotError::TotalCrcMismatch {
                expected: total_expected,
                got: total_got,
            });
        }
    }

    Ok(SnapshotData {
        database_id,
        v: v as u64,
        e: e as u64,
        schema_epoch,
        created_lsn,
        arrays: RawArrays {
            alive,
            parent,
            graph,
            node_type,
            state,
            epoch,
            attr_count,
            child_off,
            child_tgt,
            dep_out_off,
            dep_out_tgt,
            dep_in_off,
            dep_in_tgt,
            edges,
        },
        catalog_ops: ops,
        catalog_deprecations: deps,
        catalog_valid,
    })
}

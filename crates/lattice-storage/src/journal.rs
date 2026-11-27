//! Journal segments (SPEC-03 §6): the only authority (P7).
//!
//! Layout: `journal/<NNNNNNNNNNNN>.log`, 12-digit zero-padded segment
//! numbers (SPEC-03 §9.7). Segment = `[record]*`; record =
//! `[u32 len][u32 crc32c][body]`; body = `[u8 kind][u64 lsn][u64 txn_id]
//! [payload]`; COMMIT payload = `[u64 first_lsn][u64 last_lsn]`.
//!
//! - LSNs are gap-free per Database, assigned by the writer at append.
//! - A txn is valid iff its COMMIT is present and CRC-valid (no torn txns).
//! - Torn tail: a partial/failed record at the end of the ACTIVE segment is
//!   dropped at open (SPEC-03 §9.6); corruption in a SEALED segment is a
//!   `JournalError::Corrupt` refusal (SPEC-03 §8 — the journal is sacred).
//! - Rotation at `max_segment_bytes` (a config parameter — SPEC-02 §10 law;
//!   never a code literal).
//! - Sync = `File::sync_data` (fdatasync on Linux — SPEC-03 §9.8).

use crate::crc32c::crc32c_of;
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// Record kinds (SPEC-03 §6 registry; payload semantics belong to the tx
/// engine — the journal frames and sequences opaque typed payloads).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordKind {
    BeginTxn = 1,
    AddVertex = 2,
    DelVertex = 3,
    AddEdge = 4,
    DelEdge = 5,
    SetAttr = 6,
    AppendCells = 7,
    RowAppendBatch = 8,
    Action = 9,
    SchemaTxn = 10,
    Grant = 11,
    Revoke = 12,
    Commit = 13,
}

impl RecordKind {
    pub fn from_u8(v: u8) -> Option<RecordKind> {
        use RecordKind::*;
        Some(match v {
            1 => BeginTxn,
            2 => AddVertex,
            3 => DelVertex,
            4 => AddEdge,
            5 => DelEdge,
            6 => SetAttr,
            7 => AppendCells,
            8 => RowAppendBatch,
            9 => Action,
            10 => SchemaTxn,
            11 => Grant,
            12 => Revoke,
            13 => Commit,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalRecord {
    pub kind: RecordKind,
    pub lsn: u64,
    pub txn_id: u64,
    pub payload: Vec<u8>,
}

/// One committed transaction as recovered from the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalTxn {
    pub txn_id: u64,
    pub first_lsn: u64,
    pub last_lsn: u64,
    pub records: Vec<JournalRecord>,
}

#[derive(Debug)]
pub enum JournalError {
    Io(io::Error),
    /// Corruption inside a SEALED segment — refuse start (SPEC-03 §8: never
    /// silently healed).
    Corrupt {
        segment: u32,
        offset: u64,
        reason: String,
    },
    /// LSN continuity broken — refuse (gap-free law).
    LsnGap {
        segment: u32,
        expected: u64,
        found: u64,
    },
    /// COMMIT without BEGIN / range mismatch (structural).
    OrphanCommit {
        segment: u32,
        txn_id: u64,
    },
    /// Writer misuse: append outside an open txn / duplicate txn id.
    NoOpenTxn {
        txn_id: u64,
    },
    TxnAlreadyOpen {
        txn_id: u64,
    },
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JournalError::Io(e) => write!(f, "journal i/o: {e}"),
            JournalError::Corrupt {
                segment,
                offset,
                reason,
            } => {
                write!(
                    f,
                    "journal corrupt in segment {segment} at offset {offset}: {reason}"
                )
            }
            JournalError::LsnGap {
                segment,
                expected,
                found,
            } => {
                write!(
                    f,
                    "lsn gap in segment {segment}: expected {expected}, found {found}"
                )
            }
            JournalError::OrphanCommit { segment, txn_id } => {
                write!(f, "commit without begin: txn {txn_id} in segment {segment}")
            }
            JournalError::NoOpenTxn { txn_id } => {
                write!(f, "append for txn {txn_id} without begin_txn")
            }
            JournalError::TxnAlreadyOpen { txn_id } => write!(f, "txn {txn_id} already open"),
        }
    }
}

impl std::error::Error for JournalError {}

impl From<io::Error> for JournalError {
    fn from(e: io::Error) -> Self {
        JournalError::Io(e)
    }
}

fn segment_name(no: u32) -> String {
    format!("{no:012}.log")
}

fn segment_path(dir: &Path, no: u32) -> PathBuf {
    dir.join(segment_name(no))
}

/// Segment numbers present in `dir` (parsed from the 12-digit convention).
fn list_segments(dir: &Path) -> Result<Vec<u32>, JournalError> {
    let mut segs = Vec::new();
    for entry in fs::read_dir(dir)? {
        let name = entry?.file_name();
        let name = name.to_string_lossy();
        if let Some(stem) = name.strip_suffix(".log")
            && stem.len() == 12
            && stem.chars().all(|c| c.is_ascii_digit())
        {
            segs.push(stem.parse::<u32>().map_err(|_| JournalError::Corrupt {
                segment: 0,
                offset: 0,
                reason: format!("segment number out of range: {name}"),
            })?);
        }
    }
    Ok(segs)
}

fn frame(kind: RecordKind, lsn: u64, txn_id: u64, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(17 + payload.len());
    body.push(kind as u8);
    body.extend_from_slice(&lsn.to_le_bytes());
    body.extend_from_slice(&txn_id.to_le_bytes());
    body.extend_from_slice(payload);
    let mut f = Vec::with_capacity(8 + body.len());
    f.extend_from_slice(&(body.len() as u32).to_le_bytes());
    f.extend_from_slice(&crc32c_of(&body).to_le_bytes());
    f.extend_from_slice(&body);
    f
}

/// Recovery summary (SPEC-03 §6 recovery scan).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RecoveryInfo {
    pub txns_applied: usize,
    pub last_lsn: u64,
    pub segments: u32,
    /// Bytes dropped from the active segment's torn tail (0 = none).
    pub torn_tail_dropped: u64,
    /// Highest segment number (0 when the journal is empty).
    pub highest_segment: u32,
}

struct TxnBuilder {
    first_lsn: u64,
    records: Vec<JournalRecord>,
}

/// Replay a journal directory in order, invoking `on_txn` per committed txn.
///
/// Sealed-segment corruption refuses (`Corrupt`); the active segment's torn
/// tail is dropped (bytes reported in `torn_tail_dropped`). No file is
/// modified — `JournalWriter::open` truncates the tail before continuing
/// (SFT-6.4).
pub fn recover(
    dir: &Path,
    mut on_txn: impl FnMut(&JournalTxn),
) -> Result<RecoveryInfo, JournalError> {
    let mut segs = list_segments(dir)?;
    if segs.is_empty() {
        return Ok(RecoveryInfo::default());
    }
    segs.sort_unstable();
    let active = segs[segs.len() - 1];

    let mut info = RecoveryInfo {
        highest_segment: active,
        segments: segs.len() as u32,
        ..Default::default()
    };
    let mut next_lsn: u64 = 1;
    let mut open: BTreeMap<u64, TxnBuilder> = BTreeMap::new();
    let mut torn = false;
    let mut valid_end_active: u64 = 0;

    'segments: for seg in segs {
        let mut data = Vec::new();
        File::open(segment_path(dir, seg))?.read_to_end(&mut data)?;
        let mut off: usize = 0;
        while off + 8 <= data.len() {
            let len = u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
            let over_cap = len > 64 * 1024 * 1024;
            let truncated = off + 8 + len as usize > data.len();
            if over_cap || truncated {
                if seg == active {
                    torn = true;
                    break 'segments;
                }
                return Err(JournalError::Corrupt {
                    segment: seg,
                    offset: off as u64,
                    reason: if over_cap {
                        format!("frame length {len} exceeds cap")
                    } else {
                        "truncated frame".into()
                    },
                });
            }
            let body = &data[off + 8..off + 8 + len as usize];
            let crc =
                u32::from_le_bytes([data[off + 4], data[off + 5], data[off + 6], data[off + 7]]);
            if crc32c_of(body) != crc {
                if seg == active {
                    torn = true;
                    break 'segments;
                }
                return Err(JournalError::Corrupt {
                    segment: seg,
                    offset: off as u64,
                    reason: "crc mismatch".into(),
                });
            }
            let kind = match RecordKind::from_u8(body[0]) {
                Some(k) => k,
                None => {
                    if seg == active {
                        torn = true;
                        break 'segments;
                    }
                    return Err(JournalError::Corrupt {
                        segment: seg,
                        offset: off as u64,
                        reason: format!("unknown record kind {}", body[0]),
                    });
                }
            };
            let lsn = u64::from_le_bytes([
                body[1], body[2], body[3], body[4], body[5], body[6], body[7], body[8],
            ]);
            let txn_id = u64::from_le_bytes([
                body[9], body[10], body[11], body[12], body[13], body[14], body[15], body[16],
            ]);
            let payload = body[17..].to_vec();
            if lsn != next_lsn {
                return Err(JournalError::LsnGap {
                    segment: seg,
                    expected: next_lsn,
                    found: lsn,
                });
            }
            next_lsn += 1;
            match kind {
                RecordKind::BeginTxn => {
                    if open
                        .insert(
                            txn_id,
                            TxnBuilder {
                                first_lsn: lsn,
                                records: Vec::new(),
                            },
                        )
                        .is_some()
                    {
                        return Err(JournalError::TxnAlreadyOpen { txn_id });
                    }
                }
                RecordKind::Commit => {
                    if payload.len() != 16 {
                        return Err(JournalError::Corrupt {
                            segment: seg,
                            offset: off as u64,
                            reason: "commit payload must be [first_lsn][last_lsn]".into(),
                        });
                    }
                    let first = u64::from_le_bytes([
                        payload[0], payload[1], payload[2], payload[3], payload[4], payload[5],
                        payload[6], payload[7],
                    ]);
                    let last = u64::from_le_bytes([
                        payload[8],
                        payload[9],
                        payload[10],
                        payload[11],
                        payload[12],
                        payload[13],
                        payload[14],
                        payload[15],
                    ]);
                    let mut b = open.remove(&txn_id).ok_or(JournalError::OrphanCommit {
                        segment: seg,
                        txn_id,
                    })?;
                    if b.first_lsn != first || lsn != last {
                        return Err(JournalError::Corrupt {
                            segment: seg,
                            offset: off as u64,
                            reason: format!(
                                "commit range mismatch: txn {txn_id} declared [{first},{last}] but spans [{},{}]",
                                b.first_lsn, lsn
                            ),
                        });
                    }
                    b.records.push(JournalRecord {
                        kind,
                        lsn,
                        txn_id,
                        payload,
                    });
                    let txn = JournalTxn {
                        txn_id,
                        first_lsn: b.first_lsn,
                        last_lsn: lsn,
                        records: b.records,
                    };
                    info.txns_applied += 1;
                    on_txn(&txn);
                }
                _ => match open.get_mut(&txn_id) {
                    Some(b) => b.records.push(JournalRecord {
                        kind,
                        lsn,
                        txn_id,
                        payload,
                    }),
                    None => {
                        return Err(JournalError::Corrupt {
                            segment: seg,
                            offset: off as u64,
                            reason: format!("record for unknown txn {txn_id}"),
                        });
                    }
                },
            }
            off += 8 + len as usize;
            if seg == active {
                valid_end_active = off as u64;
            }
        }
        if seg == active && off != data.len() {
            // Trailing bytes that never formed a frame header (< 8 bytes) —
            // a torn tail on the active segment.
            torn = true;
        }
        if seg != active && off != data.len() {
            return Err(JournalError::Corrupt {
                segment: seg,
                offset: off as u64,
                reason: "trailing bytes in sealed segment".into(),
            });
        }
    }

    if torn {
        // The active segment's file carries the torn tail: valid prefix is
        // everything before the first bad frame.
        let active_len = fs::metadata(segment_path(dir, active))?.len();
        info.torn_tail_dropped = active_len.saturating_sub(valid_end_active);
    }
    info.last_lsn = next_lsn - 1;
    let _ = open; // unclosed txns are torn by definition — discarded
    Ok(info)
}

/// Append-only journal writer over a segment directory. LSN assignment is
/// gap-free; rotation honors the configured `max_segment_bytes` (never a
/// code literal — SPEC-02 §10).
pub struct JournalWriter {
    dir: PathBuf,
    file: File,
    segment: u32,
    segment_bytes: u64,
    max_segment_bytes: u64,
    next_lsn: u64,
    open_txns: BTreeMap<u64, u64>, // txn_id -> first_lsn
}

impl JournalWriter {
    /// Create a fresh journal at `dir` (must be empty or absent).
    pub fn create(dir: &Path, max_segment_bytes: u64) -> Result<Self, JournalError> {
        fs::create_dir_all(dir)?;
        let segs = list_segments(dir)?;
        if !segs.is_empty() {
            return Err(JournalError::Corrupt {
                segment: 0,
                offset: 0,
                reason: format!("create on non-empty journal (segment {} exists)", segs[0]),
            });
        }
        let segment = 1;
        let file = File::create(segment_path(dir, segment))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            file,
            segment,
            segment_bytes: 0,
            max_segment_bytes,
            next_lsn: 1,
            open_txns: BTreeMap::new(),
        })
    }

    /// Open an existing journal: recover (dropping the torn tail from the
    /// active segment file), then continue appending with gap-free LSNs
    /// (SFT-6.4).
    pub fn open(dir: &Path, max_segment_bytes: u64) -> Result<(Self, RecoveryInfo), JournalError> {
        let info = recover(dir, |_| {})?;
        let segment = if info.highest_segment == 0 {
            1
        } else {
            info.highest_segment
        };
        fs::create_dir_all(dir)?;
        if info.torn_tail_dropped > 0 {
            let path = segment_path(dir, segment);
            let valid = fs::metadata(&path)?.len() - info.torn_tail_dropped;
            let f = OpenOptions::new().write(true).open(&path)?;
            f.set_len(valid)?;
            f.sync_data()?;
        }
        let file = OpenOptions::new()
            .append(true)
            .open(segment_path(dir, segment))?;
        let segment_bytes = fs::metadata(segment_path(dir, segment))?.len();
        let writer = Self {
            dir: dir.to_path_buf(),
            file,
            segment,
            segment_bytes,
            max_segment_bytes,
            next_lsn: info.last_lsn + 1,
            open_txns: BTreeMap::new(),
        };
        Ok((writer, info))
    }

    pub fn segment(&self) -> u32 {
        self.segment
    }

    pub fn next_lsn(&self) -> u64 {
        self.next_lsn
    }

    fn rotate_if_needed(&mut self, incoming: u64) -> Result<(), JournalError> {
        if self.segment_bytes + incoming > self.max_segment_bytes {
            self.file.flush()?;
            self.file.sync_data()?;
            self.segment += 1;
            self.file = File::create(segment_path(&self.dir, self.segment))?;
            self.segment_bytes = 0;
        }
        Ok(())
    }

    fn write_frame(
        &mut self,
        kind: RecordKind,
        txn_id: u64,
        payload: &[u8],
    ) -> Result<u64, JournalError> {
        let lsn = self.next_lsn;
        let f = frame(kind, lsn, txn_id, payload);
        self.rotate_if_needed(f.len() as u64)?;
        self.file.write_all(&f)?;
        self.segment_bytes += f.len() as u64;
        self.next_lsn += 1;
        Ok(lsn)
    }

    /// Open a transaction: BEGIN_TXN is journaled; the returned LSN is the
    /// txn's first.
    pub fn begin_txn(&mut self, txn_id: u64) -> Result<u64, JournalError> {
        if self.open_txns.contains_key(&txn_id) {
            return Err(JournalError::TxnAlreadyOpen { txn_id });
        }
        let first = self.write_frame(RecordKind::BeginTxn, txn_id, &[])?;
        self.open_txns.insert(txn_id, first);
        Ok(first)
    }

    /// Append one record into an open txn; returns its LSN.
    pub fn append(
        &mut self,
        txn_id: u64,
        kind: RecordKind,
        payload: &[u8],
    ) -> Result<u64, JournalError> {
        if kind == RecordKind::BeginTxn || kind == RecordKind::Commit {
            return Err(JournalError::NoOpenTxn { txn_id }); // misuse: use begin/commit
        }
        if !self.open_txns.contains_key(&txn_id) {
            return Err(JournalError::NoOpenTxn { txn_id });
        }
        self.write_frame(kind, txn_id, payload)
    }

    /// Commit an open txn: journals COMMIT with the verified LSN range and
    /// returns `(first_lsn, last_lsn)`. The COMMIT record's own LSN is
    /// pre-reserved so the frame can carry the full `[first][last]` range.
    pub fn commit_txn(&mut self, txn_id: u64) -> Result<(u64, u64), JournalError> {
        let first = self
            .open_txns
            .remove(&txn_id)
            .ok_or(JournalError::NoOpenTxn { txn_id })?;
        let last = self.next_lsn; // deterministic: no LSN is consumed before the frame
        let mut payload = Vec::with_capacity(16);
        payload.extend_from_slice(&first.to_le_bytes());
        payload.extend_from_slice(&last.to_le_bytes());
        let written = self.write_frame(RecordKind::Commit, txn_id, &payload)?;
        debug_assert_eq!(written, last, "pre-reserved commit LSN must not drift");
        Ok((first, written))
    }

    /// fdatasync the active segment (group-commit boundary — SPEC-03 §9.8).
    pub fn sync(&mut self) -> Result<(), JournalError> {
        self.file.flush()?;
        self.file.sync_data()?;
        Ok(())
    }
}

//! Wire framing (SPEC-02 §2, T-301): `[u8 type][u32 len][payload]`, LE
//! everywhere, `read_exact` discipline (no incremental parsing), TLV
//! payloads `[u16 tag][u32 len][value]`.
//!
//! - `len` bounds: 0..max_frame (default 16 MiB, absolute cap 256 MiB).
//!   Over-bound ⇒ `FRAME_TOO_LARGE`, connection closed (the stream is
//!   desynced by definition).
//! - Every request carries `request_id` (TLV tag 0x0001, mandatory); every
//!   reply echoes it — pipelining correlation (WPT-2.3).
//! - The message type registry is CLOSED (additive via minor versions only).

use std::fmt;

/// Message type registry (SPEC-02 §2, closed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    // control 0x00–0x0F
    Hello,
    Params,
    Auth,
    Ready,
    Ping,
    Pong,
    Cancel,
    Bye,
    Error,
    Resume,
    // transaction 0x10–0x1F
    TxnBegin,
    TxnCommit,
    TxnRollback,
    // write 0x20–0x2F
    CreateNode,
    DeleteNode,
    Link,
    Unlink,
    AttrSet,
    RowAppend,
    CellsAppend,
    Action,
    Grant,
    Revoke,
    // read/meta 0x30–0x3F
    Snapshot,
    NodeRead,
    CursorOpen,
    CursorFetch,
    CursorClose,
    Blockers,
    History,
    SchemaPlan,
    SchemaTxn,
    // results 0x40–0x4F
    Ack,
    DataArrow,
    Event,
    // watch 0x50–0x5F
    WatchSub,
    WatchUnsub,
}

impl FrameType {
    pub fn byte(self) -> u8 {
        match self {
            FrameType::Hello => 0x01,
            FrameType::Params => 0x02,
            FrameType::Auth => 0x03,
            FrameType::Ready => 0x05,
            FrameType::Ping => 0x06,
            FrameType::Pong => 0x07,
            FrameType::Cancel => 0x08,
            FrameType::Bye => 0x09,
            FrameType::Error => 0x0A,
            FrameType::Resume => 0x0B,
            FrameType::TxnBegin => 0x10,
            FrameType::TxnCommit => 0x11,
            FrameType::TxnRollback => 0x12,
            FrameType::CreateNode => 0x20,
            FrameType::DeleteNode => 0x21,
            FrameType::Link => 0x22,
            FrameType::Unlink => 0x23,
            FrameType::AttrSet => 0x24,
            FrameType::RowAppend => 0x25,
            FrameType::CellsAppend => 0x26,
            FrameType::Action => 0x27,
            FrameType::Grant => 0x28,
            FrameType::Revoke => 0x29,
            FrameType::Snapshot => 0x30,
            FrameType::NodeRead => 0x31,
            FrameType::CursorOpen => 0x32,
            FrameType::CursorFetch => 0x33,
            FrameType::CursorClose => 0x34,
            FrameType::Blockers => 0x35,
            FrameType::History => 0x36,
            FrameType::SchemaPlan => 0x37,
            FrameType::SchemaTxn => 0x38,
            FrameType::Ack => 0x40,
            FrameType::DataArrow => 0x41,
            FrameType::Event => 0x42,
            FrameType::WatchSub => 0x50,
            FrameType::WatchUnsub => 0x51,
        }
    }

    pub fn from_byte(b: u8) -> Option<FrameType> {
        Some(match b {
            0x01 => FrameType::Hello,
            0x02 => FrameType::Params,
            0x03 => FrameType::Auth,
            0x05 => FrameType::Ready,
            0x06 => FrameType::Ping,
            0x07 => FrameType::Pong,
            0x08 => FrameType::Cancel,
            0x09 => FrameType::Bye,
            0x0A => FrameType::Error,
            0x0B => FrameType::Resume,
            0x10 => FrameType::TxnBegin,
            0x11 => FrameType::TxnCommit,
            0x12 => FrameType::TxnRollback,
            0x20 => FrameType::CreateNode,
            0x21 => FrameType::DeleteNode,
            0x22 => FrameType::Link,
            0x23 => FrameType::Unlink,
            0x24 => FrameType::AttrSet,
            0x25 => FrameType::RowAppend,
            0x26 => FrameType::CellsAppend,
            0x27 => FrameType::Action,
            0x28 => FrameType::Grant,
            0x29 => FrameType::Revoke,
            0x30 => FrameType::Snapshot,
            0x31 => FrameType::NodeRead,
            0x32 => FrameType::CursorOpen,
            0x33 => FrameType::CursorFetch,
            0x34 => FrameType::CursorClose,
            0x35 => FrameType::Blockers,
            0x36 => FrameType::History,
            0x37 => FrameType::SchemaPlan,
            0x38 => FrameType::SchemaTxn,
            0x40 => FrameType::Ack,
            0x41 => FrameType::DataArrow,
            0x42 => FrameType::Event,
            0x50 => FrameType::WatchSub,
            0x51 => FrameType::WatchUnsub,
            _ => return None,
        })
    }
}

/// Default negotiated max frame (16 MiB); absolute cap 256 MiB (§2).
pub const DEFAULT_MAX_FRAME: u64 = 16 * 1024 * 1024;
pub const ABSOLUTE_MAX_FRAME: u64 = 256 * 1024 * 1024;
const HEADER_LEN: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    /// FRAME_TOO_LARGE — connection closed, no recovery (§2).
    FrameTooLarge {
        len: u64,
        max: u64,
    },
    Truncated {
        need: usize,
        have: usize,
    },
    UnknownType(u8),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FrameError::FrameTooLarge { len, max } => {
                write!(
                    f,
                    "FRAME_TOO_LARGE: frame {len} exceeds negotiated max {max}"
                )
            }
            FrameError::Truncated { need, have } => {
                write!(f, "frame truncated: need {need}, have {have}")
            }
            FrameError::UnknownType(b) => write!(f, "unknown frame type {b:#04x}"),
        }
    }
}

impl std::error::Error for FrameError {}

/// A parsed frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub frame_type: FrameType,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(frame_type: FrameType, payload: Vec<u8>) -> Self {
        Self {
            frame_type,
            payload,
        }
    }

    /// Encode: `[u8 type][u32 len LE][payload]`.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + self.payload.len());
        out.push(self.frame_type.byte());
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.payload);
        out
    }
}

/// Frame-level length check (FRAME_TOO_LARGE before any body read — the
/// stream is desynced by an over-bound frame, so the header alone decides).
pub fn check_frame_bound(header: &[u8; HEADER_LEN], max_frame: u64) -> Result<u32, FrameError> {
    let Some(frame_type) = FrameType::from_byte(header[0]) else {
        return Err(FrameError::UnknownType(header[0]));
    };
    let _ = frame_type;
    let len = u32::from_le_bytes(le_bytes(&header[1..5])) as u64;
    if len > max_frame {
        return Err(FrameError::FrameTooLarge {
            len,
            max: max_frame,
        });
    }
    Ok(len as u32)
}

/// Parse a full frame body (read_exact discipline: caller supplies exactly
/// the announced payload; short bodies are stream desyncs).
pub fn parse_frame(frame_type_byte: u8, payload: Vec<u8>) -> Result<Frame, FrameError> {
    let frame_type =
        FrameType::from_byte(frame_type_byte).ok_or(FrameError::UnknownType(frame_type_byte))?;
    Ok(Frame {
        frame_type,
        payload,
    })
}

fn le_bytes<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(src);
    out
}

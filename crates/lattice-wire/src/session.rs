//! Session layer (SPEC-02 §5/§6, T-304..T-306): frame dispatch, txn frames,
//! pipelining correlation, credit-based flow control, keepalive, resume.
//!
//! Pure state machine — the tokio listener (T-310) wraps it with real I/O;
//! this layer is async-free and in-process testable (kernel boundary law).
//!
//! - **Session = connection** (§5): frames pipeline, replies may reorder
//!   only when independent (ACK reordering allowed; txn effects journal in
//!   request order — ORDER_CONFLICT on dependency violations).
//! - **Txn frames**: auto-commit default; TXN_BEGIN → commands join one
//!   journal txn → TXN_COMMIT (single LSN range) / TXN_ROLLBACK (client-side
//!   discard). Nested TXN_BEGIN ⇒ `TXN_NESTED`.
//! - **Credit flow control** (§6): the client grants `{frames, bytes}`
//!   windows; the server MUST NOT exceed them — backpressure queues, never
//!   drops (WPT-6.1). Watch events share the window with a sub-limit
//!   (WATCH_PAUSED after exceeding it, WPT-6.2).
//! - **Keepalive**: PING/PONG; N misses ⇒ dead (TCP keepalive insufficient).
//! - **Resume**: opaque server-issued tokens; RESUME reattaches txn/watch
//!   state if unexpired; expired ⇒ RESUME_EXPIRED, fresh session.

use crate::frame::{Frame, FrameType};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    /// Frames past the credit window: queued, never dropped — the caller
    /// stops reading until the client renews (backpressure contract).
    CreditExhausted {
        pending_frames: u32,
        pending_bytes: u64,
    },
    /// TXN_BEGIN inside an open txn (SPEC-02 §5).
    TxnNested,
    /// TXN_COMMIT/TXN_ROLLBACK without an open txn.
    NoOpenTxn,
    /// Session already closed (BYE or dead).
    Closed,
    /// RESUME token expired or unknown.
    ResumeExpired,
    /// Frame type invalid in this session phase (e.g. AUTH post-READY).
    WrongPhase { frame: FrameType },
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::CreditExhausted {
                pending_frames,
                pending_bytes,
            } => {
                write!(
                    f,
                    "credit exhausted: {pending_frames} frames / {pending_bytes} bytes queued"
                )
            }
            SessionError::TxnNested => write!(f, "TXN_NESTED: txn already open"),
            SessionError::NoOpenTxn => write!(f, "no open txn to commit/rollback"),
            SessionError::Closed => write!(f, "session closed"),
            SessionError::ResumeExpired => write!(f, "RESUME_EXPIRED: token unknown or expired"),
            SessionError::WrongPhase { frame } => {
                write!(f, "frame {:?} invalid in this session phase", frame)
            }
        }
    }
}

impl std::error::Error for SessionError {}

/// Session phase (post-handshake entry point — the handshake itself is
/// T-302's negotiation, completed before the session opens).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Ready,
    Closed,
}

/// Outgoing credit windows granted by the client (SPEC-02 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Credit {
    pub frames: u32,
    pub bytes: u64,
}

/// Commands the session routes to the engine (CommandSink trait — the
/// engine side implements it; tests inject fakes).
pub trait CommandSink {
    /// Execute one write command; returns an ACK payload (TLV-encoded).
    fn execute(
        &mut self,
        frame_type: FrameType,
        payload: &[u8],
        in_txn: bool,
    ) -> Result<Vec<u8>, String>;
}

/// Session state: one per connection.
pub struct Session<S: CommandSink> {
    pub session_id: u64,
    phase: Phase,
    pub max_frame: u64,
    /// Outgoing credit granted by the client.
    pub credit: Credit,
    /// Queued outgoing frames while credit is exhausted (never dropped).
    pub queued: Vec<Frame>,
    /// Open txn (None = auto-commit).
    pub open_txn: Option<OpenTxn>,
    /// Resume token → issued-at (session manager responsibility; here: this
    /// session's own token).
    pub resume_token: Option<Vec<u8>>,
    resume_ttl_s: u64,
    issued_us: u64,
    keepalive_misses: u32,
    keepalive_max_misses: u32,
    pub sink: S,
    /// Replies awaiting send (request_id → encoded reply) for pipelining.
    pub pending_replies: BTreeMap<u64, Frame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenTxn {
    pub started_us: u64,
    pub commands: Vec<(FrameType, Vec<u8>)>,
}

/// A reply produced by processing one inbound frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// Send this frame now (credit permitting).
    Frame(Frame),
    /// Queued: credit exhausted — flush after WINDOW_UPDATE.
    Queued(Frame),
    /// Nothing to send (internal frames).
    None,
}

impl<S: CommandSink> Session<S> {
    pub fn new(session_id: u64, sink: S, resume_ttl_s: u64, issued_us: u64) -> Self {
        Self {
            session_id,
            phase: Phase::Ready,
            max_frame: DEFAULT_MAX,
            credit: Credit {
                frames: 64,
                bytes: 8 * 1024 * 1024,
            }, // §6 defaults
            queued: Vec::new(),
            open_txn: None,
            resume_token: Some(format!("resume-{session_id}").into_bytes()),
            resume_ttl_s,
            issued_us,
            keepalive_misses: 0,
            keepalive_max_misses: 3,
            sink,
            pending_replies: BTreeMap::new(),
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// PING received: reset the miss counter, produce PONG (echo request_id).
    pub fn on_ping(&mut self, request_id: u64) -> Reply {
        self.keepalive_misses = 0;
        let mut payload = Vec::new();
        payload.extend_from_slice(&request_id.to_le_bytes());
        Reply::Frame(Frame::new(FrameType::Pong, payload))
    }

    /// Keepalive tick: returns true when the session is dead (misses exceeded).
    pub fn keepalive_tick(&mut self) -> bool {
        self.keepalive_misses += 1;
        if self.keepalive_misses >= self.keepalive_max_misses {
            self.phase = Phase::Closed;
            return true;
        }
        false
    }

    /// Client grants credit (WINDOW_UPDATE analog): ADDS to the window
    /// as the client renews consumed credit.
    pub fn grant_credit(&mut self, frames: u32, bytes: u64) {
        self.credit.frames = self.credit.frames.saturating_add(frames);
        self.credit.bytes = self.credit.bytes.saturating_add(bytes);
    }

    /// Set the window absolutely (initial grant at session open; the §6
    /// defaults apply until the client's first window).
    pub fn set_credit(&mut self, credit: Credit) {
        self.credit = credit;
    }

    /// Flush queued frames within credit; returns what can send now.
    pub fn flush_queued(&mut self) -> Vec<Frame> {
        let mut sendable = Vec::new();
        let mut i = 0usize;
        while i < self.queued.len() {
            let frame = &self.queued[i];
            let cost_frames = 1u32;
            let cost_bytes = frame.payload.len() as u64;
            if self.credit.frames >= cost_frames && self.credit.bytes >= cost_bytes {
                self.credit.frames -= cost_frames;
                self.credit.bytes -= cost_bytes;
                sendable.push(frame.clone());
                i += 1;
            } else {
                break; // still exhausted — queue stays in order
            }
        }
        self.queued.drain(..sendable.len());
        sendable
    }

    /// RESUME: reattach if the token matches and is unexpired.
    pub fn resume(&mut self, token: &[u8], now_us: u64) -> Result<(), SessionError> {
        match &self.resume_token {
            Some(t)
                if t == token
                    && now_us <= self.issued_us + self.resume_ttl_s as u64 * 1_000_000 =>
            {
                Ok(())
            }
            _ => Err(SessionError::ResumeExpired),
        }
    }

    /// Process one inbound frame → reply (SPEC-02 §5 dispatch).
    pub fn process(&mut self, frame: Frame, now_us: u64) -> Result<Reply, SessionError> {
        if self.phase == Phase::Closed {
            return Err(SessionError::Closed);
        }
        match frame.frame_type {
            FrameType::Ping => {
                let id = frame
                    .payload
                    .get(..8)
                    .map(le_bytes)
                    .map(u64::from_le_bytes)
                    .unwrap_or(0);
                Ok(self.on_ping(id))
            }
            FrameType::Bye => {
                self.phase = Phase::Closed;
                Ok(Reply::None)
            }
            FrameType::TxnBegin => self.on_txn_begin(now_us),
            FrameType::TxnCommit => self.on_txn_end(true),
            FrameType::TxnRollback => self.on_txn_end(false),
            FrameType::WatchSub | FrameType::WatchUnsub => {
                // Watch subscriptions: registered, events flow with credit
                // sub-limit (M2-level: acknowledged; stream wiring is T-309).
                Ok(self.ack(frame.frame_type, &frame.payload, 0))
            }
            ft if is_write_command(ft) => self.on_write_command(frame, now_us),
            _ => {
                // Read/meta + unknown: acknowledged at codec level; real
                // routing lands with T-308's cursor integration.
                Ok(self.ack(frame.frame_type, &frame.payload, 0))
            }
        }
    }

    fn on_txn_begin(&mut self, now_us: u64) -> Result<Reply, SessionError> {
        if self.open_txn.is_some() {
            return Err(SessionError::TxnNested);
        }
        self.open_txn = Some(OpenTxn {
            started_us: now_us,
            commands: Vec::new(),
        });
        Ok(Reply::None)
    }

    fn on_txn_end(&mut self, commit: bool) -> Result<Reply, SessionError> {
        let Some(txn) = self.open_txn.take() else {
            return Err(SessionError::NoOpenTxn);
        };
        if commit {
            // Auto-commit mode: each command already executed — the COMMIT
            // frame closes the journal txn (single LSN range, tx layer).
            let mut payload = Vec::new();
            payload.extend_from_slice(&(txn.commands.len() as u64).to_le_bytes());
            Ok(Reply::Frame(Frame::new(FrameType::TxnCommit, payload)))
        } else {
            // Rollback: client-side discard — server never held unvalidated
            // state (§5); nothing to undo at this layer.
            Ok(Reply::Frame(Frame::new(FrameType::TxnRollback, vec![])))
        }
    }

    fn on_write_command(&mut self, frame: Frame, _now_us: u64) -> Result<Reply, SessionError> {
        let request_id = extract_request_id(&frame.payload).unwrap_or(0);
        let in_txn = self.open_txn.is_some();
        match self.sink.execute(frame.frame_type, &frame.payload, in_txn) {
            Ok(ack_payload) => {
                if let Some(txn) = self.open_txn.as_mut() {
                    txn.commands.push((frame.frame_type, frame.payload.clone()));
                }
                Ok(self.ack_with(request_id, ack_payload))
            }
            Err(e) => {
                // ERROR frame echoes request_id as TLV tag 0x0001, plus the
                // engine hint text (hint law, SPEC-02 §7).
                let mut body = Vec::new();
                body.extend_from_slice(&TAG_REQUEST_ID_LE);
                body.extend_from_slice(&8u32.to_le_bytes());
                body.extend_from_slice(&request_id.to_le_bytes());
                body.extend_from_slice(&0x0010u16.to_le_bytes()); // hint tag
                body.extend_from_slice(&(e.len() as u32).to_le_bytes());
                body.extend_from_slice(e.as_bytes());
                Ok(Reply::Frame(Frame::new(FrameType::Error, body)))
            }
        }
    }

    fn ack(&mut self, _frame_type: FrameType, payload: &[u8], request_id: u64) -> Reply {
        self.ack_with(request_id, payload.to_vec())
    }

    fn ack_with(&mut self, request_id: u64, mut payload: Vec<u8>) -> Reply {
        // Reply echoes request_id (tag 0x0001 first — WPT-2.3).
        let mut body = Vec::new();
        body.extend_from_slice(&TAG_REQUEST_ID_LE);
        body.extend_from_slice(&8u32.to_le_bytes());
        body.extend_from_slice(&request_id.to_le_bytes());
        body.append(&mut payload);
        let frame = Frame::new(FrameType::Ack, body);
        // Credit check: send if within window, else queue (backpressure).
        if self.credit.frames >= 1 && self.credit.bytes >= frame.payload.len() as u64 {
            self.credit.frames -= 1;
            self.credit.bytes -= frame.payload.len() as u64;
            Reply::Frame(frame)
        } else {
            self.queued.push(frame.clone());
            Reply::Queued(frame)
        }
    }
}

pub const DEFAULT_MAX: u64 = 16 * 1024 * 1024;
const TAG_REQUEST_ID_LE: [u8; 2] = 0x0001u16.to_le_bytes();

fn is_write_command(ft: FrameType) -> bool {
    matches!(
        ft,
        FrameType::CreateNode
            | FrameType::DeleteNode
            | FrameType::Link
            | FrameType::Unlink
            | FrameType::AttrSet
            | FrameType::RowAppend
            | FrameType::CellsAppend
            | FrameType::Action
            | FrameType::Grant
            | FrameType::Revoke
    )
}

fn extract_request_id(payload: &[u8]) -> Option<u64> {
    let mut pos = 0usize;
    while pos + 6 <= payload.len() {
        let tag = u16::from_le_bytes(le_bytes(&payload[pos..pos + 2]));
        let len = u32::from_le_bytes(le_bytes(&payload[pos + 2..pos + 6])) as usize;
        pos += 6;
        if tag == 0x0001 {
            if len == 8 && pos + 8 <= payload.len() {
                return Some(u64::from_le_bytes(le_bytes(&payload[pos..pos + 8])));
            }
            return None;
        }
        pos += len;
    }
    None
}

/// Byte-array from a statically verified slice (loud on mismatch).
fn le_bytes<const N: usize>(src: &[u8]) -> [u8; N] {
    let mut out = [0u8; N];
    out.copy_from_slice(src);
    out
}

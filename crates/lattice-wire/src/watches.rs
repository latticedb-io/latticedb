//! Watches (SPEC-02 §9, T-309): server-side subscriptions to mutation
//! streams. Events are strictly epoch-ordered (the journal is the only
//! authority — P7), resumable, filtered pre-dispatch (subtree ⊆ engine
//! ancestor knowledge, kinds, min_epoch).
//!
//! Fairness (WPT-9.3): each subscriber owns a pending buffer filled by
//! `poll()` and drained by `drain()` under the credit mechanism (§6) —
//! one slow client cannot starve others because draining is per-sub, not
//! per-connection.
//!
//! Event payload TLV: [epoch u64][kind u8][node u32][delta len u32][bytes].

use crate::frame::FrameType;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;

/// Watch-side TLV tags (SPEC-02 §9; disjoint from §7/§8).
pub mod watch_tags {
    pub const WATCH_ID: u16 = 0x0070;
    pub const CURRENT_EPOCH: u16 = 0x0071;
    pub const SUBTREE: u16 = 0x0072;
    pub const KINDS: u16 = 0x0073;
    pub const MIN_EPOCH: u16 = 0x0074;
    pub const EVENT_EPOCH: u16 = 0x0075;
    pub const EVENT_KIND: u16 = 0x0076;
    pub const EVENT_NODE: u16 = 0x0077;
    pub const EVENT_DELTA: u16 = 0x0078;
}

/// Watch error codes (0x03xx registry extension).
pub mod watch_codes {
    pub const WATCH_NOT_FOUND: u16 = 0x0317;
    pub const BAD_WATCH_PARAMS: u16 = 0x0318;
}

/// Mutation event kinds (SPEC-02 §9 closed set).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EventKind {
    State = 0,
    Attr = 1,
    Struct = 2,
    Action = 3,
    Enabled = 4,
    Timer = 5,
}

impl EventKind {
    pub const ALL: u64 = (1 << 6) - 1;

    pub fn from_bit(bit: u8) -> Option<Self> {
        match bit {
            0 => Some(Self::State),
            1 => Some(Self::Attr),
            2 => Some(Self::Struct),
            3 => Some(Self::Action),
            4 => Some(Self::Enabled),
            5 => Some(Self::Timer),
            _ => None,
        }
    }

    pub fn bit(self) -> u64 {
        1 << (self as u64)
    }

    /// Parse the TLV kinds list: one byte per kind (hint law on unknown).
    pub fn kinds_mask(bytes: &[u8]) -> Result<u64, WatchError> {
        let mut mask = 0u64;
        let mut known = Vec::new();
        for b in bytes {
            match Self::from_bit(*b) {
                Some(k) => mask |= k.bit(),
                None => {
                    for i in 0..6u8 {
                        known.push(i.to_string());
                    }
                    return Err(WatchError {
                        code: watch_codes::BAD_WATCH_PARAMS,
                        hint: format!("known kinds: {}", known.join(", ")),
                        detail: format!("unknown event kind {b}"),
                    });
                }
            }
        }
        Ok(mask)
    }
}

/// One mutation event (journal-derived, epoch-ordered by construction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationEvent {
    pub epoch: u64,
    pub kind: EventKind,
    pub node: u32,
    /// Kind-stable delta payload (raw bytes; shapes documented in SPEC-04).
    pub delta: Vec<u8>,
}

/// The engine watch surface: journal-derived event feed (P7 — the journal
/// is the only authority; events are replayable, epoch-ascending).
pub trait WatchSource {
    fn current_epoch(&self) -> u64;
    /// Events strictly after `after_epoch`, epoch-ascending, at most
    /// `max` (caller pages by draining repeatedly).
    fn events_since(&self, after_epoch: u64, max: usize) -> Vec<MutationEvent>;
    /// Subtree membership (engine ancestor knowledge).
    fn in_subtree(&self, node: u32, root: u32) -> bool;
}

/// Watch error (0x03xx registry).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchError {
    pub code: u16,
    pub hint: String,
    pub detail: String,
}

impl fmt::Display for WatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:#06x}: {} (hint: {})",
            self.code, self.detail, self.hint
        )
    }
}

impl std::error::Error for WatchError {}

struct Subscription {
    subtree: Option<u32>,
    kinds: u64,
    min_epoch: u64,
    /// Delivery cursor: next event must have epoch > this (strict order,
    /// no dup/loss — WPT-9.1).
    next_epoch: u64,
    /// Per-subscriber pending buffer (fairness, WPT-9.3).
    pending: VecDeque<MutationEvent>,
}

/// Server-side watch registry.
pub struct WatchTable {
    next_id: u64,
    subs: BTreeMap<u64, Subscription>,
}

impl WatchTable {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            subs: BTreeMap::new(),
        }
    }

    /// WATCH_SUB: registers the filter set. Returns (watch_id, current_epoch).
    pub fn subscribe(
        &mut self,
        source: &dyn WatchSource,
        subtree: Option<u32>,
        kinds: u64,
        min_epoch: Option<u64>,
    ) -> (u64, u64) {
        let id = self.next_id;
        self.next_id += 1;
        let current = source.current_epoch();
        self.subs.insert(
            id,
            Subscription {
                subtree,
                kinds,
                min_epoch: min_epoch.unwrap_or(0),
                next_epoch: min_epoch.unwrap_or(0).saturating_sub(1),
                pending: VecDeque::new(),
            },
        );
        (id, current)
    }

    /// WATCH_UNSUB: stops delivery (id gone; events for it are dropped).
    pub fn unsubscribe(&mut self, watch_id: u64) -> Result<(), WatchError> {
        self.subs
            .remove(&watch_id)
            .map(|_| ())
            .ok_or_else(|| WatchError {
                code: watch_codes::WATCH_NOT_FOUND,
                hint: String::new(),
                detail: format!("watch {watch_id} not found"),
            })
    }

    /// Fan-out: pull new events from the source into every subscriber's
    /// pending buffer (server-side pre-dispatch filtering happens here —
    /// once, at fill time; drains are pure buffer pops).
    pub fn poll(&mut self, source: &dyn WatchSource, cap: usize) {
        for sub in self.subs.values_mut() {
            loop {
                let events = source.events_since(sub.next_epoch, cap);
                if events.is_empty() {
                    break;
                }
                let mut advanced = false;
                for ev in events {
                    // Journal-derived input must be epoch-ascending (P7).
                    debug_assert!(
                        ev.epoch > sub.next_epoch,
                        "events_since returned non-advancing epoch"
                    );
                    sub.next_epoch = ev.epoch;
                    advanced = true;
                    // Pre-dispatch filters: kinds, min_epoch, subtree.
                    if ev.epoch < sub.min_epoch
                        || sub.kinds & ev.kind.bit() == 0
                        || match sub.subtree {
                            Some(root) if root != ev.node => !source.in_subtree(ev.node, root),
                            _ => false,
                        }
                    {
                        continue;
                    }
                    sub.pending.push_back(ev);
                }
                if !advanced {
                    break;
                }
            }
        }
    }

    /// Drain up to `budget` events for one subscriber → EVENT frame
    /// payloads. Strictly epoch-ascending (pending is filled ascending).
    pub fn drain(
        &mut self,
        watch_id: u64,
        budget: usize,
    ) -> Result<Vec<MutationEvent>, WatchError> {
        let Some(sub) = self.subs.get_mut(&watch_id) else {
            return Err(WatchError {
                code: watch_codes::WATCH_NOT_FOUND,
                hint: "subscribe first (WATCH_SUB)".into(),
                detail: format!("watch {watch_id} not found"),
            });
        };
        let mut out = Vec::with_capacity(budget.min(sub.pending.len()));
        for _ in 0..budget {
            match sub.pending.pop_front() {
                Some(ev) => out.push(ev),
                None => break,
            }
        }
        Ok(out)
    }

    /// EVENT frame payload for one event (TLV per §9).
    pub fn event_payload(ev: &MutationEvent) -> Vec<u8> {
        let mut out = Vec::with_capacity(24 + ev.delta.len());
        out.extend_from_slice(&watch_tags::EVENT_EPOCH.to_le_bytes());
        out.extend_from_slice(&8u32.to_le_bytes());
        out.extend_from_slice(&ev.epoch.to_le_bytes());
        out.extend_from_slice(&watch_tags::EVENT_KIND.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.push(ev.kind as u8);
        out.extend_from_slice(&watch_tags::EVENT_NODE.to_le_bytes());
        out.extend_from_slice(&4u32.to_le_bytes());
        out.extend_from_slice(&ev.node.to_le_bytes());
        out.extend_from_slice(&watch_tags::EVENT_DELTA.to_le_bytes());
        out.extend_from_slice(&(ev.delta.len() as u32).to_le_bytes());
        out.extend_from_slice(&ev.delta);
        out
    }

    pub fn sub_count(&self) -> usize {
        self.subs.len()
    }

    pub fn pending_of(&self, watch_id: u64) -> usize {
        self.subs
            .get(&watch_id)
            .map(|s| s.pending.len())
            .unwrap_or(0)
    }
}

impl Default for WatchTable {
    fn default() -> Self {
        Self::new()
    }
}

/// WATCH_SUB dispatch → ACK payload [watch_id TLV][current_epoch TLV].
pub struct WatchDispatch;

impl WatchDispatch {
    pub fn sub(
        table: &mut WatchTable,
        source: &dyn WatchSource,
        payload: &BTreeMap<u16, Vec<u8>>,
    ) -> Result<Vec<u8>, WatchError> {
        let subtree = payload.get(&watch_tags::SUBTREE).and_then(|b| {
            if b.len() == 4 {
                Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            } else {
                None
            }
        });
        let kinds_bytes = payload.get(&watch_tags::KINDS).map(|b| b.as_slice());
        let kinds = match kinds_bytes {
            Some(bytes) => EventKind::kinds_mask(bytes)?,
            None => EventKind::ALL,
        };
        let min_epoch = payload
            .get(&watch_tags::MIN_EPOCH)
            .and_then(|b| b.first_chunk::<8>());
        let min_epoch = match min_epoch {
            Some(chunk) => Some(u64::from_le_bytes(*chunk)),
            None if payload.contains_key(&watch_tags::MIN_EPOCH) => {
                return Err(WatchError {
                    code: watch_codes::BAD_WATCH_PARAMS,
                    hint: "min_epoch expects 8 bytes".into(),
                    detail: "field has wrong length".into(),
                });
            }
            None => None,
        };
        let (watch_id, current_epoch) = table.subscribe(source, subtree, kinds, min_epoch);
        let mut ack = Vec::new();
        ack.extend_from_slice(&watch_tags::WATCH_ID.to_le_bytes());
        ack.extend_from_slice(&8u32.to_le_bytes());
        ack.extend_from_slice(&watch_id.to_le_bytes());
        ack.extend_from_slice(&watch_tags::CURRENT_EPOCH.to_le_bytes());
        ack.extend_from_slice(&8u32.to_le_bytes());
        ack.extend_from_slice(&current_epoch.to_le_bytes());
        Ok(ack)
    }

    pub fn unsub(
        table: &mut WatchTable,
        payload: &BTreeMap<u16, Vec<u8>>,
    ) -> Result<(), WatchError> {
        let Some(bytes) = payload.get(&watch_tags::WATCH_ID) else {
            return Err(WatchError {
                code: watch_codes::BAD_WATCH_PARAMS,
                hint: "watch_id tag required".into(),
                detail: "WATCH_UNSUB without watch_id".into(),
            });
        };
        if bytes.len() != 8 {
            return Err(WatchError {
                code: watch_codes::BAD_WATCH_PARAMS,
                hint: "watch_id expects 8 bytes".into(),
                detail: format!("field has {} bytes", bytes.len()),
            });
        }
        let Some(chunk) = bytes.first_chunk::<8>() else {
            return Err(WatchError {
                code: watch_codes::BAD_WATCH_PARAMS,
                hint: "watch_id expects 8 bytes".into(),
                detail: format!("field has {} bytes", bytes.len()),
            });
        };
        table.unsubscribe(u64::from_le_bytes(*chunk))
    }

    /// Route WATCH_SUB / WATCH_UNSUB wire frames.
    pub fn dispatch(
        table: &mut WatchTable,
        source: &dyn WatchSource,
        frame_type: FrameType,
        payload: &BTreeMap<u16, Vec<u8>>,
    ) -> Result<Vec<u8>, WatchError> {
        match frame_type {
            FrameType::WatchSub => Self::sub(table, source, payload),
            FrameType::WatchUnsub => {
                Self::unsub(table, payload)?;
                Ok(Vec::new())
            }
            _ => Err(WatchError {
                code: watch_codes::BAD_WATCH_PARAMS,
                hint: "watch frames: WATCH_SUB WATCH_UNSUB".into(),
                detail: "frame is not a watch command".into(),
            }),
        }
    }
}

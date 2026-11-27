//! Timer wheel (SPEC-01 §9 timing, T-205; P8: the engine's ONLY clock).
//!
//! A transition's `timing {lag_ms?, deadline_ms?, on_deadline?}` drives two
//! timer classes, armed when a node ENTERS the transition's `from` state:
//! - **Lag**: the transition MAY fire no earlier than `from-entry + lag_ms`
//!   (guard lags, SLA slack). Dispatch consults the wheel (`lag_ok`) before
//!   allowing the motion (SGT-9.4).
//! - **Deadline**: if the transition has not fired by `from-entry +
//!   deadline_ms`, the engine fires `on_deadline` instead (typically to a
//!   failure terminal) with author = timer (SGT-9.3).
//!
//! Cancellation: when the node leaves the `from` state (any motion), both
//! timers become moot and are cancelled — a deadline firing after the
//! original fired is dropped at dispatch (StateMismatch), and this wheel
//! cancels eagerly to keep `advance` O(due).
//!
//! Time source: the wheel stores absolute journal-time (`logical_us`)
//! deadlines; `advance(now)` pops everything due. No wall clock anywhere.

use lattice_core::ids::NodeId;
use lattice_schema::key::Key;
use std::collections::BTreeMap;
use std::fmt;

/// What happens when a timer fires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerKind {
    /// Eligibility marker: the transition MAY fire from this point.
    Lag,
    /// Deadline miss: fire the `on_deadline` transition instead.
    Deadline { on_deadline: Key },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TimerEntry {
    node: NodeId,
    transition: Key,
    kind: TimerKind,
    armed_us: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerError {
    /// Armed twice for the same (node, transition) without cancellation.
    AlreadyArmed { node: u32, transition: Key },
    /// Cancellation for a (node, transition) that has no timer — benign in
    /// the state-exit path but surfaced for explicit tests.
    NotArmed { node: u32, transition: Key },
}

impl fmt::Display for TimerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimerError::AlreadyArmed { node, transition } => {
                write!(
                    f,
                    "timer already armed for node {node} transition `{transition}`"
                )
            }
            TimerError::NotArmed { node, transition } => {
                write!(
                    f,
                    "no timer armed for node {node} transition `{transition}`"
                )
            }
        }
    }
}

impl std::error::Error for TimerError {}

/// A due timer popped by [`TimerWheel::advance`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Due {
    pub node: NodeId,
    pub transition: Key,
    pub kind: TimerKind,
    pub armed_us: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TimerWheel {
    /// fire_at_us → entries (BTreeMap = chronological pop).
    heap: BTreeMap<u64, Vec<TimerEntry>>,
    /// (node id, transition) → fire_at_us (cancellation index).
    index: BTreeMap<(u32, Key), u64>,
}

impl TimerWheel {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm the lag timer: the transition MAY fire at `armed_us + lag_ms`.
    pub fn arm_lag(
        &mut self,
        node: NodeId,
        transition: &Key,
        lag_ms: u64,
        armed_us: u64,
    ) -> Result<(), TimerError> {
        self.arm(
            node,
            transition,
            TimerKind::Lag,
            armed_us + lag_ms * 1_000,
            armed_us,
        )
    }

    /// Arm the deadline timer: at `armed_us + deadline_ms` the engine fires
    /// `on_deadline` if the transition has not fired.
    pub fn arm_deadline(
        &mut self,
        node: NodeId,
        transition: &Key,
        deadline_ms: u64,
        on_deadline: &Key,
        armed_us: u64,
    ) -> Result<(), TimerError> {
        self.arm(
            node,
            transition,
            TimerKind::Deadline {
                on_deadline: on_deadline.clone(),
            },
            armed_us + deadline_ms * 1_000,
            armed_us,
        )
    }

    fn arm(
        &mut self,
        node: NodeId,
        transition: &Key,
        kind: TimerKind,
        fire_at: u64,
        armed_us: u64,
    ) -> Result<(), TimerError> {
        let idx = (node.0, transition.clone());
        if self.index.contains_key(&idx) {
            return Err(TimerError::AlreadyArmed {
                node: node.0,
                transition: transition.clone(),
            });
        }
        self.heap.entry(fire_at).or_default().push(TimerEntry {
            node,
            transition: transition.clone(),
            kind,
            armed_us,
        });
        self.index.insert(idx, fire_at);
        Ok(())
    }

    /// Cancel both timers for (node, transition) — on state exit.
    pub fn cancel(&mut self, node: NodeId, transition: &Key) -> Result<(), TimerError> {
        let idx = (node.0, transition.clone());
        let Some(&fire_at) = self.index.get(&idx) else {
            return Err(TimerError::NotArmed {
                node: node.0,
                transition: transition.clone(),
            });
        };
        self.index.remove(&idx);
        if let Some(list) = self.heap.get_mut(&fire_at) {
            list.retain(|e| !(e.node == node && &e.transition == transition));
            if list.is_empty() {
                self.heap.remove(&fire_at);
            }
        }
        Ok(())
    }

    /// Cancel ALL timers for a node (state exit invalidates every timer
    /// armed from the departed state).
    pub fn cancel_node(&mut self, node: NodeId) -> usize {
        let keys: Vec<(u32, Key)> = self
            .index
            .keys()
            .filter(|(n, _)| *n == node.0)
            .cloned()
            .collect();
        let mut cancelled = 0usize;
        for (n, t) in keys {
            if self.cancel(NodeId(n), &t).is_ok() {
                cancelled += 1;
            }
        }
        cancelled
    }

    /// Has the lag elapsed for (node, transition) at `now_us`? True when no
    /// lag timer is armed (never armed, or already cancelled by state exit).
    pub fn lag_ok(&self, node: NodeId, transition: &Key, now_us: u64) -> bool {
        match self.index.get(&(node.0, transition.clone())) {
            Some(&fire_at) => now_us >= fire_at,
            None => true,
        }
    }

    /// The fire_at timestamp for an armed timer (lag remaining math).
    pub fn fire_at(&self, node: NodeId, transition: &Key) -> Option<u64> {
        self.index.get(&(node.0, transition.clone())).copied()
    }

    /// Is a deadline armed for (node, transition)?
    pub fn deadline_armed(&self, node: NodeId, transition: &Key) -> bool {
        matches!(
            self.heap
                .values()
                .flatten()
                .find(|e| e.node == node && &e.transition == transition)
                .map(|e| &e.kind),
            Some(TimerKind::Deadline { .. })
        )
    }

    /// Pop all entries due at or before `now_us`, chronologically.
    pub fn advance(&mut self, now_us: u64) -> Vec<Due> {
        let mut due = Vec::new();
        let keys: Vec<u64> = self.heap.range(..=now_us).map(|(k, _)| *k).collect();
        for fire_at in keys {
            if let Some(list) = self.heap.remove(&fire_at) {
                for e in list {
                    self.index.remove(&(e.node.0, e.transition.clone()));
                    due.push(Due {
                        node: e.node,
                        transition: e.transition,
                        kind: e.kind,
                        armed_us: e.armed_us,
                    });
                }
            }
        }
        due
    }

    pub fn len(&self) -> usize {
        self.index.len()
    }

    pub fn is_empty(&self) -> bool {
        self.index.is_empty()
    }
}

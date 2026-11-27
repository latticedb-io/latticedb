//! Subtree U-locks (ARCH §8): exclusive per conflict-domain key, ordered
//! acquisition (keys sorted ascending) ⇒ deadlock-free under concurrent
//! multi-key writers. Stats expose real parallelism (max simultaneous
//! holders per key == 1; disjoint keys overlap).
//!
//! Blocking acquisition via a condvar — worker threads are dedicated OS
//! threads (no async in kernel crates, execution-model DEC).

use crate::{TxError, ordered_domain};
use lattice_core::ids::NodeId;
use std::collections::HashSet;
use std::sync::{Condvar, Mutex};

/// Stats for exclusion/parallelism assertions (tests + metrics).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LockStats {
    pub acquisitions: u64,
    /// Max distinct keys simultaneously held (parallelism signal).
    pub max_held_keys: u64,
}

/// U-lock table keyed by NodeId conflict domains.
pub struct SubtreeLockTable {
    held: Mutex<HashSet<NodeId>>,
    cv: Condvar,
    stats: Mutex<LockStats>,
}

impl SubtreeLockTable {
    pub fn new() -> Self {
        Self {
            held: Mutex::new(HashSet::new()),
            cv: Condvar::new(),
            stats: Mutex::new(LockStats::default()),
        }
    }

    /// Acquire U-locks on all domain keys (blocking). Keys are acquired
    /// ascending (ordered_domain) so concurrent writers with overlapping
    /// domains can never hold-and-wait in opposite orders — the classic
    /// deadlock-free discipline.
    pub fn acquire(&self, domain: Vec<NodeId>) -> Result<SubtreeGuard<'_>, TxError> {
        let keys = ordered_domain(domain);
        let mut held = self.held.lock().map_err(|_| TxError::LockPoisoned)?;
        let mut acquired = Vec::with_capacity(keys.len());
        for k in keys {
            while held.contains(&k) {
                held = self.cv.wait(held).map_err(|_| TxError::LockPoisoned)?;
            }
            held.insert(k);
            acquired.push(k);
        }
        if let Ok(mut stats) = self.stats.lock() {
            stats.acquisitions += acquired.len() as u64;
            stats.max_held_keys = stats.max_held_keys.max(held.len() as u64);
        }
        drop(held);
        Ok(SubtreeGuard {
            table: self,
            keys: acquired,
        })
    }

    pub fn stats(&self) -> LockStats {
        self.stats.lock().map(|s| s.clone()).unwrap_or_default()
    }

    pub fn is_held(&self, key: NodeId) -> bool {
        self.held.lock().map(|h| h.contains(&key)).unwrap_or(false)
    }

    /// Currently held key count (exclusion audit).
    pub fn held_count(&self) -> usize {
        self.held.lock().map(|h| h.len()).unwrap_or(0)
    }
}

impl Default for SubtreeLockTable {
    fn default() -> Self {
        Self::new()
    }
}

/// One held lock set; releases on drop (notifies waiters).
pub struct SubtreeGuard<'t> {
    table: &'t SubtreeLockTable,
    keys: Vec<NodeId>,
}

impl SubtreeGuard<'_> {
    /// The acquired keys (ascending).
    pub fn keys(&self) -> &[NodeId] {
        &self.keys
    }
}

impl Drop for SubtreeGuard<'_> {
    fn drop(&mut self) {
        let mut held = match self.table.held.lock() {
            Ok(h) => h,
            Err(_) => return, // poisoned: the process is failing anyway
        };
        for k in &self.keys {
            held.remove(k);
        }
        drop(held);
        self.table.cv.notify_all();
    }
}

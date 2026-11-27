//! Identifiers and epochs (ARCHITECTURE §4). One `node_id` space per Database
//! across all graphs; graphs own subsets via `graph_id`.

use std::fmt;

pub const NO_PARENT: u32 = u32::MAX;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $inner:ty) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub $inner);

        impl From<$inner> for $name {
            fn from(v: $inner) -> Self {
                Self(v)
            }
        }
        impl From<$name> for $inner {
            fn from(v: $name) -> Self {
                v.0
            }
        }        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

id_type!(
    /// Database-scoped node identifier (`u32`, one space across all graphs).
    NodeId,
    u32
);
id_type!(
    /// Graph namespace partition inside a Database.
    GraphId,
    u16
);
id_type!(
    /// Catalog-scoped attribute definition id, stable per schema-epoch lineage.
    AttrId,
    u16
);
id_type!(
    /// Per-node monotone change counter; also the snapshot marker.
    Epoch,
    u64
);
id_type!(
    /// Journal log sequence number: total order of committed facts per Database.
    Lsn,
    u64
);
id_type!(
    /// Position on a node's key axis (`k`, the "primary key of the node-table").
    KeyIndex,
    u64
);
id_type!(
    /// Edge identifier, monotone per Database.
    EdgeId,
    u32
);

impl std::ops::Add for Epoch {
    type Output = Epoch;
    fn add(self, rhs: Epoch) -> Epoch {
        Epoch(self.0 + rhs.0)
    }
}

impl std::ops::Add<u64> for Epoch {
    type Output = Epoch;
    fn add(self, rhs: u64) -> Epoch {
        Epoch(self.0 + rhs)
    }
}

/// Node-id allocation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdSpaceError {
    Exhausted,
}

impl fmt::Display for IdSpaceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdSpaceError::Exhausted => write!(f, "node-id space exhausted (u32)"),
        }
    }
}

impl std::error::Error for IdSpaceError {}

/// The per-Database node-id space. Deterministic by construction: ids are
/// allocated monotonically and never reused, so replay and snapshot restore
/// reproduce identical assignments. `u32::MAX` is reserved as `NO_PARENT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeIdSpace {
    next: u32,
}

impl NodeIdSpace {
    pub const fn new() -> Self {
        Self { next: 0 }
    }

    /// Restore from a snapshot (T-106): `next` is the persisted watermark.
    pub fn restore(next: u32) -> Self {
        Self { next }
    }

    pub fn allocate(&mut self) -> Result<NodeId, IdSpaceError> {
        if self.next == NO_PARENT {
            return Err(IdSpaceError::Exhausted);
        }
        let id = NodeId(self.next);
        self.next += 1;
        Ok(id)
    }

    /// Allocation watermark (persisted in snapshots).
    pub fn watermark(&self) -> u32 {
        self.next
    }

    pub fn len(&self) -> u32 {
        self.next
    }

    pub fn is_empty(&self) -> bool {
        self.next == 0
    }
}

impl Default for NodeIdSpace {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocation_is_monotone_and_never_reused() {
        let mut space = NodeIdSpace::new();
        assert_eq!(space.allocate().unwrap(), NodeId(0));
        assert_eq!(space.allocate().unwrap(), NodeId(1));
        assert_eq!(space.watermark(), 2);
        assert_eq!(space.len(), 2);
        assert!(!space.is_empty());
    }

    #[test]
    fn exhaustion_is_an_error_not_a_wrap() {
        let mut space = NodeIdSpace::restore(NO_PARENT);
        assert_eq!(space.allocate(), Err(IdSpaceError::Exhausted));
    }

    #[test]
    fn restore_resumes_at_watermark() {
        let mut space = NodeIdSpace::restore(7);
        assert_eq!(space.allocate().unwrap(), NodeId(7));
        assert_eq!(space.watermark(), 8);
    }
}

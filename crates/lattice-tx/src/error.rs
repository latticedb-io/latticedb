//! Kernel error surface: the closed wire-code registry (SPEC-02 §7), the
//! command-rejection type, and the post-sequencing failure type.
//!
//! - [`KernelError`] — a command the kernel REFUSED. Nothing was journaled,
//!   nothing applied. Carries a registry code plus the hint-law text.
//! - [`TxError`] — a failure of the kernel itself (journal, storage, an
//!   invariant). After the sequencing point it poisons the database: memory
//!   may differ from the journal and the kernel must stop serving.

use std::fmt;

/// Closed error-code registry (SPEC-02 §7, `0x03xx`). One registry, owned by
/// the kernel; the wire layer re-exports it — never a second copy.
pub mod codes {
    pub const AUTH_FAILED: u16 = 0x0301;
    pub const FORBIDDEN: u16 = 0x0302;
    pub const NOT_FOUND: u16 = 0x0303;
    pub const ALREADY_EXISTS: u16 = 0x0304;
    pub const INVALID_KEY: u16 = 0x0305;
    pub const SCHEMA: u16 = 0x0306;
    pub const INVALID_KIND_PARAMS: u16 = 0x0307;
    pub const CYCLE: u16 = 0x0308;
    pub const MULTI_PARENT: u16 = 0x0309;
    pub const CONTAINMENT: u16 = 0x030A;
    pub const HAS_DEPENDENTS: u16 = 0x030B;
    pub const REQUIREMENTS_UNMET: u16 = 0x030C;
    pub const TERMINAL: u16 = 0x030D;
    pub const NOT_APPENDABLE: u16 = 0x030E;
    pub const NOT_MONOTONE: u16 = 0x030F;
    pub const R13_IN_USE: u16 = 0x0310;
    pub const BACKWARD_RANK: u16 = 0x0311;
    pub const ORDER_CONFLICT: u16 = 0x0312;
    pub const TXN_NESTED: u16 = 0x0313;
    pub const RESUME_EXPIRED: u16 = 0x0314;
    pub const VERSION_MISMATCH: u16 = 0x0315;
    pub const CAPABILITY_MISMATCH: u16 = 0x0316;
    pub const FRAME_TOO_LARGE: u16 = 0x0317;
    pub const CANCELLED: u16 = 0x0318;
    pub const LIMIT_EXCEEDED: u16 = 0x0319;
    pub const INTERNAL: u16 = 0x03FF;

    /// Registry name of a code (`""` for codes outside the registry).
    pub fn name(code: u16) -> &'static str {
        match code {
            AUTH_FAILED => "AUTH_FAILED",
            FORBIDDEN => "FORBIDDEN",
            NOT_FOUND => "NOT_FOUND",
            ALREADY_EXISTS => "ALREADY_EXISTS",
            INVALID_KEY => "INVALID_KEY",
            SCHEMA => "SCHEMA",
            INVALID_KIND_PARAMS => "INVALID_KIND_PARAMS",
            CYCLE => "CYCLE",
            MULTI_PARENT => "MULTI_PARENT",
            CONTAINMENT => "CONTAINMENT",
            HAS_DEPENDENTS => "HAS_DEPENDENTS",
            REQUIREMENTS_UNMET => "REQUIREMENTS_UNMET",
            TERMINAL => "TERMINAL",
            NOT_APPENDABLE => "NOT_APPENDABLE",
            NOT_MONOTONE => "NOT_MONOTONE",
            R13_IN_USE => "R13_IN_USE",
            BACKWARD_RANK => "BACKWARD_RANK",
            ORDER_CONFLICT => "ORDER_CONFLICT",
            TXN_NESTED => "TXN_NESTED",
            RESUME_EXPIRED => "RESUME_EXPIRED",
            VERSION_MISMATCH => "VERSION_MISMATCH",
            CAPABILITY_MISMATCH => "CAPABILITY_MISMATCH",
            FRAME_TOO_LARGE => "FRAME_TOO_LARGE",
            CANCELLED => "CANCELLED",
            LIMIT_EXCEEDED => "LIMIT_EXCEEDED",
            INTERNAL => "INTERNAL",
            _ => "",
        }
    }
}

/// Hint-law cap: a rejection with enumerable alternatives lists at most this
/// many (SPEC-02 §7).
pub const HINT_LIMIT: usize = 50;

/// A refused command: registry code + hint + detail. Nothing was journaled
/// and nothing applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelError {
    pub code: u16,
    pub hint: String,
    pub detail: String,
}

impl KernelError {
    pub fn new(code: u16, hint: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            code,
            hint: hint.into(),
            detail: detail.into(),
        }
    }

    /// Hint-law text for an unknown name: `known <kind>s: a, b, c` over the
    /// first [`HINT_LIMIT`] sorted names.
    pub fn name_hint(kind: &str, known: impl IntoIterator<Item = String>) -> String {
        let mut names: Vec<String> = known.into_iter().collect();
        names.sort();
        let total = names.len();
        names.truncate(HINT_LIMIT);
        let mut hint = format!("known {kind}s: {}", names.join(", "));
        if total > HINT_LIMIT {
            hint.push_str(&format!(" (+{} more)", total - HINT_LIMIT));
        }
        hint
    }

    pub fn code_name(&self) -> &'static str {
        let name = codes::name(self.code);
        if name.is_empty() {
            "UNKNOWN_CODE"
        } else {
            name
        }
    }
}

impl fmt::Display for KernelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code_name(), self.detail)?;
        if !self.hint.is_empty() {
            write!(f, " (hint: {})", self.hint)?;
        }
        Ok(())
    }
}

impl std::error::Error for KernelError {}

/// A failure of the kernel itself. `Invariant` is raised only AFTER the
/// sequencing point: the caller must poison the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxError {
    /// Engine gate rejected a legacy fact (pre-kernel sequencer path).
    Validation { rule: String },
    /// Journal append failed (io / corruption / gap).
    Journal { reason: String },
    /// Queue closed — pool shutting down.
    Closed,
    /// Lock table poisoned / inconsistent — refuse rather than deadlock.
    LockPoisoned,
    /// OS thread spawn failed (name/OOM) — refuse, no partial pool.
    ThreadSpawn { reason: String },
    /// A journaled record failed to apply, or an internal invariant broke.
    /// Memory may differ from the journal.
    Invariant { detail: String },
    /// Snapshot / MANIFEST / part failure outside the journal.
    Storage { reason: String },
    /// Schema compile or op-log failure outside a command rejection.
    Schema { reason: String },
}

impl fmt::Display for TxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation { rule } => write!(f, "validation failed: {rule}"),
            Self::Journal { reason } => write!(f, "journal: {reason}"),
            Self::Closed => write!(f, "sequencer closed"),
            Self::LockPoisoned => write!(f, "lock table poisoned"),
            Self::ThreadSpawn { reason } => write!(f, "thread spawn: {reason}"),
            Self::Invariant { detail } => write!(f, "kernel invariant broken: {detail}"),
            Self::Storage { reason } => write!(f, "storage: {reason}"),
            Self::Schema { reason } => write!(f, "schema: {reason}"),
        }
    }
}

impl std::error::Error for TxError {}

impl From<lattice_storage::journal::JournalError> for TxError {
    fn from(e: lattice_storage::journal::JournalError) -> Self {
        Self::Journal {
            reason: e.to_string(),
        }
    }
}

impl From<lattice_storage::manifest::ManifestError> for TxError {
    fn from(e: lattice_storage::manifest::ManifestError) -> Self {
        Self::Storage {
            reason: e.to_string(),
        }
    }
}

impl From<lattice_storage::snapshot::SnapshotError> for TxError {
    fn from(e: lattice_storage::snapshot::SnapshotError) -> Self {
        Self::Storage {
            reason: e.to_string(),
        }
    }
}

impl From<lattice_storage::part::PartError> for TxError {
    fn from(e: lattice_storage::part::PartError) -> Self {
        Self::Storage {
            reason: e.to_string(),
        }
    }
}

impl From<lattice_schema::entities::SchemaError> for TxError {
    fn from(e: lattice_schema::entities::SchemaError) -> Self {
        Self::Schema {
            reason: e.to_string(),
        }
    }
}

impl From<lattice_schema::opslog::OpsError> for TxError {
    fn from(e: lattice_schema::opslog::OpsError) -> Self {
        Self::Schema {
            reason: e.to_string(),
        }
    }
}

/// Post-sequencing invariant failure (see [`TxError::Invariant`]).
pub fn invariant(detail: impl Into<String>) -> TxError {
    TxError::Invariant {
        detail: detail.into(),
    }
}

//! Schema entity errors (SPEC-01 §1–§15 structural level; wire error-code
//! mapping happens at the protocol layer, M3).

use crate::key::Key;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    InvalidKey(crate::key::KeyError),
    /// Key already used in its parent entity's namespace (SPEC-01 §2).
    AlreadyExists {
        namespace: &'static str,
        key: Key,
    },
    /// Referenced catalog entity does not exist.
    NotFound {
        namespace: &'static str,
        key: Key,
    },
    /// AttributeSet members must be homogeneous (SPEC-01 §7).
    MixedSet {
        key: Key,
    },
    /// EnumType: values unique, non-empty, ≤ 256 (SPEC-01 §4).
    EnumBounds {
        key: Key,
        count: usize,
    },
    EnumValueDuplicate {
        key: Key,
        value: Key,
    },
    /// RecordType: flat, 1..=64 fields, unique names (SPEC-01 §5).
    RecordBounds {
        key: Key,
        count: usize,
    },
    RecordFieldDuplicate {
        key: Key,
        field: Key,
    },
    /// StateModel class rules (SPEC-01 §8: SGT-8.1/8.2).
    FinalizingMissingTerminal {
        key: Key,
        class: &'static str,
    },
    EndlessHasTerminal {
        key: Key,
    },
    InitialStateInvalid {
        key: Key,
        state: Key,
    },
    /// Transition actor-class law (SPEC-01 §9): machine-only non-empty sets.
    MachineOnlyActorSet {
        key: Key,
    },
    /// Display name cap (SPEC-01 §2).
    DisplayName(crate::key::KeyError),
    /// Backward motion in a finalizing model — structurally impossible (R4,
    /// SPEC-01 §8; SGT-8.4).
    BackwardRank {
        model: Key,
        from: Key,
        to: Key,
        from_rank: u64,
        to_rank: u64,
    },
    /// parent_type chain cycle (SGT-11.3).
    TypeCycle {
        key: Key,
    },
    /// Guard predicate over an attribute without declared monotonicity
    /// (SGT-13.2: `SCHEMA` at rule creation).
    UnresolvedMonotonicity {
        rule: Key,
        attr: Key,
    },
    /// AttributeSet references an unknown attribute (compile-time).
    UnresolvedSetMember {
        set: Key,
        attr: Key,
    },
}

impl fmt::Display for SchemaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaError::InvalidKey(e) => write!(f, "invalid key: {e}"),
            SchemaError::AlreadyExists { namespace, key } => {
                write!(f, "{namespace} key `{key}` already exists")
            }
            SchemaError::NotFound { namespace, key } => {
                write!(f, "{namespace} key `{key}` not found")
            }
            SchemaError::MixedSet { key } => {
                write!(
                    f,
                    "attribute set `{key}` mixes scalar-family and vector members"
                )
            }
            SchemaError::EnumBounds { key, count } => {
                write!(f, "enum `{key}` has {count} values; cap is 256")
            }
            SchemaError::EnumValueDuplicate { key, value } => {
                write!(f, "enum `{key}` declares value `{value}` twice")
            }
            SchemaError::RecordBounds { key, count } => {
                write!(f, "record `{key}` has {count} fields; cap is 64")
            }
            SchemaError::RecordFieldDuplicate { key, field } => {
                write!(f, "record `{key}` declares field `{field}` twice")
            }
            SchemaError::FinalizingMissingTerminal { key, class } => {
                write!(f, "finalizing model `{key}` lacks a {class} terminal")
            }
            SchemaError::EndlessHasTerminal { key } => {
                write!(f, "endless model `{key}` must declare no terminals")
            }
            SchemaError::InitialStateInvalid { key, state } => {
                write!(
                    f,
                    "model `{key}`: initial state `{state}` must be declared and non-terminal"
                )
            }
            SchemaError::MachineOnlyActorSet { key } => {
                write!(
                    f,
                    "transition `{key}`: guard/system/timer never appear alone in an invokable set (use empty actors)"
                )
            }
            SchemaError::DisplayName(e) => write!(f, "invalid display name: {e}"),
            SchemaError::BackwardRank {
                model,
                from,
                to,
                from_rank,
                to_rank,
            } => write!(
                f,
                "model `{model}`: transition {from}→{to} goes backward (rank {from_rank} → {to_rank}); backward motion is structurally impossible — use a re-run instance"
            ),
            SchemaError::TypeCycle { key } => {
                write!(f, "node type `{key}` participates in a parent_type cycle")
            }
            SchemaError::UnresolvedMonotonicity { rule, attr } => write!(
                f,
                "rule `{rule}` reads attribute `{attr}` without declared monotonicity — latest/attr comparisons require increasing|decreasing (SGT-13.2)"
            ),
            SchemaError::UnresolvedSetMember { set, attr } => {
                write!(
                    f,
                    "attribute set `{set}` references unknown attribute `{attr}`"
                )
            }
        }
    }
}

impl std::error::Error for SchemaError {}

impl From<crate::key::KeyError> for SchemaError {
    fn from(e: crate::key::KeyError) -> Self {
        SchemaError::InvalidKey(e)
    }
}

//! The closed catalog inventory (SPEC-01 §1/§5/MANIFEST §5): ScalarType
//! (built-in), EnumType, RecordType, KeyType, AttributeDef, AttributeSet,
//! AttributeGroup, StateModel, Transition, NodeType, IndexDef, NamedRule,
//! Role/Grant, Graph schema. Everything typed lives here as data; there are
//! no inline definitions anywhere (SPEC-01 §1).

pub mod error;

pub use error::SchemaError;

use crate::key::Key;
use lattice_core::ids::NodeId;
use lattice_core::value::Value;
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Scalar / element types (SPEC-01 §3)
// ---------------------------------------------------------------------------

/// The built-in scalar set — fixed, non-deletable, present in every fresh
/// Database (SPEC-01 §3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScalarType {
    Bool,
    I64,
    F64,
    /// `decimal(p,s)`: i128 scaled by 10^s; p ∈ [1,38], s ∈ [0,p].
    Decimal {
        precision: u8,
        scale: u8,
    },
    String,
    Bytes,
    /// Microseconds since Unix epoch, UTC.
    Timestamp,
    /// Microseconds.
    Duration,
    NodeRef,
}

/// Field/element type inside records and vectors: scalar ∪ enum — flat by
/// construction (no Record variant ⇒ depth-1 records are unrepresentable,
/// SGT-5.3 at the type level).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ElemType {
    Scalar(ScalarType),
    Enum(Key),
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

/// EnumType (SPEC-01 §4): values unique, non-empty, ≤ 256 (u8-backed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumType {
    pub key: Key,
    pub values: Vec<Key>,
}

impl EnumType {
    pub fn new(key: Key, values: Vec<Key>) -> Result<Self, SchemaError> {
        if values.is_empty() || values.len() > 256 {
            return Err(SchemaError::EnumBounds {
                key: key.clone(),
                count: values.len(),
            });
        }
        let mut seen = BTreeMap::new();
        for v in &values {
            if seen.insert(v.clone(), ()).is_some() {
                return Err(SchemaError::EnumValueDuplicate {
                    key: key.clone(),
                    value: v.clone(),
                });
            }
        }
        Ok(Self { key, values })
    }
}

/// RecordType (SPEC-01 §5): flat depth-1, 1..=64 fields, key-grammar names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordType {
    pub key: Key,
    pub fields: Vec<(Key, ElemType)>,
}

impl RecordType {
    pub fn new(key: Key, fields: Vec<(Key, ElemType)>) -> Result<Self, SchemaError> {
        if fields.is_empty() || fields.len() > lattice_core::value::MAX_RECORD_FIELDS {
            return Err(SchemaError::RecordBounds {
                key: key.clone(),
                count: fields.len(),
            });
        }
        let mut seen = BTreeMap::new();
        for (name, _) in &fields {
            if seen.insert(name.clone(), ()).is_some() {
                return Err(SchemaError::RecordFieldDuplicate {
                    key: key.clone(),
                    field: name.clone(),
                });
            }
        }
        Ok(Self { key, fields })
    }
}

/// KeyType (SPEC-01 §6): the node's single key axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisKind {
    Ordinal,
    Temporal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Density {
    Dense,
    Sparse,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyTypeDef {
    pub key: Key,
    pub density: Density,
    pub axis: AxisKind,
}

/// Attribute monotonicity (SPEC-01 §7, micro-decision 7): required when a
/// guard predicate reads the attribute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Monotonicity {
    #[default]
    None,
    Increasing,
    Decreasing,
}

/// AttributeDef (SPEC-01 §7).
#[derive(Debug, Clone, PartialEq)]
pub struct AttributeDef {
    pub key: Key,
    pub attr_type: AttrType,
    pub required: bool,
    /// Default evaluates at read time; never backfills (absence semantics).
    pub default: Option<Value>,
    /// Per-attribute codec override (SPEC-03 §5 codec registry).
    pub codec_override: Option<String>,
    pub monotonicity: Monotonicity,
    /// Extra scope required to write, beyond write-attribute (SGT-7.2).
    pub write_scope: Option<String>,
}

/// Attribute type: scalar | enum | record | vector<T> (SPEC-01 §7).
#[derive(Debug, Clone, PartialEq)]
pub enum AttrType {
    Scalar(ScalarType),
    Enum(Key),
    Record(Key),
    /// Vector<T> along the node's KeyType axis.
    Vector {
        elem: ElemType,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetKind {
    /// scalar/enum/record members — the node's "metadata row" (k=0 columns).
    ScalarSet,
    /// vector<T> members — the node's table definition (row-append target).
    TableSet,
}

/// AttributeSet (SPEC-01 §7): first-class logical entity, shared across node
/// types; homogeneous kind enforced at compile time (T-103) and at op
/// validation (T-104) once members resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeSet {
    pub key: Key,
    pub kind: SetKind,
    pub members: Vec<Key>,
    pub write_scope: Option<String>,
}

/// AttributeGroup (SPEC-01 §7): UI-only, zero engine semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttributeGroup {
    pub key: Key,
    pub display_name: String,
    pub members: Vec<Key>,
}

/// State model class (SPEC-01 §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelClass {
    Finalizing,
    Endless,
}

/// State (SPEC-01 §8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub key: Key,
    pub name: String,
    pub success_terminal: bool,
    pub failure_terminal: bool,
}

/// StateModel (SPEC-01 §8): class rules validated structurally; ranks are a
/// compile-time property (T-103).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateModel {
    pub key: Key,
    pub class: ModelClass,
    pub states: Vec<State>,
    pub initial: Key,
}

impl StateModel {
    pub fn new(
        key: Key,
        class: ModelClass,
        states: Vec<State>,
        initial: Key,
    ) -> Result<Self, SchemaError> {
        let success = states.iter().filter(|s| s.success_terminal).count();
        let failure = states.iter().filter(|s| s.failure_terminal).count();
        match class {
            ModelClass::Finalizing => {
                if success == 0 {
                    return Err(SchemaError::FinalizingMissingTerminal {
                        key: key.clone(),
                        class: "success",
                    });
                }
                if failure == 0 {
                    return Err(SchemaError::FinalizingMissingTerminal {
                        key: key.clone(),
                        class: "failure",
                    });
                }
            }
            ModelClass::Endless => {
                if success + failure > 0 {
                    return Err(SchemaError::EndlessHasTerminal { key: key.clone() });
                }
            }
        }
        let init = states.iter().find(|s| s.key == initial);
        match init {
            Some(s) if !s.success_terminal && !s.failure_terminal => {}
            _ => {
                return Err(SchemaError::InitialStateInvalid {
                    key: key.clone(),
                    state: initial,
                });
            }
        }
        Ok(Self {
            key,
            class,
            states,
            initial,
        })
    }

    pub fn state(&self, key: &Key) -> Option<&State> {
        self.states.iter().find(|s| &s.key == key)
    }
}

/// Actor classes (SPEC-01 §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ActorClass {
    Human,
    Agent,
    System,
    Timer,
    Guard,
}

/// Transition timing (SPEC-01 §9).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TransitionTiming {
    /// Minimum delay after arming before the transition MAY fire.
    pub lag_ms: Option<u64>,
    /// If not fired within deadline, `on_deadline` fires instead.
    pub deadline_ms: Option<u64>,
    pub on_deadline: Option<Key>,
}

/// Effects v1 (SPEC-01 §9, micro-decision 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Creates a child instance under the acting node.
    Spawn {
        instance_of: Key,
    },
    /// Decision semantics — cancels XOR siblings.
    SelectBranch,
    CancelSiblings,
}

/// Transition (SPEC-01 §9): the central schema object. Non-empty human/agent
/// actors ⇒ invokable action; empty ⇒ engine-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transition {
    pub key: Key,
    pub model: Key,
    pub from: Key,
    pub to: Key,
    /// NamedRules; transition enabled iff all hold (§9).
    pub requirements: Vec<Key>,
    pub actors: Vec<ActorClass>,
    pub payload_type: Option<Key>,
    pub timing: TransitionTiming,
    pub effects: Vec<Effect>,
}

impl Transition {
    /// Argument count mirrors the SPEC-01 §9 entity shape (the central object
    /// of the model) — a documented exception to the 7-arg lint.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        key: Key,
        model: Key,
        from: Key,
        to: Key,
        requirements: Vec<Key>,
        actors: Vec<ActorClass>,
        payload_type: Option<Key>,
        timing: TransitionTiming,
        effects: Vec<Effect>,
    ) -> Result<Self, SchemaError> {
        let machine_only = !actors.is_empty()
            && actors.iter().all(|a| {
                matches!(
                    a,
                    ActorClass::System | ActorClass::Timer | ActorClass::Guard
                )
            });
        if machine_only {
            return Err(SchemaError::MachineOnlyActorSet { key: key.clone() });
        }
        Ok(Self {
            key,
            model,
            from,
            to,
            requirements,
            actors,
            payload_type,
            timing,
            effects,
        })
    }

    /// Non-empty ⇒ invokable as an action by those actor classes (§9).
    pub fn is_action(&self) -> bool {
        !self.actors.is_empty()
    }
}

/// NodeType (SPEC-01 §11). `state_locks` is the state→writable-sets mapping
/// (SPEC-01 §7): absence = all effective sets writable in every state
/// (absence semantics); locks are a consequence of state, never edited
/// directly (SGT-7.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeType {
    pub key: Key,
    pub display_name: String,
    pub parent_type: Option<Key>,
    pub state_model: Key,
    pub attribute_sets: Vec<Key>,
    pub key_type: Key,
    pub state_locks: Vec<StateLock>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateLock {
    pub state: Key,
    pub writable_sets: Vec<Key>,
}

/// IndexDef (SPEC-01 §12): declared, rebuildable derived views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexKind {
    ByState { type_key: Key },
    AttrPresence { attr: Key },
    SetPresence { set_key: Key },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDef {
    pub key: Key,
    pub kind: IndexKind,
}

/// Predicate whitelist forms (SPEC-01 §13) — closed registry; free-form code
/// MUST be rejected (R7). Wall-clock predicates are unrepresentable (P8).
#[derive(Debug, Clone, PartialEq)]
pub enum PredicateForm {
    AttrCmp {
        attr: Key,
        op: CmpOp,
        value: Value,
    },
    AttrPresent {
        attr: Key,
    },
    CellCountGte {
        attr: Key,
        k: u64,
    },
    CellFieldCountGte {
        attr: Key,
        field: Key,
        value: Value,
        min: u64,
    },
    LatestCmp {
        attr: Key,
        op: CmpOp,
        value: Value,
    },
    /// Staleness via journal time only (SGT-13.3 wall-clock ban).
    EpochGte {
        attr: Key,
        age_us: i64,
    },
    ChildrenState {
        success: bool,
        expect_all: bool,
    },
    SetMember {
        attr: Key,
        values: Vec<Value>,
    },
    StrEq {
        attr: Key,
        value: String,
    },
    StrPrefix {
        attr: Key,
        value: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Gt,
    Gte,
    Lt,
    Lte,
}

/// NamedRule (SPEC-01 §13): reusable monotone predicate.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedRule {
    pub key: Key,
    pub form: PredicateForm,
}

/// Scopes (SPEC-01 §14).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    ReadSubtree,
    WriteStructure,
    WriteAttribute,
    InvokeAction,
    Administer,
}

/// Role (SPEC-01 §14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub key: Key,
    pub scopes: Vec<Scope>,
}

/// Grant target (SPEC-01 §14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantTarget {
    Graph(Key),
    Node(NodeId),
}

/// Grant: journaled binding of a role to a subject at a target (SPEC-01 §14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grant {
    pub role: Key,
    pub subject: String,
    pub target: GrantTarget,
}

/// Graph schema entity (SPEC-01 §15): the namespace. `created_at` is a
/// journal-authored timestamp fact (µs UTC), never a wall-clock predicate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphSchema {
    pub key: Key,
    pub display_name: String,
    pub created_at_us: i64,
}

//! The R1–R13 gate functions (see crate docs for the rule map). Each check
//! returns `Ok(())` or a [`Violation`] carrying the rule id and a precise
//! reason — machine-readable for agents (hint law, SPEC-02 §7).

use lattice_core::arrays::GraphArrays;
use lattice_core::ids::NodeId;
use lattice_core::value::{RecordValue, Value};
use lattice_schema::entities::{
    AttrType, AttributeDef, ElemType, EnumType, Monotonicity, RecordType, ScalarType,
};
use lattice_schema::key::Key;
use lattice_schema::txn::SchemaOp;
use std::collections::BTreeMap;
use std::fmt;

fn scalar_conforms(t: ScalarType, value: &Value) -> bool {
    match (t, value) {
        (ScalarType::Bool, Value::Bool(_))
        | (ScalarType::I64, Value::I64(_))
        | (ScalarType::F64, Value::F64(_))
        | (ScalarType::String, Value::Str(_))
        | (ScalarType::Bytes, Value::Bytes(_))
        | (ScalarType::Timestamp, Value::Timestamp(_))
        | (ScalarType::Duration, Value::Duration(_))
        | (ScalarType::NodeRef, Value::NodeRef(_)) => true,
        (ScalarType::Decimal { precision, scale }, Value::Decimal(d)) => {
            d.precision == precision && d.scale == scale
        }
        _ => false,
    }
}

fn elem_conforms(elem: &ElemType, value: &Value, enums: &BTreeMap<Key, EnumType>) -> bool {
    match elem {
        ElemType::Scalar(t) => scalar_conforms(*t, value),
        ElemType::Enum(k) => match value {
            Value::Enum(i) => enums.get(k).is_some_and(|e| (*i as usize) < e.values.len()),
            _ => false,
        },
    }
}

fn record_conforms(
    declared: &RecordType,
    rec: &RecordValue,
    enums: &BTreeMap<Key, EnumType>,
) -> bool {
    // A present field must sit on a declared position and match its type;
    // absent fields are always legal (absence semantics, SPEC-01 §5).
    (0..rec.field_count()).all(|i| {
        rec.get(i).is_none_or(|v| {
            declared
                .fields
                .get(i)
                .is_some_and(|(_, elem)| elem_conforms(elem, v, enums))
        })
    })
}

/// The acting subject's required scope for an operation (R11).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequiredScope {
    ReadSubtree,
    WriteStructure,
    WriteAttribute,
    InvokeAction,
    Administer,
}

impl fmt::Display for RequiredScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            RequiredScope::ReadSubtree => "read_subtree",
            RequiredScope::WriteStructure => "write_structure",
            RequiredScope::WriteAttribute => "write_attribute",
            RequiredScope::InvokeAction => "invoke_action",
            RequiredScope::Administer => "administer",
        };
        f.write_str(s)
    }
}

/// A kernel violation: rule id + precise reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub rule: &'static str,
    pub reason: String,
}

impl Violation {
    pub fn new(rule: &'static str, reason: impl Into<String>) -> Self {
        Self {
            rule,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.rule, self.reason)
    }
}

impl std::error::Error for Violation {}

type R = Result<(), Violation>;

/// The kernel gate set.
pub struct Validators;

impl Validators {
    /// R10 — key grammar (SPEC-01 §2). Delegates to the Key newtype.
    pub fn r10_key(key_text: &str) -> R {
        match Key::parse(key_text) {
            Ok(_) => Ok(()),
            Err(e) => Err(Violation::new(
                "R10",
                format!("invalid key `{key_text}`: {e}"),
            )),
        }
    }

    /// R8 — enum bounds: values non-empty, ≤ 256, unique (SPEC-01 §4).
    pub fn r8_enum_bounds(values: &[Key]) -> R {
        if values.is_empty() || values.len() > 256 {
            return Err(Violation::new(
                "R8",
                format!("{} values; must be 1..=256", values.len()),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for v in values {
            if !seen.insert(v.as_str()) {
                return Err(Violation::new("R8", format!("duplicate enum value `{v}`")));
            }
        }
        Ok(())
    }

    /// R9 — record bounds: 1..=64 fields, unique names (SPEC-01 §5).
    pub fn r9_record_bounds(field_names: &[Key]) -> R {
        if field_names.is_empty() || field_names.len() > lattice_core::value::MAX_RECORD_FIELDS {
            return Err(Violation::new(
                "R9",
                format!("{} fields; must be 1..=64", field_names.len()),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for f in field_names {
            if !seen.insert(f.as_str()) {
                return Err(Violation::new("R9", format!("duplicate field `{f}`")));
            }
        }
        Ok(())
    }

    /// R6 — write schema conformance: the attribute must be declared; the
    /// value type must match; node_ref values must reference alive nodes.
    /// `declared_types` maps attr_id → (attr key, type tag u8 — see
    /// ColumnType registry); `arrays` checks node_ref liveness.
    pub fn r6_write_schema(
        attr_declared: bool,
        attr_key: &Key,
        type_matches: bool,
        value: &Value,
        arrays: &GraphArrays,
    ) -> R {
        if !attr_declared {
            return Err(Violation::new(
                "R6",
                format!("attribute `{attr_key}` not declared in the catalog (SCHEMA)"),
            ));
        }
        if !type_matches {
            return Err(Violation::new(
                "R6",
                format!("value type mismatch for attribute `{attr_key}`"),
            ));
        }
        if let Value::NodeRef(target) = value
            && !arrays.is_alive(*target)
        {
            return Err(Violation::new(
                "R6",
                format!("node_ref target {target} does not exist (or is deleted)"),
            ));
        }
        Ok(())
    }

    /// R6 — value/type conformance: the value must fit the attribute's
    /// declared type — scalar kind, decimal `(p, s)`, enum index range, record
    /// field positions and field types, vector element type. Decimal
    /// precision/scale must equal the declaration exactly: parts store the
    /// unscaled value plus the attribute's `(p, s)`, so a differing value
    /// would not round-trip (SPEC-03 D1).
    pub fn r6_attr_type(
        def: &AttributeDef,
        value: &Value,
        enums: &BTreeMap<Key, EnumType>,
        records: &BTreeMap<Key, RecordType>,
    ) -> R {
        let ok = match &def.attr_type {
            AttrType::Scalar(t) => scalar_conforms(*t, value),
            AttrType::Enum(k) => match value {
                Value::Enum(i) => enums.get(k).is_some_and(|e| (*i as usize) < e.values.len()),
                _ => false,
            },
            AttrType::Record(k) => match value {
                Value::Record(rec) => records
                    .get(k)
                    .is_some_and(|r| record_conforms(r, rec, enums)),
                _ => false,
            },
            AttrType::Vector { elem } => elem_conforms(elem, value, enums),
        };
        if ok {
            Ok(())
        } else {
            Err(Violation::new(
                "R6",
                format!(
                    "value {:?} does not conform to attribute `{}` (declared {:?})",
                    value, def.key, def.attr_type
                ),
            ))
        }
    }

    /// R6 — element conformance for vector cells (CELLS_APPEND / ROW_APPEND).
    pub fn r6_elem_type(elem: &ElemType, value: &Value, enums: &BTreeMap<Key, EnumType>) -> R {
        if elem_conforms(elem, value, enums) {
            Ok(())
        } else {
            Err(Violation::new(
                "R6",
                format!("value {value:?} does not conform to element type {elem:?}"),
            ))
        }
    }

    /// R7 — monotone writes: an attribute declaring `increasing` accepts new
    /// values ≥ current (by journal LSN order); `decreasing` accepts ≤.
    /// `None` disables the gate. Comparison via the algebra's total order.
    pub fn r7_monotone_write(
        monotonicity: Monotonicity,
        current: Option<&Value>,
        new: &Value,
    ) -> R {
        use lattice_schema::entities::Monotonicity as M;
        let (Some(cur), m) = (current, monotonicity) else {
            return Ok(());
        };
        let ordering: Option<std::cmp::Ordering> = match (cur, new) {
            (Value::I64(a), Value::I64(b)) => Some(a.cmp(b)),
            (Value::F64(a), Value::F64(b)) => a.partial_cmp(b),
            (Value::Timestamp(a), Value::Timestamp(b)) => Some(a.cmp(b)),
            (Value::Duration(a), Value::Duration(b)) => Some(a.cmp(b)),
            (Value::Decimal(a), Value::Decimal(b)) => Some(a.unscaled.cmp(&b.unscaled)),
            _ => None,
        };
        let Some(ord) = ordering else { return Ok(()) };
        match m {
            // ord = cur.cmp(new): Greater ⇒ new < cur (a decrease).
            M::Increasing if ord == std::cmp::Ordering::Greater => Err(Violation::new(
                "R7",
                "NOT_MONOTONE: attribute declares increasing — lower value rejected",
            )),
            // Less ⇒ new > cur (an increase).
            M::Decreasing if ord == std::cmp::Ordering::Less => Err(Violation::new(
                "R7",
                "NOT_MONOTONE: attribute declares decreasing — higher value rejected",
            )),
            _ => Ok(()),
        }
    }

    /// R12 — append-only vector axis: explicit k below the current max is
    /// rejected NOT_APPENDABLE (SPEC-01 §6; re-run pattern for corrections).
    pub fn r12_append_only(current_max_k: Option<u64>, k: u64) -> R {
        match current_max_k {
            Some(max) if k < max => Err(Violation::new(
                "R12",
                format!(
                    "NOT_APPENDABLE: k {k} is below current max {max}; corrections belong to a re-run instance"
                ),
            )),
            _ => Ok(()),
        }
    }

    /// R11 — kernel scope: the subject's scope set must contain the required
    /// scope (in-transaction; allow/deny lands in the journal — P9).
    pub fn r11_scope(subject_scopes: &[RequiredScope], required: RequiredScope) -> R {
        if subject_scopes.contains(&required) {
            Ok(())
        } else {
            Err(Violation::new(
                "R11",
                format!("FORBIDDEN: subject lacks required scope `{required}`"),
            ))
        }
    }

    /// R2 — containment: child type's parent_type must equal the parent
    /// node's type (SGT-11.1; UVFlow rule).
    pub fn r2_containment(
        child_type: &Key,
        child_parent_type: Option<&Key>,
        parent_node_type: &Key,
    ) -> R {
        match child_parent_type {
            Some(pt) if pt == parent_node_type => Ok(()),
            Some(pt) => Err(Violation::new(
                "R2",
                format!(
                    "CONTAINMENT: type `{child_type}` requires parent type `{pt}`, node has `{parent_node_type}`"
                ),
            )),
            None => Err(Violation::new(
                "R2",
                format!(
                    "CONTAINMENT: type `{child_type}` is a root type — cannot nest under `{parent_node_type}`"
                ),
            )),
        }
    }

    /// R3 — dependency acyclicity: adding edge src→dst must not close a
    /// cycle in the dependency DAG (DFS from dst reaching src — SGT-15.3).
    /// `dep_out` accessor: node → outgoing dependency targets.
    pub fn r3_acyclicity(
        arrays: &GraphArrays,
        dep_out: &dyn Fn(NodeId) -> Vec<NodeId>,
        src: NodeId,
        dst: NodeId,
    ) -> R {
        // Walk dst's outgoing dependency edges; reaching src = cycle.
        let mut stack = vec![dst];
        let mut seen = std::collections::BTreeSet::new();
        while let Some(n) = stack.pop() {
            if n == src {
                return Err(Violation::new(
                    "R3",
                    format!(
                        "CYCLE: edge {src}→{dst} closes a dependency cycle through {dst}→…→{src}"
                    ),
                ));
            }
            if !seen.insert(n) {
                continue;
            }
            for next in dep_out(n) {
                stack.push(next);
            }
        }
        let _ = arrays; // arrays kept for API symmetry (dep_out closes over it)
        Ok(())
    }

    /// R13 — guarded evolution: surfaced from lattice-schema::txn (rule ids
    /// R13_DRAIN / R13_IN_USE / R13_INDEX_STABILITY). This wrapper only
    /// classifies: destructive ops must pass txn validation first.
    pub fn r13_classify(op: &SchemaOp, is_destructive: bool, passed_txn_validation: bool) -> R {
        if is_destructive && !passed_txn_validation {
            return Err(Violation::new(
                "R13",
                format!("destructive op {op:?} failed reference-count validation"),
            ));
        }
        Ok(())
    }
}

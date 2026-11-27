//! Template bundles (SPEC-01 §17): versioned, ordered batches of schema
//! transactions shipped with the SDK/CLI — the kernel ships no business
//! content (P11, domain neutrality); initialization responsibility lives
//! here.
//!
//! Laws (SPEC-01 §17):
//! - **Idempotent**: re-install = no-op (entities already present are
//!   satisfied; nothing new to apply ⇒ epoch does not grow).
//! - **Resumable**: install applies as ONE atomic schema txn — a failed
//!   install leaves the catalog untouched and can be retried.
//! - **Immutable once published**: a new template version = a new bundle.
//! - Installable only by an `administer`-scoped subject (enforced at the
//!   auth layer, M3 — the API here is the catalog-side half).
//! - **Active meta-schema mode**: nothing distinguishes "fixed after init"
//!   from "continuously evolved" — the installer is just a convenience over
//!   [`SchemaState::commit`] (SGT-17.4 covers tenant-driven evolution).

use lattice_core::value::Value;
use lattice_schema::compile::compile;
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use lattice_schema::txn::{SchemaOp, SchemaState};

/// A published template bundle: named, versioned, ordered ops.
#[derive(Debug, Clone, PartialEq)]
pub struct Bundle {
    pub name: &'static str,
    pub version: u32,
    pub ops: Vec<SchemaOp>,
}

/// Bundle install report (SGT-17.2 evidence).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub name: &'static str,
    pub version: u32,
    /// Ops applied by this call (already-present ops are skipped).
    pub applied: usize,
    /// Ops satisfied by pre-existing entities (idempotent skips).
    pub skipped: usize,
    pub epoch_before: u64,
    pub epoch_after: u64,
    /// `false` = pure no-op (everything pre-existed).
    pub installed: bool,
}

/// Bundle keys are compile-time literals verified by the SGT-17 tests; a
/// violation is a programmer error surfaced as a loud invariant failure.
fn k(s: &str) -> Key {
    match Key::parse(s) {
        Ok(key) => key,
        Err(e) => panic!("bundle fixture key {s:?} violates the key grammar: {e}"),
    }
}

fn state(kk: &str, name: &str, success: bool, failure: bool) -> State {
    State {
        key: k(kk),
        name: name.to_string(),
        success_terminal: success,
        failure_terminal: failure,
    }
}

fn attribute(kk: &str, t: AttrType) -> AttributeDef {
    AttributeDef {
        key: k(kk),
        attr_type: t,
        required: false,
        default: None,
        codec_override: None,
        monotonicity: Monotonicity::None,
        write_scope: None,
    }
}

fn transition(kk: &str, model: &str, from: &str, to: &str, actors: Vec<ActorClass>) -> SchemaOp {
    SchemaOp::AddTransition(
        Transition::new(
            k(kk),
            k(model),
            k(from),
            k(to),
            Vec::new(),
            actors,
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap_or_else(|e| panic!("bundle fixture invalid ({}): {e}", "compile-time literal")),
    )
}

/// `process-mgmt` v1: work/task lifecycle — states primary (SGT-17.3
/// fixture family).
pub fn process_mgmt() -> Bundle {
    Bundle {
        name: "process-mgmt",
        version: 1,
        ops: vec![
            SchemaOp::CreateKeyType(KeyTypeDef {
                key: k("task_seq"),
                density: Density::Dense,
                axis: AxisKind::Ordinal,
            }),
            SchemaOp::CreateEnum(
                EnumType::new(
                    k("task_status"),
                    vec![k("open"), k("running"), k("done"), k("failed")],
                )
                .unwrap_or_else(|e| panic!("bundle fixture invalid: {e}")),
            ),
            SchemaOp::CreateAttribute(attribute(
                "task_title",
                AttrType::Scalar(ScalarType::String),
            )),
            SchemaOp::CreateAttribute(attribute("task_status", AttrType::Enum(k("task_status")))),
            SchemaOp::CreateAttributeSet(AttributeSet {
                key: k("task_meta"),
                kind: SetKind::ScalarSet,
                members: vec![k("task_title"), k("task_status")],
                write_scope: None,
            }),
            SchemaOp::CreateStateModel(
                StateModel::new(
                    k("task_flow"),
                    ModelClass::Finalizing,
                    vec![
                        state("open", "Open", false, false),
                        state("running", "Running", false, false),
                        state("done", "Done", true, false),
                        state("failed", "Failed", false, true),
                    ],
                    k("open"),
                )
                .expect("terminals + initial valid"),
            ),
            transition(
                "task_start",
                "task_flow",
                "open",
                "running",
                vec![ActorClass::Human],
            ),
            transition(
                "task_complete",
                "task_flow",
                "running",
                "done",
                vec![ActorClass::Human],
            ),
            transition(
                "task_fail",
                "task_flow",
                "running",
                "failed",
                vec![ActorClass::Agent],
            ),
            SchemaOp::CreateNodeType(NodeType {
                key: k("task"),
                display_name: "Task".into(),
                parent_type: None,
                state_model: k("task_flow"),
                attribute_sets: vec![k("task_meta")],
                key_type: k("task_seq"),
                state_locks: Vec::new(),
            }),
        ],
    }
}

/// `state-driver` v1: endless machine models — fact/guard/timer-driven.
pub fn state_driver() -> Bundle {
    Bundle {
        name: "state-driver",
        version: 1,
        ops: vec![
            SchemaOp::CreateKeyType(KeyTypeDef {
                key: k("machine_seq"),
                density: Density::Dense,
                axis: AxisKind::Temporal,
            }),
            SchemaOp::CreateAttribute(attribute("machine_rpm", AttrType::Scalar(ScalarType::I64))),
            SchemaOp::CreateAttribute(attribute(
                "machine_series",
                AttrType::Vector {
                    elem: ElemType::Scalar(ScalarType::I64),
                },
            )),
            SchemaOp::CreateAttributeSet(AttributeSet {
                key: k("machine_meta"),
                kind: SetKind::ScalarSet,
                members: vec![k("machine_rpm")],
                write_scope: None,
            }),
            SchemaOp::CreateAttributeSet(AttributeSet {
                key: k("machine_table"),
                kind: SetKind::TableSet,
                members: vec![k("machine_series")],
                write_scope: None,
            }),
            SchemaOp::CreateStateModel(
                StateModel::new(
                    k("machine_cycle"),
                    ModelClass::Endless,
                    vec![
                        state("idle", "Idle", false, false),
                        state("running", "Running", false, false),
                    ],
                    k("idle"),
                )
                .expect("endless: no terminals"),
            ),
            // Engine-only transitions (empty actors): guard/timer armed per
            // SPEC-01 §9 — machine classes never appear alone in a set.
            transition(
                "machine_start",
                "machine_cycle",
                "idle",
                "running",
                Vec::new(),
            ),
            transition(
                "machine_stop",
                "machine_cycle",
                "running",
                "idle",
                Vec::new(),
            ),
            SchemaOp::CreateNodeType(NodeType {
                key: k("machine"),
                display_name: "Machine".into(),
                parent_type: None,
                state_model: k("machine_cycle"),
                attribute_sets: vec![k("machine_meta"), k("machine_table")],
                key_type: k("machine_seq"),
                state_locks: Vec::new(),
            }),
        ],
    }
}

/// `analytics` v1: dataset/series patterns — vectors primary.
pub fn analytics() -> Bundle {
    Bundle {
        name: "analytics",
        version: 1,
        ops: vec![
            SchemaOp::CreateKeyType(KeyTypeDef {
                key: k("series_time"),
                density: Density::Dense,
                axis: AxisKind::Temporal,
            }),
            SchemaOp::CreateEnum(
                EnumType::new(k("sensor_kind"), vec![k("temperature"), k("pressure")])
                    .unwrap_or_else(|e| {
                        panic!("bundle fixture invalid ({}): {e}", "compile-time literal")
                    }),
            ),
            SchemaOp::CreateAttribute(attribute(
                "sensor_label",
                AttrType::Scalar(ScalarType::String),
            )),
            SchemaOp::CreateAttribute(attribute("sensor_kind", AttrType::Enum(k("sensor_kind")))),
            SchemaOp::CreateAttribute(attribute(
                "sensor_readings",
                AttrType::Vector {
                    elem: ElemType::Scalar(ScalarType::F64),
                },
            )),
            SchemaOp::CreateAttributeSet(AttributeSet {
                key: k("sensor_meta"),
                kind: SetKind::ScalarSet,
                members: vec![k("sensor_label"), k("sensor_kind")],
                write_scope: None,
            }),
            SchemaOp::CreateAttributeSet(AttributeSet {
                key: k("sensor_table"),
                kind: SetKind::TableSet,
                members: vec![k("sensor_readings")],
                write_scope: None,
            }),
            SchemaOp::CreateStateModel(
                StateModel::new(
                    k("sensor_life"),
                    ModelClass::Finalizing,
                    vec![
                        state("provisioned", "Provisioned", false, false),
                        state("streaming", "Streaming", false, false),
                        state("decommissioned", "Decommissioned", true, false),
                        state("defective", "Defective", false, true),
                    ],
                    k("provisioned"),
                )
                .expect("terminals + initial valid"),
            ),
            transition(
                "sensor_activate",
                "sensor_life",
                "provisioned",
                "streaming",
                Vec::new(),
            ),
            transition(
                "sensor_decommission",
                "sensor_life",
                "streaming",
                "decommissioned",
                vec![ActorClass::Human],
            ),
            transition(
                "sensor_quarantine",
                "sensor_life",
                "streaming",
                "defective",
                Vec::new(),
            ),
            SchemaOp::CreateNodeType(NodeType {
                key: k("sensor"),
                display_name: "Sensor".into(),
                parent_type: None,
                state_model: k("sensor_life"),
                attribute_sets: vec![k("sensor_meta"), k("sensor_table")],
                key_type: k("series_time"),
                state_locks: Vec::new(),
            }),
            // Analytics needs a default literal (read-time evaluation, never
            // backfill — SPEC-01 §5 absence semantics).
            SchemaOp::AlterAttributeCodec {
                attr_key: k("sensor_readings"),
                codec: "identity".into(),
            },
            SchemaOp::CreateRule(NamedRule {
                key: k("has_label"),
                form: PredicateForm::AttrPresent {
                    attr: k("sensor_label"),
                },
            }),
            SchemaOp::CreateRole(Role {
                key: k("analytics_reader"),
                scopes: vec![Scope::ReadSubtree],
            }),
            SchemaOp::GrantOp(Grant {
                role: k("analytics_reader"),
                subject: "agent:analytics".into(),
                target: GrantTarget::Graph(k("analytics")),
            }),
            SchemaOp::CreateGraph(GraphSchema {
                key: k("analytics"),
                display_name: "Analytics".into(),
                created_at_us: 0,
            }),
            // Default literal demo: label defaults to empty string at read.
            SchemaOp::CreateAttribute(AttributeDef {
                key: k("sensor_note"),
                attr_type: AttrType::Scalar(ScalarType::String),
                required: false,
                default: Some(Value::Str("".into())),
                codec_override: None,
                monotonicity: Monotonicity::None,
                write_scope: None,
            }),
        ],
    }
}

/// Enumerate the shipped bundles (v1 set, SPEC-01 §17).
pub fn shipped() -> Vec<Bundle> {
    vec![process_mgmt(), state_driver(), analytics()]
}

/// Errors of the installer (bundle-level; op-level verdicts ride in
/// [`InstallError::Rejected`]).
#[derive(Debug, Clone, PartialEq)]
pub enum InstallError {
    UnknownBundle(String),
    /// The atomic schema txn rejected — nothing applied, retry-safe.
    Rejected(lattice_schema::txn::TxnError),
    /// The post-install catalog fails to compile — a bundle bug, never
    /// persisted (commit already rolled back).
    NotCompilable(String),
}

impl std::fmt::Display for InstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstallError::UnknownBundle(name) => {
                write!(
                    f,
                    "unknown bundle `{name}`; shipped: process-mgmt, state-driver, analytics"
                )
            }
            InstallError::Rejected(e) => write!(f, "bundle install rejected (atomic): {e}"),
            InstallError::NotCompilable(e) => {
                write!(f, "bundle produced a non-compilable catalog: {e}")
            }
        }
    }
}

impl std::error::Error for InstallError {}

/// A bundle op is already satisfied when a same-named entity exists in its
/// namespace (idempotence check — SPEC-01 §17: re-install = no-op).
fn satisfied(catalog: &lattice_schema::catalog::Catalog, op: &SchemaOp) -> bool {
    use SchemaOp::*;
    match op {
        CreateGraph(g) => catalog.graphs.contains_key(&g.key),
        DropGraph { .. } => false,
        CreateEnum(e) => catalog.enums.contains_key(&e.key),
        AddEnumValue { enum_key, value } => catalog
            .enums
            .get(enum_key)
            .map(|e| e.values.contains(value))
            .unwrap_or(false),
        DeprecateEnumValue { .. } | RemoveEnumValue { .. } => false,
        CreateRecord(r) => catalog.records.contains_key(&r.key),
        AddRecordField { .. } | RemoveRecordField { .. } => false,
        CreateKeyType(kk) => catalog.key_types.contains_key(&kk.key),
        CreateAttribute(a) => catalog.attributes.contains_key(&a.key),
        AlterAttributeCodec { attr_key, codec } => catalog
            .attributes
            .get(attr_key)
            .map(|a| a.codec_override.as_deref() == Some(codec.as_str()))
            .unwrap_or(false),
        CreateAttributeSet(s) => catalog.attr_sets.contains_key(&s.key),
        CreateStateModel(m) => catalog.state_models.contains_key(&m.key),
        AddState { model_key, state } => catalog
            .state_models
            .get(model_key)
            .map(|m| m.state(&state.key).is_some())
            .unwrap_or(false),
        AddTransition(t) => catalog.transitions.contains_key(&t.key),
        AlterTransitionTiming {
            transition_key,
            timing,
        } => catalog
            .transitions
            .get(transition_key)
            .map(|t| &t.timing == timing)
            .unwrap_or(false),
        DeprecateState { .. } | RemoveState { .. } => false,
        CreateNodeType(n) => catalog.node_types.contains_key(&n.key),
        DeprecateNodeType { .. } => false,
        CreateIndex(i) => catalog.indices.contains_key(&i.key),
        DropIndex { .. } => false,
        CreateRule(r) => catalog.rules.contains_key(&r.key),
        RemoveRule { .. } => false,
        CreateRole(r) => catalog.roles.contains_key(&r.key),
        GrantOp(g) => catalog.grants.iter().any(|existing| {
            existing.role == g.role && existing.subject == g.subject && existing.target == g.target
        }),
        Revoke { .. } => false,
    }
}

/// Install a bundle into `state`: skip satisfied ops, apply the rest as ONE
/// atomic txn (resumable: failure leaves nothing applied). Every accepted
/// install must leave a compilable catalog (P3).
pub fn install(state: &mut SchemaState, bundle: &Bundle) -> Result<InstallReport, InstallError> {
    let epoch_before = state.catalog.schema_epoch;
    let (pending, skipped): (Vec<SchemaOp>, Vec<SchemaOp>) = bundle
        .ops
        .iter()
        .cloned()
        .partition(|op| !satisfied(&state.catalog, op));
    if pending.is_empty() {
        return Ok(InstallReport {
            name: bundle.name,
            version: bundle.version,
            applied: 0,
            skipped: skipped.len(),
            epoch_before,
            epoch_after: epoch_before,
            installed: false,
        });
    }
    state
        .commit(pending.clone())
        .map_err(InstallError::Rejected)?;
    compile(&state.catalog).map_err(|e| InstallError::NotCompilable(e.to_string()))?;
    Ok(InstallReport {
        name: bundle.name,
        version: bundle.version,
        applied: pending.len(),
        skipped: skipped.len(),
        epoch_before,
        epoch_after: state.catalog.schema_epoch,
        installed: true,
    })
}

/// Install a shipped bundle by name.
pub fn install_named(state: &mut SchemaState, name: &str) -> Result<InstallReport, InstallError> {
    let bundle = shipped()
        .into_iter()
        .find(|b| b.name == name)
        .ok_or_else(|| InstallError::UnknownBundle(name.to_string()))?;
    install(state, &bundle)
}

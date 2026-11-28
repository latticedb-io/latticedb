//! Shared fixture for the kernel acceptance tests (T-313): a small but
//! complete catalog — composition forest, finalizing and endless models,
//! scalar / increasing / record / enum / node_ref / vector attributes, a
//! table set, rules, user and engine-armed transitions, a spawn effect —
//! and a state digest for replay-identity assertions.
#![allow(dead_code)]

use lattice_core::ids::{EdgeId, NodeId};
use lattice_core::value::Value;
use lattice_schema::catalog::Catalog;
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use lattice_schema::txn::{SchemaOp, SchemaState};
use lattice_tx::{Command, EngineState, Kernel, KernelLimits, KernelOptions, Reply, Subject};
use std::sync::atomic::{AtomicU64, Ordering};

pub fn key(s: &str) -> Key {
    Key::parse(s).unwrap()
}

static DIR_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh, private database directory.
pub fn test_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "latticedb-kernel-{name}-{}-{}",
        std::process::id(),
        DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Explicit limits — nothing hidden in a default.
pub fn limits() -> KernelLimits {
    KernelLimits {
        journal_max_segment_bytes: 1 << 20,
        watch_buffer_events: 4096,
        reader_grace_secs: 0,
        node_read_preview_cells: 4,
    }
}

/// A microsecond clock the test advances by hand — never wall-clock.
pub type TestClock = std::sync::Arc<std::sync::atomic::AtomicU64>;

pub fn test_clock() -> TestClock {
    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1_000))
}

/// Kernel options over a test clock the test controls.
pub fn options_with(clock: TestClock) -> KernelOptions {
    KernelOptions {
        limits: limits(),
        cursor_idle_ttl_us: 60_000_000,
        clock: std::sync::Arc::new(move || clock.load(std::sync::atomic::Ordering::Relaxed)),
    }
}

/// Kernel options with a clock that never moves (no cursor ever expires).
pub fn options() -> KernelOptions {
    options_with(test_clock())
}

pub fn subject() -> Subject {
    Subject::trusted("test-subject")
}

pub fn state(k: &str, name: &str, success: bool, failure: bool) -> State {
    State {
        key: key(k),
        name: name.into(),
        success_terminal: success,
        failure_terminal: failure,
    }
}

fn attr(k: &str, ty: AttrType, monotonicity: Monotonicity) -> SchemaOp {
    SchemaOp::CreateAttribute(AttributeDef {
        key: key(k),
        attr_type: ty,
        required: false,
        default: None,
        codec_override: None,
        monotonicity,
        write_scope: None,
    })
}

fn vector(k: &str, elem: ElemType) -> SchemaOp {
    attr(k, AttrType::Vector { elem }, Monotonicity::None)
}

fn node_type(k: &str, parent: Option<&str>, model: &str, sets: Vec<&str>) -> SchemaOp {
    SchemaOp::CreateNodeType(NodeType {
        key: key(k),
        display_name: k.to_string(),
        parent_type: parent.map(key),
        state_model: key(model),
        attribute_sets: sets.into_iter().map(key).collect(),
        key_type: key("ordinal"),
        state_locks: Vec::new(),
    })
}

#[allow(clippy::too_many_arguments)]
pub fn transition(
    k: &str,
    model: &str,
    from: &str,
    to: &str,
    requirements: Vec<&str>,
    actors: Vec<ActorClass>,
    effects: Vec<Effect>,
) -> SchemaOp {
    SchemaOp::AddTransition(
        Transition::new(
            key(k),
            key(model),
            key(from),
            key(to),
            requirements.into_iter().map(key).collect(),
            actors,
            None,
            TransitionTiming::default(),
            effects,
        )
        .unwrap(),
    )
}

/// The fixture catalog as one schema txn's ops.
pub fn schema_ops() -> Vec<SchemaOp> {
    vec![
        SchemaOp::CreateGraph(GraphSchema {
            key: key("pm"),
            display_name: "Project management".into(),
            created_at_us: 0,
        }),
        SchemaOp::CreateKeyType(KeyTypeDef {
            key: key("ordinal"),
            density: Density::Dense,
            axis: AxisKind::Ordinal,
        }),
        SchemaOp::CreateEnum(EnumType::new(key("prio"), vec![key("low"), key("high")]).unwrap()),
        SchemaOp::CreateRecord(
            RecordType::new(
                key("evidence"),
                vec![
                    (key("f0"), ElemType::Scalar(ScalarType::I64)),
                    (key("f1"), ElemType::Scalar(ScalarType::String)),
                ],
            )
            .unwrap(),
        ),
        SchemaOp::CreateStateModel(
            StateModel::new(
                key("task_flow"),
                ModelClass::Finalizing,
                vec![
                    state("open", "Open", false, false),
                    state("running", "Running", false, false),
                    state("done", "Done", true, false),
                    state("failed", "Failed", false, true),
                ],
                key("open"),
            )
            .unwrap(),
        ),
        SchemaOp::CreateStateModel(
            StateModel::new(
                key("sub_flow"),
                ModelClass::Endless,
                vec![state("live", "Live", false, false)],
                key("live"),
            )
            .unwrap(),
        ),
        attr(
            "title",
            AttrType::Scalar(ScalarType::String),
            Monotonicity::None,
        ),
        attr(
            "amount",
            AttrType::Scalar(ScalarType::I64),
            Monotonicity::Increasing,
        ),
        attr(
            "owner",
            AttrType::Scalar(ScalarType::NodeRef),
            Monotonicity::None,
        ),
        attr(
            "evidence",
            AttrType::Record(key("evidence")),
            Monotonicity::None,
        ),
        attr("prio", AttrType::Enum(key("prio")), Monotonicity::None),
        attr(
            "budget",
            AttrType::Scalar(ScalarType::Decimal {
                precision: 10,
                scale: 2,
            }),
            Monotonicity::None,
        ),
        attr(
            "due_at",
            AttrType::Scalar(ScalarType::Timestamp),
            Monotonicity::None,
        ),
        attr(
            "sla",
            AttrType::Scalar(ScalarType::Duration),
            Monotonicity::None,
        ),
        attr(
            "blob",
            AttrType::Scalar(ScalarType::Bytes),
            Monotonicity::None,
        ),
        vector("log", ElemType::Scalar(ScalarType::I64)),
        vector("reading", ElemType::Scalar(ScalarType::F64)),
        SchemaOp::CreateAttributeSet(AttributeSet {
            key: key("task_meta"),
            kind: SetKind::ScalarSet,
            members: [
                "title", "amount", "owner", "evidence", "prio", "budget", "due_at", "sla", "blob",
            ]
            .into_iter()
            .map(key)
            .collect(),
            write_scope: None,
        }),
        SchemaOp::CreateAttributeSet(AttributeSet {
            key: key("task_table"),
            kind: SetKind::TableSet,
            members: vec![key("log"), key("reading")],
            write_scope: None,
        }),
        SchemaOp::CreateRule(NamedRule {
            key: key("has_title"),
            form: PredicateForm::AttrPresent { attr: key("title") },
        }),
        SchemaOp::CreateRule(NamedRule {
            key: key("amount_gte_10"),
            form: PredicateForm::AttrCmp {
                attr: key("amount"),
                op: CmpOp::Gte,
                value: Value::I64(10),
            },
        }),
        SchemaOp::CreateRule(NamedRule {
            key: key("owner_set"),
            form: PredicateForm::AttrPresent { attr: key("owner") },
        }),
        node_type("project", None, "task_flow", vec!["task_meta"]),
        node_type(
            "task",
            Some("project"),
            "task_flow",
            vec!["task_meta", "task_table"],
        ),
        node_type("subtask", Some("task"), "sub_flow", vec!["task_meta"]),
        // Locked while `running`: only the table set is writable (SGT-7.3).
        SchemaOp::CreateNodeType(NodeType {
            key: key("sealed_task"),
            display_name: "Sealed task".into(),
            parent_type: Some(key("project")),
            state_model: key("task_flow"),
            attribute_sets: vec![key("task_meta"), key("task_table")],
            key_type: key("ordinal"),
            state_locks: vec![StateLock {
                state: key("running"),
                writable_sets: vec![key("task_table")],
            }],
        }),
        transition(
            "start",
            "task_flow",
            "open",
            "running",
            vec!["has_title"],
            vec![ActorClass::Human],
            Vec::new(),
        ),
        transition(
            "agent_start",
            "task_flow",
            "open",
            "running",
            vec!["has_title"],
            vec![ActorClass::Agent],
            Vec::new(),
        ),
        transition(
            "complete",
            "task_flow",
            "running",
            "done",
            vec!["amount_gte_10"],
            vec![ActorClass::Human],
            Vec::new(),
        ),
        transition(
            "fail",
            "task_flow",
            "running",
            "failed",
            Vec::new(),
            vec![ActorClass::Human],
            Vec::new(),
        ),
        // Two requirements — WPT-7.4's "2 unmet rules" shape.
        transition(
            "close",
            "task_flow",
            "running",
            "done",
            vec!["amount_gte_10", "owner_set"],
            vec![ActorClass::Human],
            Vec::new(),
        ),
        // Engine-armed (no invokable actor): the GUARD edge's target.
        transition(
            "auto_done",
            "task_flow",
            "running",
            "done",
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
        transition(
            "spawn_sub",
            "task_flow",
            "open",
            "running",
            Vec::new(),
            vec![ActorClass::Human],
            vec![Effect::Spawn {
                instance_of: key("subtask"),
            }],
        ),
    ]
}

/// A fresh database with the fixture catalog installed (one schema txn).
pub fn kernel(name: &str) -> Kernel {
    let dir = test_dir(name);
    let mut kernel = Kernel::create(&dir, 1, options()).expect("create database");
    let reply = kernel
        .execute(0, &subject(), &Command::SchemaTxn { ops: schema_ops() })
        .expect("install fixture schema");
    assert_eq!(reply, Reply::Schema { epoch: 1 });
    kernel
}

/// The database directory of a kernel made by [`kernel`] is the caller's:
/// tests that reopen keep the path they passed to `Kernel::create`.
pub fn reopen(dir: &std::path::Path) -> Kernel {
    Kernel::open(dir, options()).expect("reopen database").0
}

/// Everything the in-memory state holds, as comparable text: catalog epoch,
/// per-node structure / type / state / epoch / attr count, the edge table,
/// and every cell with its attr id and LSN.
pub fn digest(state: &EngineState) -> String {
    let machine = state.machine();
    let arrays = state.arrays();
    // Op count, not txn count: a snapshot squashes the pre-checkpoint txns
    // into one op-log blob, and the logical state is the same either way.
    let schema_ops: usize = state.schema.ops_log.iter().map(Vec::len).sum();
    let mut out = format!(
        "schema_epoch={} schema_ops={schema_ops}\n",
        state.schema.catalog.schema_epoch
    );
    for id in 0..arrays.len() as u32 {
        let node = NodeId(id);
        out.push_str(&format!(
            "n{} alive={} parent={:?} graph={} type={:?} state={} epoch={} attrs={}\n",
            id,
            arrays.is_alive(node),
            arrays.parent_of(node),
            arrays.graph_of(node).0,
            machine.node_type_key(arrays.type_of(node)),
            arrays.state_of(node),
            arrays.epoch_of(node).0,
            arrays.attr_count_of(node),
        ));
    }
    for i in 0..arrays.edge_slots() {
        let rec = arrays.edge_slot(EdgeId(i as u32)).unwrap();
        out.push_str(&format!(
            "e{} {}->{} kind={} aux={} alive={}\n",
            i, rec.src.0, rec.dst.0, rec.kind, rec.aux, rec.alive
        ));
    }
    let mut cells: Vec<String> = machine
        .cells()
        .map(|((node, attr, k), cell)| {
            format!(
                "c{}:{}:{}={:?}@{}",
                node.0, attr.0, k.0, cell.value, cell.lsn.0
            )
        })
        .collect();
    cells.sort();
    out.push_str(&cells.join("\n"));
    out
}

/// The catalog of a fresh database, for callers building their own schema.
pub fn fresh_schema() -> SchemaState {
    SchemaState::new(Catalog::fresh())
}

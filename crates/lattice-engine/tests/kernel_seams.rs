//! T-313 engine seams: the read-only / mutating split the kernel's write
//! path rests on (ARCHITECTURE §5.1: plan → journal → apply), and the
//! helpers recovery needs.
//!
//! - `check_action` performs every check and mutates nothing.
//! - A rejected action leaves NO partial effect — not even behind an earlier
//!   effect of the same transition.
//! - Node-type codes are the caller's (lineage-stable) and survive a recompile.
//! - A created node enters its model's `initial` state even when that is not
//!   the first declared state.
//! - Edge removal forgets GUARD params; aux ids are never reused.

use lattice_core::arrays::GraphArrays;
use lattice_core::ids::{NodeId, NodeIdSpace};
use lattice_core::value::Value;
use lattice_engine::dispatch::{DispatchError, Machine};
use lattice_schema::catalog::Catalog;
use lattice_schema::compile::compile;
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use lattice_schema::txn::{SchemaOp, SchemaState};
use std::collections::BTreeMap;

fn key(s: &str) -> Key {
    Key::parse(s).unwrap()
}

fn state(k: &str, success: bool, failure: bool) -> State {
    State {
        key: key(k),
        name: k.to_string(),
        success_terminal: success,
        failure_terminal: failure,
    }
}

fn transition(k: &str, requirements: Vec<Key>, effects: Vec<Effect>) -> SchemaOp {
    SchemaOp::AddTransition(
        Transition::new(
            key(k),
            key("flow"),
            key("open"),
            key("running"),
            requirements,
            vec![ActorClass::Human],
            None,
            TransitionTiming::default(),
            effects,
        )
        .unwrap(),
    )
}

/// `flow` declares `done` FIRST and `open` (the initial state) SECOND — a
/// created node must not simply take state code 0.
fn fixture() -> (SchemaState, Machine) {
    let mut schema = SchemaState::new(Catalog::fresh());
    let ops = vec![
        SchemaOp::CreateKeyType(KeyTypeDef {
            key: key("ordinal"),
            density: Density::Dense,
            axis: AxisKind::Ordinal,
        }),
        SchemaOp::CreateStateModel(
            StateModel::new(
                key("flow"),
                ModelClass::Finalizing,
                vec![
                    state("done", true, false),
                    state("open", false, false),
                    state("running", false, false),
                    state("failed", false, true),
                ],
                key("open"),
            )
            .unwrap(),
        ),
        SchemaOp::CreateStateModel(
            StateModel::new(
                key("sub"),
                ModelClass::Endless,
                vec![state("live", false, false)],
                key("live"),
            )
            .unwrap(),
        ),
        SchemaOp::CreateAttribute(AttributeDef {
            key: key("title"),
            attr_type: AttrType::Scalar(ScalarType::String),
            required: false,
            default: None,
            codec_override: None,
            monotonicity: Monotonicity::None,
            write_scope: None,
        }),
        SchemaOp::CreateAttribute(AttributeDef {
            key: key("amount"),
            attr_type: AttrType::Scalar(ScalarType::I64),
            required: false,
            default: None,
            codec_override: None,
            monotonicity: Monotonicity::None,
            write_scope: None,
        }),
        SchemaOp::CreateAttributeSet(AttributeSet {
            key: key("meta"),
            kind: SetKind::ScalarSet,
            members: vec![key("title"), key("amount")],
            write_scope: None,
        }),
        SchemaOp::CreateRule(NamedRule {
            key: key("has_title"),
            form: PredicateForm::AttrPresent { attr: key("title") },
        }),
        SchemaOp::CreateNodeType(NodeType {
            key: key("task"),
            display_name: "Task".into(),
            parent_type: None,
            state_model: key("flow"),
            attribute_sets: vec![key("meta")],
            key_type: key("ordinal"),
            state_locks: vec![],
        }),
        SchemaOp::CreateNodeType(NodeType {
            key: key("subtask"),
            display_name: "Subtask".into(),
            parent_type: Some(key("task")),
            state_model: key("sub"),
            attribute_sets: vec![],
            key_type: key("ordinal"),
            state_locks: vec![],
        }),
        // Reachability: the compiler rejects states no transition leads to.
        SchemaOp::AddTransition(
            Transition::new(
                key("finish"),
                key("flow"),
                key("running"),
                key("done"),
                Vec::new(),
                vec![ActorClass::Human],
                None,
                TransitionTiming::default(),
                Vec::new(),
            )
            .unwrap(),
        ),
        SchemaOp::AddTransition(
            Transition::new(
                key("fail"),
                key("flow"),
                key("running"),
                key("failed"),
                Vec::new(),
                vec![ActorClass::Human],
                None,
                TransitionTiming::default(),
                Vec::new(),
            )
            .unwrap(),
        ),
        transition("start", vec![key("has_title")], Vec::new()),
        transition(
            "spawn",
            Vec::new(),
            vec![Effect::Spawn {
                instance_of: key("subtask"),
            }],
        ),
        // Spawn FIRST, then a select_branch that cannot succeed without a
        // NodeRef payload.
        transition(
            "spawn_then_fail",
            Vec::new(),
            vec![
                Effect::Spawn {
                    instance_of: key("subtask"),
                },
                Effect::SelectBranch,
            ],
        ),
    ];
    schema.commit(ops).expect("fixture schema");
    let compiled = compile(&schema.catalog).expect("compiles");
    let codes = schema.node_type_codes();
    let machine = Machine::with_type_codes(
        GraphArrays::new(NodeIdSpace::new()),
        compiled,
        &schema.catalog,
        &codes,
    );
    (schema, machine)
}

/// Everything the machine holds, for before/after comparison.
fn shape(machine: &Machine) -> (lattice_core::arrays::RawArrays, usize, usize) {
    (
        machine.arrays.to_raw(),
        machine.cells().count(),
        machine.wheel.len(),
    )
}

#[test]
fn rejected_action_leaves_no_partial_effect() {
    let (_schema, mut machine) = fixture();
    let task = machine
        .arrays
        .add_root(
            lattice_core::ids::GraphId(0),
            machine.node_type_code(&key("task")).unwrap(),
        )
        .unwrap();
    machine.initialize_node(task).unwrap();
    machine
        .set_cell(
            task,
            &key("title"),
            lattice_core::ids::KeyIndex(0),
            Value::str("t").unwrap(),
            lattice_core::ids::Lsn(1),
        )
        .unwrap();
    let before = shape(&machine);
    let watermark = machine.arrays.id_space().watermark();

    let err = machine
        .apply_action(
            task,
            &key("spawn_then_fail"),
            ActorClass::Human,
            "alice",
            None,
        )
        .expect_err("select_branch has no payload");
    assert!(matches!(err, DispatchError::MissingBranchSelection));

    // The spawn effect that precedes the failing one did not happen either.
    assert_eq!(shape(&machine), before);
    assert_eq!(
        machine.arrays.id_space().watermark(),
        watermark,
        "no node id consumed"
    );
    assert_eq!(machine.arrays.children_of(task).len(), 0);
}

#[test]
fn check_action_mutates_nothing_and_apply_does_the_work() {
    let (_schema, mut machine) = fixture();
    let task = machine
        .arrays
        .add_root(
            lattice_core::ids::GraphId(0),
            machine.node_type_code(&key("task")).unwrap(),
        )
        .unwrap();
    machine.initialize_node(task).unwrap();
    let before = shape(&machine);

    // Requirement unmet: refused by the check …
    assert!(matches!(
        machine.check_action(task, &key("start"), ActorClass::Human, None),
        Err(DispatchError::RequirementsUnmet { .. })
    ));
    // … and by apply, with the same refusal.
    assert!(matches!(
        machine.apply_action(task, &key("start"), ActorClass::Human, "alice", None),
        Err(DispatchError::RequirementsUnmet { .. })
    ));
    assert_eq!(shape(&machine), before, "a refused action changes nothing");

    // A valid spawn: the check reports the spawn and touches nothing.
    let check = machine
        .check_action(task, &key("spawn"), ActorClass::Human, None)
        .expect("valid");
    assert_eq!(check.spawns.len(), 1);
    assert_eq!(check.spawns[0].0, key("subtask"));
    assert_eq!(shape(&machine), before, "check_action is read-only");
    assert_eq!(
        machine.peek_next_node_id(),
        NodeId(1),
        "the id the apply will allocate"
    );

    // The apply does it — once.
    let outcome = machine
        .apply_action(task, &key("spawn"), ActorClass::Human, "alice", None)
        .expect("applies");
    assert_eq!(outcome.spawned, Some(NodeId(1)));
    assert_ne!(shape(&machine), before);
    assert_eq!(machine.state_key_of(task).unwrap(), key("running"));
}

#[test]
fn type_codes_come_from_the_caller_and_survive_a_recompile() {
    let (schema, machine) = fixture();
    // Deliberately NOT catalog order: task = 7, subtask = 2.
    let codes: BTreeMap<Key, u16> = [(key("task"), 7), (key("subtask"), 2)]
        .into_iter()
        .collect();
    let compiled = compile(&schema.catalog).unwrap();
    let mut machine = Machine::with_type_codes(machine.arrays, compiled, &schema.catalog, &codes);
    let task = machine
        .arrays
        .add_root(
            lattice_core::ids::GraphId(0),
            machine.node_type_code(&key("task")).unwrap(),
        )
        .unwrap();
    assert_eq!(machine.node_type_code(&key("task")), Some(7));
    assert_eq!(machine.arrays.type_of(task), 7);

    // A new epoch (same catalog here) keeps the caller's codes.
    let fresh = compile(&schema.catalog).unwrap();
    machine.recompile_in_place(fresh, &schema.catalog, &codes);
    assert_eq!(machine.node_type_code(&key("task")), Some(7));
    assert_eq!(machine.node_type_key(7), Some(&key("task")));
    assert_eq!(machine.node_type_key(2), Some(&key("subtask")));
    assert_eq!(machine.arrays.type_of(task), 7);
}

#[test]
fn initialize_node_enters_the_models_initial_state() {
    let (_schema, mut machine) = fixture();
    let task = machine
        .arrays
        .add_root(
            lattice_core::ids::GraphId(0),
            machine.node_type_code(&key("task")).unwrap(),
        )
        .unwrap();
    // Creation alone leaves code 0 = `done`, a terminal — never a start state.
    assert_eq!(machine.state_key_of(task).unwrap(), key("done"));
    machine.initialize_node(task).unwrap();
    assert_eq!(
        machine.state_key_of(task).unwrap(),
        key("open"),
        "the declared initial state"
    );
    // A terminal node cannot act; the initialized one can.
    assert!(
        machine
            .check_action(task, &key("spawn"), ActorClass::Human, None)
            .is_ok()
    );
}

#[test]
fn remove_edge_forgets_guard_params_and_aux_ids_are_never_reused() {
    let (_schema, mut machine) = fixture();
    let a = machine
        .arrays
        .add_root(
            lattice_core::ids::GraphId(0),
            machine.node_type_code(&key("task")).unwrap(),
        )
        .unwrap();
    let b = machine
        .arrays
        .add_root(
            lattice_core::ids::GraphId(0),
            machine.node_type_code(&key("task")).unwrap(),
        )
        .unwrap();
    machine.initialize_node(a).unwrap();
    machine.initialize_node(b).unwrap();

    let aux = machine
        .add_guard_edge(a, b, &key("open"), &key("start"))
        .unwrap();
    assert_eq!(aux, 0);
    assert_eq!(
        machine.guard_tau_for(a, b, &key("open")),
        Some(key("start"))
    );
    let eid = lattice_core::ids::EdgeId(machine.arrays.edge_slots() as u32 - 1);

    machine.remove_edge(eid).unwrap();
    assert_eq!(
        machine.guard_tau_for(a, b, &key("open")),
        None,
        "params do not outlive the edge"
    );

    // The next guard edge gets a fresh aux; restoring an old one never
    // lowers the allocator.
    let aux2 = machine
        .add_guard_edge(a, b, &key("open"), &key("start"))
        .unwrap();
    assert_eq!(aux2, 1);
    machine.restore_guard_param(0, key("open"), key("start"));
    assert_eq!(machine.peek_next_guard_aux(), 2);
}

#[test]
fn node_ref_holder_sees_live_referencers_only() {
    let (_schema, mut machine) = fixture();
    let code = machine.node_type_code(&key("task")).unwrap();
    let target = machine
        .arrays
        .add_root(lattice_core::ids::GraphId(0), code)
        .unwrap();
    let holder = machine
        .arrays
        .add_root(lattice_core::ids::GraphId(0), code)
        .unwrap();
    machine.initialize_node(target).unwrap();
    machine.initialize_node(holder).unwrap();
    machine
        .set_cell(
            holder,
            &key("title"),
            lattice_core::ids::KeyIndex(0),
            Value::str("h").unwrap(),
            lattice_core::ids::Lsn(1),
        )
        .unwrap();
    let targets = std::collections::BTreeSet::from([target]);
    assert_eq!(machine.node_ref_holder(&targets), None);

    machine
        .set_cell(
            holder,
            &key("amount"),
            lattice_core::ids::KeyIndex(0),
            Value::NodeRef(target),
            lattice_core::ids::Lsn(2),
        )
        .unwrap();
    assert_eq!(machine.node_ref_holder(&targets), Some(holder));

    // Cells die with their node, so a deleted holder no longer references.
    machine.arrays.delete_node(holder).unwrap();
    machine.drop_cells(holder);
    assert_eq!(
        machine.node_ref_holder(&targets),
        None,
        "a deleted holder's cells are gone"
    );
}

#[test]
fn drop_cells_removes_exactly_that_nodes_cells() {
    let (_schema, mut machine) = fixture();
    let code = machine.node_type_code(&key("task")).unwrap();
    let a = machine
        .arrays
        .add_root(lattice_core::ids::GraphId(0), code)
        .unwrap();
    let b = machine
        .arrays
        .add_root(lattice_core::ids::GraphId(0), code)
        .unwrap();
    for (node, text) in [(a, "a"), (b, "b")] {
        machine
            .set_cell(
                node,
                &key("title"),
                lattice_core::ids::KeyIndex(0),
                Value::str(text).unwrap(),
                lattice_core::ids::Lsn(1),
            )
            .unwrap();
        machine
            .set_cell(
                node,
                &key("amount"),
                lattice_core::ids::KeyIndex(0),
                Value::I64(1),
                lattice_core::ids::Lsn(2),
            )
            .unwrap();
    }
    assert_eq!(machine.cells().count(), 4);
    assert_eq!(machine.drop_cells(a), 2);
    assert_eq!(machine.cells().count(), 2);
    assert_eq!(
        machine.latest_cell(b, &key("title")).unwrap().value,
        Value::str("b").unwrap()
    );
    assert_eq!(machine.latest_cell(a, &key("title")), None);
}

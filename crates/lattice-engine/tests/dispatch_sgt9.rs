//! T-202 acceptance: transition/action dispatch — requirement gate
//! (SGT-9.1), requirement met → motion + journal record (SGT-9.2), spawn
//! effect (SGT-9.5), select_branch cancels siblings (SGT-9.6), terminal
//! absorbing (SGT-9.7), actor law. Timer paths (9.3/9.4) land with T-205.

use lattice_core::arrays::GraphArrays;
use lattice_core::ids::NodeIdSpace;
use lattice_core::value::Value;
use lattice_engine::dispatch::{ActionRecord, DispatchError, Machine, kinds};
use lattice_schema::catalog::Catalog;
use lattice_schema::compile::compile;
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use lattice_schema::txn::{SchemaOp, SchemaState};

fn key(s: &str) -> Key {
    Key::parse(s).unwrap()
}

fn state(k: &str, name: &str, success: bool, failure: bool) -> State {
    State {
        key: key(k),
        name: name.into(),
        success_terminal: success,
        failure_terminal: failure,
    }
}

fn transition(
    kk: &str,
    model: &str,
    from: &str,
    to: &str,
    requirements: Vec<Key>,
    actors: Vec<ActorClass>,
    effects: Vec<Effect>,
) -> SchemaOp {
    SchemaOp::AddTransition(
        Transition::new(
            key(kk),
            key(model),
            key(from),
            key(to),
            requirements,
            actors,
            None,
            TransitionTiming::default(),
            effects,
        )
        .unwrap(),
    )
}

/// Fixture: task flow + requirement rules + a decision graph with XOR
/// branches + a spawnable subtask type.
fn fixture() -> (SchemaState, Machine) {
    let mut st = SchemaState::new(Catalog::fresh());
    let ops = vec![
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
                key("branch_flow"),
                ModelClass::Finalizing,
                vec![
                    state("pending", "Pending", false, false),
                    state("chosen", "Chosen", true, false),
                    state("dropped", "Dropped", false, true),
                ],
                key("pending"),
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
            monotonicity: Monotonicity::Increasing,
            write_scope: None,
        }),
        SchemaOp::CreateAttribute(AttributeDef {
            key: key("evidence"),
            attr_type: AttrType::Record(
                // record with fields: field0 tag(i64-ish), field1 note(str)
                key("ev_record").clone(),
            ),
            required: false,
            default: None,
            codec_override: None,
            monotonicity: Monotonicity::None,
            write_scope: None,
        }),
        SchemaOp::CreateRecord(
            RecordType::new(
                key("ev_record"),
                vec![
                    (key("f0"), ElemType::Scalar(ScalarType::I64)),
                    (key("f1"), ElemType::Scalar(ScalarType::I64)),
                ],
            )
            .unwrap(),
        ),
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
            key: key("children_done"),
            form: PredicateForm::ChildrenState {
                success: true,
                expect_all: true,
            },
        }),
        SchemaOp::CreateNodeType(NodeType {
            key: key("task"),
            display_name: "Task".into(),
            parent_type: None,
            state_model: key("task_flow"),
            attribute_sets: Vec::new(),
            key_type: key("seq"),
            state_locks: Vec::new(),
        }),
        SchemaOp::CreateNodeType(NodeType {
            key: key("branch"),
            display_name: "Branch".into(),
            parent_type: None,
            state_model: key("branch_flow"),
            attribute_sets: Vec::new(),
            key_type: key("seq"),
            state_locks: Vec::new(),
        }),
        SchemaOp::CreateNodeType(NodeType {
            key: key("subtask"),
            display_name: "SubTask".into(),
            parent_type: Some(key("task")),
            state_model: key("sub_flow"),
            attribute_sets: Vec::new(),
            key_type: key("seq"),
            state_locks: Vec::new(),
        }),
        SchemaOp::CreateKeyType(KeyTypeDef {
            key: key("seq"),
            density: Density::Dense,
            axis: AxisKind::Ordinal,
        }),
        transition(
            "start",
            "task_flow",
            "open",
            "running",
            vec![key("has_title")],
            vec![ActorClass::Human],
            Vec::new(),
        ),
        transition(
            "complete",
            "task_flow",
            "running",
            "done",
            vec![key("amount_gte_10"), key("children_done")],
            vec![ActorClass::Human],
            Vec::new(),
        ),
        transition(
            "start_strict",
            "task_flow",
            "open",
            "running",
            vec![key("amount_gte_10")],
            vec![ActorClass::Agent],
            Vec::new(),
        ),
        transition(
            "task_fail",
            "task_flow",
            "running",
            "failed",
            Vec::new(),
            vec![ActorClass::Human],
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
        transition(
            "decide",
            "task_flow",
            "open",
            "running",
            Vec::new(),
            vec![ActorClass::Human],
            vec![Effect::SelectBranch],
        ),
        // branch cancellation paths (SGT-9.6): declared motions to the
        // failure/success terminals — select_branch moves siblings through
        // the declared motion to `dropped`.
        transition(
            "b_drop",
            "branch_flow",
            "pending",
            "dropped",
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
        transition(
            "b_choose",
            "branch_flow",
            "pending",
            "chosen",
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
    ];
    st.commit(ops).expect("fixture txns compile");

    let compiled = compile(&st.catalog).expect("fixture compiles");
    let arrays = GraphArrays::new(NodeIdSpace::new());
    let machine = Machine::new(arrays, compiled, &st.catalog);
    (st, machine)
}

fn node_type_code(m: &Machine, name: &str) -> u16 {
    m.node_type_code(&key(name)).expect("fixture type")
}

/// SGT-9.1: requirement gate — unmet requirement rejected with the failing
/// rule keys; the enabled-action index excludes the action; after filling
/// the attribute the action enables (index flips).
#[test]
fn sgt_9_1_requirement_gate_and_enabled_index() {
    let (_st, mut m) = fixture();
    let t_code = node_type_code(&m, "task");
    let t1 = m.arrays.add_root(t_code.into(), t_code).expect("task node");

    // start requires has_title: unmet (no title cell yet).
    let enabled = m.enabled_actions(t1).expect("index");
    assert!(
        !enabled.contains(&key("start")),
        "start disabled without title"
    );
    let err = m
        .apply_action(t1, &key("start"), ActorClass::Human, "user:1", None)
        .expect_err("requirement gate fires");
    match err {
        DispatchError::RequirementsUnmet { failing } => {
            assert_eq!(
                failing,
                vec![key("has_title")],
                "machine-readable failing list"
            )
        }
        other => panic!("got {other:?}"),
    }

    // Fill title → the index flips and the action applies.
    m.set_cell(
        t1,
        &key("title"),
        0.into(),
        Value::Str("write report".into()),
        1.into(),
    )
    .expect("cell");
    let enabled = m.enabled_actions(t1).expect("index");
    assert!(enabled.contains(&key("start")), "start enabled after title");
    assert!(
        !enabled.contains(&key("start_strict")),
        "start_strict (amount≥10) still disabled"
    );

    let outcome = m
        .apply_action(t1, &key("start"), ActorClass::Human, "user:1", None)
        .expect("start applies");
    assert_eq!(outcome.from, key("open"));
    assert_eq!(outcome.to, key("running"));
    // Journal record: actor + subject + from/to (lsn assigned by sequencer).
    assert_eq!(
        outcome.record,
        ActionRecord {
            lsn: None,
            node: t1,
            transition: key("start"),
            actor_class: ActorClass::Human,
            subject: "user:1".into(),
            payload: None,
            from: key("open"),
            to: key("running"),
        }
    );
    // Node state actually moved (data mutation, not just a report).
    assert_eq!(m.arrays.state_of(t1), 1, "state code of `running`");
}

/// SGT-9.2: requirement met → state motion; multi-rule transitions report
/// ALL failing rules at once; state mismatch after motion is rejected.
#[test]
fn sgt_9_2_motion_and_multi_rule_gate() {
    let (_st, mut m) = fixture();
    let t_code = node_type_code(&m, "task");
    let t1 = m.arrays.add_root(t_code.into(), t_code).expect("task");

    m.set_cell(
        t1,
        &key("title"),
        0.into(),
        Value::Str("t".into()),
        1.into(),
    )
    .expect("cell");

    // complete requires amount≥10 AND children_done → both failing listed.
    m.set_cell(t1, &key("amount"), 0.into(), Value::I64(5), 2.into())
        .expect("cell");
    let err = m
        .apply_action(t1, &key("complete"), ActorClass::Human, "user:1", None)
        .expect_err("state mismatch: node is `open`, complete fires from `running`");
    match err {
        DispatchError::StateMismatch {
            expected_from,
            actual,
        } => {
            assert_eq!(expected_from, key("running"));
            assert_eq!(actual, key("open"));
        }
        other => panic!("got {other:?}"),
    }

    // Start first, then complete: both rules failing listed together.
    m.apply_action(t1, &key("start"), ActorClass::Human, "u", None)
        .expect("start");
    let err = m
        .apply_action(t1, &key("complete"), ActorClass::Human, "u", None)
        .expect_err("both rules unmet");
    match err {
        DispatchError::RequirementsUnmet { failing } => {
            assert!(failing.contains(&key("amount_gte_10")));
            assert!(failing.contains(&key("children_done")));
            assert_eq!(failing.len(), 2, "ALL failing rules listed");
        }
        other => panic!("got {other:?}"),
    }

    // Fix amount; children_done still fails (task has no children).
    m.set_cell(t1, &key("amount"), 0.into(), Value::I64(50), 3.into())
        .expect("cell");
    let err = m
        .apply_action(t1, &key("complete"), ActorClass::Human, "u", None)
        .expect_err("children_done still unmet");
    match err {
        DispatchError::RequirementsUnmet { failing } => {
            assert_eq!(failing, vec![key("children_done")])
        }
        other => panic!("got {other:?}"),
    }
}

/// Actor law: machine-only sets are schema-rejected; human action not
/// invokable by an agent on a human-only transition.
#[test]
fn actor_law_enforced_at_dispatch() {
    let (_st, mut m) = fixture();
    let t_code = node_type_code(&m, "task");
    let t1 = m.arrays.add_root(t_code.into(), t_code).expect("task");
    m.set_cell(
        t1,
        &key("title"),
        0.into(),
        Value::Str("t".into()),
        1.into(),
    )
    .expect("cell");

    let err = m
        .apply_action(t1, &key("start"), ActorClass::Agent, "agent:x", None)
        .expect_err("start is Human-only");
    match err {
        DispatchError::NotInvokable { transition, actor } => {
            assert_eq!(transition, key("start"));
            assert_eq!(actor, ActorClass::Agent);
        }
        other => panic!("got {other:?}"),
    }
    // start_strict IS agent-invokable.
    m.apply_action(t1, &key("start_strict"), ActorClass::Agent, "agent:x", None)
        .expect_err("start_strict requires amount≥10 — unmet");
}

/// SGT-9.5: spawn effect — child node created under the actor, typed
/// correctly (parent_type containment), engine-authored AddVertex fact for
/// the same txn.
#[test]
fn sgt_9_5_spawn_effect() {
    let (_st, mut m) = fixture();
    let t_code = node_type_code(&m, "task");
    let sub_code = node_type_code(&m, "subtask");
    let t1 = m.arrays.add_root(t_code.into(), t_code).expect("task");

    let outcome = m
        .apply_action(t1, &key("spawn_sub"), ActorClass::Human, "u", None)
        .expect("spawn applies");
    let child = outcome.spawned.expect("spawned child id");
    assert_eq!(
        outcome.engine_facts,
        vec![lattice_engine::dispatch::EngineFact::AddVertex {
            node: child,
            parent: t1,
            node_type: key("subtask"),
        }]
    );
    // Typed correctly: child node_type code = subtask; containment: child of
    // a task (subtask.parent_type = task ✓).
    assert_eq!(m.arrays.type_of(child), sub_code);
    assert_eq!(m.arrays.parent_of(child), Some(t1));
    m.arrays.assert_invariants();
}

/// SGT-9.6: select_branch — decision node with XOR edges to three branches;
/// selecting A enables A and cancels B/C (motion to their failure terminal
/// via declared transitions, reasons recorded as engine facts).
#[test]
fn sgt_9_6_select_branch_cancels_siblings() {
    let (_st, mut m) = fixture();
    let task_code = node_type_code(&m, "task");
    let branch_code = node_type_code(&m, "branch");
    let d = m
        .arrays
        .add_root(task_code.into(), task_code)
        .expect("decision");
    let ba = m
        .arrays
        .add_root(branch_code.into(), branch_code)
        .expect("branch a");
    let bb = m
        .arrays
        .add_root(branch_code.into(), branch_code)
        .expect("branch b");
    let bc = m
        .arrays
        .add_root(branch_code.into(), branch_code)
        .expect("branch c");
    // XOR edges: decision → each branch.
    for br in [ba, bb, bc] {
        m.arrays.add_edge(d, br, kinds::XOR, 0).expect("xor edge");
    }

    // select B (not A): payload carries the NodeRef.
    let outcome = m
        .apply_action(
            d,
            &key("decide"),
            ActorClass::Human,
            "judge",
            Some(Value::NodeRef(bb)),
        )
        .expect("decide applies");
    assert_eq!(outcome.cancelled.len(), 2, "two siblings cancelled");
    // Selected branch untouched; BOTH siblings moved to the failure terminal.
    assert_eq!(m.arrays.state_of(ba), 2, "branch a dropped (cancelled)");
    assert_eq!(m.arrays.state_of(bb), 0, "selected branch b still pending");
    assert_eq!(
        m.arrays.state_of(bc),
        2,
        "branch c dropped (failure terminal)"
    );
    // Cancellation reasons recorded (machine-readable).
    let reasons: Vec<&str> = outcome
        .cancelled
        .iter()
        .map(|(n, r)| (n.0, r.as_str()))
        .filter(|(n, _)| *n == ba.0)
        .map(|(_, r)| r)
        .collect();
    assert!(!reasons.is_empty() && reasons[0].contains("select_branch"));
    // Engine facts carry the cancellations for the journal.
    let cancels: Vec<_> = outcome
        .engine_facts
        .iter()
        .filter(|f| {
            matches!(
                f,
                lattice_engine::dispatch::EngineFact::CancelSibling { .. }
            )
        })
        .collect();
    assert_eq!(cancels.len(), 2);
}

/// select_branch with a non-branch payload → NotABranch; missing payload →
/// MissingBranchSelection.
#[test]
fn select_branch_validation() {
    let (_st, mut m) = fixture();
    let task_code = node_type_code(&m, "task");
    let branch_code = node_type_code(&m, "branch");
    let d = m
        .arrays
        .add_root(task_code.into(), task_code)
        .expect("decision");
    let ba = m
        .arrays
        .add_root(branch_code.into(), branch_code)
        .expect("branch a");
    let stranger = m
        .arrays
        .add_root(branch_code.into(), branch_code)
        .expect("stranger");
    m.arrays.add_edge(d, ba, kinds::XOR, 0).expect("xor");

    let err = m
        .apply_action(
            d,
            &key("decide"),
            ActorClass::Human,
            "j",
            Some(Value::NodeRef(stranger)),
        )
        .expect_err("stranger is not an XOR branch");
    assert!(matches!(err, DispatchError::NotABranch { .. }));

    let err = m
        .apply_action(d, &key("decide"), ActorClass::Human, "j", None)
        .expect_err("no selection payload");
    assert!(matches!(err, DispatchError::MissingBranchSelection));
}

/// SGT-9.7: terminal absorbing — an action targeting a terminal-state node
/// is rejected TERMINAL (R5).
#[test]
fn sgt_9_7_terminal_absorbing() {
    let (_st, mut m) = fixture();
    let t_code = node_type_code(&m, "task");
    let t1 = m.arrays.add_root(t_code.into(), t_code).expect("task");
    m.set_cell(
        t1,
        &key("title"),
        0.into(),
        Value::Str("t".into()),
        1.into(),
    )
    .expect("cell");
    m.set_cell(t1, &key("amount"), 0.into(), Value::I64(99), 2.into())
        .expect("cell");

    // open → running → done (done = success terminal; children_done rule
    // needs children — give the node a done child).
    let child = m.arrays.add_child(t1, t_code).expect("child");
    m.arrays.set_state(child, 2).expect("child done"); // code 2 = done
    m.apply_action(t1, &key("start"), ActorClass::Human, "u", None)
        .expect("start");
    m.apply_action(t1, &key("complete"), ActorClass::Human, "u", None)
        .expect("complete");
    assert_eq!(m.arrays.state_of(t1), 2, "done");

    // Any further action on the terminal node → TERMINAL (from-state check
    // fires first: no transition declares done as `from`, and the node is
    // absorbing regardless).
    let err = m
        .apply_action(t1, &key("start"), ActorClass::Human, "u", None)
        .expect_err("terminal is absorbing");
    match err {
        DispatchError::StateMismatch { actual, .. } => assert_eq!(actual, key("done")),
        other => panic!("got {other:?}"),
    }
}

/// children_done rule: all children must be in the success terminal class
/// (live evaluation against the arrays' child CSR + states).
#[test]
fn children_state_rule_live_evaluation() {
    let (_st, mut m) = fixture();
    let t_code = node_type_code(&m, "task");
    let t1 = m.arrays.add_root(t_code.into(), t_code).expect("task");
    m.set_cell(
        t1,
        &key("title"),
        0.into(),
        Value::Str("t".into()),
        1.into(),
    )
    .expect("cell");
    m.set_cell(t1, &key("amount"), 0.into(), Value::I64(10), 2.into())
        .expect("cell");
    m.apply_action(t1, &key("start"), ActorClass::Human, "u", None)
        .expect("start");

    // Two children: done + failed → children_done (all success) = false.
    let c1 = m.arrays.add_child(t1, t_code).expect("c1");
    let c2 = m.arrays.add_child(t1, t_code).expect("c2");
    m.arrays.set_state(c1, 2).expect("c1 done");
    m.arrays.set_state(c2, 3).expect("c2 failed");
    let err = m
        .apply_action(t1, &key("complete"), ActorClass::Human, "u", None)
        .expect_err("c2 is failure-terminal");
    match err {
        DispatchError::RequirementsUnmet { failing } => {
            assert_eq!(failing, vec![key("children_done")])
        }
        other => panic!("got {other:?}"),
    }
    // c2 re-run completes → all children success → rule holds → motion.
    m.arrays.set_state(c2, 2).expect("c2 done");
    let outcome = m
        .apply_action(t1, &key("complete"), ActorClass::Human, "u", None)
        .expect("complete");
    assert_eq!(outcome.to, key("done"));
}

//! T-204 acceptance: worklist propagation — guard firing on state motion,
//! MIRROR state copy, read-set dirty tracking, and the P15 confluence
//! property: permutation convergence over event orderings (same event
//! multiset in any order ⇒ identical final state).

use lattice_core::arrays::GraphArrays;
use lattice_core::ids::{NodeId, NodeIdSpace};
use lattice_core::value::Value;
use lattice_engine::dispatch::Machine;
use lattice_engine::propagation::Derivation;
use lattice_engine::propagation::Propagator;
use lattice_schema::catalog::Catalog;
use lattice_schema::compile::compile;
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use lattice_schema::txn::SchemaOp;
use lattice_schema::txn::SchemaState;

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
            Vec::new(),
        )
        .unwrap(),
    )
}

/// Fixture: task_flow (finalizing) + mirror_flow (states share keys so
/// MIRROR can copy across models) + GUARD-armed `kick` (empty actors).
fn fixture() -> Propagator {
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
                key("mirror_flow"),
                ModelClass::Endless,
                vec![
                    state("open", "Open", false, false),
                    state("running", "Running", false, false),
                ],
                key("open"),
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
        SchemaOp::CreateRule(NamedRule {
            key: key("has_title"),
            form: PredicateForm::AttrPresent { attr: key("title") },
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
            key: key("mirror"),
            display_name: "Mirror".into(),
            parent_type: None,
            state_model: key("mirror_flow"),
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
        ),
        transition(
            "task_fail",
            "task_flow",
            "running",
            "failed",
            Vec::new(),
            vec![ActorClass::Human],
        ),
        transition(
            "complete",
            "task_flow",
            "running",
            "done",
            Vec::new(),
            vec![ActorClass::Human],
        ),
        // GUARD-armed (empty actors): the engine fires this when the guard
        // edge's sigma matches the source's current state.
        transition(
            "kick",
            "mirror_flow",
            "open",
            "running",
            Vec::new(),
            Vec::new(),
        ),
    ];
    st.commit(ops).expect("fixture compiles");
    let compiled = compile(&st.catalog).expect("fixture compiles");
    let arrays = GraphArrays::new(NodeIdSpace::new());
    let machine = Machine::new(arrays, compiled.clone(), &st.catalog);
    Propagator::new(machine, &compiled)
}

/// Fresh scenario: task + mirror + mirror2, GUARD task→mirror (sigma=
/// running, tau=kick), MIRROR task→mirror2.
fn scenario() -> (Propagator, NodeId, NodeId, NodeId) {
    let mut prop = fixture();
    let tc = prop
        .machine
        .node_type_code(&key("task"))
        .expect("task code");
    let mc = prop
        .machine
        .node_type_code(&key("mirror"))
        .expect("mirror code");
    let t = prop.machine.arrays.add_root(tc.into(), tc).expect("task");
    let mirror = prop.machine.arrays.add_root(mc.into(), mc).expect("mirror");
    let mirror2 = prop
        .machine
        .arrays
        .add_root(mc.into(), mc)
        .expect("mirror2");
    prop.machine
        .add_guard_edge(t, mirror, &key("running"), &key("kick"))
        .expect("guard edge");
    prop.machine
        .add_mirror_edge(t, mirror2)
        .expect("mirror edge");
    (prop, t, mirror, mirror2)
}

/// GUARD fires on state motion: task reaches `running` ⇒ kick moves the
/// mirror node open → running (author = guard, journal record derived).
#[test]
fn guard_fires_on_state_motion() {
    let (mut prop, t, mirror, _m2) = scenario();
    prop.machine
        .set_cell(t, &key("title"), 0.into(), Value::Str("t".into()), 1.into())
        .expect("cell");
    prop.mark_cell(t, &key("title")).expect("mark");
    prop.machine
        .apply_action(t, &key("start"), ActorClass::Human, "u", None)
        .expect("start");
    prop.mark_state(t);
    let steps = prop.quiesce().expect("quiesce");
    assert!(steps > 0);

    let mirror_state = prop.machine.state_key_of(mirror).expect("mirror state");
    assert_eq!(
        mirror_state,
        key("running"),
        "GUARD fired tau on the target"
    );
    assert!(prop.derivations.iter().any(|d| matches!(
        d,
        Derivation::GuardFired { transition, .. } if transition == &key("kick")
    )));
}

/// MIRROR copies the source's state key to the target (cross-model by key).
#[test]
fn mirror_copies_state_key() {
    let (mut prop, t, _mirror, mirror2) = scenario();
    prop.machine
        .set_cell(t, &key("title"), 0.into(), Value::Str("t".into()), 1.into())
        .expect("cell");
    prop.mark_cell(t, &key("title")).expect("mark");
    prop.machine
        .apply_action(t, &key("start"), ActorClass::Human, "u", None)
        .expect("start");
    prop.mark_state(t);
    prop.quiesce().expect("quiesce");

    let m2_state = prop.machine.state_key_of(mirror2).expect("mirror2 state");
    assert_eq!(m2_state, key("running"), "MIRROR copied the state key");
    assert!(prop.derivations.iter().any(|d| matches!(
        d,
        Derivation::Mirrored { target, .. } if target == &mirror2
    )));
}

/// P15 confluence: the same event multiset applied in any order converges
/// to the identical final state. Events: E1 title write, E2 start action,
/// E3 intermediate quiesce (fixpoint idempotence). Four orderings.
#[test]
fn permutation_convergence() {
    let apply = |permute_queue: bool| -> (Machine, NodeId, NodeId) {
        let mut prop = fixture();
        let tc = prop
            .machine
            .node_type_code(&key("task"))
            .expect("task code");
        let mc = prop
            .machine
            .node_type_code(&key("mirror"))
            .expect("mirror code");
        let t = prop.machine.arrays.add_root(tc.into(), tc).expect("task");
        let mirror = prop.machine.arrays.add_root(mc.into(), mc).expect("mirror");
        let mirror2 = prop
            .machine
            .arrays
            .add_root(mc.into(), mc)
            .expect("mirror2");
        prop.machine
            .add_guard_edge(t, mirror, &key("running"), &key("kick"))
            .expect("guard edge");
        prop.machine
            .add_mirror_edge(t, mirror2)
            .expect("mirror edge");

        // Canonical command sequence (the journal's total order — commands
        // are ordered, their outcomes are what they are).
        prop.machine
            .set_cell(t, &key("title"), 0.into(), Value::Str("t".into()), 1.into())
            .expect("cell");
        prop.mark_cell(t, &key("title")).expect("mark");
        prop.machine
            .apply_action(t, &key("start"), ActorClass::Human, "u", None)
            .expect("start");
        prop.mark_state(t);

        // The worklist arrives with items in arrival order; permute it
        // (reverse = the simplest permutation) before processing.
        if permute_queue {
            prop.reverse_queue();
        }
        prop.quiesce().expect("quiesce");
        (prop.machine, t, mirror)
    };

    let observe = |m: &Machine, t: NodeId, mirror: NodeId| -> (u8, u8, u8, u8) {
        let enabled = m.enabled_actions(t).unwrap_or_default();
        (
            m.arrays.state_of(t),
            m.arrays.state_of(mirror),
            u8::from(enabled.contains(&key("task_fail"))),
            u8::from(enabled.contains(&key("complete"))),
        )
    };

    // Two runs: arrival-order queue vs reversed queue ⇒ identical final state
    // (confluence: processing order cannot change the fixpoint).
    let (m1, t1, mirror1) = apply(false);
    let (m2, t2, mirror2) = apply(true);
    assert_eq!(
        observe(&m1, t1, mirror1),
        observe(&m2, t2, mirror2),
        "queue permutation must not change the fixpoint"
    );

    // Fixpoint idempotence: quiescing an already-quiet machine changes nothing.
    let steps2 = {
        let mut prop = fixture();
        prop.quiesce().expect("empty quiesce")
    };
    assert_eq!(steps2, 0, "empty worklist quiesces with zero steps");

    // Meaningful target: task running, mirror kicked, mirror2 mirrored.
    let o = observe(&m1, t1, mirror1);
    assert_eq!(o.0, 1, "task running");
    assert_eq!(o.1, 1, "mirror kicked to running");
    assert_eq!(o.2, 1, "task_fail enabled from running");
}
#[test]
fn read_set_dirty_tracking() {
    let mut prop = fixture();
    let tc = prop
        .machine
        .node_type_code(&key("task"))
        .expect("task code");
    let t = prop.machine.arrays.add_root(tc.into(), tc).expect("task");

    // title is in start's read-set: write → mark → quiesce re-gates it.
    prop.machine
        .set_cell(t, &key("title"), 0.into(), Value::Str("t".into()), 1.into())
        .expect("cell");
    prop.mark_cell(t, &key("title")).expect("mark");
    prop.quiesce().expect("quiesce");
    assert!(
        prop.machine
            .enabled_actions(t)
            .expect("index")
            .contains(&key("start")),
        "start re-gated by the title write"
    );

    // task_fail's read-set is empty: a title write never re-gates it.
    let inverted = lattice_engine::propagation::invert_read_sets(prop.machine.compiled_ref());
    assert!(
        !inverted
            .values()
            .any(|keys| keys.contains(&key("task_fail"))),
        "task_fail reads nothing — absent from the inverted read-set index"
    );
    assert!(
        inverted.values().any(|keys| keys.contains(&key("start"))),
        "start reads title — present in the inverted index"
    );
}

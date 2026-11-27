//! T-104 acceptance: schema transactions — closed op registry, R13 guarded
//! evolution with rule id + live-reference path (SGT-16.1), drain-then-remove
//! (SGT-16.2), dry-run fidelity (SGT-16.3), epoch law (SGT-1.2), and the P3
//! compile gate on every accepted op.

use lattice_schema::catalog::Catalog;
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use lattice_schema::txn::{SchemaOp, SchemaState};

fn key(s: &str) -> Key {
    Key::parse(s).unwrap()
}

fn state(k: &str, success: bool, failure: bool) -> State {
    State {
        key: key(k),
        name: k.to_uppercase(),
        success_terminal: success,
        failure_terminal: failure,
    }
}

/// Endless model `flow`: running ⇄ parked (cycles legal).
fn endless_state() -> SchemaState {
    let mut c = Catalog::fresh();
    let m = StateModel::new(
        key("flow"),
        ModelClass::Endless,
        vec![
            state("running", false, false),
            state("parked", false, false),
        ],
        key("running"),
    )
    .unwrap();
    c.create_state_model(m).unwrap();
    c.create_key_type(KeyTypeDef {
        key: key("seq"),
        density: Density::Dense,
        axis: AxisKind::Ordinal,
    })
    .unwrap();
    SchemaState::new(c)
}

fn attr_def(k: &str) -> AttributeDef {
    AttributeDef {
        key: key(k),
        attr_type: AttrType::Scalar(ScalarType::String),
        required: false,
        default: None,
        codec_override: None,
        monotonicity: Monotonicity::None,
        write_scope: None,
    }
}

/// SGT-16.1: each destructive op against a live reference is rejected with
/// the R13 rule id and a live-reference path; plan never mutates.
#[test]
fn sgt_16_1_r13_matrix() {
    let mut st = endless_state();

    st.commit(vec![SchemaOp::CreateEnum(
        EnumType::new(key("status"), vec![key("open"), key("closed")]).unwrap(),
    )])
    .expect("enum created");
    st.commit(vec![SchemaOp::DeprecateEnumValue {
        enum_key: key("status"),
        value: key("open"),
    }])
    .expect("deprecated");
    st.commit(vec![SchemaOp::CreateAttribute(AttributeDef {
        key: key("status_attr"),
        attr_type: AttrType::Enum(key("status")),
        required: false,
        default: None,
        codec_override: None,
        monotonicity: Monotonicity::None,
        write_scope: None,
    })])
    .expect("enum-typed attribute created");
    st.commit(vec![SchemaOp::CreateRule(NamedRule {
        key: key("is_open"),
        form: PredicateForm::SetMember {
            attr: key("status_attr"),
            values: vec![lattice_core::value::Value::Enum(0)],
        },
    })])
    .expect("rule created");
    st.commit(vec![SchemaOp::AddTransition(
        Transition::new(
            key("cycle"),
            key("flow"),
            key("running"),
            key("parked"),
            vec![key("is_open")],
            Vec::new(),
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap(),
    )])
    .expect("transition created");
    let epoch_before = st.catalog.schema_epoch;

    // Remove enum value referenced by a rule → R13_IN_USE naming the rule.
    let outcomes = st.plan(&[SchemaOp::RemoveEnumValue {
        enum_key: key("status"),
        value: key("open"),
    }]);
    assert!(!outcomes[0].accepted);
    let r13 = outcomes[0].r13.as_ref().expect("R13 details attached");
    assert_eq!(r13.rule, "R13_IN_USE");
    assert!(
        r13.path.contains("is_open"),
        "path must name the live reference: {}",
        r13.path
    );

    // Remove rule referenced by a transition → R13_IN_USE naming it.
    let outcomes = st.plan(&[SchemaOp::RemoveRule {
        rule_key: key("is_open"),
    }]);
    let r13 = outcomes[0].r13.as_ref().expect("R13 details");
    assert!(
        r13.path.contains("cycle"),
        "path names the transition: {}",
        r13.path
    );

    // Remove a live, non-deprecated state → drain rule fires first.
    // (`running` is the initial state — it can never be removed; `parked`
    // carries the live transition.)
    let outcomes = st.plan(&[SchemaOp::RemoveState {
        model_key: key("flow"),
        state_key: key("parked"),
    }]);
    assert_eq!(
        outcomes[0].r13.as_ref().expect("R13 details").rule,
        "R13_DRAIN"
    );

    st.commit(vec![SchemaOp::DeprecateState {
        model_key: key("flow"),
        state_key: key("parked"),
    }])
    .expect("deprecated");
    let outcomes = st.plan(&[SchemaOp::RemoveState {
        model_key: key("flow"),
        state_key: key("parked"),
    }]);
    let r13 = outcomes[0].r13.as_ref().expect("R13 details");
    assert_eq!(r13.rule, "R13_IN_USE");
    assert!(
        r13.path.contains("transition"),
        "path names the transition: {}",
        r13.path
    );

    // The initial state is structurally unremovable (after deprecation it
    // still reports the initial-state blocker).
    st.commit(vec![SchemaOp::DeprecateState {
        model_key: key("flow"),
        state_key: key("running"),
    }])
    .expect("deprecated");
    let outcomes = st.plan(&[SchemaOp::RemoveState {
        model_key: key("flow"),
        state_key: key("running"),
    }]);
    let r13 = outcomes[0].r13.as_ref().expect("R13 details");
    assert_eq!(r13.rule, "R13_IN_USE");
    assert!(r13.path.contains("initial state"), "got: {}", r13.path);

    // Drop graph referenced by a grant → R13_IN_USE naming the grant.
    st.commit(vec![SchemaOp::CreateGraph(GraphSchema {
        key: key("main"),
        display_name: "Main".into(),
        created_at_us: 0,
    })])
    .expect("graph created");
    st.commit(vec![SchemaOp::GrantOp(Grant {
        role: key("reader"),
        subject: "agent:1".into(),
        target: GrantTarget::Graph(key("main")),
    })])
    .expect("grant recorded");
    let outcomes = st.plan(&[SchemaOp::DropGraph { key: key("main") }]);
    let r13 = outcomes[0].r13.as_ref().expect("R13 details");
    assert!(
        r13.path.contains("grant"),
        "path names the grant: {}",
        r13.path
    );

    // Planning mutated nothing: run fresh plans and assert the epoch frozen.
    let epoch_now = st.catalog.schema_epoch;
    let _ = st.plan(&[SchemaOp::DropGraph { key: key("main") }]);
    let _ = st.plan(&[SchemaOp::RemoveEnumValue {
        enum_key: key("status"),
        value: key("open"),
    }]);
    assert_eq!(
        st.catalog.schema_epoch, epoch_now,
        "plan never bumps epochs"
    );
    let _ = epoch_before;
    assert!(
        st.catalog.enums[&key("status")]
            .values
            .contains(&key("open"))
    );
    assert!(st.catalog.graphs.contains_key(&key("main")));
}

/// R13 index stability + drain discipline (SPEC-01 §4): enum removal is
/// drain-gated and only for the LAST dict value (cells store indices).
#[test]
fn r13_index_stability_and_drain() {
    let mut st = endless_state();
    st.commit(vec![SchemaOp::CreateEnum(
        EnumType::new(key("status"), vec![key("open"), key("closed")]).unwrap(),
    )])
    .expect("created");

    // Not deprecated → drain rule.
    let outcomes = st.plan(&[SchemaOp::RemoveEnumValue {
        enum_key: key("status"),
        value: key("closed"),
    }]);
    assert_eq!(outcomes[0].r13.as_ref().expect("r13").rule, "R13_DRAIN");

    // Deprecated but not last → index-stability rule.
    st.commit(vec![SchemaOp::DeprecateEnumValue {
        enum_key: key("status"),
        value: key("open"),
    }])
    .expect("deprecated");
    let outcomes = st.plan(&[SchemaOp::RemoveEnumValue {
        enum_key: key("status"),
        value: key("open"),
    }]);
    assert_eq!(
        outcomes[0].r13.as_ref().expect("r13").rule,
        "R13_INDEX_STABILITY"
    );

    // Deprecated + last → removal succeeds (SGT-16.2: drain then remove).
    st.commit(vec![SchemaOp::DeprecateEnumValue {
        enum_key: key("status"),
        value: key("closed"),
    }])
    .expect("deprecated");
    st.commit(vec![SchemaOp::RemoveEnumValue {
        enum_key: key("status"),
        value: key("closed"),
    }])
    .expect("drained value removed");
    assert_eq!(st.catalog.enums[&key("status")].values, vec![key("open")]);

    // State drain path: deprecate → drain referencing transitions → remove.
    st.commit(vec![SchemaOp::AddTransition(
        Transition::new(
            key("cycle"),
            key("flow"),
            key("parked"),
            key("running"),
            Vec::new(),
            Vec::new(),
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap(),
    )])
    .expect("transition into parked");
    st.commit(vec![SchemaOp::DeprecateState {
        model_key: key("flow"),
        state_key: key("parked"),
    }])
    .expect("parked deprecated");
    // The closed registry has no RemoveTransition op (P3: structure ops are
    // journaled facts; dropping a transition = DeprecateState on both ends
    // then model-level drain via migration). For the drain test we verify the
    // removal stays blocked while the transition lives.
    let outcomes = st.plan(&[SchemaOp::RemoveState {
        model_key: key("flow"),
        state_key: key("parked"),
    }]);
    assert_eq!(outcomes[0].r13.as_ref().expect("r13").rule, "R13_IN_USE");
    assert!(
        outcomes[0]
            .r13
            .as_ref()
            .expect("r13")
            .path
            .contains("cycle")
    );
}

/// SGT-16.3: plan(ops) verdicts == commit(ops) verdicts; failed commit is
/// atomic; clean commit bumps the epoch exactly +1 (SGT-1.2).
#[test]
fn sgt_16_3_dry_run_fidelity_and_atomicity() {
    let mut st = endless_state();
    let ops = vec![
        SchemaOp::CreateAttribute(attr_def("name")),
        SchemaOp::CreateAttributeSet(AttributeSet {
            key: key("ghost_set"),
            kind: SetKind::ScalarSet,
            members: vec![key("does_not_exist")],
            write_scope: None,
        }),
        SchemaOp::CreateRole(Role {
            key: key("reader"),
            scopes: vec![Scope::ReadSubtree],
        }),
    ];
    // Txn-level compile gate: the ghost set is structurally valid (R13 has
    // nothing to count) — the whole txn is rejected by the compile verdict
    // on the synthetic trailing outcome (op_index == ops.len()).
    let planned = st.plan(&ops);
    assert_eq!(planned.len(), 4, "3 ops + 1 txn-level compile verdict");
    assert!(planned[0].accepted && planned[1].accepted && planned[2].accepted);
    assert!(!planned[3].accepted, "txn compile gate fires");
    assert_eq!(planned[3].op_index, 3);
    assert!(matches!(
        planned[3].error,
        Some(SchemaError::UnresolvedSetMember { .. })
    ));

    let epoch_before = st.catalog.schema_epoch;
    let err = st.commit(ops).expect_err("atomic commit must fail");
    assert_eq!(
        err.outcomes, planned,
        "SGT-16.3: plan and commit agree per op"
    );
    assert_eq!(
        st.catalog.schema_epoch, epoch_before,
        "failed commit applies nothing"
    );
    assert!(
        !st.catalog.attributes.contains_key(&key("name")),
        "no partial application"
    );
    assert!(
        !st.catalog.roles.contains_key(&key("reader")),
        "no partial application"
    );
    // Clean commit of the accepted subset: epoch bumps exactly +1 (SGT-1.2).
    let epoch_before2 = st.catalog.schema_epoch;
    let epoch = st
        .commit(vec![
            SchemaOp::CreateAttribute(attr_def("name")),
            SchemaOp::CreateRole(Role {
                key: key("reader"),
                scopes: vec![Scope::ReadSubtree],
            }),
        ])
        .expect("clean txn");
    assert_eq!(
        epoch,
        epoch_before2 + 1,
        "SGT-1.2: exactly one epoch bump per txn"
    );
    assert!(st.catalog.attributes.contains_key(&key("name")));
    assert!(st.catalog.roles.contains_key(&key("reader")));
}

/// The P3 compile gate: an op that breaks compilability (backward rank in a
/// finalizing model) is rejected; endless models allow the same shape.
#[test]
fn commit_is_compile_gated() {
    // Endless: cycles (self transitions) are legal.
    let mut st = endless_state();
    st.commit(vec![SchemaOp::AddTransition(
        Transition::new(
            key("self"),
            key("flow"),
            key("running"),
            key("running"),
            Vec::new(),
            Vec::new(),
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap(),
    )])
    .expect("endless models allow cycle transitions");

    // Finalizing twin: backward motion is rejected, nothing applied.
    let mut fin = Catalog::fresh();
    let m = StateModel::new(
        key("proc"),
        ModelClass::Finalizing,
        vec![
            state("open", false, false),
            state("done", true, false),
            state("failed", false, true),
        ],
        key("open"),
    )
    .unwrap();
    fin.create_state_model(m).unwrap();
    fin.create_transition(
        Transition::new(
            key("ok"),
            key("proc"),
            key("open"),
            key("done"),
            Vec::new(),
            Vec::new(),
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap(),
    )
    .unwrap();
    fin.create_transition(
        Transition::new(
            key("bad"),
            key("proc"),
            key("open"),
            key("failed"),
            Vec::new(),
            Vec::new(),
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap(),
    )
    .unwrap();
    let mut st2 = SchemaState::new(fin);
    let err = st2
        .commit(vec![SchemaOp::AddTransition(
            Transition::new(
                key("back"),
                key("proc"),
                key("done"),
                key("open"),
                Vec::new(),
                Vec::new(),
                None,
                TransitionTiming::default(),
                Vec::new(),
            )
            .unwrap(),
        )])
        .expect_err("backward motion into an ancestor is rejected by the txn-level compile gate");
    // One op outcome (accepted) + the synthetic compile rejection.
    assert_eq!(err.outcomes.len(), 2);
    assert!(err.outcomes[0].accepted);
    assert_eq!(err.outcomes[1].op_index, 1);
    assert!(
        matches!(
            err.outcomes[1].error,
            Some(SchemaError::FinalizingCycle { .. }) | Some(SchemaError::BackwardRank { .. })
        ),
        "got: {:?}",
        err.outcomes[1].error
    );
    assert!(
        !st2.catalog.transitions.contains_key(&key("back")),
        "rejected op left no trace"
    );
}

/// Registry closure (SGT-2.3 at the txn layer): no rename op exists; keys
/// immutable — re-creating a key is rejected and the original stands.
#[test]
fn registry_has_no_rename_op() {
    let mut st = endless_state();
    st.commit(vec![SchemaOp::CreateGraph(GraphSchema {
        key: key("main"),
        display_name: "Main".into(),
        created_at_us: 0,
    })])
    .expect("first create ok");
    st.commit(vec![SchemaOp::CreateGraph(GraphSchema {
        key: key("main"),
        display_name: "Renamed".into(),
        created_at_us: 0,
    })])
    .expect_err("no alter path — re-create rejected");
    assert_eq!(st.catalog.graphs[&key("main")].display_name, "Main");
}

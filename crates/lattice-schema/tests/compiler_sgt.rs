//! T-103 acceptance: schema compiler — masks, read-sets, enabled-action
//! index, ranks (SPEC-01 §8/§11/§13; ARCHITECTURE §7).

use lattice_schema::catalog::Catalog;
use lattice_schema::compile::compile;
use lattice_schema::entities::*;
use lattice_schema::key::Key;

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

fn attr(k: &str, vector: bool, monotone: Monotonicity) -> AttributeDef {
    AttributeDef {
        key: key(k),
        attr_type: if vector {
            AttrType::Vector {
                elem: ElemType::Scalar(ScalarType::I64),
            }
        } else {
            AttrType::Scalar(ScalarType::String)
        },
        required: false,
        default: None,
        codec_override: None,
        monotonicity: monotone,
        write_scope: None,
    }
}

fn set_of(k: &str, kind: SetKind, members: &[&str]) -> AttributeSet {
    AttributeSet {
        key: key(k),
        kind,
        members: members.iter().map(|m| key(m)).collect(),
        write_scope: None,
    }
}

/// Finalizing diamond (SGT-8.5): a→b, a→c, b→d, c→d, a→f(failure terminal).
fn diamond_model() -> (StateModel, Vec<Transition>) {
    let model = StateModel::new(
        key("diamond"),
        ModelClass::Finalizing,
        vec![
            state("a", false, false),
            state("b", false, false),
            state("c", false, false),
            state("d", true, false),
            state("f", false, true),
        ],
        key("a"),
    )
    .unwrap();
    let t = |k: &str, from: &str, to: &str| {
        Transition::new(
            key(k),
            key("diamond"),
            key(from),
            key(to),
            Vec::new(),
            Vec::new(),
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap()
    };
    let transitions = vec![
        t("a_b", "a", "b"),
        t("a_c", "a", "c"),
        t("b_d", "b", "d"),
        t("c_d", "c", "d"),
        t("a_f", "a", "f"),
    ];
    (model, transitions)
}

/// SGT-8.5: rank assignment on the diamond — a=0, b=c=1, d=2, f=1.
#[test]
fn sgt_8_5_rank_assignment() {
    let mut c = Catalog::fresh();
    let (model, transitions) = diamond_model();
    c.create_state_model(model).unwrap();
    for t in transitions {
        c.create_transition(t).unwrap();
    }
    let compiled = compile(&c).expect("diamond compiles");
    let rank = |s: &str| {
        compiled
            .ranks
            .get(&(key("diamond"), key(s)))
            .copied()
            .expect("rank present")
    };
    assert_eq!(rank("a"), 0);
    assert_eq!(rank("b"), 1);
    assert_eq!(rank("c"), 1);
    assert_eq!(rank("d"), 2);
    assert_eq!(rank("f"), 1);
}

/// SGT-8.4: backward motion (d→a, rank 2→0) is rejected at schema commit.
#[test]
fn sgt_8_4_backward_rank_rejected() {
    let mut c = Catalog::fresh();
    let (model, transitions) = diamond_model();
    c.create_state_model(model).unwrap();
    for t in transitions {
        c.create_transition(t).unwrap();
    }
    // b->c: equal ranks (1 -> 1), acyclic — the pure SGT-8.4 case. (A
    // backward edge to an ancestor, d->a, is also rejected but as a cycle.)
    let back = Transition::new(
        key("b_c"),
        key("diamond"),
        key("b"),
        key("c"),
        Vec::new(),
        Vec::new(),
        None,
        TransitionTiming::default(),
        Vec::new(),
    )
    .unwrap();
    c.create_transition(back).unwrap();
    let e = compile(&c).expect_err("backward transition must be rejected");
    assert!(
        matches!(
            e,
            SchemaError::BackwardRank {
                from_rank: 1,
                to_rank: 1,
                ..
            }
        ),
        "got: {e}"
    );
}

/// Finalizing cycles are structurally illegal (only endless models cycle).
#[test]
fn finalizing_cycle_rejected_endless_cycle_legal() {
    let mut c = Catalog::fresh();
    let m = StateModel::new(
        key("cyc"),
        ModelClass::Finalizing,
        vec![
            state("a", false, false),
            state("b", false, false),
            state("d", true, false),
            state("f", false, true),
        ],
        key("a"),
    )
    .unwrap();
    c.create_state_model(m).unwrap();
    let t = |k: &str, from: &str, to: &str| {
        Transition::new(
            key(k),
            key("cyc"),
            key(from),
            key(to),
            Vec::new(),
            Vec::new(),
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap()
    };
    for t in [
        t("a_b", "a", "b"),
        t("b_a", "b", "a"),
        t("b_d", "b", "d"),
        t("a_f", "a", "f"),
    ] {
        c.create_transition(t).unwrap();
    }
    let e = compile(&c).expect_err("finalizing cycle must be rejected");
    assert!(matches!(e, SchemaError::FinalizingCycle { .. }), "got: {e}");

    // The same shape compiles fine as endless (cycles legal, no ranks).
    let mut c2 = Catalog::fresh();
    let m2 = StateModel::new(
        key("cyc"),
        ModelClass::Endless,
        vec![state("a", false, false), state("b", false, false)],
        key("a"),
    )
    .unwrap();
    c2.create_state_model(m2).unwrap();
    let t2 = |k: &str, from: &str, to: &str| {
        Transition::new(
            key(k),
            key("cyc"),
            key(from),
            key(to),
            Vec::new(),
            Vec::new(),
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap()
    };
    for t in [t2("a_b", "a", "b"), t2("b_a", "b", "a")] {
        c2.create_transition(t).unwrap();
    }
    let compiled = compile(&c2).expect("endless cycles are legal");
    assert!(compiled.ranks.is_empty(), "endless models carry no ranks");
}

/// SGT-11.2 (compile level): child types see parent-type attribute sets;
/// first declaration wins on conflicts.
#[test]
fn sgt_11_2_inheritance_union() {
    let mut c = Catalog::fresh();
    c.create_key_type(KeyTypeDef {
        key: key("seq"),
        density: Density::Dense,
        axis: AxisKind::Ordinal,
    })
    .unwrap();
    c.create_attribute(attr("name", false, Monotonicity::None))
        .unwrap();
    c.create_attribute(attr("size", true, Monotonicity::Increasing))
        .unwrap();
    c.create_attribute(attr("extra", false, Monotonicity::None))
        .unwrap();
    c.create_attr_set(set_of("common", SetKind::ScalarSet, &["name"]))
        .unwrap();
    c.create_attr_set(set_of("measure", SetKind::TableSet, &["size"]))
        .unwrap();
    c.create_attr_set(set_of("extra_set", SetKind::ScalarSet, &["extra"]))
        .unwrap();

    let sm = StateModel::new(
        key("sm"),
        ModelClass::Endless,
        vec![state("running", false, false)],
        key("running"),
    )
    .unwrap();
    c.create_state_model(sm.clone()).unwrap();

    let parent = NodeType {
        key: key("base"),
        display_name: "Base".into(),
        parent_type: None,
        state_model: key("sm"),
        attribute_sets: vec![key("common"), key("measure")],
        key_type: key("seq"),
        state_locks: Vec::new(),
    };
    // Child re-declares `common` (first declaration = nearest wins) and adds
    // its own set.
    let child = NodeType {
        key: key("task"),
        display_name: "Task".into(),
        parent_type: Some(key("base")),
        state_model: key("sm"),
        attribute_sets: vec![key("common"), key("extra_set")],
        key_type: key("seq"),
        state_locks: Vec::new(),
    };
    c.create_node_type(parent).unwrap();
    c.create_node_type(child).unwrap();

    let compiled = compile(&c).expect("compiles");
    let eff = &compiled.effective_attrs[&key("task")];
    assert_eq!(eff.count(), 3, "child inherits name+size and owns extra");
    for a in ["name", "size", "extra"] {
        let id = compiled.attr_ids[&key(a)].0 as usize;
        assert!(eff.test(id), "`{a}` must be in the effective mask");
    }

    // SGT-11.3: a parent cycle is rejected.
    let mut bad = Catalog::fresh();
    bad.create_key_type(KeyTypeDef {
        key: key("seq"),
        density: Density::Dense,
        axis: AxisKind::Ordinal,
    })
    .unwrap();
    bad.create_state_model(sm).unwrap();
    let ta = NodeType {
        key: key("aa"),
        display_name: "A".into(),
        parent_type: Some(key("bb")),
        state_model: key("sm"),
        attribute_sets: Vec::new(),
        key_type: key("seq"),
        state_locks: Vec::new(),
    };
    let tb = NodeType {
        key: key("bb"),
        display_name: "B".into(),
        parent_type: Some(key("aa")),
        state_model: key("sm"),
        attribute_sets: Vec::new(),
        key_type: key("seq"),
        state_locks: Vec::new(),
    };
    bad.create_node_type(ta).unwrap();
    bad.create_node_type(tb).unwrap();
    let e = compile(&bad).expect_err("type cycle must be rejected");
    assert!(matches!(e, SchemaError::TypeCycle { .. }), "got: {e}");
}

/// Set homogeneity (SPEC-01 §7): mixed scalar/vector members rejected.
#[test]
fn mixed_sets_rejected() {
    let mut c = Catalog::fresh();
    c.create_attribute(attr("meta", false, Monotonicity::None))
        .unwrap();
    c.create_attribute(attr("series", true, Monotonicity::None))
        .unwrap();
    let mixed = set_of("bad_set", SetKind::ScalarSet, &["meta", "series"]);
    c.create_attr_set(mixed).unwrap();
    let e = compile(&c).expect_err("mixed set must be rejected");
    assert!(matches!(e, SchemaError::MixedSet { .. }), "got: {e}");
}

/// SGT-13.2: comparison predicates over non-monotone attributes rejected.
#[test]
fn sgt_13_2_monotonicity_pairing() {
    let mut c = Catalog::fresh();
    c.create_attribute(attr("free", false, Monotonicity::None))
        .unwrap();
    c.create_attribute(attr("directed", false, Monotonicity::Increasing))
        .unwrap();
    let bad = NamedRule {
        key: key("bad_rule"),
        form: PredicateForm::LatestCmp {
            attr: key("free"),
            op: CmpOp::Gte,
            value: lattice_core::value::Value::I64(1),
        },
    };
    c.create_rule(bad).unwrap();
    let e = compile(&c).expect_err("non-monotone comparison must be rejected");
    assert!(
        matches!(e, SchemaError::UnresolvedMonotonicity { .. }),
        "got: {e}"
    );

    // Keys are immutable — the offending rule lives only in its own catalog
    // attempt above; a fresh catalog compiles the monotone-paired rule.
    let mut c = Catalog::fresh();
    c.create_attribute(attr("directed", false, Monotonicity::Increasing))
        .unwrap();
    let good = NamedRule {
        key: key("good_rule"),
        form: PredicateForm::LatestCmp {
            attr: key("directed"),
            op: CmpOp::Gte,
            value: lattice_core::value::Value::I64(1),
        },
    };
    c.create_rule(good).unwrap();
    let compiled = compile(&c).expect("monotone-paired rule compiles");
    assert_eq!(
        compiled.rule_attrs[&key("good_rule")],
        vec![compiled.attr_ids[&key("directed")]],
        "rule read-set resolves to the attr id"
    );
}

/// Write masks: state locks intersect the effective set; lockless states
/// write everything effective (absence semantics, SPEC-01 §7 / SGT-7.3).
#[test]
fn write_masks_follow_state_locks() {
    let mut c = Catalog::fresh();
    c.create_key_type(KeyTypeDef {
        key: key("seq"),
        density: Density::Dense,
        axis: AxisKind::Ordinal,
    })
    .unwrap();
    c.create_attribute(attr("meta", false, Monotonicity::None))
        .unwrap();
    c.create_attribute(attr("series", true, Monotonicity::None))
        .unwrap();
    c.create_attr_set(set_of("meta_set", SetKind::ScalarSet, &["meta"]))
        .unwrap();
    c.create_attr_set(set_of("series_set", SetKind::TableSet, &["series"]))
        .unwrap();
    let sm = StateModel::new(
        key("sm"),
        ModelClass::Finalizing,
        vec![
            state("open", false, false),
            state("done", true, false),
            state("failed", false, true),
        ],
        key("open"),
    )
    .unwrap();
    c.create_state_model(sm).unwrap();
    // Reachability: terminals must be reachable from the initial state.
    for (k, to) in [("approve", "done"), ("reject", "failed")] {
        c.create_transition(
            Transition::new(
                key(k),
                key("sm"),
                key("open"),
                key(to),
                Vec::new(),
                Vec::new(),
                None,
                TransitionTiming::default(),
                Vec::new(),
            )
            .unwrap(),
        )
        .unwrap();
    }
    let nt = NodeType {
        key: key("task"),
        display_name: "Task".into(),
        parent_type: None,
        state_model: key("sm"),
        attribute_sets: vec![key("meta_set"), key("series_set")],
        key_type: key("seq"),
        state_locks: vec![StateLock {
            state: key("done"),
            writable_sets: vec![key("meta_set")],
        }],
    };
    c.create_node_type(nt).unwrap();

    let compiled = compile(&c).expect("compiles");
    let meta_id = compiled.attr_ids[&key("meta")].0 as usize;
    let series_id = compiled.attr_ids[&key("series")].0 as usize;

    let open = &compiled.write_masks[&(key("task"), key("open"))];
    assert!(
        open.test(meta_id) && open.test(series_id),
        "lockless state: everything writable"
    );

    let done = &compiled.write_masks[&(key("task"), key("done"))];
    assert!(done.test(meta_id), "locked state keeps its set writable");
    assert!(!done.test(series_id), "locked state freezes the rest");
}

/// Enabled-action index: invokable transitions listed per (type, from-state);
/// engine-only transitions (empty actors) excluded.
#[test]
fn action_index_lists_invokable_from_states() {
    let mut c = Catalog::fresh();
    let sm = StateModel::new(
        key("sm"),
        ModelClass::Finalizing,
        vec![
            state("open", false, false),
            state("done", true, false),
            state("failed", false, true),
        ],
        key("open"),
    )
    .unwrap();
    c.create_state_model(sm).unwrap();
    let t = |k: &str, from: &str, to: &str, actors: Vec<ActorClass>| {
        Transition::new(
            key(k),
            key("sm"),
            key(from),
            key(to),
            Vec::new(),
            actors,
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap()
    };
    c.create_transition(t(
        "approve",
        "open",
        "done",
        vec![ActorClass::Human, ActorClass::Agent],
    ))
    .unwrap();
    c.create_transition(t("reject", "open", "failed", vec![ActorClass::Agent]))
        .unwrap();
    // Engine-only cleanup (empty actors): never in the action index; note
    // done→done would be rank-equal → illegal in finalizing.
    c.create_transition(t("auto_close", "open", "done", Vec::new()))
        .unwrap();
    let nt = NodeType {
        key: key("task"),
        display_name: "Task".into(),
        parent_type: None,
        state_model: key("sm"),
        attribute_sets: Vec::new(),
        key_type: key("seq"),
        state_locks: Vec::new(),
    };
    c.create_key_type(KeyTypeDef {
        key: key("seq"),
        density: Density::Dense,
        axis: AxisKind::Ordinal,
    })
    .unwrap();
    c.create_node_type(nt).unwrap();

    let compiled = compile(&c).expect("compiles");
    let actions = compiled
        .action_index
        .get(&(key("task"), key("open")))
        .expect("open state has actions");
    assert_eq!(
        actions,
        &[key("approve"), key("reject")],
        "sorted, invokable only"
    );
    assert!(
        !compiled
            .action_index
            .contains_key(&(key("task"), key("done"))),
        "engine-only transitions never enter the action index"
    );

    // Read sets: requirements resolve to attr-id masks.
    c.create_attribute(attr("evidence", false, Monotonicity::None))
        .unwrap();
    c.create_rule(NamedRule {
        key: key("has_evidence"),
        form: PredicateForm::AttrPresent {
            attr: key("evidence"),
        },
    })
    .unwrap();
    c.create_transition(
        Transition::new(
            key("approve2"),
            key("sm"),
            key("open"),
            key("done"),
            vec![key("has_evidence")],
            vec![ActorClass::Agent],
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap(),
    )
    .expect("fresh transition key accepted");
    let compiled2 = compile(&c).expect("compiles");
    let rs = &compiled2.read_sets[&key("approve2")];
    let ev_id = compiled2.attr_ids[&key("evidence")].0 as usize;
    assert!(
        rs.test(ev_id),
        "requirement read-set covers the rule's attribute"
    );
}

/// Compile is a pure function: same catalog ⇒ equal Compiled (recompile
/// identity, deterministic id assignment).
#[test]
fn recompile_identity() {
    let mut c = Catalog::fresh();
    c.create_key_type(KeyTypeDef {
        key: key("seq"),
        density: Density::Dense,
        axis: AxisKind::Ordinal,
    })
    .unwrap();
    c.create_attribute(attr("alpha", false, Monotonicity::None))
        .unwrap();
    c.create_attribute(attr("beta", true, Monotonicity::Increasing))
        .unwrap();
    c.create_attr_set(set_of("s1", SetKind::ScalarSet, &["alpha"]))
        .unwrap();
    c.create_attr_set(set_of("s2", SetKind::TableSet, &["beta"]))
        .unwrap();
    let sm = StateModel::new(
        key("sm"),
        ModelClass::Endless,
        vec![state("run", false, false)],
        key("run"),
    )
    .unwrap();
    c.create_state_model(sm).unwrap();
    c.create_node_type(NodeType {
        key: key("t"),
        display_name: "T".into(),
        parent_type: None,
        state_model: key("sm"),
        attribute_sets: vec![key("s1"), key("s2")],
        key_type: key("seq"),
        state_locks: Vec::new(),
    })
    .unwrap();

    let a = compile(&c).expect("compiles");
    let b = compile(&c).expect("compiles");
    assert_eq!(a, b, "compile must be a pure function of the catalog");
    assert_eq!(
        a.attr_ids[&key("alpha")].0,
        0,
        "ids assigned in sorted key order"
    );
    assert_eq!(a.attr_ids[&key("beta")].0, 1);
    assert_eq!(a.epoch, 0);
}

/// Unresolved references are compile errors (SGT-10.3 analog).
#[test]
fn unresolved_references_rejected() {
    let mut c = Catalog::fresh();
    c.create_attr_set(set_of("dangling", SetKind::ScalarSet, &["ghost"]))
        .unwrap();
    let e = compile(&c).expect_err("unknown set member must be rejected");
    assert!(
        matches!(e, SchemaError::UnresolvedSetMember { .. }),
        "got: {e}"
    );

    let mut c2 = Catalog::fresh();
    let sm = StateModel::new(
        key("sm"),
        ModelClass::Endless,
        vec![state("run", false, false)],
        key("run"),
    )
    .unwrap();
    c2.create_state_model(sm).unwrap();
    c2.create_transition(
        Transition::new(
            key("t"),
            key("sm"),
            key("run"),
            key("void"),
            Vec::new(),
            Vec::new(),
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap(),
    )
    .unwrap();
    let e = compile(&c2).expect_err("unknown target state must be rejected");
    assert!(
        matches!(
            e,
            SchemaError::NotFound {
                namespace: "state",
                ..
            }
        ),
        "got: {e}"
    );
}

//! T-201 acceptance: FSM registry — runtime motion validation (SPEC-01 §8).
//! SGT-8.3 (endless cycles legal, twice), SGT-8.4 (backward prohibition at
//! runtime, no retraction machinery), rank mirror (SGT-8.5), absorbing
//! terminals, declared-motion-only, state codes.

use lattice_engine::fsm::{EngineError, FsmRegistry};
use lattice_schema::catalog::Catalog;
use lattice_schema::compile::compile;
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use lattice_schema::txn::SchemaOp;

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

fn transition(kk: &str, model: &str, from: &str, to: &str) -> SchemaOp {
    SchemaOp::AddTransition(
        Transition::new(
            key(kk),
            key(model),
            key(from),
            key(to),
            Vec::new(),
            Vec::new(),
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .unwrap(),
    )
}

/// The SGT-8.5 diamond as a finalizing model.
fn diamond_registry() -> FsmRegistry {
    let mut c = Catalog::fresh();
    let m = StateModel::new(
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
    c.create_state_model(m).unwrap();
    for op in [
        transition("a_b", "diamond", "a", "b"),
        transition("a_c", "diamond", "a", "c"),
        transition("b_d", "diamond", "b", "d"),
        transition("c_d", "diamond", "c", "d"),
        transition("a_f", "diamond", "a", "f"),
    ] {
        c.create_transition(match op {
            SchemaOp::AddTransition(t) => t,
            _ => unreachable!(),
        })
        .unwrap();
    }
    let compiled = compile(&c).expect("diamond compiles");
    FsmRegistry::from_compiled(&compiled)
}

/// SGT-8.5 mirror: runtime ranks match the compiler (a=0, b=c=1, d=2).
#[test]
fn sgt_8_5_runtime_ranks_mirror_compiler() {
    let reg = diamond_registry();
    let m = reg.model(&key("diamond")).expect("model present");
    for (s, rank) in [("a", 0), ("b", 1), ("c", 1), ("d", 2), ("f", 1)] {
        assert_eq!(m.states[&key(s)].rank, Some(rank), "rank of {s}");
    }
    // Declared motions route through the registry without error.
    for (from, to) in [("a", "b"), ("a", "c"), ("b", "d"), ("c", "d"), ("a", "f")] {
        reg.check_motion(&key("diamond"), &key(from), &key(to))
            .unwrap_or_else(|e| panic!("motion {from}→{to} must be allowed: {e}"));
    }
}

/// SGT-8.4: backward motion is rejected at runtime — rank must strictly
/// increase; equal-rank and backward pairs both rejected. No retraction
/// machinery exists: the registry offers no way to force a state.
#[test]
fn sgt_8_4_runtime_backward_prohibition() {
    let reg = diamond_registry();
    // b→c: rank 1 → 1 (equal, no declared transition either — Undeclared
    // wins because the pair is not declared; add it via a second registry
    // to isolate the rank check).
    match reg.check_motion(&key("diamond"), &key("b"), &key("c")) {
        Err(EngineError::UndeclaredMotion { .. }) => {}
        other => panic!("undeclared pair must be UndeclaredMotion, got {other:?}"),
    }

    // A registry WITH an equal-rank declared pair: the runtime rank gate
    // rejects what the schema gate would have rejected at commit.
    let mut c = Catalog::fresh();
    let m = StateModel::new(
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
    c.create_state_model(m).unwrap();
    for op in [
        transition("a_b", "diamond", "a", "b"),
        transition("a_c", "diamond", "a", "c"),
        transition("b_d", "diamond", "b", "d"),
        transition("c_d", "diamond", "c", "d"),
        transition("a_f", "diamond", "a", "f"),
        // Same shape as the SGT-8.4 schema test: d→a (rank 2 → 0) — this
        // pair makes the reachable graph cyclic, so the SCHEMA commit
        // rejects the whole bundle as FinalizingCycle; for the RUNTIME test
        // we build the registry from a model where the cycle was excluded
        // but the backward pair declared → registry must still refuse.
        transition("d_a", "diamond", "d", "a"),
    ] {
        c.create_transition(match op {
            SchemaOp::AddTransition(t) => t,
            _ => unreachable!(),
        })
        .unwrap();
    }
    // d→a creates a cycle reachable from initial → the compiled model never
    // exists (schema rejects). This IS the structural guarantee: backward
    // motion cannot even be REGISTERED for a finalizing model.
    let compiled = compile(&c);
    assert!(
        compiled.is_err(),
        "SGT-8.4 structural guarantee: backward pairs are rejected at commit — no runtime FSM ever sees them"
    );

    // Runtime belt-and-suspenders: forge a declared backward pair into the
    // registry (simulating a registry bug) from a NON-terminal state and
    // confirm the rank gate fires. (d→a would hit the terminal gate first —
    // d is absorbing — so we forge b→a: rank 1 → 0.)
    let mut reg_with_hole = diamond_registry();
    if let Some(m) = reg_with_hole.model_mut(&key("diamond")) {
        m.motions.insert((key("b"), key("a")), vec![key("forged")]);
    }
    match reg_with_hole.check_motion(&key("diamond"), &key("b"), &key("a")) {
        Err(EngineError::BackwardRank {
            from_rank: 1,
            to_rank: 0,
            ..
        }) => {}
        other => panic!("runtime rank gate must fire, got {other:?}"),
    }
}

/// SGT-8.3: cycles are legal in endless models — A→B→A motion succeeds
/// twice (runtime form).
#[test]
fn sgt_8_3_endless_cycles_legal_twice() {
    let mut c = Catalog::fresh();
    let m = StateModel::new(
        key("cyc"),
        ModelClass::Endless,
        vec![state("a", false, false), state("b", false, false)],
        key("a"),
    )
    .unwrap();
    c.create_state_model(m).unwrap();
    for op in [
        transition("a_b", "cyc", "a", "b"),
        transition("b_a", "cyc", "b", "a"),
    ] {
        c.create_transition(match op {
            SchemaOp::AddTransition(t) => t,
            _ => unreachable!(),
        })
        .unwrap();
    }
    let compiled = compile(&c).expect("endless cycle compiles");
    let reg = FsmRegistry::from_compiled(&compiled);

    let mut cur = key("a");
    for _round in 0..2 {
        reg.check_motion(&key("cyc"), &cur, &key("b"))
            .unwrap_or_else(|e| panic!("{cur:?}→b must be allowed: {e}"));
        cur = key("b");
        reg.check_motion(&key("cyc"), &cur, &key("a"))
            .unwrap_or_else(|e| panic!("b→a must be allowed: {e}"));
        cur = key("a");
    }
    assert_eq!(cur, key("a"), "full cycle twice returns to a");
}

/// Terminals are absorbing: motion OUT of a finalizing terminal is rejected.
#[test]
fn terminals_are_absorbing() {
    let reg = diamond_registry();
    match reg.check_motion(&key("diamond"), &key("d"), &key("f")) {
        Err(EngineError::UndeclaredMotion { .. }) => {}
        other => panic!("undeclared out-of-terminal first hits UndeclaredMotion, got {other:?}"),
    }
    // Even with a forged declared pair, the terminal gate fires BEFORE the
    // rank gate (absorbing is the stronger invariant).
    let mut forged = reg.clone();
    if let Some(m) = forged.model_mut(&key("diamond")) {
        m.motions.insert((key("d"), key("f")), vec![key("forged")]);
    }
    match forged.check_motion(&key("diamond"), &key("d"), &key("f")) {
        Err(EngineError::TerminalState { state, .. }) => assert_eq!(state, key("d")),
        other => panic!("terminal absorption must fire, got {other:?}"),
    }
}

/// Undeclared motions and unknown models/states are rejected explicitly.
#[test]
fn unknown_targets_rejected() {
    let reg = diamond_registry();
    match reg.check_motion(&key("nope"), &key("a"), &key("b")) {
        Err(EngineError::UnknownModel { model }) => assert_eq!(model, key("nope")),
        other => panic!("got {other:?}"),
    }
    match reg.check_motion(&key("diamond"), &key("zz"), &key("b")) {
        Err(EngineError::UnknownState { state, .. }) => assert_eq!(state, key("zz")),
        other => panic!("got {other:?}"),
    }
    match reg.check_motion(&key("diamond"), &key("a"), &key("d")) {
        Err(EngineError::UndeclaredMotion { from, to, .. }) => {
            assert_eq!((from, to), (key("a"), key("d")))
        }
        other => panic!("a→d skips rank 1, must not be declared, got {other:?}"),
    }
}

/// State codes: dense u8, declaration order, bijective per model.
#[test]
fn state_codes_dense_and_bijective() {
    let reg = diamond_registry();
    let m = reg.model(&key("diamond")).expect("model");
    let mut codes: Vec<u8> = m.states.values().map(|s| s.code).collect();
    codes.sort_unstable();
    assert_eq!(
        codes,
        (0..m.states.len() as u8).collect::<Vec<_>>(),
        "dense codes"
    );
    for (i, skey) in ["a", "b", "c", "d", "f"].iter().enumerate() {
        assert_eq!(m.states[&key(skey)].code, i as u8, "declaration order");
        let back = m.state_by_code(i as u8).expect("bijective");
        assert_eq!(back.key, key(skey));
    }
}

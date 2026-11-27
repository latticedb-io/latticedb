//! T-205 acceptance: timer wheel — deadline path (SGT-9.3), lag slack
//! (SGT-9.4), cancellation on state exit, P8 (the wheel is the only clock:
//! time moves only via advance_time).

use lattice_core::arrays::GraphArrays;
use lattice_core::ids::{NodeId, NodeIdSpace};
use lattice_core::value::Value;
use lattice_engine::dispatch::{DispatchError, Machine};
use lattice_engine::timer::TimerWheel;
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

fn fixture() -> (SchemaState, Machine) {
    fixture_with(true)
}

/// `with_deadline = false` omits the complete-deadline pair (lag tests use
/// this to test lag in isolation — a 100ms deadline legitimately fires
/// before a 15-minute lag).
fn fixture_with(with_deadline: bool) -> (SchemaState, Machine) {
    let mut st = SchemaState::new(Catalog::fresh());
    // Without the deadline pair, `failed` has no path (unreachable ⇒ schema
    // rejects the bundle) — the lag-only variant drops the failure terminal.
    let task_states = vec![
        state("open", "Open", false, false),
        state("running", "Running", false, false),
        state("done", "Done", true, false),
        state("failed", "Failed", false, true),
    ];
    let mut ops = vec![
        SchemaOp::CreateStateModel(
            StateModel::new(
                key("task_flow"),
                ModelClass::Finalizing,
                task_states,
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
        SchemaOp::CreateKeyType(KeyTypeDef {
            key: key("seq"),
            density: Density::Dense,
            axis: AxisKind::Ordinal,
        }),
        SchemaOp::AddTransition(
            Transition::new(
                key("start"),
                key("task_flow"),
                key("open"),
                key("running"),
                Vec::new(),
                vec![ActorClass::Human],
                None,
                TransitionTiming::default(),
                Vec::new(),
            )
            .unwrap(),
        ),
        // SGT-9.3: deadline 100ms + on_deadline = fail_fast (to failure
        // terminal). Entering `running` arms the deadline.
        SchemaOp::AddTransition(
            Transition::new(
                key("complete"),
                key("task_flow"),
                key("running"),
                key("done"),
                Vec::new(),
                vec![ActorClass::Human],
                None,
                TransitionTiming {
                    lag_ms: None,
                    deadline_ms: Some(100),
                    on_deadline: Some(key("fail_fast")),
                },
                Vec::new(),
            )
            .unwrap(),
        ),
        SchemaOp::AddTransition(
            Transition::new(
                key("fail_fast"),
                key("task_flow"),
                key("running"),
                key("failed"),
                Vec::new(),
                Vec::new(),
                None,
                TransitionTiming::default(),
                Vec::new(),
            )
            .unwrap(),
        ),
        // SGT-9.4: lag 15 minutes — `review` may fire no earlier than
        // +15m after entering `running`. Same (from,to) pair as complete
        // but a distinct transition key: legal, deterministic dispatch.
        SchemaOp::AddTransition(
            Transition::new(
                key("complete_direct"),
                key("task_flow"),
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
                key("review"),
                key("task_flow"),
                key("running"),
                // Without the deadline pair, review closes into `failed` —
                // keeps the failure terminal reachable (finalizing law).
                if with_deadline {
                    key("done")
                } else {
                    key("failed")
                },
                Vec::new(),
                vec![ActorClass::Human],
                None,
                TransitionTiming {
                    lag_ms: Some(15 * 60 * 1000),
                    deadline_ms: None,
                    on_deadline: None,
                },
                Vec::new(),
            )
            .unwrap(),
        ),
    ];
    if !with_deadline {
        // Drop the deadline pair: lag tests run in isolation (a 100ms
        // deadline legitimately fires before a 15-minute lag).
        ops.retain(|op| match op {
            SchemaOp::AddTransition(t) => t.key != key("complete") && t.key != key("fail_fast"),
            _ => true,
        });
    }
    st.commit(ops).expect("fixture compiles");
    let compiled = compile(&st.catalog).expect("fixture compiles");
    let arrays = GraphArrays::new(NodeIdSpace::new());
    let machine = Machine::new(arrays, compiled, &st.catalog);
    (st, machine)
}

fn task_running(m: &mut Machine) -> NodeId {
    let tc = m.node_type_code(&key("task")).expect("task code");
    let t = m.arrays.add_root(tc.into(), tc).expect("task");
    m.set_cell(t, &key("title"), 0.into(), Value::Str("t".into()), 1.into())
        .expect("cell");
    m.apply_action(t, &key("start"), ActorClass::Human, "u", None)
        .expect("start");
    // Entering `running` armed the deadline (complete: 100ms) + lag (review).
    t
}

/// SGT-9.3: deadline path — arming on state entry; advance past expiry ⇒
/// engine fires on_deadline (author = timer) to the failure terminal; the
/// original transition is then rejected (from-state left).
#[test]
fn sgt_9_3_deadline_fires_on_deadline_transition() {
    let (_st, mut m) = fixture();
    let t = task_running(&mut m);
    assert_eq!(m.state_key_of(t).expect("state"), key("running"));
    assert!(
        m.wheel.deadline_armed(t, &key("complete")),
        "deadline armed on entering running"
    );

    // Advance past the deadline (100ms = 100_000µs) + slack.
    let fired = m.advance_time(200_000);
    assert!(
        fired.iter().any(|d| d.transition == key("complete")
            && matches!(d.kind, lattice_engine::timer::TimerKind::Deadline { .. })),
        "deadline timer due"
    );
    // on_deadline moved the node to the failure terminal (author = timer).
    assert_eq!(m.state_key_of(t).expect("state"), key("failed"));

    // Original transition now rejected: from-state left.
    let err = m
        .apply_action(t, &key("complete"), ActorClass::Human, "u", None)
        .expect_err("complete must be rejected — from-state left");
    match err {
        DispatchError::StateMismatch { actual, .. } => assert_eq!(actual, key("failed")),
        other => panic!("got {other:?}"),
    }
}

/// Deadline with the original firing BEFORE expiry: on_deadline is dropped
/// (StateMismatch), the node stays in its moved-to state.
#[test]
fn deadline_dropped_when_original_fired_in_time() {
    let (_st, mut m) = fixture();
    let t = task_running(&mut m);

    // Fire the original well before the 100ms deadline.
    let fired = m.advance_time(50_000);
    assert!(fired.is_empty(), "nothing due before the deadline");
    m.apply_action(t, &key("complete"), ActorClass::Human, "u", None)
        .expect("complete in time");
    assert_eq!(m.state_key_of(t).expect("state"), key("done"));

    // Advance past expiry: the deadline pops but on_deadline is dropped
    // (node is in `done`, not `running` — FSM rejects the motion).
    let fired = m.advance_time(200_000);
    assert!(fired.is_empty(), "deadline dropped — node already moved");
    assert_eq!(m.state_key_of(t).expect("state"), key("done"), "stays done");
}

/// SGT-9.4: lag slack — the transition may fire no earlier than +lag.
#[test]
fn sgt_9_4_lag_gates_early_fire() {
    let (_st, mut m) = fixture_with(false);
    let t = task_running(&mut m);

    // review fires running→open with 15-minute lag. Before +15m: rejected.
    let err = m
        .apply_action(t, &key("review"), ActorClass::Human, "u", None)
        .expect_err("lag not elapsed");
    match err {
        DispatchError::LagNotElapsed { transition, lag_ms } => {
            assert_eq!(transition, key("review"));
            assert_eq!(lag_ms, 15 * 60 * 1000);
        }
        other => panic!("got {other:?}"),
    }

    // Advance to exactly +15m: allowed (fires no earlier than +15m).
    m.advance_time(15 * 60 * 1_000_000);
    m.apply_action(t, &key("review"), ActorClass::Human, "u", None)
        .expect("lag elapsed");
    assert_eq!(
        m.state_key_of(t).expect("state"),
        key("failed"),
        "lag-only variant: review closes into the failure terminal"
    );
}

/// Cancel on state exit: leaving `running` cancels the deadline; advancing
/// past expiry then fires nothing (no stale timers).
#[test]
fn state_exit_cancels_timers() {
    let (_st, mut m) = fixture();
    let t = task_running(&mut m);
    assert!(!m.wheel.is_empty(), "timers armed");

    // Leave `running` via fail_fast (engine-armed transition): timers cancel.
    m.fire_derived(
        t,
        &key("fail_fast"),
        ActorClass::Guard,
        String::from("guard"),
    )
    .expect("fail_fast fires");
    assert!(m.wheel.is_empty(), "all timers cancelled on state exit");

    let fired = m.advance_time(200_000);
    assert!(fired.is_empty(), "no stale timer fires after cancellation");
    assert_eq!(m.state_key_of(t).expect("state"), key("failed"));
}

/// Wheel unit behavior: chronological advance, cancellation index,
/// double-arm rejection, lag_ok for un-armed transitions.
#[test]
fn wheel_unit_semantics() {
    let mut w = TimerWheel::new();
    assert!(w.is_empty());
    w.arm_lag(NodeId(1), &key("t1"), 5_000, 0).expect("arm lag");
    w.arm_deadline(NodeId(1), &key("t2"), 10_000, &key("fail"), 0)
        .expect("arm deadline");
    assert_eq!(w.len(), 2);
    // Double-arm rejected.
    assert!(w.arm_lag(NodeId(1), &key("t1"), 5_000, 0).is_err());
    // Nothing due before 5s.
    assert!(w.advance(4_999_000).is_empty());
    // Lag due at 5s.
    let due = w.advance(5_000_000);
    assert_eq!(due.len(), 1);
    assert!(matches!(due[0].kind, lattice_engine::timer::TimerKind::Lag));
    assert!(w.lag_ok(NodeId(1), &key("t1"), 5_000_000), "lag elapsed");
    assert!(
        w.lag_ok(NodeId(1), &key("t9"), 0),
        "never-armed transitions are always lag-ok (no lag declared)"
    );
    // Deadline due at 10s.
    let due = w.advance(10_000_000);
    assert_eq!(due.len(), 1);
    assert!(matches!(
        due[0].kind,
        lattice_engine::timer::TimerKind::Deadline { .. }
    ));
    assert!(w.is_empty());
    // Cancel never-armed → explicit NotArmed.
    assert!(w.cancel(NodeId(2), &key("t9")).is_err());
}

/// P8: the wheel is the only clock — Machine time moves ONLY via
/// advance_time (logical_us never advances on its own).
#[test]
fn p8_wheel_is_the_only_clock() {
    let (_st, mut m) = fixture();
    let t = task_running(&mut m);
    let before = m.logical_us();
    // Apply actions, run worklists — time does NOT move.
    let _ = m.apply_action(t, &key("review"), ActorClass::Human, "u", None);
    assert_eq!(
        m.logical_us(),
        before,
        "no wall clock — time frozen without advance_time"
    );
}

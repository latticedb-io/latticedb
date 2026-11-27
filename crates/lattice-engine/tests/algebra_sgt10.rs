//! T-203 acceptance: dependency algebra — SGT-10.1 (KOFN param range),
//! SGT-10.2 (WEIGHTED normalization), SGT-10.3 (guard param resolution),
//! plus satisfaction rollup semantics per kind group (AND/OR/KOFN/WEIGHTED)
//! and confluence (order-insensitivity property).

use lattice_core::value::Decimal;
use lattice_engine::algebra::{
    IncomingEdge, Kind, ParamError, Verdict, evaluate_incoming, validate_guard_params,
    validate_kofn, validate_kofn_against_degree, validate_weights_normalized,
};
use lattice_schema::key::Key;
use std::collections::BTreeMap;

fn key(s: &str) -> Key {
    Key::parse(s).unwrap()
}

/// SGT-10.1: KOFN k=3 over 2 incoming edges → INVALID_KIND_PARAMS.
#[test]
fn sgt_10_1_kofn_param_range() {
    // k must not exceed the incoming degree (3 over 2 sources).
    let err = validate_kofn_against_degree(3, 2).expect_err("k=3 over 2 rejected");
    assert!(matches!(
        err,
        ParamError::InvalidKindParams { kind: "KOFN", .. }
    ));
    // k=0 is always invalid.
    let err = validate_kofn(0).expect_err("k=0 rejected");
    assert!(matches!(
        err,
        ParamError::InvalidKindParams { kind: "KOFN", .. }
    ));
    // k=2 over 2 sources is fine; k=2 over 3 also fine.
    assert!(validate_kofn_against_degree(2, 2).is_ok());
    assert!(validate_kofn_against_degree(2, 3).is_ok());
    assert!(validate_kofn(2).is_ok());
}

/// SGT-10.2: WEIGHTED edges into one target summing to 1.0001 → rejected;
/// summing to 1.0000 → accepted. Weights must be in (0, 1].
#[test]
fn sgt_10_2_weights_normalization() {
    // 0.5000 + 0.5000 = 1.0000 → accepted.
    assert!(validate_weights_normalized(9, &[5000, 5000]).is_ok());
    // 0.5000 + 0.5001 = 1.0001 → rejected.
    let err = validate_weights_normalized(9, &[5000, 5001]).expect_err("1.0001 rejected");
    assert!(matches!(
        err,
        ParamError::WeightsNotNormalized {
            target: 9,
            total_unscaled: 10001
        }
    ));
    // Zero and negative weights rejected.
    assert!(matches!(
        validate_weights_normalized(9, &[0, 10000]),
        Err(ParamError::InvalidKindParams {
            kind: "WEIGHTED",
            ..
        })
    ));
    // decimal(6,4) type-level cap: p=6, s=4 → unscaled ≤ 9999 cannot even
    // represent 1.0000? No: decimal(6,4) means 6 total digits, 4 after the
    // point → max 99.9999. Weight 1.0000 = unscaled 10000 with s=4 → 6 total
    // digits (1.0000 has 5 significant digits +1 leading zero) ✓.
    let w = Decimal::new(10000, 6, 4).expect("1.0000 in decimal(6,4)");
    assert_eq!(w.unscaled, 10000);
    let over = Decimal::new(10001, 6, 4).expect("1.0001 fits p=6");
    assert_eq!(over.unscaled, 10001);
}

/// SGT-10.3: GUARD sigma=unknown state → SCHEMA (unresolved reference);
/// tau and predicate likewise.
#[test]
fn sgt_10_3_guard_param_resolution() {
    let mut sigma_states = BTreeMap::new();
    sigma_states.insert(key("running"), ());
    let mut tau_transitions = BTreeMap::new();
    tau_transitions.insert(key("close"), ());
    let mut rules = BTreeMap::new();
    rules.insert(key("has_evidence"), ());

    // sigma=running ✓, tau=close ✓ → OK.
    assert!(
        validate_guard_params(
            &key("running"),
            &key("close"),
            &None,
            &sigma_states,
            &tau_transitions,
            &rules,
        )
        .is_ok()
    );

    // sigma=unknown → SCHEMA.
    let err = validate_guard_params(
        &key("unknown_state"),
        &key("close"),
        &None,
        &sigma_states,
        &tau_transitions,
        &rules,
    )
    .expect_err("unknown sigma");
    assert!(matches!(
        err,
        ParamError::UnresolvedReference { kind: "GUARD", .. }
    ));

    // tau=unknown → SCHEMA.
    let err = validate_guard_params(
        &key("running"),
        &key("nope"),
        &None,
        &sigma_states,
        &tau_transitions,
        &rules,
    )
    .expect_err("unknown tau");
    assert!(matches!(
        err,
        ParamError::UnresolvedReference { kind: "GUARD", .. }
    ));

    // predicate=unknown → SCHEMA.
    let err = validate_guard_params(
        &key("running"),
        &key("close"),
        &Some(key("ghost_rule")),
        &sigma_states,
        &tau_transitions,
        &rules,
    )
    .expect_err("unknown predicate");
    assert!(matches!(
        err,
        ParamError::UnresolvedReference { kind: "GUARD", .. }
    ));
}

/// Satisfaction rollup per kind group: AND all, OR any, KOFN ≥ k,
/// WEIGHTED full weight; groups combine conjunctively.
#[test]
fn satisfaction_rollup_per_group() {
    // AND: all satisfied.
    let and_ok: Vec<IncomingEdge> = vec![(Kind::And, true, 0), (Kind::And, true, 0)];
    assert_eq!(evaluate_incoming(&and_ok, None), Verdict::Satisfied);
    let and_bad: Vec<IncomingEdge> = vec![(Kind::And, true, 0), (Kind::And, false, 0)];
    assert_eq!(evaluate_incoming(&and_bad, None), Verdict::NotSatisfied);

    // OR: any satisfied.
    let or_ok: Vec<IncomingEdge> = vec![(Kind::Or, false, 0), (Kind::Or, true, 0)];
    assert_eq!(evaluate_incoming(&or_ok, None), Verdict::Satisfied);
    let or_bad: Vec<IncomingEdge> = vec![(Kind::Or, false, 0), (Kind::Or, false, 0)];
    assert_eq!(evaluate_incoming(&or_bad, None), Verdict::NotSatisfied);

    // AND + OR combine conjunctively: one AND unsatisfied kills it.
    let mixed_bad: Vec<IncomingEdge> = vec![(Kind::And, false, 0), (Kind::Or, true, 0)];
    assert_eq!(evaluate_incoming(&mixed_bad, None), Verdict::NotSatisfied);

    // Unconditional: no incoming edges (root target).
    assert_eq!(evaluate_incoming(&[], None), Verdict::Unconditional);
}

/// KOFN semantics at the evaluator level: k=2 of 3 sources — all-satisfied
/// ⇒ satisfied; partial ⇒ caller refines against the k param.
#[test]
fn kofn_rollup() {
    let three: Vec<IncomingEdge> = vec![(Kind::Kofn, true, 0); 3];
    assert_eq!(
        evaluate_incoming(&three, Some(2)),
        Verdict::Satisfied,
        "3 ≥ k=2"
    );
    let two_of_three: Vec<IncomingEdge> = vec![
        (Kind::Kofn, true, 0),
        (Kind::Kofn, true, 0),
        (Kind::Kofn, false, 0),
    ];
    assert_eq!(
        evaluate_incoming(&two_of_three, Some(2)),
        Verdict::Satisfied,
        "2 ≥ k=2"
    );
    let one_of_three: Vec<IncomingEdge> = vec![
        (Kind::Kofn, true, 0),
        (Kind::Kofn, false, 0),
        (Kind::Kofn, false, 0),
    ];
    assert_eq!(
        evaluate_incoming(&one_of_three, Some(2)),
        Verdict::NotSatisfied,
        "1 < k=2"
    );
    // Absent k param: conservative all-satisfied default.
    assert_eq!(
        evaluate_incoming(&two_of_three, None),
        Verdict::NotSatisfied
    );
}

/// WEIGHTED rollup: full weight (all satisfied, normalized set) ⇒ satisfied;
/// partial weight ⇒ not.
#[test]
fn weighted_rollup() {
    // Edges into one target share the normalized pool: 4 edges × 2500 = 1.0000
    // (SGT-10.2 normalization). All satisfied ⇒ sum = 10000 ⇒ satisfied.
    let all: Vec<IncomingEdge> = vec![
        (Kind::Weighted, true, 2500),
        (Kind::Weighted, true, 2500),
        (Kind::Weighted, true, 2500),
        (Kind::Weighted, true, 2500),
    ];
    assert_eq!(evaluate_incoming(&all, None), Verdict::Satisfied);
    // One of two 5000-weight edges satisfied ⇒ 5000 < 10000 ⇒ not satisfied.
    let partial: Vec<IncomingEdge> =
        vec![(Kind::Weighted, true, 5000), (Kind::Weighted, false, 5000)];
    assert_eq!(evaluate_incoming(&partial, None), Verdict::NotSatisfied);
}

/// Confluence property: the verdict is invariant under permutation of the
/// incoming edge order (P15 — order-insensitive evaluation).
#[test]
fn verdict_invariant_under_permutation() {
    let base: Vec<IncomingEdge> = vec![
        (Kind::And, true, 0),
        (Kind::Or, false, 0),
        (Kind::Weighted, true, 10000),
        (Kind::And, true, 0),
        (Kind::Or, true, 0),
    ];
    let v1 = evaluate_incoming(&base, None);
    let v2 = evaluate_incoming(&base.iter().rev().cloned().collect::<Vec<_>>(), None);
    let v3 = evaluate_incoming(
        &base[3..]
            .iter()
            .chain(base[..3].iter())
            .cloned()
            .collect::<Vec<_>>(),
        None,
    );
    assert_eq!(v1, v2);
    assert_eq!(v1, v3);
}

/// Registry byte stability: kind ↔ byte round-trips (the on-disk edge kind).
#[test]
fn kind_bytes_roundtrip() {
    for b in 1..=8u8 {
        let k = Kind::from_byte(b).expect("registry byte");
        assert_eq!(k.byte(), b);
    }
    assert!(Kind::from_byte(0).is_none());
    assert!(Kind::from_byte(9).is_none());
}

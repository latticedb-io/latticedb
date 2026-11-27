//! T-102 acceptance: catalog entities + key grammar — SGT-2.1–2.3, plus the
//! structural entity rules they sit on (SPEC-01 §4/§5/§8/§9 unit level).

use lattice_schema::catalog::{BUILTIN_SCALARS, Catalog, EDGE_KINDS};
use lattice_schema::entities::*;
use lattice_schema::key::{Key, KeyError};

fn key(s: &str) -> Key {
    Key::parse(s).unwrap()
}

fn node_type(k: &str) -> NodeType {
    NodeType {
        key: key(k),
        display_name: "Task".into(),
        parent_type: None,
        state_model: key("task_sm"),
        attribute_sets: Vec::new(),
        key_type: key("seq"),
        state_locks: Vec::new(),
    }
}

/// SGT-17.1 shape at catalog level: built-ins present, user catalog empty.
#[test]
fn fresh_catalog_matches_spec_17_shape() {
    let c = Catalog::fresh();
    assert_eq!(c.schema_epoch, 0);
    assert!(c.graphs.is_empty() && c.node_types.is_empty());
    assert_eq!(BUILTIN_SCALARS.len(), 9, "closed built-in scalar set");
    assert_eq!(EDGE_KINDS.len(), 8, "fixed edge-kind registry");
}

/// SGT-2.1: key grammar violations are rejected before any mutation; the
/// catalog epoch stays untouched.
#[test]
fn sgt_2_1_key_grammar_enforced() {
    for bad in ["9bad", "Bad", "a-b"] {
        let e = Key::parse(bad).expect_err("grammar violation must be rejected");
        assert!(
            matches!(
                e,
                KeyError::InvalidStart | KeyError::InvalidChar { .. } | KeyError::TooLong { .. }
            ),
            "`{bad}` → {e}"
        );
    }

    // The catalog API cannot even receive an invalid key: Key::parse gates
    // construction. Verify an insertion attempt with a valid shape succeeds
    // and violations never reach the map.
    let mut c = Catalog::fresh();
    c.create_node_type(node_type("task"))
        .expect("valid key accepted");
    assert_eq!(
        c.schema_epoch, 0,
        "raw inserts never bump the epoch (txn owns it)"
    );
    assert_eq!(c.node_types.len(), 1);
}

/// SGT-2.2: key uniqueness per namespace — second `task` rejected, first
/// entity unaffected.
#[test]
fn sgt_2_2_key_uniqueness_per_namespace() {
    let mut c = Catalog::fresh();
    c.create_node_type(node_type("task"))
        .expect("first insert ok");

    let mut second = node_type("task");
    second.display_name = "Impostor".into();
    let e = c
        .create_node_type(second)
        .expect_err("duplicate key must be rejected");
    assert_eq!(
        e,
        SchemaError::AlreadyExists {
            namespace: "node_type",
            key: key("task")
        },
        "got: {e}"
    );

    let first = c
        .node_types
        .get(&key("task"))
        .expect("first entity unaffected");
    assert_eq!(first.display_name, "Task");
    assert_eq!(c.node_types.len(), 1);

    // Same key in a DIFFERENT namespace is legal (namespace-scoped uniqueness).
    c.create_state_model(
        StateModel::new(
            key("task_sm"),
            ModelClass::Endless,
            vec![State {
                key: key("running"),
                name: "Running".into(),
                success_terminal: false,
                failure_terminal: false,
            }],
            key("running"),
        )
        .unwrap(),
    )
    .expect("same key shape in another namespace is fine");
}

/// SGT-2.3: keys are immutable — there is no rename op in the mutation
/// surface; "altering" by re-insert is rejected, the original stands.
#[test]
fn sgt_2_3_keys_immutable() {
    let mut c = Catalog::fresh();
    c.create_node_type(node_type("task")).expect("insert ok");

    let mut altered = node_type("task");
    altered.display_name = "Renamed".into();
    let e = c
        .create_node_type(altered)
        .expect_err("no rename/alter path exists");
    assert!(matches!(e, SchemaError::AlreadyExists { .. }));

    let first = c.node_types.get(&key("task")).expect("original intact");
    assert_eq!(first.display_name, "Task", "original payload untouched");
}

/// Structural entity rules (unit level): enum bounds, record bounds + depth-1
/// by construction, state-model class rules, actor-class law.
#[test]
fn entity_structural_validation() {
    // EnumType: non-empty, ≤256, unique (SPEC-01 §4).
    assert!(EnumType::new(key("e"), Vec::new()).is_err());
    let many: Vec<Key> = (0..257u32).map(|i| key(&format!("v{i}"))).collect();
    assert!(matches!(
        EnumType::new(key("e"), many),
        Err(SchemaError::EnumBounds { count: 257, .. })
    ));
    assert!(matches!(
        EnumType::new(key("e"), vec![key("a"), key("a")]),
        Err(SchemaError::EnumValueDuplicate { value, .. }) if value == key("a")
    ));

    // RecordType: 1..=64 fields, unique names (SPEC-01 §5); depth-1 is a
    // type-level guarantee — ElemType has no record variant.
    let fields: Vec<(Key, ElemType)> = (0..65u32)
        .map(|i| (key(&format!("f{i}")), ElemType::Scalar(ScalarType::I64)))
        .collect();
    assert!(matches!(
        RecordType::new(key("r"), fields),
        Err(SchemaError::RecordBounds { count: 65, .. })
    ));
    assert!(
        RecordType::new(
            key("r"),
            vec![
                (
                    key("amount"),
                    ElemType::Scalar(ScalarType::Decimal {
                        precision: 10,
                        scale: 2
                    })
                ),
                (key("tag"), ElemType::Enum(key("e")))
            ]
        )
        .is_ok()
    );

    // StateModel class rules (SGT-8.1/8.2 unit level).
    let running = State {
        key: key("running"),
        name: "R".into(),
        success_terminal: false,
        failure_terminal: false,
    };
    let done_ok = State {
        key: key("done"),
        name: "D".into(),
        success_terminal: true,
        failure_terminal: false,
    };
    let failed = State {
        key: key("failed"),
        name: "F".into(),
        success_terminal: false,
        failure_terminal: true,
    };
    assert!(matches!(
        StateModel::new(
            key("m"),
            ModelClass::Finalizing,
            vec![running.clone(), done_ok.clone()],
            key("running")
        ),
        Err(SchemaError::FinalizingMissingTerminal {
            class: "failure",
            ..
        })
    ));
    let terminal_init = State {
        key: key("done"),
        ..done_ok.clone()
    };
    assert!(matches!(
        StateModel::new(
            key("m"),
            ModelClass::Finalizing,
            vec![terminal_init, failed.clone(), running.clone()],
            key("done")
        ),
        Err(SchemaError::InitialStateInvalid { .. })
    ));
    assert!(matches!(
        StateModel::new(
            key("m"),
            ModelClass::Endless,
            vec![done_ok.clone()],
            key("done")
        ),
        Err(SchemaError::EndlessHasTerminal { .. })
    ));
    assert!(
        StateModel::new(
            key("m"),
            ModelClass::Endless,
            vec![running.clone()],
            key("running")
        )
        .is_ok()
    );

    // Actor-class law (SPEC-01 §9): machine classes never alone in a set.
    assert!(matches!(
        Transition::new(
            key("t"),
            key("m"),
            key("running"),
            key("done"),
            Vec::new(),
            vec![ActorClass::System],
            None,
            TransitionTiming::default(),
            Vec::new()
        ),
        Err(SchemaError::MachineOnlyActorSet { .. })
    ));
    assert!(
        Transition::new(
            key("t"),
            key("m"),
            key("running"),
            key("done"),
            Vec::new(),
            vec![ActorClass::Human, ActorClass::Timer],
            None,
            TransitionTiming::default(),
            Vec::new()
        )
        .is_ok()
    );
    let t = Transition::new(
        key("t"),
        key("m"),
        key("running"),
        key("done"),
        Vec::new(),
        Vec::new(),
        None,
        TransitionTiming::default(),
        Vec::new(),
    )
    .expect("empty actors = engine-only");
    assert!(!t.is_action());
    let act = Transition::new(
        key("a"),
        key("m"),
        key("running"),
        key("done"),
        Vec::new(),
        vec![ActorClass::Agent],
        None,
        TransitionTiming::default(),
        Vec::new(),
    )
    .unwrap();
    assert!(act.is_action());
}

/// Grants: idempotent grant + counted revoke (SPEC-01 §14).
#[test]
fn grants_idempotent_and_revocable() {
    let mut c = Catalog::fresh();
    c.create_role(Role {
        key: key("reader"),
        scopes: vec![Scope::ReadSubtree],
    })
    .unwrap();
    let target = GrantTarget::Graph(key("main"));
    c.grant(Grant {
        role: key("reader"),
        subject: "agent:42".into(),
        target: target.clone(),
    });
    c.grant(Grant {
        role: key("reader"),
        subject: "agent:42".into(),
        target: target.clone(),
    });
    assert_eq!(c.grants.len(), 1, "duplicate grant is a no-op");
    assert_eq!(c.revoke(&key("reader"), "agent:42", &target), 1);
    assert_eq!(c.revoke(&key("reader"), "agent:42", &target), 0);
}

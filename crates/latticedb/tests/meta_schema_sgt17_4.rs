//! T-209 acceptance: active meta-schema e2e (SPEC-01 §17, SGT-17.4) — the
//! engine does NOT distinguish "fixed after init" from "continuously
//! evolved" catalogs: a tenant holding `administer` scope mutates the
//! catalog via schema transactions across the service lifetime while live
//! facts exist; replay reproduces the full epoch history.

use lattice_core::arrays::GraphArrays;
use lattice_core::ids::NodeIdSpace;
use lattice_core::value::Value;
use lattice_engine::dispatch::Machine;
use lattice_schema::catalog::Catalog;
use lattice_schema::compile::{compile, compile_with_lineage};
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use lattice_schema::txn::{SchemaOp, SchemaState};
use latticedb::bundles::install_named;

fn key(s: &str) -> Key {
    Key::parse(s).unwrap()
}

/// Placeholder for the mem::replace handoff — never dereferenced (recompile
/// takes the real machine's state before returning).
fn unreachable_service_machine() -> Machine {
    let compiled = compile(&Catalog::fresh()).expect("empty catalog compiles");
    Machine::new(
        GraphArrays::new(NodeIdSpace::new()),
        compiled,
        &Catalog::fresh(),
    )
}

fn state(k: &str, name: &str, success: bool, failure: bool) -> State {
    State {
        key: key(k),
        name: name.into(),
        success_terminal: success,
        failure_terminal: failure,
    }
}

/// The service runtime: schema state + machine rebuilt per schema epoch
/// (the engine recompiles masks at every epoch bump — ARCHITECTURE §7).
struct Service {
    schema: SchemaState,
    machine: Machine,
}

impl Service {
    fn bootstrap() -> Self {
        let mut schema = SchemaState::new(Catalog::fresh());
        install_named(&mut schema, "process-mgmt").expect("bundle install");
        let compiled = compile(&schema.catalog).expect("bootstrap compiles");
        let machine = Machine::new(
            GraphArrays::new(lattice_core::ids::NodeIdSpace::new()),
            compiled,
            &schema.catalog,
        );
        Self { schema, machine }
    }

    /// Evolve: apply a schema txn, recompile, rebuild the machine — live
    /// facts ride in the arrays, which transfer across rebuilds untouched.
    fn evolve(&mut self, ops: Vec<SchemaOp>) {
        self.schema.commit(ops).expect("tenant schema txn accepted");
        // Lineage-stable recompile: the new epoch inherits the old attr_ids.
        let prev_ids = self.machine.compiled_ref().attr_ids.clone();
        let compiled =
            compile_with_lineage(&self.schema.catalog, Some(&prev_ids)).expect("compiles");
        self.machine = std::mem::replace(&mut self.machine, unreachable_service_machine())
            .recompile(compiled, &self.schema.catalog);
    }

    fn task_code(&self) -> u16 {
        self.machine
            .node_type_code(&key("task"))
            .expect("task type present")
    }

    /// Observable state: (node states by key, machine invariant check).
    fn assert_invariants(&self) {
        self.machine.arrays.assert_invariants();
    }
}

/// SGT-17.4: tenant evolves the catalog across the service lifetime —
/// schema txns succeed across epochs while live facts are untouched; replay
/// (op-log re-application) reproduces the full epoch history; masks
/// recompile identically.
#[test]
fn sgt_17_4_active_meta_schema_across_live_facts() {
    // ---- Phase 1: bootstrap via template bundle + live facts ----
    let mut svc = Service::bootstrap();
    let tc = svc.task_code();
    let t1 = svc
        .machine
        .arrays
        .add_root(tc.into(), tc)
        .expect("live task");
    svc.machine.arrays.set_state(t1, 0).expect("state open");
    svc.machine
        .set_cell(
            t1,
            &key("task_title"),
            0.into(),
            Value::Str("live task".into()),
            1.into(),
        )
        .expect("live cell");
    let epoch_bootstrap = svc.schema.catalog.schema_epoch;
    svc.assert_invariants();

    // ---- Phase 2: additive evolution — new attribute + set (free per §16) ----
    svc.evolve(vec![
        SchemaOp::CreateAttribute(AttributeDef {
            key: key("priority"),
            attr_type: AttrType::Scalar(ScalarType::I64),
            required: false,
            default: None,
            codec_override: None,
            monotonicity: Monotonicity::Increasing,
            write_scope: None,
        }),
        SchemaOp::CreateAttributeSet(AttributeSet {
            key: key("task_priority"),
            kind: SetKind::ScalarSet,
            members: vec![key("priority")],
            write_scope: None,
        }),
    ]);

    // ---- Phase 3: model evolution mid-lifetime (registry-legal ops) ----
    // AddState + its incoming transition land in ONE txn: the txn-level
    // compile gate requires reachability at every commit (a lone new state
    // would be unreachable). unblock/resolve MUST go forward in rank
    // (blocked=1 → done=2; blocked→open is backward — SGT-8.4 law).
    svc.evolve(vec![
        SchemaOp::AddState {
            model_key: key("task_flow"),
            state: state("blocked", "Blocked", false, false),
        },
        SchemaOp::AddTransition(
            Transition::new(
                key("block"),
                key("task_flow"),
                key("open"),
                key("blocked"),
                Vec::new(),
                vec![ActorClass::Human],
                None,
                TransitionTiming::default(),
                Vec::new(),
            )
            .expect("block"),
        ),
    ]);
    svc.evolve(vec![SchemaOp::AddTransition(
        Transition::new(
            key("resolve"),
            key("task_flow"),
            key("blocked"),
            key("done"),
            Vec::new(),
            vec![ActorClass::Human],
            None,
            TransitionTiming::default(),
            Vec::new(),
        )
        .expect("resolve"),
    )]);

    // ---- Phase 4: the NEW transition fires on the OLD live node ----
    // Live facts survived every evolution (same node id, same state).
    assert_eq!(
        svc.machine.arrays.state_of(t1),
        0,
        "still open after 4 evolutions"
    );
    let outcome = svc
        .machine
        .apply_action(t1, &key("block"), ActorClass::Human, "tenant", None)
        .expect("NEW transition fires on the pre-evolution node");
    assert_eq!(outcome.to, key("blocked"));
    let outcome = svc
        .machine
        .apply_action(t1, &key("resolve"), ActorClass::Human, "tenant", None)
        .expect("resolve moves blocked → done (forward)");
    assert_eq!(outcome.to, key("done"));
    svc.assert_invariants();

    // ---- Epoch history ----
    assert_eq!(
        svc.schema.catalog.schema_epoch,
        epoch_bootstrap + 3,
        "bundle + attr/set txn + model-evo txn + resolve txn = 4 epochs"
    );

    // ---- Replay: re-apply the op-log into a fresh state ----
    let replayed_txns = svc.schema.ops_log.clone();
    let mut replay = SchemaState::new(Catalog::fresh());
    for txn_ops in replayed_txns {
        replay.commit(txn_ops).expect("replay txn accepted");
    }
    assert_eq!(
        replay.catalog.schema_epoch, svc.schema.catalog.schema_epoch,
        "replay reproduces the full epoch history"
    );
    assert_eq!(
        replay.catalog, svc.schema.catalog,
        "catalog reproduces exactly"
    );
    assert_eq!(
        compile(&replay.catalog),
        compile(&svc.schema.catalog),
        "masks identical"
    );

    // Live-cell check: the cell written in phase 1 is still there.
    let cell = svc
        .machine
        .latest_cell(t1, &key("task_title"))
        .expect("live cell survived every evolution");
    assert_eq!(cell.value, Value::Str("live task".into()));
}

/// SGT-17.4 companion: tenant drains an evolved rule mid-lifetime
/// (registry-legal: RemoveRule exists; note v1 has NO RemoveAttribute —
/// BACKLOG SQ-01).
#[test]
fn active_meta_schema_evolve_and_drain() {
    let mut st = SchemaState::new(Catalog::fresh());
    install_named(&mut st, "state-driver").expect("bundle");

    st.commit(vec![SchemaOp::CreateAttribute(AttributeDef {
        key: key("temp_field"),
        attr_type: AttrType::Scalar(ScalarType::String),
        required: false,
        default: None,
        codec_override: None,
        monotonicity: Monotonicity::None,
        write_scope: None,
    })])
    .expect("temp attr added");
    st.commit(vec![SchemaOp::CreateRule(NamedRule {
        key: key("temp_rule"),
        form: PredicateForm::AttrPresent {
            attr: key("temp_field"),
        },
    })])
    .expect("temp rule added");

    // Drain: remove the rule (registry-legal). NOTE: the attribute itself
    // is eternal in v1 — §16's registry has no RemoveAttribute op (see
    // BACKLOG SQ-01; R13's text presupposes one, the registry lacks it).
    st.commit(vec![SchemaOp::RemoveRule {
        rule_key: key("temp_rule"),
    }])
    .expect("drained rule removed");
    assert!(!st.catalog.rules.contains_key(&key("temp_rule")));
    assert!(st.catalog.attributes.contains_key(&key("temp_field")));

    // Replay reproduces both the additions and the removal.
    let mut replay = SchemaState::new(Catalog::fresh());
    for txn_ops in st.ops_log.clone() {
        replay.commit(txn_ops).expect("replay txn accepted");
    }
    assert_eq!(replay.catalog, st.catalog, "post-drain replay identical");
    assert!(!replay.catalog.rules.contains_key(&key("temp_rule")));
}

/// Op-log is append-only across the whole lifetime: BOTH the add and the
/// alter are recorded (replay fidelity depends on it).
#[test]
fn ops_log_is_append_only() {
    let mut st = SchemaState::new(Catalog::fresh());
    install_named(&mut st, "process-mgmt").expect("bundle");
    let after_bundle = st.ops_log.len();

    st.commit(vec![SchemaOp::CreateAttribute(AttributeDef {
        key: key("extra"),
        attr_type: AttrType::Scalar(ScalarType::Bool),
        required: false,
        default: None,
        codec_override: None,
        monotonicity: Monotonicity::None,
        write_scope: None,
    })])
    .expect("attr added");
    st.commit(vec![SchemaOp::AlterAttributeCodec {
        attr_key: key("extra"),
        codec: "identity".into(),
    }])
    .expect("codec altered");
    assert_eq!(
        st.ops_log.len(),
        after_bundle + 2,
        "op-log records BOTH the add and the alter — journal-shaped history"
    );
    assert_eq!(
        st.catalog.attributes[&key("extra")]
            .codec_override
            .as_deref(),
        Some("identity"),
        "alter visible in catalog"
    );
}

//! T-112 acceptance: template bundles — fresh-DB shape (SGT-17.1),
//! idempotence (SGT-17.2), install correctness (SGT-17.3).

use lattice_schema::catalog::{BUILTIN_SCALARS, Catalog, EDGE_KINDS};
use lattice_schema::compile::compile;
use latticedb::bundles::{install, install_named, shipped};

/// SGT-17.1: fresh DB shape — exactly built-in scalars + edge registry,
/// zero graphs, epoch 0. The kernel ships no business content.
#[test]
fn sgt_17_1_fresh_db_shape() {
    let c = Catalog::fresh();
    assert_eq!(c.schema_epoch, 0);
    assert_eq!(c.graphs.len(), 0);
    assert_eq!(c.node_types.len(), 0);
    assert_eq!(BUILTIN_SCALARS.len(), 9, "built-in scalars present");
    assert_eq!(EDGE_KINDS.len(), 8, "edge-kind registry present");
    // No bundle content is pre-seeded.
    assert!(c.state_models.is_empty() && c.attributes.is_empty() && c.roles.is_empty());
}

/// SGT-17.2: bundle idempotence — install twice; the second run is a no-op
/// (all entities exist) and the epoch grew only ONCE across both installs.
#[test]
fn sgt_17_2_bundle_idempotence() {
    for name in ["process-mgmt", "state-driver", "analytics"] {
        let mut st = lattice_schema::txn::SchemaState::new(Catalog::fresh());
        let first = install_named(&mut st, name).expect("first install");
        assert!(first.installed);
        assert_eq!(first.applied, first.applied, "report carries counts");
        assert_eq!(
            first.epoch_after - first.epoch_before,
            1,
            "one txn per install"
        );

        let epoch_after_first = st.catalog.schema_epoch;
        let second = install_named(&mut st, name).expect("second install must not fail");
        assert!(!second.installed, "re-install is a no-op");
        assert_eq!(second.applied, 0, "nothing new to apply");
        assert_eq!(
            second.skipped,
            first.applied + first.skipped,
            "all ops satisfied"
        );
        assert_eq!(
            st.catalog.schema_epoch, epoch_after_first,
            "epoch grew only once"
        );
    }
}

/// SGT-17.3: install `process-mgmt` — every declared type/model/transition
/// exists, and a sample graph is instantiable (compile validates the whole
/// reference graph).
#[test]
fn sgt_17_3_process_mgmt_install_correctness() {
    let mut st = lattice_schema::txn::SchemaState::new(Catalog::fresh());
    let report = install_named(&mut st, "process-mgmt").expect("install");
    assert!(report.installed);

    let c = &st.catalog;
    assert!(
        c.key_types
            .contains_key(&lattice_schema::key::Key::parse("task_seq").unwrap())
    );
    assert!(
        c.enums
            .contains_key(&lattice_schema::key::Key::parse("task_status").unwrap())
    );
    assert!(
        c.attributes
            .contains_key(&lattice_schema::key::Key::parse("task_title").unwrap())
    );
    assert!(
        c.attr_sets
            .contains_key(&lattice_schema::key::Key::parse("task_meta").unwrap())
    );
    assert!(
        c.state_models
            .contains_key(&lattice_schema::key::Key::parse("task_flow").unwrap())
    );
    for t in ["task_start", "task_complete", "task_fail"] {
        assert!(
            c.transitions
                .contains_key(&lattice_schema::key::Key::parse(t).unwrap()),
            "transition {t} present"
        );
    }
    assert!(
        c.node_types
            .contains_key(&lattice_schema::key::Key::parse("task").unwrap())
    );

    // Sample graph instantiable: the compiled catalog has action masks for
    // the task type, ranks for the flow, and the enabled-action index.
    let compiled = compile(c).expect("bundle catalog compiles");
    let task = lattice_schema::key::Key::parse("task").unwrap();
    let open = lattice_schema::key::Key::parse("open").unwrap();
    let actions = compiled
        .action_index
        .get(&(task, open.clone()))
        .expect("task@open enables actions");
    assert_eq!(actions.len(), 1, "task_start is the only action at open");
    let task_flow = lattice_schema::key::Key::parse("task_flow").unwrap();
    assert_eq!(compiled.ranks.get(&(task_flow, open)), Some(&0));
}

/// All three shipped bundles coexist in one Database (distinct namespaces)
/// and the whole set installs atomically per bundle.
#[test]
fn all_bundles_install_in_one_database() {
    let mut st = lattice_schema::txn::SchemaState::new(Catalog::fresh());
    let bundles = shipped();
    assert_eq!(bundles.len(), 3, "v1 ships exactly three bundles");
    for b in &bundles {
        install(&mut st, b).expect("install");
    }
    // Cross-bundle compile sanity.
    compile(&st.catalog).expect("combined catalog compiles");
    assert!(st.catalog.schema_epoch >= 3, "one txn per bundle");
}

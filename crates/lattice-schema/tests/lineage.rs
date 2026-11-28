//! Lineage of ids (ARCHITECTURE §4): node-type codes, graph ids and attribute
//! ids are indexes of FIRST APPEARANCE in the committed op history — never a
//! per-epoch sort order. Adding an entity that sorts before existing ones
//! must not renumber them: snapshots carry codes per node and parts carry
//! attribute ids.

use lattice_core::ids::AttrId;
use lattice_schema::catalog::Catalog;
use lattice_schema::compile::compile_with_lineage;
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use lattice_schema::txn::{SchemaOp, SchemaState};

fn key(s: &str) -> Key {
    Key::parse(s).unwrap()
}

fn graph(k: &str) -> SchemaOp {
    SchemaOp::CreateGraph(GraphSchema {
        key: key(k),
        display_name: k.to_string(),
        created_at_us: 0,
    })
}

fn attr(k: &str) -> SchemaOp {
    SchemaOp::CreateAttribute(AttributeDef {
        key: key(k),
        attr_type: AttrType::Scalar(ScalarType::I64),
        required: false,
        default: None,
        codec_override: None,
        monotonicity: Monotonicity::None,
        write_scope: None,
    })
}

fn node_type(k: &str) -> SchemaOp {
    SchemaOp::CreateNodeType(NodeType {
        key: key(k),
        display_name: k.to_string(),
        parent_type: None,
        state_model: key("m"),
        attribute_sets: vec![],
        key_type: key("ordinal"),
        state_locks: vec![],
    })
}

fn base() -> Vec<SchemaOp> {
    vec![
        SchemaOp::CreateKeyType(KeyTypeDef {
            key: key("ordinal"),
            density: Density::Dense,
            axis: AxisKind::Ordinal,
        }),
        SchemaOp::CreateStateModel(
            StateModel::new(
                key("m"),
                ModelClass::Endless,
                vec![State {
                    key: key("live"),
                    name: "Live".into(),
                    success_terminal: false,
                    failure_terminal: false,
                }],
                key("live"),
            )
            .unwrap(),
        ),
    ]
}

#[test]
fn ids_follow_first_appearance_not_alphabetical_order() {
    let mut schema = SchemaState::new(Catalog::fresh());
    // Epoch 1: zeta, zebra, z_graph.
    let mut ops = base();
    ops.extend([graph("z_graph"), attr("zebra"), node_type("zeta")]);
    schema.commit(ops).expect("epoch 1");
    // Epoch 2: aaa, alpha, a_graph — every one sorts BEFORE its predecessor.
    schema
        .commit(vec![graph("a_graph"), attr("alpha"), node_type("aaa")])
        .expect("epoch 2");

    assert_eq!(schema.graph_ids()[&key("z_graph")], 0);
    assert_eq!(schema.graph_ids()[&key("a_graph")], 1);
    assert_eq!(schema.attr_id_lineage()[&key("zebra")], AttrId(0));
    assert_eq!(schema.attr_id_lineage()[&key("alpha")], AttrId(1));
    assert_eq!(schema.node_type_codes()[&key("zeta")], 0);
    assert_eq!(schema.node_type_codes()[&key("aaa")], 1);
}

#[test]
fn compiling_with_the_lineage_reproduces_the_live_attribute_ids() {
    let mut schema = SchemaState::new(Catalog::fresh());
    schema.commit(vec![attr("zebra")]).expect("epoch 1");
    schema.commit(vec![attr("alpha")]).expect("epoch 2");

    // A fresh open has only the op history: the ids it rebuilds equal the
    // ones the live process assigned.
    let rebuilt =
        compile_with_lineage(&schema.catalog, Some(&schema.attr_id_lineage())).expect("compiles");
    assert_eq!(rebuilt.attr_ids[&key("zebra")], AttrId(0));
    assert_eq!(rebuilt.attr_ids[&key("alpha")], AttrId(1));
    // Without the lineage the sort order would swap them — which is exactly
    // why the lineage is derived from the op history.
    let naive = compile_with_lineage(&schema.catalog, None).expect("compiles");
    assert_ne!(naive.attr_ids[&key("zebra")], AttrId(0));
}

/// A snapshot of a fresh database carries an EMPTY op-log: replaying it must
/// yield a fresh catalog — not a phantom schema txn with an epoch bump.
#[test]
fn replaying_an_empty_op_log_is_a_fresh_catalog() {
    let (state, outcomes) = lattice_schema::opslog::replay_ops(&[]).expect("empty replay");
    assert!(outcomes.is_empty());
    assert!(state.ops_log.is_empty(), "no txn was committed");
    assert_eq!(state.catalog.schema_epoch, 0);
}

#[test]
fn entities_that_entered_without_an_op_still_get_unique_codes() {
    // A catalog built directly (fixtures, no op history): every entity is
    // still coded — in key order — and no two share a code.
    let mut catalog = Catalog::fresh();
    catalog
        .create_key_type(KeyTypeDef {
            key: key("ordinal"),
            density: Density::Dense,
            axis: AxisKind::Ordinal,
        })
        .unwrap();
    catalog
        .create_state_model(
            StateModel::new(
                key("m"),
                ModelClass::Endless,
                vec![State {
                    key: key("live"),
                    name: "Live".into(),
                    success_terminal: false,
                    failure_terminal: false,
                }],
                key("live"),
            )
            .unwrap(),
        )
        .unwrap();
    let node_type = |k: &str| NodeType {
        key: key(k),
        display_name: k.to_string(),
        parent_type: None,
        state_model: key("m"),
        attribute_sets: vec![],
        key_type: key("ordinal"),
        state_locks: vec![],
    };
    catalog.create_node_type(node_type("zeta")).unwrap();
    catalog.create_node_type(node_type("aaa")).unwrap();

    let schema = SchemaState::new(catalog);
    let codes = schema.node_type_codes();
    assert_eq!(codes.len(), 2, "every type has a code");
    assert_eq!(codes[&key("aaa")], 0, "no op history: key order");
    assert_eq!(codes[&key("zeta")], 1);
    let unique: std::collections::BTreeSet<u16> = codes.values().copied().collect();
    assert_eq!(unique.len(), 2, "codes are unique");
}

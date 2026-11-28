//! T-313 — the durable record layouts (SPEC-03 §6 payloads) as golden bytes.
//!
//! The journal is the authority and its payload layout is a format contract:
//! once a record is on disk, every later version must decode it. These tests
//! pin each layout byte-for-byte and prove the decoder refuses anything that
//! is not exactly one well-formed record.

use lattice_core::value::{Decimal, RecordValue, Value};
use lattice_schema::entities::{ActorClass, Grant, GrantTarget};
use lattice_schema::key::Key;
use lattice_schema::opslog;
use lattice_schema::txn::SchemaOp;
use lattice_storage::journal::RecordKind;
use lattice_tx::records::{Author, Row};
use lattice_tx::{Record, RecordData, RecordError};

fn key(s: &str) -> Key {
    Key::parse(s).unwrap()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn golden(rec: Record, kind: RecordKind, want: &str) {
    assert_eq!(rec.kind(), kind);
    let bytes = rec.encode();
    assert_eq!(hex(&bytes), want, "layout of {kind:?}");
    let back = Record::decode(kind, &bytes).expect("golden bytes decode");
    assert_eq!(back, rec, "decode(encode(x)) == x");
}

#[test]
fn vertex_layout() {
    golden(
        Record::new(RecordData::AddVertex {
            node: 5,
            parent: None,
            graph: 1,
            node_type: key("task"),
            authored: Author::User,
        }),
        RecordKind::AddVertex,
        // node | NO_PARENT | graph | "task" | user
        "05000000ffffffff010004007461736b00",
    );
    golden(
        Record::new(RecordData::AddVertex {
            node: 6,
            parent: Some(5),
            graph: 1,
            node_type: key("task"),
            authored: Author::Engine,
        }),
        RecordKind::AddVertex,
        "0600000005000000010004007461736b01",
    );
    golden(
        Record::new(RecordData::DelVertex { node: 0x1122_3344 }),
        RecordKind::DelVertex,
        "44332211",
    );
}

#[test]
fn edge_layout() {
    golden(
        Record::new(RecordData::AddEdge {
            edge: 9,
            src: 1,
            dst: 2,
            kind: 4,
            aux: 3,
            guard: None,
        }),
        RecordKind::AddEdge,
        // edge | src | dst | kind | aux | no guard
        "09000000000000000100000002000000040300000000",
    );
    golden(
        Record::new(RecordData::AddEdge {
            edge: 9,
            src: 1,
            dst: 2,
            kind: 4,
            aux: 0,
            guard: Some((key("open"), key("auto_done"))),
        }),
        RecordKind::AddEdge,
        // … | guard | "open" | "auto_done"
        "0900000000000000010000000200000004000000000104006f70656e09006175746f5f646f6e65",
    );
    golden(
        Record::new(RecordData::DelEdge { edge: 0x0102 }),
        RecordKind::DelEdge,
        "0201000000000000",
    );
}

#[test]
fn attribute_layout() {
    golden(
        Record::new(RecordData::SetAttr {
            node: 3,
            attr: key("amount"),
            value: Value::I64(25),
        }),
        RecordKind::SetAttr,
        // node | "amount" | value: len 9, tag I64, 25
        "030000000600616d6f756e7409000000021900000000000000",
    );
    golden(
        Record::new(RecordData::AppendCells {
            node: 3,
            attr: key("log"),
            cells: vec![(0, Value::I64(1)), (2, Value::I64(2))],
        }),
        RecordKind::AppendCells,
        // node | "log" | 2 cells | (k=0, I64 1) | (k=2, I64 2)
        "0300000003006c6f6702000000000000000000000009000000020100000000000000020000000000000009000000020200000000000000",
    );
    golden(
        Record::new(RecordData::RowAppend {
            node: 1,
            set: key("t"),
            rows: vec![
                Row {
                    k_skip: 0,
                    cells: vec![(key("a"), Value::I64(1))],
                },
                Row {
                    k_skip: 2,
                    cells: Vec::new(),
                },
            ],
        }),
        RecordKind::RowAppendBatch,
        // node | "t" | 2 rows | (skip 0, 1 cell: "a" I64 1) | (skip 2, no cells)
        "01000000010074020000000000000001000000010061090000000201000000000000000200000000000000",
    );
}

#[test]
fn action_layout() {
    golden(
        Record::new(RecordData::Action {
            node: 4,
            transition: key("start"),
            actor: ActorClass::Human,
            subject: "alice".into(),
            payload: None,
            from: key("open"),
            to: key("running"),
        }),
        RecordKind::Action,
        // node | "start" | human | "alice" | no payload | "open" | "running"
        "04000000050073746172740005000000616c6963650004006f70656e070072756e6e696e67",
    );
    golden(
        Record::new(RecordData::Action {
            node: 4,
            transition: key("start"),
            actor: ActorClass::Timer,
            subject: "timer".into(),
            payload: Some(Value::str("go").unwrap()),
            from: key("open"),
            to: key("running"),
        }),
        RecordKind::Action,
        // … | timer(3) | "timer" | payload: len 7 = tag Str, len 2, "go" | …
        "0400000005007374617274030500000074696d657201070000000502000000676f04006f70656e070072756e6e696e67",
    );
}

#[test]
fn schema_and_grant_layouts_delegate_to_the_op_log() {
    let ops = vec![SchemaOp::CreateRole(lattice_schema::entities::Role {
        key: key("editor"),
        scopes: vec![lattice_schema::entities::Scope::WriteAttribute],
    })];
    let schema = Record::new(RecordData::SchemaTxn { ops: ops.clone() });
    assert_eq!(schema.kind(), RecordKind::SchemaTxn);
    assert_eq!(schema.encode(), opslog::encode_ops(&ops));
    assert_eq!(
        Record::decode(RecordKind::SchemaTxn, &schema.encode()).unwrap(),
        schema
    );

    let grant = Grant {
        role: key("editor"),
        subject: "alice".into(),
        target: GrantTarget::Graph(key("pm")),
    };
    let rec = Record::new(RecordData::Grant {
        grant: grant.clone(),
    });
    assert_eq!(rec.kind(), RecordKind::Grant);
    assert_eq!(
        rec.encode(),
        opslog::encode_ops(&[SchemaOp::GrantOp(grant.clone())])
    );
    assert_eq!(
        Record::decode(RecordKind::Grant, &rec.encode()).unwrap(),
        rec
    );

    let rec = Record::new(RecordData::Revoke {
        role: key("editor"),
        subject: "alice".into(),
        target: GrantTarget::Graph(key("pm")),
    });
    assert_eq!(rec.kind(), RecordKind::Revoke);
    assert_eq!(
        Record::decode(RecordKind::Revoke, &rec.encode()).unwrap(),
        rec
    );
}

#[test]
fn every_value_type_round_trips_inside_a_record() {
    let values = [
        Value::Bool(true),
        Value::I64(-7),
        Value::F64(2.5),
        Value::Decimal(Decimal::new(-3050, 10, 2).unwrap()),
        Value::str("naïve").unwrap(),
        Value::bytes(vec![0, 255, 7]).unwrap(),
        Value::Timestamp(1_700_000_000_000_000),
        Value::Duration(-99),
        Value::NodeRef(lattice_core::ids::NodeId(42)),
        Value::Enum(1),
        Value::Record(
            RecordValue::from_pairs(&[(0, Value::I64(5)), (2, Value::str("note").unwrap())])
                .unwrap(),
        ),
    ];
    for value in values {
        let rec = Record::new(RecordData::SetAttr {
            node: 1,
            attr: key("x"),
            value: value.clone(),
        });
        let bytes = rec.encode();
        let back = Record::decode(RecordKind::SetAttr, &bytes).expect("decodes");
        assert_eq!(back, rec, "{value:?}");
    }
}

#[test]
fn the_decoder_refuses_everything_that_is_not_exactly_one_record() {
    let rec = Record::new(RecordData::AddVertex {
        node: 5,
        parent: None,
        graph: 1,
        node_type: key("task"),
        authored: Author::User,
    });
    let bytes = rec.encode();

    // Truncated: a byte is missing.
    assert_eq!(
        Record::decode(RecordKind::AddVertex, &bytes[..bytes.len() - 1]),
        Err(RecordError::Truncated {
            need: bytes.len(),
            have: bytes.len() - 1
        })
    );
    // Trailing: a byte too many.
    let mut long = bytes.clone();
    long.push(0);
    assert_eq!(
        Record::decode(RecordKind::AddVertex, &long),
        Err(RecordError::Trailing { bytes: 1 })
    );
    // An author tag outside the registry.
    let mut bad_tag = bytes.clone();
    *bad_tag.last_mut().unwrap() = 7;
    assert_eq!(
        Record::decode(RecordKind::AddVertex, &bad_tag),
        Err(RecordError::BadTag {
            field: "author",
            tag: 7
        })
    );
    // A key that breaks the grammar.
    // node | parent | graph = 10 bytes, then the key length + bytes.
    let mut bad_key = rec.encode();
    bad_key.truncate(10);
    bad_key.extend_from_slice(&4u16.to_le_bytes());
    bad_key.extend_from_slice(b"Task");
    assert!(matches!(
        Record::decode(RecordKind::AddVertex, &bad_key),
        Err(RecordError::BadText { .. })
    ));
    // A payload read under the wrong kind.
    assert!(Record::decode(RecordKind::DelVertex, &bytes).is_err());
    // BEGIN/COMMIT are journal frames, never record content.
    assert_eq!(
        Record::decode(RecordKind::BeginTxn, &[]),
        Err(RecordError::BadTag {
            field: "kind",
            tag: RecordKind::BeginTxn as u8
        })
    );
    // A guard flag that is neither 0 nor 1.
    let edge = Record::new(RecordData::AddEdge {
        edge: 1,
        src: 1,
        dst: 2,
        kind: 4,
        aux: 0,
        guard: None,
    })
    .encode();
    let mut bad_guard = edge.clone();
    *bad_guard.last_mut().unwrap() = 5;
    assert_eq!(
        Record::decode(RecordKind::AddEdge, &bad_guard),
        Err(RecordError::BadTag {
            field: "guard",
            tag: 5
        })
    );
    // A GRANT record that carries something else.
    let ops = opslog::encode_ops(&[SchemaOp::CreateRule(lattice_schema::entities::NamedRule {
        key: key("r"),
        form: lattice_schema::entities::PredicateForm::AttrPresent { attr: key("a") },
    })]);
    assert!(matches!(
        Record::decode(RecordKind::Grant, &ops),
        Err(RecordError::BadOps { .. })
    ));
}

//! R6 value/type conformance (SPEC-01 §3–§7): the gate the kernel applies to
//! every cell write. A value must fit its attribute's declared type exactly —
//! decimal `(p, s)` included, since parts store the unscaled value plus the
//! attribute's `(p, s)`.

use lattice_core::ids::NodeId;
use lattice_core::value::{Decimal, RecordValue, Value};
use lattice_schema::catalog::Catalog;
use lattice_schema::entities::*;
use lattice_schema::key::Key;
use lattice_schema::txn::{SchemaOp, SchemaState};
use lattice_validate::Validators;

fn key(s: &str) -> Key {
    Key::parse(s).unwrap()
}

fn attr(k: &str, ty: AttrType) -> AttributeDef {
    AttributeDef {
        key: key(k),
        attr_type: ty,
        required: false,
        default: None,
        codec_override: None,
        monotonicity: Monotonicity::None,
        write_scope: None,
    }
}

struct Types {
    enums: std::collections::BTreeMap<Key, EnumType>,
    records: std::collections::BTreeMap<Key, RecordType>,
}

fn types() -> Types {
    let mut schema = SchemaState::new(Catalog::fresh());
    schema
        .commit(vec![
            SchemaOp::CreateEnum(
                EnumType::new(key("prio"), vec![key("low"), key("high")]).unwrap(),
            ),
            SchemaOp::CreateRecord(
                RecordType::new(
                    key("ev"),
                    vec![
                        (key("f0"), ElemType::Scalar(ScalarType::I64)),
                        (key("f1"), ElemType::Scalar(ScalarType::String)),
                    ],
                )
                .unwrap(),
            ),
        ])
        .expect("types");
    let catalog = schema.catalog;
    Types {
        enums: catalog.enums,
        records: catalog.records,
    }
}

fn conforms(def: &AttributeDef, value: &Value, t: &Types) -> bool {
    Validators::r6_attr_type(def, value, &t.enums, &t.records).is_ok()
}

#[test]
fn every_declared_type_accepts_its_own_values() {
    let t = types();
    let cases = [
        (
            attr("b", AttrType::Scalar(ScalarType::Bool)),
            Value::Bool(false),
        ),
        (attr("i", AttrType::Scalar(ScalarType::I64)), Value::I64(-5)),
        (
            attr("f", AttrType::Scalar(ScalarType::F64)),
            Value::F64(1.5),
        ),
        (
            attr("s", AttrType::Scalar(ScalarType::String)),
            Value::str("x").unwrap(),
        ),
        (
            attr("y", AttrType::Scalar(ScalarType::Bytes)),
            Value::bytes(vec![1u8]).unwrap(),
        ),
        (
            attr("t", AttrType::Scalar(ScalarType::Timestamp)),
            Value::Timestamp(7),
        ),
        (
            attr("d", AttrType::Scalar(ScalarType::Duration)),
            Value::Duration(7),
        ),
        (
            attr("r", AttrType::Scalar(ScalarType::NodeRef)),
            Value::NodeRef(NodeId(3)),
        ),
        (
            attr(
                "m",
                AttrType::Scalar(ScalarType::Decimal {
                    precision: 10,
                    scale: 2,
                }),
            ),
            Value::Decimal(Decimal::new(-12_345, 10, 2).unwrap()),
        ),
        (attr("e", AttrType::Enum(key("prio"))), Value::Enum(1)),
        (
            attr("rec", AttrType::Record(key("ev"))),
            Value::Record(
                RecordValue::from_pairs(&[(0, Value::I64(1)), (1, Value::str("n").unwrap())])
                    .unwrap(),
            ),
        ),
        (
            attr(
                "v",
                AttrType::Vector {
                    elem: ElemType::Scalar(ScalarType::I64),
                },
            ),
            Value::I64(9),
        ),
        (
            attr(
                "ve",
                AttrType::Vector {
                    elem: ElemType::Enum(key("prio")),
                },
            ),
            Value::Enum(0),
        ),
    ];
    for (def, value) in &cases {
        assert!(
            conforms(def, value, &t),
            "{} must accept {value:?}",
            def.key
        );
    }
}

#[test]
fn values_outside_the_declared_type_are_refused_with_the_r6_reason() {
    let t = types();
    let decimal = attr(
        "m",
        AttrType::Scalar(ScalarType::Decimal {
            precision: 10,
            scale: 2,
        }),
    );
    let record = attr("rec", AttrType::Record(key("ev")));
    let cases = [
        (
            attr("s", AttrType::Scalar(ScalarType::String)),
            Value::I64(1),
        ),
        (
            attr("i", AttrType::Scalar(ScalarType::I64)),
            Value::Bool(true),
        ),
        // Decimal (p, s) must match exactly: a different scale would not
        // round-trip through a part, which stores the attribute's (p, s).
        (
            decimal.clone(),
            Value::Decimal(Decimal::new(1, 10, 3).unwrap()),
        ),
        (decimal.clone(), Value::I64(100)),
        // Enum index beyond the declared values.
        (attr("e", AttrType::Enum(key("prio"))), Value::Enum(2)),
        // Record fields: a position outside the declared shape, and a
        // present field of the wrong type.
        (
            record.clone(),
            Value::Record(RecordValue::from_pairs(&[(2, Value::I64(1))]).unwrap()),
        ),
        (
            record.clone(),
            Value::Record(RecordValue::from_pairs(&[(1, Value::I64(1))]).unwrap()),
        ),
        (record, Value::I64(0)),
        (
            attr(
                "v",
                AttrType::Vector {
                    elem: ElemType::Scalar(ScalarType::I64),
                },
            ),
            Value::str("x").unwrap(),
        ),
    ];
    for (def, value) in &cases {
        let err =
            Validators::r6_attr_type(def, value, &t.enums, &t.records).expect_err("must refuse");
        assert_eq!(err.rule, "R6");
        assert!(err.reason.contains("does not conform"), "{}", err.reason);
    }
}

#[test]
fn absence_semantics_a_present_field_must_conform_an_absent_one_is_free() {
    let t = types();
    let record = attr("rec", AttrType::Record(key("ev")));
    // Only f1 present: f0 is absent — legal (SPEC-01 §5).
    let partial =
        Value::Record(RecordValue::from_pairs(&[(1, Value::str("note").unwrap())]).unwrap());
    assert!(conforms(&record, &partial, &t));
    // Present field with the right type, even when another is absent.
    let other = Value::Record(RecordValue::from_pairs(&[(0, Value::I64(4))]).unwrap());
    assert!(conforms(&record, &other, &t));
}

#[test]
fn element_conformance_for_vector_cells() {
    let t = types();
    let i64_elem = ElemType::Scalar(ScalarType::I64);
    assert!(Validators::r6_elem_type(&i64_elem, &Value::I64(3), &t.enums).is_ok());
    let err =
        Validators::r6_elem_type(&i64_elem, &Value::str("no").unwrap(), &t.enums).unwrap_err();
    assert_eq!(err.rule, "R6");
    let enum_elem = ElemType::Enum(key("prio"));
    assert!(Validators::r6_elem_type(&enum_elem, &Value::Enum(1), &t.enums).is_ok());
    assert!(Validators::r6_elem_type(&enum_elem, &Value::Enum(9), &t.enums).is_err());
    assert!(Validators::r6_elem_type(&enum_elem, &Value::I64(1), &t.enums).is_err());
}

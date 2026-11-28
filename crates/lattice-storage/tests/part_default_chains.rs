//! Parts built with the DEFAULT chain of every column type round-trip — the
//! chains the kernel's checkpoint actually selects (SPEC-03 §5: "auto-selected
//! by scalar type").
//!
//! Regression: the LSN column used to be encoded with the VALUE column's
//! codec, so a dictionary / bitpack / Gorilla chain failed with
//! "codec does not apply to column type I64" and no such part could be built.

use lattice_core::ids::{AttrId, GraphId, KeyIndex, Lsn, NodeId};
use lattice_core::value::{Decimal, Value};
use lattice_storage::codecs::{ColumnType, default_chain};
use lattice_storage::part::{Part, PartBuilder, PartCell, PartValueType, merge};

fn typed(ty: ColumnType) -> PartValueType {
    PartValueType::Typed(ty)
}

fn value_for(ty: ColumnType, i: i64) -> Value {
    match ty {
        ColumnType::Bool => Value::Bool(i % 2 == 0),
        ColumnType::I64 => Value::I64(i * 1000 - 7),
        ColumnType::F64 => Value::F64(i as f64 / 8.0),
        ColumnType::Str => Value::str(format!("value-{i}")).unwrap(),
        ColumnType::Bytes => Value::bytes(vec![i as u8, 1, 2]).unwrap(),
        ColumnType::Enum => Value::Enum(i as u8),
        ColumnType::Dec => Value::Decimal(Decimal::new(i128::from(i), 10, 2).unwrap()),
    }
}

#[test]
fn every_default_chain_builds_and_reads_back_its_column() {
    let types = [
        ColumnType::Bool,
        ColumnType::I64,
        ColumnType::F64,
        ColumnType::Str,
        ColumnType::Bytes,
        ColumnType::Enum,
        ColumnType::Dec,
    ];
    for ty in types {
        let mut builder = PartBuilder::new(
            GraphId(1),
            AttrId(3),
            1,
            typed(ty),
            false,
            default_chain(ty),
        );
        if ty == ColumnType::Dec {
            builder.decimal_params(10, 2);
        }
        // Two nodes, several cells each — the LSN column must round-trip too.
        let mut written = Vec::new();
        for node in 1..=2u32 {
            for k in 0..5u64 {
                let cell = PartCell {
                    node: NodeId(node),
                    k: KeyIndex(k),
                    lsn: Lsn(100 + u64::from(node) * 10 + k),
                    value: Some(value_for(ty, (node * 10) as i64 + k as i64)),
                };
                written.push(cell.clone());
                builder.add(cell);
            }
        }
        let image = builder.finish().expect("default chain builds");
        let part = Part::from_bytes(&image.bytes).expect("parses");
        let read = part.all_cells().expect("decodes");
        assert_eq!(read, written, "column type {ty:?}");
    }
}

#[test]
fn default_chain_parts_merge_deterministically() {
    // Two generations of the same three cells; the newer part overwrites.
    let build = |generation: u64, lsn_base: u64, tag: u8| -> Part {
        let mut builder = PartBuilder::new(
            GraphId(1),
            AttrId(1),
            generation,
            typed(ColumnType::Str),
            false,
            default_chain(ColumnType::Str),
        );
        for k in 0..3u64 {
            builder.add(PartCell {
                node: NodeId(1),
                k: KeyIndex(k),
                lsn: Lsn(lsn_base + k),
                value: Some(Value::str(format!("v{k}-{tag}")).unwrap()),
            });
        }
        Part::from_bytes(&builder.finish().unwrap().bytes).unwrap()
    };
    let old = build(1, 10, 0);
    let new = build(2, 50, 1);
    let m1 = merge(&[&old, &new]).expect("merge").into_bytes();
    let m2 = merge(&[&old, &new]).expect("merge").into_bytes();
    assert_eq!(m1, m2, "same inputs ⇒ same output bytes (D3)");
    let merged = Part::from_bytes(&m1).unwrap();
    let cells = merged.all_cells().unwrap();
    assert_eq!(cells.len(), 3, "one cell per (node, k)");
    assert!(
        cells
            .iter()
            .all(|c| matches!(&c.value, Some(Value::Str(s)) if s.ends_with("-1"))),
        "greatest LSN wins: every cell comes from the newer part"
    );
    assert!(cells.iter().all(|c| c.lsn.0 >= 50));
}

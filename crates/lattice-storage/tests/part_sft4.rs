//! T-107 + T-110 acceptance: parts — ordering (SFT-4.1), scalar overwrite
//! resolution via greatest-LSN (SFT-4.2, scalar-as-column k=0), zone-map
//! pushdown with decode counters (SFT-4.3), presence survival through merge
//! (SFT-4.4).

use lattice_core::ids::{AttrId, GraphId, KeyIndex, Lsn, NodeId};
use lattice_core::value::{Decimal, RecordValue, Value};
use lattice_storage::codecs::{CodecChain, ColumnType, IDENTITY};
use lattice_storage::part::{Part, PartBuilder, PartCell, PartValueType, merge};

fn chain() -> CodecChain {
    CodecChain::pure(IDENTITY)
}

fn cell(node: u32, k: u64, lsn: u64, v: Value) -> PartCell {
    PartCell {
        node: NodeId(node),
        k: KeyIndex(k),
        lsn: Lsn(lsn),
        value: Some(v),
    }
}

fn tombstone(node: u32, k: u64, lsn: u64) -> PartCell {
    PartCell {
        node: NodeId(node),
        k: KeyIndex(k),
        lsn: Lsn(lsn),
        value: None,
    }
}

/// Deterministic xorshift for the fuzz test.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
}

/// SFT-4.1 (fuzz slice): randomized appends across 1000 nodes → cells
/// strictly (node_id, k)-sorted; binary-search lookup exact for every cell.
#[test]
fn sft_4_1_ordering_and_exact_lookup() {
    let mut rng = Rng::new(0x5F7_DEAD_BEEF);
    let mut b = PartBuilder::new(
        GraphId(1),
        AttrId(7),
        1,
        PartValueType::Typed(ColumnType::I64),
        false,
        chain(),
    );
    let mut expect: Vec<(u32, u64, i64)> = Vec::new();
    for i in 0..1000u32 {
        let base = rng.next() % 50;
        for j in 0..5u64 {
            let v = (i as i64) * 100 + (j as i64);
            b.add(cell(i, base + j, (i as u64) * 10 + j, Value::I64(v)));
            expect.push((i, base + j, v));
        }
    }
    let image = b.finish().expect("build");
    let part = Part::from_bytes(&image.bytes).expect("parse");
    assert_eq!(part.run_count(), 1000, "one run per node");
    assert_eq!(part.cell_count, 5000);

    // Strict ordering: run nodes ascending; within-run ks dense-ascending.
    let mut last = (0u32, 0u64);
    for r in 0..part.run_count() {
        let node = part.run_node(r);
        assert!(node.0 >= last.0);
        let cells = part.run_cells(r).expect("decode run");
        assert!(
            cells.windows(2).all(|w| w[0].k.0 < w[1].k.0),
            "run ks ascending"
        );
        assert!(cells.windows(2).all(|w| w[0].node.0 <= w[1].node.0));
        last = (node.0, cells.last().expect("non-empty").k.0);
    }

    // Binary-search lookup exact for every cell (including absent keys).
    for (node, k, v) in &expect {
        let hit = part.lookup(NodeId(*node), KeyIndex(*k)).expect("lookup");
        match hit {
            Some(c) => assert_eq!(c.value, Some(Value::I64(*v)), "cell ({node},{k})"),
            None => panic!("cell ({node},{k}) missing"),
        }
    }
    let miss = part.lookup(NodeId(5000), KeyIndex(3)).expect("lookup");
    assert!(miss.is_none(), "unknown key reads as absent");
}

/// SFT-4.2 (scalar-as-column, T-110): three overwrites of one scalar across
/// three parts — read = newest LSN; merge of all three ⇒ exactly one cell.
#[test]
fn sft_4_2_scalar_overwrite_resolution() {
    let mk = |generation: u64, lsn: u64, v: i64| -> Part {
        let mut b = PartBuilder::new(
            GraphId(1),
            AttrId(3),
            generation,
            PartValueType::Typed(ColumnType::I64),
            false,
            chain(),
        );
        // Scalar = column with fixed k=0 (SPEC-03 §9.1).
        b.add(cell(42, 0, lsn, Value::I64(v)));
        let img = b.finish().expect("build");
        Part::from_bytes(&img.bytes).expect("parse")
    };
    let p1 = mk(1, 10, 100);
    let p2 = mk(2, 20, 200);
    let p3 = mk(3, 30, 300);

    // Read resolution across parts: greatest LSN wins.
    let all = [&p1, &p2, &p3];
    let mut best: Option<PartCell> = None;
    for p in &all {
        let hit = p
            .lookup(42.into(), 0.into())
            .expect("lookup")
            .expect("scalar present");
        best = match best {
            Some(b) if b.lsn >= hit.lsn => Some(b),
            _ => Some(hit),
        };
    }
    assert_eq!(
        best.expect("resolved").value,
        Some(Value::I64(300)),
        "newest LSN value"
    );

    // Merge of all three ⇒ exactly one cell, still the newest.
    let merged_img = merge(&all).expect("merge");
    let merged = Part::from_bytes(&merged_img.bytes).expect("parse merged");
    let cells = merged.all_cells().expect("decode merged");
    assert_eq!(cells.len(), 1, "overwrite collapses to one cell");
    assert_eq!(cells[0].lsn, Lsn(30));
    assert_eq!(cells[0].value, Some(Value::I64(300)));
}

/// Zone pushdown (SFT-4.3 slice): runs outside the filter's zone range are
/// never decoded (decode counter proves it); results stay exact.
#[test]
fn zone_pushdown_skips_undecoded_runs() {
    let mut b = PartBuilder::new(
        GraphId(1),
        AttrId(5),
        1,
        PartValueType::Typed(ColumnType::I64),
        false,
        chain(),
    );
    // 100 runs × 50 cells; run r carries values r*1000 + i (i < 50).
    for r in 0..100u32 {
        for i in 0..50u64 {
            b.add(cell(
                r,
                i,
                r as u64 * 100 + i,
                Value::I64((r as i64) * 1000 + i as i64),
            ));
        }
    }
    let img = b.finish().expect("build");
    let part = Part::from_bytes(&img.bytes).expect("parse");
    assert_eq!(part.decoded_runs(), 0, "nothing decoded yet");

    // Filter: values >= 90_000 → only runs 90..=99 can match; the other 90
    // runs are skippable via zone maps without decoding.
    let filter_lo = Value::I64(90_000);
    let filter_hi = Value::I64(i64::MAX);
    let mut matched = Vec::new();
    for r in 0..part.run_count() {
        if !part.run_admits_range(r, &filter_lo, &filter_hi) {
            continue; // zone pushdown: skip without decoding
        }
        for c in part.run_cells(r).expect("decode admitted run") {
            if let Some(Value::I64(v)) = c.value
                && v >= 90_000
            {
                matched.push(v);
            }
        }
    }
    assert_eq!(matched.len(), 10 * 50, "exact results through pushdown");
    assert!(
        part.decoded_runs() <= 12,
        "zone pushdown: ≤ 10 matching runs + slack decoded, got {}",
        part.decoded_runs()
    );
}

/// SFT-4.4: a record cell with an absent field survives a merge exactly —
/// no phantom defaults materialize; presence bitmask byte-identical.
#[test]
fn sft_4_4_presence_survives_merge() {
    let mk = |generation: u64, lsn: u64, with_second: bool| -> Part {
        let mut rec = RecordValue::default();
        rec.set(0, Value::Str("kept".into())).unwrap();
        if with_second {
            rec.set(1, Value::I64(7)).unwrap();
        }
        let mut b = PartBuilder::new(
            GraphId(1),
            AttrId(9),
            generation,
            PartValueType::Record,
            false,
            chain(),
        );
        b.add(cell(1, 0, lsn, Value::Record(rec)));
        let img = b.finish().expect("build");
        Part::from_bytes(&img.bytes).expect("parse")
    };
    let older = mk(1, 10, true); // both fields present
    let newer = mk(2, 20, false); // only field 0 → field 1 reads absent

    let merged_img = merge(&[&older, &newer]).expect("merge");
    let merged = Part::from_bytes(&merged_img.bytes).expect("parse");
    let cells = merged.all_cells().expect("decode");
    assert_eq!(cells.len(), 1);
    match &cells[0].value {
        Some(Value::Record(rec)) => {
            assert_eq!(rec.presence_mask(), 0b01, "field 1 absent after merge");
            assert_eq!(rec.get(0), Some(&Value::Str("kept".into())));
            assert_eq!(rec.get(1), None, "no phantom default for the absent field");
        }
        other => panic!("expected record cell, got {other:?}"),
    }

    // Absence survives a full encode/decode round-trip too (not just merge):
    // rebuild the newer part's image bytes and re-parse them.
    let mut rb = PartBuilder::new(
        GraphId(1),
        AttrId(9),
        2,
        PartValueType::Record,
        false,
        chain(),
    );
    let mut rec = RecordValue::default();
    rec.set(0, Value::Str("kept".into())).unwrap();
    rb.add(cell(1, 0, 20, Value::Record(rec)));
    let reparsed = Part::from_bytes(&rb.finish().expect("build").bytes).expect("parse");
    let cells = reparsed.all_cells().expect("decode");
    match &cells[0].value {
        Some(Value::Record(rec)) => assert_eq!(rec.presence_mask(), 0b01),
        other => panic!("expected record, got {other:?}"),
    }
}

/// Tombstones win by LSN and suppress older cells (SPEC-03 §4).
#[test]
fn tombstone_suppresses_older_cells() {
    let live = {
        let mut b = PartBuilder::new(
            GraphId(1),
            AttrId(2),
            1,
            PartValueType::Typed(ColumnType::I64),
            false,
            chain(),
        );
        b.add(cell(7, 3, 10, Value::I64(1)));
        Part::from_bytes(&b.finish().expect("build").bytes).expect("parse")
    };
    let deleted = {
        let mut b = PartBuilder::new(
            GraphId(1),
            AttrId(2),
            2,
            PartValueType::Typed(ColumnType::I64),
            false,
            chain(),
        );
        b.add(tombstone(7, 3, 20));
        Part::from_bytes(&b.finish().expect("build").bytes).expect("parse")
    };
    let merged_img = merge(&[&live, &deleted]).expect("merge");
    let merged = Part::from_bytes(&merged_img.bytes).expect("parse");
    let cells = merged.all_cells().expect("decode");
    assert_eq!(cells.len(), 1, "tombstone survives as the winner");
    assert_eq!(cells[0].lsn, Lsn(20));
    assert_eq!(cells[0].value, None, "tombstoned cell reads as absent");
}

/// Merge determinism (D3 slice): same inputs ⇒ identical output bytes.
#[test]
fn merge_is_deterministic() {
    let mk = |generation: u64| -> Part {
        let base = (generation - 1) * 1000; // distinct LSN spaces per part
        let mut b = PartBuilder::new(
            GraphId(1),
            AttrId(4),
            generation,
            PartValueType::Typed(ColumnType::I64),
            false,
            chain(),
        );
        for i in 0..10u32 {
            b.add(cell(
                i,
                0,
                base + i as u64 * 3 + 1,
                Value::I64(i as i64 * 10),
            ));
            b.add(cell(
                i,
                1,
                base + i as u64 * 3 + 2,
                Value::I64(i as i64 * 10 + 1),
            ));
        }
        Part::from_bytes(&b.finish().expect("build").bytes).expect("parse")
    };
    let a = mk(1);
    let bpart = mk(2);
    let m1 = merge(&[&a, &bpart]).expect("merge 1");
    let m2 = merge(&[&a, &bpart]).expect("merge 2");
    assert_eq!(m1.bytes, m2.bytes, "D3: same inputs ⇒ same output bytes");
}

/// Decimal parts restore (precision, scale) from the part header.
#[test]
fn decimal_parts_roundtrip_exact() {
    let mut b = PartBuilder::new(
        GraphId(1),
        AttrId(6),
        1,
        PartValueType::Typed(ColumnType::Dec),
        false,
        chain(),
    );
    b.decimal_params(10, 2);
    // SGT-3.1 value: 0.30 exactly.
    b.add(cell(
        1,
        0,
        1,
        Value::Decimal(Decimal::new(30, 10, 2).unwrap()),
    ));
    let img = b.finish().expect("build");
    let part = Part::from_bytes(&img.bytes).expect("parse");
    let cells = part.all_cells().expect("decode");
    assert_eq!(
        cells[0].value,
        Some(Value::Decimal(Decimal::new(30, 10, 2).unwrap())),
        "decimal exactness through the part layer"
    );
}

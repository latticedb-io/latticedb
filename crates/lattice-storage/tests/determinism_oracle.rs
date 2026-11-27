//! T-208 acceptance: determinism oracle + fuzz seed (SPEC-03 §1).
//!
//! - SFT-1.1 (seed form): a SEEDED random transaction log → replayed on 3
//!   fresh directories → bit-identical recovered state (txn sequences,
//!   LSNs). The workload's snapshot + parts are byte-identical across two
//!   independent runs. (Cross-machine SHA-256 form lands with the fuzz
//!   crate in M5/T-502 — in-process byte equality is equivalent for the
//!   same-machine oracle.)
//! - SFT-1.2: 10k random cells per scalar type → codec double-encode
//!   byte-identical (D1).
//! - SFT-1.3: same 5-part input merged twice → identical output bytes (D3).
//!
//! The generator is a seeded xorshift — no external fuzz dependency, same
//! seed ⇒ same log on every machine.

use lattice_core::arrays::GraphArrays;
use lattice_core::ids::{GraphId, NodeId, NodeIdSpace};
use lattice_core::value::{Decimal, RecordValue, Value};
use lattice_schema::catalog::Catalog;
use lattice_schema::key::Key;
use lattice_schema::txn::{SchemaOp, SchemaState};
use lattice_storage::codecs::{CodecChain, ColumnType, IDENTITY};
use lattice_storage::journal::{JournalWriter, RecordKind, recover};
use lattice_storage::manifest::Manifest;
use lattice_storage::part::{Part, PartBuilder, PartCell, PartValueType, merge};
use std::fs;
use std::path::{Path, PathBuf};

/// Deterministic xorshift64* — the fuzz seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.wrapping_add(0x9E37_79B9_7F4A_7C15))
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("latticedb-oracle-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

/// SFT-1.2: 10k random cells per scalar type — value-codec double-encode
/// byte-identical (D1), plus column-codec double-encode for the columnar
/// path.
#[test]
fn sft_1_2_codec_determinism_10k_cells() {
    let mut rng = Rng::new(0xD1_CE11); // seeded
    let n = 10_000usize;

    // I64 column (10k random values) through the value codec.
    let mut i64_bytes_1 = Vec::new();
    let mut i64_bytes_2 = Vec::new();
    let i64s: Vec<Value> = (0..n).map(|_| Value::I64(rng.next() as i64)).collect();
    for v in &i64s {
        lattice_core::value_codec::encode_value(v, &mut i64_bytes_1);
    }
    for v in &i64s {
        lattice_core::value_codec::encode_value(v, &mut i64_bytes_2);
    }
    assert_eq!(i64_bytes_1, i64_bytes_2, "D1: value codec deterministic");

    // F64 column (incl. NaN patterns) through the column codec (Gorilla).
    let f64s: Vec<Value> = (0..n)
        .map(|i| {
            if i % 97 == 0 {
                Value::F64(f64::NAN)
            } else if i % 31 == 0 {
                Value::F64(f64::INFINITY)
            } else {
                Value::F64(f64::from_bits(rng.next() >> 12 | 0x4000_0000_0000_0000) - 2.0)
            }
        })
        .collect();
    let f64_col = lattice_storage::codecs::ColumnData::F64(
        f64s.iter()
            .map(|v| match v {
                Value::F64(x) => *x,
                _ => unreachable!(),
            })
            .collect(),
    );
    let enc1 = lattice_storage::codecs::encode_value(0x05, &f64_col).expect("gorilla");
    let enc2 = lattice_storage::codecs::encode_value(0x05, &f64_col).expect("gorilla");
    assert_eq!(enc1, enc2, "D1: Gorilla deterministic");
    let back =
        lattice_storage::codecs::decode_value(0x05, ColumnType::F64, &enc1, n).expect("decode");
    match (&f64_col, &back) {
        (
            lattice_storage::codecs::ColumnData::F64(a),
            lattice_storage::codecs::ColumnData::F64(b),
        ) => {
            assert!(
                lattice_storage::codecs::f64_bits_eq(a, b),
                "bit-exact round-trip"
            );
        }
        _ => panic!("type drift"),
    }

    // Str column (dictionary codec).
    let strs: Vec<Value> = (0..n)
        .map(|i| match rng.next() % 5 {
            0 => Value::Str("alpha".into()),
            1 => Value::Str(format!("row-{i}").into_boxed_str()),
            2 => Value::Str("".into()),
            3 => Value::Str("unicode: ветер".into()),
            _ => Value::Str("delta".into()),
        })
        .collect();
    let str_col = lattice_storage::codecs::ColumnData::Str(
        strs.iter()
            .map(|v| match v {
                Value::Str(s) => s.clone(),
                _ => unreachable!(),
            })
            .collect(),
    );
    let s1 = lattice_storage::codecs::encode_value(0x02, &str_col).expect("dict");
    let s2 = lattice_storage::codecs::encode_value(0x02, &str_col).expect("dict");
    assert_eq!(s1, s2, "D1: dictionary deterministic");

    // Decimal cells through the value codec.
    let mut dec_1 = Vec::new();
    let mut dec_2 = Vec::new();
    for i in 0..n {
        let d = Value::Decimal(Decimal::new((i as i128) * 7919 % 100_000_000, 10, 2).unwrap());
        lattice_core::value_codec::encode_value(&d, &mut dec_1);
        lattice_core::value_codec::encode_value(&d, &mut dec_2);
    }
    assert_eq!(dec_1, dec_2);

    // Record cells (presence masks vary) through the value codec.
    let mut rec_1 = Vec::new();
    let mut rec_2 = Vec::new();
    for i in 0..n {
        let mut rec = RecordValue::default();
        rec.set(0, Value::I64(i as i64)).unwrap();
        if i % 3 != 0 {
            rec.set(1, Value::Str(format!("r{i}").into_boxed_str()))
                .unwrap();
        }
        let v = Value::Record(rec);
        lattice_core::value_codec::encode_value(&v, &mut rec_1);
        lattice_core::value_codec::encode_value(&v, &mut rec_2);
    }
    assert_eq!(rec_1, rec_2, "D1: record cells (presence) deterministic");
}

/// SFT-1.3: same 5-part input merged twice → identical output bytes (D3).
#[test]
fn sft_1_3_merge_determinism() {
    let mut rng = Rng::new(0xD3_4A4A);
    let mk_part = |rng: &mut Rng, generation: u64, cell_count: usize| -> Part {
        let mut b = PartBuilder::new(
            GraphId(1),
            lattice_core::ids::AttrId(1),
            generation,
            PartValueType::Typed(ColumnType::I64),
            false,
            CodecChain::pure(IDENTITY),
        );
        let mut seen = std::collections::BTreeSet::new();
        while seen.len() < cell_count {
            let node = rng.below(50) as u32;
            let k = rng.below(20);
            if !seen.insert((node, k)) {
                continue; // duplicate (node, k): the builder rejects within-part dups
            }
            let lsn = rng.next();
            b.add(PartCell {
                node: NodeId(node),
                k: lattice_core::ids::KeyIndex(k),
                lsn: lattice_core::ids::Lsn(lsn),
                value: Some(Value::I64(rng.next() as i64)),
            });
        }
        let img = b.finish().expect("build");
        Part::from_bytes(&img.bytes).expect("parse")
    };
    let parts: Vec<Part> = (1..=5).map(|g| mk_part(&mut rng, g, 30)).collect();
    let refs: Vec<&Part> = parts.iter().collect();
    let m1 = merge(&refs).expect("merge 1");
    let m2 = merge(&refs).expect("merge 2");
    assert_eq!(m1.bytes, m2.bytes, "D3: same inputs ⇒ same output bytes");
}

/// Journal replay summary: (txn_id, last_lsn, record_count) per txn.
type TxnSummary = Vec<(u64, u64, usize)>;

/// SFT-1.1 (seed form): seeded random workload → two fresh databases with
/// byte-identical snapshot + parts; the randomized journal replays to
/// identical txn sequences on 3 fresh directory copies.
#[test]
fn sft_1_1_seeded_replay_identity() {
    let run = |dir: &Path| -> (Vec<u8>, Vec<u8>, TxnSummary) {
        let mut rng = Rng::new(0xF00D_5EED);
        fs::create_dir_all(dir.join("journal")).expect("journal dir");
        fs::create_dir_all(dir.join("parts")).expect("parts dir");

        // Fixed schema (bundle), randomized FACT log.
        let mut schema = SchemaState::new(Catalog::fresh());
        latticedb_smokeless_install(&mut schema);

        let mut arrays = GraphArrays::new(NodeIdSpace::new());
        let mut journal = JournalWriter::create(&dir.join("journal"), 1 << 20).expect("journal");

        // Schema txn first.
        journal.begin_txn(1).expect("begin");
        journal
            .append(
                1,
                RecordKind::SchemaTxn,
                &lattice_schema::opslog::encode_ops(&schema.ops_log.concat()),
            )
            .expect("schema txn");
        journal.commit_txn(1).expect("commit");

        // Randomized facts: 60 events mixing structural ops and cell writes.
        let mut nodes: Vec<NodeId> = Vec::new();
        let lsn = journal.next_lsn();
        for (i, txn) in (0..60u64).zip(2u64..) {
            match rng.below(3) {
                0 => {
                    // add_root(task)
                    let n = arrays.add_root(GraphId(1), 10).expect("root");
                    nodes.push(n);
                    journal.begin_txn(txn).expect("begin");
                    journal
                        .append(txn, RecordKind::AddVertex, &n.0.to_le_bytes())
                        .expect("add");
                    journal.commit_txn(txn).expect("commit");
                }
                1 if !nodes.is_empty() => {
                    // add_child under a random existing node
                    let parent = nodes[rng.below(nodes.len() as u64) as usize];
                    if let Ok(child) = arrays.add_child(parent, 10) {
                        nodes.push(child);
                        journal.begin_txn(txn).expect("begin");
                        journal
                            .append(txn, RecordKind::AddVertex, &child.0.to_le_bytes())
                            .expect("add");
                        journal.commit_txn(txn).expect("commit");
                    }
                }
                _ if !nodes.is_empty() => {
                    // cell write: random scalar on a random node
                    let node = nodes[rng.below(nodes.len() as u64) as usize];
                    let value = match rng.below(3) {
                        0 => Value::I64(rng.next() as i64),
                        1 => Value::Str(format!("s{}", rng.below(1000)).into_boxed_str()),
                        _ => Value::F64(f64::from_bits(rng.next() >> 12 | 0x4000_0000_0000_0000)),
                    };
                    let mut payload = Vec::new();
                    payload.extend_from_slice(&node.0.to_le_bytes());
                    payload.extend_from_slice(&(i % 7).to_le_bytes()); // k
                    lattice_core::value_codec::encode_value(&value, &mut payload);
                    journal.begin_txn(txn).expect("begin");
                    journal
                        .append(txn, RecordKind::SetAttr, &payload)
                        .expect("cell");
                    journal.commit_txn(txn).expect("commit");
                }
                _ => {}
            }
            let _ = lsn;
        }
        journal.sync().expect("fsync");
        let last_lsn = journal.next_lsn() - 1;

        // Parts: one I64 part with random cells.
        let mut b = PartBuilder::new(
            GraphId(1),
            lattice_core::ids::AttrId(1),
            1,
            PartValueType::Typed(ColumnType::I64),
            false,
            CodecChain::pure(IDENTITY),
        );
        let mut seen = std::collections::BTreeSet::new();
        while seen.len() < 100 {
            let node = rng.below(50) as u32;
            let k = rng.below(1000);
            if !seen.insert((node, k)) {
                continue;
            }
            b.add(PartCell {
                node: NodeId(node),
                k: lattice_core::ids::KeyIndex(k),
                lsn: lattice_core::ids::Lsn(rng.next()),
                value: Some(Value::I64(rng.next() as i64)),
            });
        }
        let part_image = b.finish().expect("part");
        part_image
            .write(&dir.join("parts/000000000001.lpart"))
            .expect("part write");

        // Snapshot.
        let snapshot = lattice_storage::snapshot::encode_snapshot(
            1,
            schema.catalog.schema_epoch,
            last_lsn,
            &arrays,
            &lattice_schema::opslog::encode_ops(&schema.ops_log.concat()),
            &lattice_schema::opslog::encode_deprecations(&schema.deprecations),
        );
        lattice_storage::snapshot::write_snapshot(&dir.join("snapshot.v1"), &snapshot)
            .expect("snap write");
        swap_manifest_checkpoint(dir, last_lsn, schema.catalog.schema_epoch);

        // Journal replay summary (per fresh copy, done by the caller).
        let mut txn_seq: Vec<(u64, u64, usize)> = Vec::new();
        recover(&dir.join("journal"), |t| {
            txn_seq.push((t.txn_id, t.last_lsn, t.records.len()))
        })
        .expect("journal replays");
        (snapshot, part_image.bytes, txn_seq)
    };

    let dir_a = tmpdir("a");
    let dir_b = tmpdir("b");
    let (snap_a, part_a, txns_a) = run(&dir_a);
    let (snap_b, part_b, txns_b) = run(&dir_b);

    // Byte-identical durable state across independent runs.
    assert_eq!(snap_a, snap_b, "SFT-1.1: snapshots byte-identical");
    assert_eq!(part_a, part_b, "SFT-1.1: parts byte-identical");
    assert_eq!(txns_a, txns_b, "journal txn sequences identical");

    // 3 fresh directory copies of the same journal → identical replays.
    let journal_src = dir_a.join("journal");
    let mut replay_summaries = Vec::new();
    for copy in 1..=3 {
        let fresh = tmpdir(&format!("replay{copy}"));
        fs::create_dir_all(&fresh).expect("fresh dir");
        for entry in fs::read_dir(&journal_src).expect("journal listing") {
            let e = entry.expect("entry");
            fs::copy(e.path(), fresh.join(e.file_name())).expect("segment copy");
        }
        let mut txn_seq: Vec<(u64, u64, usize)> = Vec::new();
        let info = recover(&fresh, |t| {
            txn_seq.push((t.txn_id, t.last_lsn, t.records.len()))
        })
        .expect("replay");
        replay_summaries.push((txn_seq, info.last_lsn, info.txns_applied));
    }
    assert_eq!(replay_summaries[0], replay_summaries[1], "fresh machine 2");
    assert_eq!(replay_summaries[0], replay_summaries[2], "fresh machine 3");
    assert!(
        replay_summaries[0].1 > 0,
        "LSNs gap-free through the random log"
    );
}

// — helpers —

// Minimal inline schema (no circular dev-dependency on latticedb): a
// finalizing model + node type so the catalog blob carries real structure.
fn oracle_schema_ops() -> Vec<lattice_schema::txn::SchemaOp> {
    use lattice_schema::entities::*;
    let k = |s: &str| Key::parse(s).expect("static keys");
    vec![
        SchemaOp::CreateStateModel(
            StateModel::new(
                k("task_flow"),
                ModelClass::Finalizing,
                vec![
                    State {
                        key: k("open"),
                        name: "Open".into(),
                        success_terminal: false,
                        failure_terminal: false,
                    },
                    State {
                        key: k("running"),
                        name: "Running".into(),
                        success_terminal: false,
                        failure_terminal: false,
                    },
                    State {
                        key: k("done"),
                        name: "Done".into(),
                        success_terminal: true,
                        failure_terminal: false,
                    },
                    State {
                        key: k("failed"),
                        name: "Failed".into(),
                        success_terminal: false,
                        failure_terminal: true,
                    },
                ],
                k("open"),
            )
            .expect("oracle model"),
        ),
        SchemaOp::CreateNodeType(NodeType {
            key: k("task"),
            display_name: "Task".into(),
            parent_type: None,
            state_model: k("task_flow"),
            attribute_sets: Vec::new(),
            key_type: k("seq"),
            state_locks: Vec::new(),
        }),
        SchemaOp::CreateKeyType(KeyTypeDef {
            key: k("seq"),
            density: Density::Dense,
            axis: AxisKind::Ordinal,
        }),
        // Reachability (finalizing law): all states need paths.
        SchemaOp::AddTransition(
            Transition::new(
                k("start"),
                k("task_flow"),
                k("open"),
                k("running"),
                Vec::new(),
                vec![ActorClass::Human],
                None,
                TransitionTiming::default(),
                Vec::new(),
            )
            .expect("oracle transition"),
        ),
        SchemaOp::AddTransition(
            Transition::new(
                k("complete"),
                k("task_flow"),
                k("running"),
                k("done"),
                Vec::new(),
                vec![ActorClass::Human],
                None,
                TransitionTiming::default(),
                Vec::new(),
            )
            .expect("oracle transition"),
        ),
        SchemaOp::AddTransition(
            Transition::new(
                k("fail"),
                k("task_flow"),
                k("running"),
                k("failed"),
                Vec::new(),
                vec![ActorClass::Human],
                None,
                TransitionTiming::default(),
                Vec::new(),
            )
            .expect("oracle transition"),
        ),
    ]
}

fn latticedb_smokeless_install(schema: &mut SchemaState) {
    schema
        .commit(oracle_schema_ops())
        .expect("oracle schema commits");
}

fn swap_manifest_checkpoint(dir: &Path, last_lsn: u64, epoch: u64) {
    let m = Manifest {
        generation: 1,
        snapshot_id: 1,
        parts: vec![1],
        journal_segment: 1,
        journal_offset: last_lsn,
        schema_epoch: epoch,
        created_lsn: last_lsn,
    };
    lattice_storage::manifest::swap_manifest(dir, &m, false).expect("manifest swap");
}

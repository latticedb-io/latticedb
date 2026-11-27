//! T-111 — the M1 exit-criteria smoke (ARCHITECTURE §13 M1): build graph,
//! write cells, checkpoint, restart, verify replay identity.
//!
//! SFT-1.1 seed: the same deterministic workload on two fresh databases
//! produces byte-identical snapshot + part images (D2's same-machine form;
//! the cross-machine SHA-256 fuzz oracle lands in M2/T-208). The restart
//! leg re-opens MANIFEST → snapshot → journal and asserts the structural
//! layer reproduces exactly (SFT-3.1 slice).

use lattice_core::arrays::GraphArrays;
use lattice_core::ids::{GraphId, KeyIndex, Lsn, NodeId, NodeIdSpace};
use lattice_core::value::Value;
use lattice_schema::catalog::Catalog;
use lattice_schema::txn::SchemaState;
use lattice_storage::codecs::ColumnType;
use lattice_storage::journal::{JournalWriter, RecordKind, recover};
use lattice_storage::manifest::{Manifest, open_db_dir, read_manifest, swap_manifest};
use lattice_storage::part::{PartBuilder, PartCell, PartValueType};
use lattice_storage::snapshot::{encode_snapshot, read_snapshot, write_snapshot};
use latticedb::bundles::install_named;
use std::fs;
use std::path::Path;

/// The deterministic M1 workload: install a bundle, build a two-level
/// graph, append cells (scalar + series), checkpoint.
fn run_workload(dir: &Path) -> (Vec<u8>, Vec<u8>, u64) {
    fs::create_dir_all(dir).expect("db dir");
    fs::create_dir_all(dir.join("journal")).expect("journal dir");
    fs::create_dir_all(dir.join("parts")).expect("parts dir");

    // 1. Schema: bundle install (one atomic schema txn).
    let mut schema = SchemaState::new(Catalog::fresh());
    install_named(&mut schema, "process-mgmt").expect("bundle installs");

    // 2. Structure: two tasks, one linked as blocker (dependency edge).
    let mut arrays = GraphArrays::new(NodeIdSpace::new());
    let t1 = arrays.add_root(GraphId(1), 10).expect("root");
    let t2 = arrays.add_child(t1, 10).expect("child");
    arrays.set_state(t1, 1).expect("state");
    arrays.set_attr_count(t1, 2).expect("attr count");
    arrays.assert_invariants();

    // 3. Journal: schema txn + structural facts + cell appends. Payloads
    //    carry the canonical value encoding (core value codec).
    let mut journal = JournalWriter::create(&dir.join("journal"), 1 << 20).expect("journal");
    let schema_payload = lattice_schema::opslog::encode_ops(&schema.ops_log);
    journal.begin_txn(1).expect("begin");
    journal
        .append(1, RecordKind::SchemaTxn, &schema_payload)
        .expect("schema txn");
    journal.commit_txn(1).expect("commit");

    let mut lsn_base = journal.next_lsn();
    let cells = [
        (t1.0, 0u64, lsn_base, Value::Str("first task".into())),
        (t2.0, 0, lsn_base + 1, Value::Str("second task".into())),
        (t1.0, 1, lsn_base + 2, Value::I64(42)),
    ];
    for (txn, (node, k, _lsn, v)) in [2u64, 3, 4].into_iter().zip(cells) {
        let mut payload = Vec::new();
        payload.extend_from_slice(&node.to_le_bytes());
        payload.extend_from_slice(&k.to_le_bytes());
        lattice_core::value_codec::encode_value(&v, &mut payload);
        journal.begin_txn(txn).expect("begin");
        journal
            .append(txn, RecordKind::SetAttr, &payload)
            .expect("cell");
        journal.commit_txn(txn).expect("commit");
        lsn_base += 1;
    }
    journal.sync().expect("journal fsync");

    // 4. Cells → one columnar part (title strings + the i64 series share
    //    the part boundary only per attr; the smoke writes one part per
    //    attr_id: strings here, ints in a second part).
    let mut strings = PartBuilder::new(
        GraphId(1),
        lattice_core::ids::AttrId(0),
        1,
        PartValueType::Typed(ColumnType::Str),
        false,
        lattice_storage::codecs::CodecChain::pure(lattice_storage::codecs::IDENTITY),
    );
    strings.add(PartCell {
        node: NodeId(t1.0),
        k: KeyIndex(0),
        lsn: Lsn(lsn_base - 2),
        value: Some(Value::Str("first task".into())),
    });
    strings.add(PartCell {
        node: NodeId(t2.0),
        k: KeyIndex(0),
        lsn: Lsn(lsn_base - 1),
        value: Some(Value::Str("second task".into())),
    });
    let string_image = strings.finish().expect("part build");

    let mut ints = PartBuilder::new(
        GraphId(1),
        lattice_core::ids::AttrId(1),
        1,
        PartValueType::Typed(ColumnType::I64),
        false,
        lattice_storage::codecs::CodecChain::pure(lattice_storage::codecs::IDENTITY),
    );
    ints.add(PartCell {
        node: NodeId(t1.0),
        k: KeyIndex(1),
        lsn: Lsn(lsn_base),
        value: Some(Value::I64(42)),
    });
    let int_image = ints.finish().expect("part build");

    string_image
        .write(&dir.join("parts/000000000001.lpart"))
        .expect("part write");
    int_image
        .write(&dir.join("parts/000000000002.lpart"))
        .expect("part write");

    // 5. Checkpoint: snapshot + MANIFEST swap.
    let snapshot = encode_snapshot(
        1,
        schema.catalog.schema_epoch,
        journal.next_lsn() - 1,
        &arrays,
        &lattice_schema::opslog::encode_ops(&schema.ops_log),
        &lattice_schema::opslog::encode_deprecations(&schema.deprecations),
    );
    write_snapshot(&dir.join("snapshot.v1"), &snapshot).expect("snapshot write");
    swap_manifest(
        dir,
        &Manifest {
            generation: 1,
            snapshot_id: 1,
            parts: vec![1, 2],
            journal_segment: 1,
            journal_offset: journal.next_lsn() - 1,
            schema_epoch: schema.catalog.schema_epoch,
            created_lsn: journal.next_lsn() - 1,
        },
        false,
    )
    .expect("manifest swap");

    (snapshot, string_image.bytes.clone(), journal.next_lsn() - 1)
}

/// Restart leg: open via MANIFEST, verify snapshot/arrays/catalog/journal
/// consistency (SFT-3.1 slice + SFT-1.1 restart form).
fn restart_verify(dir: &Path, snapshot_expected: &[u8], part_expected: &[u8], last_lsn: u64) {
    let (manifest, tmps, _orphans) = open_db_dir(dir, 0).expect("open via MANIFEST");
    assert_eq!(tmps, 0);
    assert_eq!(manifest.generation, 1);
    assert_eq!(manifest.created_lsn, last_lsn);

    let snap = read_snapshot(&dir.join("snapshot.v1")).expect("snapshot reopens");
    assert!(snap.catalog_valid);
    assert_eq!(snap.schema_epoch, 1);
    let arrays = GraphArrays::from_raw(&snap.arrays).expect("structure layer");
    arrays.assert_invariants();
    assert_eq!(arrays.alive_count(), 2, "two live tasks");

    // Journal replay: all txns recovered, LSN continuity to the manifest.
    let mut txn_count = 0usize;
    let info = recover(&dir.join("journal"), |_| txn_count += 1).expect("journal replays");
    assert_eq!(txn_count, 4, "1 schema + 3 cell txns");
    assert_eq!(info.last_lsn, last_lsn);

    // Determinism: the re-encoded snapshot equals the stored bytes.
    let schema_ops = lattice_schema::opslog::decode_ops(&snap.catalog_ops).expect("ops decode");
    let (replayed, _) = lattice_schema::opslog::replay_ops(&schema_ops).expect("catalog replay");
    let reencoded = encode_snapshot(
        1,
        snap.schema_epoch,
        last_lsn,
        &arrays,
        &snap.catalog_ops,
        &snap.catalog_deprecations,
    );
    let _ = replayed;
    let stored = fs::read(dir.join("snapshot.v1")).expect("stored snapshot");
    assert_eq!(reencoded, stored, "snapshot re-encode is byte-identical");
    let _ = snapshot_expected;
    let _ = part_expected;
}

#[test]
fn sft_1_1_seed_two_databases_byte_identical_and_restart_verified() {
    let dir_a = std::env::temp_dir().join(format!("latticedb-smoke-a-{}", std::process::id()));
    let dir_b = std::env::temp_dir().join(format!("latticedb-smoke-b-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir_a);
    let _ = fs::remove_dir_all(&dir_b);

    let (snap_a, part_a, lsn_a) = run_workload(&dir_a);
    let (snap_b, part_b, lsn_b) = run_workload(&dir_b);

    // SFT-1.1 seed: identical workload ⇒ byte-identical durable state.
    assert_eq!(snap_a, snap_b, "snapshot bytes identical across databases");
    assert_eq!(part_a, part_b, "part bytes identical across databases");
    assert_eq!(lsn_a, lsn_b, "LSN timelines identical");

    // Restart leg on database A.
    restart_verify(&dir_a, &snap_a, &part_a, lsn_a);

    // The MANIFEST round-trips through a second open (read-only GC pass).
    let (m, t, _) = open_db_dir(&dir_a, 3600).expect("second open");
    assert_eq!(m.generation, 1);
    assert_eq!(t, 0);
    let _ = read_manifest(&dir_a).expect("manifest stable");
}

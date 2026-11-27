//! The M1 exit-criteria smoke (ARCHITECTURE §13 M1, SFT-1.1 seed) — the
//! same deterministic workload the integration test runs, exposed through
//! the CLI. Two fresh databases receive the identical workload; the run
//! passes only if the durable state (snapshot + parts) is byte-identical
//! across them and the restart leg (MANIFEST → snapshot → journal replay)
//! verifies.

use std::fs;
use std::path::Path;

use lattice_core::arrays::GraphArrays;
use lattice_core::ids::{GraphId, KeyIndex, Lsn, NodeId, NodeIdSpace};
use lattice_core::value::Value;
use lattice_schema::catalog::Catalog;
use lattice_schema::txn::SchemaState;
use lattice_storage::codecs::{CodecChain, ColumnType, IDENTITY};
use lattice_storage::journal::{JournalWriter, RecordKind, recover};
use lattice_storage::manifest::{Manifest, open_db_dir, swap_manifest};
use lattice_storage::part::{PartBuilder, PartCell, PartValueType};
use lattice_storage::snapshot::{encode_snapshot, read_snapshot, write_snapshot};

use crate::bundles::install_named;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmokeSummary {
    pub snapshot_len: usize,
    pub part_len: usize,
    pub last_lsn: u64,
}

fn workload(dir: &Path) -> Result<(Vec<u8>, Vec<u8>, u64), String> {
    fn jstr(e: impl std::fmt::Display) -> String {
        format!("journal: {e}")
    }
    fs::create_dir_all(dir).map_err(|e| format!("db dir: {e}"))?;
    fs::create_dir_all(dir.join("journal")).map_err(|e| format!("journal dir: {e}"))?;
    fs::create_dir_all(dir.join("parts")).map_err(|e| format!("parts dir: {e}"))?;

    let mut schema = SchemaState::new(Catalog::fresh());
    install_named(&mut schema, "process-mgmt").map_err(|e| format!("bundle: {e}"))?;

    let mut arrays = GraphArrays::new(NodeIdSpace::new());
    let t1 = arrays
        .add_root(GraphId(1), 10)
        .map_err(|e| format!("root: {e}"))?;
    let t2 = arrays
        .add_child(t1, 10)
        .map_err(|e| format!("child: {e}"))?;
    arrays.set_state(t1, 1).map_err(|e| format!("state: {e}"))?;
    arrays
        .set_attr_count(t1, 2)
        .map_err(|e| format!("attr count: {e}"))?;
    arrays.assert_invariants();

    let mut journal = JournalWriter::create(&dir.join("journal"), 1 << 20).map_err(jstr)?;
    let schema_payload = lattice_schema::opslog::encode_ops(&schema.ops_log.concat());
    journal.begin_txn(1).map_err(jstr)?;
    journal
        .append(1, RecordKind::SchemaTxn, &schema_payload)
        .map_err(jstr)?;
    journal.commit_txn(1).map_err(jstr)?;

    let mut lsn = journal.next_lsn();
    let cells = [
        (t1.0, 0u64, Value::Str("first task".into())),
        (t2.0, 0, Value::Str("second task".into())),
        (t1.0, 1, Value::I64(42)),
    ];
    for (txn, (node, k, v)) in [2u64, 3, 4].into_iter().zip(cells) {
        let mut payload = Vec::new();
        payload.extend_from_slice(&node.to_le_bytes());
        payload.extend_from_slice(&k.to_le_bytes());
        lattice_core::value_codec::encode_value(&v, &mut payload);
        journal.begin_txn(txn).map_err(jstr)?;
        journal
            .append(txn, RecordKind::SetAttr, &payload)
            .map_err(jstr)?;
        journal.commit_txn(txn).map_err(jstr)?;
        lsn += 1;
    }
    journal.sync().map_err(jstr)?;

    let mut strings = PartBuilder::new(
        GraphId(1),
        lattice_core::ids::AttrId(0),
        1,
        PartValueType::Typed(ColumnType::Str),
        false,
        CodecChain::pure(IDENTITY),
    );
    strings.add(PartCell {
        node: NodeId(t1.0),
        k: KeyIndex(0),
        lsn: Lsn(lsn - 2),
        value: Some(Value::Str("first task".into())),
    });
    strings.add(PartCell {
        node: NodeId(t2.0),
        k: KeyIndex(0),
        lsn: Lsn(lsn - 1),
        value: Some(Value::Str("second task".into())),
    });
    let string_image = strings.finish().map_err(jstr)?;

    let mut ints = PartBuilder::new(
        GraphId(1),
        lattice_core::ids::AttrId(1),
        1,
        PartValueType::Typed(ColumnType::I64),
        false,
        CodecChain::pure(IDENTITY),
    );
    ints.add(PartCell {
        node: NodeId(t1.0),
        k: KeyIndex(1),
        lsn: Lsn(lsn),
        value: Some(Value::I64(42)),
    });
    let int_image = ints.finish().map_err(jstr)?;

    string_image
        .write(&dir.join("parts/000000000001.lpart"))
        .map_err(jstr)?;
    int_image
        .write(&dir.join("parts/000000000002.lpart"))
        .map_err(jstr)?;

    let snapshot = encode_snapshot(
        1,
        schema.catalog.schema_epoch,
        journal.next_lsn() - 1,
        &arrays,
        &lattice_schema::opslog::encode_ops(&schema.ops_log.concat()),
        &lattice_schema::opslog::encode_deprecations(&schema.deprecations),
    );
    write_snapshot(&dir.join("snapshot.v1"), &snapshot).map_err(jstr)?;
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
    .map_err(jstr)?;

    Ok((snapshot, string_image.bytes, journal.next_lsn() - 1))
}

pub fn verify(dir: &Path) -> Result<u64, String> {
    let (manifest, tmps, _) = open_db_dir(dir, 0).map_err(|e| format!("open: {e}"))?;
    if tmps != 0 {
        return Err(format!("unexpected tmp files: {tmps}"));
    }
    let last_lsn = manifest.created_lsn;
    let snap = read_snapshot(&dir.join("snapshot.v1")).map_err(|e| format!("snapshot: {e}"))?;
    if !snap.catalog_valid {
        return Err("catalog blob invalid on a healthy database".into());
    }
    let arrays = GraphArrays::from_raw(&snap.arrays).map_err(|e| format!("structure: {e}"))?;
    arrays.assert_invariants();
    if arrays.alive_count() != 2 {
        return Err(format!(
            "expected 2 live tasks, got {}",
            arrays.alive_count()
        ));
    }
    let mut txn_count = 0usize;
    let info =
        recover(&dir.join("journal"), |_| txn_count += 1).map_err(|e| format!("journal: {e}"))?;
    if txn_count != 4 {
        return Err(format!("expected 4 txns, got {txn_count}"));
    }
    if info.last_lsn != last_lsn {
        return Err(format!(
            "journal last lsn {}: expected {}",
            info.last_lsn, last_lsn
        ));
    }
    let reencoded = encode_snapshot(
        1,
        snap.schema_epoch,
        last_lsn,
        &arrays,
        &snap.catalog_ops,
        &snap.catalog_deprecations,
    );
    let stored = fs::read(dir.join("snapshot.v1")).map_err(|e| format!("stored snapshot: {e}"))?;
    if reencoded != stored {
        return Err("snapshot re-encode diverges from stored bytes".into());
    }
    Ok(last_lsn)
}

/// Run the full smoke: two fresh databases, byte-identity, restart verify.
pub fn run(base: &str) -> Result<SmokeSummary, String> {
    let dir_a = Path::new(base).join("smoke-a");
    let dir_b = Path::new(base).join("smoke-b");
    let _ = fs::remove_dir_all(&dir_a);
    let _ = fs::remove_dir_all(&dir_b);

    let (snap_a, part_a, lsn_a) = workload(&dir_a)?;
    let (snap_b, part_b, lsn_b) = workload(&dir_b)?;
    if snap_a != snap_b {
        return Err("snapshot bytes diverge across identical workloads".into());
    }
    if part_a != part_b {
        return Err("part bytes diverge across identical workloads".into());
    }
    if lsn_a != lsn_b {
        return Err("LSN timelines diverge across identical workloads".into());
    }
    // Restart leg through the public, process-independent entry.
    let verified_lsn = verify(&dir_a)?;
    if verified_lsn != lsn_a {
        return Err(format!(
            "verified lsn {verified_lsn} != workload lsn {lsn_a}"
        ));
    }
    Ok(SmokeSummary {
        snapshot_len: snap_a.len(),
        part_len: part_a.len(),
        last_lsn: lsn_a,
    })
}

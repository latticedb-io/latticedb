//! T-313 storage seams the kernel's sequencing point and checkpoint use:
//!
//! - `write_txn`: one call writes BEGIN + records + COMMIT and hands back
//!   every record's LSN plus the txn range.
//! - `rotate`: a checkpoint starts a new segment; LSNs stay continuous
//!   across segments (SPEC-03 §6).
//! - `gc_snapshots`: superseded snapshots go once past the reader grace.

use lattice_storage::journal::{JournalWriter, RecordKind, TxnLsns};
use lattice_storage::manifest::gc_snapshots;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "latticedb-storage-{name}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// One committed txn: `(first_lsn, last_lsn, data records)`.
type CommittedTxn = (u64, u64, Vec<(RecordKind, Vec<u8>)>);

fn committed(dir: &Path) -> Vec<CommittedTxn> {
    let mut out = Vec::new();
    lattice_storage::journal::recover(&dir.join("journal"), |txn| {
        out.push((
            txn.first_lsn,
            txn.last_lsn,
            txn.records
                .iter()
                .filter(|r| r.kind != RecordKind::Commit)
                .map(|r| (r.kind, r.payload.clone()))
                .collect(),
        ));
    })
    .expect("journal recovers");
    out
}

#[test]
fn write_txn_is_one_call_one_range_and_reports_every_lsn() {
    let dir = test_dir("write_txn");
    let mut journal = JournalWriter::create(&dir.join("journal"), 1 << 20).expect("create");

    let first = journal
        .write_txn(
            1,
            &[
                (RecordKind::AddVertex, b"v"),
                (RecordKind::SetAttr, b"a"),
                (RecordKind::SetAttr, b"b"),
            ],
        )
        .expect("txn 1");
    assert_eq!(
        first,
        TxnLsns {
            first: 1,
            last: 5,
            records: vec![2, 3, 4]
        }
    );
    // An empty txn is still a well-formed BEGIN/COMMIT pair.
    let empty = journal.write_txn(2, &[]).expect("txn 2");
    assert_eq!(
        empty,
        TxnLsns {
            first: 6,
            last: 7,
            records: vec![]
        }
    );
    journal.sync().expect("sync");

    assert_eq!(
        committed(&dir),
        vec![
            (
                1,
                5,
                vec![
                    (RecordKind::AddVertex, b"v".to_vec()),
                    (RecordKind::SetAttr, b"a".to_vec()),
                    (RecordKind::SetAttr, b"b".to_vec())
                ]
            ),
            (6, 7, vec![]),
        ],
        "payloads are opaque and preserved byte-for-byte"
    );
}

#[test]
fn rotate_starts_a_new_segment_with_continuous_lsns() {
    let dir = test_dir("rotate");
    let mut journal = JournalWriter::create(&dir.join("journal"), 1 << 20).expect("create");
    journal
        .write_txn(1, &[(RecordKind::AddVertex, b"a")])
        .expect("txn 1");
    assert_eq!(journal.segment(), 1);
    journal.rotate().expect("rotate");
    assert_eq!(journal.segment(), 2);
    let after = journal
        .write_txn(2, &[(RecordKind::AddVertex, b"b")])
        .expect("txn 2");
    assert_eq!(
        after.first, 4,
        "the LSN sequence did not restart with the segment"
    );
    journal.sync().expect("sync");
    drop(journal);

    let (reopened, info) = JournalWriter::open(&dir.join("journal"), 1 << 20).expect("reopen");
    assert_eq!(info.segments, 2);
    assert_eq!(info.last_lsn, 6);
    assert_eq!(
        reopened.segment(),
        2,
        "appends continue in the newest segment"
    );
    assert_eq!(committed(&dir).len(), 2);
}

#[test]
fn superseded_snapshots_go_once_past_the_reader_grace() {
    let dir = test_dir("gc_snapshots");
    std::fs::create_dir_all(&dir).unwrap();
    let touch = |id: u64, age: Duration| {
        let path = dir.join(format!("snapshot.v{id}"));
        std::fs::write(&path, b"snapshot").unwrap();
        let file = std::fs::File::options().write(true).open(&path).unwrap();
        let when = std::time::SystemTime::now() - age;
        file.set_modified(when).expect("set mtime");
    };
    touch(1, Duration::from_secs(600));
    touch(2, Duration::from_secs(600));
    touch(3, Duration::from_secs(2));
    // Not a snapshot: never touched.
    std::fs::write(dir.join("snapshot.vx"), b"x").unwrap();

    // Grace 10 s: v1 is past it, v3 is not, v2 is the live one.
    assert_eq!(gc_snapshots(&dir, 2, 10).unwrap(), vec![1]);
    assert!(dir.join("snapshot.v2").is_file());
    assert!(dir.join("snapshot.v3").is_file());
    assert!(
        dir.join("snapshot.vx").is_file(),
        "unrelated files are left alone"
    );
    // Grace 1 s: now v3 goes too.
    assert_eq!(gc_snapshots(&dir, 2, 1).unwrap(), vec![3]);
    assert!(
        dir.join("snapshot.v2").is_file(),
        "the live snapshot always stays"
    );
    assert_eq!(
        gc_snapshots(&dir, 2, 0).unwrap(),
        vec![],
        "nothing left to remove"
    );
}

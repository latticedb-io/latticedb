//! T-105 acceptance: journal segments — framing, LSN sequencing, rotation,
//! torn-tail recovery (SPEC-03 §6; SFT-6.1/6.2/6.4; SFT-6.3 lands with the
//! checkpoint horizon in T-109).

use lattice_storage::journal::{JournalError, JournalWriter, RecordKind, RecoveryInfo, recover};
use std::fs;
use std::path::PathBuf;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("latticedb-journal-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn write_txn(w: &mut JournalWriter, txn_id: u64, payloads: &[&[u8]]) -> (u64, u64) {
    w.begin_txn(txn_id).unwrap();
    for (i, p) in payloads.iter().enumerate() {
        let kind = match i % 3 {
            0 => RecordKind::AddVertex,
            1 => RecordKind::SetAttr,
            _ => RecordKind::AppendCells,
        };
        w.append(txn_id, kind, p).unwrap();
    }
    w.commit_txn(txn_id).unwrap()
}

fn collect(dir: &std::path::Path) -> Vec<(u64, u64, usize)> {
    let mut txns = Vec::new();
    recover(dir, |t| txns.push((t.txn_id, t.last_lsn, t.records.len()))).unwrap();
    txns
}

/// Happy path: framing survives a round-trip; txn ranges verified; LSNs
/// gap-free across mixed records (SFT-6.2 stitching precondition).
#[test]
fn framing_roundtrip_and_ranges() {
    let dir = tmpdir("happy");
    {
        let mut w = JournalWriter::create(&dir, 1 << 20).unwrap();
        write_txn(&mut w, 1, &[b"a", b"bb", b"ccc"]);
        write_txn(&mut w, 2, &[b"x"]);
        w.sync().unwrap();
    }
    // txn 1: BEGIN(1) + 3 payloads(2..4) + COMMIT(5) → records vec = 4
    // (payload records + COMMIT; BEGIN lives in the txn header).
    let txns = collect(&dir);
    assert_eq!(txns, vec![(1, 5, 4), (2, 8, 2)]);
    let info = recover(&dir, |_| {}).unwrap();
    assert_eq!(info.last_lsn, 8);
    assert_eq!(info.txns_applied, 2);
    assert_eq!(info.torn_tail_dropped, 0);
}

/// SFT-6.1: kill -9 mid-append analog — garbage/partial tail after a
/// committed txn → open applies only CRC-valid committed txns; the partial
/// tail is dropped; subsequent writes continue gap-free.
#[test]
fn sft_6_1_torn_txn_discarded_and_writes_continue() {
    let dir = tmpdir("torn");
    {
        let mut w = JournalWriter::create(&dir, 1 << 20).unwrap();
        write_txn(&mut w, 1, &[b"committed"]);
        // Torn txn: BEGIN + one record + a HALF frame (no COMMIT).
        w.begin_txn(2).unwrap();
        w.append(2, RecordKind::SetAttr, b"doomed").unwrap();
        w.sync().unwrap();
    }
    // Simulate the crash mid-frame: truncate/poison the tail.
    let seg = dir.join("000000000001.log");
    let meta = fs::metadata(&seg).unwrap();
    let mut data = fs::read(&seg).unwrap();
    data.push(0xAA); // stray byte → partial trailing frame
    data.extend_from_slice(&[1, 2, 3]); // garbage
    fs::write(&seg, &data).unwrap();
    let _ = meta;

    // Recovery: txn 1 applied, txn 2 gone, tail bytes reported.
    let mut seen = Vec::new();
    let info = recover(&dir, |t| seen.push(t.txn_id)).unwrap();
    assert_eq!(seen, vec![1], "torn txn must not apply");
    assert_eq!(info.txns_applied, 1);
    assert!(info.torn_tail_dropped > 0, "tail bytes reported");

    // SFT-6.4: open truncates only the unparseable tail; the torn txn's
    // CRC-valid frames (BEGIN + record, LSNs 4..5) remain in the file with
    // their LSNs burned (append-only, no reuse) — the txn never applies
    // because its COMMIT never landed. New writes continue at 6, gap-free.
    // txn 1 spans [1..3].
    let mut w = JournalWriter::open(&dir, 1 << 20).unwrap().0;
    assert_eq!(w.next_lsn(), 6, "torn txn's valid frames burned LSNs 4..5");
    write_txn(&mut w, 3, &[b"after-crash"]);
    w.sync().unwrap();
    let txns = collect(&dir);
    assert_eq!(
        txns,
        vec![(1, 3, 2), (3, 8, 2)],
        "LSNs continue without gap or reuse"
    );
    assert_eq!(txns[1].1, 8, "txn 3 spans [6..8] (begin+payload+commit)");
}

/// SFT-6.1 corruption variant: a poisoned frame inside a SEALED segment
/// (after rotation it is no longer the active one) → refuse start.
#[test]
fn sealed_segment_corruption_refuses() {
    let dir = tmpdir("sealed");
    {
        // Tiny rotation budget → several segments; first becomes sealed.
        let mut w = JournalWriter::create(&dir, 200).unwrap();
        for i in 1..=5u64 {
            write_txn(
                &mut w,
                i,
                &[b"payload-for-segment-rotation-test-xxxxxxxxxxxx"],
            );
        }
        w.sync().unwrap();
    }
    assert!(
        fs::read_dir(&dir).unwrap().count() >= 3,
        "rotation happened"
    );
    // Poison bytes in the FIRST (sealed) segment.
    let first = dir.join("000000000001.log");
    let mut data = fs::read(&first).unwrap();
    let last = data.len() - 1;
    data[last] ^= 0xFF;
    fs::write(&first, &data).unwrap();

    let result = recover(&dir, |_| {});
    match result {
        Err(JournalError::Corrupt { segment: 1, .. }) => {}
        other => panic!("expected sealed-segment corruption, got: {other:?}"),
    }
}

/// SFT-6.2: rotation at the configured budget; segments stitch seamlessly;
/// LSN continuity verified across segment boundaries.
#[test]
fn sft_6_2_rotation_and_continuity() {
    let dir = tmpdir("rotation");
    {
        let mut w = JournalWriter::create(&dir, 256).unwrap();
        for i in 1..=12u64 {
            write_txn(&mut w, i, &[b"012345678901234567890123456789"]);
        }
        w.sync().unwrap();
    }
    let segs = fs::read_dir(&dir).unwrap().count();
    assert!(segs >= 3, "small budget must rotate; got {segs} segments");

    let mut seen = Vec::new();
    let info = recover(&dir, |t| seen.push(t.txn_id)).unwrap();
    assert_eq!(
        seen,
        (1..=12u64).collect::<Vec<_>>(),
        "all txns across all segments"
    );
    assert_eq!(
        info.last_lsn,
        12 * 3,
        "gap-free across boundaries: 3 records per txn"
    );
    assert_eq!(info.segments, segs as u32);
    assert_eq!(info.highest_segment, segs as u32);
    assert_eq!(info.torn_tail_dropped, 0);
}

/// SFT-6.4 full loop: crash → open (tail dropped) → write → crash → open:
/// LSNs strictly gap-free across restarts; committed prefix never lost.
#[test]
fn sft_6_4_restart_loop_keeps_lsns_gap_free() {
    let dir = tmpdir("restarts");
    let mut next_txn = 1u64;
    for round in 0..3 {
        let (mut w, info) = JournalWriter::open(&dir, 512).unwrap_or_else(|e| {
            if round == 0 && matches!(e, JournalError::Io(_)) {
                // First round: journal absent → create.
                let w = JournalWriter::create(&dir, 512).unwrap();
                return (w, RecoveryInfo::default());
            }
            panic!("open failed: {e}");
        });
        let base = info.last_lsn;
        for i in 0..2u64 {
            let (first, last) =
                write_txn(&mut w, next_txn, &[format!("round-{round}-{i}").as_bytes()]);
            assert_eq!(first, base + i * 3 + 1, "gap-free within round {round}");
            assert_eq!(last, base + i * 3 + 3);
            next_txn += 1;
        }
        w.sync().unwrap();
    }
    let txns = collect(&dir);
    assert_eq!(txns.len(), 6, "every committed txn of every round survived");
    let mut expect_lsn = 1u64;
    for (i, t) in txns.iter().enumerate() {
        assert_eq!(t.0, (i + 1) as u64, "txn ids ordered");
        assert!(t.1 >= expect_lsn);
        expect_lsn = t.1 + 1;
    }
    let info = recover(&dir, |_| {}).unwrap();
    assert_eq!(info.last_lsn, 6 * 3, "6 txns × 3 records, zero gaps");
}

/// Writer misuse is explicit: append without begin, duplicate txn, bad
/// record kinds through the wrong API.
#[test]
fn writer_misuse_is_explicit() {
    let dir = tmpdir("misuse");
    let mut w = JournalWriter::create(&dir, 1 << 20).unwrap();
    match w.append(9, RecordKind::SetAttr, b"x") {
        Err(JournalError::NoOpenTxn { txn_id: 9 }) => {}
        other => panic!("expected NoOpenTxn, got {other:?}"),
    }
    w.begin_txn(9).unwrap();
    match w.begin_txn(9) {
        Err(JournalError::TxnAlreadyOpen { txn_id: 9 }) => {}
        other => panic!("expected TxnAlreadyOpen, got {other:?}"),
    }
    match w.append(9, RecordKind::BeginTxn, b"") {
        Err(JournalError::NoOpenTxn { .. }) => {}
        other => panic!("reserved kinds must go through begin/commit, got {other:?}"),
    }
}

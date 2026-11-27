//! T-109 acceptance: MANIFEST swap atomicity (SFT-2.1), stale-tmp + orphan
//! GC (SFT-2.2), segment retirement with audit retention (SFT-6.3).

use lattice_storage::journal::{JournalWriter, RecordKind, recover};
use lattice_storage::manifest::{
    Manifest, archive_dir_of, open_db_dir, read_manifest, retire_segments, swap_manifest,
};
use std::fs;
use std::path::{Path, PathBuf};

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("latticedb-manifest-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn manifest_with_generation(generation: u64) -> Manifest {
    Manifest {
        generation,
        snapshot_id: generation,
        parts: vec![generation * 10, generation * 10 + 1],
        journal_segment: 1,
        journal_offset: generation * 100,
        schema_epoch: generation,
        created_lsn: generation * 1000,
    }
}

/// SFT-2.1: kill-crash analog between the tmp write and the rename (1000
/// loops) — MANIFEST always parses and sits at generation N or N+1, never
/// torn; the next successful swap recovers cleanly.
#[test]
fn sft_2_1_swap_survives_crash_at_every_point() {
    let dir = tmpdir("swap-crash");
    swap_manifest(&dir, &manifest_with_generation(1), false).expect("initial swap");
    let mut crashes = 0usize;
    for n in 2..=1001u64 {
        let crash = n % 2 == 0; // alternate: crash before rename / clean swap
        swap_manifest(&dir, &manifest_with_generation(n), crash).expect("swap call");
        if crash {
            crashes += 1;
        }
        let m = read_manifest(&dir).expect("MANIFEST must always parse");
        let valid = m.generation == n || m.generation == n - 1;
        assert!(
            valid,
            "generation must be {} or {}, got {}",
            n,
            n - 1,
            m.generation
        );
        assert_eq!(m.parts.len(), 2, "content is never torn");
    }
    assert_eq!(crashes, 500);

    // The final clean swap lands the newest generation.
    swap_manifest(&dir, &manifest_with_generation(1002), false).expect("final swap");
    assert_eq!(read_manifest(&dir).expect("read").generation, 1002);
}

/// SFT-2.2: crash mid-swap leaves `MANIFEST.tmp.*` behind; reopening GCs
/// them, keeps every listed part, and removes orphan parts past the grace
/// window while preserving recent orphans.
#[test]
fn sft_2_2_stale_tmp_and_orphan_gc() {
    let dir = tmpdir("gc");
    let parts_dir = dir.join("parts");
    fs::create_dir_all(&parts_dir).unwrap();

    let live = manifest_with_generation(5);
    swap_manifest(&dir, &live, false).expect("initial swap");

    // Crash mid-swap: tmp files from generations 6 and 7 remain.
    swap_manifest(&dir, &manifest_with_generation(6), true).expect("crash swap 6");
    swap_manifest(&dir, &manifest_with_generation(7), true).expect("crash swap 7");
    assert_eq!(
        fs::read_dir(&dir)
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("MANIFEST.tmp"))
            .count(),
        2,
        "tmp files on disk"
    );

    // Parts: two listed (alive) + two orphans (one old, one fresh).
    for p in &live.parts {
        fs::write(parts_dir.join(format!("{:012}.lpart", p)), b"live").unwrap();
    }
    let orphan_old = parts_dir.join("999999999999.lpart");
    let orphan_fresh = parts_dir.join("888888888888.lpart");
    fs::write(&orphan_old, b"orphan").unwrap();
    fs::write(&orphan_fresh, b"orphan").unwrap();
    // Backdate the old orphan beyond any grace window.
    let old_time = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let _ = filetime_backdate(&orphan_old, old_time);

    let (m, tmps_removed, orphans_removed) = open_db_dir(&dir, 60).expect("open");
    assert_eq!(m, live, "MANIFEST content untouched by GC");
    assert_eq!(tmps_removed, 2, "stale tmp files removed");
    assert_eq!(orphans_removed, 1, "old orphan removed, fresh orphan kept");
    assert!(!orphan_old.exists());
    assert!(
        orphan_fresh.exists(),
        "reader-grace window protects recent orphans"
    );
    for p in &live.parts {
        assert!(
            parts_dir.join(format!("{:012}.lpart", p)).exists(),
            "listed parts alive"
        );
    }
    assert!(
        fs::read_dir(&dir)
            .unwrap()
            .filter(|e| e
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("MANIFEST.tmp"))
            .count()
            == 0,
        "tmp GC leaves no residue"
    );
}

/// SFT-6.3: segments below the checkpoint horizon — audit_retain=true keeps
/// them archived and importable; audit_retain=false deletes them; active
/// segments are untouched in both cases.
#[test]
fn sft_6_3_archive_retention() {
    let dir = tmpdir("retention");
    let journal = dir.join("journal");
    {
        let mut w = JournalWriter::create(&journal, 128).unwrap();
        for i in 1..=6u64 {
            w.begin_txn(i).unwrap();
            w.append(
                i,
                RecordKind::AddVertex,
                format!("segment-filler-{i}-xxxxxxxxxxxxxxxxxxxx").as_bytes(),
            )
            .unwrap();
            w.commit_txn(i).unwrap();
        }
        w.sync().unwrap();
    }
    assert!(
        fs::read_dir(&journal).unwrap().count() >= 4,
        "rotation produced segments"
    );

    // Checkpoint horizon: segments below 4 retire.
    let (archived, deleted) = retire_segments(&journal, 4, true).expect("retire with retain");
    assert!(archived >= 1, "some segments archived");
    assert_eq!(deleted, 0);
    let archive = archive_dir_of(&journal);
    let archived_count = fs::read_dir(&archive).unwrap().count();
    assert_eq!(archived_count as usize, archived);
    // Archived segments remain importable.
    let mut txns = 0usize;
    recover(&archive, |_| txns += 1).expect("archived segments import");
    assert!(txns >= 1, "archived journal replays");
    // Active segments stay in place.
    let active = fs::read_dir(&journal)
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().is_file())
        .count();
    assert!(active >= 1, "horizon segments untouched");

    // Without retention: below-4 retirement deletes.
    let dir2 = tmpdir("retention-nokeep");
    let journal2 = dir2.join("journal");
    {
        let mut w = JournalWriter::create(&journal2, 128).unwrap();
        for i in 1..=6u64 {
            w.begin_txn(i).unwrap();
            w.append(
                i,
                RecordKind::AddVertex,
                format!("filler-{i}-yyyyyyyyyyyyyyyyyyyyyy").as_bytes(),
            )
            .unwrap();
            w.commit_txn(i).unwrap();
        }
        w.sync().unwrap();
    }
    let (archived2, deleted2) = retire_segments(&journal2, 4, false).expect("retire no-keep");
    assert_eq!(archived2, 0);
    assert!(deleted2 >= 1);
    assert!(
        !journal2.join("archive").exists(),
        "no archive dir without retention"
    );
}

fn filetime_backdate(path: &Path, time: std::time::SystemTime) -> std::io::Result<()> {
    let f = std::fs::OpenOptions::new().append(true).open(path)?;
    f.set_modified(time)
}

//! T-111 (CLI leg): manage a database with the REAL binary across separate
//! processes — `--smoke` writes in process one, `--verify` re-opens and
//! verifies in process two; a corrupted database must fail with a non-zero
//! exit. This is the "DB managed by the CLI" gate, not a library call.

use std::process::Command;

fn binary() -> &'static str {
    env!("CARGO_BIN_EXE_latticedb")
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(binary())
        .args(args)
        .output()
        .expect("binary spawns");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

#[test]
fn cli_manages_database_across_processes() {
    let base = std::env::temp_dir().join(format!("latticedb-cli-managed-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let base_str = base.to_str().expect("utf8 temp path");
    let db = base.join("smoke-a");
    let db_str = db.to_str().expect("utf8 temp path");

    // Process 1: create + write + checkpoint (the M1 workload).
    let (code, stdout, stderr) = run(&["--smoke", base_str]);
    assert_eq!(code, 0, "smoke must exit 0; stderr: {stderr}");
    assert!(stdout.contains("smoke OK"), "stdout: {stdout}");
    assert!(db.join("MANIFEST").exists(), "database persisted");

    // Process 2 (independent): re-open the existing database and verify.
    let (code, stdout, stderr) = run(&["--verify", db_str]);
    assert_eq!(code, 0, "verify must exit 0; stderr: {stderr}");
    assert!(stdout.contains("verify OK"), "stdout: {stdout}");
    assert!(
        stdout.contains("last_lsn 12"),
        "workload LSN timeline: {stdout}"
    );

    // Idempotence: verify again — same result, no mutation.
    let (code2, stdout2, _) = run(&["--verify", db_str]);
    assert_eq!(code2, 0);
    assert_eq!(stdout2, stdout, "verify is read-only");

    // Corruption gate: flip one byte in the snapshot → non-zero exit.
    let snap = db.join("snapshot.v1");
    let mut bytes = std::fs::read(&snap).expect("snapshot readable");
    let len = bytes.len();
    bytes[len - 60] ^= 0xFF; // inside catalog/struct region (pre-total)
    std::fs::write(&snap, &bytes).unwrap();
    let (code3, _, stderr3) = run(&["--verify", db_str]);
    assert_ne!(code3, 0, "corrupted database must fail verification");
    assert!(
        stderr3.contains("FAILED"),
        "stderr names the failure: {stderr3}"
    );

    let _ = std::fs::remove_dir_all(&base);
}

use std::fs;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .canonicalize()
        .expect("repo root must resolve from the tests crate")
}

fn workspace_manifest() -> String {
    fs::read_to_string(repo_root().join("Cargo.toml")).expect("workspace Cargo.toml must exist")
}

/// The workspace members must match ARCHITECTURE §3 exactly.
#[test]
fn workspace_layout_matches_architecture_s3() {
    let manifest = workspace_manifest();
    assert!(
        manifest.contains("members = [\"crates/*\", \"cli\", \"tests\"]"),
        "workspace members must cover crates/*, cli, tests"
    );

    for name in [
        "lattice-core",
        "lattice-schema",
        "lattice-storage",
        "lattice-engine",
        "lattice-validate",
        "lattice-tx",
        "lattice-wire",
        "lattice-admin",
        "lattice-pooler",
        "latticedb",
    ] {
        let pkg = repo_root().join("crates").join(name).join("Cargo.toml");
        let text =
            fs::read_to_string(&pkg).unwrap_or_else(|e| panic!("missing manifest {pkg:?}: {e}"));
        assert!(
            text.contains(&format!("name = \"{name}\"")),
            "crates/{name}/Cargo.toml must declare package {name}"
        );
    }

    let cli = fs::read_to_string(repo_root().join("cli").join("Cargo.toml"))
        .expect("cli/Cargo.toml must exist");
    assert!(
        cli.contains("name = \"latticedb-cli\""),
        "cli package must be latticedb-cli"
    );
}

/// Kernel crates must not depend on any async runtime (SPEC-02 §12.8; ARCH §3:
/// blocking kernel APIs only). `lattice-wire`/`lattice-admin` own the runtime
/// and are exempt.
#[test]
fn kernel_crates_carry_no_async_runtime() {
    const KERNEL: [&str; 6] = [
        "lattice-core",
        "lattice-schema",
        "lattice-storage",
        "lattice-engine",
        "lattice-validate",
        "lattice-tx",
    ];
    const BANNED: [&str; 5] = ["tokio", "async-std", "smol", "glommio", "tokio-uring"];

    for name in KERNEL {
        let pkg = repo_root().join("crates").join(name).join("Cargo.toml");
        let text = fs::read_to_string(&pkg).expect("kernel crate manifest must exist");
        for banned in BANNED {
            assert!(
                !text.contains(banned),
                "kernel crate {name} must not reference async runtime {banned}"
            );
        }
    }
}

/// Toolchain file must pin rustfmt + clippy so CI and local runs agree (T-000).
#[test]
fn toolchain_pins_rustfmt_and_clippy() {
    let text = fs::read_to_string(repo_root().join("rust-toolchain.toml"))
        .expect("rust-toolchain.toml must exist");
    assert!(
        text.contains("components = [\"rustfmt\", \"clippy\"]"),
        "rust-toolchain.toml must include rustfmt and clippy components"
    );
}

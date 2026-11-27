# fuzz/

Determinism oracle + replay fuzz (cargo-fuzz targets), seeded at M2 (T-208),
expanded in M5 (T-502). Targets here are excluded from the root workspace;
cargo-fuzz manages this directory as its own workspace.

- SPEC-03 §1 D1–D4: replay identity, codec determinism, merge determinism.
- Entry points: `fuzz_targets/` (created with `cargo fuzz init` at T-208).

# STATE — session handoff (volatile; durable rules live in AGENTS.md)

> Updated: 2026-11-27 · Read this first after any restart, then continue from "Next".

## Where we are

**M0 + M1 COMPLETE** (tag `m1-graph-core`, 2026-11-27). 36 green test targets
across the workspace; fmt/clippy `-D warnings` clean; production code carries
zero unwrap/expect (owner Rust rules, AGENTS.md). Supervisor review deferred
to the V1 gate (owner decision); tasks sit `done (m<N> tag)` in TASKS.md.
Trunk-based: main only; tags per AGENTS.md rule 9. Next milestone: **M2 —
semantics engine** (T-201..T-209: FSM registry, transition dispatch, dependency
algebra, worklist, timer wheel, stall detector, validator suite R1–R13,
determinism oracle seed).

## Done (in order)

1. Contract: MANIFEST v0.3 (16 pillars), ARCHITECTURE.md, SPEC-01…04 — all FROZEN v1.0
   (`docs/specs/`). All micro-decisions owner-confirmed; post-freeze amendments go via
   DEC entries (SPEC-01 §19 DEC-01; SPEC-02 §12.8–12.9; SPEC-03 §9.8; MANIFEST §9/§10).
2. TASKS.md — 47 tasks M0–M5 with spec↔test traceability (SGT/WPT/SFT/MCT). Status
   column added (M0 table first).
3. AGENTS.md (this repo) — binding agent rules. BACKLOG.md live (BL-01 replication seam).
4. Brand/infra settled (see MANIFEST §9 Q12; crates.io claim at M1).
5. 2026-11-27: M1 delivered — T-101 core (ids/arrays/cells + Value::Record +
  core value codec), T-102 catalog+grammar, T-103 compiler (ranks, masks,
  read-sets, action index), T-104 schema txns (R13, txn-level compile gate),
  T-105 journal (CRC32C, LSN, torn tail), T-106 snapshot (scoped CRCs, op-log
  blob), T-107+T-110 parts (zone maps, presence, LSN merge), T-108 codecs
  (Gorilla/delta-of-delta/varints; zstd feature-gated), T-109 MANIFEST+GC+retention,
  T-112 bundles (3 v0, idempotent), T-111 CLI smoke (`--smoke <dir>`, byte-identity
  + restart verify). Tags: `t000-skeleton`, `m0-bootstrap`, `m1-graph-core`.
  Earlier: git repo live, pushed to `github.com/latticedb-io/latticedb` (SSH remote);
   README.md; four pre-implementation DECs researched + frozen; **T-000** — workspace
   skeleton (12 packages per ARCH §3: 10 crates + latticedb-cli + latticedb-tests),
   CI workflow (fmt/clippy/test, GitHub Actions), rust-toolchain.toml (stable +
   rustfmt/clippy), rustfmt.toml (edition 2024), .gitignore, fuzz/README (M2 seeding),
   workspace lints (clippy all=warn), Cargo.lock committed. Skeleton invariants tested
   (tests/tests/skeleton.rs: layout per ARCH §3, no-async-runtime law, toolchain pin).
   Local acceptance: fmt/clippy -D warnings/test all green (23 ok targets).

## Environment facts

- Repo: `/home/sergey/dev/latticedb`, git + remote `origin` = SSH
  `git@github.com:latticedb-io/latticedb.git`. Toolchain: rustup 1.29.3, stable
  1.99.1 (edition 2024 supported).
- **GitHub Actions availability on org `latticedb-io` NOT yet confirmed** — verify the
  first CI run after push; if Actions are disabled, owner must enable them.
- LICENSE decided 2026-11-27: **Apache-2.0** (`LICENSE` file, copyright Emergenta;
  `workspace.package.license = "Apache-2.0"`). crates.io claim unblocked for M1.
- Roles: owner + lead agent = architect pair; lead agent = engineer + QA;
  **Supervisor** = separate independent agent sessions, review-only, gates DoD.

## Next (in order)

1. **T-001 done** (2026-11-27, review): config tree (6 groups, 40 parameters,
   deny-unknown-fields + key/section error annotation), safety clamps (startup errors,
   never silent), 4 profiles (dev/laptop/server/analytics, embedded TOML, v1),
   `plan_reload` hot/restart classes (8 hot / 32 restart, completeness-tested),
   autodetect (HostSpecs → deterministic TOML; Host probes CPU/RAM, Hdd conservative
   until M4 disk probing). Tests: `crates/latticedb/tests/config_wpt10.rs` — 6 fns
   covering WPT-10.3 (legal extremes round-trip, config level), 10.4 (byte-identical
   autodetect), 10.5 (strict validation naming key+section), 10.6 (hot/restart
   classes + 40-param anti-drift completeness). WPT-10.3/10.6 wire-level variants
   land in M3 (noted in test docs).
2. **T-101** (M1 chain start): `lattice-core` — identifiers, epochs, node-id spaces,
   SoA/CSR arrays, cell types (scalars, records + presence bitmask). Acceptance: cell
   round-trip tests; array invariants (SPEC-01 §3/§5, ARCH §4). Config consumption
   wiring (arena budgets etc.) follows per-task.
3. M1 continues per TASKS.md (T-102…T-112). Tag `m0-bootstrap` remains planned at the
   V1-gate sign-off (owner deferred Supervisor to end of V1; do not tag milestones
   early).

## DoD reminder (per AGENTS.md)

spec refs cited → acceptance tests green → Supervisor sign-off → TASKS.md updated →
no unplanned features. Every test is functional (data/state assertions), never "exit 0".

# STATE — session handoff (volatile; durable rules live in AGENTS.md)

> Updated: 2026-11-27 · Read this first after any restart, then continue from "Next".

## Where we are

Contract frozen AND all pre-implementation architecture decisions closed (2026-11-27):
execution model (thread pools, one process, no edge IPC), wire runtime (tokio in
wire/admin only — SPEC-02 §12.8), allocator (mimalloc — §12.9), journal I/O boundary
(fdatasync at group commit — SPEC-03 §9.8), Q10 auth v1 confirmed (MANIFEST §9).
**T-000 implemented, local acceptance green, awaiting Supervisor review; T-001 next.**

## Done (in order)

1. Contract: MANIFEST v0.3 (16 pillars), ARCHITECTURE.md, SPEC-01…04 — all FROZEN v1.0
   (`docs/specs/`). All micro-decisions owner-confirmed; post-freeze amendments go via
   DEC entries (SPEC-01 §19 DEC-01; SPEC-02 §12.8–12.9; SPEC-03 §9.8; MANIFEST §9/§10).
2. TASKS.md — 47 tasks M0–M5 with spec↔test traceability (SGT/WPT/SFT/MCT). Status
   column added (M0 table first).
3. AGENTS.md (this repo) — binding agent rules. BACKLOG.md live (BL-01 replication seam).
4. Brand/infra settled (see MANIFEST §9 Q12; crates.io claim at M1).
5. 2026-11-27: git repo live, pushed to `github.com/latticedb-io/latticedb` (SSH remote);
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
- LICENSE still undecided (owner). Needed before crates.io claim (M1). Workspace
  `license` field intentionally unset.
- Roles: owner + lead agent = architect pair; lead agent = engineer + QA;
  **Supervisor** = separate independent agent sessions, review-only, gates DoD.

## Next (in order)

1. **Supervisor review of T-000** (status: review) → then T-000 done.
2. **T-001**: config subsystem — TOML (serde+toml crate), strict validation
   (deny_unknown_fields), profiles dev/laptop/server/analytics, autodetect skeleton,
   `resources.allocator.*` mapping (SPEC-02 §12.9), policy-table file format shared
   (SPEC-02 §10, §12.4). Acceptance: WPT-10.3–10.6 as real functional tests.
3. Then M1 chain per TASKS.md (T-101…T-112), tasks land in `review` for Supervisor.

## DoD reminder (per AGENTS.md)

spec refs cited → acceptance tests green → Supervisor sign-off → TASKS.md updated →
no unplanned features. Every test is functional (data/state assertions), never "exit 0".

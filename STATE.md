# STATE — session handoff (volatile; durable rules live in AGENTS.md)

> Updated: 2026-11-26 · Read this first after any restart, then continue from "Next".

## Where we are

The **contract is complete and frozen**. No specs are floating. Development has NOT
started yet — the next action is **T-000 kickoff**.

## Done (in order)

1. Contract: MANIFEST v0.3 (16 pillars), ARCHITECTURE.md, SPEC-01…04 — all FROZEN v1.0
   (`docs/specs/`). All micro-decisions owner-confirmed; post-freeze amendments go via
   DEC entries (example: SPEC-01 §19 DEC-01, attribute-set finalization).
2. TASKS.md — 47 tasks M0–M5 with spec↔test traceability (SGT/WPT/SFT/MCT).
3. AGENTS.md (this repo) — binding agent rules: roles, strict testing law, DoD, scope
   guard. BACKLOG.md stub created.
4. Brand/infra settled: company **Emergenta**, product **LatticeDB**, repo codename
   `uvdb`; domains emergenta.io/.dev (canonical .dev) + latticedb.io registered; GitHub
   org **latticedb-io** exists; images → GHCR only (Docker Hub deferred: paid, blocked
   from Russia); crates.io `latticedb` free (claim at M1), PyPI `latticedb` TAKEN
   (nearest conceptual neighbor — inspect later, SDK will be `emergenta-latticedb`).

## Environment facts

- Repo folder renamed: now `/home/sergey/dev/latticedb` (was uvdb). NOT a git repo yet.
- Host: Linux, zsh; opencode sessions run from the repo folder.
- Roles: owner + lead agent = architect pair; lead agent = engineer + QA;
  **Supervisor** = separate independent agent sessions, review-only, gates DoD.

## Next (in order)

1. **T-000**: `git init` in `/home/sergey/dev/latticedb`, first commit (all contract
   docs), GitHub remote `github.com/latticedb-io/latticedb`, CI skeleton
   (fmt/clippy/test), workspace skeleton per ARCHITECTURE §3, rust-toolchain file.
   CI is GitHub Actions — confirm Actions availability on the org before promising status checks.
2. **T-001**: config subsystem (TOML, strict validation, profiles, autodetect skeleton;
   WPT-10.3–10.6).
3. Then M1 chain per TASKS.md (T-101…T-112), engineer+QA = lead agent, tasks land in
   `review` for Supervisor sign-off.

## DoD reminder (per AGENTS.md)

spec refs cited → acceptance tests green → Supervisor sign-off → TASKS.md updated →
no unplanned features. Every test is functional (data/state assertions), never "exit 0".

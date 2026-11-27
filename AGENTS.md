# AGENTS.md — LatticeDB project rules (binding for every agent working in this repo)

## Project

**LatticeDB** (by Emergenta) — multi-graph, schema-typed, stateful graph database with
scalar + vector attributes and deterministic derived state. Repo codename: `uvdb`.

## Contract (frozen, change only via decision entries)

1. `MANIFEST.md` — v0.3, constitution: 16 pillars, non-goals, decision log.
2. `ARCHITECTURE.md` — components, crate layout, data flows, concurrency, security.
3. `docs/specs/SPEC-01-schema-graph.md` — FROZEN v1.0.
4. `docs/specs/SPEC-02-wire-protocol.md` — FROZEN v1.0.
5. `docs/specs/SPEC-03-storage-format.md` — FROZEN v1.0.
6. `docs/specs/SPEC-04-mcp-surface.md` — FROZEN v1.0.

Post-freeze spec changes REQUIRE a decision entry (`DEC-x` in the owning spec's
decision log or MANIFEST §10) approved by the architect pair. **No silent spec drift.**

## Roles

- **Architect pair**: repo owner + lead agent. Spec/manifest changes, DEC entries,
  architecture judgments.
- **Engineer + QA** (lead agent): implements tasks, writes and runs all tests.
- **Supervisor** (independent agent, separate sessions): reviews completed tasks against
  spec references and acceptance criteria; signs off or returns with findings. The
  Supervisor never implements and never shares session context with the engineer.

## Working rules

1. **Spec-locked development.** Take tasks only from `TASKS.md`. Every task cites spec
   sections + acceptance test IDs. Do not implement anything absent from TASKS.md —
   propose a TASKS/BACKLOG entry to the architects instead.
2. **Testing law (strict).** Every test — unit included — must be a real functional test:
   assert data consistency, mutation, state outcomes, replay identity. "Process exited 0"
   is not a passing test. Spec Test Definition tables are binding: purpose, principal
   target, expected outcome, level.
3. **Definition of Done:** spec refs cited → acceptance tests green → Supervisor
   sign-off → TASKS.md status updated → no unplanned features.
4. **Scope guard.** New feature ideas go to `BACKLOG.md` with rationale. V1 scope is
   frozen: M1–M4 (+ M5 hardening under Supervisor gates).
5. **Pillars are law.** MANIFEST P1–P16 apply to every change: no hardcoded parameters
   (SPEC-02 §10), no state-write APIs (P2), no schema inline definitions (P3), journal is
   the only authority (P7), timer wheel is the only clock (P8), compactness (P11).
6. **Determinism is sacred.** Any change that could affect replay output MUST be covered
   by the determinism oracle (SPEC-03 §1 D1–D4, SFT-1.1) before merge.
7. **Status ledger.** Update TASKS.md statuses in real time; keep notes terse. Volatile
   session state goes to `STATE.md` (create on first use), durable rules here.
8. **Language.** Code, docs, comments, commit messages: English. Commit style: concise
   imperative, reference task ID (`T-107: part run directory + zone maps`).
9. **Tag discipline (owner-mandated).** Every milestone completion gets an annotated
   tag `m<N>-<slug>` (e.g. `m0-bootstrap`, `m1-graph-core`) pushed to origin as part of
   that milestone's Definition of Done; task-level checkpoints may use `t<NNN>-<slug>`
   (e.g. `t000-skeleton`). Tags are never skipped or retro-batched — the Supervisor uses
   them as review ranges (`git diff <prev-tag>..<tag>`).

## Rust rules (owner-mandated)

1. **Zero `unwrap()` / `expect()` outside tests.** Errors are values: every
   fallible operation returns `Result` with its module's error type.
2. **Error structs per trait/module.** Every trait declares a dedicated
   error type (`type Err` / `*Error` struct) and every fallible method is
   bound to it; every module exposes its own typed error enum implementing
   `Display + std::error::Error`; composition goes through `From`.
3. **Explicit exception processing.** No silent fallbacks: errors are
   propagated (`?`), mapped with context, or asserted as documented
   invariants (`assert!`/`debug_assert!`) — never swallowed, never defaulted.

## Non-goals (do not build)

No SQL, no query planner, no general OLAP engine, no JSON type, no document store, no
state-write API, no arbitrary user code, no wall-clock predicates, no ANN/BM25 in core
v1, no multi-master distribution, no manual state management. Full list: MANIFEST §4.

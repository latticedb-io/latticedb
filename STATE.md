# STATE — session handoff (volatile; durable rules live in AGENTS.md)

> Updated: 2026-11-28 · Read this first after any restart, then continue from "Next".

## Where we are

M0–M2 tagged. M3 in progress under the re-cut order (MANIFEST §10 "Sequence re-cut",
2026-11-28): core first, then listeners, external surfaces last.

**T-313 kernel assembly DELIVERED → `review` (Supervisor, deferred to V1 gate).** The
database now runs end to end in library mode: create → write (structure, attributes incl.
row append, actions with spawn, schema txns, grants) → explicit txns → checkpoint → close →
reopen with `live = replay`. Whole workspace: **259 tests green, `cargo fmt --check`
clean, `cargo clippy --workspace --all-targets -- -D warnings` clean.**

Everything is UNCOMMITTED in the latticedb working tree (docs + code) — commit pending
owner. Suggested split: (1) `DEC: sequence re-cut — core + CLI first, external surfaces
last` for MANIFEST/ARCHITECTURE/TASKS/BACKLOG/README/AGENTS/STATE; (2) `T-313: kernel
assembly — durable write path, recovery, explicit txns, sequencer` for the code.

## Decided this session (2026-11-28)

- **T-311 toolchain (owner):** Gradle Kotlin DSL · JDK 21 LTS · coroutines-first ·
  JUnit 5 + kotlin-test. Valid whenever the SDK (now M4) is scheduled.
- **Working rule (owner):** re-read MANIFEST / ARCHITECTURE / specs before every task
  and refine the live docs → AGENTS.md rule 10.
- **Sequence (owner):** workable DB core + full CLI first → listeners → external
  surfaces last (SDK, MCP, gRPC admin, REST/WS, SPA, pooler).

## Pending owner decisions (recorded in BACKLOG "Spec questions")

- **SQ-04** control-frame wire contract — required before T-319 (listeners).
- **SQ-05** PROXY v2 length byte order — before T-319 / T-402.
- **SQ-06** pooler implementation language — before T-405.
- **SQ-07** Host → Database (P6): V1 task vs declared deferral.
- **SQ-09** logical time has no journal record — blocks the timer path (timer-fired
  actions, lag/deadline recovery) and journal-authored graph timestamps.
- **SQ-10** no wire code for a state-locked attribute write (mapped to `FORBIDDEN`).

## Next (in order = TASKS.md M3 ID order)

1. **T-314 kernel read path** — NOT started. Rule-10 re-read before starting: ARCH §5.2,
   SPEC-02 §8, SPEC-01 §6/§12, TASKS.md T-314 row (WPT-8.1–8.4 kernel forms, SGT-6.2).
   Build on `lattice_tx::Kernel::read_state(session)` / `EngineState`; note the kernel
   keeps ALL cells in memory (`Machine::cells`) — "values from parts" applies to restore,
   so cursors can read the in-memory store first and prove parity with parts.
   Watch out: `edge_count()` counts LIVE edges, `edge_slots()` is the id space.
2. T-315 secondary indices → T-316 full CLI → T-317 / T-318 kernel semantics completion
   → T-319 listeners → T-320 WPT socket-level pass.

## T-313 notes (for whoever reviews or continues)

**Law of the assembly:** one mutation primitive. `EngineState::plan(command) → records`
validates read-only; `EngineState::apply(record, lsn)` serves BOTH live execution and
journal replay. Write path in `Kernel::commit_unit`: plan → journal (`BEGIN` + records +
`COMMIT` in one `JournalWriter::write_txn`) → apply each record with its own LSN →
quiesce → publish events → sync. Any failure after the journal write poisons the kernel.

**Explicit txn (SPEC-02 §5):** `begin_txn(session)` clones the engine state into a
scratch; the session's commands are planned+applied on the scratch and buffered; a
failing command aborts the txn (WPT-5.2); `commit_txn` journals + applies the buffered
records on live, refused `ORDER_CONFLICT` if live moved meanwhile. One per database (ED-25).

**Record payloads:** `lattice_tx::records` (golden-tested in `records_golden.rs`);
types/attrs/transitions/graphs by KEY; engine-authored spawn vertices follow the ACTION
as `ADD_VERTEX{author=engine}`.

**Where things live:** `crates/lattice-tx/src/{error,records,command,state,kernel,handle,lock}.rs`.
The legacy in-memory fact path (`gate.rs`, `sequencer.rs`, `writer.rs`) is deleted.
Tests: `crates/lattice-tx/tests/{kernel_write_path,kernel_recovery,records_golden,
parallel_confluence}.rs` + `tests/common/mod.rs` (fixture catalog, `digest(state)`).
Seam tests: engine `kernel_seams.rs`, validate `type_conformance.rs`, schema `lineage.rs`,
storage `storage_seams.rs` + `part_default_chains.rs`, core `value_codec.rs` unit tests.

**Defects fixed in earlier crates (details in BACKLOG):** ED-14 lineage of ids/codes,
ED-21 core value codec, ED-22 part LSN column, ED-23 empty op-log replay, ED-24 lock
table poison handling.

**Known gaps, decided not absorbed:** timer path (SQ-09); guard params rebuilt from the
journal at open because the snapshot format cannot hold them (ED-15); journal horizon
never advances (ED-16); wire command layer still placeholder-grade until T-319 (ED-18).

## Environment facts

- Repo `/home/sergey/dev/latticedb`, remote `origin` = SSH
  `git@github.com:latticedb-io/latticedb.git`; `main` == `origin/main` at `0a23bb5`
  (everything after is uncommitted).
- `~/dev/sdk-kotlin` does not exist; `gh` is logged out (the owner creates remotes).
- JDK 21 + 25 installed; Gradle 9.9.2 cached in `~/.gradle/wrapper/dists`; no
  `gradle`/`mvn`/`kotlinc` on PATH.
- Roles: owner + lead agent = architect pair; lead agent = engineer + QA; Supervisor =
  separate sessions, review-only (deferred to the V1 gate so far).

## DoD reminder (per AGENTS.md)

spec refs cited → acceptance tests green → Supervisor sign-off → TASKS.md updated →
no unplanned features. Every test is functional (data/state assertions), never "exit 0".

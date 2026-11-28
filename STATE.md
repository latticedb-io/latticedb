# STATE — session handoff (volatile; durable rules live in AGENTS.md)

> Updated: 2026-11-28 · Read this first after any restart, then continue from "Next".

## Where we are

M0–M2 tagged. M3 in progress under the re-cut order (MANIFEST §10 "Sequence re-cut",
2026-11-28): core first, then listeners, external surfaces last.

- **T-313 kernel assembly — `review`** (committed `04342fd`, pushed).
- **T-314 kernel read path — `review`** (committed and pushed after T-313; see
  `git log`). The database now reads as well as it writes in library mode:
  snapshot / time travel / node read / cursors over parts ⊕ staging / blockers /
  history. Whole workspace: **271 tests green, `cargo fmt --check` clean,
  `cargo clippy --workspace --all-targets -- -D warnings` clean.**

Both tasks wait for the Supervisor (deferred to the V1 gate). Working tree clean.

**Model re-read (owner-mandated, 2026-11-28):** `docs/MODEL.md` is required reading
(AGENTS rules 10, 11: a name is not a definition; [S]/[D]/[O] labels; guess audit §11).
Fixed two name-based guesses (ED-29 row-append key axis, ED-30 `shared_key`). Second
pass after the owner's review: a node is NOT a table — relational words are an analogy
for `k`; a scalar is one value stored internally as the k = 0 cell. Full gap audit M1–M5:
39 open decisions queued in BACKLOG (TASKS.md "Gap audit").

## Decided this session (2026-11-28)

- **T-311 toolchain (owner):** Gradle Kotlin DSL · JDK 21 LTS · coroutines-first ·
  JUnit 5 + kotlin-test. Valid whenever the SDK (now M4) is scheduled.
- **Working rule (owner):** re-read MANIFEST / ARCHITECTURE / specs before every task
  and refine the live docs → AGENTS.md rule 10.
- **Sequence (owner):** workable DB core + full CLI first → listeners → external
  surfaces last (SDK, MCP, gRPC admin, REST/WS, SPA, pooler).

## Pending owner decisions — the decision queue

**Every open decision is queued in BACKLOG.md** ("Spec questions" → the table at the
top): SQ-01…SQ-43, ordered by the milestone each blocks, recommendation per row. The
owner is working through the queue so that tasks never stop for a decision. When a row
is decided: record the decision in its SQ entry, turn it into a DEC / task row if it
changes the contract, and strike it from the queue.

Defects that need no decision (fix next, before T-315): SQ-23 journal frame cap not
enforced on write; SQ-24 `DropGraph` with live nodes; SQ-17 record field position.

## Next (in order = TASKS.md M3 ID order)

1. **T-315 secondary indices + key-aligned cursor — NOT started.** Rule-10 re-read first:
   `docs/MODEL.md` (§3 node-as-a-table, §5 read model), SPEC-01 §6/§12, ARCH §5.2,
   SPEC-02 §8, TASKS.md T-315 row. Two halves: (a) IndexDef views (by_state,
   attr_presence, set_presence) behind `lattice_tx::read::select_nodes` — today's
   selectors are scans; prove "index == scan" and "drop + rebuild == identical"
   (SGT-12.1/12.2); (b) the `shared_key` key-aligned cursor — vectors sharing a node key
   aligned on `(node_id, k)` and returned as records (kernel `Selector` has no SharedKey;
   T-314's single-column cursor is its one-column case). **Do NOT start (b) before the
   owner confirms SQ-13's parameters.**
2. T-316 full CLI → T-317 / T-318 kernel semantics completion → T-319 listeners →
   T-320 WPT socket-level pass.

## T-314 notes (for whoever reviews or continues)

- **Model:** reads run on the sequencer thread between committed units (consistent by
  construction, not lock-free — ED-26). Cursors read parts ⊕ staging; pushdown by
  liveness / selection / run directory / zone maps; one decode per node per cursor.
  `at_epoch` rebuilds a throwaway state from snapshot + parts + journal prefix.
- **API:** `lattice_tx::{Read, ReadReply, Selector, CursorFilter, Budget}` +
  `Kernel::read(session, &Read)`; `KernelOptions { limits, cursor_idle_ttl_us, clock }`
  (the clock is injected: ephemeral session state only). `Kernel::create/open` now take
  `KernelOptions` (tests: `common::options()`, `options_with(clock)`).
- **Tests:** `crates/lattice-tx/tests/kernel_read_path.rs` (12 tests: parity in every
  layer state, WPT-8.1..8.4, SGT-6.2, zone pushdown, node read, history, cursor ttl,
  selectors, scopes/read-your-writes). The 1M-cell test takes ~9 s / ~470 MB.
- **Bugs the read tests caught and fixed in the read path:** paged cursor re-emitted its
  first page forever; staged-only nodes bypassed selection; I64-stored part cells not
  re-typed; upper-bound typo in the value filter.
- **Gaps recorded (BACKLOG):** ED-26, ED-27, ED-28, SQ-11, SQ-12, SQ-13, ED-20 extended.

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

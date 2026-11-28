# TASKS — LatticeDB V1 (the ledger)

> Contract: MANIFEST v0.3 · ARCHITECTURE.md · SPEC-01…04 (all frozen v1.0)
> Scope: **V1 = single development stage spanning M1–M4** (M5 hardening = Supervisor gates).
> Every task cites spec sections + test IDs. No task without spec refs; no spec section
> without tasks. New ideas go to `BACKLOG.md`, never into V1 silently.

## Process law

- **Roles.** Architect pair (owner + opencode) — decisions only via decision entries
  (DEC-x in the owning spec / manifest). opencode = engineer + QA. **Supervisor**
  (independent agent, separate sessions) — reviews every completed task against spec refs
  and acceptance criteria; gates quality; never implements.
- **Testing law (strict).** Every test — unit included — is a real functional test:
  verifies data consistency, mutation, state outcome, replay identity. "Exit 0 = passed"
  is forbidden. Spec Test Definitions (purpose / principal target / expected outcome /
  level) are binding.
- **Definition of Done.** spec refs cited → acceptance tests green → Supervisor sign-off
  → ledger status updated → no unplanned features.
- **Statuses:** `pending → in_progress → review (Supervisor) → done` (+ `blocked`).

---

## M0 — bootstrap (pre-M1)

| ID | Task | Spec refs | Acceptance | Deps | Status |
|---|---|---|---|---|---|
| T-000 | Repo skeleton: workspace per ARCHITECTURE §3, CI (fmt/clippy/test), toolchain files | ARCH §3 | CI green on empty crates | — | done (m0-bootstrap) |
| T-001 | Config subsystem: TOML parse, strict validation, profiles, autodetect skeleton | SPEC-02 §10 | WPT-10.3–10.6 | T-000 | review (Supervisor, deferred to V1 gate) |

## M1 — graph core + storage + schema graph

| ID | Task | Spec refs | Acceptance | Deps | Status |
|---|---|---|---|---|---|
| T-101 | `lattice-core`: identifiers, epochs, node-id spaces, SoA/CSR arrays, cell types (scalars, records + presence bitmask) | ARCH §4; SPEC-01 §3, §5 | cell round-trip tests; array invariants | T-001 | done (m1-graph-core) |
| T-102 | `lattice-schema` entities: full catalog inventory, key grammar | SPEC-01 §2–§15 | SGT-2.1–2.3 | T-101 | done (m1-graph-core) |
| T-103 | Schema compiler: masks, read-sets, enabled-action index, ranks | ARCH §7; SPEC-01 §7, §9 | compile/recompile identity tests | T-102 | done (m1-graph-core) |
| T-104 | Schema transactions: op registry, R13, dry-run | SPEC-01 §16 | SGT-16.1–16.3 | T-103 | done (m1-graph-core) |
| T-105 | Journal: segments, framing, LSN, rotation, torn tail | SPEC-03 §6 | SFT-6.1–6.4 | T-101 | done (m1-graph-core) |
| T-106 | Snapshot: header/arrays/catalog blob/footer, CRC | SPEC-03 §3 | SFT-3.1–3.3 | T-105 | done (m1-graph-core) |
| T-107 | Parts: format, run directory, presence, zone maps | SPEC-03 §4 | SFT-4.1, 4.3, 4.4 | T-101, T-108 | done (m1-graph-core) |
| T-108 | Codec registry + chains + determinism | SPEC-03 §5 | SFT-5.1–5.3 | T-101 | done (m1-graph-core) |
| T-109 | MANIFEST swap + GC + archive retention | SPEC-03 §2 | SFT-2.1–2.2, 6.3 | T-106 | done (m1-graph-core) |
| T-110 | Scalar-as-column (k=0) + LSN overwrite resolution | SPEC-03 §4.1/§9.1 | SFT-4.2 | T-107 | done (m1-graph-core) |
| T-111 | M1 CLI smoke: create → write → restart → replay verify | ARCH §13 M1 | SFT-1.1 (seed mode); exit criteria met | T-104, T-106–T-110 | done (m1-graph-core) |
| T-112 | Template bundle installer + 3 bundles v0 | SPEC-01 §17 | SGT-17.1–17.3 | T-104 | done (m1-graph-core) |

## M2 — semantics engine

| ID | Task | Spec refs | Acceptance | Deps | Status |
|---|---|---|---|---|---|
| T-201 | FSM registry, ranks, backward prohibition | SPEC-01 §8 | SGT-8.1–8.5 | T-103 | done (m2-semantics-engine) |
| T-202 | Transition/action dispatch: requirements, actors, effects | SPEC-01 §9 | SGT-9.1–9.2, 9.5–9.7 | T-201 | done (m2-semantics-engine) |
| T-203 | Dependency algebra: 8 kind handlers | SPEC-01 §10; base report §3 | SGT-10.1–10.3 + algebra property tests | T-201 | done (m2-semantics-engine) |
| T-204 | Worklist propagation + read-set dirty tracking | ARCH §5.1, §8 | confluence property tests (permutation convergence) | T-202, T-203 | done (m2-semantics-engine) |
| T-205 | Timer wheel: lag, deadline, on_deadline | SPEC-01 §9 | SGT-9.3–9.4 | T-204 | done (m2-semantics-engine) |
| T-206 | Stall detector + blockers computation | SPEC-02 §8 | MCT-4.2 (core level) | T-204 | done (m2-semantics-engine) |
| T-207 | Validator suite R1–R13 complete | SPEC-01 §16; SPEC-02 §7 | SGT-13.x–15.x | T-101, T-102, T-104 (tests: T-202) | done (m2-semantics-engine) |
| T-208 | Determinism oracle + fuzz seeded | SPEC-03 §1 D1–D4 | SFT-1.1–1.3 | T-111 | done (m2-semantics-engine) |
| T-209 | Active meta-schema e2e | SPEC-01 §17 | SGT-17.4 | T-103, T-104, T-112, T-202 | done (m2-semantics-engine) |

Deps cells for T-207 and T-209 were malformed in the original ledger (self-referential);
corrected 2026-11-28 from the crate manifests and test imports — Supervisor verifies.

## M3 — kernel assembly + full CLI + wire

> **Re-cut 2026-11-28** (MANIFEST §10 "Sequence re-cut"): the core is assembled and proven
> through the full CLI before the listeners; T-311/T-312 (Kotlin SDK, MCP gateway) moved to
> M4. T-301–T-310 were accepted at codec/unit level against fakes (BACKLOG ED-12); the
> kernel-level and socket-level forms are completed by T-313–T-320. Execution order =
> ID order.

| ID | Task | Spec refs | Acceptance | Deps | Status |
|---|---|---|---|---|---|
| T-301 | Framing, type registry, TLV | SPEC-02 §2 | WPT-2.1–2.3 | T-111 | done (codec level; live forms → T-319/T-320) |
| T-302 | Handshake, capabilities, PROXY v2 | SPEC-02 §3 | WPT-3.1–3.4 | T-301 | done (unit level; live forms → T-319/T-320; byte order: SQ-05) |
| T-303 | Auth: policy table, SCRAM, cert, UDS | SPEC-02 §4 | WPT-4.1–4.5 | T-302 | done (in-process; live forms → T-319/T-320) |
| T-304 | Sessions, pipelining, TXN frames | SPEC-02 §5 | WPT-5.1–5.2 | T-302 | done (codec level; kernel form → T-313, wire form → T-320) |
| T-305 | Resume, keepalive, cancellation | SPEC-02 §5 | WPT-5.3–5.6 | T-304 | done (5.3/5.4 codec level; 5.5/5.6 untested → T-319/T-320) |
| T-306 | Credit flow control | SPEC-02 §6 | WPT-6.1–6.2 | T-304 | done (codec level; live forms → T-320) |
| T-307 | Write commands + hint law | SPEC-02 §7 | WPT-7.1–7.5 | T-304, M2 | done (core forms vs fake engine; kernel forms → T-313, T-318, T-320) |
| T-308 | Reads, cursors, time travel | SPEC-02 §8 | WPT-8.1–8.5 | T-307 | done (codec level vs fake; kernel forms → T-314, wire forms → T-320) |
| T-309 | Watches | SPEC-02 §9 | WPT-9.1–9.3 | T-305, T-308 | done (codec level vs fake; live forms → T-313, T-320) |
| T-310 | Concurrent subtree-locked writers + sequencer | ARCH §8 | confluence under parallel load; WPT-5.1 parallel variant | T-307 | done (in-memory fact path replaced by the durable kernel in T-313; its 5 tests ported) |
| T-313 | **Kernel assembly**: library-mode database — create/open/recover/checkpoint/close; write path validate → U-lock → sequencer → durable journal → apply → propagation → quiesce → watches; full fact set (structure incl. edges, attributes incl. row append, actions, schema txns, grants); multi-command txns | ARCH §5.1, §5.3, §8; SPEC-02 §5; SPEC-03 §1 (D2), §3, §6; SPEC-01 §9, §16 | WPT-5.2, WPT-7.1, WPT-7.3, WPT-7.4 (kernel forms); SGT-1.1, SGT-3.3, SGT-9.2, SGT-9.5 (kernel / journal forms); SFT-1.1 (kernel form: live = replay, 3 fresh copies); SFT-3.1; SFT-6.4 | T-104–T-110, T-202–T-208, T-310 | review (Supervisor, deferred to V1 gate) — tests: `lattice-tx` kernel_write_path, kernel_recovery, records_golden, parallel_confluence |
| T-314 | **Kernel read path**: snapshot (epoch-consistent, at_epoch), node read, cursors over arrays/parts, blockers, history | ARCH §5.2; SPEC-02 §8; SPEC-01 §6 | WPT-8.1–8.4 (kernel forms); SGT-6.2; SFT-4.3 (kernel form) | T-313, T-206 | review (Supervisor, deferred to V1 gate) — tests: `lattice-tx` kernel_read_path |
| T-315 | **Secondary indices + key-aligned cursor**: IndexDef views (by_state, attr_presence, set_presence) — rebuildable from the journal; index-backed cursor selectors replace T-314's scans; the `shared_key` key-aligned cursor (vectors sharing a node key fetched as records — `docs/MODEL.md` §5; parameters SQ-13) | SPEC-01 §6, §12; ARCH §5.2; SPEC-02 §8 | SGT-12.1, SGT-12.2, SGT-6.1 (record form) | T-314, SQ-13 | pending |
| T-316 | **Full CLI** (`latticedb-cli`, library mode): admin ops + the SPEC-02 §7–§10 command registry + schema plan/apply + bundles | ARCH §2, §13; SPEC-02 §7–§10; SPEC-01 §16–§17; SPEC-03 §7 | CLI-driven end-to-end on the real binary: SGT-17.1–17.3; SGT-16.3; WPT-7.1, WPT-7.3, WPT-7.4; WPT-8.2; restart/replay identity | T-313, T-314, T-315 | pending |
| T-317 | **Kernel semantics completion I — attributes & key axis**: enum/record lifecycle, sparse axis, monotone guard, predicate whitelist, tightening, replay across epochs | SPEC-01 §1, §3–§7, §13, §16 | SGT-4.1–4.3, 5.2, 5.4, 6.1, 6.3, 6.4, 7.1, 7.3, 13.1, 16.4, 16.5 | T-313, T-314 | pending |
| T-318 | **Kernel semantics completion II — authorization, aggregation, graph edges**: R11 scope checks + grants, children_state aggregation, cross-graph dependency edges | SPEC-01 §13–§15; SPEC-02 §7 | SGT-13.4, 14.1–14.3, 15.1, 15.2; WPT-7.5 | T-317 | pending |
| T-319 | **Listeners**: UDS + TCP data plane, session manager, Host `serve` | ARCH §2, §8; SPEC-02 §1–§6 | WPT-1.1; WPT-2.1, 2.2; WPT-3.2–3.4; WPT-4.1–4.2; WPT-5.3–5.6; WPT-10.1, 10.2 | T-301–T-306, T-313, T-314, BACKLOG SQ-04 DEC | pending |
| T-320 | **WPT socket-level integration pass** over live connections | SPEC-02 §2–§10 (integration-level tables) | WPT-2.3, 5.1–5.2, 6.1–6.2, 7.1–7.5, 8.1–8.4, 9.1, 9.3, 10.3, 10.6; closes ED-02, ED-12, ED-13 | T-319, T-307–T-309 | pending |

## M4 — external surfaces (protocol clients only) + TLS + pooler

| ID | Task | Spec refs | Acceptance | Deps | Status |
|---|---|---|---|---|---|
| T-311 | Kotlin SDK core (`sdk-kotlin` repo) | SPEC-02 (client side) | mirrors WPT-2–5 acceptance from JVM | T-301–T-305, T-319 | pending (moved from M3, DEC 2026-11-28; toolchain decided — STATE.md) |
| T-312 | MCP gateway: 16 tools | SPEC-04 §3–§7 | MCT-3.1–3.3, 4.x, 5.x, 6.1–6.3, 7.1–7.2 | T-308–T-310, T-319 | pending (moved from M3, DEC 2026-11-28) |
| T-401 | Admin gRPC: health, metrics, SchemaAdmin, storage ops | SPEC-02 §11 | WPT-11.1–11.2, WPT-1.2 | T-308, T-319 | pending |
| T-402 | TLS-at-origin + PROXY v2 live | SPEC-02 §3 | WPT-3.3–3.4 in real TLS setup | T-302, T-319 | pending |
| T-403 | REST/WS gateway (Kotlin) | SPEC-04 shapes over REST | parity tests vs MCP reads | T-312 | pending |
| T-404 | SPA read views (minimal) | SPEC-04 §4 shapes | visual + parity vs wire reads | T-403 | pending |
| T-405 | Pooler sidecar: identity translation, churn absorption | ARCH §2, §10 | WPT-5 churn variant through pooler; identity map e2e | T-305, T-311 (language: BACKLOG SQ-06) | pending |
| T-406 | Arrow payload completion | SPEC-02 §2; D-10.6 | zero-copy JVM read test (WPT-8.3 via SDK) | T-308, T-311 | pending |
| T-407 | Audit export seed | SPEC-02 §11 | segment export → reimport verify (SFT-6.3) | T-401 | pending |

## M5 — hardening (Supervisor-gated)

| ID | Task | Spec refs | Acceptance | Deps | Status |
|---|---|---|---|---|---|
| T-501 | Compaction under load | SPEC-03 §7 | SFT-7.1–7.4 under sustained parallel write load | M3 | pending |
| T-502 | Fuzz expansion: full oracle | SPEC-03 §1 | SFT-1.1 across 1000 randomized logs | T-208 | pending |
| T-503 | Audit export complete | SPEC-02 §11 | archive/reimport cycle green | T-407 | pending |
| T-504 | Index-module seam validation | ARCH §13 M5 | seam contract tests (ANN/BM25 stubs) | T-308 | pending |
| T-505 | Supervisor regression gate: full acceptance run | all specs | every SGT/WPT/SFT/MCT ID green, signed by Supervisor | all | pending |
| T-506 | Kernel completion: corruption detection and self-healing | SPEC-03 §8 | SFT-8.1–8.3 | T-313, T-314 | pending |

---

## Traceability checklists

- SPEC-01: SGT-1…17 → T-102–T-104, T-112, T-201–T-209, T-313, T-317, T-318, T-312
- SPEC-02: WPT-1…11 → T-301–T-309, T-313, T-314, T-319, T-320, T-401–T-402, T-311–T-312
- SPEC-03: SFT-1…8 → T-105–T-110, T-208, T-313, T-501–T-502, T-506
- SPEC-04: MCT-1…8 → T-312, T-403–T-404, T-505

## Spec test coverage audit (2026-11-28)

> Method: every test ID defined in the frozen specs was matched against the repo —
> by-ID test function names and ID mentions. **Untested** = no test anywhere carries the
> ID or asserts its expected outcome. Binding test levels are unchanged. This is the
> punch list behind "tested and fixed before anything external is wired" (MANIFEST §10).

- **Untested, kernel — closed by T-313/T-317/T-318:**
  SPEC-01 SGT-4.1, 4.2, 4.3, 5.2, 5.4, 6.1, 6.3, 6.4, 7.1, 7.3, 12.1, 12.2,
  13.1, 13.4, 14.1, 14.2, 14.3, 15.1, 15.2, 16.4, 16.5 (21 IDs; indices → T-315);
  SPEC-02 WPT-7.5 (T-307 ran "core forms" against a fake engine).
  **Closed by T-313 at kernel level:** SGT-1.1, 3.3, 9.2, 9.5; WPT-5.1 (parallel
  variant), 5.2, 7.1, 7.3, 7.4.
- **Untested, need the listener — closed by T-319/T-320:**
  WPT-1.1, 5.3, 5.5, 5.6, 10.1, 10.2.
- **Untested, admin plane — closed by T-401:** WPT-1.2, 11.1, 11.2.
- **Untested, hardening — closed by M5:** SFT-7.1–7.4 (T-501), SFT-8.1–8.3 (T-506).
  SFT-4.3 (zone pushdown) was here as "mention only"; closed at kernel level by T-314.
- **Codec/unit level only while the binding level is integration (ED-12 → T-320):**
  WPT-2.1–2.3, 4.1–4.3, 5.1–5.2, 6.1–6.2, 7.x, 8.1–8.4, 9.1, 9.3, 10.3, 10.6.
- **Carried by tests not named after the ID — Supervisor verifies at the gate:**
  SGT-1.2, 3.1, 3.2, 3.4, 5.1, 5.3, 7.2, 7.3, 11.1, 13.3, 15.3, 16.2; WPT-4.4, 5.4.
- **Hygiene:** `sft_8_4_runtime_backward_prohibition` renamed to
  `sgt_8_4_runtime_backward_prohibition` (T-313) — SFT-8.4 does not exist.

## T-313 delivery notes (2026-11-28)

**Law:** one mutation primitive. `plan(command) → records` validates read-only;
`apply(record, lsn)` serves BOTH live execution and journal replay, so live = replay by
construction. Write path: plan → journal (`BEGIN` + records + `COMMIT`, one call) → apply
→ quiesce → publish events → sync. A failure after the journal write poisons the kernel.
Module docs: `crates/lattice-tx/src/{state,kernel,handle,records}.rs`.

**Delivered:** `lattice-tx` rebuilt as the kernel — `error` (one code registry; the wire
crate re-exports it), `records` (durable layouts, golden-tested), `command` (typed
commands every surface builds), `state` (EngineState: R2/R3/R6/R7/R12 + state locks +
action checks), `kernel` (create/open/recover/checkpoint/close, write path, explicit
txns, watch emission), `handle` (sequencer thread + writer pool). The in-memory fact path
(`gate`/`sequencer`/`writer`) is removed; T-310's tests are ported to the durable kernel.

**Defects found in earlier crates by the kernel tests, fixed here (BACKLOG):** core value
codec decoded I64/F64/Timestamp/Duration as 0, panicked on NodeRef, skipped 16 bytes past
Decimal (ED-21); part builder encoded its LSN column with the value codec, so no default
chain could build (ED-22); empty op-log replay committed a phantom schema txn (ED-23);
lock table read a poisoned mutex as "nothing held" (ED-24); node-type codes and attr ids
were not lineage-stable across a restart (ED-14).

**Not in T-313 (decided, recorded):** the timer path — logical time has no journal record
(SQ-09); state-lock refusals map to `FORBIDDEN` pending a code decision (SQ-10); guard
params are rebuilt from the journal at open because the snapshot format cannot hold them
(ED-15).

## T-314 delivery notes (2026-11-28)

**Model.** Reads run between committed units on the sequencer thread, so a read sees
exactly one epoch — never a torn mix (WPT-8.1). The epoch marker is the COMMIT LSN of
the last unit the read saw. Cursors read COLUMNAR DATA from the parts ⊕ the staging tail
(cells written since the last checkpoint); a run excluded by liveness, selection, the run
directory or the zone maps is never decoded, and each node's cells are decoded once per
cursor whatever the page size. The engine keeps its own in-memory cell store for
predicates; "cursor == engine store" is a tested invariant in every layer state (tail
only, parts only, both, reopened). `at_epoch` is a time-travel read: a throwaway state is
rebuilt read-only from the retained snapshot + parts + journal prefix (WPT-8.2).

**API.** `lattice_tx::{Read, ReadReply}` — `Snapshot{graph?, subtree?, at_epoch?}`,
`NodeRead`, `CursorOpen{attr, selector, filter}` / `CursorFetch{cursor, budget{rows,
bytes}}` / `CursorClose`, `Blockers`, `History{node, range}`. `Kernel::read(session, ..)`
honours a session's explicit txn (read-your-writes). `KernelOptions` carries the limits,
the cursor idle ttl and a Host-injected clock (ephemeral session state only — P8 keeps
wall-clock out of predicates and stored bytes).

**Defects the read tests caught in the read path itself, fixed before commit:** a paged
cursor re-emitted its first page forever (the reader never sought past the resume point);
staged-only nodes bypassed node selection; part cells of I64 storage were not re-typed
(`node_ref`/`timestamp`/`duration`); an upper-bound typo in the value filter.

**Not in T-314 (decided, recorded in BACKLOG):** reads are serialized with writes, not
lock-free (ED-26); no `order` parameter and single-column cursors (ED-27); parts are read
into a cache, not mmap'd, and a cursor is not a snapshot across fetches (ED-28);
`CURSOR_EXPIRED` has no registered code — mapped to `NOT_FOUND` + hint (SQ-11); cursor
selectors scan the column — index-backed selection is T-315; the `shared_key` cursor
(the key-aligned cursor, `docs/MODEL.md` §5) waits for SQ-13's parameters; the engine holds
every cell in memory (SQ-12).

## Model errata (2026-11-28, after the owner's review)

A re-read of the whole contract with the architect's hat (`docs/MODEL.md`, now required
reading — AGENTS rules 10 and 11) found two concepts that had been implemented from
their names:

- **Row append used one `k` counter per table set.** The contract's counter is per node:
  one key axis per node (SPEC-01 §6, §18.1). Fixed in T-313's code; fixture gained a
  second table set; test `one_key_axis_per_node_spans_every_table_set` (ED-29).
- **`shared_key` was implemented as "nodes whose scalar attribute equals a value".** It
  is the key-aligned cursor over the vectors that share a node key. The wrong selector is
  removed from the kernel; the right cursor is T-315 work and waits for SQ-13's
  parameters (ED-30).

Open points the re-read surfaced: SQ-14 (first `k` of a vector axis), SQ-15 (point
append at `k == current_max`), SQ-16 (unit of the global epoch marker).

## Gap audit (2026-11-28, owner-mandated: every gap from M1 to M5)

The owner asked for every gap that could stop work for a decision, so that they are
closed up front. Result: **39 open decisions**, listed as a queue at the top of BACKLOG's
"Spec questions" section (SQ-01…SQ-43), each ordered by the milestone it blocks and each
with a recommendation. Two of them are defects to fix regardless of the decision:

- **SQ-23** — the journal frame cap is enforced only on recovery; a record over the cap
  is written and then unreadable.
- **SQ-24** — `DropGraph` ignores live nodes.
- **SQ-17** — a record field's position is taken from its key's trailing digits.

Also corrected in the same pass: `docs/MODEL.md` no longer describes a node as a table —
the relational words are an analogy for `k` only; a scalar is one value stored, internally,
as the k = 0 cell of its attribute.

Work continues on what is already decided while the queue is worked through.


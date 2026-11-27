# ARCHITECTURE — LatticeDB

> Emergenta · LatticeDB v1 · 2026-11-26 · Based on MANIFEST v0.3 (frozen)
> Specs (Schema Graph, Wire Protocol, Storage Format, MCP Surface) elaborate this document.

---

## 1. Overview & philosophy

One ledger, three event faces: **commands** (outer events — the only non-derivable input),
**derivations** (inner events — deterministic, replayable), **watches** (egress —
projections of the same journal from a single emission point).

Four minimality results the whole architecture rests on:

1. **Three write classes** — Δstructure · Δattributes · εactions. Nothing else enters;
   state is never written, only derived.
2. **Confluent propagation** — derived state is a unique monotone fixpoint
   (Kleene/Tarski); event order cannot change outcomes. This is what makes concurrent
   subtree writers safe.
3. **Schema-as-graph, compiled** — everything typed lives in the schema graph; a compiler
   turns it into dispatch masks, read-sets and the enabled-action index; runtime checks are
   bitmask tests; schema changes are journaled transactions.
4. **Journal = the only authority** — snapshot, columnar parts, indices, compiled masks
   are rebuildable projections; recovery replays to bit-identical state.

Design laws inherited from the manifest: kernel/shell split (P9) · compactness (P11) ·
domain neutrality (§7) · polyglot split (P10) · timer wheel as the only time source (P8).

## 2. Process decomposition

```
                       ┌─────────────────── Host process (latticedb) ───────────────────┐
 session/UX clients    │                                                                │
 (SDK, psql-like) ────►│  Wire listener (UDS/TCP, mTLS)  ──  Admin gRPC (DDL, metrics)  │
                       │        │ Session manager                │                      │
 pooler (sidecar) ────►│        ▼                                ▼                      │
 MCP gateway ────────► │  Transaction engine              Observability (metrics,       │
 REST/WS gateway ────► │   ├ validators R1–R13            audit export, dry-run)        │
                       │   ├ subtree U-locks                                            │
                       │   ├ SEQUENCER (journal LSN)                                    │
                       │   └ in-memory apply                                            │
                       │        │                                                       │
                       │        ▼                                                       │
                       │  Propagation engine                                            │
                       │   ├ worklist (fixpoint)         ── timer wheel (only clock)    │
                       │   ├ dependency algebra (8 kinds) ── stall detector             │
                       │   └ watch bus (single emission point)                          │
                       │        │                                                       │
                       │        ▼                                                       │
                       │  Schema compiler (per schema epoch: masks, read-sets,          │
                       │   enabled-action index, index defs)                            │
                       │        │                                                       │
                       │        ▼                                                       │
                       │  Storage engine: MANIFEST · snapshot · journal · parts         │
                       │   (self-healing: CRC + rebuild from journal)                   │
                       └────────────────────────────────────────────────────────────────┘
```

**Processes:**

- **`latticedb` (Host binary)** — everything above. Also usable as a **library** (in-process
  mode: plain function calls into the same crates; no sockets; trusted host-process subject).
- **Pooler (sidecar, first-party)** — identity translation (mTLS/SPIFFE → kernel subject +
  scopes), churn absorption, credential mapping, short-lived token issuance (phase v1.x).
  Thin, stateless besides credentials; connects upstream over few long-lived mTLS sessions.
- **MCP gateway** — Kotlin process on the SDK; speaks the wire protocol; maps MCP tools to
  commands; binds tool→scope.
- **REST/WS gateway** — Kotlin; same protocol client; serves the SPA read views.
- **CLI (`latticedb-cli`)** — ops and smoke tests; library mode + wire mode.

Layering law: gateways are protocol *clients*; there is no second path into the core.

**Configuration subsystem (owner law: no hardcoded parameters).** Every tunable — limits,
flow-control windows, timer granularity, worklist depth, merge thresholds, arena budgets,
thread pools, TTLs — is a config parameter with documented default, safety clamp, unit and
description, held in **one host config file** together with the auth policy table.
Predefined profiles (`dev / laptop / server / analytics`) ship versioned;
`latticedb config autodetect` generates a tuned set from host CPU/RAM/disk; strict
validation fails startup on unknown keys (no silent defaults); hot-reload classes are
documented. Performance per config set is measurable through the admin metrics plane —
tuning is evidence-based (SPEC-02 §10).

## 3. Rust workspace layout

```
latticedb/                    (github.com/latticedb-io/latticedb)
├── crates/
│   ├── lattice-core/         SoA/CSR graph arrays, node-id spaces, epochs, attribute
│   │                         cells (in-memory), state codes. NO I/O, NO I/O deps.
│   ├── lattice-schema/       schema-graph entities + compiler → masks, read-sets,
│   │                         enabled-action index, index-def plans.  depends: core
│   ├── lattice-storage/      snapshot writer/reader (mmap), journal segments
│   │                         (append/replay), columnar parts + merges, manifest swap,
│   │                         CRC, codecs.                       depends: core
│   ├── lattice-engine/       FSM registry, transition/action dispatch, dependency
│   │                         algebra (AND OR KOFN GUARD INHIBIT XOR MIRROR WEIGHTED),
│   │                         worklist, timer wheel, stall detector.  depends: core, schema
│   ├── lattice-validate/     rules R1–R13 + kernel scope checks (R11). depends: core, schema
│   ├── lattice-tx/           transaction engine: subtree locks, staging, sequencer,
│   │                         write path orchestration.            depends: all above
│   ├── lattice-wire/         framing, handshake, policy table, SCRAM/cert/UDS auth,
│   │                         sessions, pipelining, cursors, Arrow IPC payloads.
│   │                                                              depends: tx, engine
│   ├── lattice-admin/        gRPC control plane: metrics, dry-run plan_schema_txn,
│   │                         health, audit export.                 depends: tx, storage
│   ├── lattice-pooler/       sidecar binary (identity translation, token issuance).
│   │                                                              depends: wire
│   └── latticedb/            the Host binary: assembly + config.  depends: all
├── cli/                      latticedb-cli
├── fuzz/                     determinism oracle + replay fuzz (M5, seeded in M2)
└── tests/                    property tests (algebra confluence, replay identity)
```

**Dependency rules:** strictly downward; `wire` never touches `storage` directly; `core`
has zero I/O (keeps library mode pure and fuzzing fast); Kotlin side lives in a separate
repo (`sdk-kotlin`, `gateway-mcp`, `gateway-rest`) consuming the wire protocol only.
No async runtime in kernel crates — `wire`/`admin` own tokio/tonic and hand commands to
blocking kernel APIs via bounded queues (SPEC-02 §12.8); the global allocator is
mimalloc by default behind a build feature, its tunables surfaced as §10 config
(SPEC-02 §12.9).

## 4. Containment & addressing

```
Host ─► Database (isolated: catalog, journal, node-id space)
          ├─ Catalog: schema graph (types, models, transitions, keys, rules, policies)
          └─ Graph schema (namespace) ─► composition forest (multiple roots) ─► nodes
```

| Identifier | Scope | Type | Notes |
|---|---|---|---|
| `database_id` | Host | u32 | assigned at CREATE DATABASE |
| `node_id` | Database | u32 | **one space across all graphs**; graphs own subsets |
| `attr_id` | Catalog | u16 | stable per schema epoch lineage |
| `k` | Node (KeyType) | u64 | dense sequence by default; sparse if KeyType declares |
| `epoch` | Node | u64 | monotone change counter; snapshot marker |
| `LSN` | Database journal | u64 | total order of all committed facts incl. schema txns |
| `schema_epoch` | Database | u64 | bumped by journaled schema transactions |
| `graph_id` | Database | u16 | namespace partition; rides in zone maps |

Node addressing = `(database, node_id)`; cell addressing = `(database, node_id, attr_id, k)`.

## 5. Data flow

### 5.1 Write path (the spine)

```
command (Δstruct | Δattr | εaction)
  → parse, authenticate (policy table)            [wire]
  → authorize: kernel scopes (R11)                [validate]
  → validate R1–R13 vs compiled masks             [validate]   ← node-local only
  → acquire subtree U-lock (conflict domain: target node + parent for Δstruct)
  → SEQUENCER: assign LSN, journal append (group commit, batched vector appends)
  → in-memory apply (arrays + staging buffers for parts/index deltas)
  → propagation worklist: guards fire, aggregations recompute, requirements
    re-evaluate (read-set dirty tracking), enabled-action index update,
    MIRROR copies, timer wheel arms/deadlines
  → fixpoint quiesce (guaranteed: monotone operators)
  → emit watches (same append point, epoch-ordered)
```

Validation happens **at the sequencing point** — replay then reproduces validity exactly
(per-node fact order preserved in the journal). Derived recomputation is order-insensitive
(confluence), so concurrent writers only affect throughput, never semantics.

### 5.2 Read path

Session → read epoch marker → lock-free access: structural arrays + columnar parts.
Cursors compose the kernel set (scan → filter (zone-map pushdown) → aggregate /
merge-join-on-key / latest-per-node / top-k) over fixed vectorized batches; fetch frames
carry Arrow IPC chunks. Subtree/blocker/schema reads traverse CSR in O(result).

### 5.3 Recovery path

MANIFEST (atomic pointer) → mmap snapshot + valid parts → journal replay from committed
offset: schema txns recompile the compiler at each schema epoch; facts validated/derived
identically to live traffic; worklist rerun → quiesce → open for service. Corrupt part
(CRC) → transparent rebuild from journal. Timer wheel re-arms deadlines from journal facts.

### 5.4 Timer path

The timer wheel is an **engine actor**: firing emits a scheduled action or deadline-miss
fact and enters the same write path (author = `system/timer`). Deadlines and lags are
journal facts; re-derivable at replay; wall-clock never enters predicates.

## 6. Storage design summary

- Per Database: `MANIFEST` (atomic swap: snapshot id + journal offset + generation),
  `snapshot.v<N>` (structural arrays + catalog, CRC32C per 64 KiB block, mmap-able),
  `journal.log` segments (length-prefixed CRC'd records: BeginTxn / AddVertex / DelVertex /
  AddEdge / DelEdge / SetAttr / AppendCells / Action / SchemaTxn / Commit), `parts/`
  (immutable columnar units keyed by (graph, attribute), cells sorted by (node_id, k)).
- Codecs per scalar type: delta, delta-of-delta (timestamps), Gorilla (floats), dict/RLE
  (enums/tags), bit-pack (bool), zstd at part level; codec overridable per attribute.
- Zone maps per run-block (min/max, graph id) — cursor filter pushdown.
- Merges: background size-tiered; deterministic sort-merge; concurrent with epoch readers;
  orphaned cell bytes (post-schema-drain) physically purged at merge.
- Full byte layouts → **Storage Format Spec**.

## 7. Schema compiler & guarded evolution

- Compile inputs: schema-graph entities (manifest §5 inventory).
- Outputs: `(type, state) → writable attribute-set mask`; `(type, transition) →
  requirement read-set mask + predicate handles + actor mask + effects plan`; enabled-action
  index (per node bitset); physical secondary indices from IndexDefs.
- **Schema transaction flow:** `plan_schema_txn(ops)` (dry-run: per-op accept/reject with
  live-reference paths and impact counts) → R13 reference-count validation → journal
  SchemaTxn → incremental recompile → `schema_epoch++`.
- Evolution rules (D-4.x): additive free with absence semantics; tightening binds future
  writes only; deprecate → drain → remove; logical deletion forever; migrations = resumable
  transaction scripts with epoch checkpoints.

## 8. Concurrency model

- **Writers:** N worker threads; U-lock per subtree (granularity: graph-coarse default,
  subtree-fine opt-in); conflict domain = target node (+ parent for Δstructure).
- **Sequencer:** single thread; assigns LSN; appends journal; preserves per-node fact
  order (dense `k` assignment depends on it).
- **Staging:** per-partition staging buffers → background assembler → parts; mergers
  concurrent with readers (immutable parts).
- **Readers:** lock-free epoch snapshots (mmap + atomic manifest swap; LMDB discipline).
- **Execution model:** one process, no edge↔kernel IPC. The Host binary owns the
  wire/admin listeners; async session tasks multiplex sockets, parse frames, and submit
  commands to the synchronous kernel through bounded queues at the sequencing point.
  Kernel crates (core/engine/storage/tx) run on dedicated OS thread pools, expose
  blocking APIs only, and contain no async runtime. This topology is forced by the v1
  auth design (UDS SCM_CREDENTIALS and TLS-at-origin identify the client only when the
  DB process itself terminates the connection) and by the layering law (P12: gateways
  are protocol clients; there is no second path into the core). Pool sizes and budgets
  are SPEC-02 §10 config parameters, sized by profiles/autodetect.
- **Correctness:** validation node-local (sequencing-time) + confluent derivations ⇒
  any interleave converges to the journal's own replay. Property-tested: (a) replay
  identity, (b) permutation-convergence of derivations.

## 9. Security model

- Subjects {user | agent | service} × scope sets {read-subtree, write-structure,
  write-attribute, invoke-action, administer}; grants bound to Database/Graph/subtree
  (journaled, audited).
- Host-level policy table: `(listener, database, subject-pattern, source, tls-state) →
  (method, scope bound)`; first match; hot-reloadable config.
- Phase v1: SCRAM-SHA-256 · client-cert (CN→subject map) · UDS peer-cred map · TLS
  require/verify-ca/verify-full · admin listener with dedicated subjects + mTLS.
  Phases v1.x (OIDC/LDAP gateway federation, short-lived pooler tokens) and v2 (SPIFFE,
  IAM) per `research/identity-auth-survey.md`.
- Authn failures → security log; in-DB allow/deny → journal. TLS terminates at the DB
  process; handshake tolerates PROXY v2.

## 10. Deployment topologies

1. **Embedded/library** — in-process, no network surface; host app = trusted subject.
2. **Single host** — binary + UDS for local agents; TCP optional.
3. **Cluster (canonical)** — app pods → UDS → first-party pooler sidecar → mTLS →
   L4 LB (NLB / Gateway API TLSRoute) → Host. Gateways (MCP/REST) as separate pods.
4. **Process-per-tenant** — hard isolation variant for strict tenancy; same binaries.

## 11. Failure model & determinism strategy

- Crash → journal replay (bit-identical by construction; property-tested).
- Corruption → CRC blocks → transparent rebuild; MANIFEST swap is the only atomic rename.
- **Determinism oracle:** the fuzz suite replays randomized transaction logs and asserts
  byte-identical state across runs and across process restarts (TigerBeetle VOPR ethos);
  seeded from M2, expanded in M5. Algebra property tests: confluence, monotonicity,
  absorbing terminals, absence semantics.

## 12. Observability

- Admin gRPC: health, metrics (Prometheus exposition), `plan_schema_txn` dry-run,
  compaction controls, audit journal streaming/export (phase v1.x), slow-cursor traces.
- Watch bus doubles as the audit feed: every state motion, action, and authorization
  decision is an epoch-ordered event.

## 13. Milestone mapping

| Milestone | Components | Exit criteria |
|---|---|---|
| M1 | `core`, `storage`, `schema` (compiler skeleton), config subsystem (profiles + autodetect skeleton), `latticedb` bin skeleton, CLI | CLI smoke: build graph, write cells, restart, verify replay; config strict-validation test green |
| M2 | `engine` (algebra, FSMs, timers, worklist), `validate` (R1–R13), dry-run API | determinism fuzz green; algebra property tests green |
| M3 | `wire` (UDS+TCP full), Kotlin SDK, MCP gateway, concurrent writers + sequencer | pipelined sessions; subtree-locked parallel writes; MCP tool demo |
| M4 | `admin`, REST/WS gateway, SPA read views, TLS-at-origin, PROXY v2, pooler, Arrow payloads | gateway matrix demo; pooler identity translation live |
| M5 | hardening: compaction under load, audit export, fuzz expansion, index-module seam | load + fuzz gates; seam documented for ANN/BM25 (post-beta) |

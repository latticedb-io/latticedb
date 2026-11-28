# MANIFEST — LatticeDB

> Codename: `uvdb` · Product: **LatticeDB** · Company: **Emergenta** · Version **0.3** · 2026-11-26
> Status: **FROZEN** — all architectural discussions closed (Q10 closed 2026-11-27).
> Supporting research in `research/`.

---

## 1. Identity

LatticeDB is a compact, standalone, **general-purpose**, multi-graph, stateful database:

- **Grand-Index architecture**: a forest axis (composition), a DAG axis (dependency),
  columnar attribute parts, derived secondary indices — the database itself is the index;
  reads go through cursors composing a closed kernel set; there is no query planner.
- typed graph schema held **as graph data** (self-describing, kernel-validated, dry-runnable).
- **scalar + vector attributes** (per-node key axis → a node is a columnar table),
  enum types, flat record types. JSON is a string plus a decompose-to-records pattern.
- per-node state machines: **state is always derived, never written** — actions are the
  only state-moving input class.
- persisted as an immutable snapshot + **deterministic replay journal** (schema changes
  included); every other structure is a rebuildable projection.
- **concurrent subtree-locked writers** over a single journal sequencer.
- surfaces: native wire protocol + Kotlin SDK (first), first-party pooler, MCP tools for
  AI agents, REST/WS for humans.

One sentence: **a multi-graph, schema-typed, stateful graph database with scalar and vector
attributes, deterministic derived state, and kernel-enforced schema evolution — the
database itself is the index.**

Target domains: project/task management (OLTP) · ETL & analytics (OLAP-lite + series) ·
BI / technological state engines · financial decision support (series + audit + gates).

**Brand assets (2026-11-26).** Company umbrella: **Emergenta** — bare, no suffixes;
products endorsed "*… by Emergenta*". Domains: `emergenta.dev` (canonical) +
`emergenta.io` (alias redirect) + `latticedb.io` (product). GitHub: org `latticedb-io`
(product repos); company-level org handle pattern `emergenta-io`. Internal repo codename:
`uvdb`.

## 2. Lineage

1. **UVFlow Outcome** (deprecated, `../uvflow`) — mutator-centralization; "the engine
   deserves to be a real DB"; attributes/sets/groups/state-model vocabulary; origin of the
   UV naming: **U = trees (composition), V = the DAG inside/between (dependency)**.
2. **Technical Design Report** (`research/process-dependency-graph-engine.pdf`) — base
   specification: graph model, dependency algebra, monotone FSMs, snapshot+WAL storage,
   R1–R12 validation, kernel/shell access control, dual surfaces.
3. **Work-Graph research** — agent-first coordination demand; shapes *sufficiency
   requirements*, not shipped features.
4. **In-project research** — `research/column-store-comparison.md` (storage: D-10.x),
   `research/schema-evolution-survey.md` (evolution: D-4.x).

## 3. Pillars

- **P1 — A real database (Rust).** Standalone, compact, no host DB. Library mode = plain
  function calls. Own storage: snapshot + journal + manifest, CRC-per-block, group commit,
  mmap reads, immutable columnar parts with background merges.
- **P2 — State is derived, never written.** Write domain = **three classes**:
  Δstructure · Δattributes · εactions. Schema defines *what can happen* (transitions),
  requirement rules define *when it is allowed*, actors (human / agent / guard / timer)
  and guard edges define *when it happens*. Decision records collapse into select-branch
  transition semantics; evidence reduces to attributes + rules; recurrence to spawn
  transitions.
- **P3 — Schema is a graph, compiled.** Node types, key types, attribute defs/sets, state
  models, transitions, edge kinds, index defs, named rules, policies — all schema data.
  A schema compiler builds dispatch masks, requirement read-sets and the enabled-action
  index at each schema commit; runtime checks are bitmask tests. Schema transactions are
  dry-runnable (impact report) before commit.
- **P4 — Fixed algebra, reducibility discipline.** AND, OR, k-of-n, GUARD, INHIBIT, XOR,
  MIRROR, WEIGHTED + whitelisted monotone predicate forms. Every proposed new kind starts
  as a composition until proven irreducible.
- **P5 — Scalar + vector attributes; enums and records.** Fixed scalar set (bool, i64, f64,
  fixed-point decimal, string, bytes, timestamp, duration, node_ref); EnumType (closed
  value sets); RecordType (flat, depth-1, scalar/enum fields → per-field sub-columns);
  Vector\<T\> along the node-local **KeyType** axis (dense by default, sparse by
  declaration). No JSON type, no JSON indices — `json_data: string` +
  `indexed_json_data: record`.
- **P6 — Host → Database → Graph → forest.** Full containment hierarchy (the PostgreSQL
  analogy, extended): a **Host** (instance; listeners, auth policy, subject domain) hosts
  **Databases** — isolated storage domains (own catalog, journal, node-id space; no
  cross-Database references). Each Database carries a **catalog** (the meta-schema graph:
  types, state models, transitions, index defs, rules) and named **graph schemas** — one
  graph each; the graph schema is the namespace (a Department in UV terms). Each graph
  holds a composition **forest** (multiple roots) of **node tables** (key + columns,
  logical view) typed by the catalog. Composition edges live strictly inside their graph;
  dependency edges may cross graph schemas within a Database (the cross-schema FK
  analogy), acyclic per Database.
- **P7 — The journal is the only authority.** Facts, structure ops, actions and schema
  transactions are journaled; epochs order the world; recovery replays to bit-identical
  state. Columns, secondary indices, compiled masks — all rebuildable projections;
  self-healing via CRC + transparent rebuild from journal.
- **P8 — Monotone core, facts at the edge, one time source.** Derived state is a unique
  monotone fixpoint for **finalizing** state models; **endless** models (no terminals,
  cycles legal) are fact/guard/timer-driven event sources that feed the world but are never
  boolean-aggregated. Time enters only through the engine's timer wheel — wall-clock
  predicates are structurally impossible.
- **P9 — Unbypassable kernel, volatile shell.** Scope checks in-transaction (R11); guarded
  schema evolution (R13) with reference counting; rich, volatile policy in gateways
  (pooler, MCP, REST); every allow/deny journaled.
- **P10 — Polyglot law.** Rust = core, storage, engine, wire protocol (CPU-bound).
  JVM/Kotlin = SDK, gateways, business services. SDK roadmap: **Kotlin first**; Java, Node,
  Python, Rust, C/C++, bash/CLI after first beta.
- **P11 — Compact.** Everything fights for its bytes; nothing enters the core that can live
  above it. Business-layer concerns (comments, documents, bundles, notifications UX) are
  not core features — the DB stays domain-ignorant.
- **P12 — Surfaces from day one.** Native protocol is primary; SDKs stay thin; MCP and
  REST/WS gateways are protocol clients, never parallel paths; bulk cursor reads carry
  Arrow-compatible payloads (zero-copy for JVM/Python/Rust).
- **P13 — Connectivity law.** See §6.
- **P14 — Grand Index.** Attribute data lives in immutable columnar parts ordered by
  (node_id, k) with CSR node-run offsets; derived secondary indices (same-type, (type,
  state), attribute-presence, shared-key, action-enabled) are rebuildable views; cursors
  compose the closed kernel set (scan, filter, aggregate, merge-join-on-key,
  latest-per-node, top-k) over fixed vectorized batches.
- **P15 — Composed writes.** Action validation is node-local; propagation is confluent
  (order-insensitive fixpoint). Concurrent writers lock subtrees (U-locks); V-edges carry
  only derived effects, never locks; a single sequencer gives the journal its total order
  and preserves per-node fact order.
- **P16 — Event discipline.** Outer events = **commands** (the only non-derivable input,
  acked with epoch/LSN, batchable). Inner events = **derivations** (transitions, guard
  armings, requirement enable/disable, timer firings; journaled only for observability).
  Egress = **watches**: one emission point at the journal append, epoch-ordered, resumable,
  filterable; queue bridges (Kafka/NATS) are post-beta accessories.

## 4. Non-goals

- No SQL, no query planner, no general OLAP engine (OLAP-lite = cursors + kernels).
- No JSON type, no JSON indices; no general document store.
- No manual state management; no state-write API.
- No arbitrary user code — whitelisted monotone predicate forms only.
- No wall-clock predicates; no timers outside the engine's timer wheel.
- No in-kernel backfill state machines; no shadow-table migrations; no migration DSL.
- No ANN / BM25 in core (deferred pluggable index modules, post-beta).
- No shared-memory IPC until a measured, money-relevant latency requirement exists.
- No L7 anything in the data path.
- No distributed multi-master in v1 (per-graph sharding seam documented, no work).
- No materialized rollups / continuous aggregates in v1 (parked: future computed-attribute
  kind).

## 5. Data model

**Graph.** Containment hierarchy: Host → Database → Graph schema → composition forest →
node. One engine (Host), many Databases (isolated: own catalog, journal, node-id space),
each Database with a catalog (meta-schema graph) and named graph schemas (namespaces /
Departments), each graph a composition forest with multiple roots. Composition edges
(E_C) stay inside their graph; dependency edges (E_D) may cross graph schemas within a
Database, acyclic per Database; no cross-Database references.

**Write classes.** Δstructure · Δattributes · εactions — plus reads (queries, cursors,
watches). No state writes exist.

**Edge algebra.** AND, OR, k-of-n (quorum/weighted), GUARD (σ, τ, lag, deadline), INHIBIT,
XOR, MIRROR, WEIGHTED; monotone predicates attachable to edges and transitions.

**State models.** Per type, in schema. **Finalizing**: monotone forward chain, success/
failure terminal classes, participates in aggregation. **Endless**: no terminals, cycles
legal, motion only by actions/guards/timers; may feed guards, weighted progress and vector
reads. The **transition** is the central schema object: from→to, requirement rules,
actor classes {human, agent, system, timer, guard}, effects (spawn(instance_of),
select-branch, cancel-siblings, attribute-lock remap), timer bindings. A transition with
non-empty actor classes *is* an action type.

**Schema inventory (closed list).** ScalarType (built-in set) · EnumType · RecordType ·
Vector\<T\> · KeyType · AttributeDef · AttributeSet · StateModel · Transition · EdgeKind
(fixed registry) · NodeType · IndexDef · NamedRule/Policy · Graph namespace.

**Schema evolution.** Schema ops are journal transactions bumping **schema epochs**; the
compiler rebuilds masks incrementally; replay validates every fact against its epoch.
R13: kernel-enforced reference counting rejects breaking ops (occupied states, referenced
attributes/rules/indices, used enum values, populated key axes) — discipline:
**deprecate → drain → remove**. Additive changes are free with absence semantics
(defaults at read). Loosening applies immediately; tightening binds future writes only.
Deletion is logical forever. Migrations = resumable, idempotent transaction scripts
(epoch checkpoints), audited by the journal. `plan_schema_txn` dry-run returns per-op
accept/reject with live-reference paths and impact counts.

**Storage.** Structural SoA/CSR arrays (base) · immutable columnar parts per
(graph, attribute), cells sorted by (node_id, k) (base data layout) · zone maps mandatory,
per-type codec registry (delta, delta-of-delta, Gorilla, dict/RLE, bit-pack, zstd) ·
background size-tiered merges · secondary indices and compiled masks = derived views ·
journal = WAL duality: the only authority; parts rebuild transparently on CRC failure.

## 6. Connectivity & transport

**One binary, three sockets:** UDS (same-host, SCM_CREDENTIALS) · TCP data plane (mTLS,
TLS-at-origin, PROXY v2 tolerated) · gRPC admin (DDL, metrics, ops).

**By reach distance:** in-process = function calls · same host = UDS · in-cluster =
TCP+mTLS · cross-region/edge = TCP (QUIC post-1.0, ALPN reserved).

**Wire protocol (Postgres-v3 discipline):** `[u8 type][u32 len][payload]` framing ·
HELLO → TLS → PARAMS → SCRAM-style AUTH handshake · capability negotiation in-handshake ·
credit-based flow control · pipelining (session = connection, frames in flight) ·
out-of-band cancellation via session secret · protocol-level keepalive · opaque resume
tokens integrated with epochs · cursor fetch payloads = Arrow IPC chunks.

**Deployment shape:** app → UDS → sidecar pooler (first-party component: identity
translation, churn absorption, credential mapping) → mTLS → L4 LB (dumb pipes, NLB /
Gateway API TLSRoute) → LatticeDB. **Identity is phased** (see
`research/identity-auth-survey.md`): fixed kernel contract = subjects {user | agent |
service} × scope sets bound to Database/Graph/subtree, host-level pg_hba-style policy
table (listener × subject × source × tls-state → method); **v1** = SCRAM-SHA-256 +
client-cert (subject in CN, pg_ident-style map) + UDS peer-cred map + TLS
require/verify-ca/verify-full; **v1.x** = OIDC/LDAP federation at gateways with
pooler-issued short-lived DB tokens riding the SCRAM password field (Aurora pattern) +
MCP tool→scope binding; **v2** = SPIFFE/SPIRE workload identity on the pooler→DB hop +
cloud IAM federation. Host-level subjects (PG cluster-role precedent), DB/graph-level
grants; authn failures → security log, in-DB allow/deny → journal.

## 7. Domain neutrality

The DB ships primitives; domains name them. Sufficiency examples (all expressible, none
built-in): evidence-based closure (vector attrs + requirement rules) · approvals (actions
+ k-of-n) · decision inbox (enabled-action index + watches) · recurrence (spawn
transitions + timer) · claims/leases (action + timer-bound release) · review patterns ·
ETL lineage (E_D) · quality gates (requirements on transitions) · risk vetoes (INHIBIT) ·
compliance time-travel (epoch reads + audit).

## 8. Milestones

- **M1 — Graph core + storage + schema graph.** Structural arrays, snapshot/journal/
  manifest, columnar parts skeleton, schema compiler, CLI smoke test.
- **M2 — Semantics engine.** Full algebra, FSMs (finalizing/endless), worklist, timer
  wheel, validation R1–R13, dry-run API, determinism fuzz.
- **M3 — Working database core + full CLI + native wire listeners.** Kernel assembly
  behind the library-mode API (write path, recovery), full CLI in library mode, UDS + TCP
  listeners: framing, handshake, pipelining, cancel, keepalive, resume; concurrent
  subtree-locked writers + sequencer; socket-level protocol acceptance.
- **M4 — External surfaces.** gRPC admin plane + Kotlin SDK + MCP server + REST/WS gateway
  + SPA read views — protocol clients only, wired after the core is proven. TLS-at-origin,
  PROXY v2, first-party pooler prototype, Arrow bulk reads.
- **M5 — Hardening.** Kernel completion, audit export, compaction under load, fuzz suite,
  index-module seam validated (ANN/BM25 post-beta).

## 9. Open questions

- **Q4 — CLOSED.** Containment hierarchy frozen (P6, §5): Host → Database → Graph schema
  (namespace) → forest → node. Cross-schema E_D allowed within a Database; acyclic per
  Database; no cross-Database references.
- **Q10 — CLOSED (2026-11-27).** v1 identity method set confirmed (per
  `research/identity-auth-survey.md` §4, SPEC-02 §4): SCRAM-SHA-256 (+ `scram_plus`
  channel binding on TLS) · client-cert (CN/SAN URI → subject map) · UDS peer-cred ·
  TLS require/verify-ca/verify-full · trust (library mode only). Host-level subjects
  (PG cluster-role precedent); policy table = host config, hot-reloadable, evaluated
  once per connection. Federation (OIDC/LDAP + pooler-issued short tokens) stays v1.x;
  SPIFFE/SPIRE + cloud IAM stays v2.
- **Q12 — CLOSED.** Product: **LatticeDB** (codename `uvdb`). Company: **Emergenta**
  (bare; "by Emergenta" endorsements). Assets registered 2026-11-26: emergenta.io/.dev
  (canonical .dev, .io alias), latticedb.io, GitHub `latticedb-io`. emergenta.com parked
  by reseller — later negotiation target.

## 10. Decision log (condensed)

- **Discussion 1:** evidence reducible (attributes + rules); write domain 3 classes;
  decisions ≡ actions (select-branch semantics); attribute locking (state↔set mapping);
  transition requirements as first-class rules; endless state models; vectors justified
  by wide-domain mission; sample-as-node only when samples need identity/states.
- **Grand Index (pts 1, 2, 10, 11):** catalog→namespace→node-table model; attribute-major
  columnar parts + codecs + zone maps + kernels (D-10.1–D-10.8); Arrow payloads; index
  module seam (ANN/BM25 deferred); no planner.
- **Composed writes (pt 3):** subtree-locked writers + single sequencer; node-local
  validation; confluence guarantees.
- **Event taxonomy (pts 4, 6):** commands / derivations / watches; single emission point.
- **Discussion 3:** closed schema inventory; JSON = string + records; Transition/ActionType
  unified; IndexDef in schema; KeyType dense default.
- **Discussion 4 (D-4.1–D-4.7):** schema transactions + epochs; R13 kernel-enforced
  guarded evolution; absence semantics; loosen/tighten asymmetry; dry-run impact API;
  resumable migration scripts; logical deletion only.
- **Transport:** three sockets; Postgres-v3 framing; TLS-at-origin; PROXY v2; pooler =
  first-party identity gateway.
- **Execution model (2026-11-27):** dedicated OS thread pools in all kernel crates
  (sequencer, workers, merge pool) — no async runtime in core/engine/storage/tx. One
  process: the Host owns its listeners; there is no edge↔kernel IPC hop (forced by v1
  auth — UDS peer-cred and TLS-at-origin identify the client only when the DB process
  terminates the connection — and by P12: gateways are protocol clients, no second path
  into the core). Async session tasks submit commands to the synchronous kernel via
  bounded queues at the sequencing point; gateways/pooler stay external protocol
   clients. Pool sizes/budgets via SPEC-02 §10 config (profiles + autodetect).
- **Pre-implementation decisions (2026-11-27):** wire runtime = tokio, confined to
  `lattice-wire`/`lattice-admin` (SPEC-02 §12.8); global allocator = mimalloc default,
  build-feature-selectable, tunables in §10 config (SPEC-02 §12.9); journal I/O =
  buffered writes + `fdatasync` (default) at the group-commit boundary, O_DIRECT
  deferred behind an M5 evidence gate (SPEC-03 §9.8); Q10 closed — see §9.
- **Sequence re-cut (2026-11-28, owner):** the database core is assembled, exercised and
  fixed before anything external is wired to it. **M3** = kernel assembly behind the
  library-mode API + full CLI + native listeners (UDS/TCP) + socket-level protocol
  acceptance. **M4** = external surfaces only (gRPC admin, Kotlin SDK, MCP server,
  REST/WS gateway, SPA read views, pooler) — all protocol clients (P12). The Kotlin SDK
  and the MCP server move from M3 to M4; their scope is unchanged. Rationale: the
  contract re-read before T-311 (BACKLOG SQ-03, SQ-08) found the kernel crates tested
  only in isolation — no command path through ARCHITECTURE §5.1, no durable journal
  under the sequencer, no CLI — and library mode is a first-class reach distance
  (SPEC-02 §1, WPT-1.1), so the CLI proves the very command semantics the wire later
  carries. Surfaces stay designed-in from day one (P12); only the build order changes.
- **Frozen invariants:** timer-only time · timer wheel owns deadlines/lags · domain terms
  are labels, not entities · business layer owns ergonomics.
- **Naming & assets (2026-11-26):** company **Emergenta** (bare, no suffixes), product
  **LatticeDB**; domains emergenta.dev (canonical) / emergenta.io (alias) / latticedb.io;
  GitHub `latticedb-io`; containment hierarchy Host → Database → Graph schema → forest
  → node (Q4 closed); identity phased vector v1/v1.x/v2 accepted as working model (Q10).

## 11. Glossary

- **U / V** — composition forest axis / dependency DAG axis (UVFlow heritage).
- **Host** — the running instance: listeners, auth policy, subject domain (PG cluster analogy).
- **Database** — isolated storage domain inside a Host: own catalog, journal, node-id space.
- **Catalog** — the meta-schema graph of a Database: types, models, transitions, rules.
- **Graph schema** — a named graph inside a Database; the namespace (Department).
- **Subject** — authenticated principal (user | agent | service) carrying a scope set.
- **Policy table** — host-level ordered auth rules: (listener, subject, source, tls-state)
  → method + scopes (the pg_hba pattern).
- **Command** — outer event; the only non-derivable input (Δstructure, Δattribute, εaction).
- **Derivation** — inner event computed by the engine; replayable, correctness-irrelevant.
- **Watch** — epoch-ordered, resumable egress stream from the journal append point.
- **Transition / action** — schema-defined state motion; with actor classes = invokable action.
- **Requirement rule** — named monotone predicate gating a transition.
- **Attribute lock** — state↔attribute-set writability mapping.
- **KeyType** — named axis definition (dense/sparse, ordinal/temporal) for vector attributes.
- **Part** — immutable columnar unit of attribute data; merged in background.
- **Epoch** — per-node change counter / global snapshot marker; **schema epoch** — schema
  version stamped by journaled schema transactions.
- **Absence semantics** — pre-change cells read added fields as absent; defaults at read.
- **Kernel / shell** — unbypassable in-core checks (scopes R11, evolution R13) vs volatile
  gateway policy.
- **Finalizing / endless** — state-model classes with / without terminal states.
- **Grand Index** — DB-as-index: forest axis + (node,k)-ordered columns + derived indices.
- **Sequencer** — the single journal total-order point above concurrent subtree writers.

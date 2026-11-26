# Schema Evolution Research — Survey of Modern DBs & Consequences

> Discussion 4 deep-dive (Q3): how modern databases — relational, graph, typed-schema,
> append-only — handle schema migrations and updates. Findings + decision proposals
> D-4.1…D-4.6 refining the guarded-evolution proposal. LatticeDB project, 2026-11-25.

## 1. Scope

Our context: schema-as-graph (all schema is data), journal-replay determinism, live nodes
occupying states and holding cells written under earlier schema epochs, no inline
definitions, kernel-enforced validation. Question: what have production systems learned
about changing schemas under live data, and what should LatticeDB adopt vs reject?

## 2. Relational kernels

### 2.1 PostgreSQL
- **Transactional DDL**: schema changes commit/rollback atomically with data (the baseline
  modern kernels converged on).
- **Metadata-only additive DDL**: `ADD COLUMN` with default is O(1) since PG 11 — default
  stored in catalog, materialized at read ("missing = absent, default at read"); no rewrite.
- **Two-phase constraint tightening**: `ADD CONSTRAINT … NOT VALID` (catalog-only, rejects
  new violations) then `VALIDATE CONSTRAINT` (background scan legalizes history) —
  generalizable as *grandfathering*: new rule binds future writes, existing facts tolerated
  until validated.
- **`CREATE INDEX CONCURRENTLY`**: background build with invalid-entry tolerance.
- Lesson: additive must be metadata-only; tightening is a *process*, not a moment.

### 2.2 MySQL 8 (InnoDB)
- `ALGORITHM=INSTANT` add/drop column: metadata-only with per-row versioning; defaults
  applied at read. INPLACE for most else; table rebuilds remain for type changes.
- Lesson: same as PG — the industry converged on "add = instant, change type = rebuild".

### 2.3 Google Spanner
- Schema changes are **online, non-blocking**, applied lazily across servers; each request
  sees a consistent schema version. `NOT NULL` enforcement happens via gradual background
  validation before the constraint "takes".
- Lesson: schema versioning per request; constraint activation as a *state*, not an instant.

### 2.4 CockroachDB
- The canonical **versioned-descriptor state machine** (paper: "Online, Asynchronous, and
  Incremental Schema Changes"): a table descriptor moves through states — `DELETE_ONLY` →
  `WRITE_ONLY` → `BACKFILL` → `PUBLIC` — with old/new versions coexisting; backfills run
  chunked in the background; concurrent traffic valid against both versions.
- Newer declarative schema changer plans changes as job DAGs.
- Lesson: never a global "upgrade moment"; transitions are states with data backfilled
  incrementally. We do not need the machinery in-kernel (our facts pin to epochs), but the
  *discipline* is ours too.

### 2.5 Cassandra / DynamoDB (wide-column, schema-light)
- `ALTER TABLE … ADD` is pure metadata; missing = absent. Dropped columns tombstoned,
  physically purged by compaction later. No backfill concept.
- Lesson: **absence semantics kill most migration problems at the root** — the direct
  ancestor of our "added record fields read as absent".

### 2.6 MongoDB
- Soft schema: optional JSON-Schema **validators** (strict/moderate; error/warn); moderate
  level deliberately *grandfathers* pre-existing invalid documents, validating new writes
  only. Evolution pattern is app-side (`schema_version` field + lazy read-time migration).
- Lesson: write-time validation + grandfathering is a proven pair; retro-validation is a
  burden nobody actually wants to run eagerly.

## 3. Tooling & discipline layer

- **Flyway / Liquibase / Rails / Django / Alembic**: versioned migration scripts with
  checksum + lock tables. Imperative, auditable, but no safety analysis.
- **Atlas (Ariga) / Prisma Migrate / Skeema**: **declarative desired-state** → planner
  computes steps; shadow/dev database validates; **`atlas migrate lint` flags destructive
  and data-loss steps for human review** — safety as a *lint gate in tooling*.
- **strong_migrations (Rails)**: blocks dangerous ops at development time, suggests safe
  alternatives (add nullable, backfill, then enforce).
- **gh-ost / pt-online-schema-change (MySQL)**: shadow-table + change-capture (binlog or
  triggers) + atomic cut-over — copying history under live traffic without locks.
- **Expand–Migrate–Contract / parallel change**: add new shape alongside old → backfill in
  batches → switch readers → contract. The dominant zero-downtime discipline; lives in
  application deploy flow, not the kernel.
- Lesson: safety today is enforced *above* kernels by convention and review. Nobody
  enforces it kernel-side. That is the gap we can close: **our R13 guarded-evolution rule
  is the lint gate made unbypassable**.

## 4. Graph & typed-schema family

- **Neo4j / Memgraph / Neptune (property graphs)**: data plane is schema-light (node
  properties unvalidated centrally); schema objects = constraints (uniqueness/existence/
  type) + indexes, created/destroyed **online** with background population; data-model
  migration = versioned Cypher script tools or batched APOC rewrites. Lesson: they dodge
  the problem by not typing the data plane — we deliberately take the opposite bet, but
  steal the online lifecycle of constraints/indexes.
- **Kùzu**: typed node/rel tables (columnar); ALTER ADD/DROP property DDL; single-writer
  OLAP orientation. Closest storage relative; schema evolution story thin — no live-graph
  safety rules.
- **TigerGraph**: schema-first typed graph via GSQL; attribute addition easy; altering
  existing types rigid (recreate); global schema locks on change. Lesson (negative):
  rigid global semantics punish evolution — avoid global locks; our R13 reference-counting
  is per-reference, not per-universe.
- **SurrealDB**: default schemaless; `SCHEMAFULL` tables with `DEFINE FIELD` (type
  assertions, defaults, permissions validated at write) + `DEFINE EVENT` reactive clauses.
  Lesson: field-assertion-at-write is mainstream now; events exist but are imperative —
  we keep rules declarative and monotone.
- **TypeDB**: the philosophical sibling — **schema is a type graph**, writes validated
  against it; define/undefine transactions. Known pain: stricter schema vs older data →
  queries can hit retro-validation inconsistencies; no rigorous live-data guard.
  Lesson: validate at write, never retro-validate; guard removals with reference counts.
- **TerminusDB**: schema stored **as a graph** with **Git-like commit graph** — schema
  updates are commits; branches, diffs, time travel; document validation at commit.
  Lesson: schema-as-committed-data + time travel is proven; our journal+epochs is the
  linear, deterministic version of their branching model.

## 5. Schema-as-data / append-only family (closest to our journal model)

- **Datomic**: the pioneer — **attributes are entities in the DB** (schema-as-data).
  Evolution rules: cardinality may **loosen only** (one→many, never the reverse without
  new attribute); **attributes are never deleted** (only identity retracts); history is
  immutable; reindexing materializes schema changes. Lesson: **loosen/tighten asymmetry**
  and "deletion is logical, history keeps everything" are battle-tested disciplines.
- **XTDB v2**: append-only bitemporal SQL; **DDL lives in the transaction log** with
  valid-time applicability — schemas are versioned by the log itself.
  Lesson: *direct precedent* for our "schema ops are journal transactions"; they needed
  valid-time intervals only because they expose bitemporal SQL — our linear epochs are
  simpler and sufficient.
- **EventStoreDB ecosystem**: streams immutable; event-payload evolution via **upcasters**
  (read-time transforms) and copy-transform migrations — history never rewritten.
  Lesson: transform-on-read / add-new-event-type beats editing history — same conclusion
  as our "old cells keep orphaned bytes; new rules read absence".

## 6. Synthesis — eight themes

1. **Additive = metadata-only, absence semantics** (PG 11, MySQL INSTANT, Cassandra, Mongo).
2. **No global upgrade moment**: versioned descriptors / schema versions / log-carried DDL
   (CockroachDB, Spanner, XTDB).
3. **Tightening is a process, not a moment**: NOT VALID→VALIDATE, gradual validation,
   moderate validators (PG, Spanner, Mongo).
4. **Safety is lint today, enforced by tooling** (Atlas, strong_migrations) — never
   kernel-enforced anywhere.
5. **Migrations = scripted batches + expand/contract** when reshaping is unavoidable
   (gh-ost shadow-copy or chunked backfill — we only need the chunked script variant).
6. **History is never rewritten**: logical removals, tombstones, upcasters (Datomic,
   EventStore, Cassandra compaction purges physically later).
7. **Graph family dodges via schema-lightness; typed graph DBs suffer evolution** (Neo4j vs
   TigerGraph/TypeDB) — strong typing is rare precisely because evolution is hard; whoever
   solves it kernel-side differentiates.
8. **Schema-as-data precedents exist** (Datomic, TerminusDB, XTDB) but none combine it with
   kernel-enforced live-data guards and deterministic replay. That combination is unclaimed.

## 7. Comparison table (condensed)

| System | Schema model | Change mechanism | Breaking-change safety | Replay/determinism |
|---|---|---|---|---|
| PostgreSQL | catalog DDL | transactional, instant-add, NOT VALID→VALIDATE | none (tooling only) | — |
| MySQL 8 | catalog DDL | INSTANT/INPLACE algorithms | none | — |
| Spanner | versioned schema | online, lazy application | gradual validation | consistent-per-request versions |
| CockroachDB | versioned descriptors | state machine + chunked backfill | none (type change = rebuild) | — |
| Cassandra/Dynamo | sparse columns | metadata-only add/drop | absence semantics | — |
| MongoDB | soft validators | write-time validation, grandfathering | moderate level | — |
| Atlas/Prisma/etc | declarative desired state | planned steps + **lint for destructive ops** | review gates (tooling) | shadow-db testing |
| gh-ost | — | shadow table + capture + swap | manual | — |
| Neo4j | schema-light + constraints | online index/constraint lifecycle | n/a (nothing to break) | — |
| Kùzu | typed tables | ALTER DDL | thin | — |
| TigerGraph | typed schema | GSQL DDL, global locks | rigid | — |
| TypeDB | **schema-as-type-graph** | define/undefine txns | weak (retro-validation pain) | — |
| TerminusDB | **schema-as-graph + commits** | schema commits, branches | commit-level validation | time travel (branching) |
| Datomic | **schema-as-data** | additive; loosen-only; no deletion | loosen-only discipline | immutable history, reindex |
| XTDB v2 | DDL-in-log (bitemporal) | log-carried schema versions | — | log replay |
| EventStore | payload schemas | upcasters, copy-transform | convention | stream immutability |
| **LatticeDB (proposed)** | **schema-as-graph, kernel-validated** | **schema txns + epochs; guarded evolution kernel rule; dry-run impact API** | **R13 unbypassable + scripts** | **bit-identical replay across epochs** |

## 8. Decisions refining Discussion 4

- **D-4.1 — Schema transactions with epochs** (kept, now precedent-backed: XTDB v2, Spanner,
  CockroachDB). No valid-time machinery — linear epochs suffice for our semantics.
- **D-4.2 — Kernel-enforced guarded evolution (R13)** — reference-counted rejections for
  destructive ops. *Differentiator: every surveyed system leaves this to tooling; we make it
  unbypassable in the kernel* — required by semi-autonomous agents (same reasoning as the
  scope kernel, P9).
- **D-4.3 — Absence semantics everywhere additive**: added record fields / attributes read
  as absent on pre-change cells; defaults evaluated at read, never backfilled (PG-11-style).
- **D-4.4 — Loosen/tighten asymmetry** (Datomic generalized): loosening applies immediately;
  tightening binds future writes only — history grandfathered. Combined with immediate-effect
  semantics, this *is* the generalized NOT VALID pattern; no two-phase constraint machinery.
- **D-4.5 — Schema dry-run / impact report**: `plan_schema_txn(ops)` analyzes without
  committing and returns accept/reject per op with live-reference paths and counts
  (affected nodes, requirements, indices). First-class protocol + MCP tool — the Atlas lint,
  made queryable for agents. This answers "what breaks if I remove X?" safely.
- **D-4.6 — Migrations = resumable, idempotent transaction scripts** with epoch checkpoints
  (cursor-read → batched write → checkpoint), executed as ordinary transactions, fully
  audited. Expand/migrate/contract is the recommended pattern (pattern library post-beta).
  No in-kernel backfill state machine, no shadow-table copying, no declarative migration
  engine in v1. Physical purge of orphaned cell bytes happens at part merge (compaction) —
  never in the journal.
- **D-4.7 — Logical deletion only, forever**: schema entities are tombstoned in the schema
  graph; journal history retains everything (required by replay anyway; stated as user-facing
  contract).

## 9. Explicit rejections (with reasons)

- Shadow-table + capture-copy migrations (gh-ost-style) — unnecessary: facts are immutable
  and parts rebuildable; scripts + absence semantics cover the same need.
- In-kernel chunked backfill state machines (CockroachDB-style) — complexity unjustified
  while backfill = plain batched transactions by scripts.
- Retro-validation of historical data (TypeDB pain) — write-time validation only.
- Bitemporal schema intervals (XTDB) — linear epochs suffice.
- Declarative migration DSL in core — deferred with the DSL question (pt 5); dry-run API +
  scripts are the pre-DSL surface.

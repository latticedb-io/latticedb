# Column-Store Research — Comparison & Architectural Consequences

> Refinement point 10 (vector/attribute storage) · LatticeDB project · 2026-11-25
> Status: findings + decision proposals D-10.1…D-10.8; feeds MANIFEST v0.3 and the storage spec.

## 1. Scope

Question: how should LatticeDB store vector ("column") attributes, given the Grand Index
model (DB = forest axis + attribute columns + derived secondary indices; cursors + kernels,
no SQL planner)? Method: survey production columnar/time-series/graph storage designs,
extract transferable mechanisms, reject the rest, refine our architecture decisions.

## 2. Systems surveyed

### 2.1 Apache Parquet (file format)
Row groups → column chunks → pages; encodings (PLAIN, RLE/DICT, DELTA_BINARY_PACKED,
DELTA_BYTE_ARRAY); per-page/chunk **statistics = zone maps** (min/max/null-count); bloom
filters optional; block compression (snappy/zstd).
*Transferable:* zone maps as the universal filter-pushdown primitive; codec registry per
type; stats-based query planning without an index.

### 2.2 ClickHouse MergeTree
**Immutable parts** appended per insert; background size-tiered/logical merges; sparse
primary-key index (every N-th row → granules); **data-skipping indices** (minmax, set,
bloom, tokenbf) as optional aux structures; per-column codecs (Delta, DoubleDelta, Gorilla,
T64); TTL-driven part eviction.
*Transferable:* part discipline (append-only immutable units + merges); skipping indices as
optional secondary structures; per-column codec selection.
*Rejected:* sparse primary-key index — our key order `(node_id, k)` *is* the key; dense keys
make offsets arithmetic, not searches.

### 2.3 DuckDB
Columnar single-file storage; **vectorized execution** over fixed-size value batches;
zone maps; compression incl. FSST (fast string) and ALP (floats); ART index; checkpointing.
*Transferable:* kernel-registry discipline (scan/filter/agg/join as fixed vectorized
operators over batches); modern codec options for strings and floats.
*Rejected:* SQL planner and optimizer machinery (compactness).

### 2.4 Apache Arrow (+Flight)
Standard in-memory columnar layout; zero-copy; cross-language; Flight = columnar streaming RPC.
*Transferable:* make **Arrow-compatible batches the bulk-read payload** on our wire protocol
and SDKs — zero-copy for JVM/Python/Rust consumers; kernel outputs already fit.

### 2.5 LSM family (RocksDB / LevelDB)
WAL → memtable → immutable SSTables → compaction; bloom per SST; prefix compression.
*Transferable:* **WAL duality** — our journal is the WAL (sole authority), column parts are
the mutable-derived layer; CRC + rebuild-from-journal = self-healing storage; compaction
triggers (part count / volume thresholds).

### 2.6 InfluxDB (TSM) / TimescaleDB
Delta-of-delta timestamp encoding; Gorilla float encoding; time-partitioned chunks;
**continuous aggregates** (materialized rollups).
*Transferable:* time-series codecs (into D-10.2); *parked:* materialized rollups — possible
future "computed attribute" kind, not v1. Time partitioning unnecessary for us: per-node
k-runs are the partition unit.

### 2.7 Kùzu (columnar graph DB)
Node/relationship tables stored columnar; CSR adjacency for graph traversal.
*Transferable:* existence proof that **structural CSR + columnar properties** cohabit
efficiently in one engine — the exact split of our Grand Index. Our delta: stateful fixpoint
engine + action journal on top, which Kùzu lacks entirely.

### 2.8 LMDB
mmap B+trees, copy-on-write, **single-writer / lock-free multi-reader**, zero heap objects.
*Transferable:* read-path discipline; with concurrent writers (refinement pt 3) we keep
snapshot semantics through immutable parts + epoch reads instead of COW trees.

### 2.9 Umbra / HyPer (academic)
LSM-tree columnar hybrid, variable-length pages, hot/cold tiering.
*Transferable:* precedent that journal-first + columnar-co-design scales; we adopt the
pattern, skip tiering in v1.

### 2.10 TigerBeetle (ethos)
Fixed-size record encoding, deterministic replay, VOPR fuzz oracle, batching.
*Transferable:* deterministic rebuild requires deterministic encodings — fixed-size cell
formats where possible; fuzz suite already in M5.

## 3. Consolidated lessons

| Lesson | Source | Disposition |
|---|---|---|
| Immutable parts + background merges | ClickHouse, LSM | **Adopt** (D-10.1) |
| Zone maps / min-max stats | Parquet, CH, DuckDB | **Adopt, mandatory** (D-10.3) |
| Per-type codec registry | Parquet, CH, Influx | **Adopt** (D-10.2) |
| Data-skipping secondary indices | ClickHouse | **Adopt as optional** (Grand Index) |
| Vectorized kernels over fixed batches | DuckDB | **Adopt** (D-10.4) |
| Arrow-compatible bulk payloads | Arrow/Flight | **Adopt** (D-10.6) |
| WAL duality: journal = authority, columns rebuildable | LSM, Umbra | **Adopt** (D-10.5) |
| Deterministic fixed-size encodings | TigerBeetle | **Adopt** (D-10.2) |
| CSR adjacency + columnar cohabitation | Kùzu | **Precedent cited** |
| Sparse primary-key index | ClickHouse | **Reject** (dense keys) |
| SQL planner / cost-based optimizer | CH, DuckDB | **Reject** (compactness) |
| Time partitioning per chunk | Influx/Timescale | **Reject** (node-runs suffice) |
| Continuous aggregates / materialized views | Influx/Timescale | **Parked** (future computed-attribute kind) |
| Hot/cold tiering | Umbra | **Reject in v1** |

## 4. Decisions (refinements)

- **D-10.1 — Attribute-major columnar parts.** Base attribute data lives in immutable parts
  keyed by (graph, attribute), cells sorted by (node_id, k) with CSR node-run offsets.
  Background size-tiered merges; merge = pure sort-merge of runs (deterministic, replay-safe).
  *Supersedes* the earlier "per-node segment files" sketch (N2).
- **D-10.2 — Codec registry per scalar type.** Fixed set: delta, delta-of-delta (timestamps),
  Gorilla (float series), dictionary/RLE (low-cardinality, enums, tags), bit-packing (bool),
  zstd block compression at part level. Auto-selected by type; overridable per attribute in
  schema. Encodings deterministic (rebuild-safe).
- **D-10.3 — Zone maps mandatory** per run-block; cursor filters push down into them; bloom
  filters optional on enum/tag columns. If Q1 ends sparse-keyed, add sorted-run index as
  contingency (only mechanism affected).
- **D-10.4 — Kernel registry (closed):** scan, filter (whitelisted forms), aggregate
  (sum/avg/min/max/count/first/latest), merge-join-on-key, latest-per-node, top-k — fixed
  vectorized operators over ~4k-cell batches. No planner; cursors compose kernels.
- **D-10.5 — Self-healing storage.** Parts are rebuildable from journal; CRC per part block;
  open = manifest + snapshot + valid parts + journal replay; corrupted parts rebuilt
  transparently. Journal remains the only authority (P7 preserved).
- **D-10.6 — Arrow-compatible bulk reads.** Cursor fetch payloads = Arrow IPC chunks;
  zero-copy into JVM (Kotlin SDK) / Python / Rust; protocol framing unchanged.
- **D-10.7 — Composed-write integration.** Per-partition staging buffers (concurrent
  subtree-locked writers) → single sequencer journal → background assembler forms parts.
  Mergers run concurrently with epoch-snapshot readers.
- **D-10.8 — Rejected list frozen** (see table): no planner, no sparse primary index, no
  chunk partitioning, no materialized rollups in v1, no core text search.

## 5. Consequence on prior decisions

- N2 (per-node segment files) → superseded by D-10.1.
- Grand Index (pt 1) → concretized: structural SoA/CSR (base) + columnar parts (base data
  layout) + secondary indices (derived views) + kernels (read path).
- Composed writes (pt 3) → gains its storage story (D-10.7).
- Journal = WAL duality sharpens P7 wording: journal is authority; *everything else*,
  including attribute columns, is rebuildable projection or derived view.

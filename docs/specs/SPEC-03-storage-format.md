# SPEC-03 — Storage Format Specification

> LatticeDB v1 · Emergenta · 2026-11-26 · Status: **FROZEN v1.0 — contract**
> All micro-decisions owner-confirmed (§9). Post-freeze changes go through the
> decision-entry flow (architect pair), never silently.
> Constitution: MANIFEST v0.3 + ARCHITECTURE.md. Sibling: SPEC-01 Schema Graph (frozen),
> SPEC-02 Wire Protocol (frozen).
> Normative keywords MUST / MUST NOT / SHOULD / MAY per RFC 2119.
> Byte order: little-endian everywhere. All multi-byte integers are fixed-width.

---

## 1. Scope and determinism requirements

Per-Database storage domain (containment: Host → Database; each Database owns a complete,
independently back-upable file set):

```
<db_dir>/
├── MANIFEST                  atomic pointer: snapshot + parts + journal offset
├── snapshot.v<N>             immutable structural arrays + catalog (mmap-able)
├── journal/
│   ├── 000000000001.log      append-only segments (length + CRC + records)
│   └── 000000000002.log
└── parts/
    ├── 000000000042.lpart    immutable columnar parts (numeric generation naming)
    └── ...
```

**Determinism requirements (the contract this format serves):**
- D1: `encode(decode(x)) == x` for every codec (deterministic encodings).
- D2: replay(journal) on any machine, any process restart ⇒ **bit-identical** state
  (snapshot bytes + catalog bytes + part logical contents).
- D3: merge(parts) is a deterministic function of its input part set (same inputs ⇒ same
  output bytes, given identical codec parameters).
- D4: no wall-clock, hostname, PID, or random values may enter any stored byte (timestamps
  enter only as journal fact fields written by the engine's clock discipline).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SFT-1.1 | replay identity | randomized txn log → replay on 3 fresh machines | SHA-256 of snapshot + logical part contents identical across all | fuzz |
| SFT-1.2 | codec determinism | encode 10k random cells per type | identical input ⇒ identical encoded bytes; decode ⇒ original | unit |
| SFT-1.3 | merge determinism | same 5-part input set merged twice | identical output bytes (D3) | unit |

## 2. MANIFEST — the only mutable file

Content (TOML-serialized, ≤ 4 KiB): `{generation: u64, snapshot_id: u64, parts: [part_id],
journal_segment: u32, journal_offset: u64, schema_epoch: u64, created_lsn: u64}`.

- Swap protocol: write `MANIFEST.tmp.<gen>` → fsync file → fsync directory → atomic
  rename over `MANIFEST` → fsync directory. Readers open MANIFEST before every open/reopen
  and treat it as immutable thereafter (epoch marker).
- Kill-crash at any point leaves either old or new MANIFEST fully valid (never torn);
  stale `MANIFEST.tmp.*` files are garbage-collected at open.
- Part files listed in MANIFEST are alive; parts absent from MANIFEST and older than the
  reader-grace window are deletable.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SFT-2.1 | swap atomicity | kill -9 between write/fsync/rename (1000 loops) | MANIFEST always parses; DB opens at generation N or N+1, zero torn state | integration |
| SFT-2.2 | tmp GC | crash mid-swap → reopen | stale tmp files removed; live set untouched | unit |

## 3. Snapshot format

```
snapshot.v<N> := header | structural_arrays | catalog_blob | part_index | footer
header        := magic "LTSNAP01" | format_version u32 | database_id u32
                 V u64 (nodes) | E u64 (edges) | schema_epoch u64 | created_lsn u64
structural_arrays := SoA blocks per ARCHITECTURE §4/§5:
                 parent[u32·V] | graph_id[u16·V] | node_type[u16·V]
                 state[u8·V] | epoch[u64·V] | attr_count[u16·V]
                 child CSR (offsets u64·(V+1), targets u32)          — composition
                 dep_out CSR (offsets u64·(V+1), edge ids u32)       — I depend on
                 dep_in CSR (offsets u64·(V+1), edge ids u32)        — imposed on me
                 edge table: (src u32, dst u32, kind u8, aux u32)[E]
catalog_blob  := schema-graph serialization (SPEC-01 §16 op-log equivalent, deterministic
                 order), length-prefixed, followed by compiled-mask blob (rebuildable;
                 kept for open speed, ignored if CRC fails → recompile)
part_index    := [part_id u64, graph u16, attr u16, row_count u64, min_node u32, max_node u32]
footer        := CRC32C per 64 KiB block table | total CRC32C | magic tail
```

- Snapshots are **immutable**; a new one appears only at checkpoint (compaction) as
  `snapshot.v<N+1>`; MANIFEST swap flips the pointer; the old file is unlinked after the
  reader-grace window (mmap keeps live readers safe — LMDB discipline).
- Attribute **cell data is not in the snapshot** — it lives exclusively in parts (§4).
  Scalar attributes are columns with fixed `k = 0` (§4.1), so snapshot = structure +
  catalog, parts = all attribute values, journal = all history. Three clean layers.
- Catalog blob validity: if CRC fails, the engine MUST recompile it from journal replay
  (never trust partially); structural array CRC failure ⇒ snapshot unusable ⇒ full rebuild
  from earliest valid backup + journal (§8).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SFT-3.1 | three-layer truth | write workload → checkpoint → reopen | structure from snapshot, values from parts, history from journal; no duplication across layers | integration |
| SFT-3.2 | catalog self-heal | corrupt catalog_blob bytes only | open succeeds; catalog recompiled from journal; compiled masks byte-identical to healthy run | integration |
| SFT-3.3 | structure fatal | corrupt structural array block | open refuses with `SNAPSHOT_CORRUPT` + rebuild instructions; never half-opens | unit |

## 4. Parts — columnar attribute storage

**Part = the unit of attribute truth.** Immutable file per `(graph, attr_id, generation)`:

```
.lpart := header | run_directory | data_blocks | footer
header := magic "LPART01" | version u32 | graph u16 | attr_id u16 | gen u64
          | key_type_ref | codec_chain | cell_count u64 | row_group u32
run_directory := CSR per node-run: (node_id u32, first_k u64, cell_count u32,
                 data_offset u64, presence_offset u64)[runs]
data_blocks   := encoded cells, 64 KiB blocks, CRC32C per block
presence      := per-record-field presence bitmasks (SPEC-01 §5), packed
footer        := zone_maps per run-group (min/max/null_count) | block CRC table
                | part CRC32C | magic tail
```

- **Ordering:** cells strictly sorted by `(node_id, k)`. Dense keys: `k = first_k + i`
  within a run — offsets are arithmetic; sparse keys: explicit `k` array per run.
- **Absence:** per-cell validity bitmap (absent = bit 0) at run level for all types;
  record cells additionally carry the field-presence bitmask (SPEC-01 §5.9).
- **Zone maps** per run-group: min/max (comparable types), null_count — cursor filters
  push down through them (SPEC-02 §8).
- **Scalar = column with fixed k=0** (unification law): scalar overwrite = new cell in a
  newer part; **merge keeps the cell with the greatest journal LSN** per (node, k).
  Tombstones (deletions) are LSN-stamped and suppress older cells until purge.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SFT-4.1 | ordering invariant | fuzz-random appends across 1000 nodes | part cells strictly (node_id, k)-sorted; binary-search lookup exact for every cell | fuzz |
| SFT-4.2 | scalar overwrite resolution | 3 overwrites of one scalar across 3 parts | read = newest LSN value; merge of all 3 ⇒ same single cell | integration |
| SFT-4.3 | zone pushdown | filter query over 1M-cell part | blocks outside zone maps never decoded (assert via block-decode counter); results exact | integration |
| SFT-4.4 | presence survival | record with absent field → merge → read | absence preserved exactly; no phantom defaults materialized | unit |

## 5. Codec registry

| id | Codec | Applies to | Notes |
|---|---|---|---|
| 0x00 | identity | all | fallback, deterministic |
| 0x01 | RLE | low-cardinality | run (count, value) |
| 0x02 | dictionary | enum, string, tag | dict + indices |
| 0x03 | delta | i64, timestamp, u64 ordinals | zig-zag deltas |
| 0x04 | delta-of-delta | temporal keys, timestamps | Influx/Timescale pattern |
| 0x05 | Gorilla | f64 series | float control-bit stream |
| 0x06 | bitpack | bool, small ints | LSB-first packing |
| 0x07 | zstd (block) | any, block level | last stage only |

Codec chain per part: `[value_codec, (frame_codec zstd)?]`, auto-selected by scalar type,
overridable per AttributeDef (SPEC-01 §3/§5). **Default chain:** temporal → delta-of-delta
+ zstd; f64 series → Gorilla + zstd; enums → dictionary + zstd; bool → bitpack; default →
identity unless part bytes > threshold, then zstd. Chains are recorded in the part header;
decoders MUST honor the header, never assume defaults.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SFT-5.1 | chain round trip | every codec × every compatible type, random + edge inputs | decode(encode(x)) == x byte-exact (D1) | fuzz |
| SFT-5.2 | override honored | AttributeDef pins codec identity | part header shows identity; decode uses it even when zstd would be smaller | unit |
| SFT-5.3 | ratio sanity | 1M monotone timestamps | delta-of-delta+zstd ≤ 2 bytes/cell average (sanity bound, informational) | integration |

## 6. Journal segments

```
segment := [record]*
record  := [u32 len][u32 crc32c][body]
body    := [u8 type][u64 lsn][txn_id u64][payload]
types   := BEGIN_TXN | ADD_VERTEX | DEL_VERTEX | ADD_EDGE | DEL_EDGE | SET_ATTR
         | APPEND_CELLS {node, attr, [(k?, val)]} | ROW_APPEND_BATCH {set, rows}
         | ACTION {node, transition, actor, subject, payload?, from, to}
         | SCHEMA_TXN {ops} | GRANT | REVOKE | COMMIT
```

- **LSN:** u64, assigned by the sequencer at journal append, gap-free per Database;
  `COMMIT` carries the txn's `[first_lsn, last_lsn]`. A txn is valid iff its COMMIT is
  present and CRC-valid (no torn txns applied).
- **Rotation:** new segment at `journal.max_segment_bytes` (config, default 256 MiB) or
  on checkpoint; segments never rewritten; truncation of the *active* segment only at
  open (partial trailing record after crash = dropped with warning; committed prefix
  stands).
- **Archival:** segments below the checkpoint horizon MAY be archived (audit export,
  phase v1.x); never silently deleted when `audit_retain = true` (config).
- Recovery scan: open last segment, walk records CRC-validating backwards to the last
  valid COMMIT; everything after is discarded (partial-record tail).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SFT-6.1 | torn txn | kill -9 mid-append, 500 loops | open applies only CRC-valid committed txns; partial tail discarded; subsequent writes continue gap-free LSNs | integration |
| SFT-6.2 | rotation | force segment rotation every 100 txns | reopen stitches segments seamlessly; LSN continuity verified | integration |
| SFT-6.3 | archive retention | checkpoint with audit_retain=true | archived segments present and importable; with false → removed at horizon | integration |
| SFT-6.4 | append-after-crash | reopen after torn tail → write | new LSNs continue without gaps or reuse | integration |

## 7. Compaction and merges

- **Triggers (config):** parts-per-(graph,attr) > `merge.max_parts`, or staging bytes >
  `merge.staging_bytes`, or manual (admin plane).
- **Merge = deterministic k-way sort-merge** over inputs sorted by (node_id, k);
  resolution: greatest LSN wins per cell; tombstones purge older cells; orphaned record
  fields (post-drain) and dropped enum dict entries removed (SPEC-01 §5/§16); absent-cell
  runs collapse.
- Output: one part per (graph, attr) per merge, written to `parts/` → fsync → MANIFEST
  swap (add new, drop inputs) → input unlink after reader-grace. Merge failures leave
  inputs alive (crash-safe by construction — nothing is deleted before the swap).
- Checkpoint: merge-all + snapshot.v<N+1> build + MANIFEST swap + journal horizon advance.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SFT-7.1 | merge equivalence | 100 random parts → merge | logical read-set identical pre/post merge for every (node,k); D3 determinism | fuzz |
| SFT-7.2 | crash mid-merge | kill -9 during merge, 200 loops | inputs intact or swap complete — never half; open always consistent | integration |
| SFT-7.3 | purge correctness | deleted-node cells / drained enum values | gone after merge; journal history still replays them (audit intact) | integration |
| SFT-7.4 | concurrent readers | merge under active cursor | reader results unchanged (epoch snapshot); cursor completes on old parts | integration |

## 8. Corruption and self-healing

| Corruption | Detection | Action |
|---|---|---|
| part block CRC fail | open/fetch | rebuild part from journal (§3 layers); transparent; log `PART_REBUILT` |
| journal block CRC fail in committed txn | open | **refuse start** (`JOURNAL_CORRUPT`) — journal is the authority; operator override `--rebuild-from-snapshot` rebuilds parts/catalog from last good snapshot, losing only post-snapshot txns (explicit, logged) |
| snapshot structural CRC fail | open | refuse + guided full rebuild from archived journal (§3) |
| MANIFEST unparseable | open | fall back to `MANIFEST.tmp` scan or previous generation file if present; else refuse |

**Principle:** the journal is sacred — hardware may corrupt derived structures, and we
heal them; corruption inside the journal itself is never auto-healed silently.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SFT-8.1 | part self-heal | flip bytes in a part block | open logs PART_REBUILT; all reads correct; rebuilt bytes differ only in block CRC region | integration |
| SFT-8.2 | journal sanctity | flip byte inside committed txn | refuse start; `--rebuild-from-snapshot` path works and reports the lossy LSN range explicitly | integration |
| SFT-8.3 | manifest fallback | truncate MANIFEST mid-swap loop | previous generation recovers; no data beyond last COMMIT lost | integration |

## 9. Micro-decisions (owner-confirmed 2026-11-26)

1. **Scalar = column with fixed k=0** — CONFIRMED. A scalar *is* a vector of one element;
   one storage path for all attributes; overwrites resolve by LSN at merge. The
   scalar/vector distinction lives in the **AttributeSet kind** (`scalar_set` |
   `table_set`, SPEC-01 §7) — homogeneous sets, no mixed sets.
2. **Parts keyed per (graph, attr_id)** — CONFIRMED. Finalized hierarchy:
   `(graph_schema → attr_set → Set<attr_id> → columns)`; attr_set is the first-class
   logical entity (SPEC-01 §7 as amended), parts remain physical per attribute column.
3. **LSN per journal record; COMMIT carries txn range** — CONFIRMED.
4. **Journal CRC failure = refuse start (never silent heal)** — APPROVED.
5. **Default zstd stage ON** for parts above size threshold — CONFIRMED.
6. **Torn-tail policy: drop partial trailing record at open, warn** — CONFIRMED.
7. **File naming** — RESOLVED by pattern research: the established convention across
   RocksDB (`000023.sst`), InfluxDB TSM (`000001.tsm`), and PostgreSQL WAL (fixed-width
   numeric segments) is **bare generation numbers, semantics in file headers**. Adopted:
   `parts/000000000042.lpart`, `journal/000000000007.log` (12-digit zero-padded decimal,
   generation = MANIFEST-listed). Graph/attr/gen ride in the part header (already
   specified §4) — never in the filename. Old proposal (`p.<graph>-<attr>-<gen>.lpart`)
   superseded: no escaping/length hazards, sort-order = age order.

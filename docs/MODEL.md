# LatticeDB — the model (the one reading of the contract)

> **Purpose.** One exact reading of the frozen contract, so that no session re-derives
> it — or guesses it — from names. Non-normative: MANIFEST, ARCHITECTURE and SPEC-01…04
> win on any conflict. If this file and the contract disagree, the contract is right and
> this file has a bug — fix the file, and raise a BACKLOG SQ if the contract text itself
> is ambiguous.
>
> **Labels on every statement:**
> **[S]** stated in the contract (citation) · **[D]** derived — a necessary implication,
> with the reasoning · **[O]** open — the contract does not pin it; a BACKLOG SQ owns it.
>
> **Law. A NAME IS NOT A DEFINITION.** If a concept is not in this file, or is marked
> [O], it is a question, not an implementation choice. Ask (BACKLOG SQ) — never infer
> from the name. Two guesses have already been made from names alone (§11).

## 1. Hierarchy and identity [S]

`Host → Database → Graph schema (namespace) → composition forest (many roots) → node`
(MANIFEST P6, ARCH §4).

| Identifier | Scope | Meaning |
|---|---|---|
| `node_id` u32 | Database | ONE id space across all graphs; monotone, never reused |
| `graph_id` u16 | Database | which graph schema (namespace) a node belongs to |
| `attr_id` u16 | Catalog (lineage) | stable across schema epochs; retired ids are never reused |
| `k` u64 | **node** | the node's key value — see §3 |
| `epoch` u64 | node | per-node change counter |
| `LSN` u64 | Database journal | total order of everything committed |
| `schema_epoch` u64 | Database | +1 per committed schema txn |

A Database is an isolated storage domain: own catalog, journal, node-id space. No
cross-Database references.

## 2. Two axes [S]

- **Composition** (parent → children, a forest): inside ONE graph schema; which children
  a node has. Containment follows the type forest: a node under parent `p` has the type
  that `type(p)` declares as its child type (SPEC-01 §11).
- **Dependency** edges (8 kinds: AND, OR, KOFN, GUARD, INHIBIT, XOR, MIRROR, WEIGHTED):
  a DAG per Database, may cross graph schemas (SPEC-01 §10, §15).
- Composition and dependency are different things. A node with children or live
  dependency edges cannot be deleted (`HAS_DEPENDENTS`, SPEC-01 §3).

## 3. The node is a table — one key per node [S]

This is the concept most easily misread. In the contract's own words:

- MANIFEST §1: "scalar + vector attributes (**per-node key axis → a node is a columnar
  table**)".
- SPEC-01 §18.1: "**Single key axis per node** — Node = 'extended table': every SQL table
  has exactly one primary key; `k` is that key; composition of data = a new node type,
  **never a second axis**."
- SPEC-01 §6: "**One key axis per node.** A NodeType declares exactly one KeyType; every
  vector attribute of nodes of that type is indexed by that axis."
- MANIFEST glossary: "**KeyType** — named axis definition (dense/sparse,
  ordinal/temporal) for vector attributes."

So, exactly:

| Term | Meaning | Label |
|---|---|---|
| KeyType | a schema entity `{key, kind: dense \| sparse, axis: ordinal \| temporal}`; every NodeType declares exactly one | [S] SPEC-01 §6, §11 |
| `k` | the node's key value. Dense (default): a **per-node** monotone counter — offsets locate cells, no per-key index. Sparse: explicit ks. `temporal` = k means time; `ordinal` = k is a sequence (this selects semantics and a default codec, not a second storage path) | [S] SPEC-01 §6, §18.2 |
| scalar attribute | `scalar / enum / record` attribute. "A scalar IS a vector of one element": stored as the column's cell at **k = 0** | [S] SPEC-03 §9.1, SPEC-01 §7 |
| vector attribute | `Vector<T>`: a COLUMN along the node's key axis | [S] MANIFEST P5, SPEC-01 §7 |
| cell | the value of one attribute at `(node, k)`; every cell has k — scalars at 0 | [S] SPEC-03 §4 |
| **record** | at one `k` of one node: the cells of ALL that node's vectors at that `k`. Where a vector has no cell at that k, the record holds an **absent** cell | [D] from "one axis per node" + SPEC-01 §6 "unaddressed attributes of the set get an absent cell" |
| `table_set` | an AttributeSet whose members are all vectors: "the node's **table definition**, target of row-append commands" — a named group of the node table's columns | [S] SPEC-01 §7, §18.3 |
| `scalar_set` | an AttributeSet of scalar/enum/record attributes: "the node's metadata row — k=0 columns" | [S] SPEC-01 §7 |
| row append | `ROW_APPEND {node, table_set, rows}`: "k increments once and the addressed vector attributes receive a cell at that k; unaddressed attributes of the set get an absent cell"; a row may omit columns; `k_skip` leaves holes | [S] SPEC-01 §6, §18.3; SPEC-02 §7 |
| point append | `CELLS_APPEND`: `k = next` for the attribute, or explicit `k > current_max`; `k` below current max → `NOT_APPENDABLE` | [S] SPEC-01 §6 |

**What follows from "one axis per node" [D]:**

1. All vectors of ONE node are columns of ONE table, whatever table sets they sit in.
   A node type may declare several table sets and several types may share a set; they are
   all views on the same node table. **The next `k` is one past the greatest `k` of ANY of
   the node's vectors** — a row appended to a second table set continues the node's axis;
   it never starts a second counter. (T-313 implemented one counter per table set; fixed
   — BACKLOG ED-29.)
2. Vectors of one node are ALIGNED on their common key: `log[k]` and `reading[k]` are the
   same record (SPEC-01 test SGT-6.1: "both columns return cells at identical k").
3. An absent cell is not stored. Reading a vector at a k where it has no cell yields
   absence.
4. `k` is append-only (it is a primary key; corrections = a re-run instance).

**[O] — open under this section:** the first `k` of a vector axis (implementation: 0;
the contract does not say whether the first vector row shares k = 0 with the scalar row)
— SQ-14. A point append at exactly `k == current_max` (kernel refuses it; the R12 helper
rejects only `k < max`) — SQ-15.

## 4. Columns and parts [S]

- Attribute data lives in immutable **columnar parts, one per `(graph, attribute)`**,
  cells sorted by `(node_id, k)`, with CSR node-run offsets (SPEC-03 §4, decision D-10.1).
  "Our key order `(node_id, k)` IS the key; dense keys make offsets arithmetic, not
  searches" (column-store research §2.2) — which is why there is no sparse primary index.
- So every attribute is its own COLUMN. A node's multi-column record is not stored as a
  row anywhere: it is assembled by ALIGNING columns on the key `(node_id, k)`.
- A scalar attribute is a column with the single k = 0 (same scheme, one storage path).
- Zone maps per run; a codec chain per type; merge = the greatest LSN wins per
  `(node, k)`; parts are immutable and rebuildable; the journal is the only authority
  (SPEC-03 §4, §7; MANIFEST P7).
- Writes go to the journal and to a staging tail; a checkpoint writes staged cells into
  parts (ARCH §8). A column read is `parts ⊕ staging`.

## 5. The read model: cursors compose kernels [S]

- **Closed kernel set** (decision D-10.4, MANIFEST P14, ARCH §5.2): `scan`, `filter`
  (whitelisted forms), `aggregate` (sum/avg/min/max/count/first/latest),
  `merge-join-on-key`, `latest-per-node`, `top-k` — fixed vectorized operators over
  batches. **No planner; cursors compose kernels.** Fetch frames carry Arrow IPC.
- **Cursor index kinds** (SPEC-02 §8 `CURSOR_OPEN {index …}`; SPEC-04 §4 adds
  `set_presence`): `by_state | attr_presence | set_presence | shared_key | type_scan`.

What each kind means in this model:

| Kind | What it selects | Label |
|---|---|---|
| `type_scan` | the set of nodes of one node type (the "same-type" index) | [D] from P14's index list + SPEC-01 §12 |
| `by_state` | nodes of one type in one state — IndexDef `{type_key}` | [S] SPEC-01 §12 |
| `attr_presence` | nodes holding at least one cell of an attribute — IndexDef `{attr}` | [S] SPEC-01 §12 |
| `set_presence` | nodes whose types reference an attribute set — IndexDef `{set_key}` | [S] SPEC-01 §12 |
| **`shared_key`** | **not a node set — the KEY.** The vectors that share a key: all vectors of a node (one axis), and across nodes the vectors of nodes whose types declare the same KeyType | [D] from SPEC-01 §6 (one axis), P14 / D-10.4 (`merge-join-on-key`), confirmed by the owner 2026-11-28 |

**The `shared_key` cursor is the RECORD cursor [D].** It is the `merge-join-on-key`
kernel applied to the vectors that share a key: it aligns those columns on
`(node_id, k)` and yields **records** — `(node, k) → {column: value}` with absent cells
where a column has none — for one node, or for several nodes combined into one columnar
table keyed by `(node_id, k)`. "All vectors with the same primary key can be fetched as
records with a cursor; if multiple nodes have the same key they can be combined."

The other kernels in this model [D]: `latest-per-node` = the cell with the greatest `k`
of a column, per node (the node's latest row); `top-k` = the greatest/least `k` cells;
`aggregate` = over one column's cells; `merge-join-on-key` has a second use — columns of
DIFFERENT nodes joined on equal `k` (nodes sharing a KeyType).

**[O] — open under this section:** the `shared_key` cursor's parameters (proposed in
BACKLOG SQ-13: key_type · optional column list · node scope; the owner confirms before
T-315 builds it); `order` values (SQ-13 / ED-27).

## 6. Write classes and state [S]

- The write domain is exactly three classes: **Δstructure · Δattributes · εactions**
  (MANIFEST P2). Plus schema transactions and grants, which are their own journaled ops.
- **State is derived, never written.** The only state-moving input is an action on a
  transition. There is no state-write API. A new node starts in its model's `initial`
  state (that is creation, not a write).
- A transition: `from → to`, requirement rules, actor classes, effects (spawn,
  select_branch, cancel_siblings), timing. Actors {human, agent} make it an invokable
  action; {system, timer, guard} are engine-internal. Terminal states are absorbing.
- Validation happens at the sequencing point and is node-local; propagation is confluent
  (ARCH §5.1, P15).

## 7. Time and epochs [S]

- The **timer wheel is the only clock**; wall-clock never enters a predicate or a stored
  byte (P8, SPEC-03 D4).
- "Epoch" means two things (ARCH §4, MANIFEST glossary): the **per-node** change counter,
  and the **global snapshot marker** used by consistent / time-travel reads.

**[O]:** logical time has no journal record, so timer-fired actions cannot survive a
restart yet (SQ-09). The unit of the global epoch marker is not named; the kernel uses
the COMMIT LSN (SQ-16).

## 8. Journal and transactions [S]

- The journal is the only authority; LSNs are gap-free per Database; `COMMIT` carries the
  txn's `[first_lsn, last_lsn]`; a txn is valid iff its COMMIT is present and CRC-valid
  (SPEC-03 §6). Replay is bit-identical (SPEC-03 §1 D2).
- Commands auto-commit by default; `TXN_BEGIN … TXN_COMMIT` joins commands into one
  journal txn (one LSN range); `TXN_ROLLBACK` discards queued commands — "the server
  never holds unvalidated state"; nested begin → `TXN_NESTED` (SPEC-02 §5).

## 9. Words with exactly one meaning

node table · key axis (= KeyType axis, the node's single `k`) · cell · record · row
append · point append · absent cell · table_set · scalar_set · column (= attribute's
part) · part · run (one node's cells within a part) · cursor (a composed kernel pipeline
over batches) · kernel (one operator of the closed set) · index kind (how a cursor picks
what it reads — a node set, or for `shared_key`, the key-aligned columns) · epoch (per-node
counter OR global marker — say which) · LSN · staging (cells written since the last
checkpoint).

## 10. Open concepts — never implement these as settled

Every SQ in `BACKLOG.md`. As of 2026-11-28: SQ-04 control-frame wire contract · SQ-05
PROXY v2 byte order · SQ-06 pooler language · SQ-07 Host → Database · SQ-09 journal time
record · SQ-10 lock-rejection code · SQ-11 `CURSOR_EXPIRED` code · SQ-12 engine memory
model · SQ-13 `shared_key` cursor parameters · SQ-14 first `k` of a vector axis · SQ-15
point append at `k == current_max` · SQ-16 global epoch marker.

## 11. Guess audit — so it is not repeated

| Where | The guess | Why it was wrong | Corrected |
|---|---|---|---|
| T-313 row append | next `k` computed per **table set** | the counter is per **node** — one axis (§3) | fixed; test `one_key_axis_per_node_spans_every_table_set`; ED-29 |
| T-314 selector | `shared_key` = "nodes whose scalar attribute equals a value" | `shared_key` is the record cursor over vectors sharing a node key (§5) | removed from the kernel; SQ-13 rewritten; lands in T-315 after confirmation |
| T-308 wire codec | `SharedKey { key: String }` — a node-set selector with one string | same misreading, one layer up | recorded (ED-30); T-319 re-points the wire onto the kernel |
| T-313 vectors | first vector `k` = 0, chosen silently | not pinned by the contract | declared [O]: SQ-14 |

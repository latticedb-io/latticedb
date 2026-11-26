# SPEC-01 — Schema Graph Specification

> LatticeDB v1 · Emergenta · 2026-11-26 · Status: **FROZEN v1.0 — contract**
> All micro-decisions owner-confirmed (§18). Post-freeze changes go through the
> decision-entry flow (architect pair), never silently.
> Constitution: MANIFEST v0.3 + ARCHITECTURE.md. Companion specs: SPEC-02 Wire Protocol,
> SPEC-03 Storage Format, SPEC-04 MCP Surface.
> Normative keywords MUST / MUST NOT / SHOULD / MAY per RFC 2119.

---

## 1. Scope and model

The **catalog** is the meta-schema graph of a Database. Every typed entity — scalar types,
enums, records, key types, attributes, attribute sets, state models, states, transitions,
node types, index definitions, named rules, roles, grants, graph schemas — exists **only**
as catalog data. There are no inline definitions anywhere: a write request MUST reference
catalog entities by `key`, never declare structure ad hoc.

The catalog itself is stored as graph data and mutated exclusively through **schema
transactions** (§16). Every committed schema transaction bumps `schema_epoch`. The schema
compiler (ARCHITECTURE §7) recompiles masks/read-sets per epoch; replay applies schema
transactions in journal order, so facts validate against the epoch active at their write.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-1.1 | Schema lives in the catalog, not inline | write with undeclared attribute key | rejected `SCHEMA` (R6), DB state unchanged (verify via epoch + node cell count) | unit |
| SGT-1.2 | Schema transactions bump epochs | commit any catalog op | `schema_epoch` increments by exactly 1; journal contains one SchemaTxn record | unit |

## 2. Naming and key grammar

- **Key** (identifier, referenced by writes): MUST match `^[a-z][a-z0-9_]{0,63}$`
  (lowercase, starts with a letter, `snake_case`, ≤ 64 chars). Unique **within its parent
  entity's namespace** (e.g., state keys unique per state model; attribute keys unique per
  node type; record field names unique per record).
- **Display name**: free UTF-8, ≤ 256 bytes, not an identifier, MAY change freely.
- Keys are **immutable after creation** (rename = deprecate + create).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-2.1 | Key grammar enforced | create type with key `9bad`, `Bad`, `a-b` | each rejected `INVALID_KEY`; catalog epoch unchanged | unit |
| SGT-2.2 | Key uniqueness per namespace | create two node types keyed `task` | second rejected `ALREADY_EXISTS`; first unaffected | unit |
| SGT-2.3 | Keys immutable | attempt rename op | no such op exists in the op registry (compile-time), alter rejected | unit |

## 3. Scalar types (built-in, fixed set)

| Type | Backing | Semantics / constraints |
|---|---|---|
| `bool` | u8 (0/1) | |
| `i64` | i64 | signed 64-bit |
| `f64` | IEEE-754 double | MUST NOT be used for monetary values (validator MAY warn; decimal is the money type) |
| `decimal(p,s)` | i128 scaled by 10^s | fixed-point; p ∈ [1,38], s ∈ [0,p]; exact arithmetic only |
| `string` | UTF-8 | ≤ 65536 bytes per value |
| `bytes` | raw | ≤ 1048576 bytes per value (quota system scales later); large payloads stay external by manifest non-goal |
| `timestamp` | i64 | microseconds since Unix epoch, UTC |
| `duration` | i64 | microseconds |
| `node_ref` | u32 | node id; MUST reference an existing node in the same Database at write time (R6); deleting a referenced node MUST be rejected `HAS_DEPENDENTS` |

Built-in scalars are non-deletable and present in every fresh Database.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-3.1 | decimal exactness | write `decimal(10,2)` = 0.1+0.2, read back | cell equals `0.30` exactly (i128 30), no float drift | unit |
| SGT-3.2 | decimal range guard | write value exceeding p digits | rejected `SCHEMA` with range hint | unit |
| SGT-3.3 | node_ref integrity | write ref → delete referenced node without cascade | deletion rejected `HAS_DEPENDENTS`; ref node intact | integration |
| SGT-3.4 | string cap | write 65537-byte string | rejected `SCHEMA` (size), node untouched | unit |

## 4. EnumType

Entity: `{key, values: [value_key]}`, `values` unique, non-empty, ≤ 256 (u8-backed; dict
encoded in parts). `AddEnumValue` is additive. Value removal follows §16 R13 (no live
usage: no cell, no rule param, no default). Enums are referenced by AttributeDef and
RecordType fields.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-4.1 | enum value lifecycle | add value → write cell → attempt remove | removal rejected `R13_IN_USE` (cell references value) | integration |
| SGT-4.2 | enum bounds | declare 257 values | rejected at op validation `SCHEMA` | unit |
| SGT-4.3 | unused value drains | deprecate value → no cells/rules → remove | removal succeeds; historical journal intact; compaction drops nothing but the dict entry maps away | integration |

## 5. RecordType

Entity: `{key, fields: [{name, type}]}` where field type ∈ scalar ∪ enum. Constraints:
flat only (depth 1 — no record fields, no vector fields), 1 ≤ field count ≤ 64, field
names follow §2 key grammar.

**Absence semantics:** each stored record cell carries a **presence bitmask** (u64, one
bit per declared field position). Fields added after a cell was written read as *absent*
in that cell (bit unset). Defaults, if declared on the field, evaluate at read time and
never backfill. Field type changes MUST be rejected (R13); field removal is allowed only
when no NamedRule/transition/index references the field (§16) — orphaned bytes in
historical cells stay until part merge purges them.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-5.1 | absence on added fields | write cell under 2-field record → add field → read old cell | added field reads `absent`, others intact; presence bit unset | integration |
| SGT-5.2 | default-at-read | field with default → old cell read | value returned = default; underlying cell bytes unchanged (verify via CRC) | unit |
| SGT-5.3 | depth guard | declare record with a record field | rejected `SCHEMA` | unit |
| SGT-5.4 | field removal gating | remove field still referenced by a NamedRule | rejected `R13_IN_USE`; after rule removal → succeeds | integration |

## 6. KeyType and vector attributes

Entity: `{key, kind: dense | sparse, axis: ordinal | temporal}`.

- **One key axis per node.** A NodeType declares exactly one KeyType; every vector
  attribute of nodes of that type is indexed by that axis.
- `dense` (default): `k` is a per-node monotone counter; offset arithmetic locates cells —
  no per-key index. `sparse` (explicit only): sorted-run index built per part.
- `axis: temporal` declares k semantics as time (timestamp domain); `ordinal` as sequence.
  This is documentation + default codec selection (delta-of-delta for temporal), not a
  separate storage path.
- **Row append (default):** a command MAY append a *row* — addressing an **AttributeSet of
  kind `table`** (§7): `k` increments once and the addressed vector attributes receive a
  cell at that `k`; unaddressed attributes of the set get an absent cell. **Point append:**
  writing a single attribute uses `k = next` for that attribute or explicit
  `k > current_max` (upsert above max). Writes to `k` below current max MUST be rejected
  `NOT_APPENDABLE` (append-only axis; retro-edit belongs to a re-run/new node).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-6.1 | row append alignment | append row {time[k], value[k]} | both columns return cells at identical k; absent attrs reported absent | unit |
| SGT-6.2 | dense offset addressing | 10k row appends, read latest of 1 attr | O(1)-ish read (assert via op counter budget), correct value | integration |
| SGT-6.3 | append-only axis | explicit k below current max | rejected `NOT_APPENDABLE`; existing cells byte-identical (CRC) | unit |
| SGT-6.4 | sparse KeyType declared | declare sparse axis, write irregular ks | reads return sorted present cells only; dense counter untouched | integration |

## 7. AttributeDef and AttributeSet

- **AttributeDef** `{key, type: scalar | enum | record | vector<T>, required: bool,
  default: literal?, codec_override: codec?, monotonicity: none | increasing | decreasing,
  write_scope: scope?}`. `monotonicity` MUST be set when the attribute is read by guard
  predicates (§13) — writes violating the declared direction are rejected (R6 family);
  this is the structural guarantee behind predicate monotonicity. `write_scope`, if set,
  requires the acting subject to hold that scope beyond the usual write-attribute scope.
- **AttributeSet** `{key, kind: scalar_set | table_set, members: [attr_key],
  write_scope: scope?}` — a **first-class logical entity**, not UI sugar: the unit of
  scope elevation, attribute locking, and indexing. Sets are **shared across node types**
  (e.g., `general_attr_set` {name, description, author} referenced by many types), and a
  node's effective attributes = union of its type's sets. Sets are **homogeneous** —
  `scalar_set` (all members scalar/enum/record: the node's "metadata row" — k=0 columns)
  or `table_set` (all members `vector<T>`: the node's *table definition*, target of
  row-append commands, §6). **Mixed sets MUST be rejected** (`SCHEMA`): homogeneity keeps
  row-append targeting unambiguous and per-set indexing well-defined. Grouping across
  kinds for display/search is the job of **AttributeGroups** (below), never mixed sets.
  NodeTypes reference sets; **attribute locking** = state→set writability mapping on the
  NodeType (§11). Node state motion (transitions) changes the writable set automatically —
  locks are a consequence of state, never edited directly.
- **AttributeGroup** `{key, display_name, members: [attr_key]}` — a **UI-only entity**:
  categorizes attributes for SPA presentation and search (e.g., "generic", "lengths",
  "sensor-signals"). Groups carry **zero engine semantics** — no scope, no lock, no index
  behavior; the validator enforces key grammar + uniqueness only. Engine code MUST NOT
  branch on group membership (enforced by the closed op registry: no rule/transition/index
  may reference a group).
- **Indexing over sets:** `set_presence` (§12) answers "all nodes whose types use
  `general_attr_set`" / "nodes carrying all of {name, description, author}" — attribute
  sets, not groups, are the indexing basis.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-7.1 | monotone write guard | attr `monotonicity: increasing`, write lower value | rejected `NOT_MONOTONE`; higher value accepted | unit |
| SGT-7.2 | scope elevation | subject without `set.write_scope` writes member attr | rejected `FORBIDDEN`; with scope → accepted; both denials+allows journaled | integration |
| SGT-7.3 | lock follows state | type maps state A→set₁ writable, B→set₂; transition A→B applied | write to set₁ attr now rejected; set₂ accepted — no manual lock op exists | integration |

## 8. StateModel and States

Entity: `{key, class: finalizing | endless, states: [State], initial: state_key}`.
**State** `{key, name, success_terminal: bool, failure_terminal: bool}`.

- `finalizing`: MUST declare ≥ 1 success terminal and ≥ 1 failure terminal (aggregation
  well-definedness). Participates in AND/OR/k-of-n as source and target.
- `endless`: MUST declare **no** terminals; cycles among states legal; motion only by
  transitions (actions/guards/timers). Endless-typed nodes MUST NOT be sources/targets of
  boolean aggregations (AND/OR/k-of-n edges to/from them rejected at insert, R1 family);
  GUARD, MIRROR, WEIGHTED edges are allowed.
- `initial` MUST reference a declared, non-terminal state.
- **Ranks (backward prohibition is a fixed feature, not a flag).** For `finalizing`
  models the transition graph reachable from `initial` MUST be a **DAG** (validated at
  schema commit); each state's `rank` = longest-path depth from `initial`. Every
  transition MUST go to a strictly greater rank — backward motion is structurally
  impossible (R4). Backward *need* is served by the **re-run pattern**: a new instance
  (sibling or spawned child) with fresh state; per-instance history stays linear and
  absorbing terminals stay absorbing. **Cycles** are legal only in `endless` models
  (§8, fact-driven A→B→C→A).
- **Model shape guidance (SHOULD, not MUST):** branching is legal wherever the real
  process branches — mid-chain forward branching (e.g., `running → review | autoapprove`)
  is valid because every branch is a strictly forward path. Models SHOULD stay as linear
  as the process demands; anything that wants to go backward belongs in a re-run instance
  or an endless sibling node, never in backward edges.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-8.1 | terminal class rules | finalizing model without failure terminal | rejected `SCHEMA` | unit |
| SGT-8.2 | endless purity | edge AND from endless-typed node | insert rejected `INVALID_KIND_PARAMS`; GUARD edge to same node accepted | unit |
| SGT-8.3 | cycle legality in endless | transitions A→B→A | accepted; fact-driven round trip A→B→A via actions succeeds twice | integration |
| SGT-8.4 | backward prohibition | finalizing model with transition to equal/lower rank | rejected at schema commit `BACKWARD_RANK`; no runtime retraction machinery exists | unit |
| SGT-8.5 | rank assignment | diamond model A→B, A→C, B→D, C→D | ranks: A=0, B=C=1, D=2; all transitions strictly increasing | unit |

## 9. Transition (the central object)

Entity: `{key, model, from, to, requirements: [rule_key], actors: [human | agent |
system | timer | guard]⁰..., payload_type: record_key?, timing: {lag_ms?, deadline_ms?,
on_deadline: transition_key?}, effects: [effect]⁰...}`.

- **Requirements** — NamedRules (§13); a transition is *enabled* iff all hold at
  evaluation; enabled-ness feeds the built-in enabled-action index.
- **Actors** — non-empty ⇒ the transition is invokable as an **action** by those actor
  classes; `guard`/`system`/`timer` never appear alone in a user-invokable set. Empty
  actors ⇒ engine-only transition (guard/timer armed).
- **Timing** — `lag_ms`: minimum delay after arming before the transition MAY fire
  (guard lags, SLA slack). `deadline_ms` + `on_deadline`: if not fired within deadline
  after arming, the engine fires the `on_deadline` transition (typically to a failure
  terminal or a degraded state of endless models).
- **Effects** (v1): `spawn {instance_of: node_type}` — creates a child instance under the
  acting node; `select_branch` — decision semantics, cancels XOR siblings (validated:
  acting type has XOR edges); `cancel_siblings`. Engine-applied structure ops are journal
  facts authored by the engine — monotone-safe.
- A fired transition appends an **Action journal record**: `{lsn, node, transition,
  actor_class, subject, payload?, from, to}` — the only way state motion enters history.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-9.1 | requirement gate | action with unmet requirement | rejected `REQUIREMENTS_UNMET` listing failing rules; enabled-action index shows disabled | unit |
| SGT-9.2 | requirement met → state motion | fill attributes to satisfy rules, apply action | state = `to`; journal contains Action record with actor+payload; enabled index flips | integration |
| SGT-9.3 | deadline path | transition armed with deadline 100 ms + on_deadline | after expiry engine fires on_deadline (author=timer); original transition now rejected (from-state left) | integration |
| SGT-9.4 | lag respects slack | guard-armed transition lag 15m | fires no earlier than +15m (± timer granularity) | integration |
| SGT-9.5 | spawn effect | action with spawn(instance_of) | child node exists under actor, typed correctly, journal shows engine-authored AddVertex in same txn | integration |
| SGT-9.6 | select_branch cancels siblings | decision node with XOR edges, action select A | branch A enabled, siblings cancelled; cancellation reasons recorded | integration |
| SGT-9.7 | terminal absorbing | any action targeting a terminal-state node | rejected `TERMINAL` (R5) | unit |

## 10. EdgeKind registry (fixed)

`AND · OR · KOFN · GUARD · INHIBIT · XOR · MIRROR · WEIGHTED` — engine constants, not
catalog entities. Per-kind params (validated R1): KOFN `{k: u32, weights?}`, GUARD
`{sigma: state_key, tau: transition_key, lag_ms?, deadline_ms?, predicate: rule_key?}`,
INHIBIT `{activation: on_source_final | while_source_active}`, MIRROR `{filter?}`,
WEIGHTED `{weight: decimal(6,4)}`, XOR `{}`, AND `{}`, OR `{}`. New kinds are new engine
versions — reducibility discipline (P4) applies at design review, not in catalog.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-10.1 | param range | KOFN k=3 over 2 sources | rejected `INVALID_KIND_PARAMS` | unit |
| SGT-10.2 | weights normalization | WEIGHTED edges on one target summing to 1.0001 | rejected; summing to 1.0000 accepted | unit |
| SGT-10.3 | guard param resolution | GUARD sigma=unknown state | rejected `SCHEMA` (unresolved reference) | unit |

## 11. NodeType

Entity: `{key, display_name, parent_type: type_key?, state_model: model_key,
attribute_sets: [set_key], key_type: keytype_key}`.

- Type forest: `parent_type` forms a tree (acyclic, single inheritance); node containment
  follows the type forest exactly (UVFlow rule): node with parent p ⟹ `type(p) =
  node_type.parent_type` — enforced at node creation.
- Effective attributes = union of `attribute_sets` members (plus inherited sets from
  parent chain, first declaration wins on conflicts).
- Writability map compiled per (type, state) from all sets × state-lock mapping.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-11.1 | containment enforcement | create child of type T2 under node of type T1 where T2.parent=T3 | rejected `CONTAINMENT` | unit |
| SGT-11.2 | inheritance | child node sees parent-type attribute sets | write to inherited attr succeeds; attr not shadowed by child sets | unit |
| SGT-11.3 | type cycle | parent_type chain A→B→A | rejected `SCHEMA` | unit |

## 12. IndexDef

Entity: `{key, kind: by_state | attr_presence | set_presence, params}` — declared,
rebuildable derived views (never journaled). `action_enabled` index is built-in, not
declarable. `set_presence {set_key}` indexes nodes whose types reference an attribute set
(§7) — the basis for "all nodes with name/description/author" queries. Drop/recreate is
safe anytime; rebuild source = journal.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-12.1 | by_state correctness | 100 nodes, mixed states, index query | result set equals manual filter exactly; index drop+rebuild reproduces identical result | integration |
| SGT-12.2 | rebuild safety | corrupt index bytes | query still correct after auto-rebuild; CRC violation logged | integration |

## 13. Predicate forms (whitelist) and NamedRules

Allowed monotone forms (v1 registry — closed; each form documents its monotone direction;
attributes referenced MUST declare compatible `monotonicity` §7):

1. `attr_cmp {attr, op ∈ eq|ne|gt|gte|lt|lte, value}` — monotone per attr direction
2. `attr_present {attr}`
3. `cell_count_gte {attr, k}`
4. `cell_field_count_gte {attr, field, value, min}` (record/enum tag counting)
5. `latest_cmp {attr, op, value}` — monotone attrs only
6. `epoch_gte {attr, age_us}` — staleness via journal time (not wall clock)
7. `children_state {terminal_class: success|failure, expect_all: bool}` — aggregation form
8. `set_member {attr, values[]}`
9. `str_eq {attr, value}` / `str_prefix {attr, value}`

**NamedRule** `{key, form, params}` — reusable; referenced by transitions, GUARD
predicates, INHIBIT activations. Free-form code MUST be rejected (`NON_MONOTONE_PREDICATE`,
R7).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-13.1 | whitelist closed | rule with unknown form / embedded expression | rejected `NON_MONOTONE_PREDICATE` | unit |
| SGT-13.2 | monotone attr pairing | rule `latest_cmp` over non-monotone attr | rejected at rule creation `SCHEMA` | unit |
| SGT-13.3 | wall-clock ban | attempt time-based rule via non-catalog means | no such form compiles; only `epoch_gte` (journal time) accepted | unit |
| SGT-13.4 | children aggregation | rule `children_state(success, all)` over 3 children | transitions false→true exactly when 3rd child reaches success terminal | integration |

## 14. Roles and Grants (in-Database authorization)

- **Role** `{key, scopes: [read_subtree | write_structure | write_attribute |
  invoke_action | administer]}`.
- **Grant** `{role, subject (host-level id string), target: graph_key | node_ref}` —
  journaled; every allow/deny decision lands in the journal next to the operation (P9).
- Subjects themselves live at Host level (policy table); the catalog holds bindings only.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-14.1 | grant scope enforcement | subject with read-only grant invokes action | rejected `FORBIDDEN`; journal contains deny record | integration |
| SGT-14.2 | subtree binding | grant scoped to subtree S | write inside S allowed; sibling subtree rejected | integration |
| SGT-14.3 | revoke mid-flight | revoke while action in flight | in-flight txn completes (validated at sequencing); next command denied | integration |

## 15. Graph schema entity

`{key, display_name, created_at}` — unique per Database (§2 grammar). Composition edges
MUST stay inside their graph; dependency edges MAY cross graphs within the Database
(global acyclicity per Database, R3). No cross-Database references exist.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-15.1 | cross-graph dependency | dep edge from graph A node to graph B node | accepted; acyclicity check global | integration |
| SGT-15.2 | cross-graph composition | composition edge across graphs | rejected `MULTI_PARENT`/`CONTAINMENT` | unit |
| SGT-15.3 | cycle across graphs | dep cycle spanning 2 graphs | second edge rejected `CYCLE` with offending path | integration |

## 16. Schema transactions and guarded evolution (R13)

**Op registry (closed):** CreateGraph, DropGraph, CreateEnum, AddEnumValue,
DeprecateEnumValue, RemoveEnumValue, CreateRecord, AddRecordField, RemoveRecordField,
CreateKeyType, CreateAttribute, AlterAttributeCodec, CreateAttributeSet,
CreateStateModel, AddState, AddTransition, AlterTransitionTiming, DeprecateState,
RemoveState, CreateNodeType, DeprecateNodeType, CreateIndex, DropIndex, CreateRule,
RemoveRule, CreateRole, Grant, Revoke.

**R13 guarded-evolution rejections** (checked against live-reference counts at op
validation): remove occupied state; remove attribute referenced by any rule/transition/
index/set; remove enum value with live cells/rule params; alter KeyType of a populated
axis; alter record field type; re-parent a type with existing nodes; deprecate the model
of a live type. **Discipline: deprecate → drain → remove.** Additive ops are free
(absence semantics §5). Tightening binds future writes only. Deletion is logical forever.

**Dry-run:** `plan_schema_txn(ops)` performs full validation without commit and returns
per-op accept/reject with live-reference paths and impact counts (nodes, rules, indices).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-16.1 | R13 matrix | each destructive op against a live reference | every case rejected with rule id + path; catalog epoch unchanged | integration |
| SGT-16.2 | drain then remove | deprecate → migrate live nodes via script → remove | removal succeeds; journal history intact; replay reproduces all epochs | integration |
| SGT-16.3 | dry-run fidelity | plan(ops) then commit(ops) | plan accept/reject set identical to actual commit result | unit |
| SGT-16.4 | tightening grandfathering | tighten requirement while nodes in prior states hold violating data | existing states unaffected; next transition blocked until data fixed; no retro-validation | integration |
| SGT-16.5 | replay across epochs | random ops+facts log → replay on fresh node | bit-identical state incl. catalog (determinism oracle) | fuzz |

## 17. Bootstrap and template bundles

A fresh Database contains: built-in scalar types (§3), the fixed edge-kind registry (§10),
empty user catalog, zero graphs, `schema_epoch = 0`. **The kernel ships no business
content** (P11, domain neutrality) — initialization responsibility lives with the SDK/CLI.

**Template bundles** — versioned, ordered batches of schema transactions shipped with
SDK/CLI releases (the migration-tooling pattern, never kernel-seeded):

- `process-mgmt` — work/task lifecycle models, review patterns, spawn templates (states primary);
- `state-driver` — minimal driver models for BI / technological state engines;
- `analytics` — dataset/series patterns (vectors primary, states secondary).

Mechanics: bundles are idempotent (re-install = no-op), resumable via epoch checkpoints,
immutable once published (a new template version = a new bundle), and installable only by
a subject holding `administer` scope. **Active meta-schema mode:** the engine does not
distinguish "fixed after init" from "continuously evolved" catalogs — any subject holding
`administer` mutates the catalog via schema transactions whenever needed (R13 guards live
references; dry-run plans the change). Determinism holds across arbitrary schema history
(SGT-16.5), so a tenant-managed, constantly evolving catalog is a supported mode, not an
exception.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| SGT-17.1 | fresh DB shape | create database, dump catalog | exactly built-in scalars + registry; zero graphs; epoch 0 | unit |
| SGT-17.2 | bundle idempotence | install bundle twice | second run is a no-op (all entities exist); state unchanged; epoch grew only once | integration |
| SGT-17.3 | bundle install correctness | install `process-mgmt` | all declared types/models/transitions exist; sample graph instantiable per bundle docs | integration |
| SGT-17.4 | active meta-schema | tenant (administer scope) evolves catalog over service lifetime | schema txns succeed across epochs; live facts unaffected; replay reproduces full history | integration |

## 18. Micro-decisions log (owner-confirmed 2026-11-26)

1. **Single key axis per node** — CONFIRMED. Node = "extended table": every SQL table has
   exactly one primary key; `k` is that key; composition of data = a new node type, never
   a second axis.
2. **Append-only `k`** — CONFIRMED. `k` is the primary key of the node-table; PKs are
   immutable; corrections = re-run instance.
3. **Row append** — CONFIRMED, refined: row-append targets an **AttributeSet of kind
   `table_set`** (vector-only set = the table definition); payload may omit columns
   (absent cells); `k` advances once per row.
4. **decimal(p,s), p ≤ 38** — CONFIRMED. Matches i128 backing; exceeds SQL Server
   DECIMAL(38,s) and IEEE-754 decimal128 (p=34); covers ISO-20022 money amounts (≤ 18
   digits) with headroom. Industry-standard for money.
5. **Finalizing requires both terminal classes** — CONFIRMED. Models are linear-forward
   with legal mid-chain branching (forward paths); no "branching only at the end"
   restriction — real processes branch where they branch (§8 guidance added).
6. **Backward motion / cyclic SMs** — RESOLVED by research (§8): backward is structurally
   impossible in finalizing models (rank = longest-path depth, strictly increasing
   transitions, fixed feature — no flag, per owner); cycles legal only in endless models;
   backward *need* served by the re-run pattern. No retraction machinery ever exists.
7. **Monotonicity flag on attributes** (`increasing|decreasing|none`, required when a
   guard predicate reads the attribute) — CONFIRMED (owner: "keep flag"). Gates attribute
   write direction so predicate monotonicity stays provable; R7's structural guarantee.
8. **Effects v1 `{spawn, select_branch, cancel_siblings}`** — CONFIRMED. All three are
   live patterns from the base report (3.4 race resolution, 3.5 decision records, 5.2
   instance expansion); `cancel_siblings` reuses `select_branch` machinery.
9. **Bootstrap via template bundles (not kernel seed)** — CONFIRMED, refined by owner:
   SDK/CLI ship **versioned template bundles** for the usage-case families
   (process-mgmt / state-driver / analytics); advanced deployments (UVFlow-style) run
   **active meta-schema mode** — continuously evolved catalogs via schema transactions
   (§17). Kernel stays empty per P11 + domain neutrality.
10. **Presence bitmask (u64, ≤64 fields)** — CONFIRMED. Industry-standard absence
    mechanism (PostgreSQL row null-bitmaps, Arrow validity buffers, protobuf presence);
    alternatives (sentinel values, per-cell epoch tags, side tables) are strictly worse.

## 19. Post-freeze decision log

- **DEC-01 (2026-11-26, architect pair):** attribute-set finalization. Sets are
  first-class logical entities (scope + locks + indexing basis), homogeneous
  (`scalar_set` | `table_set`, mixed rejected), shared across node types; **AttributeGroup**
  added as a UI-only catalog entity with zero engine semantics; `set_presence` IndexDef
  kind added. Rationale: owner analysis — sets drive logic/indexing; groups drive
  presentation. Affected sections: §7, §12.

# BACKLOG — ideas parked out of V1 scope

> Entry format: `BL-NN · title · rationale · origin (architect/owner/agent)`.
> Anything entering here MUST NOT be implemented in V1 without an architect-pair
> decision that amends the contract (MANIFEST §10 decision log + spec DEC entry).

---

**BL-01 · Replication seam (v2 target) · owner + architect pair, 2026-11-27**

v1 excludes replication (MANIFEST §4: no distributed multi-master; per-graph sharding
seam documented, no work) — but the architecture must not foreclose it. The seam, as
already shaped by the frozen contract:

- **Replicas = journal followers.** Journal = only authority (P7) + bit-identical replay
  (SPEC-03 §1 D1–D4, SFT-1.1) ⇒ replication protocol = journal shipping; **LSN is the
  replication offset**; failover = promote the follower with the latest durable LSN.
  Single-writer sequencer (P15) is the primary's identity; replicas are read-only
  projections.
- **Watches are the CDC feed** (P16: epoch-ordered, resumable, filterable, single
  emission point at the journal append).
- **Determinism invariants are the replication contract.** Implementation constraints
  that must never erode (mechanically enforced by M2 property tests: replay identity,
  permutation convergence): (a) all derived state rebuildable from the journal — never
  persist non-rebuildable state as authority; (b) zero wall-clock in semantics (P8);
  (c) byte-deterministic serialization and codecs (SPEC-03 D-invariants) so replica
  snapshots are byte-comparable; (d) no hidden process-local state in kernel crates
  (`lattice-core` zero-I/O rule).

Any v2 replication design starts from this seam; no V1 work is authorized.

---

## Spec questions (owner decisions pending — no silent drift)

**SQ-01 · RemoveAttribute op missing from the §16 registry · found T-209 ·
owner decision required.** SPEC-01 §16's closed op registry (28 ops) has
RemoveEnumValue / RemoveRecordField / RemoveState / RemoveRule — but NO
RemoveAttribute (and no DeprecateAttribute). Yet the R13 text explicitly
contemplates "remove attribute referenced by any rule/transition/index/set" as
a rejection reason, which presupposes an attribute-removal path. Current v1
behavior: attributes are eternal (created, codec-altered, never removed) —
consistent with P11 compactness but in tension with the R13 text. Options:
(a) add DeprecateAttribute + RemoveAttribute ops (full lifecycle symmetry);
(b) declare attributes eternal in §16 and reword R13; (c) defer to v2.
Owner call required; any choice = DEC entry.

**SQ-02 · No AlterNodeType / SetAttach op — set-list evolution requires type
replacement · found T-209 · owner decision required.** The §16 registry has
CreateNodeType + DeprecateNodeType but no op to alter a live type's
attribute_sets (attach a new set to an existing type). SPEC-01 §7's active
meta-schema promise ("tenant mutates the catalog whenever needed") currently
forces the deprecate → drain → create-replacement cycle for what should be an
additive change (SPEC-01 §16: "additive ops are free"). Options: (a) add
AlterNodeType {type_key, add_sets, remove_sets} with R13 reference counting;
(b) keep type replacement as the only path and document the migration pattern.
Affects SGT-17.4 ergonomics directly.

**SQ-03 · Data-plane listener has no task (M3 mapping gap) · found T-311 · owner
decision required.** ARCH §13 M3 = `wire` (UDS+TCP full); MANIFEST §8 = "UDS + TCP".
TASKS.md T-301–T-310 delivered codec/state-machine libraries only: `lattice-wire` has
no tokio dependency (SPEC-02 §12.8 assigns it one), no accept loop, no session manager,
no production `CommandSink` (only the test `FakeSink`), and the Host CLI has no `serve`.
Test docs deferred socket-level variants to "T-304's session layer", then to "T-310's
listener" — neither built one. Consequences: M3 exit criteria "pipelined sessions" and
"MCP tool demo" unreachable; T-311/T-312 cannot test against a live server; ED-02 cannot
resolve. Options: (a) new task — listener + session manager + engine adapter + `serve`,
then a WPT socket-level pass (ED-12), sequenced before T-311/T-312; (b) build T-311 on
codec-level mirrors now, live interop later. Any choice = TASKS.md change.
**Resolution (2026-11-28, owner):** option (a), after the core — listeners T-319, WPT
socket-level pass T-320; DEC in MANIFEST §10 "Sequence re-cut".

**SQ-04 · Client-visible control-frame wire contract is unpinned · found T-311 ·
owner decision required.** SPEC-02 §2 says TLV tags are "per §7–§10", but no section
numbers the control-plane tags; the Rust side takes unpacked scalars (`process_hello`) and
its tests call AUTH a "placeholder". Unpinned: (1) TLV tags for HELLO / PARAMS / AUTH /
READY / RESUME / CANCEL; (2) credit renewal — §6 requires the client to renew, the closed
type registry has no WINDOW_UPDATE, Rust grants credit only in-process, so a wire client
stalls after the 64-frame default; (3) `session_secret` — CANCEL requires it, READY never
issues it; (4) ERROR frame — §7 text is `[u16 code][hint][detail]`, Rust emits TLV
request_id + hint string with no code (`CommandSink::execute` flattens errors to
`String`); (5) TXN_BEGIN carries no request_id and gets no reply; TXN_NESTED / NO_OPEN_TXN
surface as Rust `Result` values, not ERROR frames; (6) PING/PONG carry a raw u64, not
TLV; (7) HELLO has no `database` field although the policy table (§4) matches on it (P6).
No SDK or listener can be written without these. Decision = DEC entries in SPEC-02 §12.
**Prerequisite (2026-11-28): must be decided before T-319 (listeners).**

**SQ-05 · PROXY v2 length read little-endian · found T-311 · defect, owner decision
required.** `handshake.rs::parse_proxy_v2_header` reads the 16-bit address length as LE;
the PROXY protocol spec (haproxy.org `proxy-protocol.txt` §2.2) defines it in network
byte order (reference code uses `ntohs`). A standard sender emits `00 0C`; the parser
reads 3072 and fails `ProxyV2Malformed`. The T-302 unit test passes because its fixture
is built with `to_le_bytes()`. Also: a TLV tail after the address block is rejected
instead of skipped, and LOCAL (`0x20`) health-check connections are rejected. SPEC-02 §2
"little-endian everywhere" governs LatticeDB framing; the PROXY header is an external
protocol on the MANIFEST §6 path (L4 LB → LatticeDB). Options: (a) DEC — PROXY v2 follows
the upstream byte order; fix parser + fixtures before T-319 (live PROXY passthrough) and
T-402; (b) keep LE and require LatticeDB-specific proxies.

**SQ-06 · Pooler implementation language · found T-311 · doc contradiction, owner
decision required.** ARCH §3 places `lattice-pooler/` in the Rust workspace ("sidecar
binary … depends: wire") and names only `sdk-kotlin`, `gateway-mcp`, `gateway-rest` as
Kotlin repos. TASKS.md T-405 depends on T-311 (Kotlin SDK); STATE.md described the pooler
as Kotlin. Options: (a) Rust per ARCH §3 as written (reuses the `lattice-wire` codec and
SCRAM client); (b) Kotlin, via DEC amending ARCH §3.

**SQ-07 · Host → Database (P6) has no task · found T-311 · owner decision required.**
ARCH §4: `database_id` "assigned at CREATE DATABASE"; P6 / MANIFEST §5: one Host hosts
many Databases (own catalog, journal, node-id space). The repo has `database_id` only as a
snapshot-header field: no database registry, no create/open/list, one database directory
per process, no wire field naming the target database. Options: (a) V1 task (Host
database registry + `database` in HELLO) — milestone to be chosen; (b) V1 = one Database
per Host, recorded here as a declared deferral.

**SQ-08 · Kernel never assembled: no Database facade, no full CLI · found T-311
re-read · owner direction given 2026-11-28.** The kernel crates are tested in isolation,
but no code path runs a command through the ARCH §5.1 spine. Evidence: (1) `JournalSink`
is implemented only by `MemoryJournal` — the sequencer has never written the durable
journal (`sequencer.rs:35` defers the adapter to "the Host binary"); one journal txn per
fact, private `encode_fact` bytes; (2) `FactPayload` carries AttrSet / CellsAppend /
CreateNode / DeleteNode only — no actions, edges, RowAppend, grants or schema txns, and
`MachineGate` only liveness-checks CreateNode / DeleteNode (`gate.rs:101`: "replay through
the arrays path in the host"); (3) the gate does not call `lattice-validate` (R1–R13),
does not run propagation to quiesce, and emits no watches; (4) the M1 exit smoke
(`smoke.rs`) imports core/schema/storage only — it bypasses engine, validate and tx;
(5) the determinism oracle covers storage + schema replay, not engine + tx; (6)
`latticedb-cli` is an empty binary that prints its version, while ARCH §2 defines it as
"ops and smoke tests; library mode + wire mode". **Owner direction:** core + full CLI
first, listeners next, external surfaces (gRPC admin, Kotlin SDK, MCP, REST/WS, SPA,
pooler) last. MANIFEST §8 / ARCH §13 placed the SDK and MCP inside M3.
**Resolution (2026-11-28, owner):** sequence re-cut recorded in MANIFEST §10 and
ARCH §13; kernel assembly T-313 (delivered, in review), read path T-314, full CLI T-316;
the untested kernel spec IDs found by the coverage audit (TASKS.md) are closed by T-313,
T-317, T-318.

**SQ-09 · Logical time has no journal record — timers cannot survive a restart ·
found T-313 re-read · owner decision required.** P8 / ARCH §5.4: time enters only
through the timer wheel; "deadlines and lags are journal facts; re-derivable at replay".
But SPEC-03 §6's closed record-type list has no time-advance record, `Machine::logical_us`
is never persisted, and timers are armed at `logical_us` on state entry. Consequences: after
a restart the wheel is empty and `written_us` of restored cells is 0, so `epoch_gte`
predicates and lag/deadline timing diverge from the pre-restart process. T-313 therefore
keeps the timer path out of the journaled write path (no `advance_time` on the kernel) and
does not re-arm timers at open. For the same reason a graph's `created_at_us` ("a
journal-authored timestamp fact") is still supplied by the caller of `SCHEMA_TXN`. Options: (a) DEC adding an `ADVANCE_TIME {to_us}` record
to SPEC-03 §6; timer firings then enter the write path as ACTION records (author=timer);
(b) stamp journal time on every record and derive. Any choice = DEC in SPEC-03.

**SQ-10 · No wire code for a state-locked attribute write · found T-313 · owner
decision required.** SPEC-01 §7/§11: attribute locking is a consequence of state
(SGT-7.3) but the closed code registry (SPEC-02 §7) has no code for it and SGT-7.3 names
none. T-313 maps the rejection to `FORBIDDEN` with a "state lock" hint so agents are not
told to request a grant for a lock. Options: (a) keep `FORBIDDEN` + hint; (b) DEC adding a
dedicated code (e.g. `ATTR_LOCKED`).

**SQ-11 · `CURSOR_EXPIRED` has no code in the closed registry · found T-314 · owner
decision required.** SPEC-02 §8 says an idle cursor is server-side GC'd and a later fetch
gets `CURSOR_EXPIRED` — but the §7 registry (closed) lists no such code, and T-308's
private value for it collides (ED-20). The kernel reports an expired or unknown cursor as
`NOT_FOUND` with an "expired after N µs idle" hint and a malformed cursor request as
`INVALID_KEY`. Options: (a) keep the mapping and reword §8; (b) DEC adding cursor codes.

**SQ-12 · The engine holds every cell in memory · found T-314 · owner decision required.**
ARCHITECTURE §5.2 / P14 put attribute truth in columnar parts read lock-free (P1: mmap);
the engine's working state is a `BTreeMap` of ALL cells (`Machine::cells`) because
predicates (`attr_cmp`, `latest_cmp`, `cell_count_gte`, …) evaluate over cells. T-314 made
cursors read parts ⊕ staging (so parts are read truth and parity with the engine store is
a tested invariant), but memory still scales with data. Options: (a) v1 keeps the
in-memory store as the engine's working set and states it in ARCHITECTURE; (b) the engine
reads through the column store (a real re-architecture; needed for OLAP-lite sizes).

**SQ-13 · `shared_key` cursor: meaning and parameters · found T-314, corrected by the
owner 2026-11-28 · parameters need owner confirmation.** SPEC-02 §8 lists `shared_key`
among the cursor index kinds without defining it. Two implementations GUESSED, both
wrong: T-308's TLV decoder (`SharedKey { key: String }` — a node-set selector) and T-314
("nodes whose scalar attribute equals a value" — removed from the kernel). The model
(`docs/MODEL.md` §5): every other index kind selects a NODE SET; `shared_key` is about
the KEY — the vectors that share a node's key (one KeyType axis per node, SPEC-01 §6)
and, across nodes, the vectors of nodes whose types declare the same KeyType. The
`shared_key` cursor is the RECORD cursor — the `merge-join-on-key` kernel (D-10.4) over
those columns, yielding `(node, k) → {column: value}` records with absent cells, for one
node or for several nodes combined into one columnar table keyed by `(node_id, k)`.
**Proposed parameters (owner to confirm before T-315 builds it):** `key_type` (selects
the nodes by their type's KeyType) · optional column list (default: the vector
attributes effective for those nodes) · node scope (one node / subtree / every node of
that key type) · filter · order. To be pinned in SPEC-02 §8 by DEC.

**SQ-14 · First `k` of a vector axis · found on the model re-read 2026-11-28 · owner
decision required.** The kernel numbers a vector's first row `k = 0`; scalars sit at
`k = 0` too (SPEC-03 §9.1). The contract does not say whether the first vector row shares
`k = 0` with the scalar row or starts at `k = 1`. Options: (a) keep 0 and state it; (b)
vectors start at 1 so `k = 0` is only ever the scalar row.

**SQ-15 · Point append at exactly `k == current_max` · found on the model re-read
2026-11-28 · owner decision required.** SPEC-01 §6 rejects ks "below current max" and
allows "explicit `k > current_max`"; `k == max` is unnamed. `Validators::r12_append_only`
rejects only `k < max`; the kernel refuses `k <= max` (append-only: the top cell is never
overwritten). Decide and pin one rule.

**SQ-16 · The global epoch marker's unit · found on the model re-read 2026-11-28 · owner
decision required.** "Epoch" is both the per-node change counter and "the global snapshot
marker" (ARCHITECTURE §4). The contract does not name the marker's unit; the kernel uses
the COMMIT LSN of the last unit a read saw. Confirm, and pin it in SPEC-02 §8
(`at_epoch` takes an LSN).

---

## Engineering debt register (tracked deferrals from M0/M1 implementation)

> Format: `ED-NN · item · origin · target gate`. Each item must be resolved or
> re-confirmed at its target gate; the register is reviewed at every milestone tag.

**ED-01 · Autodetect disk-class probing is stubbed (`Hdd` conservative) · T-001 · M4
packaging.** Host probes CPU + RAM only; disk class needs M4 deployment tooling
(cgroup-aware RAM accounting also lands there). Config-level determinism (WPT-10.4)
already covered by `HostSpecs` being an explicit input.

**ED-02 · WPT-10.3 / WPT-10.6 wire-level variants deferred · T-001 · T-320.**
Config-level semantics fully tested now; the behavioral halves (journal equivalence
under extreme tunables; live keepalive vs restart-required `max_frame`) need the wire
stack and land with the WPT socket-level pass.

**ED-03 · Compiler recompiles from scratch per epoch · T-103 · M2/M5 performance gate.**
ARCH §7 says "the compiler rebuilds masks incrementally". Current `compile()` is a pure
full rebuild (deterministic, tested for recompile identity). Incremental rebuild is a
performance optimization — measure first via the admin metrics plane, then optimize if
schema-epoch churn is hot.

**ED-04 · CSR mutations are O(n) vector inserts/removes · T-101 · M5 compaction-under-
load gate.** Correct + invariant-tested; staging buffers and amortized structures
(slot allocation, free lists) are a measured-optimization concern, per P11 and the
no-hardcoded-law (sizing via config).

**ED-05 · Deleted nodes keep their array slots (tombstones, never reclaimed) · T-101 ·
M5.** Memory is reclaimed only via snapshot rebuild. Acceptable while node churn is
low; slot compaction must be replay-deterministic — design note required before any
change (determinism oracle gate).

**ED-06 · Enum-deprecation registries are catalog-level maps · T-104 · M1 review.**
`deprecated_enum_values` / `deprecated_states` / `deprecated_node_types` live beside
the catalog rather than inside entities — keeps SPEC-01 §2–§15 entity shapes frozen.
Serialize with the catalog snapshot (T-106); revisit placement only via DEC entry.

**ED-07 · Schema-txn validation clones the full schema state per op · T-104 · M2/M5
performance gate.** `apply_op` works on a trial clone so rejected ops never pollute
subsequent validation and every accepted prefix compiles (P3). Schema txns are rare;
measure via admin metrics before optimizing (delta-apply or undo-log).

**ED-08 · R13 checks are catalog-level; live-cell counts belong to the tx engine ·
T-104 · M2 (T-207 validator suite).** Schema-level R13 scans cover catalog references
(rules, defaults, transitions, grants, locks). Cell-level usage (enum values in live
cells, occupied states) is enforced by the engine's R6/R13 validators at sequencing —
the two layers compose; the split is documented here so the M2 suite tests both.

**ED-09 · zstd frame codec behind a cargo feature, default off in M1 · T-108 ·
T-107/M5 performance gate.** Codec id 0x07 is registered and chain-parsing honors it
(header-driven — SPEC-03 §5 "decoders MUST honor the header"); with the feature off,
default chains ship frame=None (deterministic per build; part headers record the
actual chain, so cross-build decode stays correct). Activate zstd at the parts task or
M5 when the ratio/latency evidence demands; SFT-5.3's ratio bound is met by varint
zig-zag delta-of-delta alone in the interim.

**ED-10 · Compiled-mask blob is not persisted in snapshots; open always recompiles ·
T-106 · M5 open-speed gate.** SPEC-03 §3 keeps a compiled-mask blob "for open speed,
ignored if CRC fails → recompile". v1 writes the blob as absent and recompiles from
the catalog blob deterministically (recompile identity is already acceptance-tested,
T-103). Persisting the compiled bytes is an open-latency optimization — measure first.

**ED-11 · Actor-class model research: the MachineOnlyActorSet restriction ·
owner-flagged 2026-11-27 · DISCUSSION AFTER M5 GATE.** SPEC-01 §9 currently pins:
a transition with ONLY machine classes (system/timer/guard) in its actor set is
schema-rejected (`MachineOnlyActorSet`) — engine-driven motion must declare
EMPTY actors. This bit implementation twice (state-driver bundle T-112; dispatch
fixture T-202). Owner observation: "we now have some pinned actors" — the model
conflates two orthogonal concerns: (a) WHO MAY INVOKE (authorization — human/
agent) and (b) WHAT ARMED THE TRANSITION (engine mechanics — guard/timer/system).
Research questions for the post-M5 discussion: should (b) move to a separate
`arming` field (transition declares its driver; actors declare only invokers)?
Should system/timer actors be first-class invokables for agent workflows (a
system service invoking an action)? Impact: schema entities (§9), op registry,
compiler masks (actor mask), dispatch law, journal Action records (actor_class),
MCP tool bindings. Any change = DEC entry + SPEC-01 §9 amendment; NOT
implementable inside V1.

**ED-12 · SPEC-02 integration-level WPTs accepted in codec/unit form · T-301–T-309 ·
M3 gate.** Binding test level per SPEC-02 tables vs what runs: 2.1, 2.2, 2.3 (integration)
run against the encoders with a fake peer; 4.1, 4.2 (integration) run against
`AuthEngine` in-process; 5.1–5.6, 6.1, 6.2 (integration) run against `Session` with
`FakeSink`; 7.x, 8.x, 9.1, 9.3 (integration) run against a fake `CommandExecutor`; WPT-1.1
(transport matrix) has no test at all. Resolution (2026-11-28, owner): the socket-level
pass over live connections is T-320; binding test levels stay as written.

**ED-13 · WPT-5.2 expected outcome not asserted by the test carrying its ID · T-304 ·
M3 gate (Supervisor to verify).** `wpt_5_2_txn_frames` (wire crate) exercises frame handling against
`FakeSink`; "TXN of 3 commands, middle invalid → none applied, single LSN range absent,
error names the failing command" is not asserted there.
`failing_command_mid_txn_is_error_frame` asserts that a command before the failing one
stays applied inside an open txn — the opposite of §5.2 atomicity; an artifact of
`FakeSink` executing immediately. Resolution (2026-11-28): kernel form delivered in T-313
(`lattice-tx` `wpt_5_2_txn_atomicity_none_applied_and_error_names_the_failing_command`),
wire form in T-320.

**ED-14 · Node-type codes were not lineage-stable · found T-313 · fixed in T-313.**
`Machine::new` / `recompile` assigned node-type codes by catalog enumeration order, so a
type added later with an earlier-sorting key re-typed every existing node, and nothing
persisted the key→code map that snapshots (`node_type[u16·V]`) rely on. Fix: code = index
of first appearance in the committed schema-op history (`SchemaState::node_type_codes`),
rebuildable from the op-log; graph ids follow the same rule.

**ED-15 · Machine-local state is outside the snapshot format · found T-313 · M5 kernel
completion (T-506) or DEC.** `guard_params` (GUARD sigma/tau), the timer wheel and
`logical_us` live only in `Machine` memory; SPEC-03 §3's snapshot carries structure +
catalog only. T-313 rebuilds guard params at open by scanning the full journal for
`ADD_EDGE` records of live GUARD edges — correct only while the journal is never
truncated. Timers/time: SQ-09. A durable home (snapshot field or DEC) is still needed.

**ED-16 · Journal horizon cannot advance · found T-313 · M5.** `recover()` starts at LSN 1
and fails with `LsnGap` if early segments are missing, so the kernel never retires
segments: checkpoints rotate and swap the MANIFEST but `audit_retain` is effectively always
true. Needs a horizon-aware recovery (start at `MANIFEST.created_lsn`) before SFT-6.3's
"removed at horizon" branch can hold end-to-end.

**ED-17 · Spawn effect skips containment (R2) · found T-313 · T-317/T-318.** `apply_action`'s
`spawn` creates the child without checking `instance_of.parent_type` against the acting
node's type (SPEC-01 §11 "enforced at node creation"); the shipped bundles and the SGT-9.5
fixture declare root types only. Decide: schema-commit-time check (P3) or runtime
`CONTAINMENT`.

**ED-18 · Wire command layer is placeholder-grade · found T-313 · T-319.**
`lattice_wire::commands` decodes unknown/empty values as `Bool(false)` (silent fallback,
Rust rule 3), maps every engine error of ATTR_SET to `NOT_MONOTONE` and every LINK failure
to `CYCLE`, passes GRANT/REVOKE through as no-ops, never dispatches ROW_APPEND, and takes
the LSN as a caller-supplied argument (the sequencer must assign it). T-319 re-points the
wire dispatch onto the kernel command API and deletes this path.

**ED-19 · State codes are declaration-order indices · found T-313 · T-317/T-318 (verify).**
`RemoveState` on a model would shift the codes of later states and re-type live nodes the
way ED-14 did for node types. Verify R13 occupancy checks make it unreachable, or make
codes lineage-stable.

**ED-20 · Wire-layer error codes collide with the closed registry · found T-313, extended
T-314 · T-319.** `lattice-wire/src/watches.rs` defines `WATCH_NOT_FOUND = 0x0317` and
`BAD_WATCH_PARAMS = 0x0318` — the registry (SPEC-02 §7) assigns those values to
`FRAME_TOO_LARGE` and `CANCELLED`. `lattice-wire/src/reads.rs` defines `CURSOR_EXPIRED =
0x0313`, `CURSOR_NOT_FOUND = 0x0314`, `BAD_CURSOR_PARAMS = 0x0315`, `NOT_READABLE =
0x0316` — the registry assigns them to `TXN_NESTED`, `RESUME_EXPIRED`,
`VERSION_MISMATCH`, `CAPABILITY_MISMATCH`. Not yet fixed; T-319 maps them onto registered
codes (the kernel already does: unknown/expired cursor → `NOT_FOUND`, malformed cursor
request → `INVALID_KEY`) or raises a DEC for new codes.

**ED-21 · Core value codec mis-decoded five types · found T-313 · FIXED in T-313.**
`lattice-core::value_codec::decode_value` read its fixed-width integers from the padding
of a scratch buffer: every `I64` / `F64` / `Timestamp` / `Duration` decoded as 0, `NodeRef`
panicked, and `Decimal` advanced the cursor 16 bytes too far. No test round-tripped those
tags (storage tests use typed columns). Fixed with per-tag tests
(`value_codec.rs` unit tests). Any journal or record cell written before the fix would
have decoded wrong — none exist outside tests.

**ED-22 · Part builder could not build default chains · found T-313 · FIXED in T-313.**
`PartBuilder::finish` encoded the per-run LSN column with the VALUE column's codec, so a
dictionary / bitpack / Gorilla chain failed with "codec does not apply to column type
I64" — i.e. no string, bool or float part could be written with its default chain. Every
existing part test used an identity chain. The LSN column now takes the I64 value codec;
regression test `lattice-storage` `part_default_chains.rs`.

**ED-23 · Empty op-log replay committed a phantom schema txn · found T-313 · FIXED in
T-313.** `opslog::replay_ops(&[])` committed an empty txn: one recorded schema txn and an
epoch bump for a fresh catalog. Surfaced when the kernel reopened a database whose initial
snapshot has no ops. Fixed in `replay_ops`; test in `lattice-schema` `lineage.rs`.

**ED-24 · Lock table read a poisoned mutex as "nothing held" · found T-313 · FIXED in
T-313.** `SubtreeLockTable::{stats, is_held, held_count}` returned defaults on poison and
`acquire` skipped the stats update silently (Rust rule 3). They now return
`TxError::LockPoisoned`, and `acquire` hands back the keys instead of succeeding uncounted.

**ED-25 · Explicit-txn concurrency is deliberately simple in v1 · T-313 design ·
T-319/T-320 to exercise.** One explicit txn per database; its commands run on a
session-private clone of the engine state (O(state) per BEGIN); commit is refused
`ORDER_CONFLICT` if any other unit reached the live state meanwhile. Correct and simple;
throughput of concurrent explicit txns was never a requirement in the spec — revisit with
evidence once the listener runs real sessions.

**ED-26 · Kernel reads are serialized with writes, not lock-free · T-314 design ·
T-319.** ARCHITECTURE §5.2 / §8 describe lock-free epoch-snapshot readers. T-314 reads run
on the sequencer thread between committed units: perfectly consistent (WPT-8.1 holds by
construction) and fine for the single-session CLI, but a read blocks writes and vice
versa. A lock-free read path (an immutable epoch view shared with reader threads) is
needed before concurrent sessions exist.

**ED-27 · Cursor v1 limits: no `order`, one column, no snapshot across fetches · T-314 ·
revisit with the listener.** SPEC-02 §8 gives `CURSOR_OPEN` an `order` parameter; the
kernel cursor is `(node, k)`-ascending only and has no order parameter (nothing is
silently ignored — the parameter does not exist yet). A cursor pages ONE attribute column
(the wire layer's multi-column cursor can open one per column). Each fetch sees the
latest committed state; cells of the node being paged are not re-read, so a cursor is not
a snapshot across fetches.

**ED-28 · Parts are read into a cache, not mmap'd · T-314 · M5.** P1 names mmap reads.
Parts are `read()` + parsed into an in-process cache on first use (dropped at checkpoint);
correct, costs memory. The checkpoint also parses every part twice (cells, then the read
cache). mmap and a single parse are hardening work with evidence.

**ED-29 · Row append advanced one counter per TABLE SET · T-313, found on the model
re-read 2026-11-28 · FIXED.** `EngineState::row_ks` took the next `k` from the addressed
set's members only. The contract's counter is per NODE — one key axis per node
(SPEC-01 §6, §18.1; `docs/MODEL.md` §3) — so a row appended to a second table set must
continue the node's axis. Invisible in every earlier test because no fixture had a node
type with two table sets. Fixed; the fixture gained a second table set (`scores`) and
`one_key_axis_per_node_spans_every_table_set` fails on the old logic.

**ED-30 · Wire cursor kinds were guessed from names · T-308, found T-314 · T-319.**
`lattice-wire/src/reads.rs` models `shared_key` as a node-set selector carrying one
string (`CursorKind::SharedKey { key: String }`). Per `docs/MODEL.md` §5 it is the record
cursor over vectors sharing a node key. T-319 re-points the wire cursor kinds onto the
kernel read API; until then the wire codec's `shared_key` is wrong by construction.


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


---

## Engineering debt register (tracked deferrals from M0/M1 implementation)

> Format: `ED-NN · item · origin · target gate`. Each item must be resolved or
> re-confirmed at its target gate; the register is reviewed at every milestone tag.

**ED-01 · Autodetect disk-class probing is stubbed (`Hdd` conservative) · T-001 · M4
packaging.** Host probes CPU + RAM only; disk class needs M4 deployment tooling
(cgroup-aware RAM accounting also lands there). Config-level determinism (WPT-10.4)
already covered by `HostSpecs` being an explicit input.

**ED-02 · WPT-10.3 / WPT-10.6 wire-level variants deferred · T-001 · M3 (T-30x).**
Config-level semantics fully tested now; the behavioral halves (journal equivalence
under extreme tunables; live keepalive vs restart-required `max_frame`) need the wire
stack and land with M3 acceptance.

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

**ED-10 · Compiled-mask blob is not persisted in snapshots; open always recompiles ·
T-106 · M5 open-speed gate.** SPEC-03 §3 keeps a compiled-mask blob "for open speed,
ignored if CRC fails → recompile". v1 writes the blob as absent and recompiles from
the catalog blob deterministically (recompile identity is already acceptance-tested,
T-103). Persisting the compiled bytes is an open-latency optimization — measure first.


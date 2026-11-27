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


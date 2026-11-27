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


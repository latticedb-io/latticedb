# LatticeDB

> **A multi-graph, schema-typed, stateful graph database with scalar and vector
> attributes, deterministic derived state, and kernel-enforced schema evolution —
> the database itself is the index.**
>
> By [Emergenta](https://emergenta.dev)

**Status: M0–M2 delivered, M3 in progress.** The constitution (`MANIFEST.md` v0.3),
architecture, and all four specs are frozen (v1.0); changes go through decision entries.
The kernel crates (core, schema, storage, engine, validate, tx) are built and tested;
M3 assembles them into a working database behind a full CLI before the wire listeners
and any external surface are wired on (MANIFEST §10, sequence re-cut 2026-11-28).
Progress is tracked in [`TASKS.md`](TASKS.md).

## What it is

LatticeDB is a compact, standalone, general-purpose graph database built around a
**Grand-Index architecture**: a composition forest, a dependency DAG, columnar
attribute parts, and derived secondary indices. Reads go through cursors composing
a closed kernel set — there is no query planner.

Its defining property: **state is always derived, never written.** The write domain
has exactly three classes — Δstructure · Δattributes · εactions — and everything
else (node states, aggregates, indices, compiled masks) is a deterministic
projection of an immutable snapshot plus a replay journal.

### Key ideas

- **State is derived, never written** (P2) — actions are the only state-moving
  input; per-node state machines are schema data.
- **Schema is a graph, compiled** (P3) — types, transitions, rules and policies
  live as kernel-validated graph data; a compiler turns them into dispatch masks
  checked at runtime; schema changes are dry-runnable journaled transactions.
- **The journal is the only authority** (P7) — recovery replays to bit-identical
  state; every other structure is a rebuildable, self-healing projection.
- **Scalar + vector attributes** (P5) — per-node key axis turns a node into a
  columnar table; enums and flat records; no JSON type (string + decompose pattern).
- **Fixed edge algebra** (P4) — AND, OR, k-of-n, GUARD, INHIBIT, XOR, MIRROR,
  WEIGHTED; monotone predicates; confluent propagation over concurrent
  subtree-locked writers (P15).
- **One time source** (P8) — the engine's timer wheel; wall-clock predicates are
  structurally impossible.
- **Unbypassable kernel, volatile shell** (P9) — scopes and guarded evolution
  enforced in-core; gateways stay thin protocol clients (P12).
- **Surfaces from day one** — native wire protocol (UDS/TCP), Kotlin SDK,
  first-party pooler, MCP tools for AI agents, REST/WS for humans.

### Non-goals (v1)

No SQL, no query planner, no general OLAP engine, no JSON type, no document
store, no state-write API, no arbitrary user code, no wall-clock predicates,
no ANN/BM25 in core, no multi-master distribution. Full list: `MANIFEST.md` §4.

## Documentation

| Document | Content |
|---|---|
| [`MANIFEST.md`](MANIFEST.md) | Constitution: identity, pillars P1–P16, data model, decision log, glossary |
| [`ARCHITECTURE.md`](ARCHITECTURE.md) | Components, crate layout, data flows, concurrency, security, milestones |
| [`docs/specs/SPEC-01-schema-graph.md`](docs/specs/SPEC-01-schema-graph.md) | Schema graph: entities, FSMs, transitions, evolution (frozen v1.0) |
| [`docs/specs/SPEC-02-wire-protocol.md`](docs/specs/SPEC-02-wire-protocol.md) | Native wire protocol: framing, handshake, sessions, cursors, watches (frozen v1.0) |
| [`docs/specs/SPEC-03-storage-format.md`](docs/specs/SPEC-03-storage-format.md) | Storage: snapshot, journal, parts, codecs, determinism oracle (frozen v1.0) |
| [`docs/specs/SPEC-04-mcp-surface.md`](docs/specs/SPEC-04-mcp-surface.md) | MCP tool surface for AI agents (frozen v1.0) |
| [`research/`](research/) | Supporting surveys: column stores, schema evolution, identity/auth |
| [`TASKS.md`](TASKS.md) | The task ledger — the only source of implementable work |
| [`BACKLOG.md`](BACKLOG.md) | Ideas parked outside V1 scope |

## Roadmap

| Milestone | Scope |
|---|---|
| **M0** | Bootstrap: repo skeleton, CI, config subsystem |
| **M1** | Graph core + storage + schema graph; CLI smoke test |
| **M2** | Semantics engine: full algebra, FSMs, timer wheel, R1–R13, determinism fuzz |
| **M3** | Working database core: kernel assembly behind the library-mode API, full CLI, native wire listeners (UDS/TCP); concurrent writers |
| **M4** | External surfaces (protocol clients only): gRPC admin plane, Kotlin SDK, MCP gateway, REST/WS gateway, SPA read views, pooler, Arrow bulk reads |
| **M5** | Hardening (Supervisor-gated): compaction, fuzz expansion, audit export |

## Repository layout

Rust workspace per `ARCHITECTURE.md` §3: `crates/lattice-core` (pure in-memory
kernel, zero I/O), `lattice-schema`, `lattice-storage`, `lattice-engine`,
`lattice-validate`, `lattice-tx`, `lattice-wire`, `lattice-admin`,
`lattice-pooler`, and the `latticedb` Host binary; `cli/`, `fuzz/`, `tests/`.
Kotlin SDK and gateways live in separate repos, speaking the wire protocol only.

## Development process

Development is spec-locked: every task comes from `TASKS.md` with cited spec
sections and acceptance test IDs; every test is a real functional test; changes
that could affect replay determinism must pass the determinism oracle
(`SPEC-03` §1) before merge. Spec changes require a decision entry — no silent
drift. See `AGENTS.md` for the working rules.

## License

Apache-2.0 — see [LICENSE](LICENSE). Copyright 2026 Emergenta.

## Links

- Site: [latticedb.io](https://latticedb.io)
- Company: [emergenta.dev](https://emergenta.dev)

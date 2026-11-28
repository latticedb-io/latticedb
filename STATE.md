# STATE — session handoff (volatile; durable rules live in AGENTS.md)

> Updated: 2026-11-28 · Read this first after any restart, then continue from "Next".

## Where we are

M0–M2 tagged (`m0-bootstrap`, `m1-graph-core`, `m2-semantics-engine`). M3 in progress
under the **re-cut order** (MANIFEST §10 "Sequence re-cut", 2026-11-28): workable DB
core + full CLI first → listeners → external surfaces last (Kotlin SDK, MCP, gRPC admin,
REST/WS, SPA, pooler).

T-301..T-310 are codec/unit level against fakes (BACKLOG ED-12); the kernel was never
assembled (BACKLOG SQ-08). **T-313 kernel assembly — in_progress**, implementation
starting. Nothing of the SDK (T-311) exists; it moved to M4.

## Decided this session (2026-11-28)

- **T-311 toolchain (owner):** Gradle Kotlin DSL · JDK 21 LTS · coroutines-first ·
  JUnit 5 + kotlin-test. Valid whenever the SDK (now M4) is scheduled.
- **Working rule (owner):** re-read MANIFEST / ARCHITECTURE / specs before every task
  and refine the live docs → AGENTS.md rule 10.
- **Sequence (owner):** core + full CLI → listeners → external surfaces last.

## Pending owner decisions (BACKLOG "Spec questions")

- **SQ-04** control-frame wire contract — before T-319 (listeners).
- **SQ-05** PROXY v2 length byte order — before T-319 / T-402.
- **SQ-06** pooler implementation language — before T-405.
- **SQ-07** Host → Database (P6): V1 task vs declared deferral.

## Next (in order = TASKS.md M3 ID order)

1. **T-313 kernel assembly** — re-read done (rule 10): ARCH §5.1/§5.3/§8, SPEC-02 §5,
   SPEC-03 §1/§3/§6, SPEC-01 §9/§16. Design: one mutation primitive —
   `plan(command) → records` (read-only validation) and `apply(record, lsn)` shared by
   live execution AND journal replay; write path plan → journal → apply → quiesce →
   publish → sync; explicit txns on a session-private scratch.
2. T-314 read path → T-315 secondary indices → T-316 full CLI → T-317 / T-318 kernel
   semantics completion → T-319 listeners → T-320 WPT socket-level pass.

## Environment facts

- Repo `/home/sergey/dev/latticedb`, remote `origin` = SSH
  `git@github.com:latticedb-io/latticedb.git`.
- `~/dev/sdk-kotlin` does not exist; `gh` is logged out (the owner creates remotes).
- JDK 21 + 25 installed; Gradle 9.9.2 cached in `~/.gradle/wrapper/dists`; no
  `gradle`/`mvn`/`kotlinc` on PATH.
- Roles: owner + lead agent = architect pair; lead agent = engineer + QA; Supervisor =
  separate sessions, review-only (deferred to the V1 gate so far).

## DoD reminder (per AGENTS.md)

spec refs cited → acceptance tests green → Supervisor sign-off → TASKS.md updated →
no unplanned features. Every test is functional (data/state assertions), never "exit 0".

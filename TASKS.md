# TASKS — LatticeDB V1 (the ledger)

> Contract: MANIFEST v0.3 · ARCHITECTURE.md · SPEC-01…04 (all frozen v1.0)
> Scope: **V1 = single development stage spanning M1–M4** (M5 hardening = Supervisor gates).
> Every task cites spec sections + test IDs. No task without spec refs; no spec section
> without tasks. New ideas go to `BACKLOG.md`, never into V1 silently.

## Process law

- **Roles.** Architect pair (owner + opencode) — decisions only via decision entries
  (DEC-x in the owning spec / manifest). opencode = engineer + QA. **Supervisor**
  (independent agent, separate sessions) — reviews every completed task against spec refs
  and acceptance criteria; gates quality; never implements.
- **Testing law (strict).** Every test — unit included — is a real functional test:
  verifies data consistency, mutation, state outcome, replay identity. "Exit 0 = passed"
  is forbidden. Spec Test Definitions (purpose / principal target / expected outcome /
  level) are binding.
- **Definition of Done.** spec refs cited → acceptance tests green → Supervisor sign-off
  → ledger status updated → no unplanned features.
- **Statuses:** `pending → in_progress → review (Supervisor) → done` (+ `blocked`).

---

## M0 — bootstrap (pre-M1)

| ID | Task | Spec refs | Acceptance | Deps | Status |
|---|---|---|---|---|---|
| T-000 | Repo skeleton: workspace per ARCHITECTURE §3, CI (fmt/clippy/test), toolchain files | ARCH §3 | CI green on empty crates | — | review (Supervisor) |
| T-001 | Config subsystem: TOML parse, strict validation, profiles, autodetect skeleton | SPEC-02 §10 | WPT-10.3–10.6 | T-000 | pending |

## M1 — graph core + storage + schema graph

| ID | Task | Spec refs | Acceptance | Deps |
|---|---|---|---|---|
| T-101 | `lattice-core`: identifiers, epochs, node-id spaces, SoA/CSR arrays, cell types (scalars, records + presence bitmask) | ARCH §4; SPEC-01 §3, §5 | cell round-trip tests; array invariants | T-001 |
| T-102 | `lattice-schema` entities: full catalog inventory, key grammar | SPEC-01 §2–§15 | SGT-2.1–2.3 | T-101 |
| T-103 | Schema compiler: masks, read-sets, enabled-action index | ARCH §7; SPEC-01 §7, §9 | compile/recompile identity tests | T-102 |
| T-104 | Schema transactions: op registry, R13, dry-run | SPEC-01 §16 | SGT-16.1–16.3 | T-103 |
| T-105 | Journal: segments, framing, LSN, rotation, torn tail | SPEC-03 §6 | SFT-6.1–6.4 | T-101 |
| T-106 | Snapshot: header/arrays/catalog blob/footer, CRC | SPEC-03 §3 | SFT-3.1–3.3 | T-105 |
| T-107 | Parts: format, run directory, presence, zone maps | SPEC-03 §4 | SFT-4.1, 4.3, 4.4 | T-101, T-108 |
| T-108 | Codec registry + chains + determinism | SPEC-03 §5 | SFT-5.1–5.3 | T-101 |
| T-109 | MANIFEST swap + GC | SPEC-03 §2 | SFT-2.1–2.2 | T-106 |
| T-110 | Scalar-as-column (k=0) + LSN overwrite resolution | SPEC-03 §4.1/§9.1 | SFT-4.2 | T-107 |
| T-111 | M1 CLI smoke: create → write → restart → replay verify | ARCH §13 M1 | SFT-1.1 (seed mode); exit criteria met | T-104, T-106–T-110 |
| T-112 | Template bundle installer + 3 bundles v0 | SPEC-01 §17 | SGT-17.1–17.3 | T-104 |

## M2 — semantics engine

| ID | Task | Spec refs | Acceptance | Deps |
|---|---|---|---|---|
| T-201 | FSM registry, ranks, backward prohibition | SPEC-01 §8 | SGT-8.1–8.5 | T-103 |
| T-202 | Transition/action dispatch: requirements, actors, effects | SPEC-01 §9 | SGT-9.1–9.2, 9.5–9.7 | T-201 |
| T-203 | Dependency algebra: 8 kind handlers | SPEC-01 §10; base report §3 | SGT-10.1–10.3 + algebra property tests | T-201 |
| T-204 | Worklist propagation + read-set dirty tracking | ARCH §5.1, §8 | confluence property tests (permutation convergence) | T-202, T-203 |
| T-205 | Timer wheel: lag, deadline, on_deadline | SPEC-01 §9 | SGT-9.3–9.4 | T-204 |
| T-206 | Stall detector + blockers computation | SPEC-02 §8 | MCT-4.2 (core level) | T-204 |
| T-207 | Validator suite R1–R13 complete | SPEC-01 §16; SPEC-02 §7 | SGT-13.x–15.x | T-104, T-207 deps |
| T-208 | Determinism oracle + fuzz seeded | SPEC-03 §1 D1–D4 | SFT-1.1–1.3 | T-111 |
| T-209 | Active meta-schema e2e | SPEC-01 §17 | SGT-17.4 | T-209 deps: T-104 |

## M3 — wire + concurrency + SDK + MCP

| ID | Task | Spec refs | Acceptance | Deps |
|---|---|---|---|---|
| T-301 | Framing, type registry, TLV | SPEC-02 §2 | WPT-2.1–2.3 | T-111 |
| T-302 | Handshake, capabilities, PROXY v2 | SPEC-02 §3 | WPT-3.1–3.4 | T-301 |
| T-303 | Auth: policy table, SCRAM, cert, UDS | SPEC-02 §4 | WPT-4.1–4.5 | T-302 |
| T-304 | Sessions, pipelining, TXN frames | SPEC-02 §5 | WPT-5.1–5.2 | T-302 |
| T-305 | Resume, keepalive, cancellation | SPEC-02 §5 | WPT-5.3–5.6 | T-304 |
| T-306 | Credit flow control | SPEC-02 §6 | WPT-6.1–6.2 | T-304 |
| T-307 | Write commands + hint law | SPEC-02 §7 | WPT-7.1–7.5 | T-304, M2 |
| T-308 | Reads, cursors, time travel | SPEC-02 §8 | WPT-8.1–8.5 | T-307 |
| T-309 | Watches | SPEC-02 §9 | WPT-9.1–9.3 | T-305, T-308 |
| T-310 | Concurrent subtree-locked writers + sequencer | ARCH §8 | confluence under parallel load; WPT-5.1 parallel variant | T-307 |
| T-311 | Kotlin SDK core (`sdk-kotlin` repo) | SPEC-02 (client side) | mirrors WPT-2–5 acceptance from JVM | T-301–T-305 |
| T-312 | MCP gateway: 16 tools | SPEC-04 §3–§7 | MCT-3.1–3.3, 4.x, 5.x, 6.1–6.3, 7.1–7.2 | T-308–T-310 |

## M4 — admin + gateways + pooler + TLS

| ID | Task | Spec refs | Acceptance | Deps |
|---|---|---|---|---|
| T-401 | Admin gRPC: health, metrics, SchemaAdmin, storage ops | SPEC-02 §11 | WPT-11.1–11.2 | T-308 |
| T-402 | TLS-at-origin + PROXY v2 live | SPEC-02 §3 | WPT-3.3–3.4 in real TLS setup | T-302 |
| T-403 | REST/WS gateway (Kotlin) | SPEC-04 shapes over REST | parity tests vs MCP reads | T-312 |
| T-404 | SPA read views (minimal) | SPEC-04 §4 shapes | visual + parity vs wire reads | T-403 |
| T-405 | Pooler sidecar: identity translation, churn absorption | ARCH §2, §10 | WPT-5 churn variant through pooler; identity map e2e | T-305, T-311 |
| T-406 | Arrow payload completion | SPEC-02 §2; D-10.6 | zero-copy JVM read test (WPT-8.3 via SDK) | T-308, T-311 |
| T-407 | Audit export seed | SPEC-02 §11 | segment export → reimport verify (SFT-6.3) | T-401 |

## M5 — hardening (Supervisor-gated)

| ID | Task | Spec refs | Acceptance | Deps |
|---|---|---|---|---|
| T-501 | Compaction under load | SPEC-03 §7 | SFT-7.1–7.4 under sustained parallel write load | M3 |
| T-502 | Fuzz expansion: full oracle | SPEC-03 §1 | SFT-1.1 across 1000 randomized logs | T-208 |
| T-503 | Audit export complete | SPEC-02 §11 | archive/reimport cycle green | T-407 |
| T-504 | Index-module seam validation | ARCH §13 M5 | seam contract tests (ANN/BM25 stubs) | T-308 |
| T-505 | Supervisor regression gate: full acceptance run | all specs | every SGT/WPT/SFT/MCT ID green, signed by Supervisor | all |

---

## Traceability checklists

- SPEC-01: SGT-1…17 → T-102–T-104, T-112, T-201–T-209, T-312
- SPEC-02: WPT-1…11 → T-301–T-309, T-311–T-312, T-401–T-402
- SPEC-03: SFT-1…8 → T-105–T-110, T-208, T-501–T-502
- SPEC-04: MCT-1…8 → T-312, T-403–T-404, T-505

# SPEC-02 — Wire Protocol Specification

> LatticeDB v1 · Emergenta · 2026-11-26 · Status: **FROZEN v1.0 — contract**
> All micro-decisions owner-confirmed (§12; TOML accepted for config/policy language).
> Post-freeze changes go through the decision-entry flow (architect pair), never silently.
> Constitution: MANIFEST v0.3 + ARCHITECTURE.md. Sibling: SPEC-01 Schema Graph (frozen).
> Normative keywords MUST / MUST NOT / SHOULD / MAY per RFC 2119.

---

## 1. Scope

The native protocol binds LatticeDB to clients at three reach distances: in-process
(function calls — no protocol), same-host (UDS), network (TCP+mTLS). The control plane
(DDL-adjacent ops, metrics, dry-run) rides gRPC/HTTP2 on a separate admin listener and is
specified in §11. QUIC is reserved (capability flag), not implemented in v1.

Design discipline (from the base report): Postgres-v3-style framing, capability
negotiation in-handshake, credit-based flow control, **pipelining** (never multiplexing —
poolers stay transparent), out-of-band cancellation, epoch-integrated resume tokens,
protocol-level keepalive, TLS terminates at the DB process.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-1.1 | transport matrix | same client, three bindings | identical command semantics over UDS, TCP+mTLS, and library calls (same journal LSN sequence for equivalent workloads) | integration |
| WPT-1.2 | admin isolation | data-plane listener refuses admin ops, admin listener refuses data ops | both rejected with wrong-plane error; no cross execution | integration |

## 2. Framing

```
frame := [u8 type][u32 len][payload]
```

- `len` = payload byte count (excludes the 5-byte header). Server/client MUST
  `read_exact` the full frame before interpreting — no incremental parsing.
- `len` bounds: 0 .. `max_frame` (negotiated, default **16 MiB**, cap 256 MiB). Frames
  exceeding the negotiated bound ⇒ connection closed with `FRAME_TOO_LARGE` (no recovery
  attempt — the stream is desynced by definition).
- Byte order: little-endian everywhere.
- Payload encodings: commands and small replies use **TLV** (`[u16 tag][u32 len][value]`
  sequences, little-endian, tags per §7–§10); bulk reads carry **Arrow IPC record-batch
  chunks** (capability `ARROW`). TLV is self-describing so any language binds without a
  code generator.

### Message type registry (closed; additive only via minor versions)

| Range | Class | Types |
|---|---|---|
| 0x00–0x0F | control | HELLO 0x01 · PARAMS 0x02 · AUTH 0x03 (bidirectional steps) · READY 0x05 · PING 0x06 · PONG 0x07 · CANCEL 0x08 · BYE 0x09 · ERROR 0x0A · RESUME 0x0B |
| 0x10–0x1F | transaction | TXN_BEGIN 0x10 · TXN_COMMIT 0x11 · TXN_ROLLBACK 0x12 |
| 0x20–0x2F | write | CREATE_NODE 0x20 · DELETE_NODE 0x21 · LINK 0x22 · UNLINK 0x23 · ATTR_SET 0x24 · ROW_APPEND 0x25 · CELLS_APPEND 0x26 · ACTION 0x27 · GRANT 0x28 · REVOKE 0x29 |
| 0x30–0x3F | read/meta | SNAPSHOT 0x30 · NODE_READ 0x31 · CURSOR_OPEN 0x32 · CURSOR_FETCH 0x33 · CURSOR_CLOSE 0x34 · BLOCKERS 0x35 · HISTORY 0x36 · SCHEMA_PLAN 0x37 · SCHEMA_TXN 0x38 |
| 0x40–0x4F | results | ACK 0x40 · DATA_ARROW 0x41 · EVENT 0x42 |
| 0x50–0x5F | watch | WATCH_SUB 0x50 · WATCH_UNSUB 0x51 · WATCH_EVENT 0x52 |

Every request carries `request_id: u64` (first TLV tag 0x0001, mandatory); every reply
(ACK / DATA_ARROW / ERROR) echoes it — pipelining correlation without multiplexing.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-2.1 | oversized frame | send frame with len > negotiated max | connection closed, `FRAME_TOO_LARGE`; server accepts new connections immediately after | integration |
| WPT-2.2 | read_exact discipline | corrupt/truncated payload | server closes stream; already-committed txns intact (journal LSN sequence unchanged) | integration |
| WPT-2.3 | request_id echo | pipeline 100 mixed commands | every reply carries its command's request_id; zero mismatches | integration |

## 3. Handshake and capability negotiation

```
C → HELLO   {protocol_version: u32, feature_flags: u64, client_random: 32B}
C → STARTTLS (TCP only; UDS/library skip)        — TLS handshake, TLS 1.2 min, 1.3 preferred
S → PARAMS  {server_version, capabilities: u64, resume_token_ttl_s, max_frame}
C → AUTH    (SCRAM exchange per §4; 1..n frames) 
S → READY   {session_id, resume_token?, server_random}
```

- **Capabilities** (u64 bitfield, named registry): `ARROW(0)` · `PROXY_V2(1)` ·
  `RESUME(2)` · `WATCH(3)` · `COMPRESSION_ZSTD(4)` · `QUIC(5, reserved)` ·
  `PIPELINE(6, always on v1)`. Client sets desired flags; server sets supported ∩ desired;
  both MUST NOT use unset capabilities. Unknown client flags are ignored (forward-compat);
  unknown *server-required* capability ⇒ client closes with `CAPABILITY_MISMATCH`.
- `protocol_version`: major.minor packed u32 (`major<<16|minor`). Major mismatch ⇒
  handshake fails `VERSION_MISMATCH`. Minor: server MUST accept lower minor; additive
  types only.
- **PROXY v2:** if the listener has `proxy_protocol = true`, the connection MUST begin
  with the 12-byte v2 signature, stripped before HELLO. If a listener has it **disabled**
  and the stream starts with the signature, the server MUST close immediately (footgun
  guard — never desync).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-3.1 | negotiation | client requests impossible flags | server masks to supported set; session functions using only common caps | unit |
| WPT-3.2 | major version gate | HELLO version 2.0 against 1.x server | `VERSION_MISMATCH`, no session state created | unit |
| WPT-3.3 | PROXY footgun | signature bytes on non-proxy listener | instant close; log records `PROXY_V2_UNEXPECTED` | unit |
| WPT-3.4 | proxy passthrough | PROXY v2 header on enabled listener | peer address from header used by policy table; header not visible to handlers | integration |

## 4. Authentication

Methods bound per listener by the **policy table** (host config, ordered rules, first
match, hot-reloadable):

```
rule := (listener: uds|tcp|admin, database: key|any, subject_pattern, source: cidr|any,
         tls_state: any|none|require|verify_ca|verify_full) → (method, scope_bound)
methods := scram_sha_256 | scram_sha_256_plus (TCP+TLS channel binding) | client_cert
         | uds_peer | trust (library mode only)
```

- **SCRAM-SHA-256** (RFC 7677): standard 3-step exchange inside AUTH frames; verifiers in
  host-managed secrets file (admin-managed; `pg_authid` pattern). On TCP with TLS, clients
  MUST offer channel binding `tls-server-end-point` when the server advertises
  `scram_plus`. Raw cleartext-password methods do not exist on TCP (TLS-only).
- **client_cert**: certificate chain validates against host CA; subject resolved via the
  identity map file (CN or SAN URI → subject), the `pg_ident` pattern. Cert auth implies
  `verify_full` TLS state.
- **uds_peer**: kernel SCM_CREDENTIALS `(uid, pid)` mapped to subject via host map file.
- **trust**: only in library mode (no wire).
- Authorization: resolved subject carries the bound scope set; every in-Database check is
  R11 (kernel, in-transaction); allow/deny decisions land in the journal (SPEC-01 §14).
- Authn failures: `AUTH_FAILED` (generic — no user-existence oracle), security log entry,
  counter in metrics. 5 consecutive failures ⇒ connection-source cooldown (host config,
  default 10 s).

**When the policy table applies, and for what.** It is evaluated **exactly once per
connection**, at the AUTH step of the handshake — never per command. It decides four
things: (1) whether the connection is accepted at all (listener/database/source/tls-state
gating), (2) which auth method the client must complete, (3) the **scope ceiling** bound
to the session (the ceiling for all in-Database R11 checks of that session), (4) cooldown
enforcement for the source address. Per-operation authorization is a *different* layer:
catalog Grants (SPEC-01 §14) are checked by R11 in-transaction on every operation and are
journaled. Policy-table changes (hot reload) affect **new sessions only**; in-session
permission changes go through catalog REVOKE (validated at sequencing, SGT-14.3).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-4.1 | SCRAM happy path | correct password over TCP+mTLS | READY + session; secrets file verifier never leaves host; wire captures contain no password bytes | integration |
| WPT-4.2 | SCRAM wrong password | brute attempt x6 | 5× `AUTH_FAILED` generic; 6th attempt inside cooldown refused without DB roundtrip; security log has 5 entries | integration |
| WPT-4.3 | cert identity | client cert CN=alice, map alice→svc_alice | session subject = svc_alice; journal author field = svc_alice | integration |
| WPT-4.4 | policy first-match | two overlapping rules | first rule wins (order-sensitive); changing rule order changes outcome; reload applies without restart | unit |
| WPT-4.5 | no user oracle | auth attempt against nonexistent subject | identical error code + timing profile as wrong-password (assert timings within noise band) | unit |

## 5. Sessions, pipelining, resume, keepalive, cancellation

- **Session = connection.** Frames pipeline: unlimited in-flight requests, replies MAY
  return out of order **only** when independent (rule: ACKs may reorder; effects of a
  transaction's commands are journaled in request order — the server MUST validate
  request-order dependencies at sequencing, rejecting `ORDER_CONFLICT` instead of
  guessing).
- **Transactions:** default auto-commit (command = journal txn). `TXN_BEGIN` → subsequent
  write commands join one journal txn → `TXN_COMMIT` (single LSN range) / `TXN_ROLLBACK`
  (client-side discard of queued commands; server never holds unvalidated state).
  Nested TXN_BEGIN ⇒ `TXN_NESTED` error.
- **Resume tokens:** opaque server-issued bytes in READY; client reconnect (same
  transport class) sends RESUME{token}; server reattaches transactional/watch state iff
  token unexpired (default ttl 300 s, negotiated). Watch streams re-deliver from the
  token's epoch — no gap. Expired/unknown ⇒ `RESUME_EXPIRED`, session restarts fresh.
- **Keepalive:** PING/PONG both directions; default idle interval 30 s, 3 misses ⇒ dead
  (TCP keepalive alone is insufficient — ~2 h Linux default).
- **Cancellation:** CANCEL frame carries `{session_secret, target_request_id}` and MAY be
  sent on the same connection or a dedicated control connection; the server aborts the
  target request's in-memory work at the next safe point, replies `CANCELLED` for the
  target, ACKs everything already committed (partial commits stay committed — journal is
  the law). CANCEL on another session's secret ⇒ ignored + security log.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-5.1 | pipelining integrity | 1000 pipelined writes + interleaved reads | final DB state equals serial replay; all request_ids answered | integration |
| WPT-5.2 | txn atomicity | TXN of 3 commands, middle invalid | TXN_ROLLBACK semantics: none of the 3 applied (single LSN range absent); error names the failing command | integration |
| WPT-5.3 | resume continuity | subscribe watch, disconnect, 20 events accrue, reconnect with token | exactly those 20 events delivered in epoch order; no gap, no duplicate | integration |
| WPT-5.4 | resume expiry | token past ttl | `RESUME_EXPIRED`; fresh session works; server-side watch state GC'd | unit |
| WPT-5.5 | cancellation under load | long cursor fetch + CANCEL from second connection | fetch replies `CANCELLED` ≤ 100 ms; committed prefix intact; server healthy | integration |
| WPT-5.6 | dead client | drop TCP without BYE | server reclaims session after keepalive misses; lease/lock state consistent | integration |

## 6. Flow control (credit-based)

- Client grants credits: `{frames: u32, bytes: u64}` windows; server MUST NOT exceed
  granted window; client renews on consumption (WINDOW_UPDATE analog).
- Defaults: 64 frames / 8 MiB initial window, per direction. A starved server queues,
  never drops (backpressure, not loss).
- Watch events and cursor fetches share the session window; a slow watch consumer cannot
  starve foreground replies — watches MAY be suspended (`WATCH_PAUSED` notice) after the
  session buffer exceeds its sub-limit (default 4 MiB), resuming on credit renewal.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-6.1 | server honors window | grant 2-frame window, demand 100-frame cursor | exactly 2 frames in flight until renewal; zero buffer growth beyond window | integration |
| WPT-6.2 | slow consumer isolation | stalled watch consumer + active foreground query | foreground replies latency unchanged (p99 within noise); watch paused with notice, resumes after credits | integration |

## 7. Write commands

TLV tags per command (v1; each command = one atomic op unless inside TXN):

- `CREATE_NODE {graph, type_key, parent: node_ref?, initial_attrs: [attr,val][]}` →
  ACK `{node_id}` — containment/type validation per SPEC-01 §11.
- `DELETE_NODE {node, cascade: bool}` → ACK — `HAS_DEPENDENTS` without cascade (SPEC-01 §3).
- `LINK {src, dst, kind, params}` / `UNLINK {edge}` — R1–R3 checks; ACK `{edge_id}`.
- `ATTR_SET {node, attr, value}` — scalar/enum/record set; `NOT_APPENDABLE` for vector attrs.
- `ROW_APPEND {node, table_set, rows: [{cells: [attr,val][], k_skip: u32?}]}` — table_set
  row semantics (SPEC-01 §6); `k_skip` supports gap-preserving appends; batch of rows in
  one journal record.
- `CELLS_APPEND {node, attr, cells: [(k?, val)]}` — point append to one column.
- `ACTION {node, transition_key, payload?, actor_hint}` — SPEC-01 §9 semantics;
  `REQUIREMENTS_UNMET` lists failing rule keys (machine hint, UVFlow pattern).
- `GRANT {role, subject, target}` / `REVOKE` — journaled authorization mutations (SPEC-01 §14).

**Error frames:** `[u16 code][hint: utf8][detail_len: utf8/json]`. Codes (v1 registry,
closed): `AUTH_FAILED 0x0301 · FORBIDDEN 0x0302 · NOT_FOUND 0x0303 · ALREADY_EXISTS
0x0304 · INVALID_KEY 0x0305 · SCHEMA 0x0306 · INVALID_KIND_PARAMS 0x0307 · CYCLE 0x0308 ·
MULTI_PARENT 0x0309 · CONTAINMENT 0x030A · HAS_DEPENDENTS 0x030B · REQUIREMENTS_UNMET
0x030C · TERMINAL 0x030D · NOT_APPENDABLE 0x030E · NOT_MONOTONE 0x030F · R13_IN_USE
0x0310 · BACKWARD_RANK 0x0311 · ORDER_CONFLICT 0x0312 · TXN_NESTED 0x0313 · RESUME_EXPIRED
0x0314 · VERSION_MISMATCH 0x0315 · CAPABILITY_MISMATCH 0x0316 · FRAME_TOO_LARGE 0x0317 ·
CANCELLED 0x0318 · LIMIT_EXCEEDED 0x0319 · INTERNAL 0x03FF`.

**Hint law:** where a rejection has enumerable alternatives, the hint MUST list them
(`unknown evidence-kind` pattern): unknown type key ⇒ hint lists existing types (capped
50); REQUIREMENTS_UNMET ⇒ lists failing rule keys; CONTAINMENT ⇒ names expected parent
type. Agents branch on hints without extra roundtrips.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-7.1 | row append batch | ROW_APPEND of 1000 rows, 3 columns with gaps | column reads return exactly written cells; k alignment per SPEC-01 §6; one journal record (assert via LSN delta) | integration |
| WPT-7.2 | hint enumerability | LINK with unknown type key + REQUIREMENTS_UNMET case | hints list valid alternatives (≤50) / failing rule keys; client retries successfully using hint only | unit |
| WPT-7.3 | cascade guard | DELETE_NODE of parent, cascade=false, live children | `HAS_DEPENDENTS`; nothing deleted; with cascade=true → subtree removed, journal shows ordered DelVertex records | integration |
| WPT-7.4 | action error shape | ACTION with 2 unmet rules | `REQUIREMENTS_UNMET` + failing rule keys; after satisfying them, same command succeeds — no schema change needed | integration |
| WPT-7.5 | grant round trip | GRANT → subject writes → REVOKE → subject writes | allow, then deny; both journal entries carry subject+decision | integration |

## 8. Read commands and cursors

- `SNAPSHOT {graph?, subtree?, at_epoch?}` → DATA_ARROW: nodes+states+edges+scalars
  (consistent as of epoch; at_epoch enables time-travel reads ≤ current).
- `NODE_READ {node}` → ACK + inline TLV (scalars, record cells, vector head/tail preview).
- `CURSOR_OPEN {index: by_state | attr_presence | shared_key | type_scan, params, filter:
  predicate TLV, order, window}` → ACK `{cursor_id}`; `CURSOR_FETCH {cursor_id, budget}`
  → DATA_ARROW batches (`budget` bytes/rows); `CURSOR_CLOSE`. Idle cursor TTL 60 s
  (server-side GC, `CURSOR_EXPIRED` on fetch).
- `BLOCKERS {node}` → ACK: concrete blocker set (pending guards, active inhibitions,
  deadlines, unmet requirement rules with names) — the agent debugging primitive.
- `HISTORY {node, range}` → ACK: journal-derived event list for the node (state motions,
  attr writes, actions, with authors).
- `SCHEMA_PLAN {ops}` → ACK: SPEC-01 §16 dry-run report (no commit).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-8.1 | snapshot consistency | concurrent writers during SNAPSHOT | returned graph is exactly some past epoch (no torn mix); epoch marker ≤ start | integration |
| WPT-8.2 | time travel | writes → snapshot at_epoch=older | older view exact; replay of *current* state unchanged after travel read | integration |
| WPT-8.3 | cursor pagination | 1M-cell column, fetch 8 MiB windows | all cells in order, no dup/loss, Arrow schema stable across batches | integration |
| WPT-8.4 | blockers truth | node blocked by guard+inhibition+unmet rule | BLOCKERS names all three precisely; after removing them, list empties | integration |
| WPT-8.5 | dry-run no-commit | SCHEMA_PLAN with destructive op | report rejects; catalog epoch unchanged; then commit path behaves identically to plan | unit |

## 9. Watches

`WATCH_SUB {subtree?, kinds: [state|attr|struct|action|enabled|timer], min_epoch}` → ACK
`{watch_id, current_epoch}`; `WATCH_EVENT {epoch, kind, node, delta}` frames follow,
strictly epoch-ordered, resumable by token (§5). Filters are server-side (pre-dispatch).
`WATCH_UNSUB` stops delivery. Event payload shapes are stable per kind and documented in
the MCP Surface Spec (agents consume these).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-9.1 | ordering | 500 concurrent mutations on watched subtree | events strictly epoch-ascending; no loss (compare with journal-derived set) | integration |
| WPT-9.2 | filter correctness | kinds=[state] subscription | only state-kind events delivered; attr mutations absent | unit |
| WPT-9.3 | multi-client fairness | 5 subscribers, 1k events | each receives all 1k, order preserved, no cross-starvation (WPT-6.2 mechanism) | integration |

## 10. Configuration system (no hardcoded parameters — owner law)

**Law: every tunable is a config parameter.** No module may carry a business-relevant
constant: limits, windows, thresholds, TTLs, pool sizes, arena sizes — all live in the
host configuration with documented default, safety clamp (absolute min/max), unit and
description. Defaults come from **profiles**, never from code literals.

**One host config file** (same language as the policy table; format per §12.4) holding:

| Group | Parameters (non-exhaustive) |
|---|---|
| `network` | max_frame, credit windows, keepalive interval/misses, resume ttl, cursor idle ttl, session caps per subject, listen backlog |
| `auth` | policy table, failure cooldown, verifier file path, identity maps |
| `engine` | worklist batch depth, timer wheel tick granularity, stall grace period, propagation queue caps |
| `storage` | group-commit window, fsync mode, CRC block size, part target size, merge trigger (count/bytes), staging buffer sizes, arena budgets (structure/parts/sessions) |
| `resources` | worker threads, sequencer queue depth, merge thread pool, memory budget shares |
| `observability` | metrics exposition interval, log levels, slow-cursor trace threshold |

**Profiles:** predefined `dev` / `laptop` / `server` / `analytics` (vector-heavy: larger
part budgets and cursor windows) shipped as versioned config sets; **`latticedb config
autodetect`** generates a tuned set from host resources (CPU/RAM/disk class) — the
pgtune pattern. Performance per config set is measured via the admin metrics plane
(WPT-11.1), enabling evidence-based tuning.

**Strict validation:** unknown or misspelled parameter keys are **startup errors naming
the offending key** — silent fallback to defaults is forbidden (typos must never masquerade
as configuration). Hot-reload applies to a documented safe class (keepalive, cooldown,
metrics, log levels, credit caps); restart-required parameters are marked and applied only
at restart. Internally, code may hold *safety clamps* (documented absolute bounds) but MUST
read every default from config.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-10.1 | host clamps | client requests max_frame above host clamp | PARAMS returns clamped value; over-clamp frame closes connection | unit |
| WPT-10.2 | session cap | 65th concurrent session for one subject (cap 64) | rejected `LIMIT_EXCEEDED`; first 64 unaffected | integration |
| WPT-10.3 | no-hardcoded law | set every tunable to a legal extreme (tiny/large) | behavior identical (results, journal equivalence), only latency/throughput shift; zero result divergence | integration |
| WPT-10.4 | autodetect determinism | same host specs → generated config | byte-identical generated sets across runs; profiles differ on divergent hosts | unit |
| WPT-10.5 | strict validation | typo'd parameter key `keepaliev` | startup fails naming the key and section; no silent default applied | unit |
| WPT-10.6 | hot-reload classes | reload with changed keepalive + changed max_frame | keepalive applies to new traffic live; max_frame marked restart-required and unchanged until restart | integration |

## 11. Admin plane (gRPC/HTTP2, separate listener)

Services (proto3; full .proto in implementation repo, shapes fixed here):
- `Health` — liveness/readiness, compaction state, journal lag.
- `Metrics` — Prometheus exposition: op counters, LSN rate, worklist depth, lock
  contention, part counts, memory arenas.
- `SchemaAdmin` — `PlanSchemaTxn` (same engine as wire §8), `ReloadPolicyTable`,
  `ListSubjects`.
- `Storage` — compaction trigger/status, audit export streaming (phase v1.x).
Admin auth: dedicated subjects + mTLS; admin MAY read, but data-plane write semantics
are never exposed here.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| WPT-11.1 | metrics truth | run known workload (100 writes) | op counters exact; LSN rate matches journal inspection | integration |
| WPT-11.2 | admin auth | data-plane subject calls SchemaAdmin | denied; admin subject required; both attempts logged | integration |

## 12. Micro-decisions (status)

1. **TLV payloads for commands, Arrow IPC for bulk reads** — CONFIRMED (no codegen
   dependency; self-describing; Arrow where volume lives).
2. **request_id correlation + reordered-ACK allowance with ORDER_CONFLICT guard** — CONFIRMED.
3. **Auto-commit default; explicit TXN frames for batches; rollback = client-side discard**
   — CONFIRMED.
4. **Config language: TOML** — CONFIRMED (owner accepted). One language for the entire
   host config (policy table + all tunables, §10); strict parse, order-preserving, zero-ambiguity.
5. **5-failure auth cooldown** — CONFIRMED.
6. **No hardcoded parameters; full configuration system with profiles + host autodetect**
   — CONFIRMED as owner law, implemented as §10 (supersedes the original fixed-limits table).
7. **Error-code registry + hint law** — CONFIRMED.

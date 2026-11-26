# SPEC-04 — MCP Surface Specification

> LatticeDB v1 · Emergenta · 2026-11-26 · Status: **FROZEN v1.0 — contract**
> All micro-decisions owner-confirmed (§9). Post-freeze changes go through the
> decision-entry flow (architect pair), never silently.
> Constitution: MANIFEST v0.3 + ARCHITECTURE.md. Siblings: SPEC-01 (frozen), SPEC-02
> (frozen), SPEC-03 (frozen).
> The MCP gateway is a **wire-protocol client** (Kotlin, SPEC-02) — there is no second
> path into the core. Normative keywords per RFC 2119.

---

## 1. Scope and transport

The gateway exposes LatticeDB as MCP (Model Context Protocol) tools for AI agents.
Transports: MCP `stdio` (local agents) and MCP streamable HTTP (remote agents) — both
mapping 1:1 onto wire sessions (SPEC-02 §3–§5). One MCP session = one wire session =
one authenticated subject. The gateway performs: tool→command mapping, scope binding,
response shaping (context firewall), error translation. It holds no state beyond the
session and its subscriptions.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| MCT-1.1 | transport parity | identical tool sequence over stdio and HTTP | identical journal LSN sequence and replies; gateway is stateless between transports | integration |
| WPT-1.1b | no parallel truth | tool read vs direct wire read | identical bytes for identical epoch (same snapshot path) | integration |

## 2. Design rules (the context firewall)

- **Compact responses.** Every tool response is capped (default 64 KiB, config §10);
  oversized results MUST paginate via opaque cursor tokens — never truncate silently.
  The gateway is a context firewall, not a firehose.
- **One tool call = one journal transaction.** Coarse enough to be atomic, narrow enough
  to be permission-scoped (each tool binds exactly one kernel scope, §5).
- **Hint passthrough.** Wire hints (SPEC-02 §7 hint law) travel into tool errors
  verbatim — agents branch on them without extra roundtrips.
- **No parallel truth.** All reads are snapshot reads at an epoch; the gateway never
  caches across epochs nor synthesizes state.
- **Stable shapes.** Tool result fields are versioned; additive fields only; agents rely
  on them (the enabled-action index feeds agent loops — no polling).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| MCT-2.1 | response cap | graph_snapshot of a 1 MiB subtree | paginated result ≤ cap + continuation token; assembled pages byte-equal the full read | integration |
| MCT-2.2 | atomicity | tool call failing mid-write | zero partial effects (single LSN range or none); error names the stage | integration |

## 3. Tool catalog (closed list)

| # | Tool | Wire command(s) | Required scope | Returns |
|---|---|---|---|---|
| 1 | `graph_snapshot` | SNAPSHOT | read_subtree | subtree: nodes, states, edges, scalars (paginated) |
| 2 | `read_nodes` | NODE_READ | read_subtree | batched node details (attrs, cells head/tail, edges) |
| 3 | `query` | CURSOR_OPEN/FETCH/CLOSE | read_subtree | index query results (Arrow→compact rows), paginated |
| 4 | `blockers` | BLOCKERS | read_subtree | concrete blocker set: pending guards, active inhibitions, deadlines, unmet rule keys |
| 5 | `history` | HISTORY | read_subtree | node journal-derived events with authors |
| 6 | `schema_read` | (admin read / catalog dump) | read_subtree | catalog entities for the Database (types, models, transitions, sets, rules) |
| 7 | `create_node` | CREATE_NODE | write_structure | node_id |
| 8 | `link` / `unlink` | LINK / UNLINK | write_structure | edge_id |
| 9 | `write_attr` | ATTR_SET / CELLS_APPEND | write_attribute | ack(epoch) |
| 10 | `append_rows` | ROW_APPEND | write_attribute | ack(epoch, first_k, count) |
| 11 | `act` | ACTION | invoke_action | ack(new state, epoch) |
| 12 | `decide` | ACTION (select_branch) | invoke_action | ack(branch enabled, siblings cancelled) |
| 13 | `subscribe` | WATCH_SUB/UNSUB | read_subtree | watch stream (§6) |
| 14 | `plan_schema` | SCHEMA_PLAN | administer | dry-run impact report |
| 15 | `schema_txn` | SCHEMA_TXN | administer | ack(schema_epoch) |
| 16 | `grant` / `revoke` | GRANT / REVOKE | administer | ack |

Closed registry: tools are versioned with the gateway; no dynamic tool injection in v1.
`decide` is `act` specialized to select-branch transitions (Discussion-1 collapse:
decisions are actions) — it exists as a separate tool for **semantic clarity to agents**,
not as a second code path.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| MCT-3.1 | tool→command mapping | each tool exercised against wire trace | exactly the mapped wire commands emitted, in order, with mapped args | integration |
| MCT-3.2 | scope per tool | subject lacking `invoke_action` calls `act` | MCP error `FORBIDDEN` + scope name hint; nothing journaled except the deny | integration |
| MCT-3.3 | decide semantics | `decide` on XOR node | branch enabled, siblings cancelled (SPEC-01 §9.6 behavior), one Action record with actor=agent subject | integration |

## 4. Read tools — arguments and shaping

- `graph_snapshot {graph?, subtree?, at_epoch?, depth?, include: [scalars|edges|cells_head]}` —
  default depth = full subtree; `cells_head` returns the last N cells per vector (N=16
  default) — the analytics/series preview.
- `query {index: by_state | attr_presence | set_presence | shared_key | type_scan,
  params, filter?, order?, budget?}` — filter predicates reuse SPEC-01 §13 whitelisted
  forms; results are compact rows + continuation token. This is the tool that answers
  "all nodes with name/description/author" (set_presence, DEC-01) and "everything in
  state X" (by_state).
- `blockers {node}` — result shape: `{pending_guards: [{edge, from_node, missing_state}],
  inhibitions: [{edge, source, activation}], deadlines: [{transition, fires_at}],
  unmet_rules: [rule_key]}` — empty array = unblocked. The debugging primitive for
  agent loops.
- `schema_read {entity: types|models|transitions|sets|rules|all}` — agents learn the
  world before acting; output is the catalog as of current schema epoch.
- `history {node, since_lsn?, kinds?}` — ordered, author-attributed event list.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| MCT-4.1 | set_presence query | 50 nodes across 5 types referencing `general_attr_set` | query returns exactly the 50; adding a 6th type referencing the set updates results without index rebuild | integration |
| MCT-4.2 | blockers honesty | node blocked by guard + unmet rule + deadline | all three listed with precise names; resolving them empties the set | integration |
| MCT-4.3 | cells_head budget | node with 1M-cell vector | returns ≤ N cells + total_count + latest_k; response ≤ cap | unit |
| MCT-4.4 | at_epoch travel | mutate, then snapshot at older epoch | older state exact; current reads unaffected | integration |

## 5. Write tools — semantics inherited

All write tools inherit wire-command validation (R1–R13) and journal semantics verbatim;
the gateway adds zero validation of its own (single semantics engine law). `act` requires
`actor_hint` ∈ {human, agent} for journal authorship; system/timer actors are
engine-internal and not invokable via MCP. `append_rows` targets `table_set`s only
(SPEC-01 §7). `schema_txn` returns the new `schema_epoch` and MUST expose the dry-run
report of `plan_schema` on rejection — agents see *why*, with live-reference paths.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| MCT-5.1 | inherited validation | `write_attr` violating monotonicity flag | `NOT_MONOTONE` + direction hint; identical to direct-wire rejection | unit |
| MCT-5.2 | schema rejection transparency | `schema_txn` with destructive op | error carries R13 rule id, reference path, impact counts (from dry-run); catalog epoch unchanged | integration |
| MCT-5.3 | actor attribution | `act` by agent subject | journal Action record: actor_class=agent, subject id, payload | integration |

## 6. Subscribe — event stream

`subscribe {subtree?, kinds: [state_changed | attr_updated | node_added | node_removed |
action_fired | action_enabled | deadline_fired], min_epoch?}` — event shapes (stable,
additive):

- `state_changed {node, type, from, to, by_subject, actor_class, cause: action|guard|timer}`
- `attr_updated {node, attr, epoch}` (values fetched by read — no bulk in events)
- `action_enabled {node, transition}` / `action_disabled {node, transition, missing: [rule_key]}`
  — **the agent work loop primitive**: subscribe to enabled-actions instead of polling.
- `deadline_fired {node, transition, on_deadline}` · `action_fired {node, transition, actor}`
- `node_added/removed {node, type, parent}`

Delivery: epoch-ordered, resumable (SPEC-02 §5 resume tokens), subject filtered by its
read_subtree binding — a subject never observes nodes outside its grants.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| MCT-6.1 | enabled-action loop | satisfy requirement rules for 3 nodes | three `action_enabled` events with correct transition names; after `act`, `action_disabled`/fired sequence correct | integration |
| MCT-6.2 | grant-filtered stream | subject granted subtree S only | zero events from outside S across 500 mutations elsewhere | integration |
| MCT-6.3 | resume shape | disconnect/reconnect mid-stream | no gap, no duplicate, epoch-continuous (SPEC-02 WPT-5.3 at gateway level) | integration |

## 7. Error mapping (wire → MCP)

Wire code → MCP tool error `{code, hint, detail, recoverable}`:

| Wire | MCP mapping | recoverable? |
|---|---|---|
| REQUIREMENTS_UNMET | yes + failing rule keys ("satisfy these, retry") | yes |
| FORBIDDEN | yes + missing scope name (subject requests grant) | yes |
| NOT_APPENDABLE | yes ("append above current max or new node") | yes |
| SCHEMA / INVALID_KEY / INVALID_KIND_PARAMS | yes + valid alternatives list (hint law) | yes |
| CYCLE / MULTI_PARENT / CONTAINMENT | yes + offending path | yes |
| TERMINAL | **no** — terminal is absorbing; guidance: create re-run instance | no |
| R13_IN_USE / BACKWARD_RANK | no — catalog redesign required (deprecate→drain→remove) | no |
| INTERNAL | no — supervisor escalation | no |

`recoverable` is advisory for agent loops; the Supervisor uses it to classify agent
failures (retry-till-stuck vs design bug).

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| MCT-7.1 | recoverable loop | agent hits REQUIREMENTS_UNMET → satisfies → retries | success on retry, no gateway intervention; classification=recoverable | integration |
| MCT-7.2 | non-recoverable discipline | agent retries TERMINAL rejection 3 times | identical error each time; Supervisor classification=terminal; no state corruption | integration |

## 8. Gateway limits (all in host config, SPEC-02 §10 law)

Response cap 64 KiB · pagination depth unlimited (token-chained) · events per watch
buffer 4 MiB (pause/resume per SPEC-02 §6.2) · tools per session = the closed registry ·
concurrent subscriptions per session: 16.

### Tests

| ID | Purpose | Principal target | Expected outcome | Level |
|---|---|---|---|---|
| MCT-8.1 | cap clamp | host caps response at 16 KiB | paginated results respect clamp; assembled result byte-equal full read | unit |

## 9. Micro-decisions (owner-confirmed 2026-11-26)

1. **`decide` as a distinct tool** (same wire path as `act`, select-branch specialization)
   — ACCEPTED: semantic clarity for agents; single code path.
2. **No task-bundle tool** — APPROVED (Q14 closed: context assembly is business-layer/
   SDK; the MCP surface stays domain-neutral primitives).
3. **Response cap 64 KiB + pagination** — APPROVED, with owner note: **the cap is a
   configuration parameter** (SPEC-02 §10 law — defaults confirmed, value tunable per
   deployment).
4. **Closed tool registry, versioned with gateway; no dynamic tools in v1** — APPROVED
   ("for v1 is enough").
5. **`recoverable` advisory flag on errors** — ACCEPTED (feeds Supervisor
   classification).
6. **Event payloads carry references, not values** (fetch-on-read) — APPROVED (context
   firewall).

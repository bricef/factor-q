# factor-q interface inventory — every read/write endpoint, by surface

Captured from `main` (2026-07-18 snapshot), as input to the ADR-0006 registry-first
decision and the ADR-0031 `fqd`/`fq` split. Everything below was verified against
source; file references are to that snapshot.

**Legend.** Kind: `R` read · `W` write · `S` stream.
Disposition: **transplant** (body becomes a registry handler, near-verbatim) ·
**delete** (layer is derived away) · **re-point** (consumer switches to the derived
client) · **keep** (stays hand-written, outside the registry) · **decide** (needs an
ADR call).

**Headline counts.**

| Surface | Endpoints | Of which R / W / S |
|---|---|---|
| Operator CLI (`fq`) | 21 verbs | 13 R · 6 W · 2 S |
| `Views` (in-process read model) | 16 methods | 16 R |
| `ReadService` (tarpc) | 15 RPCs | 15 R |
| Write/control mechanisms | 3 distinct paths + 1 direct store write | — |
| fq-store: `CasService` | 9 RPCs | 6 R · 3 W |
| fq-store: `fq-cas` CLI | 24 verbs | ~13 R · ~11 W |
| Dashboard BFF | 11 routes (1 SSE) | 11 R |
| github-watcher (external) | 1 publish + 3 subscribe subjects | 1 W · 3 S |
| Agent-facing built-ins (ADR-0016) | 5 tools | — |

Steady-state registry estimate: **~21 runtime ops + ~20 CAS ops**, before traversal
ops arrive.

---

## 1. Operator CLI — `fq` (`fq-cli/src/main.rs`, 4,904 lines, 19 hand renderers)

| Command | Kind | Backend today | Proposed op | Disposition / notes |
|---|---|---|---|---|
| `fq init [--force]` | W | Local FS scaffold in cwd | — | **keep** — project lifecycle, not a daemon op |
| `fq run` | — | Becomes the daemon | — | **keep** — is `fqd` post-0031 |
| `fq version [--json]` | R | Local build info | `runtime.version` | transplant (RPC `version` already exists) |
| `fq reload` | W | NATS publish `fq.control.reload` (empty body); fire-and-forget | `control.reload` | transplant; RPC form gains an ack for free. #114 (`fq.toml` refresh) becomes an op-input extension |
| `fq down [--now/--no-drain]` | W | NATS publish `fq.control.down` (drain flag in body), then subscribes `fq.system.shutdown` and waits (bounded) | `control.down` | transplant; the wait-for-shutdown is naturally the op's response |
| `fq trigger <agent> [payload]` | W | **Default: in-process** — loads `AgentRegistry` from local disk, runs the runner in this process, bypassing the daemon entirely. `--via-nats`: publish `fq.trigger.<agent>` into the `fq-triggers` JetStream stream | `trigger.publish` | **decide** — in-process mode links the whole runtime and is incompatible with 0031's thin client. Retire it, or move it to a dev-only `fqd` pathway |
| `fq dead-letters list [--agent --limit --json]` | R | `operator::list_dead_letters` — reads the event stream over JetStream | `deadletter.list` | transplant; 30-day retention bound belongs in `OpMeta` docs |
| `fq dead-letters requeue <agent> [--trigger-seq --json]` | W | `operator::requeue_dead_letter` — re-publishes from the trigger stream (24 h retention); **not idempotent** | `deadletter.requeue` | transplant; idempotency caveat → op contract text |
| `fq agent list` | R | **Local FS** — `AgentRegistry::load_from_directory` on the operator's machine | `agent.list` | **decide** — post-split the daemon's registry is authoritative (RPC `agents`/`agent` already exist); local mode disappears with the thin client |
| `fq agent validate <path>` | R | Local FS parse + validate | `agent.validate` | **keep** local (pure function on a file); optionally also expose as an op |
| `fq events tail [--subject fq.>]` | S | NATS core subscribe on the given subject | `event.tail` | transplant as Stream-kind; 0031 re-expresses as cursor-poll (push upgrade additive) |
| `fq events query [--agent --type --since --limit --json]` | R | `Views.events` (projection SQLite) | `event.query` | transplant |
| `fq costs [--agent --since --json]` | R | `Views.costs` / `Views.agent_costs` | `cost.summary` / `cost.by_agent` | transplant |
| `fq status [--json]` | R | NATS connect probe + **JetStream management reads** (stream/consumer info) + `Views::open` probe | `runtime.status` | transplant — JetStream introspection must move daemon-side post-split |
| `fq doctor [--json --fail-on-issues]` | R | `open_views` → `Views.{failures, recovery, workers, active_invocations, …}` → pure `DoctorReport` assembly | `runtime.doctor` | transplant; already the exemplary shape (fetch → pure assemble → render) |
| `fq invocation list [--status --include-archived --limit --json]` | R | `Views.invocations` / `invocation_index` / `recent_archives` | `invocation.list` | transplant |
| `fq invocation show <id> [--json]` | R | `Views.invocation` (owner + archive + recent events) | `invocation.show` | transplant |
| `fq invocation drop <id> [--reason --json]` | W | `operator::drop_invocation` over the bus → publishes `invocation.operator_recovered` | `invocation.drop` | transplant; audit-classed write (kill-switch semantics per current help text) |
| `fq invocation transcript <id> [-f --format --full]` | R+S | Snapshot: `Views.transcript` (**worker WAL** DB). `--follow`: NATS subscribe `fq.agent.<agent_id>.>`, subscribed *before* snapshot to close the gap | `invocation.transcript` + `invocation.transcript_since` | transplant — the cursor pair already exists on `ReadService`; `--follow` re-points to it |
| `fq workers list [--stale-only/--alive-only --json]` | R | `Views.workers` (control-plane store) | `worker.list` | transplant |
| `fq workers show <id> [--json]` | R | `Views.worker` | `worker.show` | transplant |
| `fq workers prune [--dry-run]` | W | **`ControlPlaneStore::open` direct** — CLI-side store mutation, no daemon, no NATS, no event emitted | `worker.prune` | transplant with priority — the only write bypassing every boundary; gains an audit event as an op |

CLI global args (`--config`, `--agents-dir`, `--nats-url`, `--cache-dir`,
`--log-format`) become client-connection config, not op inputs.

---

## 2. `Views` — the in-process read model (`fq-runtime/src/views.rs`, 1,651 lines)

The de-facto read API over `RuntimeDbPaths { worker, control_plane, projection }`.
These method bodies are the transplant source for read-op handlers.

`event_count` · `events` · `costs` · `agent_costs` · `failures` · `workers` ·
`worker` · `recovery` · `executions` · `transcript` · `active_invocations` ·
`invocations` · `invocation_index` · `recent_archives` ·
`agent_id_for_invocation` · `invocation`

Notes:
- `failures`, `recovery`, `executions`, `event_count` are consumed only by
  doctor/status compositions — decide per-method whether each is an op in its own
  right or internal to a composite handler.
- `transcript` is the one read that crosses store domains (worker WAL).
- `agent_id_for_invocation` is an internal join helper, not an op.

---

## 3. `ReadService` — tarpc read mirror (`fq-runtime/src/read_service.rs`, feature `read-service`)

1:1 wire-mirror of `Views` plus a forwarding handler; loopback-only bind
(non-loopback refused), unauthenticated, bincode.

RPCs (15): `version` · `health` · `active_invocations` · `workers` ·
`worker(id)` · `invocations(filters…)` · `invocation(id)` · `transcript(id,…)` ·
`transcript_since(id, cursor,…)` · `events(filters…)` · `costs(…)` ·
`agent_costs(…)` · `agents` · `agent(id)`

Disposition: **delete** — this layer is exactly what the registry derives.
`health` is the one RPC with real logic beyond forwarding (NATS/stream checks);
it transplants as `runtime.health`. `bind`/`connect` are replaced by the derived
transport under 0031's TLS+secret middleware.

---

## 4. Write & control mechanisms (three disjoint paths today)

1. **NATS control subjects** (`bus.rs`): `fq.control.reload` (empty body),
   `fq.control.down` (drain flag). CLI publishes; daemon subscribes. → ops
   `control.reload` / `control.down`. **Decide:** whether a NATS binding survives
   as a derived adapter (loopback compat) or retires behind the tarpc face.
2. **Trigger wire contract**: `fq.trigger.<agent>` → `fq-triggers` JetStream
   stream. External publishers exist (§7), so `trigger.publish` must preserve this
   wire contract — the op wraps the publish; the subject remains the ingress for
   non-CLI producers.
3. **Operator API** (`control_plane/operator.rs`): `drop_invocation` (emits
   `invocation.operator_recovered`), `list_dead_letters`, `requeue_dead_letter` —
   all take `&bus`; transplant near-verbatim.
4. **Direct store write**: `workers prune` (§1) — the outlier to eliminate.

CLI-consumed response events: `fq.system.shutdown` (awaited by `down`).

---

## 5. fq-store — CAS surfaces

**`CasService`** (tarpc, feature `service`; loopback-only, unauthenticated):
`put` · `get` · `get_range` · `has` · `size` · `stats` · `remove` ·
`has_block` · `remove_block` (the last three are the GC internals #183 flagged —
in a registry they become permission-gated rather than surface-trimmed).

**`fq-cas` CLI** (feature `cli`), 24 verbs:

- Blob: `put` · `get` · `has` · `size` · `metrics`
- `serve` (binds `CasService`) — **keep** (process lifecycle)
- Object (named, versioned): `object put` · `get` · `ls` · `rm` · `resolve` · `history` · `bind`
- `gc` (verified online GC run)
- Key: `key generate` (Ed25519 pair)
- Grants (root authority): `grant add` · `ls` · `check` · `rm`
- Tokens (biscuit): `token mint` · `attenuate` · `inspect`

Permission vocabulary already shipped: `Verb { Read, Write, Delete, List, Grant }`
over scopes (`grants.rs`) + biscuit capability tokens — **this is the semantics
`OpMeta.permission` should bind to**, for both stores' ops and the runtime's.

Disposition: **decide** (0031 lists it as open) — fold under `fqd`'s
transport-auth as `cas.* / object.* / grant.* / token.*` in the same registry, or
run a second registry instance in fq-store built on the same `fq-ops` crate.
Crash-domain separation survives either way.

---

## 6. fq-dashboard — BFF consumer (read-only)

11 axum routes over `ReadServiceClient`:

`/` (health) · `/invocations` · `/invocations/{id}` ·
`/invocations/{id}/transcript` · `/invocations/{id}/transcript/stream`
(SSE via 1 s `transcript_since` cursor-poll) · `/assets/datastar.js` (static) ·
`/events` · `/costs` · `/costs/{agent}` · `/agents` · `/agents/{id}`

Disposition: **re-point** to the derived typed client; routes unchanged. The SSE
bridge is already the reference implementation of the Stream-kind adapter.

---

## 7. github-watcher — external wire client (Go)

- **Publishes**: `fq.trigger.<agentID>` (JSON body per the trigger wire contract)
  into `fq-triggers` (`publisher.go`).
- **Subscribes**: `fq.agent.<agent>.{triggered, completed, failed}` (`events.go`,
  OutcomeReactor).

Disposition: **unchanged** through phases 1–2 — these are event-log/wire-contract
surfaces, not ops. The watcher is the natural first consumer of the derived
MCP/REST face later. The wire-contract doc is a compatibility boundary the
registry must not break.

---

## 8. Agent-facing built-ins (ADR-0016) — phase-4 convergence

`ToolRegistry` builtins with hand-written JSON schemas at the MCP boundary:
`file_read` · `file_write` · `exec` · `self_inspect` · `report_outcome`.
(`artifact_put`/`artifact_get` — #89 — not yet shipped.) MCP tools register
alongside under `__` namespacing per the #177 decision.

Disposition: converge onto the same registry as capability-filtered ops exposed
through the MCP-server adapter. Until then, share the schema conventions so
convergence stays mechanical.

---

## 9. Adjacent, explicitly out of scope

- **Event bus subjects & payload schemas** (`fq.agent.*`, system, coordination,
  heartbeats): the event log is the system of record (ADR-0026), not an operation
  surface. Ops *emit* onto it (the `operation.invoked` audit family); the registry
  does not refactor it.
- **MCP client + ADR-0018 server-initiated servicing** (sampling, elicitation,
  roots): agent-runtime machinery, not operator surface.
- **Process lifecycle**: `fq init`, `fq run`/`fqd`, `fq-cas serve`, shell help.
- **Config loading/precedence** internals (op inputs reference config; they don't
  replace it).

---

## 10. Cross-cutting observations for the refactor

1. **Multiplicity, measured**: each read op exists in four hand-maintained places
   today (`Views` method → `ReadService` RPC → forwarding handler → clap variant +
   dispatch + renderer), five counting the BFF route. Writes are spread across
   three unrelated mechanisms. The registry collapses 4–5 descriptions to one, plus
   the kept renderers.
2. **Three boundary-bypassing paths** must be resolved jointly with 0031:
   in-process `trigger` (links the runtime into the CLI), `workers prune` (direct
   store write, no event), `agent list` (local-FS truth diverging from the
   daemon's registry). All three are invisible to any future auth/audit layer
   until they become ops.
3. **Daemon-side-only capabilities** hiding in the CLI: `status`'s JetStream
   management reads and `health`'s NATS checks cannot be performed by a thin
   client — they must transplant regardless of the registry decision.
4. **Streams reduce to two patterns**: the `transcript_since` cursor pair (proven,
   drives the dashboard SSE) and raw subject subscribe (`events tail` — the one
   true push endpoint; 0031 proposes cursor-poll re-expression, push additive
   later). `OpKind::Stream` as snapshot+cursor convention covers both consumers.
5. **Retention-coupled semantics** (dead-letters list 30 d, requeue 24 h,
   transcript follow-vs-snapshot gap, reload affects next-trigger-only, drop's
   kill-switch behaviour, requeue non-idempotency) are currently help-text lore —
   as `OpMeta` contract text every derived surface inherits them.
6. **The permission model exists already** (fq-store `Verb`×scope + biscuits,
   default-deny gate). The registry doesn't invent authz; it gives the runtime's
   ops somewhere to declare it — and makes #183's "trim the RPC surface" a
   metadata setting instead of surgery.

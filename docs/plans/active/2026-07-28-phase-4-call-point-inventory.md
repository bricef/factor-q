# Phase 4 — call-point inventory and work breakdown

Every consumer of runtime state that does not go through the
authenticated edge, surveyed at main `1323c62` (2026-07-28). This
document is the basis of the Phase-4 migration (the
[execution plan](2026-07-20-registry-and-split-execution.md)'s ~17
verb flips): each row is a unit of work, grouped into cohorts below,
dispatchable to the fleet or workable in sequence.

**Registry state at survey time**: the daemon serves the `invocation`
view (get/list), `invocation.drop`, and the `turn` atom
(get/list/stream), plus `List(Operation)`. Everything else below
needs its declaration registered first (and where marked, an fq-ops
verb-enum variant or a domain-model amendment).

## A. CLI verbs

Verb numbering is stable and referenced by the cohorts. `CLI` =
`services/fq-runtime/crates/fq-cli/src/lib.rs`, `OPSURF` =
`…/fq-cli/src/operator_surface.rs`.

| # | Verb | Data path today | Goldens | Target op | Op exists? |
|---|---|---|---|---|---|
| 1 | `fq init` | local file writes | — | stays local | n/a |
| 2 | `fq run` | is the daemon | — | becomes `fqd` (Phase 5) | n/a |
| 3 | `fq reload` | NATS publish `fq.control.reload`, fire-and-forget, no liveness check | `reload_human` | `control.reload` command | no — `Control` enum lacks `Reload` |
| 4 | `fq down [--now]` | NATS publish `fq.control.down` + heartbeat liveness gate + shutdown-event wait (`CLI:3909-3992`) | `down_human`, `down_now_human` | `control.down` command | enum + fixture exist; not registered |
| 5 | `fq trigger <agent>` (in-process) | full second execution path: disk registry, WAL write, MCP spawns, real LLM (`CLI:1326-1546`) | none (deliberate) | **retire (D-1)** | n/a |
| 6 | `fq trigger --via-nats` | NATS publish `fq.trigger.<agent>` | `trigger_via_nats_human` | `trigger.publish` command | enum + fixture exist; not registered |
| 7 | `fq dead-letters list` | bus + `operator::list_dead_letters` ephemeral scan | `dead_letters_list_*` | `dead_letter.list` atom | no |
| 8 | `fq dead-letters requeue` | bus + `operator::requeue_dead_letter` | `dead_letters_requeue_*` | `dead_letter.requeue` command | no enum variant |
| 9 | `fq agent list` | CLI's own disk read (skew vs daemon's live registry) | — | `agent.list` view | no — transplant from `ReadService::agents` |
| 10 | `fq agent validate` | local file parse | — | stays local | n/a |
| 11 | `fq events tail` | core-NATS subscribe, silent-drop, non-resumable | — | `event.stream` atom | no |
| 12 | `fq events query` | direct Views → `Views::events` | `events_query_*` | `event.list` atom | no |
| 13 | `fq costs` | direct Views → `Views::costs` | `costs_*` | `cost.summary` report | ReportId + fixture exist; not registered |
| 14 | `fq status` | direct JetStream probe + direct Views (`CLI:1772-1875`) | `status_*` (only Nats::Live goldens) | `control.get` on the synthetic | no |
| 15 | `fq doctor` | direct Views ×4 | `doctor_*` | `control.doctor` report (D-5 gates the roster) | no |
| 16 | `fq invocation list` | **edge** | `invocation_list_*` | — | ✅ DONE (3b) |
| 17 | `fq invocation show` | **edge** | `invocation_show_*` | — | ✅ DONE (3b) |
| 18 | `fq invocation drop` | **four legacy paths at once**: legacy-split migration + control request + direct store opens + local `operator::drop_invocation` (`CLI:4764-4813`) | `invocation_drop_*` | `invocation.drop` — **op exists**; flip = delete the local path, move `--live` halting daemon-side | ✅ **DONE 2026-07-28** |
| 19 | `fq invocation resume` | NATS request/reply `fq.control.invocation.resume` | (invocation_resume.rs suite) | `invocation.resume` command | no — **needs a domain-model amendment** (not among the committed six verbs) |
| 20 | `fq invocation transcript` | snapshot: direct Views; `--follow`: **edge** (3d) | `transcript_*` (snapshot path) | snapshot → `turn.list` + `invocation.get{with_prompt}` | ✅ **DONE 2026-07-28** |
| 21 | `fq workers list` | direct Views, client-side filtering | `workers_list_*` | `worker.list` view (filter moves server-side) | no |
| 22 | `fq workers show` | direct Views | `workers_show_*` | `worker.get` view | no |
| 23 | `fq workers prune` | **direct store write**, no events emitted | `workers_prune_*` ×3 | `worker.prune` command, evented | no — **behaviour change** (see hazards) |
| 24 | `fq connect` | edge (TOFU pairing) | (edge_client_cli.rs) | — | ✅ DONE |
| 25 | `fq ops list` | edge | (edge_client_cli.rs) | — | ✅ DONE |
| 26 | `fq token attenuate` | offline (fq-edge) | — | — | ✅ DONE |
| 27 | `fq version` | build-time consts | — | local stays; daemon build via `control.get` | n/a |

**Count check**: flips remaining 3, 4, 6, 7, 8, 9, 11, 12, 13, 14,
15, 19, 21, 22, 23 = **15**. Verbs 18 and 20 landed 2026-07-28
(cohort 4.0), leaving the 17 the plan estimated less those two.

**Migration gate** (`edge_migration_gate.rs`, added with cohort 4.0):
the remaining legacy call points are counted and asserted, so a flip
that leaves the old path in place as a fallback fails even though its
goldens pass. It counts `open_views(` and `control_plane::operator::`
in fq-cli production source — **10 at the start of Phase 4, 8 now,
zero at the end**. Daemon-side uses carry `allow-runtime-internals:`
and are exempt: fq-cli hosts both client and daemon until the Phase-5
binary split, so the edge's own command handlers calling runtime
internals is the architecture, not debt.

**Store-open gate**: 7 sanctioned direct-open sites at Phase-4 start,
**5 now** (verb 18 took two); end-state is 4 (daemon + init only).
An eighth marker sanctioned nothing — orphaned above an unrelated
comment when the open it guarded moved — and was removed with verb
18, which is why the earlier count of 8 here was wrong. The unmarked
bypass class is `open_views` (`CLI:4140`) — used by verbs 12, 13, 15,
21, 22 — which that gate does not match; the migration gate does.

## B. Read service / dashboard

14 RPCs (`read_service.rs:121-198`); the dashboard is a pure
read-service client (its only store/bus references are in tests).

- `workers` / `worker`: **no consumer at all** — transplant sources
  for `worker.list`/`worker.get`, then delete.
- `agents` / `agent`: answer from the daemon's **live**
  `SharedRegistry` — the transplant source for `agent.list`/`agent.get`
  (and fixes verb 9's disk-read skew).
- `version` is the frozen build-skew probe (`read_service.rs:126`);
  its edge successor must not casually inherit the freeze.
- Remaining RPCs (`health`, `active_invocations`, `invocations`,
  `invocation`, `transcript`, `transcript_since`, `events`, `costs`,
  `agent_costs`) map onto ops the CLI cohorts already require; the
  dashboard re-points (attenuated read-only token) once its ops
  exist, then the read service retires (plan Phase 4.3/4.4).

## C. Control bindings and other consumers

- `fq.control.*` subjects (D-2): `reload` and `down` are fire-and-
  forget core NATS; `invocation.resume` is request/reply.
  `fq.control.drain` does not exist in code (stale prose in
  daemon_shutdown.rs). `invocation.drop`'s binding is **gone** (verb
  18, 2026-07-28) — and with it a failure class, not just a subject:
  core NATS answers "no responders" the instant nobody owns a
  subject, which the client read as "inactive or stuck, drop
  directly", so any unowned window silently bypassed the liveness
  guard. That is what the listener's subscribe-before-recovery
  ordering was defending. Over the edge an unreachable daemon is a
  connection error, not a licence to proceed, so the guard fails
  closed by construction. **Expect the same shape in `resume`'s
  flip** (verb 19): a request/reply guard whose absence reads as
  permission is the bug pattern to look for, not the ordering fix.
- In-process `fq trigger` (D-1): retiring it removes the CLI's WAL
  writer, MCP child-process lifecycle, pricing loader, and genai
  dependency.
- First-party adapters (`fq-cron`, `github-watcher`) publish
  `fq.trigger.*` under the wire-contract SPI — sanctioned, not
  flips. `github-watcher`'s outcome-tracking **subscribe** is a raw
  bus read and a future `event.stream` consumer.
- `ops/dogfood/deploy.sh` depends on `fq down`'s
  exit-0-means-confirmed contract — the `control.down` flip must
  preserve it.
- `fq-store`/`fq-cas`: Phase 7, out of scope.

## Work breakdown — cohorts

Ordered so each PR's dependencies are already merged. Every flip is
golden-identical unless flagged. Acceptance for every cohort member:
the verb speaks only the edge; its goldens pass unchanged through the
edge harness; the store-open gate count never grows; `fq-cli` calls
no `operator::*` when the phase completes.

**4.0 — pure flips over existing ops** — ✅ **done 2026-07-28**
1. Verb 18 `invocation drop` — its own PR, as planned. Delete the
   CLI's legacy implementation, relocate `--live` halting into the
   daemon's command handler, `--json`/human output preserved
   (hazard H1). Goldens byte-identical but **moved harness**: they
   ran against seeded stores with no daemon, which is exactly what
   the flipped verb cannot do.
2. Verb 20 snapshot — `invocation transcript` reads `turn.list` +
   the opening prompt from `invocation.get`; byte-identical rendering
   via the turn→entry bridge.

*"No declarations needed" was wrong.* `invocation.get` carried no
prompt — the opening prompt is the only part of a conversation that
never became an event, living solely in the WAL — so verb 20 needed a
schema change after all, via an opt-in `with_prompt` on the Get key.
That is a projection flag on an identity key, taken deliberately
because the prompt is the view's one unbounded field; it is a wart the
domain model should eventually resolve, most likely by making the
prompt an atom like everything else. **Check the target op actually
returns what the verb renders before calling a cohort "pure".**

**4.1 — view transplants** *(read service is the donor)*
3. `worker` view (`worker.get`/`worker.list`, index rows, server-side
   filters) + verbs 21, 22 flip; delete the two dead read-service
   RPCs in the same PR.
4. `agent` view from `SharedRegistry` + verb 9 flip (fixes the
   disk-read skew by construction).

**4.2 — atoms and the event surface**

*Carried in from verb 20:* **a failed LLM call publishes no event at
all.** `dispatch_llm` closes the WAL row `is_error = true` and returns
before the publish, so the fact never reaches the log — the WAL-backed
transcript rendered it as an `[error]` entry, and the Turn-backed one
cannot. No golden covers it, so the loss passed every acceptance
criterion we wrote. The event log has a hole where an atom belongs;
closing it needs a payload that does not demand
content/stop_reason/usage. Do this before or with the event surface,
since `event.list` is where an operator would otherwise have to go
looking.

5. `event` atom (`event.list`) + verb 12 flip.
6. `event.stream` + verb 11 flip (sequence-resumable tail replaces
   silent-drop subscribe).
7. `dead_letter` atom (`dead_letter.list`) + verb 7 flip.

**4.3 — commands** *(each needs its enum variant; resume needs a
model amendment first)*
8. `trigger.publish` + verb 6 flip.
9. `control.down` + verb 4 flip — preserve the liveness gate and the
   deploy script's exit contract (hazard H3).
10. `control.reload` (+ `Control::Reload` variant) + verb 3 flip —
    gains an ack the fire-and-forget path never had.
11. `dead_letter.requeue` (+ enum variant) + verb 8 flip.
12. `worker.prune` (+ enum variant) + verb 23 flip — **evented**
    mutation; reviewed golden change, not byte-identical (hazard H2).
13. `invocation.resume` — domain-model amendment PR first (new verb
    on Invocation), then registration + verb 19 flip; retire the two
    request/reply control subjects with it.
14. Retire D-1 (in-process trigger) and the remaining `fq.control.*`
    bindings; daemon banner updated.

**4.4 — reports, synthetic, dashboard**
15. `cost.summary` report + verb 13 flip.
16. `control.get` (status: version, health, stream probes move
    daemon-side) + verb 14 flip; `control.doctor` report + verb 15
    flip (D-5 decides the exposed roster).
17. Dashboard re-point over an attenuated read-only token; read
    service retires; `version`-probe freeze honoured explicitly.

## Hazards

- **H1** (verb 18): ✅ resolved 2026-07-28. It rode *four* legacy
  paths, not three (the legacy-split migration was the fourth). The
  flip was mostly deletion — a listener task, two bus methods, a
  request/response pair, ~296 lines of `lib.rs` — and it closed the
  no-responders bypass described in section C. Two consequences to
  carry forward: dropping now requires a running daemon (nothing
  under `ops/`, `scripts/`, `.github/` invokes it), and a `Receipt`
  carries AtomRefs and never state, so a verb printing ids the client
  cannot mint needs a gated follow-up read — the 3c read-your-writes
  idiom, which every remaining command flip will also need.
- **H2** (verb 23): `worker.prune` currently mutates silently; the
  flip adds events, so observable behaviour changes — its golden
  update is reviewed, not mechanical.
- **H3** (verb 4): `deploy.sh` treats `fq down` exit 0 as confirmed
  shutdown; the flip must keep that contract.
- **H4** (verb 14): `fq status` does JetStream admin introspection a
  thin client cannot keep; it moves inside `control.get`, and the
  read service's frozen `version` probe semantics must be preserved
  deliberately or dropped deliberately — not inherited by accident.

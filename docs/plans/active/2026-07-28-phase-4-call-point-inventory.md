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

`CLI:<lines>` coordinates are as of the survey commit. Since 2026-08-06
that file is split one module per verb group (#189) — `control.rs` for
verbs 3–4, `trigger.rs` for 5–6, `dead_letters.rs` for 7–8, `agents.rs`
for 9–10, `costs.rs` for 13, `status.rs` for 14, `doctor.rs` for 15,
`invocations.rs` for 16–20, `resume.rs` for 19's daemon half,
`workers.rs` for 21–23, `connections.rs` for 24–26, `version.rs` for 27 —
so read a `CLI:` reference as "the verb", not as a live line number. The
migration gate's count is unchanged by the split: it scans every file
under `src/` and exempts sibling `foo/tests.rs` fixtures the same way it
exempts inline `#[cfg(test)]` modules.

| # | Verb | Data path today | Goldens | Target op | Op exists? |
|---|---|---|---|---|---|
| 1 | `fq init` | local file writes | — | stays local | n/a |
| 2 | `fq run` | is the daemon | — | becomes `fqd` (Phase 5) | n/a |
| 3 | `fq reload` | NATS publish `fq.control.reload`, fire-and-forget, no liveness check | `reload_human` | `control.reload` command | no — `Control` enum lacks `Reload` |
| 4 | `fq down [--now]` | NATS publish `fq.control.down` + heartbeat liveness gate + shutdown-event wait (`CLI:3909-3992`) | `down_human`, `down_now_human` | `control.down` command | enum + fixture exist; not registered |
| 5 | `fq trigger <agent>` (in-process) | full second execution path: disk registry, WAL write, MCP spawns, real LLM (`CLI:1326-1546`) | none (deliberate) | **retire (D-1)** | n/a |
| 6 | `fq trigger --via-nats` | NATS publish `fq.trigger.<agent>` | `trigger_via_nats_human` | `trigger.publish` command | enum + fixture exist; not registered |
| 7 | `fq dead-letters list` | bus + `operator::list_dead_letters` ephemeral scan | `dead_letters_list_*` (**byte-identical**) | `dead_letter.list` atom, keyed on the log sequence | ✅ **DONE 2026-08-06** |
| 8 | `fq dead-letters requeue` | bus + `operator::requeue_dead_letter` | `dead_letters_requeue_*` | `dead_letter.requeue` command | no enum variant |
| 9 | `fq agent list` | CLI's own disk read (skew vs daemon's live registry) | **created in 4.1** (none existed) | `agent.list` view | ✅ **DONE 2026-08-01** |
| 10 | `fq agent validate` | local file parse | — | stays local | n/a |
| 11 | `fq events tail` | core-NATS subscribe, silent-drop, non-resumable | **created in 4.2** (none existed) | `event.stream` atom | ✅ **DONE 2026-08-05** |
| 12 | `fq events query` | direct Views → `Views::events` | `events_query_*` (**reviewed change** — a daemon-backed listing contains the daemon's own events; same correction as 4.1's roster) | `event.list` atom, served from the projection index (`Atom::with_index`; rows carry `seq` so `event.get` walks from any of them) | ✅ **DONE 2026-08-06** |
| 13 | `fq costs` | direct Views → `Views::costs` | `costs_*` | `cost.summary` report | ReportId + fixture exist; not registered |
| 14 | `fq status` | direct JetStream probe + direct Views (`CLI:1772-1875`) | `status_*` (only Nats::Live goldens) | `control.status` report (scope `Control`) — **not** `control.get`; a synthetic has no Get (2026-08-06) | no |
| 15 | `fq doctor` | direct Views ×4 | `doctor_*` | `control.doctor` report (D-5 gates the roster) | no |
| 16 | `fq invocation list` | **edge** | `invocation_list_*` | — | ✅ DONE (3b) |
| 17 | `fq invocation show` | **edge** | `invocation_show_*` | — | ✅ DONE (3b) |
| 18 | `fq invocation drop` | **four legacy paths at once**: legacy-split migration + control request + direct store opens + local `operator::drop_invocation` (`CLI:4764-4813`) | `invocation_drop_*` | `invocation.drop` — **op exists**; flip = delete the local path, move `--live` halting daemon-side | ✅ **DONE 2026-07-28** |
| 19 | `fq invocation resume` | NATS request/reply `fq.control.invocation.resume` | (invocation_resume.rs suite) | `invocation.resume` command | no enum variant — but **amendment landed 2026-08-05**; see section C |
| 20 | `fq invocation transcript` | snapshot: direct Views; `--follow`: **edge** (3d) | `transcript_*` (snapshot path) | snapshot → `turn.list`, and nothing else (the prompt is a Turn) | ✅ **DONE 2026-07-28** |
| 21 | `fq workers list` | direct Views, client-side filtering | `workers_list_*` (**reviewed change** — see 4.1) | `worker.list` view (filter moved server-side) | ✅ **DONE 2026-08-01** |
| 22 | `fq workers show` | direct Views | `workers_show_*` | `worker.get` view | ✅ **DONE 2026-08-01** |
| 23 | ~~`fq workers prune`~~ | **direct store write**, no events emitted | ~~`workers_prune_*` ×3~~ | **none — verb deleted**, replaced by a daemon retention sweep | ✅ **RETIRED 2026-08-10** |
| 24 | `fq connect` | edge (TOFU pairing) | (edge_client_cli.rs) | — | ✅ DONE |
| 25 | `fq ops list` | edge | (edge_client_cli.rs) | — | ✅ DONE |
| 26 | `fq token attenuate` | offline (fq-edge) | — | — | ✅ DONE |
| 27 | `fq version` | build-time consts | — | local stays; daemon build via `control.status` | n/a |

**Count check**: flips remaining 3, 4, 6, 8, 13, 14, 15, 19, 23 =
**9**. Verbs 18 and 20 landed 2026-07-28 (cohort 4.0); verbs 21, 22
and 9 landed 2026-08-01 (cohort 4.1); verbs 11, 7 and 12 landed
2026-08-05/06, which completes cohort 4.2.

**Migration gate** (`edge_migration_gate.rs`, added with cohort 4.0):
the remaining legacy call points are counted and asserted, so a flip
that leaves the old path in place as a fallback fails even though its
goldens pass. Daemon-side uses carry `allow-runtime-internals:` and
are exempt: fq-cli hosts both client and daemon until the Phase-5
binary split, so the edge's own command handlers calling runtime
internals is the architecture, not debt.

It counts `open_views(`, `control_plane::operator::`, and — since
cohort 4.1 — `AgentRegistry::load_from_directory`, and — since cohort
4.2 — client-side `.subscribe`. **10 at the start of Phase 4, 7 now,
zero at the end** — but read that 7 against a pattern set twice as
wide as the one the 10 was measured with.

The count went **6 -> 7** with verb 9, and that is worth reading
carefully, because a rising ratchet normally means debt was added.
Here it means the gate was under-reporting. Adding the registry
pattern admitted four sites: two daemon-side (marked), verb 9's own
listing (removed by its flip), and **verb 5's in-process `fq
trigger`**, which loads the disk registry in client code. The last is
a genuine backlog item the gate had been blind to — the lesson being
that a gate's *coverage* is as load-bearing as its number, and a
falling count over a too-narrow pattern can flatter the work.

**Store-open gate**: 7 sanctioned direct-open sites at Phase-4 start,
**5 now** (verb 18 took two); end-state is 4 (daemon + init only).
An eighth marker sanctioned nothing — orphaned above an unrelated
comment when the open it guarded moved — and was removed with verb
18, which is why the earlier count of 8 here was wrong. The unmarked
bypass class is `open_views` — now used only by verbs 13 and 15 —
which that gate does not match; the migration gate does.

## B. Read service / dashboard

14 RPCs (`read_service.rs:121-198`); the dashboard is a pure
read-service client (its only store/bus references are in tests).

- `workers` / `worker`: ✅ **deleted 2026-08-01**. Confirmed to have
  no caller anywhere — dashboard source *and* assets, adapters,
  tools, experiments — except the read service's own round-trip test.
- `agents` / `agent`: ✅ transplanted 2026-08-01, but **kept**: the
  dashboard consumes them (`fq-dashboard/src/main.rs:613`, `:625`)
  and has an agents page, so they retire with the rest of the read
  service at 4.4. The projection now lives in one place
  (`fq-runtime/src/agent_view.rs`) that both the RPCs and the Agent
  view call, so the two surfaces cannot drift in the interim.
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
  closed by construction.
- **`resume` was checked for that shape and does not have it**
  (2026-08-05). Its client bails on any request error, so the
  no-responders answer surfaces as "start the daemon first" rather
  than as permission — `invocation_resume.rs`'s daemon-down case
  pins it. Retiring the subject removes the class regardless, but the
  hazard verb 19 actually carries is a different one; see the
  amendment below.
- **Domain-model amendment for `invocation.resume`** — landed
  2026-08-05 in
  [the operator-surface domain model](../../design/committed/operator-surface-domain-model.md),
  which unblocks cohort 4.3 item 13. What it decided:
  - `invocation.resume` is a declared verb with the same shape as
    `invocation.drop` (Write invocation, receipt-returning). Two
    actions on one resource should not be reachable two different
    ways.
  - **NATS is not an external control surface** — recorded as an
    architectural principle, not a note about one verb. The bus is the
    internal event log and coordination substrate; every operator
    action is a declared op on the edge. `fq.control.*` stops being an
    entry point for anything outside the daemon.
  - Two table entries that would otherwise have been minted into code
    here are reconciled: `traversal.run` is marked **planned** (the
    graph executor is held, #414 — nothing named `traversal` exists),
    and `deadletter.requeue` becomes `dead_letter.requeue`, which is
    what `Domain::DeadLetter`'s snake_case segment renders and what
    rows 7/8 above already say.
  - Row 19's "committed six verbs" was reading a model that disagreed
    with itself: the verb table and the deltas prose said six, while
    the appendix's domain-verb list named seven — the extra being
    `traversal.run`. Post-amendment the two agree at **seven current**
    domain verbs (the six real ones plus resume), with the traversal
    trio pulled out and marked planned.
- **Verb 19's real hazard is `drop`'s second lesson, not its first.**
  PR #445 established that `invocation.drop` must never report
  `NotFound` for work the daemon is running, because the liveness
  authority and the identity authority do not share a clock. Resume
  has the same two-authority shape, arranged differently, and the
  amendment states its invariant: **it must never refuse an
  invocation whose state it has already changed, and never act on a
  terminal decision it cannot yet see.** Concretely, for the flip:
  - The interrupted-result injection is a committed transaction, and
    two steps after it (stored-identity validation, agent lookup)
    still report failure — while the injection is exactly what makes
    the invocation stop being Ambiguous, so a failed resume strands
    work no second resume will accept. Order every fallible step
    before the injection; past it the command must be infallible and
    answer with a receipt naming the `invocation.operator_resumed`
    atom. Note this makes the audit publish load-bearing rather than
    best-effort — a receipt needs a sequence to name.
  - The terminal precondition reads folds an async consumer writes,
    so a resume issued inside the drop→consumer window is accepted
    and re-drives a dropped invocation (#383, open). The edge's read
    gate is the fix already in the building: evaluate terminality at
    or after a watermark, not from whatever the fold currently says.
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
2. Verb 20 snapshot — `invocation transcript` reads `turn.list`;
   byte-identical rendering via the turn→entry bridge.

*"No declarations needed" was wrong.* `invocation.get` carried no
prompt, so verb 20 needed a schema change after all, via an opt-in
`with_prompt` on the Get key — a projection flag on an identity key,
taken deliberately because the prompt is the view's one unbounded
field, and recorded here as a wart the domain model should eventually
resolve by making the prompt an atom like everything else. **Check the
target op actually returns what the verb renders before calling a
cohort "pure".**

*And the reason given for the wart was itself wrong* (corrected
2026-08-05). The claim was that the opening prompt "never became an
event and lives solely in the WAL". It had been an event all along:
the runner publishes `EventPayload::LlmRequest` immediately before
every provider call, and its payload carries the `messages` sent —
the opening call's being exactly the system prompt and the trigger's
user message. `TurnFold` simply never matched `LlmRequest`. Teaching
it to (`TurnAction::Prompt`, folded from the opening request) makes
the prompt a Turn like everything else, and the whole workaround —
`with_prompt`, `InvocationDetailView::prompt`, `Views::invocation_prompt`,
the second edge call — deletes. **When a value seems to be missing
from the log, grep the publish sites before designing around its
absence.**

**4.1 — view transplants** — ✅ **done 2026-08-01**
3. `worker` view (`worker.get`/`worker.list`, index rows, server-side
   filters) + verbs 21, 22 flip; the two dead read-service RPCs
   deleted in the same PR.
4. `agent` view from `SharedRegistry` + verb 9 flip. The donor RPCs
   **stayed** — the dashboard consumes them until 4.4 — so the
   projection was extracted to one shared module both call.

Four things this cohort taught, which the later ones inherit:

- **A gate's coverage matters as much as its number.** Verb 9's
  legacy path was a disk registry load, which the migration gate did
  not match, so flipping it would have left the count untouched. The
  pattern was widened, which raised the count 6 -> 7 by admitting
  verb 5's in-process `fq trigger` — a real backlog item the gate had
  been blind to. Check a verb's legacy path is *counted* before
  trusting a flip to move the number.
- **A golden can pin an impossible world.** `workers_list_*` expected
  a roster with no daemon row, though the daemon has always
  self-registered, and a worker `alive` with a stale heartbeat, which
  the sweep repairs on sight. The flip did not break them; it
  revealed they described a store no daemon had touched. Updating
  them was correct, and is the one reviewed golden change of the
  migration so far. When a golden breaks on a flip, ask whether it
  was right before assuming the flip is wrong.
- **A verb with no goldens needs its oracle built first.** Verb 9 had
  none, so goldens were written against the *old* code path and made
  to pass before anything moved. Goldens authored after a flip only
  record what the flip produced.
- **Grep the tests for consumers, not just the source.**
  `daemon_shutdown.rs` drove `fq workers list --json` *after the
  daemon exited*, which the flipped verb cannot do by construction.
  The section-B survey looked at production callers only.

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

**4.3 — commands** *(each needs its enum variant; resume's model
amendment landed 2026-08-05 — see section C)* — items 8, 9, 10 and 14
**done**; 11 and 13 are their own flips; **12 is blocked, see below**.
8. `trigger.publish` + verb 6 flip. **Its receipt is empty, and that is
   the finding**: a trigger has no identity to name. The wire contract
   makes the message body the payload itself — opaque, written directly
   by external publishers — and `publish_trigger` returns only the
   JetStream ack sequence, which is a position, not a name. Giving
   triggers an identity is a wire-contract change with external
   consumers and is being decided separately.
9. `control.down` + verb 4 flip — preserve the liveness gate and the
   deploy script's exit contract (hazard H3).
10. `control.reload` (+ `Control::Reload` variant) + verb 3 flip —
    gains an ack the fire-and-forget path never had.
11. `dead_letter.requeue` (+ enum variant) + verb 8 flip.
12. ~~`worker.prune` (+ enum variant) + verb 23 flip~~ — **done
    2026-08-10, by deletion.** The verb is gone, there is no
    `Worker::Prune` variant, and **no `worker_pruned` event type was
    added.** Reclaiming stale `coordination_worker` rows is a daemon
    retention sweep (`state.stale_worker_retention_days`, default 7
    days, `-1` disables). See
    [ADR-0006 Appendix E](../../adrs/accepted/0006-registry-first-api.md#appendix-e--amendment-workerprune-is-retired-not-evented-2026-08-10).

    The blocker recorded below was real, and the way out was to
    question its premise rather than pay it. It read:

    > **Blocked, and the blocker is structural rather than incidental.**
    > The evented design is not optional decoration: a receipt carries no
    > state (D3), so the only way the flipped verb can still name the
    > workers it removed — which its goldens pin — is to co-emit one audit
    > atom per eviction and let the client walk the receipt's `AtomRef`s
    > with a gated `event.get`. That needs an event type
    > (`worker_pruned`), and no existing payload means it. The variant and
    > its two match arms have to land in `events.rs`, which is pinned at
    > its exact size in `.file-size-baseline` (zero slack) […] bless a
    > small budget bump for `events.rs`, or split it first.

    Every branch of that cost — the event type, the `events.rs` budget,
    the receipt walk — was being paid to preserve **an operator verb
    that should not exist**. `worker_id` is the daemon's `runtime_id`, a
    fresh UUID per run, so `coordination_worker` grows by a row per
    restart and prune was the only thing reclaiming it. The system
    should not depend on operator remediations to work normally. Once
    the sweep is the daemon's own housekeeping there is no operator
    decision to audit, so no receipt to fill and no event to mint:
    `events.rs` is untouched and its budget is unchanged.

    Two things the evented design would have inherited, and which the
    rewrite had to fix instead:
    - **`stale` is not `prunable`.** Prune deleted on `status = 'stale'`
      alone — a ~30s heartbeat lapse. On a timer that destroys
      `fq workers list --stale-only` as a diagnostic. Deletion got its
      own window, in days.
    - **The ownership guard.** `coordination_invocation_owner.worker_id`
      had no check at all. A latent bug, not a can't-happen — see the
      note under hazard H2.
13. `invocation.resume` — the amendment is done; this is now
    `Invocation::Resume` + registration + verb 19 flip, retiring the
    request/reply control subject with it. **Implement the invariant
    deliberately** (section C): fallible steps before the injection,
    receipt after it, terminality read at a watermark.
14. Retire D-1 (in-process trigger) and the remaining `fq.control.*`
    bindings; daemon banner updated. **Done** for reload and down — both
    listeners, both subjects, both bus method pairs and the down-mode
    body markers are deleted. `fq.control.invocation.resume` is the last
    one standing and retires with item 13.

**4.4 — reports, synthetic, dashboard**
15. `cost.summary` report + verb 13 flip.
16. `control.status` **report** (version, health, stream probes move
    daemon-side) + verb 14 flip; `control.doctor` report + verb 15
    flip (D-5 decides the exposed roster). **Three things land here,
    from the 2026-08-05/06 model amendment**
    ([ADR-0006 Appendix D](../../adrs/accepted/0006-registry-first-api.md)):
    - **The machinery reads are reports, not a Get.** A synthetic has
      no Get, no key, no filter and no state schema — it is a
      permission scope hosting `control.down`/`control.reload`. Declare
      `Control` report identities (`control.status`, `control.doctor`)
      rather than reaching for `OpId::Get(Domain::Control)`.
    - **Realise it in `fq-ops` in this cohort**, since the declaration
      lands here anyway: drop `state_schema` from `Synthetic` and its
      `new::<State>` type parameter, drop `OpId::Get` from
      `derived_ops` for synthetics and the matching resolve arm, fix
      the three synthetic assertions in `fq-ops/tests/registry.rs`, the
      `ControlState` fixture, and `opid.rs`'s module doc — then
      **regenerate `tests/snapshots/exemplar_registry.json`**, whose
      `synthetic` entry serialises a `state_schema` today. A reviewed
      snapshot change, not a mechanical one.
    - **`control.status` carries the registry's state, load errors
      included** — so `agent.list`'s sum-typed index row
      (`AgentEntryView::LoadError`) retires and verb 9's rendering
      re-sources its error block from here. A reviewed golden change,
      not byte-identical.
17. Dashboard re-point over an attenuated read-only token; read
    service retires; `version`-probe freeze honoured explicitly.
    **Resolves a live divergence**: `read_service.rs` still answers
    the dashboard from `Views::transcript`, which stamps entries with
    the WAL's `intent_at`, while the flipped CLI reads the Turn atom
    and stamps `envelope.timestamp`. Since one is written before the
    call and the other after it, the two surfaces disagree on the
    same transcript's timestamps by the operation's duration until
    this lands. Human rendering never prints the field, so the
    divergence is only visible to `--json` consumers correlating
    against WAL or log data. If intent-time is wanted back, it
    belongs on the atom (cohort 4.2 shape work) — not joined across
    the atom boundary at the edge.

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
- **H2** (verb 23) — **resolved 2026-08-10, differently than planned.**
  The hazard was "the flip adds events, so observable behaviour
  changes". There is no flip and no events: the verb is deleted and its
  three goldens with it. `status_human.golden` changed too — `fq status`
  no longer offers prune as a remediation, because there is nothing for
  the operator to do.

  Retiring it surfaced a **latent bug** worth recording, since a timer
  would have made it routine. `prune_stale_workers` deleted on worker
  status alone, with no check on `coordination_invocation_owner`, whose
  `worker_id` is a plain column and not a foreign key — so a delete
  stranded the ownership row silently rather than failing. It is
  reachable, not theoretical: **nothing consumes `worker.orphaned`.**
  It is published for observability and never handled, so the only
  things that clear an `in_flight` owner row are the worker's own
  `invocation.archived` (which a dead worker never sends) and the
  operator recovery path. A worker can therefore sit stale owning live
  work indefinitely, well past any retention window. The sweep now
  refuses to collect a worker that still owns `in_flight` or
  `ambiguous` invocations, and logs a warning when it declines —
  because that state means a stuck invocation nobody has recovered.
- **H3** (verb 4): `deploy.sh` treats `fq down` exit 0 as confirmed
  shutdown; the flip must keep that contract.
- **H4** (verb 14): `fq status` does JetStream admin introspection a
  thin client cannot keep; it moves inside `control.status`, and the
  read service's frozen `version` probe semantics must be preserved
  deliberately or dropped deliberately — not inherited by accident.

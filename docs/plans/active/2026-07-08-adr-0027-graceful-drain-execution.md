# Graceful-drain deploys — execution plan for `fq drain` (ADR-0027)

**Status:** draft (2026-07-08). The buildable execution plan for
[ADR-0027](../../adrs/accepted/0027-graceful-drain-deploys.md) ("deploys are a
graceful drain — suspend and resume, not kill-and-replace"). It grounds the
ADR's decision in the current runtime, resolves the drain-signal mechanism
against the real worker/control-plane boundary, and sequences the work into
PR-sized steps. Prompted by the M0 flywheel
([close-the-loop plan](2026-07-05-m0-close-the-loop.md)): the dogfood daemon is
redeployed by hand today — `pkill -INT` + rebuild + restart — a manual step
that bottlenecks the loop as it accelerates and breaks the autonomy goal.

## The bet: the deploy primitive already exists

ADR-0027's load-bearing observation is that **factor-q is already a
durable-execution runtime, so a deploy is a _controlled_ recovery cycle** — the
machinery built for crash recovery is the deploy primitive. A survey of the
current code confirms it: most of what `fq drain` needs is already built and
reused unchanged.

| Machinery | Anchor | Role in drain |
|---|---|---|
| Recovery (`scan_in_flight → categorise → resume`) | `worker/recovery.rs:148/61`, resume `worker/reducer/runner.rs:708` | The "resume under the new binary" half — unchanged |
| Step-boundary checkpoint | `worker/reducer/runner.rs:1035` | The exact suspend point; state already durable there |
| ack-on-dispatch + durable pull consumer | `control_plane/dispatcher.rs:241`, `bus.rs:369` | No lost / double-run triggers; un-consumed stay queued |
| `fq reload` control-plane shape | `bus.rs:74/544/565`, listener `fq-cli/src/main.rs:1637` | The command→daemon-acts pattern to clone (its _shape_) |
| Per-worker directed messages | `worker/archive_ack.rs:55` (`fq.worker.{id}.…`) | The transport pattern for the future remote drain hop |
| `ConfigSnapshot` / refresh-between-invocations | [ADR-0020](../../adrs/accepted/0020-mcp-notification-handling.md), `events.rs:650` | A resumed invocation keeps the config it began with |

The genuine delta is one hard thing — a cooperative **suspend-at-boundary**
primitive — plus its plumbing. Everything else is reuse. Correctness therefore
rests entirely on the existing invariants (WAL, recovery, `ConfigSnapshot`,
ack-on-dispatch), exactly as ADR-0027 §Decision.4 requires; no new durability
machinery is introduced.

## The drain signal is a domain method on the `Worker` trait

The central design question is how a `fq.control.drain` message — received
control-plane-side on NATS — reaches an in-flight invocation running inside the
worker's reducer loop, and does so **when worker and control-plane are separate
nodes** (the v2 split the runtime is built toward).

### Correcting the seam

The worker is **not** driven through a tarpc interface. tarpc appears only in
`fq-store`, where `CasService` (`fq-store/src/service.rs:60`) mirrors an
in-process `ContentStore` trait onto a wire trait — a useful _precedent_ for
"domain trait mirrored onto RPC", but it is the content store, not the worker.

The worker seam is the `Worker` `#[async_trait]`
(`worker/mod.rs:108`), today **in-process** (control-plane and worker share the
single `fq run` process; the trait enforces the role boundary at compile time so
v2 is a process split, not a redesign — `worker/mod.rs:9-16`). Its own
doc-comments name the planned remote transport as **NATS**
(`control_plane/dispatcher.rs:88-90`), not tarpc. Whether the worker ever adopts
tarpc for its remote transport is a separate decision; the drain design below is
specified against the _trait_, so it holds under either wire.

The rule the boundary enforces: *"anything the control-plane asks of a worker
goes through the trait; the control-plane has no other handle on worker
internals"* (`worker/mod.rs:14-16`). The dispatcher already calls the worker
this way — `self.worker.run_invocation(...)` on an `Arc<dyn Worker>`
(`control_plane/dispatcher.rs:243`). Drain must ride the same seam.

### The design

1. **A new method on `Worker`** (`worker/mod.rs:108`), the same seam as
   `run_invocation`:

   ```rust
   async fn request_drain(&self, req: DrainRequest) -> Result<(), ExecutorError>;
   // optionally, for the orchestrator to poll toward the deadline T:
   async fn drain_status(&self) -> DrainState;
   ```

2. **Domain types in the worker module** (next to `InvocationOutcome`, _not_ in
   `bus.rs` — the transport must not leak into the domain):

   - `DrainRequest { deadline_ms, reason }` — the command.
   - `DrainState { Running | Draining { since_ms } | Drained }` — the observable
     state, and the worker-local flag the reducer polls.

3. **A new `InvocationOutcome::Suspended { invocation_id }`** variant
   (`worker/mod.rs:61`, today only `Completed` / `BudgetExceeded`) so a clean
   boundary-suspend is reported distinctly from completion or budget exhaustion.
   The dispatcher treats `Suspended` as a benign non-error: nothing to
   redeliver, the WAL row stays in-flight for recovery.

4. **Propagation.**
   - `fq drain` publishes a typed `DrainRequest` on a control subject — reusing
     the `fq.control.reload` _shape_ (`bus.rs:74/544`) but **not its plumbing**:
     reload deliberately does _not_ reach in-flight work (in-flight invocations
     snapshot their agent at trigger time and a registry swap never disturbs
     them — `dispatcher.rs:71-75`); drain must do the opposite.
   - The control-plane's drain listener calls `worker.request_drain(req)`
     **through the `Arc<dyn Worker>`** it already holds.
   - **v1 (co-located):** `ReducerRunner::request_drain` flips an internal
     `DrainState` flag (a worker-local `AtomicBool` / `watch` field on the
     runner). **v2 (remote):** the future NATS remote-worker adapter implements
     `request_drain` by publishing the typed `DrainRequest` to the worker's
     per-worker subject; a worker-side consumer — modelled on the existing
     `ArchiveAckConsumer` (`worker/archive_ack.rs`) — flips the _local_ flag.
     Either way the control-plane only ever touches the trait.
   - `run_loop_inner` polls the flag at the **top of its step loop**
     (`runner.rs:980`), before `reducer.step`. The previous iteration's
     checkpoint (`runner.rs:1035`) is already durable, so it returns
     `Suspended` at a clean, side-effect-free boundary.
   - The new binary starts and the **existing recovery**
     (`scan_in_flight → categorise → SafeResume → resume`) picks the suspended
     invocations up. No new durability machinery.

### Why not an in-process `watch<bool>`

A `tokio::sync::watch<bool>` created in the daemon is bound to the worker's
local runtime. Across a process split the control-plane holds no `Sender` and no
handle to it — it is unroutable by construction — and it is not a trait method,
so it cannot be RPC'd. It also violates the boundary rule: the `Worker` trait is
the _only_ handle the control-plane has. Putting drain on the trait inverts the
ownership correctly: **the signal is domain-typed and trait-routed; the
in-memory flag becomes a purely worker-local implementation detail**,
reconstructed on the worker node from the received `DrainRequest`, never a
shared object spanning the boundary.

## Groundwork already landed

- **SIGTERM capture** (PR #20). `fq run` handled only SIGINT; SIGTERM — the
  signal process managers, `docker stop`, and deploy scripts send — killed it
  abruptly. Now both map to the same clean shutdown. This is the precondition
  for a supervised stop / deploy to be graceful at all — signal capture, not a
  drain.
- **Worker deregister on graceful shutdown** (PR #21). A clean exit now marks
  the coordination row `shutdown` (symmetric with startup `register_worker`)
  instead of leaving it to age into `stale`, cutting the stale-worker cruft.

## Phased plan (PR-sized, riskiest first)

- **PR-1 — the suspend primitive.** `Worker::request_drain` + `DrainRequest` /
  `DrainState` domain types + `InvocationOutcome::Suspended` + the reducer
  boundary poll. Driven by tests, no control command yet. Correctness proven by
  **extending the `sim.rs` fault harness**: suspend-at-boundary → resume →
  `assert_equivalent` to an uninterrupted run. This isolates and property-tests
  the risky core; the v1 flag is an implementation detail of `ReducerRunner`.
- **PR-2 — daemon integration.** Give the (co-located) drain path a handle to
  the `Arc<dyn Worker>`; stop pulling new triggers on drain (adapt the
  dispatcher's shutdown oneshot to distinguish _drain_ from _abort_); track the
  otherwise-detached recovery-resume tasks so they are drainable too.
- **PR-3 — `fq drain` the command.** Clone the reload control-plane wiring
  (`fq.control.drain`, `Commands::Drain`, a drain listener beside the reload
  one); bounded wait on the deadline _T_; `system.shutdown` with
  `reason="drain"`. In v1 the listener calls `request_drain` in-process; the
  NATS-to-worker hop is deferred until the worker actually splits.
- **PR-4 — manual deploy script: deferred, not planned (2026-07-08).** A
  `just deploy` (`fq drain` → await exit → rebuild → restart → health probe)
  was the ADR's suggested first milestone, but any such script — even a
  parameterised, instance-agnostic one — would be superseded the moment the CD
  pipeline below lands. So we skip the stopgap and go straight to automated
  Continuous Deployment. In the meantime `fq drain` is usable by hand: drain,
  rebuild, then re-launch via the instance's own launch script (e.g. the
  dogfood `~/fq-dogfood/run.sh`, which owns that instance's cwd/secrets/NATS).
  This makes the **CD companion below the immediate next maturity step.**
- **Deferred to a companion ADR** (ADR-0027 §Consequences flags these as
  separate): a versioned container image, an auto-deploy watcher (separate
  adapter vs. integrated), and the **health-check + rollback safety layer** — a
  CI-green change can still crash the live workload, so a merge must not be able
  to take the daemon down irrecoverably.

## Decisions settled

- **Drain-signal mechanism:** a domain type routed through the `Worker` trait
  (above), not an in-process channel. `CancellationToken` is unnecessary and not
  a current dependency.
- **Deadline _T_ + fallback:** _T_ is a config field (the tunable-is-config
  pattern already used for `max_iterations`, `config.rs`), default ~120s. Past
  _T_, `fq drain` falls back to the **existing abort → crash-recovery** path
  (those invocations become "ambiguous") — never a mid-tool-call force-kill,
  never a block-forever.
- **Config on resume (for now): naive resume-with-new-config.** A drained
  invocation resumes with the agent definition from the _current_ (new binary's)
  registry; step-0's config is already baked into the replayed `state_blob`, so
  history stays original while future steps use the new config
  (`runner.rs` resume path; the refresh-between-invocations precedent,
  [ADR-0020](../../adrs/accepted/0020-mcp-notification-handling.md)). This is
  acceptable for single-agent invocations today.

## Deferred / open questions

- **Resume under the original config (the "correct" behavior).** The principled
  choice is to finish an in-flight workflow under the exact config it began
  with. That requires durably **caching versioned graph and agent-definition
  snapshots** keyed per invocation — which we do not do today. **Captured here
  as deliberate tech-debt to revisit**, and it compounds once the
  [graph executor](2026-07-07-graph-executor-two-node-vertical.md) lands (a
  multi-node traversal spanning a deploy would otherwise mix definition
  versions mid-graph). Naive resume-with-new-config ships first; the versioned
  snapshot cache is the follow-up.
- **Step-boundary reachability.** A step wrapping a long tool call (a `just ci`
  build, a minutes-long model call) will not reach a boundary quickly; the
  deadline _T_ bounds the wait, and the mid-step case inherits crash-recovery's
  existing categorisation. Confirm the poll site and the precise idempotent
  resume point for a step that has already emitted an external side effect.
- **Broadcast vs. per-worker drain subject.** `fq.control.drain` (broadcast,
  like reload) vs. `fq.worker.{id}.control.drain` (directed, like
  `archive_acked`). Broadcast suffices for the single-daemon case; the directed
  form is the natural fit once multiple workers exist.
- **Worker remote transport.** The worker's planned remote transport is NATS,
  not tarpc; whether it adopts tarpc later is orthogonal to this drain work but
  worth a deliberate decision when v2 is built.
- **Health-check + rollback.** Definition (consumers connected? a synthetic
  trigger round-trips?) and the rollback mechanism (image pinning, versions
  retained) — likely the companion ADR named above.

## Interlocks

Builds directly on the reducer WAL / recovery, `ConfigSnapshot`
([ADR-0020](../../adrs/accepted/0020-mcp-notification-handling.md)), and
ack-on-dispatch; composes with
[ADR-0010](../../adrs/accepted/0010-agent-execution-isolation.md) (isolation)
and reuses the
[ADR-0026](../../adrs/accepted/0026-event-log-system-of-record.md) event log to
record a deploy as a first-class event. The safety-layer theme mirrors the
trigger-side cost-control gap noted in the [backlog](../backlog.md): every place
autonomy replaces a human checkpoint needs a safety layer where the checkpoint
was.

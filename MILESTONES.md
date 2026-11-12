# Milestones

A milestone is a state the project reaches, with a test that says it has
been reached. This file says what each one means and why it exists. The
GitHub [milestones](https://github.com/bricef/factor-q/milestones) hold the
issues and show the progress; this file holds the meaning.

For what runs today, see [STATUS.md](STATUS.md). For where the project is
going and why, see [VISION.md](VISION.md).

## How milestones are used

- **A milestone has an exit.** Each one below says what is true when it is
  reached, as something that can be checked. A theme with no exit is a
  label, not a milestone.
- **An issue belongs to the milestone whose exit needs it.** GitHub allows
  an issue one milestone. An issue that no milestone's exit needs has none,
  and that is the normal case: most open issues are in no milestone.
- **A milestone is closed when its exit holds.** One that is dropped
  because the plan moved is closed too, and says "retired" in its
  description; it is listed under [Retired](#retired) below.
- **Plans describe the work; milestones say where it gets to.** Each
  milestone names the plan or review it comes from. A plan sequences the
  work and records what happened ([docs/plans](docs/plans/README.md)); the
  milestone is the state at the end of it.
- **This file and GitHub change together.** Adding, renaming, re-scoping or
  closing a milestone changes its section here in the same pull request.
  The GitHub description is one paragraph; this file is the longer account.
  Issue lists and counts are not copied here, because they go stale: follow
  the link to the milestone.

## The sequence

The active track is **production readiness**, set out in the
[2026-09-03 review](docs/reviews/2026-09-03-production-readiness-review.md)
as five phases. Phases 0 and 1 were finished in September 2026, before the
project used milestones for this track.

| Milestone | State | Comes after |
|---|---|---|
| [Production readiness, Phase 2](#production-readiness-phase-2-the-record-is-trustworthy) | In progress | Phases 0 and 1 (done) |
| [v0.0.1](#v001-first-tagged-release) | Not started | Phase 2 |
| [Production readiness, Phase 3](#production-readiness-phase-3-bounded-growth-and-operable-state) | Not started | Phase 2 |
| [M1 Net zero](#m1-net-zero-loop-instrumentation-and-reliability) | In progress, alongside the track above | M0 (met 2026-07-20) |
| [Storage M3–M5](#storage-m3m5-extraction-embedding-service-wiring) | Parked | Phase 2 |
| [Security, isolation and guardrails](#security-isolation-and-guardrails) | Parked | Phase 2 for its first item |
| [Beta gate](#beta-gate) | Standing gate | Nothing; it fills as alpha goes on |

Three milestones have been [reached](#reached) and one
[retired](#retired). Some planned work has
[no milestone yet](#planned-with-no-milestone-yet).

One name is used for two things. "Phase 2" in this file means Phase 2 of
the production-readiness review. The older product plan
([MCP, memory and skills](docs/plans/active/2026-04-11-phase-2-mcp-and-memory.md))
is also called Phase 2; its remaining work is the storage milestone below
and the Memory and Skills services after it.

## Open milestones

### Production readiness, Phase 2: the record is trustworthy

[GitHub milestone](https://github.com/bricef/factor-q/milestone/9)

**Purpose.** What the event log, the write-ahead log and the stores say
happened is what happened. Phases 0 and 1 made the daemon safe to leave
running and hard to wedge. This phase is about the record it keeps: an
outcome that is decided is never lost, a trigger starts its work once, a
resumed run replays what it should, one oversized result cannot stop an
invocation or its archive, and a store is never left half-upgraded.

**Where it comes from.** Phase 2 of the
[production-readiness review](docs/reviews/2026-09-03-production-readiness-review.md),
ten items. The work found more than the review listed, and those findings
were added as they were confirmed: serialised schema migrations for both
stores, the gaps the exactly-once design record leaves open, a drain that
could lose a trigger, and the size of the archive event. One item, sending
only the new part of the conversation in each `llm.request`, was deferred
to Phase 3 by decision.

**Reached when** every issue in the milestone is closed and these hold on
`main`:

- crash tests for a lost `completed` publish, a resume after sampling, and
  an oversized tool output, all green;
- both July 2026 incidents replayed against a real broker, green
  (`tests/july_incident_replay.rs`);
- the dependency audit green, on every merge and on the daily run.

The running history, the decisions and the exit criteria as amended are in
the comments on [#410](https://github.com/bricef/factor-q/issues/410).

**What it holds back.** The review parks the storage track, Memory and
Skills, the multi-node work and container isolation until this phase is
done, because none of them makes the record harder to lose. The one
exception it names is the egress proxy, which is the first item after this
phase.

### v0.0.1: first tagged release

[GitHub milestone](https://github.com/bricef/factor-q/milestone/11)

**Purpose.** The first release cut from a tag. Until now the only
published artefacts are the rolling `main-latest` builds, and the release
workflow has never run.

**Where it comes from.** The maintainer's stated next steps after Phase 2,
recorded on 2026-11-12. Two steps, in order: review every open bug for
whether it still describes the code after Phase 2, then mint the tag.

**Reached when** `v0.0.1` exists as a tagged release built by
`release.yml`.

**Open question.** The review's Phase 4 ends by cutting `v0.1.0`. Whether
`v0.0.1` comes before that or replaces it, and which Phase 4 items it
needs first, is not decided; it is recorded on
[#978](https://github.com/bricef/factor-q/issues/978).

### Production readiness, Phase 3: bounded growth and operable state

[GitHub milestone](https://github.com/bricef/factor-q/milestone/10)

**Purpose.** The stores and streams stop growing without bound, and the
instance can be backed up, restored and watched. Today several tables and
streams grow for the life of the instance, there is no restore drill, and
nothing exports a metric.

**Where it comes from.** Phase 3 of the
[production-readiness review](docs/reviews/2026-09-03-production-readiness-review.md):
retention for the write-ahead log and the ownership table, size limits on
the streams, backups with a restore that has been run once, a post-deploy
health gate with rollback, one metrics endpoint with one alert, and a
decision on how long the event trail lives.

**Reached when**, in the review's words: the worker and control-plane
databases are flat over a seven-day dogfood window; a restore from backup
on a clean host boots and answers `fq status`; the alert has fired once in
a drill.

**State.** Not started. The milestone holds the issues that already exist
for it; most of the review's items have no issue yet and are filed when
the phase begins.

### M1 Net zero: loop instrumentation and reliability

[GitHub milestone](https://github.com/bricef/factor-q/milestone/3)

**Purpose.** [VISION.md](VISION.md) defines M1 as net zero: for every day
of maintainer effort, a day of work comes out. That cannot be argued; it
has to be measured. This milestone is the measuring, and the reliability
of the autonomous loop being measured.

**Where it comes from.** The milestone ladder in [VISION.md](VISION.md),
and the
[measurement instrument plan](docs/plans/active/2026-09-17-measurement-instrument.md),
which sequences five deliverables: the attempt ledger, the objective
diagrams and measures, the human-entered primitives, the trajectory plot,
and the weekly report.

**Reached when** the question "are we at net zero?" has a measured answer:
all five deliverables exist, and the loop runs without routine rescue, so
that what is measured is the system and not the maintainer holding it up.
This milestone is the instrument and not the result. Whether the answer is
yes is M1 itself.

**State.** In progress, alongside production readiness. The ledger, the
measures and the human-entered primitives are on `main`; the trajectory
plot and the weekly report are not built.

### Storage M3–M5: extraction, embedding, service wiring

[GitHub milestone](https://github.com/bricef/factor-q/milestone/5)

**Purpose.** The storage and vector foundation that the Memory and Skills
services are built on. M1 (content-addressed store, index, garbage
collection) and M2 (access control) are done. What remains is extraction
(M3), embedding and retrieval (M4), and the service wiring and SDK (M5).

**Where it comes from.** The
[storage and vector foundation plan](docs/plans/active/2026-06-27-storage-vector-foundation.md).

**Reached when** M3, M4 and M5 are done by that plan's own criteria.

**State.** Parked. The production-readiness review holds this track until
Phase 2 is done. The milestone also carries the design review of the
store's operator surface, which has to come before the store gets a
registry of its own: that was the last phase of the registry work and was
sequenced with M5.

### Security, isolation and guardrails

[GitHub milestone](https://github.com/bricef/factor-q/milestone/6)

**Purpose.** What an agent declares it may reach is enforced and not
advisory: network egress, the filesystem and process boundary, and the
secrets and spend that pass through it.

**Where it comes from.** [SECURITY.md](SECURITY.md) is the posture and the
risk register.
[ADR-0010](docs/adrs/accepted/0010-agent-execution-isolation.md) is
container isolation, accepted and unbuilt.
[ADR-0037](docs/adrs/draft/0037-isolation-sequenced-after-graph-execution.md),
a draft, holds isolation until the graph executor's first pass.

**Reached when** its four issues are settled: the egress proxy and
container isolation built, and secret redaction and cost control at the
trigger boundary each either built or decided against.

**State.** Parked, in a known order. The egress proxy is the first item
after Phase 2, by the review's own exception. Container isolation waits on
the graph executor. The other two await decisions.

This is the least sharply defined milestone here. It has no single test,
and its items become due at different times. It should be split or
restated when the egress proxy is scheduled.

### Beta gate

[GitHub milestone](https://github.com/bricef/factor-q/milestone/8)

**Purpose.** A record of the things accepted during alpha for speed or for
ease of debugging that must not survive into beta.

**Reached when** the milestone is empty. Alpha ends then.

**State.** A standing gate and not a work package. An issue is added each
time such a trade-off is made, so it is expected to grow before it
shrinks.

## Reached

### Graph Readiness

[GitHub milestone](https://github.com/bricef/factor-q/milestone/1),
closed 2026-07-23.

Seven structural and durability items cleared in July 2026 ahead of the
graph executor work: one name for each event variant, the
parent-directory fsync, a sequence column on the write-ahead log,
agent-declared task outcomes, the decision on
[ADR-0006](docs/adrs/accepted/0006-registry-first-api.md), a stated
lifetime for the event trail, and lifecycle exits that are no longer
silent.

Its purpose was not written down at the time. This account is
reconstructed from its contents.

### Registry-first API + daemon/CLI split (ADR-0006 + ADR-0031)

[GitHub milestone](https://github.com/bricef/factor-q/milestone/4),
reached 2026-08-26.

One registry of operations, with the command line, the authenticated edge
and the dashboard derived from it, and the `fq` client split from the
`fqd` daemon
([ADR-0006](docs/adrs/accepted/0006-registry-first-api.md),
[ADR-0031](docs/adrs/accepted/0031-daemon-cli-split.md)). The date is when
its
[plan](docs/plans/closed/2026-07-20-registry-and-split-execution.md)
closed.

The plan's last phase, a registry for the store, was sequenced with
storage M5 and moved to the storage milestone with its prerequisite.

### Exactly-once trigger dispatch (ADR-0032)

[GitHub milestone](https://github.com/bricef/factor-q/milestone/2),
reached 2026-11-11.

A trigger starts its work once. The design is a durable claim on each
trigger delivery
([ADR-0032](docs/adrs/accepted/0032-exactly-once-trigger-dispatch.md),
accepted), and both July 2026 incidents, in which one trigger produced
several runs, replay green against a real broker. Its
[plan](docs/plans/closed/2026-07-18-exactly-once-trigger-dispatch.md)
closed the same day.

The design record lists the gaps it knowingly leaves. Those, and the other
work that followed the decision, are part of
[Phase 2](#production-readiness-phase-2-the-record-is-trustworthy).

## Retired

### Observability floor

[GitHub milestone](https://github.com/bricef/factor-q/milestone/7),
retired 2026-11-12, not reached.

It stood for structured logs, metrics and alerting. The
production-readiness review placed its one remaining issue, a metrics
endpoint with a consumer-lag alert, in
[Phase 3](#production-readiness-phase-3-bounded-growth-and-operable-state),
and the issue moved there. Stuck-invocation detection was delivered under
it. The wider floor is still unbuilt, as [STATUS.md](STATUS.md) says.

## Planned, with no milestone yet

A milestone is opened when the work is next, not before.

- **Production readiness, Phases 4 and 5.** Phase 4 is release
  engineering: pinned and attested builds, token expiry, and a tagged
  release installed from binaries on a host that never had the source.
  Phase 5 is thirty days on that release with no intervention caused by a
  runtime defect. Both are defined in the
  [review](docs/reviews/2026-09-03-production-readiness-review.md). How
  Phase 4 relates to `v0.0.1` is the open question recorded above.
- **The graph executor.** The
  [two-node vertical](docs/plans/active/2026-07-07-graph-executor-two-node-vertical.md)
  is the first multi-agent work
  ([ADR-0007](docs/adrs/accepted/0007-inter-agent-communication.md)) and
  the first step towards M2 in [VISION.md](VISION.md). It is held, with
  its exit conditions tracked on
  [#414](https://github.com/bricef/factor-q/issues/414).
- **Memory and Skills.** The services built on the storage milestone,
  from the
  [MCP, memory and skills plan](docs/plans/active/2026-04-11-phase-2-mcp-and-memory.md).

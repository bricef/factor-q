# Storage and Scaling

How factor-q stores its event history, how that history grows with
usage, and what backing stores and operational practices are
appropriate at different scales.

## Architecture: NATS holds the event bodies

Every meaningful action in factor-q becomes an event on the event
bus. NATS with JetStream is the system's event store: full payloads,
30-day window. A separate SQLite projection consumer materialises
events into queryable tables for CLI inspection (`fq events query`,
`fq costs`), and **the projection is not authoritative** — it can be
dropped and rebuilt from NATS by replaying the stream from
`deliver_all`.

That is a sizing statement, not a durability one, and the difference
matters. ADR-0011 made NATS the source of truth;
[ADR-0026](../../adrs/accepted/0026-event-log-system-of-record.md)
overturned exactly that half in 2026-07 — NATS is transport and a
replay window, not the log of record — and its replacement, a
CAS-backed archive service, has not been built. So there is no
system of record for the event trail today: past the 30-day window
the bodies are gone, and what survives is cost-bearing projection
rows and the archived per-invocation outcome. Size for the window;
do not size as though anything behind it is being kept.

This split matters for sizing because the two stores hold different
shapes of data:

- **NATS** holds **full event bodies** (system prompts, message
  history, tool outputs). Every event is persisted at full fidelity.
- **SQLite** holds **metadata and denormalised columns** (agent id,
  invocation id, event type, timestamps, cost/token counts). The
  full payload lives in NATS; SQLite joins back via `event_id` when
  needed.

SQLite rows are therefore stable in size regardless of tool output
length or conversation history depth. NATS storage grows
proportionally with real event size.

## Per-event size breakdown

All events share a ~200-byte envelope. The per-payload size depends
on the event type:

| Event            | Typical size      | Size driver                                                         |
|------------------|-------------------|---------------------------------------------------------------------|
| `triggered`      | 1–10 KB           | System prompt in the `ConfigSnapshot`                               |
| `llm.request`    | 2 KB → 100+ KB    | Full message history, grows quadratically with tool-loop depth      |
| `llm.response`   | 500 B – 2 KB      | Content + tool calls + usage                                        |
| `tool.call`      | 300 B – 1 KB      | Parameters                                                          |
| `tool.result`    | 500 B – MBs       | Output (file contents, shell output) is unbounded                   |
| `completed`      | ~300 B            | Totals                                                              |
| `failed`         | ~500 B            | Error kind + message                                                |

The two fat drivers are `llm.request` (full history repeated each
turn) and `tool.result` (unbounded output).

**There is no `cost` event.** Cost rides `envelope.cost` on the
`llm.response` (and on `llm.failure` where usage was recoverable), so
it costs a few dozen bytes on an event that was being written anyway,
not a ~400-byte event of its own. Every count below is one event per
LLM call lighter than an older draft of this page assumed.

### Per-invocation examples

**Simple single-turn call:**

```text
  triggered            2 KB
  llm.request          3 KB    # system + user
  llm.response         1 KB    # cost rides its envelope
  completed            300 B
  total             ~ 6 KB
```

**Three-tool-call loop with 2 KB file reads:**

```text
  triggered              2 KB
  llm.request #1         2 KB    # system + user
  llm.request #2         5 KB    # + assistant + tool result
  llm.request #3         8 KB
  llm.request #4        11 KB
  other events          10 KB    # responses, tool calls/results, dispatches
  total              ~ 38 KB
```

**Ten-tool-call loop with 10 KB file reads:**

`llm.request` sizes grow 2 → 12 → 22 → ... → 102 KB across the ten
turns, totalling ~570 KB just for the LLM request events. Plus
~100 KB of `tool.result` events and ~10 KB of other events.

Total: **~680 KB per invocation.**

## Daily growth model

Taking ~30 KB/invocation as a moderate average:

| Invocations/day        | Per day | Per month | Per year |
|------------------------|--------:|----------:|---------:|
| 100 (personal use)     |    3 MB |     90 MB |     1 GB |
| 1,000 (small team)     |   30 MB |    900 MB |    11 GB |
| 10,000 (ops swarm)     |  300 MB |      9 GB |   110 GB |
| 100,000 (prod fleet)   |    3 GB |     90 GB |     1 TB |

For agents doing heavier tool loops (~300 KB/invocation average),
multiply everything by 10.

## SQLite projection sizing

Because the SQLite projection stores metadata only, row sizes are
stable at **~300–500 bytes each**, regardless of what the underlying
event carried.

| Invocations/day    | Events/day | SQLite growth | Per year  |
|--------------------|-----------:|--------------:|----------:|
| 100                |     ~1,000 |       ~400 KB |   ~150 MB |
| 1,000              |    ~10,000 |         ~4 MB |   ~1.5 GB |
| 10,000             |   ~100,000 |        ~40 MB |    ~15 GB |
| 100,000            | ~1,000,000 |       ~400 MB |   ~150 GB |

SQLite is genuinely happy into the hundreds of GB as long as queries
hit indexes. Growth at moderate use is comfortable for years without
intervention. The daemon applies `[state].retention_days` on its
hourly retention schedule (30 days by default), keeping growth
bounded and aligned with the default NATS window — with one
deliberate exemption: cost-bearing rows (`total_cost` set) are never
swept, because spend accounting is a primary platform concern and
all-time cost figures must survive retention. Growth of the exempt
set is one row per priced LLM call, typed columns only.

**That window is not projection-only.** The same setting and the same
tick also sweep `invocation_archive` in `control-plane.db` — the
per-invocation final phase, state blob and timestamps — keyed on
`archived_at`. That table is *not* rebuildable from anything, so the
one knob governs both a cache and a record, and turning it down
discards outcome history rather than just re-derivable rows.

Keep `retention_days` at or below the stream retention. A longer
window makes the projection the sole holder of older non-cost rows,
which replay cannot rebuild. Non-cost aggregates sourced from the
projection (event counts, failure tallies) cover at most this window.

### Schema

`projection.db` holds three tables. `events` is the one this page
sizes; the other two are small and mentioned because a backup or a
rebuild has to account for them.

```sql
CREATE TABLE events (
    event_id        TEXT PRIMARY KEY,          -- UUID v7, time-sortable
    seq             INTEGER,                   -- stream position, the universal cursor
    timestamp       TEXT NOT NULL,             -- RFC3339
    agent_id        TEXT NOT NULL,
    invocation_id   TEXT NOT NULL,
    event_type      TEXT NOT NULL,

    -- Denormalised columns for common filters; NULL when not applicable
    model              TEXT,
    input_tokens       INTEGER,
    output_tokens      INTEGER,
    cache_read_tokens  INTEGER,
    cache_write_tokens INTEGER,
    total_cost         REAL,                   -- NULL means "no known spend", and exempts the row from the sweep
    error_kind         TEXT,
    error_message      TEXT,
    duration_ms        INTEGER
);

CREATE INDEX idx_events_agent_time ON events(agent_id, timestamp);
CREATE INDEX idx_events_invocation ON events(invocation_id);
CREATE INDEX idx_events_type_time ON events(event_type, timestamp);
CREATE INDEX idx_events_time ON events(timestamp);
```

- **`invocation_summary`** — one operator-facing line per invocation
  (#216), last write wins. Derived: a reprojection replays the summary
  events and never re-calls the LLM.
- **`triggers`** — the permanent record of a published trigger. Exempt
  from the sweep *structurally* rather than by predicate: the sweep
  deletes from `events` only, so a trigger's record outlives the log
  it was noticed on without a second clause to keep in step.

There is no `cumulative_cost` and no `tool_name` column; running
totals are computed at query time, and a tool's name is read from the
payload in NATS rather than denormalised here.

## NATS backing store sizing

NATS holds the full event stream, so storage must accommodate the
retention window at the per-invocation rate above. At the default
30-day retention:

| Usage level            | ~30 KB/inv | ~300 KB/inv |
|------------------------|-----------:|------------:|
| 100 invocations/day    |      90 MB |      900 MB |
| 1,000 invocations/day  |     900 MB |        9 GB |
| 10,000 invocations/day |       9 GB |       90 GB |
| 100,000 invocations/day|      90 GB |      900 GB |

The fq-events stream is configured with S2 compression (see
[bus.rs](../../../services/fq-runtime/crates/fq-runtime/src/bus.rs) and
ADR-0011). Text-heavy event bodies typically compress 2–4x at
negligible CPU cost, so divide the raw numbers above by 2–3 in
practice.

## Backing store recommendations by scale

### Personal / single-tenant / phase 1 (up to ~1k inv/day)

- **Local NVMe SSD** on the host running NATS
- A **~50 GB** volume is plenty for 30-day retention even
  uncompressed
- Docker volume or bind mount into `/data/nats`
- Daily filesystem snapshots cover backup needs

### Small team / moderate use (1k–10k inv/day)

- **Dedicated SSD or cloud block storage** (AWS EBS gp3, GCP PD SSD,
  Hetzner Cloud Volumes)
- **100–200 GB** volume, grown as needed
- S2 compression turns ~100 GB of events into ~30 GB stored
- Block storage is easy to snapshot, move, and resize

### Large-scale / production (10k+ inv/day)

- **NVMe SSD or provisioned-IOPS block storage** (EBS io2,
  equivalents)
- **500 GB – 2 TB** depending on retention requirements
- Consider **clustered NATS** (3 nodes with JetStream replicas) for
  durability and high availability
- **Tiered retention**: a primary stream with 7-day `max_age` for
  hot access, and a mirror stream with longer retention periodically
  exported to object storage for audit/compliance

### What's a bad fit

- **Spinning rust / HDDs**: JetStream's write pattern is
  append-sequential so HDDs will technically work, but random reads
  during consumer catch-up get painful at scale.
- **S3 as primary storage**: latency is 10–100x higher than block
  storage. JetStream wasn't designed for it. S3 *is* a good fit for
  cold tiers (periodic export from a mirror stream).
- **NFS / networked filesystems**: JetStream uses `fsync` heavily
  for durability; NFS semantics around fsync are unreliable.

## Operational practices

### Backups

JetStream streams can be backed up two ways:

1. **Filesystem snapshots** of the `store_dir` (LVM, ZFS, EBS, etc.).
   Simple and fast if the backing store supports them.
2. **`nats stream backup`** — NATS's own stream-level export/import.
   Works independently of the filesystem.

**The runtime has three SQLite databases, and only one of them is the
projection.** A backup plan that covers the stream and `projection.db`
covers the least important two-thirds of the picture:

| File | Rebuildable? | What losing it costs |
|---|---|---|
| `projection.db` | **Yes**, by replay — except cost-bearing rows older than the stream window, which exist nowhere else | Query history, and any spend figure past 30 days |
| `control-plane.db` | **No** | Worker roster, schedules, pending waits, and `invocation_archive` — every finished invocation's outcome |
| `worker.db` | **No** | Every in-flight invocation: the dispatch WAL and the reducer state resume reads from |

So `control-plane.db` and `worker.db` are the ones a backup exists
for, and neither was mentioned here before. `projection.db` is worth
copying anyway — restoring it is cheaper than a full replay, and it
is the sole holder of swept-past cost rows — but it is the one file
of the three you could lose and rebuild.

`worker.db` is the awkward one to snapshot, because a consistent copy
of in-flight state is a moving target. Take it during a drained stop
(`fq down`), when every invocation has checkpointed at a step
boundary and nothing is mid-write.

### Monitoring

NATS exposes stream statistics via its HTTP monitoring endpoint
(port 8222 in the default config). `GET /jsz?streams=1` returns
current stream stats including message count, byte count, and
retention state. Watch for:

- **Byte count approaching `max_file_store`** → increase the cap or
  tighten retention
- **Consumer lag** growing → the SQLite projection consumer is
  falling behind
- **Message count without bounded growth** → retention policy not
  taking effect

### Retention

Default retention is 30 days (`max_age`) with S2 compression. To
adjust:

- **Shorter retention** — update `DEFAULT_MAX_AGE` in `bus.rs` or
  surface a config field. The stream setting applies on creation;
  existing streams need explicit update via the NATS API.
- **Longer retention** — same, plus bump `max_file_store` in
  `nats.conf` to match the expected size.
- **Cold tier** — set up a mirror stream with longer retention on a
  separate, cheaper backing store.

The SQLite pruning that was once deferred here has shipped: it is the
hourly retention sweep described above, and it prunes the archive as
well as the projection. What remains deferred from
[the phase 1 plan](../../plans/closed/2026-04-02-phase-1-foundation.md)'s
deferred-work section is the scheduled job that refreshes *external*
data, which is a different thing on the same scheduler.

### Rebuilding the projection

If SQLite is lost, corrupted, or intentionally dropped, factor-q
can replay the entire fq-events stream from NATS with a
`deliver_all` consumer and re-materialise the projection. This is a
first-class recovery path, not a fallback — we rely on it to let
schema changes in the projection roll forward safely.

## Migration path

The scaling recommendations above form a clean progression:

1. Start with a local docker volume on a developer laptop.
2. Move to dedicated block storage when the volume approaches ~20 GB
   or daily throughput becomes noticeable.
3. Cluster NATS when HA or durability matters more than cost.
4. Add a cold tier when audit retention exceeds the primary stream
   window.

Nothing about factor-q's architecture locks you into any particular
step on this ladder. Streams can be moved between hosts via the
`nats stream backup`/`restore` commands, and cluster migration is a
supported NATS operation.

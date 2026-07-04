# Operating fq-cas

Practical guide to running the `fq-cas` content-addressed store in production —
starting with **storage reclamation** (garbage collection), the one operation
that needs an operator's attention. Everyday use (`put` / `get` / `object …` /
`serve`) is covered by `fq-cas --help`; this manual covers the parts that keep a
long-lived store healthy.

## Orientation: where the data lives

Everything sits under one root directory (`--root`, env `FQ_CAS_ROOT`, default
`./.fq-cas`):

```text
<root>/blocks/<aa>/<hash>[.<gen>]   content-defined blocks, deduplicated
<root>/objects/<aa>/<cid>           JSON manifests (an object's ordered blocks)
<root>/index.db                     SQLite: names, versions, reference counts
```

Content is immutable and shared: identical blocks across objects are stored once.
The index tracks, transactionally, how many live names reference each object and
how many live objects reference each block — the counts that decide what is
garbage.

## Reclaiming storage: `fq-cas gc`

Deleting a name (`fq-cas object rm …`, or overwriting it) does **not** free disk
immediately — it only drops references. Reclaiming the now-unreferenced blocks
and manifests is the job of `fq-cas gc`:

```console
$ fq-cas gc
reclaimed objects     3
reclaimed blocks      41
orphan files reaped   0
refcounts reconciled  0
alarms                none — every invariant holds
```

One command runs the full **reachability audit**, which does four things:

| Line | What it reclaims |
|---|---|
| **reclaimed objects / blocks** | Manifests and blocks no live name references any more — the common case, after deletes and overwrites. |
| **orphan files reaped** | Block/manifest files left on disk by a crash mid-write (the file was fsync'd before its index row committed). |
| **refcounts reconciled** | Reference counts left inflated by a crash mid-write (a reservation that never completed), corrected back down so the storage can be freed. |
| **alarms** | Invariant violations the audit will **not** silently repair — see [Alarms](#alarms). |

`gc` is safe to run on a live store, at any time, as often as you like. It never
removes anything a live name still needs, and it never blocks writers.

### Machine-readable output

For monitoring or scripting, `--json` emits the same report as JSON:

```console
$ fq-cas gc --json
{
  "reclaimed_objects": 3,
  "reclaimed_blocks": 41,
  "orphan_blocks": 0,
  "orphan_objects": 0,
  "reconciled": 0,
  "alarms": []
}
```

### The grace period

Orphan-file reaping and refcount reconciliation are **crash recovery** — and a
file or reservation that looks orphaned might instead be an *in-flight write*
that simply hasn't committed its index row yet. To avoid reaping live work, `gc`
only touches files and reservations that have gone untouched for at least the
**grace period** (`--grace`, in seconds; default **900** = 15 minutes):

```console
$ fq-cas gc --grace 60      # 1-minute grace — more aggressive recovery
$ fq-cas gc --grace 3600    # 1-hour grace — extra caution near heavy writers
```

Reclaiming *properly* unreferenced objects and blocks (the common case) is not
grace-gated — that happens on every run regardless. The grace only bounds how
soon a *crash-orphaned* file or reservation is cleaned up. The default is safe
for any normal workload; lower it only if you need prompt recovery after a known
crash, and keep it comfortably above your longest expected write.

### Running against a store

`gc` operates on the **local** store and its index; it cannot run against a
remote `--server` (that endpoint exposes only the content-addressed layer, not
the name index). Point it at the root:

```console
$ fq-cas --root /var/lib/fq-cas gc
$ FQ_CAS_ROOT=/var/lib/fq-cas fq-cas gc --json
```

### When to run it

- **Periodically** — e.g. a nightly `fq-cas gc` cron entry. Reclamation cost
  scales with how much has been deleted since the last run.
- **After bulk deletes** — reclaim the space promptly rather than waiting.
- **After a crash or unclean shutdown** — one `gc` past the grace restores every
  invariant (bounded recovery); a second run will report all-zero once the store
  has converged.

## Alarms

The `alarms` line is the one that should page someone. It reports invariant
violations the protocol makes *impossible* under normal operation — above all
the **forbidden state**: a live name whose content is missing a block. `gc` will
**not** auto-repair these; it surfaces them and **exits non-zero**, so a cron job
or monitor notices:

```console
$ fq-cas gc; echo "exit: $?"
reclaimed objects     0
...
ALARM: 1 invariant violation(s) — this must never happen; investigate:
  LostLiveBlock { name: "research.papers.doc1", object: Cid(…) }
exit: 1
```

An alarm means something outside the protocol corrupted the store — disk
failure, an out-of-band file deletion, a bug. Treat it as data loss: stop,
investigate, and restore from backup rather than running more `gc`. It is a
report, not a routine repair.

## Safety guarantees

`gc` is built on a verified protocol (see the design docs below), so a few
properties hold by construction:

- **No lost live data.** It never removes a block or manifest a live name still
  needs — even when a writer is concurrently re-adding the same content.
- **Leak over lose.** If it is ever unsure, it *keeps* storage (to reclaim on a
  later pass) rather than risk removing something live. Transient over-retention
  is normal and self-heals.
- **Wait-free for writers.** Running `gc` never blocks or fails a concurrent
  `put`; a writer whose block `gc` is reclaiming simply re-materializes it.

## See also

- [`storage-garbage-collection.md`](../design/storage-garbage-collection.md) —
  the online-reclaim protocol and the reachability-audit backstop.
- [`storage-gc-verification.md`](../design/storage-gc-verification.md) — the
  invariants, the fault map, and the TLA⁺ / DST verification behind the above.

- [Access control](access-control.md) — the grants model, and revocation
  semantics / the token TTL (what takes effect when, and why revocation is
  immediate in-process).

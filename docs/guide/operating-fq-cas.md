# Operating fq-cas

Practical guide to running the `fq-cas` content-addressed store in production —
starting with **storage reclamation** (garbage collection), the one operation
that needs an operator's attention. Everyday use (`put` / `get` / `object …` /
`serve`) is covered by `fq-cas --help`; this manual covers the parts that keep a
long-lived store healthy.

## Orientation: where the data lives

Everything sits under one root directory (`--root`, env `FQ_CAS_ROOT`, default
`.fq-cas`, relative to the process's working directory):

```text
<root>/blocks/<aa>/<hash>[.<gen>]   content-defined blocks, deduplicated
<root>/objects/<aa>/<cid>           JSON manifests (an object's ordered blocks)
<root>/index.db                     SQLite: names, versions, reference counts
<root>/grants.db                    SQLite: the grant-event log + permission projection
```

Content is immutable and shared: identical blocks across objects are stored once.
The index tracks, transactionally, how many live names reference each object and
how many live objects reference each block — the counts that decide what is
garbage.

## A root that does not exist

Writing creates a store; reading does not. `object put` and `grant add`, the
two database commands that can be the first thing done to a new store, create
the root, parents included, as `put` does when it first writes. Every other
command that opens `index.db` or `grants.db` (`object get`, `object ls`,
`object resolve`, `object history`, `object rm`, `object bind`, `gc`,
`grant ls`, `grant check`, `grant rm` and `token mint`) refuses a root that is
not there. It prints one line naming the directory, as an absolute path, and
where the path came from, exits 1 and creates nothing:

```text
fq-cas: the store root /home/me/src/.fq-cas (the default, in the working directory) does not exist. Point --root or FQ_CAS_ROOT at an existing store, or start a new one with `fq-cas object put` or `fq-cas grant add`.
```

The path came from `--root`, from `FQ_CAS_ROOT`, or, as here, from the default.
The default is the case to look for: `.fq-cas` is taken relative to the working
directory, so a command run from the wrong directory looks for a store that is
not there. The refusal is deliberate. A missing root is almost always a path
that is wrong like this, and if these commands created it, `object ls` would
print nothing, `grant check` would answer `denied`, `gc` would report that every
invariant holds and `token mint` would mint a token with no grants, each exiting
as it would for a real, empty store.

The refusal exits 1, which is also how `grant check` reports a denial and how
`gc` reports an alarm, so a script that reads only the exit status cannot tell
them apart; the line on stderr does.

A root that exists but is not a directory, one with a file among its parents,
one that is a symbolic link to something that does not exist, and one that
`object put` or `grant add` cannot create, because its parent is not writable
for instance, are named in the same way, with the reason:

```text
fq-cas: the store root /srv/fq-cas (from --root) is not a directory
fq-cas: the store root /srv/fq-cas (from --root) is a symbolic link to /mnt/store, which does not exist
fq-cas: cannot create the store root /srv/fq-cas (from --root): Permission denied (os error 13)
```

A link whose target is missing is refused by `object put` and `grant add` too,
since neither can create a directory through it; create the target, or point
the root elsewhere. A link to a directory that exists is followed.

`object put` and `grant add` check what they can before they create anything:
that `object put`'s input path exists, and that `grant add`'s agent id, verbs
and scope are valid. A mistyped argument therefore prints its own error and
leaves no new directory behind. A failure after those checks leaves the new
root with an empty database in it: an `object put` whose input exists but
cannot be read (a directory, or a file without read permission) leaves the root
and an empty `index.db`, and the next command uses them as a new store. With
`-` for its input, `object put` checks nothing first: the root is created
before standard input is read, so a failed read leaves them too.

The content-addressed commands work as they always have: `put` creates the
directories it writes into, and `get`, `has`, `size` and `metrics` answer for a
missing root as for an empty store.

## Schema upgrades on open

`index.db` and `grants.db` each record a schema version. A command that opens
one — `object …` and `gc` open `index.db`; `grant …` and `token mint` open
`grants.db` — first brings it up to the version its binary was built for,
creating the file in the root if it is not there; the root itself is created
only as [above](#a-root-that-does-not-exist). For `grants.db` that includes
bringing the permission projection level with the grant log, if it has fallen
behind (a file from before the projection existed, or one whose projection was
wiped). This needs no operator involvement, and it is safe for several
processes to do it at once:

- **One at a time.** An upgrade holds the file's write lock from the moment it
  reads the version until it commits. A second process opening the same file — a
  `gc` cron entry that fires while an `object put` is starting, say, both of
  them opening `index.db` — waits for the first to finish, then finds the file
  current. The lock is SQLite's own, so this holds between processes, not only
  within one.
- **All or nothing.** The schema changes, the version that records them and the
  projection catch-up commit together. An upgrade stopped part-way, by an error,
  a crash or a kill, leaves the file at its old version with nothing applied, and
  the next command runs it again from the start.

The wait is bounded at 30 seconds, and so is the wait for another process that
is creating the same file at the same moment. A command that cannot get the
lock in that time fails with `database is locked` and changes nothing, while
the process holding the lock carries on; running the command again once that
process has finished is enough. A lock that is never released means a process
stopped in the middle of a write.

### A store a newer `fq-cas` upgraded

An `fq-cas` will not open a database that a newer `fq-cas` has upgraded,
because it cannot know what the newer schema means, even to read it. Every
command that opens `index.db` or `grants.db` refuses such a file — the read-only
ones (`object get`, `object ls`, `object resolve`, `object history`,
`grant ls`, `grant check`, and `token mint`, which only reads `grants.db`) as
well as the ones that write — before it runs any upgrade or projection
catch-up. It prints one line naming the file and both versions and exits 1:

```text
fq-cas: incompatible schema: /srv/fq-cas/index.db is at version 7, this binary supports up to 6. The file was written by a newer fq-cas: run an fq-cas at least as new as that one against this store.
```

The file is left as the newer binary left it. The one step that comes before the
check is the switch into WAL mode that the command makes when it first connects,
and every `fq-cas` keeps both files in WAL mode already, so in practice it
changes nothing. The check is made when a command opens the file, so a command
already running when a newer `fq-cas` upgrades the file carries on with it; the
runtime's stores have the same limit.

The remedy is the one the message names: run an `fq-cas` at least as new as the
one that wrote the file — usually by finishing an upgrade that reached some of
the hosts or scripts using this store and not others. Do not delete the file to
get past the refusal:
`index.db` holds the store's names and reference counts and `grants.db` its
grant log, and neither is rebuilt from anything else. Rolling `fq-cas` back
across a schema change is not supported; it would mean restoring the store from
before that change as well.

`grant check` also exits 1 for a denial, so a script that reads only its exit
status takes a refusal for a denial; the line on stderr tells them apart.
Commands that open neither database — `put`, `get`, `has`, `size`, `metrics`,
`serve`, `key generate`, `token attenuate` and `token inspect` — are unaffected.

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

For monitoring or scripting, `--json` emits the same figures as JSON:

```console
$ fq-cas gc --json
{
  "reclaimed_objects": 3,
  "reclaimed_blocks": 41,
  "orphan_blocks": 0,
  "orphan_objects": 0,
  "orphan_temps": 0,
  "orphan_temp_bytes": 0,
  "reconciled": 0,
  "alarms": []
}
```

Two things to know before you parse it. The object is **hand-built** at the
CLI boundary rather than serialised from the report type, so treat these eight
keys as the contract and not the internal struct. And each entry in `alarms`
is a Rust **debug string**, not a nested object — a populated array looks
like:

```json
"alarms": ["LostLiveBlock { name: \"research.papers.doc1\", object: Cid(…) }"]
```

So a monitor should branch on `alarms` being non-empty (or on the exit code,
which is the same signal), and treat the strings as text for a human to read
rather than something to destructure.

### The grace period

Orphan-file reaping and refcount reconciliation are **crash recovery** — and a
file or reservation that looks orphaned might instead be an *in-flight write*
that simply hasn't committed its index row yet. To avoid reaping live work, `gc`
only touches files and reservations that have gone untouched for at least the
**grace period** (`--grace`, in seconds; default **900** = 15 minutes):

```console
fq-cas gc --grace 60      # 1-minute grace — more aggressive recovery
fq-cas gc --grace 3600    # 1-hour grace — extra caution near heavy writers
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
fq-cas --root /var/lib/fq-cas gc
FQ_CAS_ROOT=/var/lib/fq-cas fq-cas gc --json
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

That transcript is what a **terminal** shows, where both streams land
together. The counters go to stdout; the `ALARM:` block goes to **stderr**,
deliberately. A cron job that captures only stdout gets the counters, the
`alarms none` line silently absent, and no sign of the violation — so keep
stderr, or branch on the exit code, which carries the same signal on its own.

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

- [`storage-garbage-collection.md`](../design/committed/storage-garbage-collection.md) —
  the online-reclaim protocol and the reachability-audit backstop.
- [`storage-gc-verification.md`](../design/committed/storage-gc-verification.md) — the
  invariants, the fault map, and the TLA⁺ / DST verification behind the above.

- [Access control](access-control.md) — the grants model, and revocation
  semantics / the token TTL (what takes effect when, and why revocation is
  immediate in-process).

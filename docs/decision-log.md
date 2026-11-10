# Decision log

Decisions that are too small for an [ADR](adrs/README.md) but still need
to be findable later: accepting a dependency advisory, holding a version
back, or making an operational choice with a known cost.

- **Order:** newest first.
- **Contents:** each entry says what was decided, why it is acceptable,
  what it costs, and the condition that reopens it.
- **Changes:** an entry is never rewritten. When a decision changes or its
  condition is met, add a new entry that points back to it.
- **Bigger decisions:** anything hard to reverse or that sets a pattern is
  an ADR.
- **Risks:** a decision that leaves a security exposure open is also a row
  in the [risk register](../SECURITY.md#risk-register).

## 2026-11-10: Accept RUSTSEC-2026-0459 (SQLite in `libsqlite3-sys` 0.30)

- **Decision.** Accept the advisory until `sqlx` allows a fixed
  `libsqlite3-sys`.
  - `deny.toml` ignores it, with its reason. That one list is shared by
    `cargo deny` and `cargo audit` (see `just audit`).
- **The finding.**
  - CVE-2026-119375 (CVSS 4.0: 6.8, medium) affects SQLite before 3.54.
  - SQLite can return uninitialised page-cache memory when it opens a
    database beside a `-wal` file whose page size differs, so crafted
    files can make ordinary queries return stale bytes of process memory.
  - factor-q links `libsqlite3-sys` 0.30.1 through `sqlx-sqlite` 0.9.0,
    which bundles an affected SQLite.
- **Why it cannot be fixed yet.**
  - The fix is `libsqlite3-sys` 0.39 (SQLite 3.54).
  - `sqlx-sqlite` 0.9.0, the latest release on 2026-11-10, requires
    `libsqlite3-sys >=0.30.1, <0.38.0`.
- **Why it is acceptable.**
  - Exploiting it needs an attacker-written database and WAL pair that
    factor-q then opens.
  - factor-q has no user-generated or user-supplied database files. It
    opens only its own SQLite state files in its own state directory, and
    no code path restores, imports or `ATTACH`es a database from
    elsewhere.
  - A party able to write those files already controls the runtime's
    state, which is a larger exposure than this one.
- **Cost.** Until the upgrade, the bundled SQLite carries a known bug.
  Today it is reachable only by someone who can already write factor-q's
  state.
- **Revisit when** either:
  - a `sqlx` release allows `libsqlite3-sys` 0.39 or later
    ([#941](https://github.com/bricef/factor-q/issues/941)); or
  - factor-q starts opening database files it did not write itself.
- **Recorded in:**
  - [risk register](../SECURITY.md#risk-register) R8;
  - the `RUSTSEC-2026-0459` entry in `deny.toml`;
  - [#941](https://github.com/bricef/factor-q/issues/941).

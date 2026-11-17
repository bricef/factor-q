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

## 2026-11-17: Accept RUSTSEC-2026-0543 and RUSTSEC-2026-0542 (`chrono` and `humantime` are unmaintained)

- **Decision.** Accept both advisories until factor-q links neither crate.
  - `deny.toml` ignores them, each with its reason. That one list is shared
    by `cargo deny` and `cargo audit` (see `just audit`).
- **The finding.**
  - Both are informational advisories of the kind `unmaintained`. Neither
    describes a vulnerability. They reached the advisory database on
    2026-11-17 and turned the dependency audit red on `main` and on every
    open pull request that day.
  - RUSTSEC-2026-0543: `chrono` is unmaintained. Every version is
    affected and there is no patched release. The advisory recommends
    `jiff` or `time`.
  - RUSTSEC-2026-0542: `humantime` is unmaintained "because chrono is now
    unmaintained and humantime wraps chrono". Every version is affected.
- **Why it cannot be fixed by an upgrade.**
  - 0.4.45 and 2.4.0, the versions factor-q links, are the latest of each.
  - `chrono` is a direct dependency of five crates (`fq-ops`,
    `fq-runtime`, `fq-daemon`, `fq-cli`, `fq-dashboard`), used in 62
    source files. Every timestamp in the runtime is a
    `chrono::DateTime<Utc>`, the event envelope's included, and event
    timestamps go on the wire as the RFC 3339 text chrono's `serde`
    support writes, which the golden corpus pins. Replacing it is a
    migration that has to keep that text byte for byte.
  - `humantime` is not used by factor-q. It is reached only through
    `tarpc` 0.38.0, the latest release, which calls
    `humantime::format_rfc3339` in two tracing spans to print an RPC's
    deadline and has no feature that turns the dependency off.
- **Why it is acceptable.**
  - No vulnerability is known in either crate. What the advisories change
    is what happens if one is found: there would be no upstream release
    to move to.
  - What factor-q asks of `chrono` is narrow: reading the clock,
    converting epoch milliseconds, duration arithmetic, and writing and
    parsing RFC 3339 timestamps. The text it parses comes from three
    places: its own events, an operator's `--since` argument, and a model
    provider's `Retry-After` HTTP date (`llm/genai/errors.rs`).
  - `humantime` only formats a deadline for a log field; nothing factor-q
    receives is parsed by it.
- **Cost.**
  - A vulnerability found in either crate from now on has no upstream
    fix. factor-q would have to migrate under time pressure, or carry a
    patched fork.
  - Two more entries in a list that is meant to stay short, one of them
    for a crate at the centre of the runtime.
- **Revisit when** any of:
  - an advisory of any other kind (a vulnerability, unsoundness) is
    published against `chrono` or `humantime`: this acceptance does not
    cover it;
  - the migration off `chrono` lands
    ([#1050](https://github.com/bricef/factor-q/issues/1050));
  - a `tarpc` release drops `humantime`, or factor-q leaves `tarpc`
    ([#1051](https://github.com/bricef/factor-q/issues/1051));
  - either advisory is withdrawn because the crate has maintainers again.
- **Recorded in:**
  - [risk register](../SECURITY.md#risk-register) R9;
  - the `RUSTSEC-2026-0543` and `RUSTSEC-2026-0542` entries in
    `deny.toml`;
  - [#1050](https://github.com/bricef/factor-q/issues/1050) and
    [#1051](https://github.com/bricef/factor-q/issues/1051).

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

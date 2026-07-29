# ADR-0034: Relicensing to Apache-2.0 — factor-q becomes open source

## Status

Accepted (2026-07-29). **Supersedes the licensing decision in
[ADR-0022](0022-binary-distribution-and-licensing.md) §7 and all of
[ADR-0033](0033-bsl-reaffirmation.md).** BSL 1.1 is replaced by the
Apache License 2.0, effective immediately. ADR-0022's binary
distribution, release-matrix, and release-pipeline decisions (§§1–6) are
unaffected and remain in force.

## Context

[ADR-0022](0022-binary-distribution-and-licensing.md) §7 chose BSL 1.1 on
2026-06-27: personal non-commercial use free, organizational or
commercial use gated behind a licence from `licensing@factorq.top`, each
release converting to Apache-2.0 four years after publication.

The [2026-07-25 cleanroom
review](../../reviews/2026-07-25-factor-q-cleanroom-review.md) §1.4
challenged that choice: *"BSL is buying protection you don't need at a
price you can't afford."* The licence protects commercial revenue that
does not exist, from competitors who do not exist, in a project with no
tagged release — while deterring the contributors and early adopters an
unfunded solo project most needs. The reviewer's recommendation was
concrete: **name the specific competitive scenario BSL prevents.** If it
can be named, keep BSL. If the honest answer is "someone might one day
fork it and sell hosting," then Apache-2.0 is a cheap option to hold
until the project is successful enough to be worth forking.

[ADR-0033](0033-bsl-reaffirmation.md) held BSL rather than resolve that,
and was explicit about what it was doing: *"That scenario has not been
written down, and this ADR does not invent one. The critique is not
rebutted on its merits."* It deferred to a list of revisit triggers —
the first tagged release, the first inbound licensing enquiry, a recorded
instance of someone declining to engage on licensing grounds, a decision
to pursue the services wedge, or evidence of a fork-and-host competitor.

**No trigger has fired.** There is still no tagged `vX.Y.Z` release (only
the rolling `main-latest` pre-release), no inbound licensing enquiry, and
no observed competitor. This ADR is a maintainer decision taken *ahead*
of the triggers, on the grounds that the triggers were the wrong
instrument: every one of them fires *after* the adoption cost has already
been paid. A licence that deters an early adopter produces no evidence of
having done so — the non-adopter simply never appears. Waiting for
evidence that the gate is costing something is waiting for a signal the
gate itself suppresses.

The named scenario was never written down because it does not exist
concretely. That is the answer to the reviewer's question, arrived at by
a year of not being able to answer it.

## Decision

**factor-q is licensed under the Apache License 2.0** (SPDX:
`Apache-2.0`). factor-q is now open source in the OSI sense.

- The `LICENSE` file is the verbatim Apache-2.0 text from
  `apache.org/licenses/LICENSE-2.0.txt`, with the appendix copyright
  filled in as `Copyright 2026 Brice Fernandes`.
- `[workspace.package] license` is `"Apache-2.0"`; every member crate
  inherits it via `license.workspace = true`.
- **The commercial gate is removed entirely.** The "personal use free,
  organizations pay" carve-out, the four-year Change Date, and the
  `licensing@factorq.top` contact are all gone from `README.md` and the
  licence itself. The review noted the `licensing@` address independently
  raised diligence questions for organizations evaluating the project;
  with the gate gone, the signal goes with it.

### Why Apache-2.0

- **It is the destination already promised.** BSL named Apache-2.0 as its
  Change License. Every release was going to land here within four years.
  This is the same endpoint reached sooner, not a change of direction —
  which makes it the choice least likely to surprise anyone who evaluated
  factor-q under the old terms.
- **Express patent grant** (§3). For an agent runtime that schedules and
  executes third-party work, the patent peace Apache-2.0 provides is
  worth more than MIT's brevity.
- **Inbound=outbound by default** (§5): contributions are Apache-2.0
  unless stated otherwise, which removes the CLA prerequisite ADR-0022
  imposed.
- **The reviewer named it.** Apache-2.0 was the specific alternative
  recommended, and adopting the recommendation as given keeps the record
  legible.

### Alternatives reconsidered

- **AGPL-3.0** — rejected. ADR-0022 rejected it for failing to deliver a
  hard commercial gate; that goal is now abandoned, so AGPL would have to
  earn its place on fork-and-host deterrence alone. It does deter that,
  but it costs adoption at organizations with blanket AGPL bans — which
  is precisely the cost this ADR exists to shed. Trading a licence that
  deters adopters for a different licence that deters adopters is not
  progress.
- **MIT** — rejected. No express patent grant, and it would break the
  Apache-2.0 conversion promise BSL already made to anyone who evaluated
  the project under those terms.
- **Apache-2.0 OR MIT** (the Rust ecosystem convention) — rejected as
  ceremony without benefit here. The dual form exists largely for
  crates.io library compatibility; factor-q is a runtime and a binary,
  not a widely-depended-on library.

## Consequences

- **This is a one-way door.** Apache-2.0 grants are irrevocable for the
  versions published under them. Any future commercial gating could only
  apply to *later* versions, and would require a CLA from every external
  contributor accepted in the interim. Reversing this decision costs
  strictly more than making it did.
- **No CLA is required**, and ADR-0022's "CLA/DCO required before
  accepting external contributions" consequence is discharged. Apache-2.0
  §5 makes inbound contributions Apache-2.0 by default. A DCO may still
  be adopted later for provenance hygiene, but it is no longer a
  prerequisite to the first outside PR.
- **The relicense is clean.** Every human commit in the repository is the
  maintainer's; all other commit authors are CI bots or factor-q's own
  agents acting on the maintainer's behalf. As sole copyright holder,
  no third-party consent was needed.
- **No BSL-licensed release is stranded.** No `vX.Y.Z` release ever
  shipped under BSL, so no user holds a version under the old terms whose
  rights need reconciling. Anyone who obtained the source under BSL
  retains those BSL rights for that copy; those terms were strictly
  narrower, so nobody loses anything.
- **The four-year Change Date clock is moot** and stops being tracked.
- **factor-q appears as open source** on GitHub's licence detection,
  OSI listings, and dependency scanners — removing the diligence friction
  the review identified.
- **Dependency compatibility is unchanged.** factor-q's dependencies are
  permissive (MIT/Apache-2.0), all of which are compatible with
  Apache-2.0 distribution.
- **Legal review is still outstanding but its scope shrinks sharply.**
  ADR-0022 flagged that the BSL Additional Use Grant — bespoke wording —
  needed review before the first tagged release. That bespoke text is
  gone; what remains is unmodified, widely-litigated stock Apache-2.0.
- **No commercial protection remains in the licence.** If factor-q is
  later commercialized, the moat has to be the hosted service, the
  operational knowledge, or the trademark — not the source terms. The
  cleanroom review's observation that the near-term wedge is a *services*
  business, which source licences do not protect anyway, is the reason
  this is an acceptable trade rather than a concession.

## References

- [ADR-0022](0022-binary-distribution-and-licensing.md) §7 — the original
  BSL 1.1 decision, superseded here. §§1–6 (distribution, release
  pipeline) stand.
- [ADR-0033](0033-bsl-reaffirmation.md) — the reaffirmation and its
  revisit triggers, superseded here in full.
- [2026-07-25 cleanroom
  review](../../reviews/2026-07-25-factor-q-cleanroom-review.md) §1.4 —
  the critique this ADR finally acts on.
- Issue #398 — the review's master tracking issue.
- `/LICENSE`, `/Cargo.toml`, `/README.md` — the artifacts changed.

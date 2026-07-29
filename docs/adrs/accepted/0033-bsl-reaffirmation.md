# ADR-0033: Reaffirming BSL 1.1 after the 2026-07-25 licensing critique

## Status

Accepted (2026-07-27). **Superseded in full by
[ADR-0034](0034-apache-2-relicense.md) (2026-07-29):** factor-q
relicensed to Apache-2.0 two days later, ahead of every revisit trigger
listed below. The critique recorded here was upheld, not rebutted; the
decision to hold BSL pending a tagged release was reversed on the grounds
that the triggers all fire after the adoption cost has been paid.

Reaffirms the licensing decision in
[ADR-0022](0022-binary-distribution-and-licensing.md) §7; does not alter
that ADR's distribution or release-pipeline decisions.

## Context

[ADR-0022](0022-binary-distribution-and-licensing.md) §7 chose **BSL 1.1**
on 2026-06-27: personal non-commercial use free, organizational or
commercial use requires a licence via `licensing@factorq.top`, each
release converting to Apache-2.0 four years after publication. AGPL was
rejected there because it is OSI open source and *permits* organizational
use under copyleft, so it does not deliver the intended hard gate;
PolyForm Noncommercial was the other finalist, and BSL won on
battle-tested adoption and its delayed-open-source clause.

The [2026-07-25 cleanroom
review](../../reviews/2026-07-25-factor-q-cleanroom-review.md) challenged
that choice as finding 1.4 (master tracking issue #398). This ADR records
the challenge and the response, so the question is settled by reference
rather than re-litigated from scratch at each review.

### The critique

Recorded as made, not softened. Its headline: *"BSL is buying protection
you don't need at a price you can't afford."* Its substance:

- **The trade as it stands.** The licence protects commercial revenue
  that does not exist, from competitors who do not exist, in a project
  with no tagged release — by deterring precisely the contributors and
  early adopters an unfunded solo project most needs.
- **The `licensing@` signal.** A `licensing@` address on a custom domain
  implies a commercial entity behind the project, and raises diligence
  questions for any organisation that might otherwise trial it.
- **Not a claim that BSL is wrong.** The reviewer was explicit: *"the
  Sentry/HashiCorp/MariaDB reasoning is real and the four-year conversion
  is the honest version of it."*
- **The sharper point.** The cost is being paid *now*, while the benefit
  is contingent on a future that a licence choice does not by itself
  create.
- **The wedge is a services business.** The Working Backwards exercise
  identified the *workflow optimisation service* as the strongest
  near-term wedge — and services businesses are not protected by source
  licences.

**The reviewer's recommendation.** Write down the specific competitive
scenario BSL prevents. If it can be named concretely, keep BSL. If the
honest answer is "someone might one day fork it and sell hosting," note
that this requires the project to first be successful enough to be worth
forking, and that Apache-2.0 until that point is a cheap option to hold.

## Decision

**BSL 1.1 stands.** [ADR-0022](0022-binary-distribution-and-licensing.md)
§7 is unamended: the licence, the personal-use carve-out, the
`licensing@factorq.top` contact, and the four-year Apache-2.0 conversion
all remain as decided. The current licensing posture is an **accepted
state** for a pre-release project, and the costs the critique identifies
are carried knowingly.

### The residual is not closed

The reviewer asked for a concretely named competitive scenario. **That
scenario has not been written down**, and this ADR does not invent one.
The critique is not rebutted on its merits.

What is decided is narrower: to **hold the current state** rather than
spend effort revisiting a licence choice before the project has a tagged
release. The unnamed scenario is a **known gap, recorded here rather than
resolved**.

### Revisit triggers (proposed, pending maintainer confirmation)

The maintainer has not yet ratified this list. Its purpose is that the
licence gets revisited on *evidence* rather than on review cadence:

- **The first tagged release** — the point at which the licence begins to
  have real effect on adoption.
- **The first inbound commercial or licensing enquiry** to
  `licensing@factorq.top`.
- **A recorded instance of an external contributor or organisation
  declining to engage on licensing grounds** — an actual instance, not
  speculation.
- **A decision to pursue the workflow-optimisation-service wedge
  commercially.**
- **Evidence of an actual fork-and-host competitor.**

## Consequences

- **Future reviews should cite this ADR rather than re-open finding
  1.4.** The critique is on the record with its response; re-deriving it
  costs review attention that the open findings need more.
- **The identified costs are knowingly carried** — contributor and
  early-adopter deterrence, and the diligence friction the `licensing@`
  signal creates for organisations evaluating the project.
- **The named-scenario gap remains open** until one of the revisit
  triggers fires. Nothing here converts "we chose not to look again yet"
  into "we answered the question".

## References

- [ADR-0022](0022-binary-distribution-and-licensing.md) — binary
  distribution, release pipeline, and the BSL 1.1 licensing decision
  (§7) that this ADR reaffirms.
- [2026-07-25 cleanroom
  review](../../reviews/2026-07-25-factor-q-cleanroom-review.md) §1.4 —
  the critique recorded above.
- Issue #398 — the review's master tracking issue.

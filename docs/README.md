# factor-q documentation

This folder holds the project's design records, decision history, plans,
and how-to guides. Each subfolder has a distinct job; the table below is
the fastest way to find what you need, and each entry links to the
subfolder's own README where one exists.

Start with the project root documents for the big picture:
[VISION.md](../VISION.md) (*what* factor-q is and *why*),
[ARCHITECTURE.md](../ARCHITECTURE.md) (*what* subsystems compose it and
*how* they fit), and [CONTRIBUTING.md](../CONTRIBUTING.md) (how work is
done here). The design principles in
[`design/committed/design-principles.md`](design/committed/design-principles.md)
are the *how we decide* layer that sits underneath everything else — read
them before proposing a design change.

## Where things live

| Folder | What's in it | When to use it |
|---|---|---|
| [`guide/`](guide/) | How-to guides describing the system **as it works today** — agent definitions, MCP, the reducer harness, access control, storage backends, operating the CAS. | You want to *use* or *operate* factor-q and need the current, live reference. Guides track present state (unlike ADRs, which are point-in-time). |
| [`adrs/`](adrs/) | Architecture Decision Records: significant, hard-to-reverse decisions and their reasoning. Split into `accepted/` and `draft/`, named `NNNN-slug.md`. | You want to know *what was decided and why*. Each ADR is a snapshot of a decision at a point in time; later ADRs supersede earlier ones. See the [ADR index](adrs/README.md). |
| [`design/`](design/) | Design documents, split into `committed/` (the system as built, or a decision in force) and `aspirational/` (design-ahead explorations and proposals not yet scheduled). Dated `*-assessment.md` files here are point-in-time snapshots. | You want the reasoning behind a subsystem's shape, or you're exploring a design that isn't committed yet. See the [design README](design/README.md) for how docs move between `committed/` and `aspirational/`. |
| [`plans/`](plans/) | Implementation plans, split into `active/` (in-flight work) and `closed/` (completed or abandoned), named `YYYY-MM-DD-slug.md`. [`backlog.md`](plans/backlog.md) collects deferred work not yet committed to a phase. | You want to know what's being built now, what shipped, or what's parked for later. |

## How the folders relate

- A **plan** in `plans/active/` describes work in progress; when it lands
  or is dropped it moves to `plans/closed/`.
- A decision worth recording graduates into an **ADR** in `adrs/accepted/`.
- The **design** docs explain the *why* behind subsystems; an aspirational
  design that is adopted becomes an ADR and its doc moves to
  `design/committed/`.
- Once something is built, a **guide** in `guide/` documents how to use it
  and stays current as the system evolves — so a guide, not an ADR, is the
  place to look for present-day behaviour.

Documentation is expected to be as legible to an LLM reader as to a human
one (see design principle 1): prefer complete examples and structured
formats over long prose, and keep folder purposes crisp so the tree stays
easy to orient on.

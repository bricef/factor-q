# Implementation plans

Plans describe **work**, not behaviour: what is being built, in what
order, and how we'll know it's done. Each is named `YYYY-MM-DD-slug.md`
(dated when the plan was opened) and lives in one of two folders:

- **[`active/`](active/)** — work in flight. If code is changing on a
  branch right now, the plan explaining it should be here.
- **[`closed/`](closed/)** — completed or abandoned. A plan moves here,
  in the same change that ends the work, and becomes historical record:
  closed plans preserve the sequencing, verification results, and
  mid-flight decisions of the work as it actually happened, and are not
  updated afterwards.

Consult `active/` to see what is being built now (and why recent commits
look the way they do); consult `closed/` to see how a shipped subsystem's
implementation actually unfolded — often the best record of the traps and
detours that the final code no longer shows. Deferred work that isn't yet
committed to a phase is tracked as
[GitHub issues](https://github.com/bricef/factor-q/issues), not as plans.

A plan is not a design doc: the reasoning behind a subsystem's shape
belongs in [`../design/`](../design/) and [`../adrs/`](../adrs/); a plan
sequences the execution and records what happened along the way.

# Guides

How-to guides describing the system **as it works today**. A guide is a
living reference: it tracks the present behaviour of shipped code and is
updated as the system evolves. This is the key contrast with the rest of
`docs/` — ADRs and reviews are point-in-time records that are superseded
rather than rewritten, and plans describe work rather than behaviour — so
for *current* behaviour, a guide is the place to look.

Consult a guide when you want to **use or operate** factor-q: authoring
agent definitions and skills, connecting MCP servers, understanding the
reducer harness's execution model, configuring access control, running
the daemon, or implementing and operating storage. The *why* behind a
subsystem's shape is deliberately not here — that lives in
[`../design/`](../design/) and [`../adrs/`](../adrs/).

Guides carry the same trust contract as committed design docs: if a
guide contradicts the code, one of them is wrong — fix whichever it is.
When a feature ships, updating (or writing) its guide is part of landing
the work.

#!/usr/bin/env python3
"""Verdict for a live-matrix run: did every arm complete, and did every
reasoning part a model produced reach the next request unchanged?

Reads the run's `events.ndjson`. For each declared arm, its invocation must
have ended in `completed` with no `llm.failure`, and every assistant turn
that was followed by another request must have been replayed in that
request verbatim — reasoning, text and tool calls, in the order they were
recorded (canonical-JSON equality, so a lost signature, a reordered part
or a re-encoded token all fail). Arms are declared as `ARM=reasoning` or
`ARM=none`: a reasoning arm must have carried at least one reasoning part
across a turn — a run where the model never reasoned proves nothing and
fails — and a `none` arm (the control) must have recorded no reasoning
part at all.

Readable text is reported, never asserted: whether a provider returns a
summary beside its continuity token is the provider's choice. The count
is there so a run where summaries vanish is visible in the log.

An arm is judged on its latest invocation; earlier ones (the harness re-triggers
an arm once when the provider was unavailable) are reported as superseded.

Exit 0 when every declared arm passes, 1 otherwise, 2 on a usage error.
"""

import argparse
import json
import sys
from collections import defaultdict


def load(path):
    with open(path, encoding="utf-8") as fh:
        return [json.loads(line) for line in fh if line.strip()]


def kind(ev):
    # `factor-q/llm_request@1` -> `llm_request`
    return ev["envelope"]["schema_id"].split("/", 1)[1].split("@", 1)[0]


def body(ev):
    return ev["payload"]["payload"]


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=False)


def reasoning_parts(parts):
    return [p for p in parts if p.get("kind") == "reasoning"]


def readable_chars(parts):
    return sum(len(p.get("content", {}).get("text") or "") for p in reasoning_parts(parts))


def judge(evs, expect):
    """One invocation's verdict: (passed, lines)."""
    lines = []
    terminal = [kind(ev) for ev in evs if kind(ev) in ("completed", "failed")]
    failures = [ev for ev in evs if kind(ev) == "llm_failure"]
    cost = sum(float((ev["envelope"].get("cost") or {}).get("total_cost", 0.0)) for ev in evs)
    ok = True

    if terminal != ["completed"]:
        ok = False
        lines.append(f"  terminal={terminal or 'none'} (wanted ['completed'])")
    for f in failures:
        ok = False
        b = body(f)
        lines.append(f"  llm.failure: {b.get('error_kind')} {str(b.get('error_message'))[:200]}")

    produced = carried = 0
    kinds = defaultdict(int)
    readable = 0
    turns = 0
    for idx, ev in enumerate(evs):
        if kind(ev) != "llm_response":
            continue
        turns += 1
        parts = body(ev).get("parts", [])
        for p in reasoning_parts(parts):
            kinds[p.get("content", {}).get("kind", "?")] += 1
        produced += len(reasoning_parts(parts))
        readable += readable_chars(parts)
        following = next((e for e in evs[idx + 1 :] if kind(e) == "llm_request"), None)
        if following is None:
            continue  # the final turn is never replayed; nothing to check
        want = canonical({"kind": "assistant", "parts": parts})
        replayed = any(canonical(m) == want for m in body(following).get("messages", []))
        if not replayed:
            ok = False
            lines.append(
                f"  turn {turns}: the assistant turn was not replayed verbatim in the next request "
                f"(parts={[p.get('kind') for p in parts]})"
            )
        else:
            carried += len(reasoning_parts(parts))

    if expect == "reasoning" and carried == 0:
        ok = False
        lines.append("  no reasoning part was carried across a turn — the run proves nothing")
    if expect == "none" and produced > 0:
        ok = False
        lines.append(f"  the control recorded {produced} reasoning part(s)")

    lines.insert(
        0,
        f"  turns={turns} reasoning produced={produced} carried={carried} "
        f"kinds={dict(kinds) or '{}'} readable_chars={readable} cost=${cost:.4f}",
    )
    return ok, lines


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("events", help="the run's events.ndjson")
    ap.add_argument(
        "--expect",
        action="append",
        default=[],
        metavar="ARM=reasoning|none",
        help="an arm to judge and what it must show; repeatable",
    )
    args = ap.parse_args()
    expects = {}
    for item in args.expect:
        arm, _, what = item.partition("=")
        if what not in ("reasoning", "none") or not arm:
            print(f"bad --expect {item!r}: want ARM=reasoning or ARM=none", file=sys.stderr)
            return 2
        expects[arm] = what
    if not expects:
        print("nothing to judge: pass at least one --expect", file=sys.stderr)
        return 2

    by_arm = defaultdict(lambda: defaultdict(list))
    for ev in load(args.events):
        env = ev["envelope"]
        by_arm[env["agent_id"]][env["invocation_id"]].append(ev)

    all_ok = True
    for arm, expect in expects.items():
        invocations = by_arm.get(arm, {})
        if not invocations:
            all_ok = False
            print(f"FAIL {arm} ({expect}): no invocation recorded")
            continue
        # An arm's verdict is its latest invocation. The harness re-triggers
        # an arm once when the provider was unavailable through the
        # runtime's retry budget; the earlier attempt is reported, not
        # judged — it said nothing about the round trip.
        ordered = sorted(invocations.items(), key=lambda item: item[1][0]["envelope"]["timestamp"])
        for inv, evs in ordered[:-1]:
            terminal = [kind(ev) for ev in evs if kind(ev) in ("completed", "failed")]
            failures = [str(body(ev).get("error_message"))[:120] for ev in evs if kind(ev) == "llm_failure"]
            print(f"note {arm}: earlier invocation {inv} superseded (terminal={terminal or 'none'}, failures={failures or 'none'})")
        inv, evs = ordered[-1]
        ok, lines = judge(evs, expect)
        all_ok &= ok
        print(f"{'PASS' if ok else 'FAIL'} {arm} ({expect}) invocation {inv}")
        print("\n".join(lines))
    return 0 if all_ok else 1


if __name__ == "__main__":
    sys.exit(main())

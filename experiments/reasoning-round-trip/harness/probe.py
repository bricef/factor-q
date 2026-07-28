#!/usr/bin/env python3
"""
Does omitting reasoning blocks on a tool-result turn silently disable thinking?

Issue: https://github.com/bricef/factor-q/issues/437

Anthropic's contract says thinking blocks are *required* to be passed back within a
tool-use turn, and that when conversation history is incompatible with thinking the API
"silently disables thinking for that request" rather than erroring. factor-q never
captures thinking blocks (genai drops the signature end to end), so every continuation
turn we send is in exactly that shape.

This probe measures the difference directly. Identical turn 1; two arms on the
continuation turn:

  echo  -- assistant turn echoed verbatim, thinking blocks intact   [correct protocol]
  strip -- assistant turn reduced to text + tool_use only           [factor-q today]

Reported per arm: HTTP status, count of thinking/redacted_thinking blocks in the
response, and usage.output_tokens_details.thinking_tokens.

Dependency-free by design (stdlib urllib only) so it runs anywhere the daemon runs.
The API key is never logged, echoed, or written to the results file.
"""

import argparse
import json
import os
import sys
import urllib.error
import urllib.request

API = "https://api.anthropic.com/v1/messages"
DEFAULT_MODELS = ["claude-fable-5", "claude-opus-5"]

# Two tools whose applicability is genuinely ambiguous, so choosing between them
# requires deliberation *before* the first call. This matters: the experiment is only
# valid if turn 1 emits a thinking block. If it doesn't, there is nothing to strip and
# both arms send byte-identical requests (see validity check in verdict()).
TOOLS = [
    {
        "name": "get_current_conditions",
        "description": (
            "Point-in-time instrument reading for a site. Returns the instantaneous "
            "value at the moment of the call. No smoothing, no history."
        ),
        "input_schema": {
            "type": "object",
            "properties": {"site": {"type": "string"}, "metric": {"type": "string"}},
            "required": ["site", "metric"],
        },
    },
    {
        "name": "get_windowed_aggregate",
        "description": (
            "Aggregated value for a site over a trailing window. Smooths transient "
            "spikes. Requires a window in hours; the caller must choose the window."
        ),
        "input_schema": {
            "type": "object",
            "properties": {
                "site": {"type": "string"},
                "metric": {"type": "string"},
                "window_hours": {"type": "integer"},
            },
            "required": ["site", "metric", "window_hours"],
        },
    },
]

# Under-specified on purpose: the model must reason about whether a transient spike or a
# smoothed trend is the right evidence for a *sustained* breach, and pick a window --
# that deliberation is what should land in turn 1's thinking block. The continuation
# then needs arithmetic against the result, so both turns have reasoning to do.
# NB: keep the scenario free of any safety surface. An earlier draft framed this as
# reactor coolant temperature and Claude Fable 5 refused it outright
# (stop_reason="refusal", empty content), which kills the run before it starts.
QUESTION = (
    "Our SLA says we must page the on-call engineer if the checkout service's p99 "
    "latency is *sustainably* above the 340ms budget -- a momentary spike doesn't "
    "count. Investigate and tell me whether to page, by how many milliseconds we're "
    "over, and what percentage over budget that represents."
)
TOOL_RESULT = "351.2ms"


def post(key, payload, timeout=180):
    req = urllib.request.Request(
        API,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": key,
            "anthropic-version": "2023-06-01",
            "content-type": "application/json",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, json.loads(r.read())
    except urllib.error.HTTPError as e:
        try:
            return e.code, json.loads(e.read() or b"{}")
        except json.JSONDecodeError:
            return e.code, {"error": "non-JSON error body"}
    except urllib.error.URLError as e:
        return 0, {"error": f"transport: {e.reason}"}


def summarise(body):
    content = body.get("content") or []
    usage = body.get("usage") or {}
    return {
        "thinking_blocks": sum(
            1 for b in content if b.get("type") in ("thinking", "redacted_thinking")
        ),
        "thinking_tokens": usage.get("output_tokens_details", {}).get("thinking_tokens"),
        "output_tokens": usage.get("output_tokens"),
        "input_tokens": usage.get("input_tokens"),
        "block_types": [b.get("type") for b in content],
        "stop_reason": body.get("stop_reason"),
    }


def run(key, model, max_tokens, effort=None):
    # Turn 1 is identical for both arms. With no `effort`, this is exactly what genai
    # emits for factor-q today (no agent sets effort, and genai's model-name suffix
    # inference finds nothing on these ids). Thinking is still active either way:
    # always-on for Fable 5, on-by-default for Opus 5. Passing --effort raises the
    # odds that turn 1 actually *emits* a thinking block, which the experiment needs.
    base = {"model": model, "max_tokens": max_tokens, "tools": TOOLS}
    if effort:
        base["output_config"] = {"effort": effort}
    status, t1 = post(key, {**base, "messages": [{"role": "user", "content": QUESTION}]})
    if status != 200:
        return {"model": model, "error": f"turn 1 HTTP {status}", "detail": t1}

    content = t1.get("content") or []
    if t1.get("stop_reason") == "refusal":
        return {
            "model": model,
            "error": "turn 1 refused by a safety classifier (stop_reason=refusal) -- "
                     "the scenario has a safety surface; pick a more benign one",
            "turn1": summarise(t1),
        }
    tool_uses = [b for b in content if b.get("type") == "tool_use"]
    if not tool_uses:
        return {
            "model": model,
            "error": "turn 1 produced no tool_use block",
            "turn1": summarise(t1),
        }

    # Models may issue several tool calls in parallel. Every tool_use needs a matching
    # tool_result in the immediately following message or the API rejects the turn --
    # returning only the first one 400s on both arms and looks like a protocol finding
    # when it is really a harness bug.
    tool_result_msg = {
        "role": "user",
        "content": [
            {"type": "tool_result", "tool_use_id": tu["id"], "content": TOOL_RESULT}
            for tu in tool_uses
        ],
    }

    arms = {
        "echo": content,
        "strip": [b for b in content if b.get("type") in ("text", "tool_use")],
    }

    result = {"model": model, "turn1": summarise(t1)}
    for arm, assistant_content in arms.items():
        status, body = post(
            key,
            {
                **base,
                "messages": [
                    {"role": "user", "content": QUESTION},
                    {"role": "assistant", "content": assistant_content},
                    tool_result_msg,
                ],
            },
        )
        result[arm] = {"http": status}
        result[arm].update(summarise(body) if status == 200 else {"detail": body})
    return result


def verdict(r):
    """Classify one model's result. Returns (label, explanation)."""
    if "error" in r:
        return "ERROR", r["error"]
    # VALIDITY GATE. If turn 1 emitted no thinking block there is nothing for the strip
    # arm to remove, so both arms send identical requests and any comparison is
    # meaningless. This is not a null result -- it is a failed experiment. Raise
    # --effort or make the turn-1 decision harder, then re-run.
    if r.get("turn1", {}).get("thinking_blocks", 0) == 0:
        return "INVALID", "turn 1 emitted no thinking block; arms are identical, nothing tested"
    echo, strip = r.get("echo", {}), r.get("strip", {})
    if strip.get("http") != 200:
        return "REJECTED", f"strip arm returned HTTP {strip.get('http')} (loud failure, not silent)"
    e_tok = echo.get("thinking_tokens") or 0
    s_tok = strip.get("thinking_tokens") or 0
    e_blk = echo.get("thinking_blocks", 0)
    s_blk = strip.get("thinking_blocks", 0)
    if e_tok > 0 and s_tok == 0 and s_blk == 0:
        return "SILENT DISABLE", f"echo thought ({e_tok} tok, {e_blk} blocks), strip did not"
    if e_tok > 0 and s_tok > 0:
        pct = round(100 * (e_tok - s_tok) / e_tok, 1)
        return "BOTH THOUGHT", f"echo {e_tok} tok vs strip {s_tok} tok ({pct:+}% delta)"
    if e_tok == 0 and s_tok == 0:
        return "INCONCLUSIVE", "neither arm spent thinking tokens; task may be too easy"
    return "UNEXPECTED", f"echo {e_tok} tok, strip {s_tok} tok"


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[1])
    ap.add_argument("--models", default=",".join(DEFAULT_MODELS),
                    help=f"comma-separated model ids (default: {','.join(DEFAULT_MODELS)})")
    ap.add_argument("--max-tokens", type=int, default=4000)
    ap.add_argument("--effort", default=None,
                    help="output_config.effort (low|medium|high|xhigh|max). Omit to "
                         "replicate factor-q exactly; raise it if turn 1 won't think.")
    ap.add_argument("--repeat", type=int, default=3,
                    help="runs per model; turn-1 thinking is non-deterministic (default: 3)")
    ap.add_argument("--out", help="write full JSON results here (never contains the key)")
    args = ap.parse_args()

    key = os.environ.get("ANTHROPIC_API_KEY")
    if not key:
        print("ANTHROPIC_API_KEY is not set. Use harness/run.sh, which sources it.",
              file=sys.stderr)
        return 2

    # Whether turn 1 emits a thinking block is non-deterministic, so a single run is
    # not evidence. Repeat and aggregate over the runs that came out valid.
    results = []
    for model in (m.strip() for m in args.models.split(",")):
        for i in range(args.repeat):
            r = run(key, model, args.max_tokens, args.effort)
            r["run"] = i + 1
            r["verdict"] = verdict(r)[0]
            results.append(r)
            print(f"  {model} run {i + 1}/{args.repeat}: {r['verdict']}", file=sys.stderr)

    if args.out:
        with open(args.out, "w") as f:
            json.dump(results, f, indent=2)

    w = "=" * 78
    print(f"\n{w}\n{'model':<18} {'valid':<7} {'echo tok':<12} {'strip tok':<12} {'silent-disable?'}\n{w}")
    for model in (m.strip() for m in args.models.split(",")):
        runs = [r for r in results if r["model"] == model]
        valid = [r for r in runs if r["verdict"] not in ("INVALID", "ERROR")]
        if not valid:
            reasons = {r["verdict"] for r in runs}
            print(f"{model:<18} {'0/' + str(len(runs)):<7} no valid runs ({', '.join(sorted(reasons))})")
            continue
        e = [r["echo"].get("thinking_tokens") or 0 for r in valid]
        s = [r["strip"].get("thinking_tokens") or 0 for r in valid]
        disabled = sum(1 for r in valid if (r["strip"].get("thinking_tokens") or 0) == 0
                       and r["strip"].get("thinking_blocks", 0) == 0)
        print(f"{model:<18} {str(len(valid)) + '/' + str(len(runs)):<7} "
              f"{'mean ' + str(round(sum(e) / len(e), 1)):<12} "
              f"{'mean ' + str(round(sum(s) / len(s), 1)):<12} "
              f"{disabled}/{len(valid)} runs")
    print(w)
    print("Silent disable would show as strip-arm runs with 0 thinking tokens AND 0")
    print("thinking blocks while the echo arm thinks. Any other pattern refutes it.")
    print(w)
    if args.out:
        print(f"full results: {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

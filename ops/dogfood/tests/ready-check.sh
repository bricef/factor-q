#!/usr/bin/env bash
# `bring_up`'s ready check reads the daemon's log and looks for the one
# line that means the runtime is serving — "Runtime ready. Press Ctrl-C
# to stop." (fq-daemon/src/hosted.rs). It must find that line whatever
# else the daemon has logged: a quiet start, and a busy one whose log is
# already megabytes by the time the line appears.
#
# The version this pins replaced (#753):
#
#     if printf '%s' "$fresh" | grep -q "Runtime ready"; then ...
#
# Under `set -o pipefail` — which deploy.sh sets — that is a trap.
# `grep -q` exits the instant it matches and closes the pipe; once
# "$fresh" outgrows the 64 KiB pipe buffer the `printf` cannot finish its
# write, dies of SIGPIPE, and pipefail reports the *pipeline* as 141. The
# `if` then takes the false branch on exactly the reads that contain the
# line. On 2026-09-14 that told the operator the daemon "did not log
# 'Runtime ready' within 1200s" about a daemon that had logged it after
# 15, and --auto rolled a good build back twice.
#
# Driven through `deploy.sh --wait-ready <container> <since> <tag>`, with
# a stub `docker` on PATH serving a fixture log — no compose stack, no
# daemon, seconds.
#
#   ops/dogfood/tests/ready-check.sh        (just ops-ci)
set -uo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
deploy="$here/../deploy.sh"
failed=0
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# A `docker` that answers `docker logs …` with the fixture and nothing
# else. First on PATH, so deploy.sh's own `docker logs` reaches it.
mkdir -p "$tmp/bin"
cat > "$tmp/bin/docker" <<'STUB'
#!/usr/bin/env bash
[ "${1:-}" = "logs" ] && exec cat "$FQ_FAKE_LOG"
exit 0
STUB
chmod +x "$tmp/bin/docker"

# The daemon's real startup preamble, copied from the lines fq-daemon
# prints on the way up (src/daemon.rs, src/hosted.rs) and from the
# registry's unenforced-`sandbox.network` warning (fq-agent/src/registry.rs),
# which the dogfood instance emits once per agent that declares hosts.
preamble() {
    cat <<'LOG'
factor-q runtime starting
  runtime id:       0b5f6b1e-2f5a-4c3d-9a77-1e0f3c2b8d41
  version:          0.1.0 (7d4433690051 x86_64-unknown-linux-gnu)
  NATS:             nats://nats:4222
  agent directory:  /var/lib/factor-q/agents
  cache directory:  /var/lib/factor-q/cache
  state directory:  /var/lib/factor-q/state
2026-09-14T13:28:44.118221Z  WARN fq_agent::registry: sandbox.network is declared but NOT enforced: this agent has ambient network access and can reach any host. The declaration is currently a no-op (#35); enforcement is tracked by #208 (proxy) and #209 (ADR-0010). agent="issue-fixer" path=/var/lib/factor-q/agents/issue-fixer.md
2026-09-14T13:28:44.118402Z  WARN fq_agent::registry: sandbox.network is declared but NOT enforced: this agent has ambient network access and can reach any host. The declaration is currently a no-op (#35); enforcement is tracked by #208 (proxy) and #209 (ADR-0010). agent="pr-reviewer" path=/var/lib/factor-q/agents/pr-reviewer.md
2026-09-14T13:28:44.118533Z  WARN fq_agent::registry: sandbox.network is declared but NOT enforced: this agent has ambient network access and can reach any host. The declaration is currently a no-op (#35); enforcement is tracked by #208 (proxy) and #209 (ADR-0010). agent="backlog-groomer" path=/var/lib/factor-q/agents/backlog-groomer.md
  agents loaded:    9 (errors: 0)
  edge:             0.0.0.0:9470
  worker db:        /var/lib/factor-q/state/worker.db
  control-plane db: /var/lib/factor-q/state/control_plane.db
  projection db:    /var/lib/factor-q/cache/projection.db
  control plane:    v4
  worker schema:    v2
  worker:           0b5f6b1e-2f5a-4c3d-9a77-1e0f3c2b8d41 (host: fq-dogfood)
  summariser:       anthropic/claude-haiku-4.5
  pricing entries:  742 (cache: /var/lib/factor-q/cache/pricing.json)
  MCP tools:        11
LOG
}
ready_lines() {
    printf '\n'
    printf 'Runtime ready. Press Ctrl-C to stop.\n'
    printf '  - projection consumer is materialising events into SQLite\n'
    printf '  - trigger dispatcher is listening on fq.trigger.*\n'
    printf '  - edge is listening on 0.0.0.0:9470\n'
}
# What a live instance logs in the seconds after the edge opens: the
# dispatcher picking up the trigger backlog, a consumer materialising.
# 64 KiB is the pipe buffer — the threshold the old check went blind at —
# so the fixture clears it several times over.
chatter() {  # $1 = how many lines
    local i
    for ((i = 1; i <= $1; i++)); do
        printf '2026-09-14T13:29:%02d.%06dZ  INFO fq_runtime::trigger::dispatcher: dispatching trigger seq=%d subject=fq.trigger.github.issue agent=issue-fixer invocation=8f%04x-4c3d-9a77-1e0f3c2b8d41\n' \
            $((i % 60)) $((i * 37 % 1000000)) "$i" "$i"
    done
}

run() {  # run <fixture file> <ready_wait> → sets rc and elapsed
    local t0 t1
    t0="$(date +%s)"
    out="$(FQ_FAKE_LOG="$1" READY_WAIT="$2" PATH="$tmp/bin:$PATH" \
        bash "$deploy" --wait-ready fq-dogfood-fqd-1 2026-09-14T13:28:42Z 7d4433690051 2>&1)"
    rc=$?
    t1="$(date +%s)"
    elapsed=$((t1 - t0))
}
check() {  # check <name> <want rc> <want max seconds> <must contain, or "">
    if [ "$rc" != "$2" ]; then
        printf '  FAIL %s: rc=%s, want %s (in %ss)\n      %s\n' "$1" "$rc" "$2" "$elapsed" "$out"; failed=1; return
    fi
    if [ "$elapsed" -gt "$3" ]; then
        printf '  FAIL %s: took %ss, want at most %ss — the line was there and was not seen\n' "$1" "$elapsed" "$3"; failed=1; return
    fi
    if [ -n "$4" ] && ! printf '%s' "$out" | grep -qF "$4"; then
        printf '  FAIL %s: output does not mention %q\n      %s\n' "$1" "$4" "$out"; failed=1; return
    fi
    printf '  ok   %s\n' "$1"
}

# 1. A quiet start: the line is found. (The old check passed this one —
#    which is why it survived review and every deploy until the instance
#    got busy.)
{ preamble; ready_lines; } > "$tmp/quiet.log"
run "$tmp/quiet.log" 5
check "a quiet start is ready at once" 0 2 ""

# 2. The regression. Same daemon, same line, a log that has crossed the
#    pipe buffer by the time the check reads it. This is what happened on
#    2026-09-14; on the old code it waits out READY_WAIT and returns 1.
{ preamble; ready_lines; chatter 1200; } > "$tmp/busy.log"
[ "$(wc -c < "$tmp/busy.log")" -gt 65536 ] || { echo "  FAIL fixture is not over the 64 KiB pipe buffer" >&2; failed=1; }
run "$tmp/busy.log" 5
check "a busy daemon's Runtime ready is still seen" 0 2 ""

# 3. A daemon that never comes up still fails, at READY_WAIT, saying so.
{ preamble; chatter 1200; } > "$tmp/wedged.log"
run "$tmp/wedged.log" 3
check "a wedged daemon still fails at READY_WAIT" 1 8 "did not log 'Runtime ready' within 3s"

# 4. The failure patterns are read out of a big log too — the same
#    pipeline trap, pointing the other way: a panic early in a long log
#    was missed, and the deploy waited out READY_WAIT instead of stopping.
{ preamble; printf "thread 'main' panicked at crates/fq-daemon/src/daemon.rs:155:\n"; chatter 1200; } > "$tmp/panic.log"
run "$tmp/panic.log" 5
check "a panic in a big log stops the wait" 1 2 "the daemon failed to start on 7d4433690051"

# 5. A failed wait says what the daemon was saying. Unattended, the
#    rollback replaces the container before anyone reads the
#    notification, so "docker compose logs fqd" points at evidence that
#    no longer exists; the deploy log has to carry it.
contains() {  # contains <name> <needle>
    if printf '%s' "$out" | grep -qF -- "$2"; then printf '  ok   %s\n' "$1"
    else printf '  FAIL %s: output does not contain %q\n      %s\n' "$1" "$2" "$out"; failed=1; fi
}
run "$tmp/wedged.log" 3
contains "a timed-out wait quotes the daemon log"    "--- last 30 lines of the daemon log ---"
contains "  … the lines themselves, indented"        "    | 2026-09-14T13:29:"
contains "  … and closes the quote"                  "--- end of the daemon log ---"
# The reason stays the last line: deploy.sh reads it off the tail to
# notify with, and to decide whether to roll back.
if [ "$(printf '%s\n' "$out" | tail -1)" = "the daemon did not log 'Runtime ready' within 3s (docker compose logs fqd)" ]; then
    printf '  ok   the reason is still the last line\n'
else
    printf '  FAIL the reason is not the last line: %q\n' "$(printf '%s\n' "$out" | tail -1)"; failed=1
fi
run "$tmp/panic.log" 5
contains "a refused start quotes the log too"        "--- last 30 lines of the daemon log ---"

# 6. A container that logged nothing at all says so, rather than
#    printing an empty quote block.
: > "$tmp/silent.log"
run "$tmp/silent.log" 2
check "a silent container says it was silent" 1 6 "the daemon logged nothing since it was started"

[ "$failed" = 0 ] && echo "ready-check: all cases pass" || { echo "ready-check: FAILED" >&2; exit 1; }

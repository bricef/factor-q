#!/usr/bin/env bash
#
# The parallel-workers live drill (plan §3, the Phase-2 gate's live
# leg): a scratch daemon with N real invocations in flight, driven
# through the drain / clean-shutdown / crash-recovery lifecycle.
#
# What it proves, on the real binary against a real broker and LLM:
#   0. the startup guard refuses max_concurrent > 1 without
#      per-invocation workspaces (fails loud, not silent clobbering);
#   1. N invocations run concurrently, each in its own provisioned
#      workspace directory; `fq down` suspends all N at step
#      boundaries, the daemon exits cleanly, and the suspended
#      workspaces survive;
#   2. the next binary's recovery resumes each suspended invocation
#      exactly once, to completion, and reclaims the workspaces;
#   3. SIGTERM is drain semantics (ADR-0027): all N suspend at step
#      boundaries, workspaces survive, the next start resumes them —
#      what a process manager or `docker stop` gets;
#   4. a hard kill (SIGKILL) with N in flight loses nothing: restart
#      recovery resumes and completes all N. (Ctrl-C is documented as
#      a fast stop — crash-equivalent — so this leg covers it too.)
#
# The per-invocation workspace directories are the observable: N dirs
# means N in flight, dirs persisting after a drain means suspended,
# zero dirs means completed-and-reclaimed.
#
# Prerequisites:
#   - ANTHROPIC_API_KEY set (each invocation makes 2 haiku calls)
#   - NATS with JetStream running (`just infra-up`), and no other fq
#     daemon consuming the same broker's trigger stream
#   - the fq binary built (`just drill` builds first)
#
# Run with `just drill`, or directly: tests/smoke/drain-drill.sh

set -euo pipefail

# --- configuration -----------------------------------------------------

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FQ_BIN="${REPO_ROOT}/target/debug/fq"
FQD_BIN="${REPO_ROOT}/target/debug/fqd"
TMP_ROOT="$(mktemp -d -t fq-drill-XXXXXX)"
N=3
SLEEP_SECS=15
AGENT_ID="drill-sleeper-$$"
export FQ_DAEMON_CONFIG="${TMP_ROOT}/fqd.toml"
# The pairing store is user-side, under XDG_CONFIG_HOME. Without
# this, `fq down` below resolves through the developer's own
# pairings and — with exactly one paired daemon, the normal case —
# stops THAT daemon instead of the drill's.
export XDG_CONFIG_HOME="${TMP_ROOT}/config"
# The edge identity lives in the state dir, and the default is
# shared with any real daemon on this box. Sharing it means the
# identity is *adopted* rather than minted, so no admin token is
# printed — and the drill has no way to pair. Its own dir, its own
# identity, its own token.
export FQ_STATE_DIR="${TMP_ROOT}/state"

CHECKS_RUN=0
CHECKS_FAILED=0

cleanup() {
    if [[ -n "${RUN_PID:-}" ]] && kill -0 "${RUN_PID}" 2>/dev/null; then
        kill -KILL "${RUN_PID}" 2>/dev/null || true
    fi
    if [[ -d "${TMP_ROOT}" ]]; then
        rm -rf "${TMP_ROOT}"
    fi
}
trap cleanup EXIT

# --- output helpers ----------------------------------------------------

bold()  { printf '\033[1m%s\033[0m' "$1"; }
green() { printf '\033[32m%s\033[0m' "$1"; }
red()   { printf '\033[31m%s\033[0m' "$1"; }
yellow(){ printf '\033[33m%s\033[0m' "$1"; }

info()  { printf '%s %s\n' "$(yellow 'i')" "$1"; }
section() { printf '\n%s\n' "$(bold "== $1 ==")"; }

check() {
    local desc="$1"; shift
    CHECKS_RUN=$((CHECKS_RUN + 1))
    if "$@"; then
        printf '%s %s\n' "$(green '✓')" "${desc}"
    else
        printf '%s %s\n' "$(red 'x')" "${desc}"
        CHECKS_FAILED=$((CHECKS_FAILED + 1))
    fi
}

# Poll until `cmd` succeeds or `timeout` seconds pass.
wait_for() {
    local timeout="$1" desc="$2"; shift 2
    local waited=0
    until "$@"; do
        if (( waited >= timeout )); then
            printf '%s timed out after %ss waiting for: %s\n' "$(red 'x')" "${timeout}" "${desc}"
            return 1
        fi
        sleep 1
        waited=$((waited + 1))
    done
}

workspace_dirs() {
    find "${TMP_ROOT}/workspace" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | wc -l
}

dirs_are()      { [[ "$(workspace_dirs)" -eq "$1" ]]; }
daemon_exited() { ! kill -0 "${RUN_PID}" 2>/dev/null; }

start_daemon() {
    local log="$1"
    "${FQD_BIN}" >"${log}" 2>&1 &
    RUN_PID=$!
    wait_for 30 "daemon ready" grep -q "Runtime ready" "${log}"
    pair_with_daemon "${log}"
}

# `fq down` is an edge command: it needs an address to dial and a token
# to present. Both are printed once, at startup — the address because
# the bind is port 0, the token because this is a fresh state dir.
pair_with_daemon() {
    local log="$1" addr token
    wait_for 30 "edge listening" grep -q "edge is listening on" "${log}"
    addr="$(sed -n 's/.*edge is listening on \([0-9.]*:[0-9]*\).*/\1/p' "${log}" | tail -1)"
    # The token is printed once ever — when the identity is *minted*.
    # Every restart after the first adopts the identity from the state
    # dir and prints nothing, so cache it. The address is not cacheable:
    # the bind is port 0, so it is new on every start.
    token="$(awk '/edge: admin token/ { seen = 1; next } seen && NF { print $1; exit }' "${log}")"
    if [[ -n "${token}" ]]; then
        printf '%s\n' "${token}" > "${TMP_ROOT}/admin-token"
    elif [[ -r "${TMP_ROOT}/admin-token" ]]; then
        token="$(cat "${TMP_ROOT}/admin-token")"
    fi
    [[ -n "${addr}" && -n "${token}" ]] || {
        printf '%s could not read the edge address or admin token from %s\n' "$(red 'x')" "${log}"
        exit 1
    }
    "${FQ_BIN}" connect "${addr}" --token "${token}" >/dev/null 2>&1 || {
        printf '%s fq connect failed\n' "$(red 'x')"
        exit 1
    }
    printf '%s\n' "${addr}" > "${TMP_ROOT}/edge-addr"
}

# Every client verb dials the daemon this drill just started. Each
# restart binds a fresh port and adds a pairing, so without an explicit
# address the client would have several stored and refuse to guess.
fq_client() {
    "${FQ_BIN}" --addr "$(cat "${TMP_ROOT}/edge-addr")" "$@"
}

trigger_n() {
    for i in $(seq 1 "${N}"); do
        fq_client trigger "${AGENT_ID}" "{\"drill\":${i}}" --via-nats >/dev/null
    done
}

# --- scratch project ---------------------------------------------------

section "scratch project"
[[ -n "${ANTHROPIC_API_KEY:-}" ]] || { printf '%s ANTHROPIC_API_KEY is not set\n' "$(red 'x')"; exit 1; }
[[ -x "${FQ_BIN}" ]]  || { printf '%s fq binary missing — run `just build`\n' "$(red 'x')"; exit 1; }
[[ -x "${FQD_BIN}" ]] || { printf '%s fqd binary missing — run `just build`\n' "$(red 'x')"; exit 1; }

mkdir -p "${TMP_ROOT}/agents" "${TMP_ROOT}/workspace" "${TMP_ROOT}/cache"

cat > "${TMP_ROOT}/fqd.toml" <<EOF
[nats]
url = "${FQ_NATS_URL:-nats://fq-dev-token@127.0.0.1:4222}"

[agents]
directory = "agents"

[workspace]
path = "${TMP_ROOT}/workspace"
per_invocation = true

[worker]
max_concurrent_invocations = ${N}

[edge]
# Port 0 — the kernel picks. A fixed port collides with any
# daemon already running on this box, including the one whose
# pairing we just isolated ourselves from.
bind = "127.0.0.1:0"

[cache]
directory = "${TMP_ROOT}/cache"

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
models = ["claude-haiku-4-5"]
EOF

cat > "${TMP_ROOT}/agents/${AGENT_ID}.md" <<EOF
---
name: ${AGENT_ID}
model: claude-haiku-4-5
budget: 0.25
max_iterations: 4
tools:
  - shell
sandbox:
  exec_cwd:
    - \${workspace}
---

You are a drill agent. Call the shell tool exactly once, with these
parameters: {"command": ["sleep", "${SLEEP_SECS}"], "cwd": "\${workspace}"}.
After the tool result arrives, respond with exactly: drill-done. Do not
call any other tool.
EOF
info "scratch project at ${TMP_ROOT} (agent ${AGENT_ID}, N=${N})"

# --- phase 0: the startup guard fails loud -----------------------------

section "phase 0 — startup guard"
sed -i 's/per_invocation = true/per_invocation = false/' "${TMP_ROOT}/fqd.toml"
if "${FQD_BIN}" >"${TMP_ROOT}/guard.log" 2>&1; then
    check "daemon refuses max_concurrent > 1 without per-invocation workspaces" false
else
    check "daemon refuses max_concurrent > 1 without per-invocation workspaces" \
        grep -q "requires per-invocation" "${TMP_ROOT}/guard.log"
fi
sed -i 's/per_invocation = false/per_invocation = true/' "${TMP_ROOT}/fqd.toml"

# --- phase 1: drain with N in flight ------------------------------------

section "phase 1 — drain with ${N} in flight"
start_daemon "${TMP_ROOT}/daemon-1.log"
trigger_n
check "all ${N} invocations in flight (one workspace dir each)" \
    wait_for 60 "${N} workspace dirs" dirs_are "${N}"

fq_client down >/dev/null
check "daemon exits after the drain joins in-flight work" \
    wait_for 90 "daemon exit" daemon_exited
# The dispatcher stops consuming by one of two paths, and which one it
# takes is a race this drill does not control. `draining()` logs "no
# longer consuming new triggers", but it is only reached at two loop
# checkpoints — the top, and after a capacity wait. With every worker
# slot already full and no further trigger arriving during the drain
# window, the dispatcher is instead parked awaiting the next message
# and leaves through its shutdown arm, logging "received shutdown
# signal". Verified against a live drill: with N=3 and max_concurrent=3
# it takes the shutdown path every time.
#
# Asserting the first spelling alone was asserting which branch won a
# race. What the drain actually guarantees is that the dispatcher
# stopped deliberately rather than died — so accept either line, and
# leave the drain's real guarantees to the checks around this one,
# which cover suspend, resume and exactly-once.
check "dispatcher stopped consuming deliberately" \
    grep -qE "no longer consuming new triggers|trigger dispatcher received shutdown signal" \
    "${TMP_ROOT}/daemon-1.log"
check "all ${N} suspended workspaces survive the shutdown" dirs_are "${N}"

# --- phase 2: next-binary recovery resumes each exactly once ------------

section "phase 2 — recovery resumes ${N}"
start_daemon "${TMP_ROOT}/daemon-2.log"
check "recovery spawned ${N} resume tasks" \
    grep -q "resume tasks:     ${N} spawned" "${TMP_ROOT}/daemon-2.log"
check "each suspended invocation resumed to completion (workspaces reclaimed)" \
    wait_for 90 "workspaces reclaimed" dirs_are 0
check "each invocation resumed exactly once" \
    [ "$(grep -c 'resuming reducer invocation' "${TMP_ROOT}/daemon-2.log")" -eq "${N}" ]

# --- phase 3: SIGTERM is drain semantics ---------------------------------

section "phase 3 — SIGTERM (graceful drain) with ${N} in flight"
trigger_n
check "all ${N} new invocations in flight" \
    wait_for 60 "${N} workspace dirs" dirs_are "${N}"
kill -TERM "${RUN_PID}"
check "daemon exits after SIGTERM's drain suspends in-flight work" \
    wait_for 90 "daemon exit" daemon_exited
check "all ${N} suspended workspaces survive SIGTERM" dirs_are "${N}"

start_daemon "${TMP_ROOT}/daemon-3.log"
check "restart resumes and completes all ${N} (workspaces reclaimed)" \
    wait_for 90 "workspaces reclaimed" dirs_are 0

# --- phase 4: hard kill loses nothing -----------------------------------

section "phase 4 — crash (SIGKILL) with ${N} in flight"
trigger_n
check "all ${N} invocations in flight" \
    wait_for 60 "${N} workspace dirs" dirs_are "${N}"
kill -KILL "${RUN_PID}"
wait_for 10 "daemon killed" daemon_exited
check "killed daemon leaves the ${N} workspaces on disk" dirs_are "${N}"

start_daemon "${TMP_ROOT}/daemon-4.log"
check "restart recovery resumes all ${N} crashed invocations to completion" \
    wait_for 120 "workspaces reclaimed" dirs_are 0
kill -INT "${RUN_PID}" 2>/dev/null || true
wait_for 30 "daemon exit" daemon_exited || true

# --- summary -------------------------------------------------------------

section "summary"
if (( CHECKS_FAILED > 0 )); then
    printf '%s %d/%d checks failed\n' "$(red 'DRILL FAILED')" "${CHECKS_FAILED}" "${CHECKS_RUN}"
    exit 1
fi
printf '%s all %d checks passed\n' "$(green 'DRILL PASSED')" "${CHECKS_RUN}"

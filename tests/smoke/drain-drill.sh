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
#      workspace directory; `fq drain` suspends all N at step
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
FQ_BIN="${REPO_ROOT}/services/fq-runtime/target/debug/fq"
TMP_ROOT="$(mktemp -d -t fq-drill-XXXXXX)"
N=3
SLEEP_SECS=15
AGENT_ID="drill-sleeper-$$"
export FQ_CONFIG="${TMP_ROOT}/fq.toml"

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
    "${FQ_BIN}" run >"${log}" 2>&1 &
    RUN_PID=$!
    wait_for 30 "daemon ready" grep -q "Runtime ready" "${log}"
}

trigger_n() {
    for i in $(seq 1 "${N}"); do
        "${FQ_BIN}" trigger "${AGENT_ID}" "{\"drill\":${i}}" --via-nats >/dev/null
    done
}

# --- scratch project ---------------------------------------------------

section "scratch project"
[[ -n "${ANTHROPIC_API_KEY:-}" ]] || { printf '%s ANTHROPIC_API_KEY is not set\n' "$(red 'x')"; exit 1; }
[[ -x "${FQ_BIN}" ]] || { printf '%s fq binary missing — run `just build`\n' "$(red 'x')"; exit 1; }

mkdir -p "${TMP_ROOT}/agents" "${TMP_ROOT}/workspace" "${TMP_ROOT}/cache"

cat > "${TMP_ROOT}/fq.toml" <<EOF
[nats]
url = "${FQ_NATS_URL:-nats://fq-dev-token@127.0.0.1:4222}"

[agents]
directory = "agents"

[workspace]
path = "${TMP_ROOT}/workspace"
per_invocation = true

[worker]
max_concurrent_invocations = ${N}

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
sed -i 's/per_invocation = true/per_invocation = false/' "${TMP_ROOT}/fq.toml"
if "${FQ_BIN}" run >"${TMP_ROOT}/guard.log" 2>&1; then
    check "daemon refuses max_concurrent > 1 without per-invocation workspaces" false
else
    check "daemon refuses max_concurrent > 1 without per-invocation workspaces" \
        grep -q "requires per-invocation" "${TMP_ROOT}/guard.log"
fi
sed -i 's/per_invocation = false/per_invocation = true/' "${TMP_ROOT}/fq.toml"

# --- phase 1: drain with N in flight ------------------------------------

section "phase 1 — drain with ${N} in flight"
start_daemon "${TMP_ROOT}/daemon-1.log"
trigger_n
check "all ${N} invocations in flight (one workspace dir each)" \
    wait_for 60 "${N} workspace dirs" dirs_are "${N}"

"${FQ_BIN}" drain >/dev/null
check "daemon exits after the drain joins in-flight work" \
    wait_for 90 "daemon exit" daemon_exited
check "dispatcher logged the drain" \
    grep -q "no longer consuming new triggers" "${TMP_ROOT}/daemon-1.log"
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

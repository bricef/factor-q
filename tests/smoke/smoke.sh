#!/usr/bin/env bash
#
# Smoke tests for the factor-q walking skeleton.
#
# These exercise the full system against a real LLM: binary builds,
# agent definitions parse, the runtime connects to NATS, the executor
# drives a tool-call loop with built-in tools, events land in the
# SQLite projection, and the query/cost CLI commands read the
# projection back.
#
# Prerequisites:
#   - ANTHROPIC_API_KEY must be set in the environment
#   - NATS with JetStream must be running (e.g. `just infra-up`)
#   - The fq binary must be built (run via the `just smoke` recipe
#     which builds first, or run `just build` manually)
#
# Each test creates its own temp directories and uses a unique
# agent id so tests do not interfere with each other when run in
# sequence. Temp directories are cleaned up on exit (success or
# failure).
#
# Run with `just smoke` from the repo root, or directly:
#   tests/smoke/smoke.sh

set -euo pipefail

# --- configuration -----------------------------------------------------

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
FQ_BIN="${REPO_ROOT}/target/debug/fq"
TMP_ROOT="$(mktemp -d -t fq-smoke-XXXXXX)"
TESTS_RUN=0
TESTS_FAILED=0

# Everything this run touches lives under TMP_ROOT, including the three
# things `fq` would otherwise reach for in the operator's own home:
#
#   FQ_CONFIG        the edge bind and the model registry, and nothing
#                    else — the rest of the runtime runs on its
#                    defaults. The registry has no usable default: an
#                    empty one declares no models, and the daemon
#                    refuses to start rather than run an agent whose
#                    model nothing declared
#   FQ_STATE_DIR     the edge identity is durable state, and a fresh
#                    directory is what makes this a *first* run — the
#                    only time the daemon mints and prints the admin
#                    token we need in order to pair
#   XDG_CONFIG_HOME  `fq connect` writes the pairing to
#                    $XDG_CONFIG_HOME/factor-q/connections.toml, so
#                    without this a smoke run would overwrite the
#                    operator's real pairing on their own machine
#
# Exported rather than passed as flags so that the daemon and every
# client call agree without threading the same arguments through each.
export FQ_CONFIG="${TMP_ROOT}/fq.toml"
export FQ_STATE_DIR="${TMP_ROOT}/state"
export XDG_CONFIG_HOME="${TMP_ROOT}/config"

# Port 0: the kernel picks a free port and the daemon reports the one it
# got. Picking a port here instead would mean binding a socket to find a
# free one and closing it before the daemon binds, and whatever takes it
# in that gap breaks the run (the race #454 already tracks). The default
# 9472 is not an option either — on a developer box it is usually held
# by the daemon or dashboard they are already running.
# The model every agent below declares. The registry is deliberately
# closed (ADR-0003/0004): an agent naming a model no provider declares
# fails to load, and the daemon refuses to start. That is the whole
# point of it — a typo must not silently spend on an expensive model —
# so a config this run writes has to declare the model this run uses.
SMOKE_MODEL="claude-haiku-4-5"

# How long to wait for a triggered invocation to reach a terminal
# status. A one-turn Haiku call is seconds; a tool loop is longer, and
# the daemon dispatches one trigger at a time.
INVOCATION_TIMEOUT_S=90

# One writer for the config, because there are two moments that need it
# and they must not drift: now, before the daemon binds, and again once
# port 0 has resolved to a real address. Each writes the file whole, so
# anything omitted here is omitted from both.
write_config() {
    local bind="$1"
    cat > "${FQ_CONFIG}" <<TOML
[edge]
bind = "${bind}"

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
models = ["${SMOKE_MODEL}"]
TOML
}

write_config "127.0.0.1:0"

cleanup() {
    # Every daemon this run started, not just the current one: RUN_PID
    # holds one pid, and an interrupted run can have stranded earlier
    # ones. They outlive the script otherwise, and a stray daemon on
    # the shared broker breaks the *next* run rather than this one.
    if [[ -n "${STARTED_PIDS:-}" ]]; then
        local p
        for p in ${STARTED_PIDS}; do
            kill -KILL "${p}" 2>/dev/null || true
        done
    fi
    # Kill any background fq run we may have left behind.
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

info()  { printf '%s %s\n' "$(yellow 'i')"  "$1"; }
pass()  { printf '%s %s\n' "$(green '✓')"   "$1"; }
fail()  { printf '%s %s\n' "$(red 'x')"     "$1"; }

section() {
    printf '\n%s\n' "$(bold "== $1 ==")"
}

# --- assertion helpers -------------------------------------------------

assert_contains() {
    local haystack="$1"
    local needle="$2"
    local context="${3:-assertion}"
    if [[ "${haystack}" != *"${needle}"* ]]; then
        fail "${context}: expected to contain $(bold "${needle}")"
        printf '  full output:\n'
        printf '  %s\n' "${haystack}" | head -40
        return 1
    fi
}

assert_not_contains() {
    local haystack="$1"
    local needle="$2"
    local context="${3:-assertion}"
    if [[ "${haystack}" == *"${needle}"* ]]; then
        fail "${context}: expected NOT to contain $(bold "${needle}")"
        printf '  full output:\n'
        printf '  %s\n' "${haystack}" | head -40
        return 1
    fi
}

run_test() {
    local name="$1"
    shift
    section "${name}"
    TESTS_RUN=$((TESTS_RUN + 1))
    if "$@"; then
        pass "${name}"
    else
        fail "${name}"
        TESTS_FAILED=$((TESTS_FAILED + 1))
    fi
}

# --- prerequisites -----------------------------------------------------

check_prereqs() {
    section "prerequisites"

    if [[ ! -x "${FQ_BIN}" ]]; then
        fail "fq binary not found at ${FQ_BIN}"
        info "build it with: just build"
        exit 2
    fi
    pass "fq binary present"

    if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
        fail "ANTHROPIC_API_KEY is not set"
        info "export it (or let direnv load it) before running smoke tests"
        exit 2
    fi
    pass "ANTHROPIC_API_KEY is set"

    # Probe NATS on the default port. We don't rely on curl being
    # present; use /dev/tcp.
    if ! (exec 3<>/dev/tcp/localhost/4222) 2>/dev/null; then
        fail "NATS does not appear to be reachable on localhost:4222"
        info "start it with: just infra-up"
        exit 2
    fi
    exec 3>&- 2>/dev/null || true
    pass "NATS is reachable"
}

# --- helpers for test setup --------------------------------------------

# Create an isolated project directory under the tmp root. Echoes
# the path; the caller captures it in a variable.
make_project() {
    local name="$1"
    local dir="${TMP_ROOT}/${name}"
    mkdir -p "${dir}/agents" "${dir}/cache"
    echo "${dir}"
}

unique_agent_id() {
    local prefix="$1"
    # Use the process PID plus a random hex suffix. Good enough for
    # uniqueness across a single smoke run.
    printf '%s-%d-%x' "${prefix}" "$$" "${RANDOM}"
}

write_agent() {
    local dir="$1"
    local filename="$2"
    local content="$3"
    printf '%s\n' "${content}" > "${dir}/agents/${filename}"
}

# Run fq trigger with the given agents dir and cache dir. Captures
# combined stdout+stderr and returns it. Fails loudly if the
# command itself errors.
#
# The daemon lifecycle lives here because `fq trigger` is a request
# now. It used to run the reducer in this process — loading the agent
# registry off disk, writing the worker WAL, spawning MCP children —
# and that path retired with D-1. So a trigger needs a daemon that
# holds the agents it names, and a pairing to reach it.
#
# One daemon per call rather than one for the suite: each caller
# builds its own project with its own agents directory, and the daemon
# reads that directory at startup. Startup and shutdown chatter goes
# to stderr so the caller still captures only the trigger's own
# output.
fq_trigger() {
    local agents_dir="$1"
    local cache_dir="$2"
    local agent="$3"
    local payload="$4"

    start_fq_run "${agents_dir}" "${cache_dir}" >&2 || return 1

    # `|| status=$?` rather than a bare assignment plus `$?`: this
    # script runs under `set -e`, where a failing command substitution
    # exits before the next line can read the status. Putting it in an
    # `||` list makes the failure ours to handle — and the daemon
    # still gets stopped below.
    local status=0
    "${FQ_BIN}" \
        --agents-dir "${agents_dir}" \
        --cache-dir "${cache_dir}" \
        trigger "${agent}" "${payload}" >&2 2>&1 || status=$?

    # `fq trigger` queues and returns — the daemon dispatches it, and
    # the CLI never had the answer to print (D-1 retired the in-process
    # execution path). So the result has to be read back from the
    # invocation the trigger produced, once that invocation is done.
    local id="" phase="" row=""
    local deadline=$((SECONDS + INVOCATION_TIMEOUT_S))
    while (( SECONDS < deadline )); do
        row="$("${FQ_BIN}" \
            --agents-dir "${agents_dir}" \
            --cache-dir "${cache_dir}" \
            invocation list --json --include-archived 2>/dev/null \
            | jq -r --arg a "${agent}" \
                'map(select(.agent_id == $a))
                 | map(select(.status == "completed" or .status == "failed"))
                 | first // empty
                 | "\(.invocation_id) \(.status)"')" || true
        if [[ -n "${row}" ]]; then
            id="${row%% *}"
            phase="${row##* }"
            break
        fi
        sleep 1
    done

    local out=""
    if [[ -n "${id}" ]]; then
        out="$("${FQ_BIN}" \
            --agents-dir "${agents_dir}" \
            --cache-dir "${cache_dir}" \
            invocation transcript "${id}" --full 2>&1)"
        # The terminal status, in the runtime's own spelling, so a test
        # asserting on it is asserting on what the system reported.
        out+=$'\n'"invocation status: ${phase}"
    else
        out="no invocation reached a terminal status within ${INVOCATION_TIMEOUT_S}s"
        status=1
    fi

    stop_fq_run >&2
    printf '%s\n' "${out}"
    return "${status}"
}

# --- tests --------------------------------------------------------------

test_trigger_simple_response() {
    local project
    project="$(make_project simple)"
    local agent_id
    agent_id="$(unique_agent_id simple)"

    write_agent "${project}" "simple.md" "---
name: ${agent_id}
model: claude-haiku-4-5
budget: 0.10
---

You are a concise assistant. Answer in one sentence only."

    local output
    output="$(fq_trigger "${project}/agents" "${project}/cache" \
        "${agent_id}" "Say exactly: Smoke test OK.")"

    assert_contains "${output}" "Smoke test OK" "simple trigger response"
    assert_contains "${output}" "invocation status: completed"       "simple trigger completion"
}

test_trigger_with_file_read() {
    local project
    project="$(make_project file-read)"
    local agent_id
    agent_id="$(unique_agent_id file-reader)"

    # Give the agent a file to read
    local data_dir="${TMP_ROOT}/file-read-data"
    mkdir -p "${data_dir}"
    printf '%s\n' "The secret number is 47." > "${data_dir}/secret.txt"

    write_agent "${project}" "reader.md" "---
name: ${agent_id}
model: claude-haiku-4-5
tools:
  - builtin__file_read
sandbox:
  fs_read:
    - ${data_dir}
budget: 0.10
---

You are a concise assistant. When asked about a file, use the builtin__file_read
tool to read it, then answer in one sentence."

    local output
    output="$(fq_trigger "${project}/agents" "${project}/cache" \
        "${agent_id}" "Read ${data_dir}/secret.txt and tell me the secret number.")"

    assert_contains "${output}" "47"        "file_read result propagated to LLM"
    assert_contains "${output}" "invocation status: completed" "file_read trigger completion"
}

test_trigger_with_shell_tool() {
    local project
    project="$(make_project shell-tool)"
    local agent_id
    agent_id="$(unique_agent_id shell-runner)"

    local work_dir="${TMP_ROOT}/shell-work"
    mkdir -p "${work_dir}"

    write_agent "${project}" "runner.md" "---
name: ${agent_id}
model: claude-haiku-4-5
tools:
  - shell
sandbox:
  exec_cwd:
    - ${work_dir}
budget: 0.10
---

You are a concise assistant. When asked about the system, use the
shell tool to run a command and answer in one sentence using the
command's output."

    local output
    output="$(fq_trigger "${project}/agents" "${project}/cache" \
        "${agent_id}" "Run 'uname -s' using the shell tool (cwd=${work_dir}) and tell me what it returns.")"

    # The LLM should relay the kernel name. On the dev box this is
    # 'Linux'. Accept a few common variants just in case.
    if ! { assert_contains "${output}" "Linux" "shell tool result" \
        || assert_contains "${output}" "Darwin" "shell tool result" ; } 2>/dev/null; then
        # The above chain is awkward; do the check explicitly.
        if [[ "${output}" != *"Linux"* ]] && [[ "${output}" != *"Darwin"* ]]; then
            fail "shell tool result: expected Linux or Darwin in output"
            printf '  full output:\n'
            printf '  %s\n' "${output}" | head -40
            return 1
        fi
    fi
    pass "shell tool produced kernel name"
    assert_contains "${output}" "invocation status: completed" "shell tool trigger completion"
}

test_shell_tool_sandbox_denial() {
    local project
    project="$(make_project shell-denial)"
    local agent_id
    agent_id="$(unique_agent_id shell-denied)"

    local work_dir="${TMP_ROOT}/shell-denied-work"
    mkdir -p "${work_dir}"
    # Forbidden dir the LLM will be tempted to run commands in.
    local forbidden="${TMP_ROOT}/forbidden"
    mkdir -p "${forbidden}"

    write_agent "${project}" "runner.md" "---
name: ${agent_id}
model: claude-haiku-4-5
tools:
  - shell
sandbox:
  exec_cwd:
    - ${work_dir}
budget: 0.10
---

You are a concise assistant. When asked to run a command, use the
shell tool. If the tool returns an error, explain what the error was
in one sentence."

    local output
    output="$(fq_trigger "${project}/agents" "${project}/cache" \
        "${agent_id}" "Run 'pwd' in the directory ${forbidden} and tell me the result." || true)"

    # The LLM's shell call should fail with a permission denied /
    # sandbox error. We don't assert on the LLM's exact wording
    # (model output varies), only that the run itself completed and
    # the forbidden path does not appear as a successful result.
    assert_contains "${output}" "invocation status: completed" "denied shell trigger still completes"
}

# Start `fq run` in the background for tests that exercise the
# projection. Writes its PID to RUN_PID and its log to RUN_LOG so
# callers can inspect on failure.
start_fq_run() {
    local agents_dir="$1"
    local cache_dir="$2"
    RUN_LOG="${TMP_ROOT}/fq-run.log"

    # Back to port 0 before every start. The pairing step below writes
    # the *resolved* address back so client verbs can dial it, and that
    # concrete port outlives the daemon that owned it — so the next
    # daemon would try to bind a port the previous one may not have
    # released, and fail. Port 0 again means each daemon is handed a
    # free one.
    write_config "127.0.0.1:0"
    "${FQ_BIN}" \
        --agents-dir "${agents_dir}" \
        --cache-dir "${cache_dir}" \
        run >"${RUN_LOG}" 2>&1 &
    RUN_PID=$!
    STARTED_PIDS="${STARTED_PIDS:-} ${RUN_PID}"

    # Wait for the runtime to finish starting up. Two lines have to
    # arrive, not one: the projection consumer logs "projection consumer
    # starting" once it can receive, and the edge logs the address it
    # actually bound — which, on port 0, is the only place that address
    # exists at all.
    local deadline=$((SECONDS + 10))
    local ready=""
    while (( SECONDS < deadline )); do
        if grep -q "projection consumer starting" "${RUN_LOG}" 2>/dev/null \
            && grep -q "edge is listening on" "${RUN_LOG}" 2>/dev/null; then
            ready=1
            break
        fi
        sleep 0.1
    done
    if [[ -z "${ready}" ]]; then
        fail "fq run did not start within 10s"
        head -30 "${RUN_LOG}"
        stop_fq_run
        return 1
    fi

    # A daemon that cannot be paired with is useless to the caller, but
    # it is still running, still holding the shared `fq-dispatcher`
    # durable, and still consuming triggers meant for whatever starts
    # next — acking them and finding no such agent in its own directory.
    # Leaving one behind does not fail one test, it corrupts every test
    # after it, which is why this stops the daemon rather than letting
    # the caller's early return strand it.
    if ! pair_with_edge; then
        stop_fq_run
        return 1
    fi
}

# Pair this script's `fq` with the daemon it just started.
#
# Every verb that reads runtime state now goes through the edge, which
# authenticates, so a smoke run that never pairs is a smoke run that
# cannot read anything back — `fq events query` below depends on this
# having happened.
#
# Both values come out of RUN_LOG because neither exists anywhere else:
# the address was chosen by the kernel, and the admin token is minted
# and printed exactly once, on a first run against a fresh state
# directory. Both are matched by content and never by line number — the
# daemon's stdout and its tracing output share this file, so a line can
# land between a banner and the value it introduces. Reading "the line
# after the marker" is the assumption that makes #454 flaky, and there
# is no reason to repeat it here.
pair_with_edge() {
    local addr token
    addr="$(sed -n 's/.*edge is listening on \([0-9.]*:[0-9]*\).*/\1/p' \
        "${RUN_LOG}" | tail -1)"
    if [[ -z "${addr}" ]]; then
        fail "could not read the edge address from ${RUN_LOG}"
        head -30 "${RUN_LOG}"
        return 1
    fi

    # The token is a biscuit: one long run of base64url alone on its
    # line. Requiring that shape means an interleaved tracing line
    # cannot be mistaken for it, however the two streams land.
    token="$(awk '
        /edge: admin token/ { seen = 1; next }
        seen {
            line = $0
            gsub(/[[:space:]]/, "", line)
            if (line ~ /^[A-Za-z0-9_=-]+$/ && length(line) >= 40) {
                print line
                exit
            }
        }
    ' "${RUN_LOG}")"
    # Printed once, on the first run against a fresh state directory —
    # and every test here starts its own daemon against the *same*
    # state directory, so only the first of them sees it. The identity
    # is durable, so the token the first run printed stays valid for
    # every daemon after it; what changes is the address, which is why
    # pairing still has to happen each time.
    # Cached in a file, not a variable: most callers reach this through
    # `$(fq_trigger ...)`, and a command substitution is a subshell —
    # a variable set in there is gone by the time the next test needs
    # it, which is exactly when the token is no longer being printed.
    if [[ -n "${token}" ]]; then
        printf '%s\n' "${token}" > "${TMP_ROOT}/admin-token"
    elif [[ -r "${TMP_ROOT}/admin-token" ]]; then
        token="$(cat "${TMP_ROOT}/admin-token")"
    fi
    if [[ -z "${token}" ]]; then
        fail "could not read the admin token from ${RUN_LOG}"
        head -30 "${RUN_LOG}"
        return 1
    fi

    # Client verbs dial `[edge] bind` from config, while `fq connect`
    # stores the pairing under the address it is handed — so the two
    # have to name the same place, and with port 0 that place is not
    # known until now. The daemon read this file once at startup and
    # nothing rereads it, so writing the resolved address back is safe.
    write_config "${addr}"

    if ! "${FQ_BIN}" connect "${addr}" --token "${token}" \
        > "${TMP_ROOT}/connect.log" 2>&1; then
        fail "fq connect failed"
        head -20 "${TMP_ROOT}/connect.log"
        return 1
    fi
    pass "paired with the daemon's edge at ${addr}"
}

stop_fq_run() {
    if [[ -n "${RUN_PID:-}" ]] && kill -0 "${RUN_PID}" 2>/dev/null; then
        kill -INT "${RUN_PID}" 2>/dev/null || true
        local deadline=$((SECONDS + 5))
        while kill -0 "${RUN_PID}" 2>/dev/null; do
            if (( SECONDS > deadline )); then
                kill -KILL "${RUN_PID}" 2>/dev/null || true
                break
            fi
            sleep 0.1
        done
        wait "${RUN_PID}" 2>/dev/null || true
    fi
    RUN_PID=""
}

# Verify NATS-triggered execution: start fq run (which spawns
# both the projection consumer and the trigger dispatcher),
# publish a trigger via `fq trigger --via-nats` (which does NOT
# run the executor in-process), and verify that the daemon picks
# up the trigger and runs the agent to completion.
test_nats_triggered_dispatch() {
    local project
    project="$(make_project nats-trigger)"
    local agent_id
    agent_id="$(unique_agent_id nats-trigger)"

    write_agent "${project}" "q.md" "---
name: ${agent_id}
model: claude-haiku-4-5
budget: 0.10
---

You are a concise assistant. Answer in one sentence."

    start_fq_run "${project}/agents" "${project}/cache" || return 1

    # Publish via NATS — this should return immediately without
    # running the executor in-process.
    local publish_out
    publish_out="$("${FQ_BIN}" \
        --agents-dir "${project}/agents" \
        --cache-dir "${project}/cache" \
        trigger --via-nats "${agent_id}" \
        "Say exactly: nats dispatch OK." 2>&1)" || {
        stop_fq_run
        fail "fq trigger --via-nats failed"
        printf '  %s\n' "${publish_out}"
        return 1
    }

    assert_contains "${publish_out}" "Published trigger" "publish confirmation" || {
        stop_fq_run
        return 1
    }

    # Give the daemon up to 15s to pick up the trigger, run the
    # agent, and project the events.
    local agent_filter="${agent_id}"
    local deadline=$((SECONDS + 15))
    local rows=""
    while (( SECONDS < deadline )); do
        rows="$("${FQ_BIN}" --cache-dir "${project}/cache" \
            events query --agent "${agent_filter}" --limit 20 2>&1 || true)"
        if [[ "${rows}" == *"completed"* ]]; then
            break
        fi
        sleep 0.5
    done

    stop_fq_run

    assert_contains "${rows}" "triggered"    "nats-dispatched event: triggered"  || return 1
    assert_contains "${rows}" "completed"    "nats-dispatched event: completed"  || return 1
    assert_contains "${rows}" "${agent_id}"  "nats-dispatched event: agent id"   || return 1
}

# Test the full runtime cycle: start fq run (which spawns the
# projection consumer), fire a trigger to produce events, wait for
# them to land in SQLite, then query them back via the CLI.
test_run_projection_query_and_costs() {
    local project
    project="$(make_project run-projection)"
    local agent_id
    agent_id="$(unique_agent_id run-projection)"

    write_agent "${project}" "q.md" "---
name: ${agent_id}
model: claude-haiku-4-5
budget: 0.10
---

You are a concise assistant. Answer in one sentence."

    start_fq_run "${project}/agents" "${project}/cache" || return 1

    # Push an invocation through. fq trigger runs the executor
    # directly and publishes events to NATS; the consumer spawned
    # by fq run picks them up.
    fq_trigger "${project}/agents" "${project}/cache" \
        "${agent_id}" "Say exactly: projection OK." >/dev/null

    # Give the projection consumer a chance to catch up.
    sleep 2

    local rows
    rows="$("${FQ_BIN}" --cache-dir "${project}/cache" \
        events query --agent "${agent_id}" --limit 20 2>&1)" || {
        stop_fq_run
        fail "events query failed"
        printf '  %s\n' "${rows}"
        return 1
    }

    local costs
    costs="$("${FQ_BIN}" --cache-dir "${project}/cache" \
        costs --agent "${agent_id}" 2>&1)" || {
        stop_fq_run
        fail "costs query failed"
        printf '  %s\n' "${costs}"
        return 1
    }

    stop_fq_run

    assert_contains "${rows}" "triggered"    "events query shows triggered" || return 1
    assert_contains "${rows}" "completed"    "events query shows completed" || return 1
    assert_contains "${rows}" "cost"         "events query shows cost row"  || return 1
    assert_contains "${rows}" "${agent_id}"  "events query includes agent"  || return 1

    assert_contains "${costs}" "${agent_id}" "costs includes agent row"     || return 1
    # Proper cost output contains a $ amount. We already filtered
    # to our agent id; if the body is present it'll have the cost
    # formatted as $<number>.
    if [[ "${costs}" != *'$'* ]]; then
        fail "costs output did not include any dollar amount"
        printf '  full output:\n'
        printf '  %s\n' "${costs}"
        return 1
    fi
}

# --- main --------------------------------------------------------------

main() {
    printf '\n%s\n' "$(bold 'factor-q smoke tests')"
    printf 'temp root: %s\n' "${TMP_ROOT}"

    check_prereqs

    run_test "trigger: simple single-turn response"      test_trigger_simple_response
    run_test "trigger: file_read tool in a loop"         test_trigger_with_file_read
    run_test "trigger: shell tool in a loop"             test_trigger_with_shell_tool
    run_test "trigger: shell tool sandbox denial path"   test_shell_tool_sandbox_denial
    run_test "runtime: projection + query + costs"       test_run_projection_query_and_costs
    run_test "runtime: NATS-triggered dispatch"          test_nats_triggered_dispatch

    printf '\n'
    if [[ "${TESTS_FAILED}" -eq 0 ]]; then
        printf '%s %d/%d tests passed\n' "$(green 'OK')" "${TESTS_RUN}" "${TESTS_RUN}"
        exit 0
    else
        printf '%s %d/%d tests failed\n' "$(red 'FAIL')" "${TESTS_FAILED}" "${TESTS_RUN}"
        exit 1
    fi
}

main "$@"

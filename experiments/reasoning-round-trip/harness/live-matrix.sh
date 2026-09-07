#!/usr/bin/env bash
# Live reasoning round-trip matrix (#437 verification).
#
# Three agents, three models, one task each — four with a Google AI Studio
# key, which switches the Gemini arm on (2026-09-07). By default the sequential
# two-tool task of the 2026-09-04 run with a mental step in front of it
# (2026-09-07; without it neither reasoning model reasons on the turns
# that get replayed — see the README), or whatever `TASK=...` names (a
# `{work}` token in it expands to the fixture directory) — which is how
# the #511 parallel-tool-call probe ran on the same matrix. Everything
# runs in a scratch daemon against a private JetStream broker so nothing
# here can touch the dogfood stack (4223) or the shared dev broker (4222).
#
# Needs raw TCP to localhost (private broker + edge): run it outside any sandbox
# that proxies HTTP only, e.g. `mise exec -- bash harness/live-matrix.sh`.
# Prereqs: `just build-runtime`, `just install-nats`, OPENROUTER_API_KEY and
# ANTHROPIC_API_KEY in the env file; AISTUDIO_API_KEY there too for the Gemini
# arm. Spends well under $0.20.
set -euo pipefail

W="$(cd "$(dirname "$0")/../../.." && pwd)"
OUT="${OUT:-${TMPDIR:-/tmp}/fq-live-matrix}"
TMP_ROOT="$OUT/run"
FQ="$W/target/debug/fq"; FQD="$W/target/debug/fqd"; NATS="$W/.tools/nats-server"
NATS_PORT=14222; NATS_TOKEN=fq-live-token
# The daemon reads the broker token from the variable `[nats] token_env`
# names, never from the URL (#540). A private name, so a FQ_NATS_TOKEN
# exported for the shared dev broker cannot leak into this run.
export FQ_LIVE_NATS_TOKEN="$NATS_TOKEN"
INVOCATION_TIMEOUT_S=${INVOCATION_TIMEOUT_S:-600}

log() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*"; }

rm -rf "$TMP_ROOT"
mkdir -p "$TMP_ROOT"/{state,config,cache,agents,work,nats} "$OUT"
export FQ_DAEMON_CONFIG="$TMP_ROOT/fqd.toml"
export FQ_STATE_DIR="$TMP_ROOT/state"
export XDG_CONFIG_HOME="$TMP_ROOT/config"
export XDG_CACHE_HOME="$TMP_ROOT/cache"
unset FQ_ADDR FQ_EDGE FQ_EDGE_TOKEN FQ_EDGE_FINGERPRINT FQ_NATS_URL || true

# Keys come from the repo-root .env (override with ENV_FILE=...); read into the
# process only, never printed. No file is fine — CI passes the keys in the
# environment — and the checks below still insist on both.
ENV_FILE="${ENV_FILE:-$W/.env}"
if [ -f "$ENV_FILE" ]; then set -a; . "$ENV_FILE"; set +a; fi
: "${OPENROUTER_API_KEY:?OPENROUTER_API_KEY missing}"
: "${ANTHROPIC_API_KEY:?ANTHROPIC_API_KEY missing}"
# The Gemini arm runs only with a Google AI Studio key, and says so either
# way: a three-arm run must never pass for a four-arm one.
GEMINI_ARM=""
if [[ -n "${AISTUDIO_API_KEY:-}" ]]; then
  GEMINI_ARM=gemini-3-thinker; log "AISTUDIO_API_KEY set — the Gemini arm runs (four arms)"
else
  log "AISTUDIO_API_KEY not set — the Gemini arm is OFF (three arms)"
fi

# ---------------------------------------------------------------- fixtures
WORK="$TMP_ROOT/work"
cat > "$WORK/notes.txt" <<'EOF'
Reasoning round-trip probe: the badger is orange.
Second line: this file exists so an agent has to read it, then count it.
Third line: nothing else here matters, but every word is counted.
EOF
EXPECTED_WORDS="$(wc -w < "$WORK/notes.txt" | tr -d ' ')"
log "fixture notes.txt has $EXPECTED_WORDS words"
# A second file, so a task can ask for two reads in one turn (#511).
cat > "$WORK/checklist.txt" <<'EOF'
Parallel probe checklist: the heron is grey.
Second line: read me in the same turn as notes.txt, not after it.
EOF

# The task. `TASK=...` substitutes another so the same daemon, agents and
# collection serve a different probe; `{work}` in it names the fixture
# directory, which the caller cannot know in advance. The sequencing
# steer ("one tool call at a time") lives in the task rather than the
# agent prompt: a task that wants parallel calls must be free to ask.
# The task opens with something to think about, before any tool is called.
# A reasoning part is only ever *carried* from a turn that another request
# follows, and on a purely mechanical first turn neither reasoning model
# reasons: Opus 5's adaptive thinking skipped it 9 times in 9, and Kimi K3
# returned a few words or an empty string. With the mental step in front,
# Opus thought 4/4 and Kimi 7/7 across every provider OpenRouter routed to
# (probes of 2026-09-07). The two tool steps and the answer are unchanged.
DEFAULT_TASK="Three steps, in order, one tool call at a time. First, work out in your head the smallest prime number greater than 40 and the sum of its digits — no tool for this. Second, read the file {work}/notes.txt with builtin__file_read. Third, run wc -w on that same file with builtin__exec, passing argv as an array. Then answer in exactly three short lines: line 1 is the prime and its digit sum, line 2 is the first line of the file verbatim, line 3 is the word count as an integer."
TASK="${TASK:-$DEFAULT_TASK}"
TASK="${TASK//\{work\}/$WORK}"
log "task: $TASK"

write_agent() { # name model budget effort-line
  cat > "$TMP_ROOT/agents/$1.md" <<EOF
---
name: $1
model: $2
tools:
  - builtin__file_read
  - builtin__exec
sandbox:
  fs_read:
    - $WORK
  exec_cwd:
    - $WORK
budget: $3
max_iterations: 8
$4
---

You are a careful assistant that reads files and runs commands. Do
exactly what the task says, then answer briefly. Pass builtin__exec
commands as an argv array, e.g. ["wc", "-w", "file"].
EOF
}
write_agent kimi-k3-reasoner   "moonshotai/kimi-k3"  1.00 "effort: medium"
write_agent opus-5-thinker     "claude-opus-5"       2.00 "effort: high"
write_agent gpt4o-mini-control "openai/gpt-4o-mini"  0.20 ""
ARMS=(kimi-k3-reasoner opus-5-thinker gpt4o-mini-control)
# Gemini 3 thinks by default and signs every function call, so the arm
# sets no effort; the readable summary rides on the adapter's capture flag.
if [[ -n "$GEMINI_ARM" ]]; then
  write_agent "$GEMINI_ARM" "gemini-3.8-flash" 0.50 ""
  ARMS+=("$GEMINI_ARM")
fi
# What each arm must show for the run to pass (verify-carry.py): a
# reasoning arm carries at least one reasoning part into its next turn,
# the control records none.
declare -A EXPECT=([kimi-k3-reasoner]=reasoning [opus-5-thinker]=reasoning [gpt4o-mini-control]=none [gemini-3-thinker]=reasoning)

write_config() { # edge bind address
  cat > "$FQ_DAEMON_CONFIG" <<EOF
[nats]
url = "nats://127.0.0.1:${NATS_PORT}"
token_env = "FQ_LIVE_NATS_TOKEN"

[agents]
directory = "agents"

[edge]
bind = "$1"

[providers.openrouter]
api_shape = "openai-compatible"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
models = ["moonshotai/kimi-k3", "openai/gpt-4o-mini"]

[providers.openrouter.pricing."moonshotai/kimi-k3"]
input_per_mtok = 3.0
output_per_mtok = 15.0
cache_read_per_mtok = 0.3

[providers.openrouter.pricing."openai/gpt-4o-mini"]
input_per_mtok = 0.15
output_per_mtok = 0.60
cache_read_per_mtok = 0.075

[providers.anthropic]
api_key_env = "ANTHROPIC_API_KEY"
models = ["claude-opus-5"]

[providers.anthropic.pricing."claude-opus-5"]
input_per_mtok = 5.0
output_per_mtok = 25.0
EOF
  # Native Gemini, only when the key is there: a provider whose key
  # variable is unset is a daemon that will not start.
  if [[ -n "$GEMINI_ARM" ]]; then
    cat >> "$FQ_DAEMON_CONFIG" <<EOF

[providers.gemini]
api_shape = "gemini"
api_key_env = "AISTUDIO_API_KEY"
models = ["gemini-3.8-flash"]

[providers.gemini.pricing."gemini-3.8-flash"]
input_per_mtok = 0.75
output_per_mtok = 3.75
cache_read_per_mtok = 0.075
EOF
  fi
}

# ---------------------------------------------------------------- lifecycle
NATS_PID=""; RUN_PID=""; TAIL_PID=""
stop_daemon() {
  if [[ -n "$RUN_PID" ]] && kill -0 "$RUN_PID" 2>/dev/null; then
    "$FQ" --addr "$(cat "$TMP_ROOT/edge-addr" 2>/dev/null || echo 127.0.0.1:1)" down --now >/dev/null 2>&1 || true
    local deadline=$((SECONDS + 15))
    while kill -0 "$RUN_PID" 2>/dev/null && (( SECONDS < deadline )); do sleep 0.2; done
    if kill -0 "$RUN_PID" 2>/dev/null; then kill -INT "$RUN_PID" 2>/dev/null || true; sleep 3; fi
    kill -KILL "$RUN_PID" 2>/dev/null || true
    wait "$RUN_PID" 2>/dev/null || true
  fi
  RUN_PID=""
}
cleanup() {
  [[ -n "$TAIL_PID" ]] && kill "$TAIL_PID" 2>/dev/null || true
  stop_daemon
  if [[ -n "$NATS_PID" ]]; then kill -TERM "$NATS_PID" 2>/dev/null || true; wait "$NATS_PID" 2>/dev/null || true; fi
  log "cleanup done (daemon + private broker stopped)"
}
trap cleanup EXIT

"$NATS" -js -a 127.0.0.1 -p "$NATS_PORT" --auth "$NATS_TOKEN" -sd "$TMP_ROOT/nats" > "$TMP_ROOT/nats.log" 2>&1 &
NATS_PID=$!
for _ in $(seq 1 50); do (exec 3<>/dev/tcp/127.0.0.1/$NATS_PORT) 2>/dev/null && break; sleep 0.1; done
(exec 3<>/dev/tcp/127.0.0.1/$NATS_PORT) 2>/dev/null || { log "private NATS did not come up"; cat "$TMP_ROOT/nats.log"; exit 1; }
log "private NATS up on 127.0.0.1:$NATS_PORT (pid $NATS_PID)"

RUN_LOG="$TMP_ROOT/fqd.log"
write_config "127.0.0.1:0"
"$FQD" --agents-dir "$TMP_ROOT/agents" --cache-dir "$TMP_ROOT/cache" > "$RUN_LOG" 2>&1 &
RUN_PID=$!
deadline=$((SECONDS + 90)); ready=""
while (( SECONDS < deadline )); do
  if grep -q "projection consumer starting" "$RUN_LOG" 2>/dev/null && grep -q "edge is listening on" "$RUN_LOG" 2>/dev/null; then ready=1; break; fi
  if ! kill -0 "$RUN_PID" 2>/dev/null; then break; fi
  sleep 0.2
done
if [[ -z "$ready" ]]; then log "fqd did not start"; sed -n '1,60p' "$RUN_LOG"; exit 1; fi

addr="$(sed -n 's/.*edge is listening on \([0-9.]*:[0-9]*\).*/\1/p' "$RUN_LOG" | tail -1)"
# The admin token is never printed: a fresh state directory gets
# `edge/admin.token` and `edge/fingerprint`, owner-only, and pairing
# needs both (the smoke suite reads them the same way).
EDGE_DIR="$FQ_STATE_DIR/edge"
[[ -n "$addr" && -r "$EDGE_DIR/admin.token" && -r "$EDGE_DIR/fingerprint" ]] || { log "could not read the edge address, admin.token or fingerprint"; sed -n '1,60p' "$RUN_LOG"; exit 1; }
token="$(<"$EDGE_DIR/admin.token")"
fingerprint="$(<"$EDGE_DIR/fingerprint")"
write_config "$addr"
"$FQ" connect "$addr" --token "$token" --fingerprint "$fingerprint" > "$TMP_ROOT/connect.log" 2>&1 || { log "fq connect failed"; cat "$TMP_ROOT/connect.log"; exit 1; }
printf '%s\n' "$addr" > "$TMP_ROOT/edge-addr"
fqc() { "$FQ" --addr "$addr" "$@"; }
log "daemon up, edge $addr, paired"

# Every payload, live, one JSON object per line.
fqc events tail --json > "$OUT/events.ndjson" 2> "$OUT/events-tail.err" &
TAIL_PID=$!
sleep 1

# ---------------------------------------------------------------- arms
declare -A RESULT
declare -A RETRIED
run_arm() { # one arm: trigger, wait, collect; sets RESULT[arm]
  local arm="$1"
  mkdir -p "$OUT/$arm/events"
  log "=== $arm: trigger"
  t0=$SECONDS
  fqc trigger "$arm" "$TASK" > "$OUT/$arm/trigger.txt" 2>&1 || { log "$arm: trigger failed"; cat "$OUT/$arm/trigger.txt"; RESULT[$arm]="trigger-failed"; return 0; }
  id=""; status=""
  deadline=$((SECONDS + INVOCATION_TIMEOUT_S))
  while (( SECONDS < deadline )); do
    row="$(fqc invocation list --json --include-archived 2>/dev/null \
      | jq -r --arg a "$arm" 'map(select(.agent_id == $a)) | map(select(.status == "completed" or .status == "failed")) | first // empty | "\(.invocation_id) \(.status)"')" || true
    if [[ -n "$row" ]]; then id="${row%% *}"; status="${row##* }"; break; fi
    sleep 2
  done
  if [[ -z "$id" ]]; then
    log "$arm: TIMED OUT after ${INVOCATION_TIMEOUT_S}s"; RESULT[$arm]="timeout"
    fqc invocation list --json --include-archived > "$OUT/$arm/invocations-at-timeout.json" 2>>"$OUT/client-stderr.log" || true
    return 0
  fi
  log "$arm: invocation $id $status in $((SECONDS - t0))s"
  RESULT[$arm]="$status"
  printf '%s\n' "$id" > "$OUT/$arm/invocation-id"
  fqc invocation transcript "$id" --full --reasoning > "$OUT/$arm/transcript-reasoning.txt" 2>&1 || true
  fqc invocation transcript "$id" --full             > "$OUT/$arm/transcript-plain.txt"     2>&1 || true
  fqc invocation transcript "$id" --json             > "$OUT/$arm/transcript.json"          2>>"$OUT/client-stderr.log" || true
  fqc invocation show "$id" --json                   > "$OUT/$arm/invocation.json"          2>>"$OUT/client-stderr.log" || true
  fqc costs --agent "$arm" --json                    > "$OUT/$arm/costs.json"               2>>"$OUT/client-stderr.log" || true
  fqc costs --agent "$arm"                           > "$OUT/$arm/costs.txt"                2>&1 || true
  fqc events query --agent "$arm" --limit 500 --json > "$OUT/$arm/events-index.json"        2>>"$OUT/client-stderr.log" || true
  # Cross-check route: index -> get, for every event id the index yields.
  n=0
  while read -r eid; do
    [[ -n "$eid" ]] || continue
    n=$((n+1))
    fqc events get "$eid" --json > "$OUT/$arm/events/$(printf '%03d' "$n")-$eid.json" 2>>"$OUT/client-stderr.log" || true
  done < <(jq -r '(if type=="array" then . else (.events // .items // []) end)[] | (.event_id // .id // empty)' "$OUT/$arm/events-index.json" 2>/dev/null || true)
  log "$arm: collected transcript, costs, $n events via get"
}

# A provider that answered 5xx or 429 through every attempt of the
# runtime's retry budget had no capacity for us, and is not wrong —
# Gemini's free tier says "high demand" for minutes at a time, and allows
# five requests a minute per model, which three turns and a retry can
# trip. Neither is a verdict on the round trip, so such an arm gets one
# more attempt after a pause. Anything else that fails stands as it fell.
capacity_failure() { # arm -> 0 when its invocation failed on a provider 5xx or 429
  local f="$OUT/$1/invocation.json"
  [[ -f "$f" ]] || return 1
  jq -e '(.. | strings) | select(test("status code .5[0-9][0-9].|UNAVAILABLE|overloaded|high demand|rate.limited|status code .429.|RESOURCE_EXHAUSTED"; "i"))' "$f" > /dev/null 2>&1
}

for arm in "${ARMS[@]}"; do
  run_arm "$arm"
  if [[ "${RESULT[$arm]:-}" == "failed" ]] && capacity_failure "$arm"; then
    log "$arm: the provider had no capacity (5xx or 429 through the retry budget) — one more attempt in 60s"
    sleep 60
    RETRIED[$arm]=1
    run_arm "$arm"
  fi
done

sleep 2
kill "$TAIL_PID" 2>/dev/null || true; TAIL_PID=""
cp "$RUN_LOG" "$OUT/fqd.log"; cp "$FQ_DAEMON_CONFIG" "$OUT/fqd.toml"; cp -r "$TMP_ROOT/agents" "$OUT/agents"
fqc costs --json > "$OUT/costs-all.json" 2>>"$OUT/client-stderr.log" || true
fqc costs        > "$OUT/costs-all.txt"  2>&1 || true

log "=== summary"
for arm in "${ARMS[@]}"; do log "$arm: ${RESULT[$arm]:-unknown}${RETRIED[$arm]:+ (second attempt; the first failed on provider capacity, a 5xx or 429)}"; done
log "events tailed: $(wc -l < "$OUT/events.ndjson") lines"
log "expected word count: $EXPECTED_WORDS"

# ------------------------------------------------------------------ verdict
# One judgement per arm from the event log, and it is the harness's exit
# status: a nightly run is only worth having if it can go red. VERIFY=0
# skips it, for a TASK= probe whose arms are being read some other way.
if [[ "${VERIFY:-1}" == "1" ]]; then
  log "=== verdict"
  expect_args=()
  for arm in "${ARMS[@]}"; do expect_args+=(--expect "$arm=${EXPECT[$arm]}"); done
  python3 "$(dirname "$0")/verify-carry.py" "$OUT/events.ndjson" "${expect_args[@]}"
fi

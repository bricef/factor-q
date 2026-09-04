#!/usr/bin/env bash
# Live reasoning round-trip matrix (#437 verification).
#
# Three agents, three models, one two-tool task each. Everything runs in
# a scratch daemon against a private JetStream broker so nothing here can
# touch the dogfood stack (4223) or the shared dev broker (4222).
#
# Needs raw TCP to localhost (private broker + edge): run it outside any sandbox
# that proxies HTTP only, e.g. `mise exec -- bash harness/live-matrix.sh`.
# Prereqs: `just build-runtime`, `just install-nats`, OPENROUTER_API_KEY and
# ANTHROPIC_API_KEY in the env file. Spends well under $0.20.
set -euo pipefail

W="$(cd "$(dirname "$0")/../../.." && pwd)"
OUT="${OUT:-${TMPDIR:-/tmp}/fq-live-matrix}"
TMP_ROOT="$OUT/run"
FQ="$W/target/debug/fq"; FQD="$W/target/debug/fqd"; NATS="$W/.tools/nats-server"
NATS_PORT=14222; NATS_TOKEN=fq-live-token
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
# process only, never printed.
ENV_FILE="${ENV_FILE:-$W/.env}"
set -a; . "$ENV_FILE"; set +a
: "${OPENROUTER_API_KEY:?OPENROUTER_API_KEY missing}"
: "${ANTHROPIC_API_KEY:?ANTHROPIC_API_KEY missing}"

# ---------------------------------------------------------------- fixtures
WORK="$TMP_ROOT/work"
cat > "$WORK/notes.txt" <<'EOF'
Reasoning round-trip probe: the badger is orange.
Second line: this file exists so an agent has to read it, then count it.
Third line: nothing else here matters, but every word is counted.
EOF
EXPECTED_WORDS="$(wc -w < "$WORK/notes.txt" | tr -d ' ')"
log "fixture notes.txt has $EXPECTED_WORDS words"

TASK="Two steps, in order. First, read the file $WORK/notes.txt with builtin__file_read. Second, run wc -w on that same file with builtin__exec, passing argv as an array. Then answer in exactly two short lines: line 1 is the first line of the file verbatim, line 2 is the word count as an integer."

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
exactly what the task says, one tool call at a time, then answer briefly.
Pass builtin__exec commands as an argv array, e.g. ["wc", "-w", "file"].
EOF
}
write_agent kimi-k3-reasoner   "moonshotai/kimi-k3"  1.00 "effort: medium"
write_agent opus-5-thinker     "claude-opus-5"       2.00 "effort: high"
write_agent gpt4o-mini-control "openai/gpt-4o-mini"  0.20 ""
ARMS=(kimi-k3-reasoner opus-5-thinker gpt4o-mini-control)

write_config() { # edge bind address
  cat > "$FQ_DAEMON_CONFIG" <<EOF
[nats]
url = "nats://${NATS_TOKEN}@127.0.0.1:${NATS_PORT}"

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
token="$(awk '/edge: admin token/ { seen = 1; next } seen { line = $0; gsub(/[[:space:]]/, "", line); if (line ~ /^[A-Za-z0-9_=-]+$/ && length(line) >= 40) { print line; exit } }' "$RUN_LOG")"
[[ -n "$addr" && -n "$token" ]] || { log "could not read edge addr/token"; sed -n '1,60p' "$RUN_LOG"; exit 1; }
write_config "$addr"
"$FQ" connect "$addr" --token "$token" > "$TMP_ROOT/connect.log" 2>&1 || { log "fq connect failed"; cat "$TMP_ROOT/connect.log"; exit 1; }
printf '%s\n' "$addr" > "$TMP_ROOT/edge-addr"
fqc() { "$FQ" --addr "$addr" "$@"; }
log "daemon up, edge $addr, paired"

# Every payload, live, one JSON object per line.
fqc events tail --json > "$OUT/events.ndjson" 2> "$OUT/events-tail.err" &
TAIL_PID=$!
sleep 1

# ---------------------------------------------------------------- arms
declare -A RESULT
for arm in "${ARMS[@]}"; do
  mkdir -p "$OUT/$arm/events"
  log "=== $arm: trigger"
  t0=$SECONDS
  fqc trigger "$arm" "$TASK" > "$OUT/$arm/trigger.txt" 2>&1 || { log "$arm: trigger failed"; cat "$OUT/$arm/trigger.txt"; RESULT[$arm]="trigger-failed"; continue; }
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
    continue
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
done

sleep 2
kill "$TAIL_PID" 2>/dev/null || true; TAIL_PID=""
cp "$RUN_LOG" "$OUT/fqd.log"; cp "$FQ_DAEMON_CONFIG" "$OUT/fqd.toml"; cp -r "$TMP_ROOT/agents" "$OUT/agents"
fqc costs --json > "$OUT/costs-all.json" 2>>"$OUT/client-stderr.log" || true
fqc costs        > "$OUT/costs-all.txt"  2>&1 || true

log "=== summary"
for arm in "${ARMS[@]}"; do log "$arm: ${RESULT[$arm]:-unknown}"; done
log "events tailed: $(wc -l < "$OUT/events.ndjson") lines"
log "expected word count: $EXPECTED_WORDS"

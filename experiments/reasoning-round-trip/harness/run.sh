#!/usr/bin/env bash
# Harness for the reasoning round-trip probe.
# Issue: https://github.com/bricef/factor-q/issues/437
#
# Usage:
#   run.sh                                  # default models (Fable 5, Opus 5)
#   run.sh --models claude-opus-4-8         # override the model list (comma-separated)
#   run.sh --secrets ~/other/.secrets/env   # override where ANTHROPIC_API_KEY is sourced from
#   run.sh --effort high --repeat 3          # what the recorded result used
#   run.sh --out results.json               # also write full JSON results
#
# The key is sourced into this process only. It is never printed, never passed on a
# command line (so it never reaches `ps`), and never written to the results file.
set -u

SECRETS="${SECRETS:-$HOME/fq-dogfood/.secrets/env}"
MODELS=""
OUT=""
EFFORT=""
REPEAT=""
HERE="$(cd "$(dirname "$0")" && pwd)"

while [ $# -gt 0 ]; do
  case "$1" in
    --secrets) SECRETS="$2"; shift 2 ;;
    --models)  MODELS="$2";  shift 2 ;;
    --effort)  EFFORT="$2";  shift 2 ;;
    --repeat)  REPEAT="$2";  shift 2 ;;
    --out)     OUT="$2";     shift 2 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

# Prefer an already-exported key; otherwise source it from the secrets file.
if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  if [ ! -r "$SECRETS" ]; then
    echo "No ANTHROPIC_API_KEY in the environment and cannot read $SECRETS" >&2
    echo "Pass --secrets <file>, or export ANTHROPIC_API_KEY." >&2
    exit 2
  fi
  # Only pull the one variable we need; ignore everything else in the file.
  ANTHROPIC_API_KEY="$(grep -m1 '^ANTHROPIC_API_KEY=' "$SECRETS" | cut -d= -f2- | tr -d '"'"'"'')"
  export ANTHROPIC_API_KEY
fi

if [ -z "${ANTHROPIC_API_KEY:-}" ]; then
  echo "ANTHROPIC_API_KEY resolved empty (checked $SECRETS)" >&2
  exit 2
fi

set --
[ -n "$MODELS" ] && set -- "$@" --models "$MODELS"
[ -n "$EFFORT" ] && set -- "$@" --effort "$EFFORT"
[ -n "$REPEAT" ] && set -- "$@" --repeat "$REPEAT"
[ -n "$OUT" ]    && set -- "$@" --out "$OUT"

exec python3 "$HERE/probe.py" "$@"

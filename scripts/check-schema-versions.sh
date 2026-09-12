#!/usr/bin/env bash
# scripts/check-schema-versions.sh — the github-watcher consumes the
# runtime's events, so every envelope version the runtime writes must be one
# the watcher reads. The runtime declares its set once (fq-ops's
# `SUPPORTED_SCHEMA_VERSIONS`, over `SCHEMA_VERSION`); the watcher declares
# its own (`supportedSchemaVersions` in adapters/github-watcher/events.go)
# because the adapter reaches factor-q only through wire contracts and a Go
# binary cannot read a Rust const — that separation is the adapter's whole
# design. This gate is what keeps the hand-kept copy honest, exactly as
# `check-pins.sh` does for the toolchain pins.
#
# The check is subset, not equality: the watcher may accept versions the
# runtime no longer writes, because the four fields it decodes
# (invocation_id, trigger_payload, task_status, error_kind) were unchanged
# by the 2 → 3 bump and older events are still on the stream. What it may
# never do is refuse a version the runtime emits — which is what happened
# between #510 and #694: the runtime moved to 3, the watcher's hard-coded
# check stayed at 2, and every completion was skipped for eight days while
# issues sat at `status:in-progress`.
set -euo pipefail

root="${FQ_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$root"

events_rs=services/fq-runtime/crates/fq-ops/src/events.rs
wire_rs=services/fq-runtime/crates/fq-ops/src/events/wire.rs
watcher_go=adapters/github-watcher/events.go

# `SUPPORTED_SCHEMA_VERSIONS` is written in terms of `SCHEMA_VERSION`, so the
# current version is substituted before the set is compared.
current="$(sed -n 's/^pub const SCHEMA_VERSION: u32 = \([0-9]*\);.*/\1/p' "$events_rs")"
runtime="$(sed -n 's/^pub const SUPPORTED_SCHEMA_VERSIONS: &\[u32\] = &\[\(.*\)\];.*/\1/p' "$wire_rs" |
    tr ',' '\n' | sed "s/SCHEMA_VERSION/${current:-?}/" | tr -cd '0-9\n' | sort -u | grep -v '^$' || true)"
watcher="$(sed -n 's/^var supportedSchemaVersions = \[\]int{\(.*\)}.*/\1/p' "$watcher_go" |
    tr ',' '\n' | tr -cd '0-9\n' | sort -u | grep -v '^$' || true)"

if [ -z "$current" ] || [ -z "$runtime" ] || [ -z "$watcher" ]; then
    echo "error: a declared schema-version set could not be read — has a declaration been renamed or reshaped? ($events_rs, $wire_rs, $watcher_go)" >&2
    exit 1
fi

missing="$(comm -23 <(printf '%s\n' "$runtime") <(printf '%s\n' "$watcher") | tr '\n' ' ')"
fmt() { printf '%s' "$1" | tr '\n' ' ' | sed 's/ $//; s/ /, /g'; }

if [ -n "${missing// /}" ]; then
    printf '  FAIL schema runtime writes {%s}, watcher reads {%s}\n' "$(fmt "$runtime")" "$(fmt "$watcher")"
    printf '         ≠ %s refuses version(s) %s\n' "$watcher_go" "$(fmt "$missing")"
    echo "error: the github-watcher would skip every event the runtime writes at that version — add it to supportedSchemaVersions once the fields the watcher decodes are confirmed unchanged (scripts/check-schema-versions.sh)" >&2
    exit 1
fi

printf '  ok   schema {%s} %s, read by %s {%s}\n' "$(fmt "$runtime")" "$wire_rs" "$watcher_go" "$(fmt "$watcher")"

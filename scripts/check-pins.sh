#!/usr/bin/env bash
# scripts/check-pins.sh — one version, many places. The toolchains and the
# broker are pinned in the repository once each (rust-toolchain.toml, the
# adapters' go.mod, ci.yml's node-version, .nats-version) and repeated by
# hand where a file cannot read the pin: the container image's base
# images (services/fq-runtime/Dockerfile), every workflow's
# `dtolnay/rust-toolchain@<v>` step, and every compose file's
# `image: nats:<v>`. The audit tools are the same shape from the other
# end: the justfile pins them (`cargo_audit_version`,
# `cargo_deny_version`) and `just audit` refuses any other version, so
# the image's `tools`-stage ARGs are copies too. "Keep in lockstep"
# comments beside each copy asked for the discipline; this is the gate
# (`just check-pins`, part of `just quality`), so a bump that misses a
# copy fails CI instead of shipping an image built with a different
# compiler from the tarball's (#102: same commit, same binary).
#
# Not covered: the `just`, `gh` and `sccache` versions in the Dockerfile's
# `tools` stage — the repository has no other pin for them to agree with
# (CI installs the latest through their setup actions).
set -euo pipefail

root="${FQ_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
cd "$root"
dockerfile=services/fq-runtime/Dockerfile
fail=0

# one <name> <expected> <where-expected-comes-from> <file>:<found> ...
# Prints the pin and every copy; marks the copies that disagree.
check() {
    local name="$1" want="$2" source="$3"; shift 3
    local bad=0 line
    for found in "$@"; do
        [ "${found#*:}" = "$want" ] || bad=1
    done
    if [ "$bad" = 0 ]; then
        printf '  ok   %-6s %-10s %s' "$name" "$want" "$source"
        for found in "$@"; do printf ', %s' "${found%%:*}"; done
        printf '\n'
    else
        printf '  FAIL %-6s %-10s %s\n' "$name" "$want" "$source"
        for found in "$@"; do
            line="${found#*:}"
            if [ "$line" = "$want" ]; then printf '         = %s\n' "${found%%:*}"
            else printf '         ≠ %s says %s\n' "${found%%:*}" "${line:-<missing>}"; fi
        done
        fail=1
    fi
}

# --- rust: rust-toolchain.toml → the image's rust stage, every workflow ---------
rust="$(sed -n 's/^channel = "\(.*\)"$/\1/p' rust-toolchain.toml)"
copies=("$dockerfile:$(sed -n 's/^FROM rust:\([0-9.]*\)-.*/\1/p' "$dockerfile" | head -1)")
for wf in .github/workflows/*.yml; do
    while read -r v; do copies+=("$wf:$v"); done < <(grep -o 'dtolnay/rust-toolchain@[0-9.]*' "$wf" | sed 's/.*@//' | sort -u)
done
check rust "$rust" rust-toolchain.toml "${copies[@]}"

# --- go: the adapters' go.mod (agreeing with each other) → the image's go stage --
copies=()
gomods=(adapters/*/go.mod)
go="$(sed -n 's/^go \([0-9.]*\)$/\1/p' "${gomods[0]}")"
for m in "${gomods[@]:1}"; do copies+=("$m:$(sed -n 's/^go \([0-9.]*\)$/\1/p' "$m")"); done
copies+=("$dockerfile:$(sed -n 's/^FROM golang:\([0-9.]*\)-.*/\1/p' "$dockerfile" | head -1)")
check go "$go" "${gomods[0]}" "${copies[@]}"

# --- node: ci.yml's node-version (agreeing with itself) → the image's node stage --
copies=()
nodes="$(grep -o 'node-version: *"[0-9.]*"' .github/workflows/ci.yml | sed 's/.*"\(.*\)"/\1/')"
node="$(printf '%s\n' "$nodes" | head -1)"
i=0
while read -r v; do i=$((i + 1)); [ "$i" -gt 1 ] && copies+=(".github/workflows/ci.yml#$i:$v"); done <<< "$nodes"
copies+=("$dockerfile:$(sed -n 's/^FROM node:\([0-9.]*\)-.*/\1/p' "$dockerfile" | head -1)")
check node "$node" ".github/workflows/ci.yml" "${copies[@]}"

# --- nats: .nats-version → every compose file's broker image --------------------
# (The Dockerfile's nats-server reads .nats-version itself, so it is not a copy.)
nats="$(tr -d '[:space:]' < .nats-version)"
copies=()
while IFS= read -r hit; do
    copies+=("${hit%%:*}:$(printf '%s' "${hit#*:}" | sed 's/.*image: *nats://; s/-.*//; s/ *$//')")
done < <(git grep -E '^\s*image:\s*nats:[0-9]' -- '*.yml' '*.yaml')
check nats "$nats" .nats-version "${copies[@]}"

# --- audit tools: the justfile's pins → the image's `tools` stage ---------------
# `just audit` compares what is on PATH against these and fails on anything
# else, and three fleet agents run `just ci` — audit phase and all — inside
# the dogfood image. A drift here is not a subtle divergence: it is `just ci`
# unable to get past its first phase in the container.
for t in audit deny; do
    T="$(printf '%s' "$t" | tr '[:lower:]' '[:upper:]')"
    want="$(sed -n "s/^cargo_${t}_version := \"\(.*\)\"\$/\1/p" justfile)"
    found="$(sed -n "s/^ARG CARGO_${T}_VERSION=\(.*\)\$/\1/p" "$dockerfile" | head -1)"
    check "$t" "$want" justfile "$dockerfile:$found"
done

if [ "$fail" != 0 ]; then
    echo "error: a pinned version has a copy that disagrees with it — bump every copy together (scripts/check-pins.sh)" >&2
    exit 1
fi

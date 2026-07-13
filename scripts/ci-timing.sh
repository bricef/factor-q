# ci-timing.sh — a small, sourceable wall-clock timing framework for shell
# pipelines that run a sequence of phases and want an aligned per-phase summary
# printed at the end (on success and on failure alike).
#
# Usage (bash, from a `just` recipe or any script):
#
#     set -uo pipefail
#     source /path/to/ci-timing.sh
#     # -- optional project state + teardown hook --
#     ci_cleanup() { ... }              # OPTIONAL: project teardown; the EXIT
#                                       #   trap runs it BEFORE the summary, so
#                                       #   it fires on success and failure both
#     ci_timing_init                    # start the clock, install the EXIT trap
#     run_phase "lint-docs" just lint-docs
#     run_phase "compile"   my_compile_fn
#     ...
#
# `run_phase` times one command, records "<label> -> <duration>", and stops the
# run on the first failure (fail-fast), propagating that command's exit code;
# the EXIT trap prints the summary either way. A phase that is not a single
# command (its own probe/branch logic) can time itself with `phase_header`,
# `record` and `human`, and fail-fast by setting `failed_label` then `exit`.
#
# This file is sourced, never executed — no execute bit needed.

# -- timing state --
declare -a T_LABEL=() T_VAL=()
t_ci_start=0
failed_label=""
phase_no=0

# Append a finished phase (label, preformatted duration string) to the table.
record() { T_LABEL+=("$1"); T_VAL+=("$2"); }

# Emit <n> copies of <ch>. Used for dot leaders and rules; avoids `tr`, which is
# byte-oriented and cannot repeat a multibyte box-drawing character.
repeat() { local n=$1 ch=$2 out= i; for (( i=0; i<n; i++ )); do out="$out$ch"; done; printf '%s' "$out"; }

# Format a whole number of seconds as e.g. "6s", "1m 56s", "1h 2m 3s".
human() {
    local s=$1
    if   [ "$s" -ge 3600 ]; then printf '%dh %dm %ds' $((s/3600)) $(((s%3600)/60)) $((s%60))
    elif [ "$s" -ge 60   ]; then printf '%dm %ds' $((s/60)) $((s%60))
    else                         printf '%ds' "$s"; fi
}

# Print the "==> [N] <label>" progress header and bump the phase counter.
phase_header() {
    phase_no=$((phase_no + 1))
    printf '\n==> [%d] %s\n' "$phase_no" "$1"
}

# Print the aligned timing summary. Called from the EXIT trap.
print_summary() {
    local total=$(( $(date +%s) - t_ci_start )) w=5 i n
    if [ "${#T_LABEL[@]}" -gt 0 ]; then
        for i in "${T_LABEL[@]}"; do [ "${#i}" -gt "$w" ] && w=${#i}; done
    fi
    printf '\n── CI timing summary %s\n' "$(repeat $((w + 2)) '─')"
    n=${#T_LABEL[@]}
    for (( i=0; i<n; i++ )); do
        printf '  %s %s %s\n' "${T_LABEL[$i]}" "$(repeat $(( w - ${#T_LABEL[$i]} + 3 )) '.')" "${T_VAL[$i]}"
    done
    printf '  %s\n' "$(repeat $((w + 5)) '─')"
    printf '  TOTAL %s %s\n' "$(repeat $(( w - 5 + 3 )) '.')" "$(human "$total")"
    [ -n "$failed_label" ] && printf '\n  x FAILED at phase: %s (exit non-zero)\n' "$failed_label"
    return 0
}

# EXIT trap: run the optional project cleanup hook (so teardown happens on
# success and failure alike), then print the summary, preserving the exit code.
_ci_on_exit() {
    local rc=$?
    declare -F ci_cleanup >/dev/null && ci_cleanup
    print_summary
    exit "$rc"
}

# Start the clock and arm the EXIT trap. Call once, after defining ci_cleanup.
ci_timing_init() {
    t_ci_start=$(date +%s)
    trap _ci_on_exit EXIT
}

# Time a single phase: print its header, run "$@", record the duration, and on
# failure set failed_label and exit with the phase's code (fail-fast — the EXIT
# trap then prints the summary gathered so far).
run_phase() {
    local label="$1"; shift
    phase_header "$label"
    local t0=$(date +%s) rc=0
    "$@" || rc=$?
    record "$label" "$(human $(( $(date +%s) - t0 )))"
    if [ "$rc" -ne 0 ]; then failed_label="$label"; exit "$rc"; fi
}

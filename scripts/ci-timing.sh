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
# `run_phase` times one command, records it, and stops the run on the first
# failure (fail-fast), propagating that command's exit code; the EXIT trap
# prints the summary either way. A phase that is not a single command (its own
# probe/branch logic) can time itself with `start_phase` / `end_phase`, and
# fail-fast by setting `failed_label` then `exit`.
#
# ## The store: an append-only event log (#223)
#
# Each `just` recipe body is its own shell, and a phase is often itself a `just`
# target that runs phases of its own:
#
#     just ci  ->  just runtime-ci  ->  (cd services/fq-runtime && just ci)
#
# In-process state cannot cross that boundary, so phases are appended to a log
# at the repo root ($FQ_CI_TIMINGS, default .ci-timings) as two events:
#
#     <epoch> <TAB> started <TAB> <label>
#     <epoch> <TAB> ended   <TAB> <label>
#
# Ephemeral: the outermost run truncates it at the start. It is deliberately
# left behind afterwards — a killed run's log is the only record of where the
# time went, so it stays inspectable. .gitignore covers it.
#
# FQ_CI_TIMINGS defaults to .ci-timings resolved against the caller's cwd, which
# is fine for a one-off script but makes the location depend on where you were
# standing. A caller with a stable anchor should set it explicitly instead — the
# factor-q justfiles pass {{justfile_directory()}}, so every gate logs to the
# repo root whichever one you enter through. Either way the outermost run
# absolutises before exporting, so children inherit a resolved path rather than
# re-resolving a relative one against their own cwd (a nested gate's cwd is its
# service directory, which would silently split the log in two).
#
# Two properties fall out of the format, and are why it is a log of events
# rather than a table of finished phases:
#
#   - Nesting is free. Read top to bottom, `started` pushes and `ended` pops:
#     the log is a linearised call tree, so a phase's depth is just the stack
#     depth when it opens. No process needs to know or announce its parent.
#     Ordering is causal across processes — a parent writes `started` before it
#     spawns the child and `ended` after the child exits — and appends of short
#     lines are atomic (O_APPEND), so the interleaving cannot corrupt. This
#     assumes phases do not run *concurrently*; parallelising them would need a
#     real span id instead.
#   - An interrupted phase stays visible. `started` with no `ended` renders as
#     "(did not finish)" rather than vanishing, which is what a table of
#     completed phases would do to a SIGKILLed run.
#
# ## Who prints
#
# The log cannot tell a nested gate from a standalone one — the innermost
# process runs the same recipe either way — so FQ_CI_NESTED marks the boundary:
#
#   - The outermost `ci_timing_init` truncates the log, owns it, and is the only
#     process that prints a summary.
#   - Nested runs inherit FQ_CI_NESTED=1, append, and print nothing (otherwise
#     every nested gate would dump a summary mid-run).
#
# A suite gate run on its own (`just runtime-ci`, which is how CI invokes it)
# has nothing to inherit, so it becomes the owner and prints its own phase
# summary. That is deliberate: CI jobs get a phase breakdown for free.
#
# This file is sourced, never executed — no execute bit needed.

# -- timing state --
# Per-process: the progress counter, whether this process owns the log, and the
# label of a failed phase. The phases themselves live in the log (see above),
# not in a shell array, because they must outlive the process that ran them.
failed_label=""
phase_no=0
_ci_timings_owner=0

: "${FQ_CI_TIMINGS:=.ci-timings}"

# Append one event: <epoch> <kind> <label> [note]. `started`/`ended` pairs are
# matched by stack depth when the log is read back.
_ci_event() {
    printf '%s\t%s\t%s\t%s\n' "$(date +%s)" "$1" "$2" "${3:-}" >>"${FQ_CI_TIMINGS}"
}

# Open a phase: print its progress header and log its start.
start_phase() {
    phase_no=$((phase_no + 1))
    printf '\n==> [%d] %s\n' "$phase_no" "$1"
    _ci_event started "$1"
}

# Close a phase. An optional note annotates the duration in the summary — for
# phases whose elapsed time alone would mislead, e.g. "already warm".
end_phase() { _ci_event ended "$1" "${2:-}"; }

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

# Walk the event log, pairing started/ended via a stack, and emit one
# "<depth> <TAB> <label> <TAB> <duration>" row per phase in the order phases
# opened. An unclosed phase gets an empty duration.
_ci_rows() {
    awk -F'\t' '
        $2 == "started" {
            d = top++            # depth = stack depth when this phase opened
            idx[d] = ++n         # remember which row this stack slot is filling
            row_depth[n] = d; row_label[n] = $3; row_start[n] = $1
            row_end[n] = ""; row_note[n] = ""
            next
        }
        $2 == "ended" && top > 0 {
            r = idx[--top]       # close the innermost open phase
            row_end[r] = $1; row_note[r] = $4
            next
        }
        END {
            for (i = 1; i <= n; i++) {
                dur = (row_end[i] == "") ? "" : row_end[i] - row_start[i]
                printf "%d\t%s\t%s\t%s\n", row_depth[i], row_label[i], dur, row_note[i]
            }
        }
    ' "${FQ_CI_TIMINGS}"
}

# The phase that actually broke. `run_phase` logs a `failed` event as the
# failure unwinds, and the innermost phase fails first — so the first event in
# the log is the useful one. A parent phase "failing" is only its child's exit
# code bubbling up, and naming the parent sends the reader to the wrong place.
_ci_failed_label() {
    awk -F'\t' '$2 == "failed" { print $3; exit }' "${FQ_CI_TIMINGS}"
}

# Print the aligned timing summary from the event log: every phase in the order
# it opened, indented by nesting depth. Called from the EXIT trap of the process
# that owns the log.
print_summary() {
    local w=5 i n total=0
    local -a D=() L=() V=()
    local depth label dur note val
    while IFS=$'\t' read -r depth label dur note; do
        [ -n "$label" ] || continue
        if [ -z "$dur" ]; then
            val="(did not finish)"
        else
            val="$(human "$dur")"
            [ -n "$note" ] && val="$val ($note)"
        fi
        D+=("$depth"); L+=("$label"); V+=("$val")
        # Top-level phases partition the run, so they alone sum to the total.
        [ "$depth" = "0" ] && [ -n "$dur" ] && total=$(( total + dur ))
    done < <(_ci_rows)
    n=${#L[@]}
    for (( i=0; i<n; i++ )); do
        local len=$(( ${#L[$i]} + 2 * D[i] ))
        [ "$len" -gt "$w" ] && w=$len
    done
    printf '\n── CI timing summary %s\n' "$(repeat $((w + 2)) '─')"
    for (( i=0; i<n; i++ )); do
        local indent=$(( 2 + 2 * D[i] ))
        printf '%s%s %s %s\n' \
            "$(repeat "$indent" ' ')" \
            "${L[$i]}" \
            "$(repeat $(( w - ${#L[$i]} - 2 * D[i] + 3 )) '.')" \
            "${V[$i]}"
    done
    printf '  %s\n' "$(repeat $((w + 5)) '─')"
    printf '  TOTAL %s %s\n' "$(repeat $(( w - 5 + 3 )) '.')" "$(human "$total")"
    local failed
    failed="$(_ci_failed_label)"
    [ -n "$failed" ] || failed="$failed_label"
    [ -n "$failed" ] && printf '\n  x FAILED at phase: %s (exit non-zero)\n' "$failed"
    return 0
}

# EXIT trap: run the optional project cleanup hook (so teardown happens on
# success and failure alike), then — only in the process that owns the log —
# print the summary, preserving the exit code.
_ci_on_exit() {
    local rc=$?
    declare -F ci_cleanup >/dev/null && ci_cleanup
    [ "$_ci_timings_owner" = "1" ] && print_summary
    exit "$rc"
}

# Start the clock and arm the EXIT trap. Call once, after defining ci_cleanup.
# Truncates and owns the log unless a parent run is already writing one.
ci_timing_init() {
    if [ -z "${FQ_CI_NESTED:-}" ]; then
        # Absolute, because nested gates run from their own directory
        # (`runtime-ci` is `cd services/fq-runtime && just ci`) — a relative
        # path would silently split the log across two files.
        case "${FQ_CI_TIMINGS}" in /*) ;; *) FQ_CI_TIMINGS="${PWD}/${FQ_CI_TIMINGS}" ;; esac
        : >"${FQ_CI_TIMINGS}"
        export FQ_CI_NESTED=1
        export FQ_CI_TIMINGS
        _ci_timings_owner=1
    fi
    trap _ci_on_exit EXIT
}

# Time a single phase: print its header, run "$@", log the duration, and on
# failure set failed_label and exit with the phase's code (fail-fast — the EXIT
# trap then prints the summary gathered so far).
run_phase() {
    local label="$1"; shift
    start_phase "$label"
    local rc=0
    "$@" || rc=$?
    end_phase "$label"
    if [ "$rc" -ne 0 ]; then
        failed_label="$label"
        _ci_event failed "$label"
        exit "$rc"
    fi
}

#!/usr/bin/env bash
# Shared control harness for the FUSE VFS bake-off.
# Spec: docs/plans/active/2026-07-09-fuse-vfs-bakeoff.md
#
# Usage:
#   run.sh <impl-binary>                 # run the default ladder against an impl + baseline
#   run.sh <impl-binary> --rungs a,b     # only these rungs
#   run.sh <impl-binary> --heavy         # add the factor-q build rung (slow, needs network)
#   run.sh --baseline-only               # run rungs against a real FS only (validates the harness)
#
# The harness is external and identical for every implementation: it drives the
# mounted filesystem from the outside and never reveals how to implement it.
# Each rung is self-asserting — it performs real operations and checks their
# outcomes, so "passed in the FUSE mount" means the VFS behaved correctly.
set -u

DEFAULT_RUNGS="smoke many_small large_file git cargo"
HEAVY_RUNGS="just_ci"

IMPL=""
BASELINE_ONLY=0
HEAVY=0
RUNGS=""

while [ $# -gt 0 ]; do
  case "$1" in
    --baseline-only) BASELINE_ONLY=1 ;;
    --heavy) HEAVY=1 ;;
    --rungs) shift; RUNGS="${1//,/ }" ;;
    -h|--help) grep '^#' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) IMPL="$1" ;;
  esac
  shift
done

[ -z "$RUNGS" ] && RUNGS="$DEFAULT_RUNGS"
[ "$HEAVY" = 1 ] && RUNGS="$RUNGS $HEAVY_RUNGS"

if [ "$BASELINE_ONLY" != 1 ]; then
  [ -n "$IMPL" ] || { echo "error: need an impl binary (or --baseline-only)"; exit 2; }
  [ -x "$IMPL" ] || { echo "error: impl '$IMPL' is not an executable"; exit 2; }
fi

for t in git cargo sha256sum grep find mktemp awk fusermount3; do
  command -v "$t" >/dev/null || echo "warning: '$t' not on PATH — some rungs will fail"
done

# ---- rungs: each takes a target dir, performs real ops, self-asserts -------

rung_smoke() {
  local d="$1"
  mkdir -p "$d/sub/deep" || return 1
  printf 'hello world\n' > "$d/sub/a.txt" || return 1
  [ "$(cat "$d/sub/a.txt")" = "hello world" ] || return 1
  [ -d "$d/sub/deep" ] || return 1
  ls "$d/sub" >/dev/null || return 1
  stat "$d/sub/a.txt" >/dev/null || return 1
  mv "$d/sub/a.txt" "$d/sub/b.txt" || return 1
  { [ -f "$d/sub/b.txt" ] && [ ! -e "$d/sub/a.txt" ]; } || return 1
  # append (offset write) then verify
  printf 'second line\n' >> "$d/sub/b.txt" || return 1
  [ "$(wc -l < "$d/sub/b.txt")" -eq 2 ] || return 1
  rm "$d/sub/b.txt" || return 1
  rmdir "$d/sub/deep" || return 1
  [ ! -e "$d/sub/b.txt" ] || return 1
}

rung_many_small() {
  local d="$1" i
  for i in $(seq 1 500); do
    mkdir -p "$d/m/$((i % 10))" || return 1
    printf 'content %d marker\n' "$i" > "$d/m/$((i % 10))/f$i.txt" || return 1
  done
  [ "$(find "$d/m" -type f | wc -l)" -eq 500 ] || return 1
  [ "$(grep -rl marker "$d/m" | wc -l)" -eq 500 ] || return 1
  for i in $(seq 1 2 500); do rm "$d/m/$((i % 10))/f$i.txt" || return 1; done
  [ "$(find "$d/m" -type f | wc -l)" -eq 250 ] || return 1
}

rung_large_file() {
  local d="$1" h1 h2
  dd if=/dev/urandom of="$d/big.bin" bs=1M count=64 status=none || return 1
  h1=$(sha256sum "$d/big.bin" | cut -d' ' -f1)
  h2=$(sha256sum < "$d/big.bin" | cut -d' ' -f1)   # read the whole file back
  [ -n "$h1" ] && [ "$h1" = "$h2" ] || return 1
  rm "$d/big.bin" || return 1
}

rung_git() {
  local d="$1"
  ( cd "$d" || exit 1
    export GIT_AUTHOR_NAME=bake GIT_AUTHOR_EMAIL=bake@example.com \
           GIT_COMMITTER_NAME=bake GIT_COMMITTER_EMAIL=bake@example.com \
           GIT_AUTHOR_DATE="2026-01-01T00:00:00Z" GIT_COMMITTER_DATE="2026-01-01T00:00:00Z"
    git init -q . || exit 1
    mkdir -p src
    printf 'fn main() { println!("hi"); }\n' > src/main.rs
    printf '# proj\n' > README.md
    git add -A || exit 1
    git commit -q -m "initial" || exit 1
    [ -z "$(git status --porcelain)" ] || exit 1
    git rev-parse HEAD >/dev/null || exit 1
    git log --oneline | grep -q initial || exit 1
  )
}

rung_cargo() {
  local d="$1"
  mkdir -p "$d/proj/src" || return 1
  cat > "$d/proj/Cargo.toml" <<'EOF'
[package]
name = "bake"
version = "0.0.0"
edition = "2021"

[[bin]]
name = "bake"
path = "src/main.rs"
EOF
  printf 'fn main() { println!("built through the vfs"); }\n' > "$d/proj/src/main.rs"
  ( cd "$d/proj" && cargo build -q --offline 2>/dev/null || cargo build -q ) || return 1
  [ -x "$d/proj/target/debug/bake" ] || return 1
  "$d/proj/target/debug/bake" | grep -q "through the vfs" || return 1
}

rung_just_ci() {
  # Heavy realistic top rung: build a factor-q workspace inside the VFS.
  local d="$1" repo="${FACTORQ_REPO:-$HOME/Code/github.com/bricef/factor-q}"
  [ -d "$repo/services/fq-runtime" ] || { echo "  (no factor-q repo at $repo — set FACTORQ_REPO)"; return 2; }
  git -C "$repo" archive --format=tar HEAD services/fq-runtime | tar -x -C "$d" || return 1
  ( cd "$d/services/fq-runtime" && cargo build -q -p fq-runtime ) || return 1
}

# ---- driver ----------------------------------------------------------------

FUSE_TIME=0
BASE_TIME=0

run_base() {   # $1 rung  -> sets BASE_TIME, returns rung rc
  local rung="$1" base start end rc
  base=$(mktemp -d)
  start=$(date +%s.%N)
  "rung_$rung" "$base"; rc=$?
  end=$(date +%s.%N)
  rm -rf "$base"
  BASE_TIME=$(awk "BEGIN{printf \"%.2f\", $end-$start}")
  return $rc
}

run_fuse() {   # $1 rung, $2 impl -> sets FUSE_TIME, returns rung rc (or 1 on mount fail)
  local rung="$1" impl="$2" mnt pid t start end rc
  mnt=$(mktemp -d)
  "$impl" "$mnt" >/tmp/fvfs-impl.log 2>&1 &
  pid=$!
  t=0
  until mount 2>/dev/null | grep -qF "$mnt type"; do
    sleep 0.1; t=$((t + 1))
    if ! kill -0 "$pid" 2>/dev/null; then
      echo "  impl exited before mounting (see /tmp/fvfs-impl.log)"; rmdir "$mnt" 2>/dev/null; FUSE_TIME=0; return 1
    fi
    if [ "$t" -gt 100 ]; then
      echo "  mount timed out"; kill "$pid" 2>/dev/null; rmdir "$mnt" 2>/dev/null; FUSE_TIME=0; return 1
    fi
  done
  start=$(date +%s.%N)
  "rung_$rung" "$mnt"; rc=$?
  end=$(date +%s.%N)
  fusermount3 -u "$mnt" 2>/dev/null || fusermount -u "$mnt" 2>/dev/null
  kill "$pid" 2>/dev/null; wait "$pid" 2>/dev/null
  rmdir "$mnt" 2>/dev/null
  FUSE_TIME=$(awk "BEGIN{printf \"%.2f\", $end-$start}")
  return $rc
}

verdict() { case "$1" in 0) echo ok ;; 2) echo skip ;; *) echo FAIL ;; esac; }

echo "=== FUSE VFS bake-off harness ==="
[ "$BASELINE_ONLY" = 1 ] && echo "mode: baseline-only (real FS)" || echo "impl: $IMPL"
echo "rungs: $RUNGS"
echo ""
printf '%-12s %-6s %-8s %-6s %-8s %-6s\n' rung fuse impl_s base base_s ratio
printf '%-12s %-6s %-8s %-6s %-8s %-6s\n' ------------ ------ -------- ------ -------- ------

fails=0
for rung in $RUNGS; do
  run_base "$rung"; brc=$?
  if [ "$BASELINE_ONLY" = 1 ]; then
    printf '%-12s %-6s %-8s %-6s %-8s %-6s\n' "$rung" "-" "-" "$(verdict $brc)" "$BASE_TIME" "-"
    [ "$brc" = 0 ] || [ "$brc" = 2 ] || fails=$((fails + 1))
    continue
  fi
  run_fuse "$rung" "$IMPL"; frc=$?
  ratio=$(awk "BEGIN{ if ($BASE_TIME>0) printf \"%.1fx\", $FUSE_TIME/$BASE_TIME; else printf \"n/a\" }")
  printf '%-12s %-6s %-8s %-6s %-8s %-6s\n' "$rung" "$(verdict $frc)" "$FUSE_TIME" "$(verdict $brc)" "$BASE_TIME" "$ratio"
  [ "$frc" = 0 ] || [ "$frc" = 2 ] || fails=$((fails + 1))
done

echo ""
if [ "$fails" = 0 ]; then echo "RESULT: all rungs passed"; exit 0
else echo "RESULT: $fails rung(s) failed"; exit 1; fi

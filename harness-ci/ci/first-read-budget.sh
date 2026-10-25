#!/usr/bin/env bash
# Enforce PLAN.md section 6.2's first-read budget: a 500 MB synthetic Claude Code transcript
# must reach state in under 3 s in Lean mode, with peak RSS under 50 MB.
#
# Usage: harness-ci/ci/first-read-budget.sh [MB]
#
# Build the bench, then run the emitted executable directly under GNU /usr/bin/time. Wrapping
# `cargo bench` would report cargo's peak RSS, not the bench child's.
set -euo pipefail
export LC_ALL=C

lean_budget_seconds=3
rss_budget_kib=$((50 * 1024))

mb="${1:-500}"
case "$mb" in
  ''|*[!0-9]*)
    echo "usage: $0 [MB]" >&2
    exit 2
    ;;
esac
mb=$((10#$mb))
if ((mb < 1)); then
  echo "usage: $0 [MB] (MB must be positive)" >&2
  exit 2
fi

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"

time_bin=/usr/bin/time
if [[ ! -x "$time_bin" ]] || ! "$time_bin" -v true >/dev/null 2>&1; then
  echo "first-read budget: GNU /usr/bin/time -v is required (Linux; on macOS use gtime)" >&2
  exit 2
fi

mkdir -p "$root/target"
work="$(mktemp -d "$root/target/first-read-budget.XXXXXX")"
trap 'rm -rf "$work"' EXIT

build_log="$work/build.log"
bench_log="$work/bench.log"
time_log="$work/time.log"

echo "first-read budget: $(rustc --version), $(cargo --version)"
if ! cargo bench -p agenttail --bench first_read --no-run --locked >"$build_log" 2>&1; then
  cat "$build_log" >&2
  echo "first-read budget: building the bench failed" >&2
  exit 1
fi

# Cargo's human output ends with: Executable benches/first_read.rs (target/release/deps/...)
bench_bin="$(sed -nE 's/^[[:space:]]*Executable .* \(([^)]*)\)[[:space:]]*$/\1/p' "$build_log" | tail -n 1)"
if [[ -z "$bench_bin" || ! -x "$bench_bin" ]]; then
  cat "$build_log" >&2
  echo "first-read budget: could not find the built bench executable in cargo output" >&2
  exit 1
fi

set +e
"$time_bin" -v -o "$time_log" "$bench_bin" "$mb" >"$bench_log" 2>&1
status=$?
set -e
cat "$bench_log"
if ((status != 0)); then
  cat "$time_log" >&2
  echo "first-read budget: the bench exited $status" >&2
  exit "$status"
fi

lean="$(sed -nE 's/^Lean:.* in ([0-9]+([.][0-9]+)?) s = .*/\1/p' "$bench_log" | tail -n 1)"
full="$(sed -nE 's/^Full:.* in ([0-9]+([.][0-9]+)?) s = .*/\1/p' "$bench_log" | tail -n 1)"
rss_kib="$(sed -nE 's/^[[:space:]]*Maximum resident set size \(kbytes\):[[:space:]]*([0-9]+)$/\1/p' "$time_log" | tail -n 1)"
if [[ -z "$lean" || -z "$full" || -z "$rss_kib" ]]; then
  cat "$time_log" >&2
  echo "first-read budget: could not read Lean/Full time or peak RSS from the output" >&2
  exit 1
fi

if ! awk \
  -v lean="$lean" \
  -v full="$full" \
  -v rss_kib="$rss_kib" \
  -v lean_budget="$lean_budget_seconds" \
  -v rss_budget="$rss_budget_kib" '
  BEGIN {
    rss_mb = rss_kib / 1024
    printf "first-read budget: Lean %.2f s (budget %s s), Full %.2f s (reported, not budgeted), peak RSS %.1f MB (budget %d MB)\n", lean, lean_budget, full, rss_mb, rss_budget / 1024
    bad = 0
    if (lean + 0 >= lean_budget + 0) {
      printf "first-read budget: FAIL: Lean %.2f s exceeds %s s\n", lean, lean_budget > "/dev/stderr"
      bad = 1
    }
    if (rss_kib + 0 >= rss_budget + 0) {
      printf "first-read budget: FAIL: peak RSS %.1f MB exceeds %d MB\n", rss_mb, rss_budget / 1024 > "/dev/stderr"
      bad = 1
    }
    exit bad
  }
'; then
  exit 1
fi

echo "first-read budget: OK"

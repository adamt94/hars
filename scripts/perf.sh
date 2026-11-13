#!/usr/bin/env bash
# Measure hars against the MVP resource budgets (docs/ROADMAP.md, docs/PERF.md).
#
# Linux only: it reads /proc. On macOS use scripts/perf-docker.sh.
#
#   scripts/perf.sh [binary] [warmup_s] [quiet_s]
#
# The MVP gate is a 300 s warm-up and a 1800 s quiet run; the defaults are
# shorter for day-to-day checks. Prints one Markdown table row.
set -euo pipefail

BIN=${1:-target/release/hars}
WARMUP=${2:-10}
QUIET=${3:-60}
PORT=${PORT:-18123}
URL="http://127.0.0.1:$PORT"

[ -r /proc/self/status ] || { echo "perf.sh needs Linux /proc" >&2; exit 1; }

# Sum of voluntary + involuntary context switches over every thread: each
# one is the process waking up (or being preempted).
switches() {
  cat /proc/"$1"/task/*/status 2>/dev/null |
    awk '/^(non)?voluntary_ctxt_switches/ { n += $2 } END { print n }'
}
cpu_ticks() { awk '{ print $14 + $15 }' /proc/"$1"/stat; }  # utime + stime

size=$(stat -c %s "$BIN")
t0=$(date +%s%N)
HARS_BIND="127.0.0.1:$PORT" "$BIN" >/dev/null 2>&1 &
pid=$!
trap 'kill "$pid" 2>/dev/null || true' EXIT
until curl -sf "$URL/api/" >/dev/null 2>&1; do
  kill -0 "$pid" 2>/dev/null || { echo "hars exited during start-up" >&2; exit 1; }
  sleep 0.002
done
start_ms=$(( ($(date +%s%N) - t0) / 1000000 ))

# Warm-up: a little traffic, so idle numbers include touched allocator pages.
for i in $(seq 1 200); do
  curl -sf -X POST "$URL/api/states/sensor.perf_$((i % 20))" \
    -H 'content-type: application/json' \
    -d "{\"state\": \"$i\", \"attributes\": {\"unit_of_measurement\": \"W\"}}" >/dev/null
done
curl -sf "$URL/api/states" >/dev/null
sleep "$WARMUP"

hz=$(getconf CLK_TCK)
ticks0=$(cpu_ticks "$pid"); sw0=$(switches "$pid")
sleep "$QUIET"
ticks1=$(cpu_ticks "$pid"); sw1=$(switches "$pid")

rss_kib=$(awk '/^VmRSS/ { print $2 }' /proc/"$pid"/status)
pss_kib=$(awk '/^Pss:/ { print $2 }' /proc/"$pid"/smaps_rollup)
threads=$(ls /proc/"$pid"/task | wc -l)

awk -v size="$size" -v start="$start_ms" -v rss="$rss_kib" -v pss="$pss_kib" \
    -v dt=$((ticks1 - ticks0)) -v hz="$hz" -v sw=$((sw1 - sw0)) -v q="$QUIET" \
    -v threads="$threads" -v w="$WARMUP" 'BEGIN {
  printf "| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |\n"
  printf "|---|---|---|---|---|---|---|---|\n"
  printf "| %.2f MiB | %d ms | %.1f MiB | %.1f MiB | %.3f %% | %.2f | %d | %ds warm-up, %ds quiet |\n",
    size / 1048576, start, rss / 1024, pss / 1024, 100 * dt / hz / q, sw / q, threads, w, q
}'

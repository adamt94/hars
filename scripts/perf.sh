#!/usr/bin/env bash
# Measure hars against the resource budgets in docs/PERF.md.
#
# Linux only: it reads /proc. On macOS use scripts/perf-docker.sh.
#
#   scripts/perf.sh [binary] [warmup_s] [quiet_s]
#
# The MVP gate is a 300 s warm-up and a 1800 s quiet run; the defaults are
# shorter for day-to-day checks. The warm-up must be longer than tokio's 10 s
# blocking-thread keep-alive (the login's bcrypt runs on one), or that thread
# exits during the quiet window. Prints one Markdown table row.
#
# The run uses a throwaway config dir with one login. PERF_CONFIG=DIR starts
# from a copy of DIR instead (an imported install, so its integrations run).
#
# PERF_GATE=1 also fails (exit 3) when a number is over its budget, as CI
# does. The budgets are the defaults below; docs/PERF.md#budgets lists them.
# Each can be overridden (MAX_BINARY_MIB=8 ...), e.g. to check that the gate
# trips.
set -euo pipefail

BIN=${1:-target/release/hars}
WARMUP=${2:-15}
QUIET=${3:-60}
PORT=${PORT:-18123}
URL="http://127.0.0.1:$PORT"

[ -r /proc/self/status ] || { echo "perf.sh needs Linux /proc" >&2; exit 1; }

# Wakeup proxy: voluntary context switches summed over every thread. Each
# one is a thread blocking, so each is followed by a wakeup. Preemptions
# (involuntary switches) are not wakeups and are left out. Threads that exit
# during the window take their counts with them, so the thread set is
# checked to be unchanged.
switches() {
  cat /proc/"$1"/task/*/status 2>/dev/null |
    awk '/^voluntary_ctxt_switches/ { n += $2 } END { print n }'
}
tasks() { ls /proc/"$1"/task | sort | tr '\n' ' '; }
cpu_ticks() { awk '{ print $14 + $15 }' /proc/"$1"/stat; }  # utime + stime

size=$(stat -c %s "$BIN")
if curl -s --max-time 1 "$URL/api/" >/dev/null 2>&1; then
  echo "something is already listening on $URL; set PORT" >&2
  exit 1
fi
# A throwaway config with one login; /api/ needs a token, as in HA.
export HARS_CONFIG
HARS_CONFIG=$(mktemp -d)
if [ -n "${PERF_CONFIG:-}" ]; then
  cp -a "$PERF_CONFIG"/. "$HARS_CONFIG"/
fi
"$BIN" auth add perf perf-password >/dev/null
# PERF_MET=1: a home in London with HA's onboarding `met` entry, so the
# forecast is fetched (from api.met.no) and held during the run.
if [ "${PERF_MET:-0}" = 1 ]; then
  cat >"$HARS_CONFIG/.storage/core.config" <<'JSON'
{"version": 1, "minor_version": 4, "key": "core.config", "data": {"latitude": 51.5072, "longitude": -0.1276, "elevation": 11, "unit_system_v2": "metric", "location_name": "Home", "time_zone": "Europe/London", "external_url": null, "internal_url": null, "currency": "GBP", "country": "GB", "language": "en", "radius": 100}}
JSON
  cat >"$HARS_CONFIG/.storage/core.config_entries" <<'JSON'
{"version": 1, "minor_version": 5, "key": "core.config_entries", "data": {"entries": [{"entry_id": "perfmet", "domain": "met", "title": "Home", "data": {"track_home": true}, "options": {}, "source": "onboarding", "disabled_by": null}]}}
JSON
fi
answers() { [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 1 "$URL/api/")" != 000 ]; }
t0=$(date +%s%N)
HARS_BIND="127.0.0.1:$PORT" "$BIN" >/dev/null 2>&1 &
pid=$!
# Wait for hars to stop (it saves on SIGTERM) before removing its config.
trap '{ kill "$pid" && wait "$pid"; } 2>/dev/null || true; rm -rf "$HARS_CONFIG" || true' EXIT
until answers; do
  kill -0 "$pid" 2>/dev/null || { echo "hars exited during start-up" >&2; exit 1; }
  if (( $(date +%s%N) - t0 > 10000000000 )); then
    echo "hars did not answer /api/ within 10 s" >&2; exit 1
  fi
  sleep 0.002
done
start_ms=$(( ($(date +%s%N) - t0) / 1000000 ))
# The answer must come from our child, not another process on the port.
kill -0 "$pid" 2>/dev/null || { echo "hars exited during start-up" >&2; exit 1; }

# Warm-up: a login and a little traffic, so idle numbers include touched
# allocator pages.
token=$("$(dirname "$0")/hars-token.sh" "$URL" perf perf-password)
for i in $(seq 1 200); do
  curl -sf --max-time 5 -X POST "$URL/api/states/sensor.perf_$((i % 20))" \
    -H "authorization: Bearer $token" -H 'content-type: application/json' \
    -d "{\"state\": \"$i\", \"attributes\": {\"unit_of_measurement\": \"W\"}}" >/dev/null
done
curl -sf --max-time 5 -H "authorization: Bearer $token" "$URL/api/states" >/dev/null
sleep "$WARMUP"
if [ "${PERF_MET:-0}" = 1 ]; then
  curl -sf --max-time 5 -H "authorization: Bearer $token" "$URL/api/states/weather.forecast_home" |
    grep -q '"attribution"' || { echo "weather.forecast_home is missing" >&2; exit 1; }
fi

hz=$(getconf CLK_TCK)
ticks0=$(cpu_ticks "$pid"); sw0=$(switches "$pid"); tasks0=$(tasks "$pid")
sleep "$QUIET"
ticks1=$(cpu_ticks "$pid"); sw1=$(switches "$pid"); tasks1=$(tasks "$pid")
if [ "$tasks0" != "$tasks1" ]; then
  echo "threads started or exited during the quiet window; wakeups/s is not exact" >&2
fi

# smaps_rollup is exact; VmRSS in status is updated lazily.
rss_kib=$(awk '/^Rss:/ { print $2 }' /proc/"$pid"/smaps_rollup)
pss_kib=$(awk '/^Pss:/ { print $2 }' /proc/"$pid"/smaps_rollup)
threads=$(ls /proc/"$pid"/task | wc -l)
# PERF_DETAIL=1: where the RSS is (file-backed vs anonymous), on stderr.
if [ "${PERF_DETAIL:-0}" = 1 ]; then
  grep -E '^(Rss|Pss_Anon|Pss_File|Anonymous|AnonHugePages):' /proc/"$pid"/smaps_rollup >&2
fi

awk -v size="$size" -v start="$start_ms" -v rss="$rss_kib" -v pss="$pss_kib" \
    -v dt=$((ticks1 - ticks0)) -v hz="$hz" -v sw=$((sw1 - sw0)) -v q="$QUIET" \
    -v threads="$threads" -v w="$WARMUP" -v gate="${PERF_GATE:-0}" \
    -v max_bin="${MAX_BINARY_MIB:-15}" -v max_start="${MAX_START_MS:-1000}" \
    -v max_rss="${MAX_RSS_MIB:-20}" -v max_cpu="${MAX_CPU_PCT:-0.1}" \
    -v max_wake="${MAX_WAKEUPS:-1}" '
function check(name, value, limit, unit) {
  if (value + 0 > limit + 0) {
    printf "over budget: %s is %s %s, budget %s %s\n", name, value, unit, limit, unit > "/dev/stderr"
    over = 1
  }
}
BEGIN {
  bin = size / 1048576; rss_m = rss / 1024; cpu = 100 * dt / hz / q; wake = sw / q
  printf "| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |\n"
  printf "|---|---|---|---|---|---|---|---|\n"
  printf "| %.2f MiB | %d ms | %.1f MiB | %.1f MiB | %.3f %% | %.2f | %d | %ds warm-up, %ds quiet |\n",
    bin, start, rss_m, pss / 1024, cpu, wake, threads, w, q
  if (gate == 1) {
    check("binary", sprintf("%.2f", bin), max_bin, "MiB")
    check("start-up", start, max_start, "ms")
    check("idle RSS", sprintf("%.1f", rss_m), max_rss, "MiB")
    check("idle CPU", sprintf("%.3f", cpu), max_cpu, "%")
    check("wakeups", sprintf("%.2f", wake), max_wake, "/s")
    if (over) exit 3
    print "within budget" > "/dev/stderr"
  }
}'

# Format the perf.sh row; compare raw measurements before display rounding.
function check(name, value, limit, unit) {
  if (value + 0 > limit + 0) {
    printf "over budget: %s is %.12g %s, budget %.12g %s\n", name, value, unit, limit, unit > "/dev/stderr"
    over = 1
  }
}
BEGIN {
  start = start_ns / 1000000
  bin = size / 1048576; rss_m = rss / 1024; cpu = 100 * dt / hz / q; wake = sw / q
  printf "| binary | start→/api/ | idle RSS | idle PSS | idle CPU | wakeups/s | threads | run |\n"
  printf "|---|---|---|---|---|---|---|---|\n"
  printf "| %.2f MiB | %d ms | %.1f MiB | %.1f MiB | %.3f %% | %.2f | %d | %ds warm-up, %ds quiet |\n",
    bin, start, rss_m, pss / 1024, cpu, wake, threads, w, q
  if (gate == 1) {
    check("binary", bin, max_bin, "MiB")
    check("start-up", start, max_start, "ms")
    check("idle RSS", rss_m, max_rss, "MiB")
    check("idle CPU", cpu, max_cpu, "%")
    check("wakeups", wake, max_wake, "/s")
    if (over) exit 3
    print "within budget" > "/dev/stderr"
  }
}

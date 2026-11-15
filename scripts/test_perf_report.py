"""Exercise the same budget comparisons perf.sh uses, without Linux /proc."""
import pathlib
import subprocess
import unittest

REPORT = pathlib.Path(__file__).with_name("perf-report.awk")


class PerfReportTests(unittest.TestCase):
    def report(self, **changes):
        values = dict(size=15 * 1048576, start_ns=1000000000, rss=20 * 1024,
                      pss=1024, dt=1000, hz=100, sw=10000, q=10000,
                      threads=4, w=15, gate=1, max_bin=15, max_start=1000,
                      max_rss=20, max_cpu=0.1, max_wake=1)
        values.update(changes)
        args = ["awk"]
        for key, value in values.items():
            args.extend(["-v", f"{key}={value}"])
        return subprocess.run(args + ["-f", str(REPORT)],
                              capture_output=True, text=True, check=False)

    def test_at_and_below_budgets_pass(self):
        for values in ({}, dict(size=1, start_ns=1, rss=1, dt=0, sw=0)):
            with self.subTest(values=values):
                result = self.report(**values)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn("within budget", result.stderr)

    def test_just_over_each_budget_fails_even_when_display_rounds_down(self):
        baseline = self.report().stdout
        for metric, values in (
            ("binary", dict(size=15 * 1048576 + 1)),
            ("start-up", dict(start_ns=1000000001)),
            ("idle RSS", dict(rss=20 * 1024 + 1)),
            ("idle CPU", dict(dt=1001)),
            ("wakeups", dict(sw=10001)),
        ):
            with self.subTest(metric=metric):
                result = self.report(**values)
                self.assertEqual(result.returncode, 3, result.stderr)
                self.assertIn(f"over budget: {metric}", result.stderr)
                self.assertEqual(result.stdout, baseline)

    def test_overrides_and_non_gating_report(self):
        self.assertEqual(self.report(max_bin=14).returncode, 3)
        self.assertEqual(self.report(size=16 * 1048576, max_bin=16).returncode, 0)
        result = self.report(size=16 * 1048576, gate=0)
        self.assertEqual(result.returncode, 0)
        self.assertEqual(result.stderr, "")

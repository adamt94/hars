"""perf-docker.sh passes perf.sh's options into the container.

A fake `docker` on PATH records its arguments, so no container is run.
"""
import os
import pathlib
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).with_name("perf-docker.sh")


class PerfDockerTests(unittest.TestCase):
    def run_script(self, env, *args, cwd=None):
        with tempfile.TemporaryDirectory() as tmp:
            out = pathlib.Path(tmp, "args")
            docker = pathlib.Path(tmp, "docker")
            docker.write_text(f'#!/bin/sh\nprintf "%s\\n" "$@" > "{out}"\n')
            docker.chmod(0o755)
            full_env = {k: v for k, v in os.environ.items()
                        if not k.startswith(("PERF_", "MAX_"))}
            full_env.update(env, PATH=f"{tmp}:{os.environ['PATH']}")
            result = subprocess.run(["bash", str(SCRIPT), *args], env=full_env,
                                    cwd=cwd, capture_output=True, text=True,
                                    check=False)
            argv = out.read_text().splitlines() if out.exists() else None
            return result, argv

    def test_forwards_only_the_options_that_are_set(self):
        result, argv = self.run_script({"PERF_GATE": "1", "MAX_RSS_MIB": "1"})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("PERF_GATE=1", argv)
        self.assertIn("MAX_RSS_MIB=1", argv)
        self.assertNotIn("PERF_MET=0", argv)
        self.assertFalse(any(a.startswith("MAX_WAKEUPS") for a in argv))
        self.assertEqual(argv[-2:], ["15", "60"])

    def test_mounts_perf_config_resolved_from_the_callers_directory(self):
        with tempfile.TemporaryDirectory() as home:
            pathlib.Path(home, "cfg").mkdir()
            result, argv = self.run_script({"PERF_CONFIG": "cfg"}, "5", "10",
                                           cwd=home)
            self.assertEqual(result.returncode, 0, result.stderr)
            real = pathlib.Path(home, "cfg").resolve()
            mount = argv[argv.index("PERF_CONFIG=/perf-config") - 2]
            self.assertEqual(pathlib.Path(mount.split(":")[0]).resolve(), real)
            self.assertTrue(mount.endswith(":/perf-config:ro"))
            self.assertEqual(argv[-2:], ["5", "10"])

    def test_rejects_a_missing_perf_config(self):
        result, argv = self.run_script({"PERF_CONFIG": "/nonexistent-hars"})
        self.assertEqual(result.returncode, 1)
        self.assertIn("not a directory", result.stderr)
        self.assertIsNone(argv)


if __name__ == "__main__":
    unittest.main()

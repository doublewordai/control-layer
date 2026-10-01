"""Exercise runner dispatch without compiling or connecting to a database."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class RunnerTests(unittest.TestCase):
    def run_runner(self, *args, fail=False, via_just=False):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            log = directory / "calls.jsonl"
            cargo = directory / "cargo"
            cargo.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['RUNNER_TEST_LOG'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "if os.environ.get('RUNNER_TEST_FAIL') and sys.argv[1:3] == ['nextest', 'run']:\n"
                "    sys.exit(42)\n"
            )
            cargo.chmod(0o755)
            (directory / "cargo-watch").symlink_to(cargo)
            env = dict(os.environ, PATH=f"{directory}:{os.environ['PATH']}", RUNNER_TEST_LOG=str(log))
            if fail:
                env["RUNNER_TEST_FAIL"] = "1"
            command = ["just", "test", "rust"] if via_just else [str(ROOT / "scripts/test-rust.sh")]
            result = subprocess.run([*command, *args], cwd=ROOT, env=env, capture_output=True, text=True)
            self.assertFalse(result.stderr, result.stderr)
            return result.returncode, [json.loads(line) for line in log.read_text().splitlines()]

    def test_full_suite_includes_doctests(self):
        status, calls = self.run_runner()
        self.assertEqual(status, 0)
        self.assertEqual(calls[1:], [
            ["nextest", "run", "--workspace", "--all-features"],
            ["test", "--doc", "--workspace", "--all-features"],
        ])

    def test_runner_filter_is_not_sent_to_rustdoc(self):
        _, calls = self.run_runner("-E", "test(foo) | test(bar)", "--cargo-profile", "ci", "-j", "4")
        self.assertIn("test(foo) | test(bar)", calls[1])
        self.assertEqual(calls[2], ["test", "--doc", "--workspace", "--all-features", "--profile", "ci"])

    def test_failure_status_survives_doctests(self):
        status, calls = self.run_runner(fail=True)
        self.assertEqual(status, 42)
        self.assertEqual(calls[-1][:2], ["test", "--doc"])

    def test_coverage_forwards_filters_and_keeps_doctests(self):
        _, calls = self.run_runner("--coverage", "some_test")
        self.assertEqual(calls[1][:2], ["llvm-cov", "nextest"])
        self.assertEqual(calls[1][-1], "some_test")
        self.assertEqual(calls[2][:2], ["test", "--doc"])

    def test_watch_preserves_filter_as_one_argument(self):
        import shlex
        _, calls = self.run_runner("--watch", "--coverage", "-E", "test(foo) | test(bar)")
        self.assertEqual(calls[1][:2], ["watch", "-s"])
        self.assertEqual(shlex.split(calls[1][2]), ["./scripts/test-rust.sh", "--coverage", "-E", "test(foo) | test(bar)"])

    def test_no_run_does_not_execute_doctests(self):
        _, calls = self.run_runner("--no-run")
        self.assertEqual(len(calls), 2)

    def test_just_preserves_nextest_filter_quoting(self):
        status, calls = self.run_runner("-E", "test(foo) | test(bar)", via_just=True)
        self.assertEqual(status, 0)
        self.assertEqual(calls[1][-2:], ["-E", "test(foo) | test(bar)"])


if __name__ == "__main__":
    unittest.main()

"""Exercise runner dispatch without compiling or connecting to a database."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]


class RunnerTests(unittest.TestCase):
    def run_runner(self, *args, fail=False, via_just=False, extra_env=None,
                   script="scripts/test-rust.sh", expect_stderr=False):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            log = directory / "calls.jsonl"
            cargo = directory / "cargo"
            cargo.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['RUNNER_TEST_LOG'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "with open(os.environ['RUNNER_TEST_LOG'] + '.env', 'a') as log:\n"
                "    log.write(json.dumps(os.environ.get('SQLX_OFFLINE')) + '\\n')\n"
                "if os.environ.get('RUNNER_TEST_FAIL') and sys.argv[1:3] == ['nextest', 'run']:\n"
                "    sys.exit(int(os.environ['RUNNER_TEST_FAIL']))\n"
                "if sys.argv[1:3] == ['sqlx', 'prepare']:\n"
                "    with open(os.environ['RUNNER_TEST_LOG']) as log:\n"
                "        attempts = sum(json.loads(line)[:2] == ['sqlx', 'prepare'] for line in log)\n"
                "    sys.exit(attempts <= int(os.environ.get('RUNNER_TEST_PREPARE_FAILURES', '0')))\n"
            )
            cargo.chmod(0o755)
            (directory / "cargo-watch").symlink_to(cargo)
            # A stand-in flock records its arguments and runs the command, so
            # these tests neither need flock nor wait on a real build.
            flock_log = directory / "flock.jsonl"
            flock_log.touch()
            flock = directory / "flock"
            flock.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "args = sys.argv[1:]\n"
                "with open(os.environ['RUNNER_TEST_FLOCK_LOG'], 'a') as log:\n"
                "    log.write(json.dumps(args) + '\\n')\n"
                "command = args[args.index(os.environ['DW_BUILD_LOCK_FILE']) + 1:]\n"
                "os.execvp(command[0], command)\n"
            )
            flock.chmod(0o755)
            env = dict(os.environ, PATH=f"{directory}:{os.environ['PATH']}", RUNNER_TEST_LOG=str(log),
                       RUNNER_TEST_FLOCK_LOG=str(flock_log), DW_BUILD_LOCK_FILE=str(directory / "build.lock"))
            for name in ("CI", "DW_NO_BUILD_LOCK", "DW_BUILD_LOCK_HELD", "SQLX_OFFLINE"):
                env.pop(name, None)
            env.update(extra_env or {})
            if fail:
                env["RUNNER_TEST_FAIL"] = "42" if fail is True else str(fail)
            command = ["just", "test", "rust"] if via_just else [str(ROOT / script)]
            result = subprocess.run([*command, *args], cwd=ROOT, env=env, capture_output=True, text=True)
            if not expect_stderr:
                self.assertFalse(result.stderr, result.stderr)
            self.stderr = result.stderr
            self.sqlx_offline = [json.loads(line) for line in Path(f"{log}.env").read_text().splitlines()]
            self.lock_file = env["DW_BUILD_LOCK_FILE"]
            self.flock_calls = [json.loads(line) for line in flock_log.read_text().splitlines()]
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

    def test_explicit_package_replaces_workspace_default(self):
        for selection in [("-p", "dwctl"), ("--package=dwctl",), ("-pdwctl",)]:
            with self.subTest(selection=selection):
                _, calls = self.run_runner(*selection)
                for call in calls[1:]:
                    self.assertNotIn("--workspace", call)
                    self.assertIn("--all-features", call)
                    self.assertEqual(call[-len(selection):], list(selection))

    def test_explicit_features_replace_all_features_default(self):
        for selection in [("--features", "foo"), ("--features=foo",), ("-Ffoo",),
                          ("-F", "foo"), ("--no-default-features",)]:
            with self.subTest(selection=selection):
                _, calls = self.run_runner(*selection)
                for call in calls[1:]:
                    self.assertNotIn("--all-features", call)
                    self.assertIn("--workspace", call)
                    self.assertEqual(call[-len(selection):], list(selection))

    def test_explicit_workspace_and_all_features_are_preserved(self):
        _, calls = self.run_runner("-p", "dwctl", "--workspace", "--features", "foo", "--all-features")
        for call in calls[1:]:
            self.assertEqual(call.count("--workspace"), 1)
            self.assertEqual(call.count("--all-features"), 1)

    def test_one_build_lock_spans_tests_and_doctests(self):
        _, calls = self.run_runner("-E", "test(foo) | test(bar)")
        probe, held = self.flock_calls
        self.assertEqual(probe, ["-n", self.lock_file, "true"])
        self.assertEqual(held[:3], ["-o", "-w", "3600"])
        self.assertEqual(held[-4:], [self.lock_file, "./scripts/test-rust.sh", "-E", "test(foo) | test(bar)"])
        self.assertEqual([call[:2] for call in calls], [["nextest", "--version"], ["nextest", "run"], ["test", "--doc"]])

    def test_build_lock_is_skipped_when_held_disabled_or_in_ci(self):
        for extra_env in [{"DW_BUILD_LOCK_HELD": "1"}, {"DW_NO_BUILD_LOCK": "1"}, {"CI": "true"}]:
            with self.subTest(extra_env=extra_env):
                status, calls = self.run_runner(extra_env=extra_env)
                self.assertEqual(status, 0)
                self.assertEqual(self.flock_calls, [])
                self.assertEqual([call[:2] for call in calls], [["nextest", "--version"], ["nextest", "run"], ["test", "--doc"]])

    def test_watch_leaves_locking_to_each_inner_run(self):
        self.run_runner("--watch")
        self.assertEqual(self.flock_calls, [])

    def test_builds_default_to_sqlx_offline(self):
        self.run_runner()
        self.assertEqual(self.sqlx_offline[1:], ["true", "true"])
        for value in ("false", ""):
            with self.subTest(value=value):
                self.run_runner(extra_env={"SQLX_OFFLINE": value})
                self.assertEqual(self.sqlx_offline[1:], [value, value])

    def test_failed_offline_build_names_the_remedy(self):
        status, _ = self.run_runner(fail=101, expect_stderr=True)
        self.assertEqual(status, 101)
        self.assertIn("./scripts/sqlx-prepare.sh", self.stderr)
        # A test failure, or a build the caller took online, is not a metadata problem.
        self.run_runner(fail=100)
        self.run_runner(fail=101, extra_env={"SQLX_OFFLINE": "false"})

    def test_sqlx_prepare_covers_every_target(self):
        scope = ["--workspace", "--", "--workspace", "--all-targets", "--all-features"]
        status, calls = self.run_runner(script="scripts/sqlx-prepare.sh", expect_stderr=True)
        self.assertEqual(status, 0)
        # Dependencies are compiled offline first so their queries are not recorded.
        self.assertEqual(calls, [
            ["check", "--workspace", "--all-targets", "--all-features", "--keep-going"],
            ["sqlx", "prepare", *scope],
        ])
        self.assertEqual(self.sqlx_offline, ["true", None])
        status, calls = self.run_runner("--check", script="scripts/sqlx-prepare.sh")
        self.assertEqual(status, 0)
        self.assertEqual(calls, [["sqlx", "prepare", "--check", *scope]])

    def test_sqlx_prepare_check_retries_once(self):
        for failures, expected in [(1, 0), (2, 1)]:
            with self.subTest(failures=failures):
                status, calls = self.run_runner(
                    "--check", script="scripts/sqlx-prepare.sh", expect_stderr=True,
                    extra_env={"RUNNER_TEST_PREPARE_FAILURES": str(failures)})
                self.assertEqual(status, expected)
                self.assertEqual(len(calls), 2)

    def test_just_without_target_dispatches_empty_default(self):
        # Execute the actual dispatch recipe, replacing only its downstream
        # integration runner so this check needs no running services.
        justfile = (ROOT / "justfile").read_text()
        start, end = "[positional-arguments]\ntest ", "\n_test-other "
        # Fail clearly if a justfile refactor moves either marker, rather than
        # splicing the wrong text and no longer exercising the dispatch path.
        self.assertEqual(justfile.count(start), 1, f"expected one {start!r} in justfile")
        self.assertEqual(justfile.count(end), 1, f"expected one {end!r} in justfile")
        recipe = justfile.split(start, 1)[1].split(end, 1)[0]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "justfile"
            path.write_text('[positional-arguments]\ntest ' + recipe +
                            '\n_test-other target="" *args="":\n    @echo dispatched target="{{target}}"\n')
            result = subprocess.run(["just", "--justfile", str(path), "test"], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), "dispatched target=")


if __name__ == "__main__":
    unittest.main()

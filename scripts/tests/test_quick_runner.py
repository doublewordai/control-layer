"""Exercise the changed-files to nextest-filterset mapping without compiling."""

import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts/test-quick.sh"


def dwctl(*filters):
    inner = " | ".join(filters)
    return f"package(dwctl) & ({inner})" if len(filters) > 1 else f"package(dwctl) & {inner}"


def test(module):
    return f"test(/^{module}::/)"


class MappingTests(unittest.TestCase):
    def filterset(self, *paths):
        result = subprocess.run([str(SCRIPT), "--stdin", "--print"], input="".join(f"{path}\n" for path in paths),
                                cwd=ROOT, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.stderr = result.stderr
        return result.stdout.strip()

    def test_api_handler_selects_its_resource(self):
        self.assertEqual(self.filterset("dwctl/src/api/handlers/users.rs"),
                         dwctl(test("api::handlers::users"), test("api::models::users")))
        self.assertEqual(self.filterset("dwctl/src/api/models/deployments/enrichment.rs"),
                         dwctl(test("api::handlers::deployments"), test("api::models::deployments")))

    def test_repository_also_selects_the_api_layer(self):
        expected = dwctl(test("api::handlers::users"), test("api::models::users"),
                         test("db::handlers::users"), test("db::models::users"))
        self.assertEqual(self.filterset("dwctl/src/db/handlers/users.rs"), expected)
        self.assertEqual(self.filterset("dwctl/src/db/models/users.rs"), expected)

    def test_shared_layer_files_select_the_layer(self):
        self.assertEqual(self.filterset("dwctl/src/api/handlers/mod.rs"), dwctl(test("api::handlers")))
        self.assertEqual(self.filterset("dwctl/src/db/handlers/repository.rs"), dwctl(test("db::handlers")))
        self.assertEqual(self.filterset("dwctl/src/db/mod.rs"), dwctl(test("db")))
        self.assertEqual(self.filterset("dwctl/src/db/pools.rs"), dwctl(test("db::pools")))

    def test_other_modules_select_their_top_level_module(self):
        self.assertEqual(self.filterset("dwctl/src/auth/middleware.rs", "dwctl/src/config.rs"),
                         dwctl(test("auth"), test("config")))
        self.assertEqual(self.filterset("dwctl/src/sync/onwards_config/tests.rs"), dwctl(test("sync")))

    def test_migrations_select_the_migration_tests(self):
        expected = dwctl(test("migrations"))
        for path in ("dwctl/migrations/20260101000000_example.sql", "dwctl/src/migrations.rs",
                     "dwctl/src/migrations/tariff_indexes_tests.rs", "dwctl/underway-migrations/1_example.sql"):
            with self.subTest(path=path):
                self.assertEqual(self.filterset(path), expected)
        self.assertEqual(self.filterset("fusillade-arsenal/migrations/20260101000000_example.sql"),
                         f"package(dwctl) & {test('migrations')} | package(fusillade-arsenal)")

    def test_test_modules_run_alone_but_the_harness_runs_the_crate(self):
        self.assertEqual(self.filterset("dwctl/src/test/anthropic.rs", "dwctl/src/test/responses/request_id.rs"),
                         dwctl(test("test::anthropic"), test("test::responses")))
        for path in ("dwctl/src/test/mod.rs", "dwctl/src/test/utils.rs", "dwctl/src/test/databases.rs",
                     "dwctl/src/test/template.rs", "dwctl/test-macros/src/lib.rs"):
            with self.subTest(path=path):
                self.assertIn("package(dwctl)", self.filterset(path).split(" | "))

    def test_crate_wide_files_fall_back_to_the_package(self):
        for path in ("dwctl/src/lib.rs", "dwctl/src/main.rs", "dwctl/src/bin/model_provisioning.rs",
                     "dwctl/Cargo.toml", "dwctl/build.rs", "dwctl/src/retention_preflight.sql",
                     "dwctl/default_templates/low_balance.html"):
            with self.subTest(path=path):
                self.assertEqual(self.filterset(path), "package(dwctl)")
        # The package already covers narrower filters in the same crate.
        self.assertEqual(self.filterset("dwctl/src/lib.rs", "dwctl/src/api/handlers/users.rs"), "package(dwctl)")

    def test_integration_tests_select_their_binary(self):
        self.assertEqual(self.filterset("dwctl/tests/cors_default.rs"), dwctl("binary(cors_default)"))
        # A deleted or shared file has no binary of its own.
        shared = dwctl("kind(test)", test("migrations"))
        self.assertEqual(self.filterset("dwctl/tests/removed_test.rs"), shared)
        self.assertEqual(self.filterset("dwctl/tests/fixtures/underway_claim.sql"), shared)

    def test_memory_budget_is_left_to_its_own_target(self):
        self.assertEqual(self.filterset("dwctl/tests/memory_budget/main.rs"), "none()")
        self.assertIn("just test memory", self.stderr)

    def test_other_crates_select_their_package(self):
        self.assertEqual(self.filterset("onwards/src/lib.rs", "fusillade/tests/integration.rs",
                                        "fusillade-core/src/lib.rs", "openai-reassembler/test_cases.json",
                                        "dwctl/src/api/handlers/users.rs"),
                         dwctl(test("api::handlers::users"), test("api::models::users")) +
                         " | package(fusillade) | package(fusillade-core) | package(onwards)"
                         " | package(openai-reassembler)")

    def test_every_workspace_member_is_mapped(self):
        members = re.search(r"^members = \[(.*?)\]", (ROOT / "Cargo.toml").read_text(), re.S | re.M).group(1)
        for member in re.findall(r'"([^"]+)"', members):
            with self.subTest(member=member):
                name = re.search(r'^name = "([^"]+)"', (ROOT / member / "Cargo.toml").read_text(), re.M).group(1)
                self.assertIn(f"package({name})", self.filterset(f"{member}/Cargo.toml").split(" | "))

    def test_workspace_wide_files_run_everything(self):
        for path in ("Cargo.toml", "Cargo.lock", ".config/nextest.toml"):
            with self.subTest(path=path):
                self.assertEqual(self.filterset(path, "dwctl/src/api/handlers/users.rs"), "all()")

    def test_non_rust_changes_run_nothing(self):
        self.assertEqual(self.filterset("README.md", "dashboard/src/App.tsx", "docs/migrations.md", "justfile",
                                        "dwctl/CHANGELOG.md", "onwards/docs/usage.md", ".sqlx/query-1.json"),
                         "none()")
        self.assertIn("nothing to run", self.stderr)
        self.assertEqual(self.filterset(), "none()")


class RunTests(unittest.TestCase):
    def run_quick(self, *args, paths=(), via_just=False):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            log = directory / "calls.jsonl"
            log.touch()
            cargo = directory / "cargo"
            cargo.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['RUNNER_TEST_LOG'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            )
            cargo.chmod(0o755)
            # The lock has its own tests; holding it here keeps flock out of these.
            env = dict(os.environ, PATH=f"{directory}:{os.environ['PATH']}", RUNNER_TEST_LOG=str(log),
                       DW_BUILD_LOCK_HELD="1")
            command = ["just", "test", "quick"] if via_just else [str(SCRIPT)]
            result = subprocess.run([*command, "--stdin", *args], input="".join(f"{path}\n" for path in paths),
                                    cwd=ROOT, env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.stderr = result.stderr
            return [json.loads(line) for line in log.read_text().splitlines()]

    def test_runs_the_filterset_through_the_workspace_runner_without_doctests(self):
        expected = dwctl(test("api::handlers::users"), test("api::models::users"))
        for via_just in (False, True):
            with self.subTest(via_just=via_just):
                calls = self.run_quick("--no-fail-fast", "-j", "4", paths=["dwctl/src/api/handlers/users.rs"],
                                       via_just=via_just)
                self.assertEqual(calls, [
                    ["nextest", "--version"],
                    ["nextest", "run", "--workspace", "--all-features", "-E", expected, "--no-tests=warn",
                     "--no-fail-fast", "-j", "4"],
                ])
                self.assertIn(expected, self.stderr)

    def test_full_suite_fallback_runs_without_a_filter(self):
        calls = self.run_quick(paths=["Cargo.lock"])
        self.assertEqual(calls[1:], [["nextest", "run", "--workspace", "--all-features", "--no-tests=warn"]])

    def test_nothing_to_run_does_not_invoke_cargo(self):
        self.assertEqual(self.run_quick(paths=["README.md"]), [])
        self.assertIn("nothing to run", self.stderr)


class ChangedFilesTests(unittest.TestCase):
    def test_collects_committed_staged_unstaged_and_untracked_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull, GIT_CONFIG_SYSTEM=os.devnull,
                       GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.com",
                       GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.com")

            def git(*args):
                subprocess.run(["git", *args], cwd=repo, env=env, check=True, capture_output=True)

            def write(path, text="changed\n"):
                (repo / path).parent.mkdir(parents=True, exist_ok=True)
                (repo / path).write_text(text)

            (repo / "scripts").mkdir()
            shutil.copy(SCRIPT, repo / "scripts/test-quick.sh")
            for path in ("dwctl/src/auth/mod.rs", "dwctl/src/config.rs", "dwctl/src/limits.rs", "onwards/src/lib.rs"):
                write(path, "base\n")
            git("init", "-q", "-b", "work")
            git("add", ".")
            git("commit", "-q", "-m", "base")
            git("update-ref", "refs/remotes/origin/main", "HEAD")
            git("tag", "other-base")

            def quick(*args):
                return subprocess.run([str(repo / "scripts/test-quick.sh"), "--print", *args], cwd=repo, env=env,
                                      capture_output=True, text=True)

            self.assertEqual(quick().stdout.strip(), "none()")
            write("dwctl/src/auth/mod.rs")
            git("commit", "-q", "-am", "committed")
            write("dwctl/src/config.rs")
            git("add", "dwctl/src/config.rs")
            write("dwctl/src/limits.rs")
            write("dwctl/src/api/handlers/users.rs")
            result = quick()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(result.stdout.strip(),
                             dwctl(test("api::handlers::users"), test("api::models::users"), test("auth"),
                                   test("config"), test("limits")))

            # The base moves with the argument: only what changed since that commit counts.
            git("add", ".")
            git("commit", "-q", "-m", "more")
            git("tag", "later-base")
            write("onwards/src/lib.rs")
            self.assertEqual(quick("--base", "later-base").stdout.strip(), "package(onwards)")
            self.assertEqual(quick("--base=other-base").stdout.strip().split(" | ")[-1], "package(onwards)")

            missing = quick("--base", "no-such-ref")
            self.assertEqual(missing.returncode, 2)
            self.assertIn("no merge base", missing.stderr)


class NoDoctestsTests(unittest.TestCase):
    def test_runner_skips_doctests_and_does_not_forward_the_flag(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            log = directory / "calls.jsonl"
            cargo = directory / "cargo"
            cargo.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['RUNNER_TEST_LOG'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
            )
            cargo.chmod(0o755)
            (directory / "cargo-watch").symlink_to(cargo)
            env = dict(os.environ, PATH=f"{directory}:{os.environ['PATH']}", RUNNER_TEST_LOG=str(log),
                       DW_BUILD_LOCK_HELD="1")

            def runner(*args):
                log.write_text("")
                subprocess.run([str(ROOT / "scripts/test-rust.sh"), *args], cwd=ROOT, env=env, check=True)
                return [json.loads(line) for line in log.read_text().splitlines()]

            self.assertEqual(runner("--no-doctests", "-E", "test(foo)")[1:],
                             [["nextest", "run", "--workspace", "--all-features", "-E", "test(foo)"]])
            self.assertEqual(runner("--watch", "--no-doctests")[1],
                             ["watch", "-s", "./scripts/test-rust.sh --no-doctests "])


if __name__ == "__main__":
    unittest.main()

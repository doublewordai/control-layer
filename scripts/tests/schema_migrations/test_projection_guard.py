"""The query guard must reject the patterns that caused stale result descriptors."""

import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "guard", Path(__file__).resolve().parents[2] / "check_query_projections.py"
)
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)


class ProjectionGuardTests(unittest.TestCase):
    def test_reintroduced_wildcards(self):
        for source in (
            '"SELECT * FROM users"',
            'r#"SELECT dm.* FROM deployed_models dm"#',
            '"select DISTINCT dm.* from deployed_models dm"',
            '"INSERT INTO users DEFAULT VALUES RETURNING *"',
            '"UPDATE users u SET verified = true RETURNING u.*"',
            '"SELECT id, dm.* FROM deployed_models dm"',
            '"SELECT id, * FROM deployed_models"',
            '"SELECT DISTINCT ON (id) dm.* FROM deployed_models dm"',
            '"SELECT\\n dm.* FROM deployed_models dm"',
        ):
            with self.subTest(source=source):
                self.assertTrue(guard.wildcards(source))

    def test_fixed_projections_and_aggregate(self):
        for source in (
            '"SELECT dm.id, dm.alias FROM deployed_models dm"',
            '"SELECT COUNT(*) FROM users"',
            '"UPDATE users SET verified = true RETURNING id, verified"',
            '"SELECT price * tokens FROM usage"',
            "($($file:literal),* $(,)?) => {}",
        ):
            with self.subTest(source=source):
                self.assertFalse(guard.wildcards(source))

    def test_comments_are_not_queries(self):
        self.assertFalse(guard.wildcards("// Avoid SELECT * FROM users\n"))
        self.assertFalse(guard.wildcards("//! SELECT * is unsafe\n"))


if __name__ == "__main__":
    unittest.main()

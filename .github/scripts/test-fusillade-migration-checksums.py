#!/usr/bin/env python3

import hashlib
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
FIXTURE = ROOT / ".github/fixtures/fusillade-migration-sha384.txt"
MIGRATIONS = ROOT / "fusillade-arsenal/migrations"


def load_expected() -> dict[str, str]:
    expected: dict[str, str] = {}
    for line in FIXTURE.read_text().splitlines():
        digest, path = line.split(maxsplit=1)
        expected[path] = digest
    return expected


def main() -> None:
    expected = load_expected()
    actual_paths = {
        path.relative_to(ROOT).as_posix() for path in MIGRATIONS.glob("*.up.sql")
    }

    if actual_paths != set(expected):
        added = sorted(actual_paths - set(expected))
        removed = sorted(set(expected) - actual_paths)
        raise SystemExit(
            "Fusillade migration checksum fixture is out of date: "
            f"added={added}, removed={removed}"
        )

    mismatches = []
    for relative_path, expected_digest in sorted(expected.items()):
        actual_digest = hashlib.sha384((ROOT / relative_path).read_bytes()).hexdigest()
        if actual_digest != expected_digest:
            mismatches.append(
                f"{relative_path}: expected {expected_digest}, got {actual_digest}"
            )

    if mismatches:
        raise SystemExit(
            "Applied Fusillade migrations are immutable; add a new migration instead:\n"
            + "\n".join(mismatches)
        )


if __name__ == "__main__":
    main()

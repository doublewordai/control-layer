#!/usr/bin/env python3
"""Synchronize Fusillade dependency requirements across release majors."""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path


def major(version: str) -> str:
    return version.split(".", 1)[0]


def parse_version(version: str) -> tuple[int, int, int]:
    parts = [int(part) for part in version.split(".")]
    return tuple((parts + [0, 0])[:3])


def requirement_accepts(requirement: str, tracked_version: str) -> bool:
    if major(requirement.lstrip("^~=")) == major(tracked_version):
        return True

    clauses = [clause.strip() for clause in requirement.split(",")]
    tracked = parse_version(tracked_version)
    comparisons = {
        ">=": lambda candidate, bound: candidate >= bound,
        "<=": lambda candidate, bound: candidate <= bound,
        ">": lambda candidate, bound: candidate > bound,
        "<": lambda candidate, bound: candidate < bound,
        "=": lambda candidate, bound: candidate == bound,
    }

    for clause in clauses:
        match = re.fullmatch(r"(>=|<=|>|<|=)\s*(\d+(?:\.\d+){0,2})", clause)
        if not match:
            return False
        if not comparisons[match.group(1)](tracked, parse_version(match.group(2))):
            return False
    return True


def sync_dependency(
    manifest: Path, dependency: str, tracked_version: str, check: bool
) -> bool:
    text = manifest.read_text()
    pattern = re.compile(
        rf'(?m)^(\s*{re.escape(dependency)}\s*=\s*'
        rf'(?:\{{[^\n}}]*?\bversion\s*=\s*)?")([^\"]+)(")'
    )
    matches = list(pattern.finditer(text))
    if len(matches) != 1:
        raise SystemExit(
            f"expected exactly one {dependency} requirement in {manifest}, "
            f"found {len(matches)}"
        )

    match = matches[0]
    requirement = match.group(2)
    if requirement_accepts(requirement, tracked_version):
        return False

    if check:
        print(
            f"{manifest} requires {dependency} {requirement}, but "
            f"release-please tracks {tracked_version}",
            file=sys.stderr,
        )
        return True

    updated = text[: match.start(2)] + tracked_version + text[match.end(2) :]
    manifest.write_text(updated)
    print(f"Updated {manifest}: {dependency} {requirement} -> {tracked_version}")
    return True


def main() -> None:
    arguments = sys.argv[1:]
    check = "--check" in arguments
    arguments = [argument for argument in arguments if argument != "--check"]
    if len(arguments) > 1:
        raise SystemExit("usage: sync-fusillade-release-dependencies.py [--check] [repo]")

    repo_root = Path(arguments[0]).resolve() if arguments else Path.cwd()
    versions = json.loads(
        (repo_root / ".release-please-manifest.json").read_text()
    )

    dependencies = (
        (repo_root / "fusillade/Cargo.toml", "fusillade-core", "fusillade-core"),
        (
            repo_root / "fusillade/Cargo.toml",
            "fusillade-arsenal",
            "fusillade-arsenal",
        ),
        (
            repo_root / "fusillade-arsenal/Cargo.toml",
            "fusillade-core",
            "fusillade-core",
        ),
        (repo_root / "dwctl/Cargo.toml", "fusillade", "fusillade"),
        (
            repo_root / "dwctl/Cargo.toml",
            "fusillade-arsenal",
            "fusillade-arsenal",
        ),
    )

    changes_needed = False
    for manifest, dependency, tracked_path in dependencies:
        tracked_version = versions.get(tracked_path)
        if not tracked_version:
            raise SystemExit(
                f"could not find tracked version for {tracked_path} in "
                ".release-please-manifest.json"
            )
        changes_needed |= sync_dependency(
            manifest, dependency, tracked_version, check
        )

    if check and changes_needed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()

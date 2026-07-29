#!/usr/bin/env python3
"""Keep publishable Fusillade dependency requirements release-compatible."""

from __future__ import annotations

import json
import re
import sys
from pathlib import Path


def major(version: str) -> str:
    return version.lstrip("^~=").split(".", 1)[0]


def sync_core_requirement(repo_root: Path, check: bool) -> bool:
    versions = json.loads(
        (repo_root / ".release-please-manifest.json").read_text()
    )
    tracked_version = versions.get("fusillade-core")
    if not tracked_version:
        raise SystemExit(
            "release manifest does not track a fusillade-core version"
        )

    manifest = repo_root / "fusillade-arsenal/Cargo.toml"
    text = manifest.read_text()
    pattern = re.compile(
        r'(?m)^(\s*fusillade-core\s*=\s*\{[^\n}]*\bversion\s*=\s*")'
        r'([^"]+)(")'
    )
    matches = list(pattern.finditer(text))
    if len(matches) != 1:
        raise SystemExit(
            f"expected one versioned fusillade-core dependency in {manifest}"
        )

    match = matches[0]
    requirement = match.group(2)
    if major(requirement) == major(tracked_version):
        return False

    if check:
        print(
            f"{manifest} requires fusillade-core {requirement}, but "
            f"Release Please tracks {tracked_version}",
            file=sys.stderr,
        )
        return True

    updated = text[: match.start(2)] + tracked_version + text[match.end(2) :]
    manifest.write_text(updated)
    print(
        f"Updated {manifest}: fusillade-core "
        f"{requirement} -> {tracked_version}"
    )
    return True


def main() -> None:
    arguments = sys.argv[1:]
    check = "--check" in arguments
    arguments = [argument for argument in arguments if argument != "--check"]
    if len(arguments) > 1:
        raise SystemExit(
            "usage: sync-fusillade-release-dependencies.py [--check] [repo]"
        )

    repo_root = Path(arguments[0]).resolve() if arguments else Path.cwd()
    changes_needed = sync_core_requirement(repo_root, check)
    if check and changes_needed:
        raise SystemExit(1)


if __name__ == "__main__":
    main()

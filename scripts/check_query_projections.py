#!/usr/bin/env python3
"""Conservative source guard for SQL wildcard projections.

This is a source guard, not a SQL parser. Dynamic SQL still needs review and
live-pooler migration tests. Reviewed exceptions must explain why their final
result descriptor is independent of base-table column additions.
"""

import re

IDENTIFIER = r'(?:[A-Za-z_][A-Za-z_0-9]*|"[^"\n]+")'
PROJECTION = re.compile(
    rf"\b(?:SELECT\s+(?:DISTINCT(?:\s+ON\s*\([^)]*\))?\s+)?|RETURNING\s+)(?:{IDENTIFIER}\s*\.\s*)?\*"
    rf"|,\s*{IDENTIFIER}\s*\.\s*\*"
    r'|,\s*\*(?=\s*(?:FROM\b|["#),;]))',
    re.IGNORECASE,
)


def wildcards(source):
    source = re.sub(
        r"^[ \t]*//[^\n]*", lambda m: " " * len(m.group()), source, flags=re.MULTILINE
    )
    source = source.replace("\\n", "  ").replace("\\\n", " \n")
    return list(PROJECTION.finditer(source))


def findings(root):
    from collections import Counter

    found = Counter()
    for tree in ("dwctl/src", "fusillade-arsenal/src"):
        for path in sorted((root / tree).rglob("*.rs")):
            source = path.read_text()
            for match in wildcards(source):
                start = source.rfind("\n", 0, match.start()) + 1
                end = source.find("\n", match.end())
                line = source[start : end if end != -1 else len(source)].strip()
                found[(str(path.relative_to(root)), line)] += 1
    return found


def main():
    from pathlib import Path
    import json

    root = Path(__file__).resolve().parent.parent
    entries = json.loads(
        (root / ".github/fixtures/stable-query-projections.json").read_text()
    )
    allowed = {}
    for entry in entries:
        key = (entry["path"], entry["source"])
        if key in allowed or not entry["reason"].strip():
            raise ValueError("duplicate or unexplained projection exemption")
        allowed[key] = entry["count"]
    found = findings(root)
    failures = []
    for key in sorted(found.keys() | allowed.keys()):
        if found.get(key, 0) != allowed.get(key, 0):
            failures.append(
                f"{key[0]}: expected {allowed.get(key, 0)} reviewed occurrences, found {found.get(key, 0)}: {key[1]}"
            )
    for failure in failures:
        print(failure)
    if not failures:
        print("PASS: SQL result projections match the reviewed stable-shape audit")
    return bool(failures)


if __name__ == "__main__":
    raise SystemExit(main())

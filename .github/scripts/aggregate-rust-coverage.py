#!/usr/bin/env python3
"""Merge LCOV line coverage without double-counting shared source lines."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--minimum", type=float, required=True)
    parser.add_argument("tracefiles", nargs="+", type=Path)
    args = parser.parse_args()
    if not 0 <= args.minimum <= 100:
        parser.error("--minimum must be between 0 and 100")
    return args


def merged_line_counts(tracefiles: list[Path]) -> dict[tuple[str, int], int]:
    line_counts: dict[tuple[str, int], int] = {}

    for tracefile in tracefiles:
        source: str | None = None
        with tracefile.open(encoding="utf-8") as lcov:
            for raw_line in lcov:
                record = raw_line.rstrip("\n")
                if record.startswith("SF:"):
                    source = str(Path(record[3:]))
                elif record.startswith("DA:"):
                    if source is None:
                        raise ValueError(f"{tracefile}: DA record without an SF record")
                    fields = record[3:].split(",")
                    if len(fields) < 2:
                        raise ValueError(f"{tracefile}: invalid DA record: {record}")
                    line_number = int(fields[0])
                    execution_count = int(fields[1])
                    key = (source, line_number)
                    line_counts[key] = line_counts.get(key, 0) + execution_count

    return line_counts


def main() -> int:
    args = parse_args()
    try:
        line_counts = merged_line_counts(args.tracefiles)
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        return 2

    lines_found = len(line_counts)
    if lines_found == 0:
        print("No instrumented Rust lines found", file=sys.stderr)
        return 2

    lines_hit = sum(count > 0 for count in line_counts.values())
    coverage = lines_hit * 100 / lines_found

    print(f"lines_found={lines_found}")
    print(f"lines_hit={lines_hit}")
    print(f"coverage={coverage:.2f}")

    if coverage < args.minimum:
        print(
            f"Backend coverage {coverage:.2f}% is below {args.minimum:.2f}%",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

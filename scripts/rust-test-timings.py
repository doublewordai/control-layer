#!/usr/bin/env python3
"""Summarize nextest JUnit reports (one local report or multiple CI shards)."""

import argparse
from collections import defaultdict
import xml.etree.ElementTree as ET


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reports", nargs="+")
    parser.add_argument("--slowest", type=int, default=20)
    args = parser.parse_args()
    tests = []
    groups = defaultdict(lambda: [0, 0.0])
    for path in args.reports:
        root = ET.parse(path).getroot()
        print(f"{path}: wall={float(root.get('time', 0)):.2f}s "
              f"tests={root.get('tests', '?')} failures={root.get('failures', '?')} "
              f"errors={root.get('errors', '?')}")
        for case in root.iter("testcase"):
            if case.find("skipped") is not None:
                continue
            duration = float(case.get("time", 0))
            name = f"{case.get('classname', '')}::{case.get('name', '')}"
            tests.append((duration, name))
            group = case.get("classname", "unknown")
            groups[group][0] += 1
            groups[group][1] += duration
    print("\nCumulative test time (parallel work, not wall time):")
    for group, (count, duration) in sorted(groups.items(), key=lambda item: item[1][1], reverse=True):
        print(f"{duration:9.2f}s {count:5d} tests  {group}")
    print("\nSlowest tests:")
    for duration, name in sorted(tests, reverse=True)[:args.slowest]:
        print(f"{duration:9.2f}s  {name}")


if __name__ == "__main__":
    main()

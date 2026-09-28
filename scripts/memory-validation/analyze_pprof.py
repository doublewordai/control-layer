#!/usr/bin/env python3
"""Decode and analyze gzip-compressed pprof heap profiles without third-party deps.

This is a minimal reader for the subset of `profile.proto` that
`jemalloc_pprof` emits. It exists so the validation harness can assert on
profiles on hosts that have no Go toolchain or `pprof` binary. It is not a
general-purpose pprof implementation: it reads `sample_type`, `sample`,
`location`, `function` and `string_table` and ignores mappings, labels,
comments and periods.

Usage:
  analyze_pprof.py PROFILE [--top N]
  analyze_pprof.py PROFILE --assert-function NAME --min-bytes X
  analyze_pprof.py --diff A B [--top N]

`PROFILE` may be gzip-compressed or raw protobuf. The "inuse space" sample
type is used when present. Flat attribution credits the leaf frame; cumulative
attribution credits every distinct frame in a stack once.
"""

from __future__ import annotations

import argparse
import gzip
import re
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from typing import Dict, Iterable, Iterator, List, Optional, Tuple

# ---------------------------------------------------------------------------
# Minimal protobuf wire decoding
# ---------------------------------------------------------------------------

WIRE_VARINT = 0
WIRE_64BIT = 1
WIRE_LENGTH = 2
WIRE_32BIT = 5

_VARINT_LIMIT = 10  # bytes; a 64-bit value needs at most 10 base-128 digits


def _read_varint(buf: bytes, pos: int) -> Tuple[int, int]:
    """Read one base-128 varint, returning ``(value, new_pos)``."""
    result = 0
    shift = 0
    while True:
        if pos >= len(buf):
            raise ValueError("truncated varint")
        byte = buf[pos]
        pos += 1
        result |= (byte & 0x7F) << shift
        if not byte & 0x80:
            return result, pos
        shift += 7
        if shift > _VARINT_LIMIT * 7:
            raise ValueError("varint too long")


def iter_fields(buf: bytes) -> Iterator[Tuple[int, int, object]]:
    """Yield ``(field_number, wire_type, value)`` for every top-level field."""
    pos = 0
    end = len(buf)
    while pos < end:
        key, pos = _read_varint(buf, pos)
        field_number = key >> 3
        wire_type = key & 0x07
        if wire_type == WIRE_VARINT:
            value, pos = _read_varint(buf, pos)
        elif wire_type == WIRE_LENGTH:
            length, pos = _read_varint(buf, pos)
            if pos + length > end:
                raise ValueError("length-delimited field overruns buffer")
            value = buf[pos : pos + length]
            pos += length
        elif wire_type == WIRE_64BIT:
            value = buf[pos : pos + 8]
            pos += 8
        elif wire_type == WIRE_32BIT:
            value = buf[pos : pos + 4]
            pos += 4
        else:
            raise ValueError(f"unsupported wire type {wire_type}")
        yield field_number, wire_type, value


def _read_packed_or_single(buf: bytes) -> List[int]:
    """Read a repeated numeric field that may be packed or repeated."""
    values: List[int] = []
    # A packed field is delivered as one length-delimited blob; a non-packed
    # repetition is delivered once per element. Both are handled by trying to
    # decode the blob as a sequence of varints, and falling back if it does not
    # consume exactly.
    if len(buf) == 0:
        return values
    pos = 0
    while pos < len(buf):
        try:
            value, pos = _read_varint(buf, pos)
        except ValueError:
            # Not packed; the caller should use the single varint value.
            return []
        values.append(value)
    return values


def _decode_int64(value: int) -> int:
    """Reinterpret an unsigned varint as a signed 64-bit integer."""
    if value >= 1 << 63:
        return value - (1 << 64)
    return value


# ---------------------------------------------------------------------------
# Profile model
# ---------------------------------------------------------------------------


@dataclass
class Function:
    id: int
    name: int = 0
    system_name: int = 0
    filename: int = 0


@dataclass
class Line:
    function_id: int = 0
    line: int = 0


@dataclass
class Location:
    id: int = 0
    lines: List[Line] = field(default_factory=list)
    address: int = 0


@dataclass
class Sample:
    location_ids: List[int] = field(default_factory=list)
    values: List[int] = field(default_factory=list)


@dataclass
class Profile:
    sample_types: List[Tuple[int, int]] = field(default_factory=list)  # (type idx, unit idx)
    samples: List[Sample] = field(default_factory=list)
    functions: Dict[int, Function] = field(default_factory=dict)
    locations: Dict[int, Location] = field(default_factory=dict)
    strings: List[str] = field(default_factory=list)

    def string(self, index: int) -> str:
        if 0 <= index < len(self.strings):
            return self.strings[index]
        return ""


def _parse_function(buf: bytes) -> Function:
    fn = Function(id=0)
    for field_number, wire, value in iter_fields(buf):
        if wire == WIRE_VARINT:
            if field_number == 1:
                fn.id = int(value)
            elif field_number == 2:
                fn.name = _decode_int64(int(value))
            elif field_number == 3:
                fn.system_name = _decode_int64(int(value))
            elif field_number == 4:
                fn.filename = _decode_int64(int(value))
    return fn


def _parse_location(buf: bytes) -> Location:
    loc = Location()
    for field_number, wire, value in iter_fields(buf):
        if wire == WIRE_VARINT and field_number == 1:
            loc.id = int(value)
        elif wire == WIRE_VARINT and field_number == 3:
            loc.address = int(value)
        elif wire == WIRE_LENGTH and field_number == 4:
            loc.lines.append(_parse_line(value))  # type: ignore[arg-type]
    return loc


def _parse_line(buf: bytes) -> Line:
    line = Line()
    for field_number, wire, value in iter_fields(buf):
        if wire == WIRE_VARINT and field_number == 1:
            line.function_id = int(value)
        elif wire == WIRE_VARINT and field_number == 2:
            line.line = _decode_int64(int(value))
    return line


def _parse_sample(buf: bytes) -> Sample:
    sample = Sample()
    for field_number, wire, value in iter_fields(buf):
        if field_number == 1:
            if wire == WIRE_VARINT:
                sample.location_ids.append(int(value))
            elif wire == WIRE_LENGTH:
                packed = _read_packed_or_single(value)  # type: ignore[arg-type]
                if packed:
                    sample.location_ids.extend(packed)
        elif field_number == 2:
            if wire == WIRE_VARINT:
                sample.values.append(_decode_int64(int(value)))
            elif wire == WIRE_LENGTH:
                packed = _read_packed_or_single(value)  # type: ignore[arg-type]
                if packed:
                    sample.values.extend(_decode_int64(v) for v in packed)
    return sample


def decode_profile(data: bytes) -> Profile:
    """Decode a raw or gzip-compressed pprof profile."""
    if data[:2] == b"\x1f\x8b":
        data = gzip.decompress(data)
    profile = Profile()
    for field_number, wire, value in iter_fields(data):
        if wire != WIRE_LENGTH:
            continue
        payload = value  # type: ignore[assignment]
        if field_number == 1:
            profile.sample_types.append(_parse_value_type(payload))  # type: ignore[arg-type]
        elif field_number == 2:
            profile.samples.append(_parse_sample(payload))  # type: ignore[arg-type]
        elif field_number == 4:
            loc = _parse_location(payload)  # type: ignore[arg-type]
            profile.locations[loc.id] = loc
        elif field_number == 5:
            fn = _parse_function(payload)  # type: ignore[arg-type]
            profile.functions[fn.id] = fn
        elif field_number == 6:
            profile.strings.append(payload.decode("utf-8", "replace"))  # type: ignore[union-attr]
    return profile


def _parse_value_type(buf: bytes) -> Tuple[int, int]:
    type_index = 0
    unit_index = 0
    for field_number, wire, value in iter_fields(buf):
        if wire == WIRE_VARINT and field_number == 1:
            type_index = _decode_int64(int(value))
        elif wire == WIRE_VARINT and field_number == 2:
            unit_index = _decode_int64(int(value))
    return type_index, unit_index


# ---------------------------------------------------------------------------
# Analysis
# ---------------------------------------------------------------------------

_HEX_ADDRESS = re.compile(r"^\s*0x[0-9a-fA-F]+\s*$")


@dataclass
class Analysis:
    sample_type_name: str
    sample_type_index: int
    total_bytes: int
    sample_count: int
    flat: Dict[str, int]
    cumulative: Dict[str, int]
    frames_total: int
    frames_unsymbolized: int

    @property
    def unsymbolized_ratio(self) -> float:
        if self.frames_total == 0:
            return 0.0
        return self.frames_unsymbolized / self.frames_total


def _find_inuse_space_index(profile: Profile) -> Tuple[int, str]:
    for index, (type_index, _unit_index) in enumerate(profile.sample_types):
        name = profile.string(type_index)
        if name == "inuse_space":
            return index, name
    for index, (type_index, _unit_index) in enumerate(profile.sample_types):
        name = profile.string(type_index)
        if "inuse" in name and "space" in name:
            return index, name
    if profile.sample_types:
        name = profile.string(profile.sample_types[0][0])
        return 0, name
    return 0, ""


def _frame_names(profile: Profile, location_id: int) -> List[str]:
    """Return function names for a location, leaf line first."""
    location = profile.locations.get(location_id)
    if location is None:
        return ["<unknown>"]
    names: List[str] = []
    for line in location.lines:
        fn = profile.functions.get(line.function_id)
        if fn is None or fn.name == 0:
            names.append("")
        else:
            names.append(profile.string(fn.name).strip())
    if not names:
        names.append("")
    return names


def _looks_unsymbolized(name: str) -> bool:
    if not name or name in ("<unknown>",):
        return True
    return bool(_HEX_ADDRESS.match(name))


def analyze(profile: Profile) -> Analysis:
    sample_type_index, sample_type_name = _find_inuse_space_index(profile)
    flat: Dict[str, int] = defaultdict(int)
    cumulative: Dict[str, int] = defaultdict(int)
    total_bytes = 0
    frames_total = 0
    frames_unsymbolized = 0

    for sample in profile.samples:
        if sample_type_index >= len(sample.values):
            continue
        value = sample.values[sample_type_index]
        if value == 0:
            continue
        total_bytes += value
        stack: List[str] = []
        for location_id in sample.location_ids:
            for name in _frame_names(profile, location_id):
                frames_total += 1
                if _looks_unsymbolized(name):
                    frames_unsymbolized += 1
                stack.append(name if name else "<unsymbolized>")
        if stack:
            # Flat: the leaf frame is the first location's first line.
            flat[stack[0]] += value
            # Cumulative: credit each distinct frame once per stack.
            for name in dict.fromkeys(stack):
                cumulative[name] += value

    return Analysis(
        sample_type_name=sample_type_name,
        sample_type_index=sample_type_index,
        total_bytes=total_bytes,
        sample_count=sum(1 for s in profile.samples if sample_type_index < len(s.values)),
        flat=dict(flat),
        cumulative=dict(cumulative),
        frames_total=frames_total,
        frames_unsymbolized=frames_unsymbolized,
    )


def load(path: str) -> Profile:
    with open(path, "rb") as handle:
        return decode_profile(handle.read())


def find_function(cumulative: Dict[str, int], name: str) -> Optional[Tuple[str, int]]:
    """Find a function by exact name, else by substring, preferring the largest.

    Symbol names differ between toolchains (``retain_validation_ballast`` vs
    ``memory_validation::retain_validation_ballast``), so an exact miss falls
    back to a substring match.
    """
    if name in cumulative:
        return name, cumulative[name]
    matches = [(key, value) for key, value in cumulative.items() if name in key]
    if not matches:
        return None
    return max(matches, key=lambda item: item[1])


# ---------------------------------------------------------------------------
# Rendering
# ---------------------------------------------------------------------------


def _human_bytes(value: int) -> str:
    sign = "-" if value < 0 else ""
    value = abs(value)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if value < 1024 or unit == "TiB":
            if unit == "B":
                return f"{sign}{value} {unit}"
            return f"{sign}{value:.1f} {unit}"
        value /= 1024
    return f"{sign}{value} B"


def print_analysis(analysis: Analysis, top: int) -> None:
    print(f"sample type:   {analysis.sample_type_name or '<none>'} (index {analysis.sample_type_index})")
    print(f"samples:       {analysis.sample_count}")
    print(f"total inuse:   {_human_bytes(analysis.total_bytes)} ({analysis.total_bytes} bytes)")
    print(f"unsymbolized:  {analysis.frames_unsymbolized}/{analysis.frames_total} frames "
          f"({analysis.unsymbolized_ratio:.1%})")

    print()
    print(f"top {top} flat by inuse space:")
    for name, value in sorted(analysis.flat.items(), key=lambda kv: kv[1], reverse=True)[:top]:
        print(f"  {_human_bytes(value):>12}  {name}")

    print()
    print(f"top {top} cumulative by inuse space:")
    for name, value in sorted(analysis.cumulative.items(), key=lambda kv: kv[1], reverse=True)[:top]:
        print(f"  {_human_bytes(value):>12}  {name}")


def print_diff(name_a: str, analysis_a: Analysis, name_b: str, analysis_b: Analysis, top: int) -> None:
    names = set(analysis_a.cumulative) | set(analysis_b.cumulative)
    rows = []
    for name in names:
        a = analysis_a.cumulative.get(name, 0)
        b = analysis_b.cumulative.get(name, 0)
        rows.append((name, a, b, b - a))
    rows.sort(key=lambda row: abs(row[3]), reverse=True)
    print(f"cumulative inuse-space diff: {name_a} -> {name_b}")
    print(f"  {'A':>12}  {'B':>12}  {'delta':>12}  function")
    for name, a, b, delta in rows[:top]:
        print(f"  {_human_bytes(a):>12}  {_human_bytes(b):>12}  {_human_bytes(delta):>12}  {name}")


# ---------------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------------


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("profile", nargs="?", help="pprof file to analyze")
    parser.add_argument("--top", type=int, default=20, help="rows to print (default 20)")
    parser.add_argument("--assert-function", metavar="NAME",
                        help="exit non-zero unless this function is present with enough cumulative bytes")
    parser.add_argument("--min-bytes", type=int, default=0,
                        help="minimum cumulative bytes for --assert-function")
    parser.add_argument("--diff", nargs=2, metavar=("A", "B"),
                        help="compare cumulative bytes between two profiles")
    return parser


def main(argv: Optional[Iterable[str]] = None) -> int:
    parser = build_parser()
    args = parser.parse_args(list(argv) if argv is not None else None)

    if args.diff:
        path_a, path_b = args.diff
        analysis_a = analyze(load(path_a))
        analysis_b = analyze(load(path_b))
        print_diff(path_a, analysis_a, path_b, analysis_b, args.top)
        return 0

    if not args.profile:
        parser.error("provide a profile or --diff A B")

    analysis = analyze(load(args.profile))
    print_analysis(analysis, args.top)

    if args.assert_function:
        match = find_function(analysis.cumulative, args.assert_function)
        if match is None:
            print(f"ASSERT FAILED: {args.assert_function} not found in the profile", file=sys.stderr)
            return 1
        key, found = match
        if found < args.min_bytes:
            print(
                f"ASSERT FAILED: {args.assert_function} cumulative inuse space "
                f"{found} bytes < required {args.min_bytes} bytes",
                file=sys.stderr,
            )
            return 1
        suffix = "" if key == args.assert_function else f" (matched {key})"
        print(
            f"ASSERT OK: {args.assert_function} cumulative inuse space "
            f"{found} bytes >= {args.min_bytes} bytes{suffix}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())

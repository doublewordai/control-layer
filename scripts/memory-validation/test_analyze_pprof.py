#!/usr/bin/env python3
"""Unit tests for the dependency-free pprof decoder in ``analyze_pprof.py``.

The test writes a tiny profile with a handwritten protobuf encoder, so the
decoder is exercised against a known stack attribution without needing a real
jemalloc profile. Run with:

    python3 -m unittest discover -s scripts/memory-validation -p 'test_*.py'
    # or, from the script directory:
    python3 test_analyze_pprof.py
"""

from __future__ import annotations

import gzip
import importlib.util
import io
import os
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout

_HERE = os.path.dirname(os.path.abspath(__file__))
_spec = importlib.util.spec_from_file_location("analyze_pprof", os.path.join(_HERE, "analyze_pprof.py"))
analyze_pprof = importlib.util.module_from_spec(_spec)
assert _spec.loader is not None
# Register before execution so dataclasses can resolve the module namespace.
sys.modules["analyze_pprof"] = analyze_pprof
_spec.loader.exec_module(analyze_pprof)


# --- handwritten protobuf encoder -----------------------------------------


def varint(value: int) -> bytes:
    out = bytearray()
    while True:
        byte = value & 0x7F
        value >>= 7
        if value:
            out.append(byte | 0x80)
        else:
            out.append(byte)
            return bytes(out)


def tag(field_number: int, wire_type: int) -> bytes:
    return varint((field_number << 3) | wire_type)


def v(field_number: int, value: int) -> bytes:
    return tag(field_number, 0) + varint(value)


def ld(field_number: int, payload: bytes) -> bytes:
    return tag(field_number, 2) + varint(len(payload)) + payload


STRINGS = [
    "",
    "inuse_space",
    "bytes",
    "main",
    "retain_validation_ballast",
    "worker",
    "0x7f0000000000",
]


def build_profile() -> bytes:
    out = bytearray()
    # sample_type { type: 1, unit: 2 }
    out += ld(1, v(1, 1) + v(2, 2))
    # function 1: retain_validation_ballast
    out += ld(5, v(1, 1) + v(2, 4))
    # function 2: worker
    out += ld(5, v(1, 2) + v(2, 5))
    # function 3: hex-address-looking name
    out += ld(5, v(1, 3) + v(2, 6))
    # location 10 -> function 1
    out += ld(4, v(1, 10) + ld(4, v(1, 1)))
    # location 11 -> function 2
    out += ld(4, v(1, 11) + ld(4, v(1, 2)))
    # location 12 -> function 3
    out += ld(4, v(1, 12) + ld(4, v(1, 3)))
    # location 13 -> no lines (unsymbolized)
    out += ld(4, v(1, 13))
    # sample { location_id: 10 value: 1000 }
    out += ld(2, v(1, 10) + v(2, 1000))
    # sample { location_id: 11, 10 value: 2000 }
    out += ld(2, v(1, 11) + v(1, 10) + v(2, 2000))
    # sample { location_id: 12 value: 500 }
    out += ld(2, v(1, 12) + v(2, 500))
    # sample { location_id: 13 value: 250 }
    out += ld(2, v(1, 13) + v(2, 250))
    # string_table
    for text in STRINGS:
        out += ld(6, text.encode("utf-8"))
    return bytes(out)


class DecoderTests(unittest.TestCase):
    def setUp(self) -> None:
        self.profile = analyze_pprof.decode_profile(build_profile())
        self.analysis = analyze_pprof.analyze(self.profile)

    def test_sample_type_and_totals(self) -> None:
        self.assertEqual(self.analysis.sample_type_name, "inuse_space")
        self.assertEqual(self.analysis.total_bytes, 1000 + 2000 + 500 + 250)
        self.assertEqual(self.analysis.sample_count, 4)

    def test_flat_attribution(self) -> None:
        self.assertEqual(self.analysis.flat.get("retain_validation_ballast"), 1000)
        self.assertEqual(self.analysis.flat.get("worker"), 2000)
        self.assertEqual(self.analysis.flat.get("0x7f0000000000"), 500)

    def test_cumulative_attribution(self) -> None:
        self.assertEqual(self.analysis.cumulative.get("retain_validation_ballast"), 3000)
        self.assertEqual(self.analysis.cumulative.get("worker"), 2000)

    def test_unsymbolized_ratio(self) -> None:
        # One hex-address frame and one empty (no-line) frame out of five.
        self.assertEqual(self.analysis.frames_total, 5)
        self.assertEqual(self.analysis.frames_unsymbolized, 2)
        self.assertAlmostEqual(self.analysis.unsymbolized_ratio, 2 / 5)

    def test_assert_function_substring(self) -> None:
        # Symbol names are often qualified; the assertion should still match.
        match = analyze_pprof.find_function(self.analysis.cumulative, "retain_validation")
        self.assertIsNotNone(match)
        key, value = match
        self.assertIn("retain_validation_ballast", key)
        self.assertEqual(value, 3000)

    def test_gzip_round_trip(self) -> None:
        compressed = gzip.compress(build_profile())
        decoded = analyze_pprof.decode_profile(compressed)
        analysis = analyze_pprof.analyze(decoded)
        self.assertEqual(analysis.cumulative.get("retain_validation_ballast"), 3000)

    def test_assert_function_cli(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "profile.pb.gz")
            with open(path, "wb") as handle:
                handle.write(gzip.compress(build_profile()))

            stdout, stderr = io.StringIO(), io.StringIO()
            with redirect_stdout(stdout), redirect_stderr(stderr):
                ok = analyze_pprof.main([path, "--assert-function", "retain_validation_ballast", "--min-bytes", "3000"])
            self.assertEqual(ok, 0, stderr.getvalue())

            with redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                failed = analyze_pprof.main([path, "--assert-function", "retain_validation_ballast", "--min-bytes", "3001"])
            self.assertEqual(failed, 1)

    def test_diff_cli(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "profile.pb.gz")
            with open(path, "wb") as handle:
                handle.write(gzip.compress(build_profile()))
            stdout = io.StringIO()
            with redirect_stdout(stdout):
                code = analyze_pprof.main(["--diff", path, path, "--top", "5"])
            self.assertEqual(code, 0)
            self.assertIn("retain_validation_ballast", stdout.getvalue())


if __name__ == "__main__":
    # Small escape hatch so the Pyroscope smoke test can build a pprof without a
    # Go toolchain: `test_analyze_pprof.py --emit-test-profile out.pb.gz`.
    if len(sys.argv) >= 3 and sys.argv[1] == "--emit-test-profile":
        with open(sys.argv[2], "wb") as handle:
            handle.write(gzip.compress(build_profile()))
        print(sys.argv[2])
    else:
        unittest.main()

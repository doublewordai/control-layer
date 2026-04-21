"""
Exploratory test for the Open Responses API with service_tier and background modes.

Tests all 4 execution modes:
1. priority + background=False (realtime, blocking)
2. priority + background=True  (realtime, non-blocking)
3. flex + background=True      (async, non-blocking)
4. flex + background=False     (async, blocking)

Also tests default/auto tiers (should behave like realtime).
"""

from openai import OpenAI
from time import sleep
import sys

client = OpenAI(
    api_key="sk-ZEI9NJyfkPdFgjiYS5cEgNab3GflUd22HEQjdAG8hn0",
    base_url="http://localhost:3001/ai/v1",
)

MODEL = "Qwen/Qwen3.5-397B-A17B-FP8"
INPUT = "What is 2+2? Reply in one word."


def poll_until_done(resp, label: str, max_wait: int = 120):
    """Poll a background response until terminal."""
    elapsed = 0
    while resp.status in ("queued", "in_progress"):
        print(f"  [{label}] status={resp.status} (waited {elapsed}s)")
        sleep(2)
        elapsed += 2
        if elapsed > max_wait:
            print(f"  [{label}] TIMEOUT after {max_wait}s")
            return resp
        resp = client.responses.retrieve(resp.id)
    return resp


def test_realtime_blocking():
    """priority + background=False — should return completed response directly."""
    print("\n=== Test 1: priority + background=False (realtime, blocking) ===")
    try:
        resp = client.responses.create(
            model=MODEL,
            input=INPUT,
            service_tier="priority",
        )
        print(f"  Status: {resp.status}")
        print(f"  ID: {resp.id}")
        print(f"  Output: {resp.output_text[:200] if hasattr(resp, 'output_text') and resp.output_text else resp.output}")
        print(f"  PASS")
    except Exception as e:
        print(f"  FAIL: {e}")


def test_realtime_background():
    """priority + background=True — should return 202, then poll."""
    print("\n=== Test 2: priority + background=True (realtime, non-blocking) ===")
    try:
        resp = client.responses.create(
            model=MODEL,
            input=INPUT,
            service_tier="priority",
            background=True,
        )
        print(f"  Initial status: {resp.status}")
        print(f"  ID: {resp.id}")

        resp = poll_until_done(resp, "priority+bg")
        print(f"  Final status: {resp.status}")
        print(f"  Output: {resp.output_text[:200] if hasattr(resp, 'output_text') and resp.output_text else resp.output}")
        print(f"  PASS")
    except Exception as e:
        print(f"  FAIL: {e}")


def test_flex_background():
    """flex + background=True — should return 202 with queued, daemon processes."""
    print("\n=== Test 3: flex + background=True (async, non-blocking) ===")
    try:
        resp = client.responses.create(
            model=MODEL,
            input=INPUT,
            service_tier="flex",
            background=True,
        )
        print(f"  Initial status: {resp.status}")
        print(f"  ID: {resp.id}")

        resp = poll_until_done(resp, "flex+bg")
        print(f"  Final status: {resp.status}")
        print(f"  Output: {resp.output_text[:200] if hasattr(resp, 'output_text') and resp.output_text else resp.output}")
        print(f"  PASS")
    except Exception as e:
        print(f"  FAIL: {e}")


def test_flex_blocking():
    """flex + background=False — should hold connection until daemon completes."""
    print("\n=== Test 4: flex + background=False (async, blocking) ===")
    try:
        resp = client.responses.create(
            model=MODEL,
            input=INPUT,
            service_tier="flex",
        )
        print(f"  Status: {resp.status}")
        print(f"  ID: {resp.id}")
        print(f"  Output: {resp.output_text[:200] if hasattr(resp, 'output_text') and resp.output_text else resp.output}")
        print(f"  PASS")
    except Exception as e:
        print(f"  FAIL: {e}")


def test_default_blocking():
    """default (no service_tier) + background=False — should behave like realtime."""
    print("\n=== Test 5: default + background=False (realtime, blocking) ===")
    try:
        resp = client.responses.create(
            model=MODEL,
            input=INPUT,
        )
        print(f"  Status: {resp.status}")
        print(f"  ID: {resp.id}")
        print(f"  Output: {resp.output_text[:200] if hasattr(resp, 'output_text') and resp.output_text else resp.output}")
        print(f"  PASS")
    except Exception as e:
        print(f"  FAIL: {e}")


def test_auto_background():
    """auto + background=True — should behave like realtime background."""
    print("\n=== Test 6: auto + background=True (realtime, non-blocking) ===")
    try:
        resp = client.responses.create(
            model=MODEL,
            input=INPUT,
            service_tier="auto",
            background=True,
        )
        print(f"  Initial status: {resp.status}")
        print(f"  ID: {resp.id}")

        resp = poll_until_done(resp, "auto+bg")
        print(f"  Final status: {resp.status}")
        print(f"  Output: {resp.output_text[:200] if hasattr(resp, 'output_text') and resp.output_text else resp.output}")
        print(f"  PASS")
    except Exception as e:
        print(f"  FAIL: {e}")


def test_retrieve_by_id():
    """Test that GET /v1/responses/{id} works for a completed realtime request."""
    print("\n=== Test 7: Retrieve by ID ===")
    try:
        # First create a realtime request
        resp = client.responses.create(
            model=MODEL,
            input=INPUT,
            service_tier="priority",
        )
        resp_id = resp.id
        print(f"  Created: {resp_id}")

        # Now retrieve it
        sleep(1)  # Give outlet handler time to write
        retrieved = client.responses.retrieve(resp_id)
        print(f"  Retrieved status: {retrieved.status}")
        print(f"  Retrieved ID: {retrieved.id}")
        print(f"  PASS")
    except Exception as e:
        print(f"  FAIL: {e}")


if __name__ == "__main__":
    # Run specific test if argument provided, otherwise run all
    tests = {
        "1": test_realtime_blocking,
        "2": test_realtime_background,
        "3": test_flex_background,
        "4": test_flex_blocking,
        "5": test_default_blocking,
        "6": test_auto_background,
        "7": test_retrieve_by_id,
    }

    if len(sys.argv) > 1:
        for arg in sys.argv[1:]:
            if arg in tests:
                tests[arg]()
            else:
                print(f"Unknown test: {arg}. Available: {', '.join(tests.keys())}")
    else:
        for test_fn in tests.values():
            test_fn()

    print("\n=== Done ===")

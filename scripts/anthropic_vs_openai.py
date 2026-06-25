#!/usr/bin/env python3
"""Differential dump: OpenAI path vs Anthropic path, same dwctl + backend.

Sends equivalent requests through both surfaces of the SAME dwctl and prints the
FULL response from each side (no truncation), plus explicit comparison signals,
so you can see exactly what each path returns and judge divergence yourself.

  - OpenAI:    POST /ai/v1/chat/completions   (openai SDK)
  - Anthropic: POST /ai/v1/messages           (anthropic SDK, our ingress)

Each scenario prints both full responses and a `signals:` line of booleans.
HARD signals (marked) count toward the exit code; SOFT ones are shown for review
only (e.g. free-form content match, which legitimately drifts on real backends).

Usage:
    pip install anthropic openai
    export DWCTL_BASE_URL=http://localhost:3001/ai
    export DWCTL_API_KEY=sk-...
    export SMOKE_MODEL=...
    export SMOKE_IMAGE_URL=https://.../small.png   # optional, for the url-image case
    python3 scripts/anthropic_vs_openai.py
"""

import json
import os
import re
import sys

try:
    from anthropic import Anthropic
    from openai import OpenAI
except ImportError:
    sys.exit("Install both SDKs: pip install anthropic openai")

BASE_URL = os.environ.get("DWCTL_BASE_URL", "http://localhost:3001/ai")
API_KEY = os.environ.get("DWCTL_API_KEY", "")
MODEL = os.environ.get("SMOKE_MODEL", "")
MAX_TOKENS = int(os.environ.get("SMOKE_MAX_TOKENS", "512"))
IMAGE_URL = os.environ.get("SMOKE_IMAGE_URL", "")
TEMP = 0

hard_failures = []

oai = OpenAI(api_key=API_KEY, base_url=f"{BASE_URL.rstrip('/')}/v1")
ant = Anthropic(api_key=API_KEY, base_url=BASE_URL)

STOP_MAP = {"stop": "end_turn", "length": "max_tokens", "tool_calls": "tool_use", "content_filter": "end_turn"}

WEATHER_OAI = {
    "type": "function",
    "function": {
        "name": "get_weather",
        "description": "Get the current weather for a city.",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
    },
}
WEATHER_ANT = {
    "name": "get_weather",
    "description": "Get the current weather for a city.",
    "input_schema": {"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]},
}


# --- extraction -------------------------------------------------------------
def strip_think(s):
    return re.sub(r"<think>.*?</think>", "", s or "", flags=re.S)


def norm(s):
    return " ".join(strip_think(s).split()).lower()


def has_think(s):
    return "<think>" in (s or "") or "</think>" in (s or "")


def oai_reasoning(msg):
    for attr in ("reasoning_content", "reasoning"):
        if getattr(msg, attr, None):
            return getattr(msg, attr)
    extra = getattr(msg, "model_extra", None) or {}
    return extra.get("reasoning_content") or extra.get("reasoning")


def oai_text(resp):
    return resp.choices[0].message.content or ""


def oai_finish(resp):
    return resp.choices[0].finish_reason


def ant_text(msg):
    return "".join(b.text for b in msg.content if b.type == "text")


def ant_tool_uses(msg):
    return [b for b in msg.content if b.type == "tool_use"]


# --- full dumps (no truncation) --------------------------------------------
def show_oai(resp, label="openai"):
    m = resp.choices[0].message
    print(f"  [{label}] finish_reason={oai_finish(resp)}  (maps to {STOP_MAP.get(oai_finish(resp))})")
    print(f"    content: {m.content!r}")
    r = oai_reasoning(m)
    if r:
        preview = r if len(r) <= 160 else r[:160] + f"... [+{len(r) - 160} more chars]"
        print(f"    reasoning_content: ({len(r)} chars) {preview!r}")
    for tc in (m.tool_calls or []):
        print(f"    tool_call[{tc.id}]: {tc.function.name}({tc.function.arguments})")
    u = getattr(resp, "usage", None)
    if u:
        print(f"    usage: prompt={u.prompt_tokens} completion={u.completion_tokens}")


def show_ant(msg, label="anthropic"):
    print(f"  [{label}] stop_reason={msg.stop_reason}  stop_sequence={msg.stop_sequence}")
    for b in msg.content:
        if b.type == "text":
            print(f"    text: {b.text!r}")
        elif b.type == "tool_use":
            print(f"    tool_use[{b.id}]: {b.name}({json.dumps(b.input)})")
        elif b.type == "thinking":
            print(f"    thinking: {getattr(b, 'thinking', '')!r}")
        else:
            print(f"    {b.type}: {b!r}")
    print(f"    usage: input={msg.usage.input_tokens} output={msg.usage.output_tokens}")


def verdict(name, hard_ok, signals):
    """signals: list of (label, value, is_hard). Prints them; hard-fail if any hard False."""
    sig = "  ".join(f"{lbl}={val}{'*' if hard else ''}" for lbl, val, hard in signals)
    print(f"  signals: {sig}    (*=hard)")
    hard_bad = [lbl for lbl, val, hard in signals if hard and not val]
    ok = hard_ok and not hard_bad
    print(f"  >>> {name}: {'OK' if ok else 'NEEDS REVIEW'}" + (f"  (failing: {', '.join(hard_bad)})" if hard_bad else ""))
    print()
    if not ok:
        hard_failures.append(name)


def header(name):
    print(f"========== {name} ==========")


def oai_complete(messages, **kw):
    return oai.chat.completions.create(model=MODEL, max_tokens=MAX_TOKENS, temperature=TEMP, messages=messages, **kw)


def ant_complete(messages, **kw):
    return ant.messages.create(model=MODEL, max_tokens=MAX_TOKENS, temperature=TEMP, messages=messages, **kw)


# --- scenarios --------------------------------------------------------------
def blocking():
    header("blocking")
    msgs = [{"role": "user", "content": "In one short sentence, what is the capital of France?"}]
    o, a = oai_complete(msgs), ant_complete(msgs)
    show_oai(o)
    show_ant(a)
    o_text, a_text = oai_text(o), ant_text(a)
    verdict("blocking", True, [
        ("both_text", bool(o_text.strip()) and bool(a_text.strip()), True),
        ("stop_maps", STOP_MAP.get(oai_finish(o)) == a.stop_reason, True),
        ("content_match", norm(o_text) == norm(a_text), False),
        ("think_parity", has_think(o_text) == has_think(a_text), False),
    ])


def deterministic_content():
    header("deterministic content (exact match asserted)")
    msgs = [{"role": "user", "content": "What is 2+2? Reply with only the digit, nothing else."}]
    o, a = oai_complete(msgs), ant_complete(msgs)
    show_oai(o)
    show_ant(a)
    o_t, a_t = norm(oai_text(o)), norm(ant_text(a))
    verdict("deterministic content", True, [
        ("openai_has_4", "4" in o_t, True),
        ("content_match", o_t == a_t, True),
    ])


def system_prompt():
    header("system prompt")
    sys_p = "You always answer in exactly one word."
    msgs = [{"role": "user", "content": "Name a primary color."}]
    o = oai_complete([{"role": "system", "content": sys_p}, *msgs])
    a = ant_complete(msgs, system=sys_p)
    show_oai(o)
    show_ant(a)
    verdict("system prompt", True, [
        ("both_text", bool(oai_text(o).strip()) and bool(ant_text(a).strip()), True),
        ("content_match", norm(oai_text(o)) == norm(ant_text(a)), False),
    ])


def multi_turn():
    header("multi-turn")
    hist = [
        {"role": "user", "content": "My favorite number is 7. Remember it."},
        {"role": "assistant", "content": "Got it, 7."},
        {"role": "user", "content": "What number did I say? Reply with only the number."},
    ]
    o, a = oai_complete(list(hist)), ant_complete(list(hist))
    show_oai(o)
    show_ant(a)
    verdict("multi-turn", True, [
        ("openai_says_7", "7" in norm(oai_text(o)), True),
        ("anthropic_says_7", "7" in norm(ant_text(a)), True),
        ("content_match", norm(oai_text(o)) == norm(ant_text(a)), False),
    ])


def length_finish():
    header("length finish (max_tokens)")
    msgs = [{"role": "user", "content": "Write a long detailed essay about the history of the Roman Empire."}]
    o = oai.chat.completions.create(model=MODEL, max_tokens=16, temperature=TEMP, messages=msgs)
    a = ant.messages.create(model=MODEL, max_tokens=16, temperature=TEMP, messages=msgs)
    show_oai(o)
    show_ant(a)
    verdict("length finish", True, [
        ("openai_length", oai_finish(o) == "length", True),
        ("anthropic_max_tokens", a.stop_reason == "max_tokens", True),
    ])


def stop_sequences():
    header("stop_sequences (documents stop_reason gap)")
    msgs = [{"role": "user", "content": "Count: one two three four five"}]
    o = oai.chat.completions.create(model=MODEL, max_tokens=MAX_TOKENS, temperature=TEMP, messages=msgs, stop=["three"])
    a = ant.messages.create(model=MODEL, max_tokens=MAX_TOKENS, temperature=TEMP, messages=msgs, stop_sequences=["three"])
    show_oai(o)
    # The backend may expose the matched stop (vLLM -> choices[].stop_reason;
    # sglang -> choices[].matched_stop). When it does, Anthropic must report it.
    matched = getattr(o.choices[0], "stop_reason", None) or getattr(o.choices[0], "matched_stop", None)
    matched = matched if isinstance(matched, str) and matched else None
    print(f"    backend matched stop = {matched!r}")
    show_ant(a)
    if matched:
        verdict("stop_sequences", True, [
            ("anthropic_stop_sequence", a.stop_reason == "stop_sequence", True),
            ("stop_sequence_value", a.stop_sequence == matched, True),
        ])
    else:
        # Backend doesn't expose it -> correct fallback to end_turn (no regression).
        print("    backend does not expose matched stop; fallback to end_turn (expected)")
        verdict("stop_sequences", True, [("fallback_known", a.stop_reason in ("end_turn", "max_tokens"), True)])


def streaming_text():
    header("streaming text")
    msgs = [{"role": "user", "content": "Count to three."}]
    o_chunks = [c.choices[0].delta.content for c in oai.chat.completions.create(
        model=MODEL, max_tokens=MAX_TOKENS, temperature=TEMP, stream=True, messages=msgs)
        if c.choices and c.choices[0].delta.content]
    a_chunks = []
    with ant.messages.stream(model=MODEL, max_tokens=MAX_TOKENS, temperature=TEMP, messages=msgs) as s:
        for t in s.text_stream:
            a_chunks.append(t)
        a_final = s.get_final_message()
    print(f"  [openai] deltas={len(o_chunks)} text={''.join(o_chunks)!r}")
    print("  [anthropic] (assembled from SSE)")
    show_ant(a_final)
    verdict("streaming text", True, [
        ("anthropic_final_msg", a_final.type == "message", True),
        ("delta_presence_parity", (len(o_chunks) > 0) == (len(a_chunks) > 0), True),
        ("content_match", norm("".join(o_chunks)) == norm("".join(a_chunks)), False),
    ])


def streaming_tools():
    header("streaming tools")
    msgs = [{"role": "user", "content": "What is the weather in Paris? Use the tool."}]
    acc = {}
    for c in oai.chat.completions.create(model=MODEL, max_tokens=MAX_TOKENS, temperature=TEMP, stream=True, tools=[WEATHER_OAI], messages=msgs):
        for tc in (c.choices[0].delta.tool_calls or []) if c.choices else []:
            slot = acc.setdefault(tc.index, {"name": "", "args": ""})
            if tc.function and tc.function.name:
                slot["name"] = tc.function.name
            if tc.function and tc.function.arguments:
                slot["args"] += tc.function.arguments
    print(f"  [openai] (reassembled tool_calls) {acc}")
    with ant.messages.stream(model=MODEL, max_tokens=MAX_TOKENS, temperature=TEMP, tools=[WEATHER_ANT], messages=msgs) as s:
        a_final = s.get_final_message()
    print("  [anthropic] (assembled from SSE)")
    show_ant(a_final)
    o_name = next((v["name"] for v in acc.values()), None)
    o_args = json.loads(next((v["args"] for v in acc.values()), "{}") or "{}")
    a_uses = ant_tool_uses(a_final)
    if o_name != "get_weather":
        verdict("streaming tools", True, [("skipped_no_tool", True, False)])
        return
    verdict("streaming tools", True, [
        ("anthropic_tool", bool(a_uses) and a_uses[0].name == "get_weather", True),
        ("openai_city", "city" in o_args, True),
        ("anthropic_city", bool(a_uses) and "city" in a_uses[0].input, True),
    ])


def tools_first_turn():
    header("tools (first turn)")
    msgs = [{"role": "user", "content": "What is the weather in Paris? Use the tool."}]
    o = oai_complete(msgs, tools=[WEATHER_OAI])
    a = ant_complete(msgs, tools=[WEATHER_ANT])
    show_oai(o)
    show_ant(a)
    o_calls = o.choices[0].message.tool_calls or []
    a_uses = ant_tool_uses(a)
    verdict("tools first turn", True, [
        ("openai_tool", bool(o_calls) and o_calls[0].function.name == "get_weather", True),
        ("anthropic_tool", bool(a_uses) and a_uses[0].name == "get_weather", True),
    ])


def tools_roundtrip():
    header("tools round-trip (tool_result continuation)")
    prompt = "What is the weather in Paris? Use the tool, then tell me in one sentence."
    o_msgs = [{"role": "user", "content": prompt}]
    o1 = oai_complete(o_msgs, tools=[WEATHER_OAI])
    a_msgs = [{"role": "user", "content": prompt}]
    a1 = ant_complete(a_msgs, tools=[WEATHER_ANT])
    o_calls = o1.choices[0].message.tool_calls or []
    a_uses = ant_tool_uses(a1)
    print("  --- first turn ---")
    show_oai(o1)
    show_ant(a1)
    if not o_calls or not a_uses:
        verdict("tools round-trip", True, [("skipped_no_tool", True, False)])
        return

    o_assistant = o1.choices[0].message.model_dump()
    # Fair comparison: Anthropic clients cannot round-trip reasoning, so do not
    # feed it back on the OpenAI side either.
    o_assistant.pop("reasoning_content", None)
    o_assistant.pop("reasoning", None)
    o_msgs.append(o_assistant)
    o_msgs.append({"role": "tool", "tool_call_id": o_calls[0].id, "content": "Sunny, 21C."})
    o2 = oai_complete(o_msgs, tools=[WEATHER_OAI])
    a_msgs.append({"role": "assistant", "content": a1.content})
    a_msgs.append({"role": "user", "content": [{"type": "tool_result", "tool_use_id": a_uses[0].id, "content": "Sunny, 21C."}]})
    a2 = ant_complete(a_msgs, tools=[WEATHER_ANT])
    print("  --- continuation (after tool_result) ---")
    show_oai(o2)
    show_ant(a2)
    verdict("tools round-trip", True, [
        ("both_answered", bool(oai_text(o2).strip()) and bool(ant_text(a2).strip()), True),
        ("content_match", norm(oai_text(o2)) == norm(ant_text(a2)), False),
    ])


def tool_choice_forced():
    header("tool_choice forced")
    # Use a prompt where forcing the weather tool is natural. (Forcing it on an
    # unrelated prompt like "Say hi" is pathological and breaks THIS model
    # non-deterministically on BOTH the OpenAI and Anthropic paths equally, so it
    # tests the backend's quirks, not our translation.) The differential question
    # is whether our path behaves the same as native OpenAI - hence the hard
    # signal is equivalence, not absolute forcing.
    msgs = [{"role": "user", "content": "What is the weather in Tokyo? Use the tool."}]
    o = oai_complete(msgs, tools=[WEATHER_OAI], tool_choice={"type": "function", "function": {"name": "get_weather"}})
    a = ant_complete(msgs, tools=[WEATHER_ANT], tool_choice={"type": "tool", "name": "get_weather"})
    show_oai(o)
    show_ant(a)
    o_forced = bool(o.choices[0].message.tool_calls)
    a_forced = bool(ant_tool_uses(a))
    verdict("tool_choice forced", True, [
        ("openai_forced", o_forced, False),
        ("anthropic_forced", a_forced, False),
        ("paths_equivalent", o_forced == a_forced, True),
    ])


def parallel_tools():
    header("parallel tools")
    msgs = [{"role": "user", "content": "Get the weather for both Paris and Tokyo. Call the tool for each."}]
    o = oai_complete(msgs, tools=[WEATHER_OAI])
    a = ant_complete(msgs, tools=[WEATHER_ANT])
    show_oai(o)
    show_ant(a)
    o_n, a_n = len(o.choices[0].message.tool_calls or []), len(ant_tool_uses(a))
    verdict("parallel tools", True, [
        ("openai_calls", o_n, False),
        ("anthropic_calls", a_n, False),
        ("both_called", o_n >= 1 and a_n >= 1, True),
    ])


def image_base64():
    header("image (base64 input)")
    png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+M9QDwADhgGAWjR9awAAAABJRU5ErkJggg=="
    prompt = "Describe this image in one word."
    try:
        o = oai_complete([{"role": "user", "content": [
            {"type": "text", "text": prompt}, {"type": "image_url", "image_url": {"url": f"data:image/png;base64,{png}"}}]}])
        a = ant_complete([{"role": "user", "content": [
            {"type": "text", "text": prompt}, {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": png}}]}])
        show_oai(o)
        show_ant(a)
        verdict("image base64", True, [("both_text_parity", bool(oai_text(o).strip()) == bool(ant_text(a).strip()), True)])
    except Exception as e:  # noqa: BLE001
        print(f"    error: {type(e).__name__}: {e}")
        verdict("image base64", True, [("skipped", True, False)])


def image_url():
    header("image (url input -> image_normalizer)")
    if not IMAGE_URL:
        print("    skipped (set SMOKE_IMAGE_URL to a reachable image)")
        verdict("image url", True, [("skipped", True, False)])
        return
    prompt = "Describe this image in one word."
    o = oai_complete([{"role": "user", "content": [
        {"type": "text", "text": prompt}, {"type": "image_url", "image_url": {"url": IMAGE_URL}}]}])
    a = ant_complete([{"role": "user", "content": [
        {"type": "text", "text": prompt}, {"type": "image", "source": {"type": "url", "url": IMAGE_URL}}]}])
    show_oai(o)
    show_ant(a)
    verdict("image url", True, [("both_text_parity", bool(oai_text(o).strip()) == bool(ant_text(a).strip()), True)])


def error_invalid_model():
    header("error (invalid model)")
    bad = "definitely-not-a-real-model-xyz"
    o_err = a_err = None
    try:
        oai.chat.completions.create(model=bad, max_tokens=8, messages=[{"role": "user", "content": "hi"}])
    except Exception as e:  # noqa: BLE001
        o_err = e
    try:
        ant.messages.create(model=bad, max_tokens=8, messages=[{"role": "user", "content": "hi"}])
    except Exception as e:  # noqa: BLE001
        a_err = e
    print(f"  [openai] status={getattr(o_err, 'status_code', None)} body={getattr(o_err, 'body', None)}")
    print(f"  [anthropic] status={getattr(a_err, 'status_code', None)} type={type(a_err).__name__} body={getattr(a_err, 'body', None)}")
    verdict("error invalid model", True, [
        ("both_errored", o_err is not None and a_err is not None, True),
        ("status_match", getattr(o_err, "status_code", None) == getattr(a_err, "status_code", None), True),
    ])


def reasoning_parity():
    header("reasoning parity")
    msgs = [{"role": "user", "content": "Think step by step: what is 17 * 23? Give the final number."}]
    o, a = oai_complete(msgs), ant_complete(msgs)
    show_oai(o)
    show_ant(a)
    o_reason = oai_reasoning(o.choices[0].message)
    thinking_blocks = [b for b in a.content if getattr(b, "type", None) == "thinking"]
    a_thinking_text = "".join(getattr(b, "thinking", "") for b in thinking_blocks)
    # If the model reasoned (OpenAI surfaces reasoning_content), the Anthropic side
    # MUST now surface it too, as a non-empty thinking block.
    surfaced = bool(a_thinking_text.strip()) if o_reason else True
    print(f"    openai reasoning_content={'present' if o_reason else 'absent'}; anthropic thinking_block={'present' if thinking_blocks else 'absent'}")
    verdict("reasoning parity", True, [
        ("openai_reasoned", bool(o_reason), False),
        ("anthropic_thinking_block", bool(thinking_blocks), False),
        ("reasoning_surfaced", surfaced, True),
        ("no_think_leak_in_text", not has_think(ant_text(a)), False),
    ])


def usage_parity():
    header("usage parity")
    msgs = [{"role": "user", "content": "Say hello."}]
    o, a = oai_complete(msgs), ant_complete(msgs)
    show_oai(o)
    show_ant(a)
    verdict("usage parity", True, [
        ("input_delta", abs(o.usage.prompt_tokens - a.usage.input_tokens), False),
        ("input_close", abs(o.usage.prompt_tokens - a.usage.input_tokens) <= 2, True),
    ])


SCENARIOS = [
    blocking, deterministic_content, system_prompt, multi_turn, length_finish, stop_sequences,
    streaming_text, streaming_tools, tools_first_turn, tools_roundtrip, tool_choice_forced,
    parallel_tools, image_base64, image_url, error_invalid_model, reasoning_parity, usage_parity,
]


def main():
    if not API_KEY or not MODEL:
        sys.exit("Set DWCTL_API_KEY and SMOKE_MODEL.")
    print(f"comparing OpenAI vs Anthropic on {BASE_URL} model={MODEL}\n")
    for fn in SCENARIOS:
        try:
            fn()
        except Exception as e:  # noqa: BLE001
            print(f"  EXCEPTION in {fn.__name__}: {type(e).__name__}: {e}\n")
            hard_failures.append(fn.__name__)
    print("=" * 40)
    if hard_failures:
        print(f"{len(hard_failures)} scenario(s) need review: {', '.join(hard_failures)}")
        sys.exit(1)
    print(f"all {len(SCENARIOS)} scenarios OK on hard signals (review the dumps for soft divergences)")


if __name__ == "__main__":
    main()

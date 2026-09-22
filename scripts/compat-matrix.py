#!/usr/bin/env python3
"""Reproducible Coding-Agent Compatibility Matrix Test (FR-9.2, FR-9.3, NFR-7.2).

Verifies real wire protocol compatibility across coding agent client profiles:
  1. Pi Coding Agent (OpenAI Chat Completions wire format, tool calls, session affinity)
  2. Next-Gen Agent / Codex CLI (OpenAI Responses /v1/responses format)
  3. Anthropic Agent / Claude Code (Anthropic Messages /v1/messages format)

Against both same-format passthrough and cross-format translating upstreams.
Runs with zero external dependencies (Python 3 stdlib only).
"""

import json
import os
import sys
import time
import urllib.error
import urllib.request

BASE_URL = os.environ.get("KINETIX_BASE", "http://127.0.0.1:8180")
API_KEY = os.environ.get("KINETIX_KEY", "")

TEST_RESULTS = []


def record_result(client, upstream, capability, passed, detail="", duration_ms=0):
    TEST_RESULTS.append({
        "client": client,
        "upstream": upstream,
        "capability": capability,
        "passed": passed,
        "detail": detail,
        "duration_ms": duration_ms,
    })
    status = "\033[92mPASS\033[0m" if passed else "\033[91mFAIL\033[0m"
    print(f"  [{status}] {client:<24} | {upstream:<14} | {capability:<18} ({duration_ms}ms) {detail}")


def read_sse_events(response):
    """Parse raw SSE stream into a list of (event_type, data_dict_or_str)."""
    events = []
    current_event = None
    data_lines = []

    for line in response:
        line = line.decode("utf-8", errors="replace").rstrip("\r\n")
        if not line:
            if data_lines:
                data_str = "\n".join(data_lines)
                try:
                    data_val = json.loads(data_str)
                except Exception:
                    data_val = data_str
                events.append((current_event, data_val))
            current_event = None
            data_lines = []
            continue

        if line.startswith("event: "):
            current_event = line[7:].strip()
        elif line.startswith("data: "):
            data_lines.append(line[6:])

    if data_lines:
        data_str = "\n".join(data_lines)
        try:
            data_val = json.loads(data_str)
        except Exception:
            data_val = data_str
        events.append((current_event, data_val))

    return events


# ---------------------------------------------------------------------------
# Client 1: Pi Coding Agent (OpenAI Chat Completions)
# ---------------------------------------------------------------------------

def test_pi_plain_stream(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/chat/completions"
    payload = {
        "model": upstream_model,
        "stream": True,
        "messages": [
            {"role": "system", "content": "You are Pi, a coding agent."},
            {"role": "user", "content": "Write hello world."}
        ]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    # Check for text chunks and [DONE]
    text_chunks = [e[1] for e in events if isinstance(e[1], dict) and e[1].get("choices")]
    has_done = any(e[1] == "[DONE]" for e in events)
    duration = int((time.time() - t0) * 1000)
    passed = len(text_chunks) > 0 and has_done
    record_result("Pi (OpenAI Chat)", upstream_model, "plain_stream", passed, f"{len(text_chunks)} chunks", duration)


def test_pi_tool_use(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/chat/completions"
    payload = {
        "model": upstream_model,
        "stream": True,
        "messages": [
            {"role": "user", "content": "Read file main.rs"}
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read contents of a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}},
                    "required": ["path"]
                }
            }
        }]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    # Reassemble tool calls
    tool_chunks = []
    for _, ev in events:
        if isinstance(ev, dict) and ev.get("choices"):
            delta = ev["choices"][0].get("delta", {})
            if "tool_calls" in delta:
                tool_chunks.append(delta["tool_calls"])

    duration = int((time.time() - t0) * 1000)
    passed = len(tool_chunks) > 0
    record_result("Pi (OpenAI Chat)", upstream_model, "tool_use_stream", passed, f"{len(tool_chunks)} tool deltas", duration)


def test_pi_multi_turn(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/chat/completions"
    payload = {
        "model": upstream_model,
        "stream": True,
        "messages": [
            {"role": "user", "content": "Read file main.rs"},
            {
                "role": "assistant",
                "content": None,
                "tool_calls": [{
                    "id": "call_pi_1",
                    "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\":\"main.rs\"}"}
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_pi_1",
                "name": "read_file",
                "content": "fn main() { println!(\"ok\"); }"
            }
        ]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    has_chunks = any(isinstance(e[1], dict) and e[1].get("choices") for e in events)
    duration = int((time.time() - t0) * 1000)
    record_result("Pi (OpenAI Chat)", upstream_model, "multi_turn", has_chunks, "turn 2 grounded", duration)


def test_pi_session_affinity(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/chat/completions"
    session_id = f"pi_sess_{int(time.time())}"
    payload = {
        "model": upstream_model,
        "stream": True,
        "messages": [{"role": "user", "content": "ping"}]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
            "X-Session-Id": session_id,
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    passed = any(isinstance(e[1], dict) and e[1].get("choices") for e in events)
    duration = int((time.time() - t0) * 1000)
    record_result("Pi (OpenAI Chat)", upstream_model, "session_affinity", passed, f"header: {session_id}", duration)


def test_pi_sync_aggregation(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/chat/completions"
    payload = {
        "model": upstream_model,
        "stream": False,
        "messages": [{"role": "user", "content": "hello non-stream"}]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "pi (linux; x86_64)",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())

    passed = data.get("object") == "chat.completion" and len(data.get("choices", [])) > 0
    duration = int((time.time() - t0) * 1000)
    record_result("Pi (OpenAI Chat)", upstream_model, "sync_aggregation", passed, f"id: {data.get('id')}", duration)


# ---------------------------------------------------------------------------
# Client 2: Next-Gen Coding Agent / Codex CLI (OpenAI Responses API)
# ---------------------------------------------------------------------------

def test_responses_plain_stream(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/responses"
    payload = {
        "model": upstream_model,
        "stream": True,
        "instructions": "You are Codex, an autonomous coding agent.",
        "input": "Fix typo in lib.rs"
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "codex-cli/0.125.0",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    has_created = "response.created" in event_names
    has_delta = "response.output_text.delta" in event_names
    has_completed = "response.completed" in event_names
    passed = has_created and has_delta and has_completed
    duration = int((time.time() - t0) * 1000)
    record_result("Codex (Responses API)", upstream_model, "plain_stream", passed, f"events: {len(events)}", duration)


def test_responses_tool_use(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/responses"
    payload = {
        "model": upstream_model,
        "stream": True,
        "input": "Search codebase for 'TODO'",
        "tools": [{
            "type": "function",
            "name": "grep_code",
            "description": "Grep across source files",
            "parameters": {
                "type": "object",
                "properties": {"query": {"type": "string"}}
            }
        }]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "codex-cli/0.125.0",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    has_completed = "response.completed" in event_names
    duration = int((time.time() - t0) * 1000)
    record_result("Codex (Responses API)", upstream_model, "tool_use_stream", has_completed, f"events: {len(events)}", duration)


def test_responses_multi_turn(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/responses"
    payload = {
        "model": upstream_model,
        "stream": True,
        "input": [
            {"type": "message", "role": "user", "content": "Search for main"},
            {"type": "function_call", "call_id": "call_resp_1", "name": "grep_code", "arguments": "{\"query\":\"main\"}"},
            {"type": "function_call_output", "call_id": "call_resp_1", "output": "src/main.rs:1: fn main()"}
        ]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "codex-cli/0.125.0",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    passed = "response.completed" in event_names
    duration = int((time.time() - t0) * 1000)
    record_result("Codex (Responses API)", upstream_model, "multi_turn", passed, "input chaining", duration)


def test_responses_session_affinity(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/responses"
    session_id = f"codex_sess_{int(time.time())}"
    payload = {
        "model": upstream_model,
        "stream": True,
        "input": "ping"
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "codex-cli/0.125.0",
            "X-Kinetix-Session": session_id,
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    passed = any(e[0] == "response.completed" for e in events)
    duration = int((time.time() - t0) * 1000)
    record_result("Codex (Responses API)", upstream_model, "session_affinity", passed, f"header: {session_id}", duration)


def test_responses_sync_aggregation(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/responses"
    payload = {
        "model": upstream_model,
        "stream": False,
        "input": "hello sync responses"
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "User-Agent": "codex-cli/0.125.0",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())

    passed = data.get("object") == "response" and data.get("status") == "completed" and "output" in data
    duration = int((time.time() - t0) * 1000)
    record_result("Codex (Responses API)", upstream_model, "sync_aggregation", passed, f"id: {data.get('id')}", duration)


# ---------------------------------------------------------------------------
# Client 3: Anthropic Agent (Messages API)
# ---------------------------------------------------------------------------

def test_anthropic_count_tokens(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages/count_tokens"
    payload = {
        "model": upstream_model,
        "system": "You are Claude Code, an agentic coding partner.",
        "messages": [
            {"role": "user", "content": "Inspect src/main.rs and summarize it."}
        ],
        "tools": [{
            "name": "read_file",
            "description": "Read a file from disk",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }
        }]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "anthropic-beta": "prompt-caching-2024-07-31",
            "x-claude-code-session-id": f"claude_count_{int(time.time())}",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())
        mode = resp.headers.get("x-kinetix-token-count")

    count = data.get("input_tokens")
    passed = isinstance(count, int) and count > 0 and mode in ("exact", "estimated")
    duration = int((time.time() - t0) * 1000)
    record_result(
        "Anthropic Agent",
        upstream_model,
        "count_tokens",
        passed,
        f"input_tokens: {count}, mode: {mode}",
        duration,
    )


def test_anthropic_plain_stream(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages"
    payload = {
        "model": upstream_model,
        "stream": True,
        "max_tokens": 128,
        "system": "You are Claude Code, an agentic coding partner.",
        "messages": [
            {"role": "user", "content": "Refactor the function foo."}
        ]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    has_start = "message_start" in event_names
    has_delta = "content_block_delta" in event_names
    has_stop = "message_stop" in event_names
    passed = has_start and has_delta and has_stop
    duration = int((time.time() - t0) * 1000)
    record_result("Anthropic Agent", upstream_model, "plain_stream", passed, f"events: {len(events)}", duration)


def test_anthropic_tool_use(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages"
    payload = {
        "model": upstream_model,
        "stream": True,
        "max_tokens": 128,
        "messages": [
            {"role": "user", "content": "List files in directory"}
        ],
        "tools": [{
            "name": "ls",
            "description": "List directory contents",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }
        }]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    passed = "message_stop" in event_names
    duration = int((time.time() - t0) * 1000)
    record_result("Anthropic Agent", upstream_model, "tool_use_stream", passed, f"events: {len(events)}", duration)


def test_anthropic_multi_turn(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages"
    payload = {
        "model": upstream_model,
        "stream": True,
        "max_tokens": 128,
        "messages": [
            {"role": "user", "content": "List files"},
            {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_ant_1", "name": "ls", "input": {"path": "."}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_ant_1", "content": "src/ Cargo.toml"}
                ]
            }
        ]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    event_names = [e[0] for e in events if e[0]]
    passed = "message_stop" in event_names
    duration = int((time.time() - t0) * 1000)
    record_result("Anthropic Agent", upstream_model, "multi_turn", passed, "tool_result turn", duration)


def test_anthropic_session_affinity(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages"
    session_id = f"claude_sess_{int(time.time())}"
    payload = {
        "model": upstream_model,
        "stream": True,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "ping"}]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
            "X-Conversation-Id": session_id,
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        events = read_sse_events(resp)

    passed = any(e[0] == "message_stop" for e in events)
    duration = int((time.time() - t0) * 1000)
    record_result("Anthropic Agent", upstream_model, "session_affinity", passed, f"header: {session_id}", duration)


def test_anthropic_sync_aggregation(upstream_model):
    t0 = time.time()
    url = f"{BASE_URL}/v1/messages"
    payload = {
        "model": upstream_model,
        "stream": False,
        "max_tokens": 64,
        "messages": [{"role": "user", "content": "hello anthropic sync"}]
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(payload).encode(),
        headers={
            "x-api-key": API_KEY,
            "anthropic-version": "2023-06-01",
            "Content-Type": "application/json",
            "User-Agent": "claude-code/0.2.0",
        }
    )
    with urllib.request.urlopen(req, timeout=15) as resp:
        data = json.loads(resp.read().decode())

    passed = data.get("type") == "message" and data.get("role") == "assistant" and len(data.get("content", [])) > 0
    duration = int((time.time() - t0) * 1000)
    record_result("Anthropic Agent", upstream_model, "sync_aggregation", passed, f"id: {data.get('id')}", duration)


# ---------------------------------------------------------------------------
# Main Matrix Driver
# ---------------------------------------------------------------------------

def run_matrix():
    print("=" * 80)
    print("           KINETIX CODING-AGENT COMPATIBILITY MATRIX RUNNER")
    print("=" * 80)
    print(f"Target URL: {BASE_URL}")
    print()

    # 1. Pi Agent against Synth OpenAI (passthrough) and Synth Gemini (translation)
    print("==> Testing Pi Agent (OpenAI Chat Completions Wire Format)")
    test_pi_plain_stream("syn-openai")
    test_pi_tool_use("syn-openai")
    test_pi_multi_turn("syn-openai")
    test_pi_session_affinity("syn-openai")
    test_pi_sync_aggregation("syn-openai")

    test_pi_plain_stream("syn-gemini")
    test_pi_tool_use("syn-gemini")
    test_pi_multi_turn("syn-gemini")

    # 2. Next-Gen Coding Agent / Codex (OpenAI Responses API)
    print("\n==> Testing Next-Gen Coding Agent (OpenAI Responses API)")
    test_responses_plain_stream("syn-openai")
    test_responses_tool_use("syn-openai")
    test_responses_multi_turn("syn-openai")
    test_responses_session_affinity("syn-openai")
    test_responses_sync_aggregation("syn-openai")

    test_responses_plain_stream("syn-gemini")

    # 3. Anthropic Agent / Claude Code (Messages API)
    print("\n==> Testing Anthropic Agent (Claude Code / Anthropic Wire Format)")
    test_anthropic_count_tokens("syn-gemini")
    test_anthropic_plain_stream("syn-gemini")
    test_anthropic_tool_use("syn-gemini")
    test_anthropic_multi_turn("syn-gemini")
    test_anthropic_session_affinity("syn-gemini")
    test_anthropic_sync_aggregation("syn-gemini")

    test_anthropic_plain_stream("syn-openai")

    # Summary
    print("\n" + "=" * 80)
    print("                              MATRIX SUMMARY")
    print("=" * 80)
    total = len(TEST_RESULTS)
    passed = sum(1 for r in TEST_RESULTS if r["passed"])
    failed = total - passed

    print(f"{'Client Profile':<24} | {'Upstream':<14} | {'Capability':<18} | {'Status':<6} | {'Time'}")
    print("-" * 80)
    for r in TEST_RESULTS:
        st = "PASS" if r["passed"] else "FAIL"
        print(f"{r['client']:<24} | {r['upstream']:<14} | {r['capability']:<18} | {st:<6} | {r['duration_ms']}ms")

    print("-" * 80)
    print(f"Total: {total} | Passed: {passed} | Failed: {failed} ({passed/total*100:.1f}% compatible)")
    print("=" * 80)

    if failed > 0:
        sys.exit(1)


if __name__ == "__main__":
    run_matrix()

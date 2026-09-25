#!/usr/bin/env python3
"""Benchmark client: drives N concurrent streaming requests against Kinetix and
reports added-latency / TTFT percentiles, throughput, and error counts.

Percentiles are over Kinetix's own added overhead (time from just-before-send to
first byte, and to stream end), which is what NFR-1.1/1.2 measure given a
zero-inference synthetic upstream.
"""
import argparse
import json
import threading
import time
import urllib.request

WORD = "lorem"


def one(url, key, path, out, idx, tool_fragments=0):
    if path == "passthrough":
        model = "syn-openai"
        body = {"model": model, "stream": True, "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]}
    elif path == "translation":
        model = "syn-gemini-3"
        body = {"model": model, "stream": True, "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]}
    elif path == "tools":
        model = "syn-openai"
        body = {"model": model, "stream": True, "max_tokens": 64,
                "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object", "properties": {"a": {"type": "string"}}}}}],
                "messages": [{"role": "user", "content": "call f"}]}
        if tool_fragments > 0:
            # NFR-1.9: ask the upstream to split the arguments into many pieces.
            body["tool_fragments"] = tool_fragments
    else:  # large
        model = "syn-openai"
        big = ("The quick brown fox. " * 4000)
        body = {"model": model, "stream": True, "max_tokens": 64,
                "messages": [{"role": "user", "content": big}]}

    data = json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method="POST",
                                 headers={"authorization": "Bearer " + key,
                                          "content-type": "application/json",
                                          "accept": "text/event-stream"})
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            ttft = None
            nbytes = 0
            while True:
                chunk = resp.read(4096)
                if not chunk:
                    break
                if ttft is None:
                    ttft = (time.perf_counter() - t0) * 1000.0
                nbytes += len(chunk)
            total = (time.perf_counter() - t0) * 1000.0
        out[idx] = (ttft if ttft is not None else total, total, nbytes, None)
    except Exception as e:  # noqa: BLE001
        out[idx] = (None, (time.perf_counter() - t0) * 1000.0, 0, str(e))


def pct(vals, p):
    if not vals:
        return 0.0
    vals = sorted(vals)
    k = min(len(vals) - 1, int(round((p / 100.0) * (len(vals) - 1))))
    return round(vals[k], 2)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", required=True)
    ap.add_argument("--key", required=True)
    ap.add_argument("--concurrency", type=int, required=True)
    ap.add_argument("--requests", type=int, required=True)
    ap.add_argument("--path", required=True)
    ap.add_argument("--tool-fragments", type=int, default=0)
    a = ap.parse_args()

    out = [None] * a.requests
    errors = 0
    sem = threading.Semaphore(a.concurrency)

    def worker(i):
        with sem:
            one(a.url, a.key, a.path, out, i, a.tool_fragments)

    t0 = time.perf_counter()
    threads = [threading.Thread(target=worker, args=(i,)) for i in range(a.requests)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    wall = time.perf_counter() - t0

    ttfts, totals = [], []
    for o in out:
        if o is None:
            continue
        ttft, total, _nb, err = o
        if err:
            errors += 1
            continue
        if ttft is not None:
            ttfts.append(ttft)
        totals.append(total)

    print(json.dumps({
        "path": a.path,
        "concurrency": a.concurrency,
        "requests": a.requests,
        "errors": errors,
        "rps": round(a.requests / wall, 1) if wall > 0 else 0,
        "p50_overhead_ms": pct(totals, 50),
        "p95_overhead_ms": pct(totals, 95),
        "p99_overhead_ms": pct(totals, 99),
        "ttft_p50_ms": pct(ttfts, 50),
        "ttft_p95_ms": pct(ttfts, 95),
    }))


if __name__ == "__main__":
    main()

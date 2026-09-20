# Testing and Benchmarks

Kinetix is verified by unit tests, golden wire fixtures, protocol torture/fuzz
tests, an end-to-end smoke test, real client acceptance (Pi), and reproducible
benchmark rigs.

## Test suites

```bash
cargo test                 # unit + integration
```

- **Unit tests** live in `#[cfg(test)]` modules across `src/` (cost, crypto,
  passthrough, predicates, trace, sse, adapters, types, limits, ratelimit, …).
- **Golden wire fixtures** (`tests/wire_fixtures.rs`) lock the exact encoded bytes
  of streaming/non-streaming responses and error frames for both formats, so any
  supported wire-output change fails CI (NFR-5.5).
- **Inbound decode fixtures** (`tests/decode_fixtures.rs`) lock request decoding
  for both formats (system hoisting, parallel tool calls, images, reasoning
  effort, unknown-extra capture, thinking signatures).
- **Protocol torture + fuzz** (`src/torture.rs`, FR-9.4/9.5): 1-byte chunks,
  UTF-8/JSON splits, char-by-char tool arguments, interleaved parallel tools,
  reasoning/text interleaving, usage-only-in-final-event, zero-token responses,
  unknown fields, malformed SSE, large tool calls, error classification
  (429/5xx/timeout vs 400, rate-limit vs quota), keepalive cadence, and bounded
  fuzzing.

## End-to-end smoke

```bash
scripts/smoke.sh                # starts a synthetic upstream + a fresh instance
```

Asserts health, auth failures, model listing, passthrough/translation/tool-call
streams, Anthropic inbound, admin login/overview/metrics/usage, Route Trace and
diagnostics, Validate/Dry Run, opaque route-id resolution, and a malformed-URL
rejection. Wired into CI after the release build.

## Local CI mirror

```bash
scripts/ci.sh            # fmt, clippy, tests, release, dashboard, smoke, bench, cargo-deny
scripts/ci.sh --fast     # skips release/smoke/bench/dashboard
```

Run this before pushing — it mirrors `.github/workflows/ci.yml`.

## Real client acceptance

Pi (a coding agent) is run against Kinetix end-to-end: plain streaming, a
tool-calling turn, multi-turn sessions, and the Anthropic-format client. See
[`docs/pi-compatibility.md`](https://github.com/PrightCord/kinetix/blob/main/docs/pi-compatibility.md)
and [`docs/compatibility.md`](https://github.com/PrightCord/kinetix/blob/main/docs/compatibility.md).

## Benchmarks

Two rigs, both in `scripts/`:

- **Correctness/path coverage** — `scripts/bench.sh` + `scripts/synthetic_upstream.py`
  (a deterministic Python upstream) across passthrough, translation, tools, large,
  and tools-large-fragments paths.
- **Capacity/overhead** — `scripts/bench-rust.sh` + `scripts/bench_rust.rs` (a
  std-only Rust upstream + load generator) across a concurrency matrix, with
  optional `CPUSET` (1 vCPU) and `ALLOC_STATS=1` (allocations/request).

Cancellation latency: `scripts/cancel_bench.py` measures how long the upstream
keeps being written after a client disconnect (NFR-1.10).

Measured results, methodology, and caveats are documented in
[`docs/benchmarks.md`](https://github.com/PrightCord/kinetix/blob/main/docs/benchmarks.md).
Headline numbers: added TTFT/total overhead of **~0.1–2 ms**, throughput far above
the 50 req/s target with zero errors, idle RSS ~13 MB, cold start ~75 ms.

> The synthetic upstream is the bottleneck at very high concurrency on one
> machine, so those rows characterize the rig, not Kinetix's ceiling.

## CI

`.github/workflows/ci.yml` runs, on every push/PR: build the dashboard → fmt →
clippy → tests → release build → smoke → benchmark matrix → `cargo deny check`,
plus a separate dashboard job (`tsc`, `vite build`).

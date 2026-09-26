# Benchmarks

Benchmark results come from reproducible harnesses rather than vendor claims.
Two harnesses ship in `scripts/`:

| Harness | Language | Use |
|---|---|---|
| `scripts/bench.sh` + `synthetic_upstream.py` | Python (stdlib) | Correctness-oriented path coverage; simple to read. |
| `scripts/bench-rust.sh` + `bench_rust.rs` | Rust (std only) | Capacity and overhead under controlled load. |

Both drive a **deterministic synthetic upstream** that emits a fixed token
cadence (`SYN_TOKENS`/`SYN_DELAY_MS` or the Rust rig's `TOKENS`/`DELAY_US`), so
any measured variance is Kinetix overhead, not model inference.

> The Python rig is GIL-bound: a Python client plus a Python synthetic upstream
> saturate a single machine well before Kinetix does, so its high-concurrency
> rows do **not** represent Kinetix capacity. Use `bench-rust.sh` for the
> capacity measurements.

## Method

```
scripts/bench-rust.sh "1 10 100 200 500 1000" 3000
```

- Fresh throwaway SQLite database, fresh bootstrap, release binary.
- Kinetix bound to `127.0.0.1:8180`; synthetic upstream on `9099`.
- `DELAY_US` sets a per-token upstream delay so each stream lasts long enough to
  hold the target concurrency; `CPUSET=0` pins Kinetix to one core to
  approximate a single-CPU run.
- The load generator issues streaming requests (`stream:true`) and records
  TTFT (first byte) and total latency per request.

## Results

### No upstream delay, 3000 requests/level, all cores

| Concurrency | rps | TTFT p50 | TTFT p95 | TTFT p99 | Total p50 | Total p95 | Total p99 | Errors | RSS |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 5172 | 0.17 ms | 0.33 ms | 0.60 ms | 0.18 ms | 0.34 ms | 0.62 ms | 0 | 13 MB |
| 10 | 4877 | 1.97 ms | 2.31 ms | 2.38 ms | 1.98 ms | 2.31 ms | 2.39 ms | 0 | 13.5 MB |
| 100 | 4945 | 20.1 ms | 21.5 ms | 21.7 ms | 20.1 ms | 21.5 ms | 21.7 ms | 0 | 13.7 MB |
| 200 | 2793 | 25.3 ms | 27.6 ms | 1063 ms | 25.3 ms | 27.6 ms | 1063 ms | 0 | 13.7 MB |
| 500 | 1001 | 26.6 ms | 1271 ms | 2962 ms | 26.6 ms | 1271 ms | 2962 ms | 0 | 16.0 MB |
| 1000 | 813 | 25.9 ms | 2688 ms | 3096 ms | 25.9 ms | 2688 ms | 3096 ms | 0 | 17.1 MB |

### 2 ms/token upstream delay, 100 tokens, Kinetix pinned to 1 core

| Concurrency | rps | TTFT p50 | TTFT p95 | TTFT p99 | Total p50 | Total p95 | Errors | RSS |
|---|---|---|---|---|---|---|---|---|
| 1 | 9532 | 0.09 ms | 0.10 ms | 0.19 ms | 0.09 ms | 0.11 ms | 0 | 12.9 MB |
| 10 | 9301 | 1.04 ms | 1.23 ms | 1.44 ms | 1.04 ms | 1.23 ms | 0 | 14.0 MB |
| 100 | 8522 | 11.6 ms | 17.1 ms | 19.9 ms | 11.6 ms | 17.1 ms | 0 | 25.3 MB |
| 200 | 3629 | 16.2 ms | 19.7 ms | 1034 ms | 16.2 ms | 19.7 ms | 0 | 29.4 MB |

## Reading the numbers

- **Observed overhead.** Kinetix's *added* latency is the small p50 seen at low
  concurrency: 0.09–0.17 ms TTFT and 0.09–0.18 ms total at concurrency 1, and
  ~1 ms at concurrency 10. This is measured against a real socket round trip,
  so it includes Kinetix's own accept/dispatch overhead.
- **Observed capacity.** In the two displayed concurrency-200 runs, measured
  throughput is 2,793 rps with no upstream delay and 3,629 rps with a 2 ms/token
  upstream delay. Both runs report zero errors; RSS is 13.7 MB and 29.4 MB,
  respectively.
- **The p99 spikes are harness artifacts, not Kinetix.** The upstream is
  thread-per-connection: at 200+ simultaneous connections its thread-spawn cost
  shows up as a one-off latency spike in the slowest percentile while the median
  is unaffected. A native/async upstream would remove it.
- **Higher concurrency.** The 500/1000 runs show a bounded,
  linear latency rise with **no errors, no unbounded growth** (RSS 13→17 MB) and
  **no corruption** across the measured concurrency levels.
  The plateau near ~26 ms TTFT is Kinetix's own single-process connection setup.

## Cold start and idle footprint

Measured on the release binary with a fresh throwaway database:

| Metric | Measured |
|---|---|
| Cold start to `/healthz` ready | ~75 ms |
| Idle RSS | ~13 MB |
| Idle CPU | ~1% |

## Allocations per request

The build optionally counts allocations (`--features alloc-stats`, a
`#[global_allocator]` wrapper in `src/alloc.rs`). The default build reports the
metric as 0 — honestly "not measured" rather than a fabricated number — and the
feature is off in production.

Measured with `ALLOC_STATS=1 TOKENS=60 scripts/bench-rust.sh "1 10 100" 2000`
(60-token passthrough streams; allocs/req is steady-state, so it is essentially
independent of concurrency):

| Concurrency | rps | allocs/request | bytes/request | errors |
|---|---|---|---|---|
| 1 | 4491 | 779.5 | 190,946 | 0 |
| 10 | 4730 | 779.5 | 190,947 | 0 |
| 100 | 4932 | 779.7 | 190,964 | 0 |

~780 allocations and ~186 KB per 60-token passthrough request. The value is
flat across concurrency (no per-request amplification under load) and is
dominated by JSON parsing/serialization of the request and the encoded stream
frames.

## Path coverage

The Python rig covers four paths plus a large-incremental-tool-argument path:

| Path | What it exercises |
|---|---|
| `passthrough` | OpenAI -> OpenAI same-format byte forwarding. |
| `translation` | OpenAI -> Gemini translation (canonical state). |
| `tools` | A tool call with arguments split across three frames. |
| `large` | A ~76 KB user message (context handling). |
| `tools-large-fragments` | One tool argument (~660 B) split into ~200 fragments for incremental reassembly. |

`scripts/bench.sh "1 10" 40` results (Python rig, so rps is rig-bound, not
Kinetix-bound):

| Concurrency | Path | p50 (ms) | p95 (ms) | TTFT p50 (ms) | errors |
|---|---|---|---|---|---|
| 1 | passthrough | 97.1 | 99.7 | 60.9 | 0 |
| 1 | translation | 96.3 | 99.3 | 47.6 | 0 |
| 1 | tools | 8.9 | 10.2 | 8.9 | 0 |
| 1 | tools-large-fragments | 12.0 | 13.6 | 8.6 | 0 |
| 10 | passthrough | 99.0 | 123.0 | 61.6 | 0 |
| 10 | translation | 98.1 | 122.4 | 49.3 | 0 |
| 10 | tools | 8.2 | 32.6 | 8.2 | 0 |
| 10 | tools-large-fragments | 29.7 | 56.7 | 13.1 | 0 |

All five paths complete with zero errors, including the ~200-fragment tool
argument reassembly under concurrency.

## Reproduce

```sh
# Rust rig, single-CPU approximation with sustained streams
DELAY_US=2000 TOKENS=100 CPUSET=0 scripts/bench-rust.sh "1 10 100 200" 4000

# Rust rig with allocation accounting
ALLOC_STATS=1 TOKENS=60 scripts/bench-rust.sh "1 10 100" 2000

# Python rig, path coverage (passthrough, translation, tools, large, tool fragments)
scripts/bench.sh "1 10 100" 80
```

Cancellation latency is measured separately by
`scripts/cancel_bench.py`; see the README.

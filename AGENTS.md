# Architecture

## components

Kinetix is a streaming-first LLM reverse proxy and routing engine written in Rust with an embedded React admin dashboard. It packages control plane, data plane, and CLI into a single binary.

- **Inbound Frontends (`src/frontends/`)**:
  - `openai.rs`: Implements `/v1/chat/completions` (streaming SSE, tool/function calling, OpenAI format).
  - `anthropic.rs`: Implements `/v1/messages` (streaming SSE, `content_block` delta events, thinking parameter blocks).
  - `responses.rs`: Implements `/v1/responses` (OpenAI Responses format with turn items and streaming output).
  - `models.rs`: Implements `/v1/models` and `/v1/models/{id}` (model discovery filtered by virtual key permissions).
- **Core Pipeline & Router (`src/pipeline.rs`, `src/router.rs`, `src/registry.rs`)**:
  - Request ingestion, virtual key validation, rate/token limit enforcement (`src/limits.rs`), and cost pre-estimation (`src/cost.rs`).
  - Route resolution: maps requested model alias or Route ID into candidate upstream targets.
  - Predicate engine (`src/predicate.rs`): filters candidates against request capabilities (e.g. streaming, thinking, context length).
  - Execution engine: handles upstream target dispatch, credential leasing from pools, streaming-safe failovers, response streaming, usage metering, and flight-recorder diagnostics (`src/trace.rs`).
- **Account Pools & Strategies (`src/pool.rs`)**:
  - Manages upstream provider credentials across pools with load-balancing strategies (`round_robin`, `priority`, `weighted`, `least_used`).
  - Tracks account health state, cooldowns, HTTP 429 rate limit backoffs, and quota exhaustion.
- **Outbound Adapters (`src/adapters/`)**:
  - Translates normalized canonical request data into vendor-specific upstream protocols:
    - `openai.rs`: OpenAI-compatible wire format.
    - `anthropic.rs`: Anthropic wire format.
    - `gemini.rs`: Google Gemini REST API wire format (`generateContent` & `streamGenerateContent`).
  - Ingests streaming chunks from upstreams and normalizes them into downstream SSE events and token accounting metrics.
- **Control Plane & Storage (`src/db.rs`, `migrations/`, `src/admin.rs`)**:
  - SQLite database running with WAL mode (`PRAGMA journal_mode=WAL`) managed via SQLx migrations.
  - Persists providers, accounts, models, routes, virtual keys, request logs, token usage, cost accounting, and plugin state.
  - Admin REST API (`/api/*`) for administration, metrics, exports (`src/export.rs`), and diagnostic traces.
- **Embedded Admin Dashboard (`dashboard/`, `src/assets.rs`)**:
  - React 19 + TypeScript + Vite + Tailwind CSS v4 single-page application.
  - Embedded directly into the Rust binary at compile time via `rust-embed`.
- **Plugin Host (`src/plugins/`, `wit/`)**:
  - WebAssembly Component Model runtime powered by `wasmtime` 48.
  - WIT contract defined in `wit/kinetix-plugin.wit` with host capabilities (HTTP egress, logging, key-value storage, credential refreshing).
  - The guest SDK, first-party plugins, catalog source, and packaging tooling live in `PrightCord/kinetix-plugins`.
  - Kinetix vendors official catalog/trust snapshots under `src/plugins/` for offline discovery, and supports dynamic remote synchronization with disk caching (`catalog.cache.json`) and verified CLI / dashboard marketplace installation.
- **CLI & Daemon Runner (`src/main.rs`, `src/cli.rs`, `src/server.rs`)**:
  - Unified binary providing both the proxy daemon (`kinetix serve`) and administrative CLI commands (`kinetix provider`, `model`, `key`, `route`, `user`, etc.).

## boundaries

- **Inbound Protocol Boundary**: Clients communicate exclusively via standard OpenAI Chat Completions, OpenAI Responses, or Anthropic Messages endpoints. Downstream clients never see Kinetix internal types or upstream provider identities (strict topology privacy).
- **Canonical Representation Boundary**: Frontends decode wire requests into internal canonical types. Outbound adapters encode canonical types into upstream wire payloads. Frontends and adapters never couple directly.
- **Outbound Upstream Boundary**: Network requests to upstreams are strictly isolated within adapters. Credentials are leased dynamically and injected at dispatch time; upstream credentials and URLs are scrubbed before logging or downstream propagation.
- **Host / Guest WASM Plugin Boundary**: Plugins run sandboxed in Wasmtime. They interact with the host solely via the WIT contract (`wit/kinetix-plugin.wit`) and are restricted by configured `HostPolicy`.
- **Data Plane vs Control Plane Boundary**: Core request routing and streaming (data plane) remain resilient even if control plane/database operations experience transient latency. Usage logs are queued asynchronously (`src/logqueue.rs`).

## dependency direction

- **Unidirectional Inward Flow**:
  - Entry point: `src/main.rs` -> `src/cli.rs` -> `src/server.rs` -> `src/app.rs`.
  - HTTP routing: `src/server.rs` mounts `src/frontends/`, `src/admin.rs`, and embedded `src/assets.rs`.
  - Request execution: `src/pipeline.rs` orchestrates `src/router.rs`, `src/registry.rs`, `src/pool.rs`, `src/adapters/`, `src/db.rs`, and `src/plugins/`.
  - Leaf modules: `src/types.rs`, `src/crypto.rs`, `src/paths.rs`, `src/sse.rs`, `src/cost.rs`, `src/limits.rs` provide pure types and utilities with zero inward dependencies on pipeline or server.
  - Plugin host: `src/plugins/` depends on Wasmtime and the host WIT definition. Guest SDK/plugins are maintained independently in `PrightCord/kinetix-plugins`.

---

# Development

## build

- **Dashboard build (Required before compiling Rust)**:
  The admin dashboard bundle must exist because it is embedded into the Rust binary via `rust-embed`.
  ```bash
  cd dashboard
  npm ci
  npm run build
  cd ..
  ```
  *Note:* If you modify dashboard files, re-embed them in the binary by touching the asset loader:
  ```bash
  touch src/assets.rs
  ```
- **Rust binary build**:
  ```bash
  # Debug build
  cargo build

  # Optimized release build
  cargo build --release
  ```
- **Plugin build**:
  Guest plugins are built in the separate `PrightCord/kinetix-plugins` repository. From that checkout:
  ```bash
  scripts/build-plugin.sh plugins/antigravity-oauth
  ```

## test

- **Unit and internal integration tests**:
  ```bash
  cargo test --quiet
  ```
- **Fast local CI gate**:
  Runs formatting check, clippy, unit tests, source-integrity check, and cargo-deny:
  ```bash
  scripts/ci.sh --fast
  ```
- **Full local CI gate**:
  Runs fmt, clippy, tests, release build, dashboard build, installer smoke test, synthetic upstream smoke test, compat matrix, benchmark, and cargo-deny:
  ```bash
  scripts/ci.sh
  ```
- **Individual test suites**:
  ```bash
  # Wire decoding and fixture tests
  cargo test --test decode_fixtures
  cargo test --test wire_fixtures

  # Plugin host subsystem tests
  cargo test --test plugins
  KINETIX_PLUGIN_E2E_PACKAGE=/path/to/plugin.kxp cargo test --test plugin_e2e

  # End-to-end smoke test against synthetic upstream
  scripts/smoke.sh 127.0.0.1:8180

  # Coding-agent compatibility matrix (Pi, Codex, Anthropic personas)
  scripts/compat-matrix.sh 127.0.0.1:8186
  ```

## lint

- **Rust linting**:
  ```bash
  cargo clippy --all-targets
  ```
- **Dashboard TypeScript check**:
  ```bash
  cd dashboard && npx tsc --noEmit
  ```
- **Dependency vulnerability & license checks**:
  ```bash
  cargo deny check
  ```

## format

- **Check Rust formatting**:
  ```bash
  cargo fmt --all -- --check
  ```
- **Apply Rust formatting**:
  ```bash
  cargo fmt --all
  ```

---

# Repository conventions

## naming

- **Rust code**: Idiomatic Rust standard (`snake_case` for modules, functions, methods, and variables; `PascalCase` for types, traits, and enum variants; `SCREAMING_SNAKE_CASE` for statics and constants).
- **Files**: Rust source files in `snake_case.rs`, shell scripts in `kebab-case.sh`, Python test harnesses in `snake_case.py`.
- **Virtual keys**: Client-facing virtual API keys always use the `sk-kinetix-` prefix.
- **Custom HTTP headers**: All downstream response diagnostic headers must use the `X-Kinetix-` prefix (e.g., `X-Kinetix-Route`, `X-Kinetix-Attempt`, `X-Kinetix-Latency-Ms`).
- **Domain vocabulary**: Use authoritative terms:
  - **Provider**: An upstream service (e.g., OpenAI, Gemini, Anthropic).
  - **Account**: Specific credentials/pool associated with a provider.
  - **Model**: An upstream model ID with pricing and capabilities.
  - **Route**: A user-defined routing rule resolving to ordered targets (previously termed "combo").
  - **Target**: A concrete pairing of provider, model, and optional account.
- **Git commits**: Follow Conventional Commits format (`feat(...)`, `fix(...)`, `refactor(...)`, `test(...)`, `docs(...)`, `chore(...)`).

## errors

- **Library & internal errors**: Use `thiserror` for defined enum errors and `anyhow::Result` for application/CLI flows.
- **HTTP error responses**: Frontends must translate errors into compliant JSON error envelopes matching the requested protocol:
  - OpenAI format: `{"error": {"message": "...", "type": "...", "code": "..."}}`.
  - Anthropic format: `{"type": "error", "error": {"type": "...", "message": "..."}}`.
- **No silent error suppression**: Never swallow errors silently or drop upstream error messages during dispatch. Upstream failures must be logged in diagnostic traces (`src/trace.rs`) before failing over or returning.
- **Distinguish error classifications**: Clearly separate retryable errors (e.g., network reset before first byte, HTTP 429, HTTP 503) from non-retryable client errors (e.g., HTTP 400, invalid parameters).

## async

- **Tokio runtime**: Async operations run on Tokio 1.x using `axum` and `hyper`.
- **Low TTFT streaming**: Streaming is the canonical execution path. Keep Time-To-First-Token minimal. Use `async-stream` and pinned response streams without unbounded buffering.
- **Cancellation safety**: Ensure downstream disconnects gracefully abort upstream requests, close SSE streams, and return leased credentials to the account pool.
- **No blocking in async tasks**: Never perform blocking I/O or heavy synchronous compute in async workers. Offload CPU-heavy or synchronous file tasks using `tokio::task::spawn_blocking`.
- **Bounded channels**: Use bounded channels (`tokio::sync::mpsc::channel`) for event queues (`src/logqueue.rs`) to prevent memory leaks under heavy load.

## database

- **SQLite with SQLx**: Managed through `sqlx::SqlitePool` with WAL mode enabled (`PRAGMA journal_mode=WAL`).
- **Migrations**: Schema changes must be written as discrete migration files in `migrations/` named with timestamps (`YYYYMMDDHHMMSS_description.sql`).
- **Parameterized queries**: All SQL queries must use parameterized bindings (`sqlx::query!` or `sqlx::query(...)` with `.bind(...)`). Never construct SQL queries using string concatenation or formatting.
- **Data plane isolation**: Read operations and token verification should avoid locking writes; log ingestion must be asynchronous to prevent DB write contention from blocking inference.

## API patterns

- **Protocol fidelity**: Do not guess provider features or inject unsupported parameters. Keep protocol semantics exact according to official OpenAI and Anthropic specifications.
- **SSE streaming structure**:
  - OpenAI SSE: Lines prefixed with `data: `, payload in JSON, stream terminated with `data: [DONE]\n\n`.
  - Anthropic SSE: Explicit `event: <name>\ndata: <json>\n\n` blocks.
  - Keepalive pulses (`: keepalive\n\n`) must be emitted at regular intervals to prevent reverse proxy/Cloudflare idle timeouts (~100s).
- **Authentication**: Virtual keys and admin tokens must be verified using constant-time comparison via `subtle::ConstantTimeEq` to prevent timing attacks.
- **Credential security**: Store upstream credentials encrypted at rest using AES-GCM-256 (`src/crypto.rs`). Never log raw API keys.

---

# Testing expectations

## unit

- Unit tests must be co-located in their respective modules in `src/` (e.g., `src/predicate.rs`, `src/limits.rs`, `src/cost.rs`, `src/auth.rs`, `src/crypto.rs`).
- Unit tests must be fast, deterministic, hermetic, and offline (no external network calls).
- Run with `cargo test --lib --bins`.

## integration

- **End-to-end proxy behavior**: Validated using `scripts/synthetic_upstream.py` as a deterministic mock upstream server:
  - `scripts/smoke.sh`: Tests routing, wire translation, virtual keys, models discovery, and admin APIs.
  - `scripts/compat-matrix.sh`: Tests coding-agent client profiles (Pi, Codex Responses, Anthropic) across multi-turn sessions, tool calling, and thinking blocks.
- **WASM plugin runtime**:
  - `tests/plugins.rs`: Tests plugin store, capabilities, lifecycle, and host policy.
  - `tests/plugin_e2e.rs`: Tests end-to-end installation, instantiation, and invocation of an external compiled `.kxp` supplied with `KINETIX_PLUGIN_E2E_PACKAGE`.

## fixtures

- Wire decoding and reassembly must be verified using real-world fixtures in `tests/decode_fixtures.rs` and `tests/wire_fixtures.rs`.
- When updating adapters or frontends to support new provider models, thinking blocks, or tool call formats, add corresponding test fixtures covering both single-turn and streaming chunks.

---

# Rules

## things agents must never do

- **NEVER attempt failover after the first byte has streamed**: Once response headers or the initial SSE byte have been flushed to the downstream client, failover is impossible. Any subsequent error must terminate the stream; never attempt retrying another target after byte 1.
- **NEVER violate topology privacy**: Under no circumstances should upstream account labels, provider URLs, internal keys, or routing topology leak into client-facing HTTP response headers, bodies, or error messages.
- **NEVER hardcode vendor presets or model capabilities**: Kinetix is strictly operator-configured. Do not add hardcoded model dictionaries, provider presets, or assumed capabilities into the Rust core.
- **NEVER commit without building the dashboard**: If modifying files in `dashboard/`, always run `npm run build` inside `dashboard/` and ensure `src/assets.rs` is refreshed before running `cargo build` or committing.
- **NEVER introduce non-permissive dependencies**: All new crate dependencies must strictly comply with `deny.toml` (OSI permissive licenses only, no GPL/AGPL, zero unreviewed security advisories).
- **NEVER push unverified commits directly to remote**: Batch commits locally and verify with `scripts/ci.sh --fast` (or `scripts/ci.sh`) before pushing to conserve CI runner resources.
- **NEVER silently mutate or discard request parameters**: Preserve payload information unless an explicit operator route policy dictates parameter stripping or rewriting.
- **NEVER forget to update AGENTS.md**: Whenever repository architecture, build scripts, testing expectations, database migrations, or conventions change, **agents must update AGENTS.md** as part of the same task.

---

# Definition of done

## build passes
- Rust project builds cleanly with zero errors in both dev and release profiles:
  ```bash
  cargo build && cargo build --release
  ```
- Admin dashboard compiles and bundles without errors:
  ```bash
  cd dashboard && npm ci && npm run build
  ```
- If plugin code is touched, validate/package it in `PrightCord/kinetix-plugins`. If the host WIT changes, coordinate the matching SDK/WIT update in that repository.

## tests pass
- All unit and fixture tests pass:
  ```bash
  cargo test --quiet
  ```
- Fast local CI checks pass:
  ```bash
  scripts/ci.sh --fast
  ```
- Full CI test suite passes before merging major milestones:
  ```bash
  scripts/ci.sh
  ```
- Plugin integration tests pass:
  ```bash
  cargo test --test plugins && cargo test --test plugin_e2e
  ```

## formatting passes
- Rust code passes formatting check with zero differences:
  ```bash
  cargo fmt --all -- --check
  ```
- Rust code passes clippy checks:
  ```bash
  cargo clippy --all-targets
  ```
- Dashboard passes TypeScript typechecking:
  ```bash
  cd dashboard && npx tsc --noEmit
  ```
- Dependency check passes:
  ```bash
  cargo deny check
  ```

## docs updated
- **`AGENTS.md` updated**: Reflects any changes made to architecture, boundaries, commands, test suites, conventions, or rules.
- **`CONTRIBUTING.md` / `README.md` / `docs/` updated**: Documentation updated whenever public CLI commands, configuration options, endpoints, or environment variables change.
- **Database migrations documented**: Any schema modifications include corresponding migration scripts in `migrations/` and updated DB documentation.

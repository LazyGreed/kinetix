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
  - Execution engine: handles upstream target dispatch, credential leasing from pools, streaming-safe failovers, response streaming, usage metering, and flight-recorder diagnostics (`src/trace.rs`). Every `credential_for()` consumer (inference, token counting, native model discovery, provider testing, startup seeding, and OAuth completion) handles typed terminal invalid-credential errors. Account disablement is fallible: callers do not report success or reauthorization-required unless persistence and registry reload succeed; schedules are forgotten only after disabled status persists. Expiry-derived refresh scheduling uses up to a five-minute lead, reduced proportionally to half the remaining lease for leases shorter than ten minutes; deadlines stay anchored while timing hints and the in-memory SHA-256 fingerprint of the resolved secret are unchanged. The opaque plugin lease handle is used only as a KV lookup key and is not part of credential-generation identity. Secret or timing changes detect plugin-side refreshes despite API v1 `credential-lease` lacking a `rotated` field; the fingerprint is never persisted or logged. Successful rotations impose a one-minute minimum retry delay when timing hints do not advance.
- **Account Pools & Strategies (`src/pool.rs`)**:
  - Manages upstream provider credentials across pools with load-balancing strategies (`round_robin`, `priority`, `weighted`, `least_used`).
  - Tracks account health state, cooldowns, HTTP 429 rate limit backoffs, and quota exhaustion.
- **Outbound Adapters (`src/adapters/`)**:
  - Translates normalized canonical request data into vendor-specific upstream protocols:
    - `openai.rs`: OpenAI-compatible wire format.
    - `anthropic.rs`: Anthropic wire format.
    - `gemini.rs`: Google Gemini REST API wire format (`generateContent` & `streamGenerateContent`).
  - Ingests streaming chunks from upstreams and normalizes them into downstream SSE events and token accounting metrics.
  - Gemini tool declarations use `parametersJsonSchema` with an explicit supported-key allowlist; documented-supported constraints are preserved, while unverified or unknown validation keywords fail closed before dispatch instead of being silently weakened.
  - An adapter indicates that it produces opaque provider state (e.g. Gemini `thoughtSignature`) by returning `Some(OpaqueStateTarget)` from the `Adapter::opaque_state_target` trait method; the default is `None`, and plugin adapters are never assumed compatible with another adapter's opaque-state protocol. An adapter with a provider-documented stand-in for state it cannot carry returns it from `Adapter::opaque_state_placeholder` (default `None`).
- **Opaque Provider State (`src/opaque_state.rs`)**:
  - Host-owned subsystem that persists provider continuation state the client protocol cannot represent (currently Gemini `thoughtSignature`) and replays it on later turns.
  - Captures signatures keyed by the post-normalization client-visible tool-call id, the client scope (virtual key id or `internal`), the provider, and the **exact originating model** plus family/producer; state from one virtual key is never served to another, and (for `generateContent`) state is never restored onto a different model. Lookup validates tool-name and explicit-session identity **before** classifying a row as non-portable, so a cross-model switch can never turn a reused id or a conflicting session into a placeholder. This is an implementation detail of `OpaqueStateStore::resolve_tool_signature` (`src/opaque_state.rs`), not a provider-behavior claim: when a row for the exact target model exists in Kinetix's own store, that row's identity is checked first, so a different model's row that merely shares the same tool-call id can never shadow it into a false `Incompatible`/placeholder outcome; rows from other models are only consulted when no exact-model row exists.
  - Encrypts values at rest with a cipher derived specifically for this subsystem (`kinetix-opaque-provider-state`, distinct from the credential and plugin-KV ciphers); only SHA-256 hashes of scope, tool-call id, session id, and tool name are persisted.
  - Uses a bounded RAM cache written synchronously for the data plane; encryption, the SQLite UPSERT, and periodic pruning run on a bounded background durability worker (`DurabilityJob` channel + `flush()` barrier), so a slow or locked database never stalls a streaming tool call. A saturated queue or a storage failure is counted, never fatal to a live response. The RAM cache TTL (1h) is deliberately shorter than the SQLite TTL (24h) so SQLite outlives the hot cache; an expired RAM entry is evicted and the lookup falls through to SQLite rather than short-circuiting to `Missing`, so a continuation on a long-running process still restores within the full 24h persistence window.
  - Consumed by `src/pipeline.rs` around the existing portability decision: compatible state is restored only after a `strip_with_warning`/`reject` boundary is settled, and any known-but-incompatible stored state enters that decision even when neither the provider nor the wire format crossed over.
  - Cross-model continuation: an adapter that has a documented placeholder for non-portable continuation state returns it from `Adapter::opaque_state_placeholder` (default `None`; Gemini returns the provider's `skip_thought_signature_validator` sentinel, which Google documents for function-call history transferred from another model). The adapter **gates the placeholder to the family that documents it**: the Gemini adapter returns the sentinel only for Gemini 3 model ids (`major == 3`, not `major >= 3`), because only that family is documented to validate the signature of a replayed function call, while Gemini 2.5 and older treat the signature as optional and must not receive the Gemini 3 validator-bypass token, and a later major family is not assumed to inherit the contract. The pipeline substitutes that placeholder onto the specific stored-but-incompatible historical call instead of stripping it — on a `strip_with_warning` Route and on a direct same-family switch with no Route policy. Never-captured ids are left untouched, a `reject` Route still refuses first, and a direct target whose adapter declares no placeholder still refuses known non-portable state. The sentinel lives on the adapter, never in the pipeline core. This behavior is regressed by `tests/opaque_state_gemini.rs` against a strict mock that rejects a foreign signature or an unsigned call.
- **Control Plane & Storage (`src/db.rs`, `migrations/`, `src/admin.rs`)**:
  - SQLite database running with WAL mode (`PRAGMA journal_mode=WAL`) managed via SQLx migrations.
  - Persists providers, accounts, models, routes, virtual keys, request logs, token usage, cost accounting, plugin state, and opaque provider state (`opaque_provider_state`, migrated by `20260926120000_opaque_provider_state.sql` and re-keyed for model scoping by `20260927090000_opaque_provider_state_model_scope.sql`).
  - Admin REST API (`/api/*`) for administration, metrics, exports (`src/export.rs`), and diagnostic traces.
- **Embedded Admin Dashboard (`dashboard/`, `src/assets.rs`)**:
  - React 19 + TypeScript + Vite + Tailwind CSS v4 single-page application.
  - Embedded directly into the Rust binary at compile time via `rust-embed`.
- **Plugin Host (`src/plugins/`, `wit/`)**:
  - WebAssembly Component Model runtime powered by `wasmtime` 48.
  - WIT contract defined in `wit/kinetix-plugin.wit` with host capabilities (HTTP egress, logging, key-value storage, credential refreshing).
  - Provider adapters receive the versioned `kinetix.plugin.request` canonical JSON contract and emit `kinetix.plugin.response` v1 envelopes; response events are strictly validated at the host boundary, while auth/body transform failures fail closed before any upstream request is sent.
  - The guest SDK, first-party plugins, catalog source, and packaging tooling live in `PrightCord/kinetix-plugins`.
  - Kinetix vendors official catalog/trust snapshots under `src/plugins/` for offline discovery, and supports dynamic remote synchronization with disk caching (`catalog.cache.json`) and verified CLI / dashboard marketplace installation.
- **CLI & Daemon Runner (`src/main.rs`, `src/cli.rs`, `src/server.rs`)**:
  - Unified binary providing both the proxy daemon (`kinetix serve`) and administrative CLI commands (`kinetix provider`, `model`, `key`, `route`, `user`, etc.).

## boundaries

- **Inbound Protocol Boundary**: Clients communicate exclusively via standard OpenAI Chat Completions, OpenAI Responses, or Anthropic Messages endpoints. Downstream clients never see Kinetix internal types or upstream provider identities (strict topology privacy).
- **Canonical Representation Boundary**: Frontends decode wire requests into internal canonical types. Outbound adapters encode canonical types into upstream wire payloads. Frontends and adapters never couple directly.
- **Outbound Upstream Boundary**: Network requests to upstreams are strictly isolated within adapters. Credentials are leased dynamically and injected at dispatch time; upstream credentials and URLs are scrubbed before logging or downstream propagation.
- **Host / Guest WASM Plugin Boundary**: Plugins run sandboxed in Wasmtime. They interact with the host solely via the WIT contract (`wit/kinetix-plugin.wit`) and are restricted by configured `HostPolicy`.
- **Data Plane vs Control Plane Boundary**: Core request routing and streaming (data plane) remain resilient even if control plane/database operations experience transient latency. Usage logs are queued asynchronously (`src/logqueue.rs`), and opaque-state capture never fails a live response (RAM write is synchronous; SQLite durability runs on a bounded async worker and storage errors/queue drops are counted and swallowed).
- **Opaque Provider-State Boundary**: Adapters only parse/encode provider continuation state; they never persist it. Persistence, scope isolation, and replay live in `src/opaque_state.rs` and are orchestrated by `src/pipeline.rs`. Opaque signatures never appear in client responses, logs, traces, the dashboard, or metrics.

## dependency direction

- **Unidirectional Inward Flow**:
  - Entry point: `src/main.rs` -> `src/cli.rs` -> `src/server.rs` -> `src/app.rs`.
  - HTTP routing: `src/server.rs` mounts `src/frontends/`, `src/admin.rs`, and embedded `src/assets.rs`.
  - Request execution: `src/pipeline.rs` orchestrates `src/router.rs`, `src/registry.rs`, `src/pool.rs`, `src/adapters/`, `src/db.rs`, and `src/plugins/`.
  - Opaque state: `src/pipeline.rs` orchestrates `src/opaque_state.rs` (RAM cache + bounded async durability worker over SQLite) and `src/crypto.rs` (derived cipher); `src/adapters/` only opts in via `Adapter::opaque_state_target` and never calls the store.
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
- **Local CI gate** (mirrors `.github/workflows/ci.yml` exactly: format, dashboard, rust, dependency-policy jobs, with the same dependency graph):
  ```bash
  scripts/run-ci.sh
  ```
- **Skip the dependency-policy (cargo-deny) job** (push-only in ci.yml):
  ```bash
  scripts/run-ci.sh --skip-deps
  ```
- **Individual test suites**:
  ```bash
  # Wire decoding and fixture tests
  cargo test --test decode_fixtures
  cargo test --test wire_fixtures

  # Plugin host subsystem tests
  cargo test --test plugins
  KINETIX_PLUGIN_E2E_PACKAGE=/path/to/plugin.kxp cargo test --test plugin_e2e

  # Gemini opaque-state capture/replay through a translated frontend
  cargo test --test opaque_state_gemini

  # End-to-end smoke test against synthetic upstream
  scripts/smoke.sh 127.0.0.1:8180

  # Hermetic compatibility matrix (#75/#83 profiles + v1 native/translated contract)
  scripts/compat-matrix.sh 127.0.0.1:8186

  # Check generated field-level compatibility docs
  python3 scripts/render-protocol-v1-compat.py --check

  # Manual release-only real-client acceptance (never normal CI).
  # Configure the full model/route matrix documented in docs/compatibility.md.
  KINETIX_BASE=https://kinetix.example.com KINETIX_KEY=sk-kinetix-... \
    KINETIX_ADMIN_TOKEN='admin credential' \
    KINETIX_ACCEPT_PI_SAME_MODEL=direct-openai \
    KINETIX_ACCEPT_PI_TRANSLATED_MODEL=translated-non-openai \
    KINETIX_ACCEPT_PI_FALLBACK_MODEL=forced-fallback-route \
    KINETIX_ACCEPT_PI_AFFINITY_MODEL=sticky-route \
    KINETIX_ACCEPT_CLAUDE_SAME_MODEL=direct-anthropic \
    KINETIX_ACCEPT_CLAUDE_TRANSLATED_MODEL=translated-non-anthropic \
    KINETIX_ACCEPT_CLAUDE_FALLBACK_MODEL=forced-fallback-route \
    KINETIX_ACCEPT_CLAUDE_AFFINITY_MODEL=sticky-route \
    KINETIX_ACCEPT_RESPONSES_OPENAI_MODEL=responses-via-openai \
    KINETIX_ACCEPT_RESPONSES_GEMINI_MODEL=responses-via-gemini \
    KINETIX_ACCEPT_RESPONSES_ANTHROPIC_MODEL=responses-via-anthropic \
    KINETIX_ACCEPT_RESPONSES_FALLBACK_MODEL=forced-fallback-route \
    KINETIX_ACCEPT_RESPONSES_AFFINITY_MODEL=sticky-route \
    bash scripts/release-client-acceptance.sh all
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
- **Protocol acceptance evidence**: `tests/fixtures/protocol-v1-compatibility.json` is authoritative. Every documented field row must cite one or more concrete cases. HTTP cases must be implemented in `scripts/protocol-v1-matrix.py`; positive translation cases must be observable at `scripts/synthetic_upstream.py`. Keep sync + stream coverage for every supported built-in frontend/adapter cell.
- **Real-client acceptance**: `scripts/release-client-acceptance.sh` and `scripts/release-client-proxy.py` are release-only. Never add them to normal CI. Real-client cases must prove multi-turn two-tool continuation, required fallback/affinity behavior, and preserve categorized artifacts without logging credentials.
- **Virtual keys**: Client-facing virtual API keys always use the `sk-kinetix-` prefix.
- **Custom HTTP headers**: All downstream response diagnostic headers must use the `X-Kinetix-` prefix (e.g., `X-Kinetix-Route`, `X-Kinetix-Attempt`, `X-Kinetix-Latency-Ms`).
- **Domain vocabulary**: Use authoritative terms:
  - **Provider**: An upstream service (e.g., OpenAI, Gemini, Anthropic).
  - **Account**: Specific credentials/pool associated with a provider.
  - **Credential enrollment**: How a provider obtains user credentials: `manual`, `auth_flow`, or `none`. Keep this separate from request `auth_scheme`.
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
- **Opaque provider state**: Store provider continuation state (e.g. Gemini `thoughtSignature`) encrypted with its own derived cipher (`kinetix-opaque-provider-state`, never the credential or plugin-KV cipher). Persist only SHA-256 hashes of scope/tool-call/session/tool-name identifiers, never the raw values, and never expose signatures in logs, traces, dashboard, metrics, or errors.

---

# Testing expectations

## unit

- Unit tests must be co-located in their respective modules in `src/` (e.g., `src/predicate.rs`, `src/limits.rs`, `src/cost.rs`, `src/auth.rs`, `src/crypto.rs`).
- Unit tests must be fast, deterministic, hermetic, and offline (no external network calls).
- Run with `cargo test --lib --bins`.

## integration

- **End-to-end proxy behavior**: Validated using `scripts/synthetic_upstream.py` as a deterministic mock upstream server:
  - `scripts/smoke.sh`: Tests routing, wire translation, virtual keys, models discovery, and admin APIs.
  - `scripts/compat-matrix.sh`: Runs the #75/#83 coding-agent profiles plus `scripts/protocol-v1-matrix.py`, which consumes `tests/fixtures/protocol-v1-compatibility.json` for native/translated/fallback compatibility coverage and checks the generated field matrix.
  - `scripts/release-client-acceptance.sh`: Manual release-only Pi/Claude Code/Codex and optional real-`.kxp` acceptance. It may consume provider quota and must never be added to normal CI.
  - `scripts/synthetic_upstream.py` must fail closed when a fixture expects Kinetix to have translated a field: e.g. the `gemini-signature-continuation` fixture returns the provider's real "missing thought_signature" 400 unless the replayed function call carries the exact stored signature, and rejects a never-captured tool-call id. A 200 is thus evidence of replay rather than a vacuous pass.
- **Opaque provider state**:
  - `tests/opaque_state_gemini.rs`: Drives two-turn OpenAI->Gemini tool conversations against a strict mock upstream, covering streaming and non-streaming replay, SQLite-only durability across a simulated restart, cross-scope isolation, and tool-call-id/name reuse rejection before dispatch. It also regresses the portability boundary without session provenance: stored Gemini state reaching an OpenAI-wire target must be rejected by a `reject` Route, stripped with a warning by a `strip_with_warning` Route, and refused outright on a direct target. Gemini 3 Flash<->Pro cross-model continuations are covered against a model-strict mock that accepts only the documented `skip_thought_signature_validator` placeholder on the second model and rejects a foreign signature or an unsigned call; this covers both a `strip_with_warning` Route and a direct same-family switch with no Route. A Gemini 3 -> 2.5 continuation is regressed separately: the mock rejects any `thoughtSignature` on the pre-Gemini-3 model, so the case passes only when Kinetix strips the incompatible state rather than injecting the Gemini 3 validator-bypass sentinel. Identity is validated before the cross-model placeholder is considered: a reused tool-call id with a different name is rejected HTTP 400 pre-dispatch, and a conflicting explicit session reaches the provider unsigned rather than being translated.
- `src/server.rs` `#[cfg(test)]`: `graceful_shutdown_flushes_queued_opaque_state` proves the shutdown path flushes queued opaque-state durability writes before `run()` returns, so a signature accepted just before shutdown survives a restart.
- **Credential refresh and plugin runtime**:
  - `src/credential_refresh.rs` tests cover proactive rotation, unchanged four-minute lease rotation at the original deadline, short-lease rotation without a hot loop when timing hints remain unchanged across multiple successes, and the refreshed-short-lease case.
  - `src/plugins/credential.rs` tests exercise `PluginCredentialStrategy` through opaque lease lookup and coordinator scheduling, proving a changed resolved lease prevents redundant scheduled rotation even though API v1 carries no `rotated` flag.
  - `tests/plugin_auth_retry.rs`: Covers terminal `credential_expired` disabling across inference and token counting, startup seeding, auth retries, and preserves a due schedule when disable persistence fails.
  - `src/admin.rs` credential-enrollment regression tests cover OAuth completion disabling an immediately rejected credential, propagating persistence failure instead of reporting `reauthorization_required`, and avoiding a successful-enrollment audit.
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
- **NEVER push unverified commits directly to remote**: Batch commits locally and verify with `scripts/run-ci.sh` before pushing to conserve CI runner resources.
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
- Local CI gate passes (mirrors `.github/workflows/ci.yml`):
  ```bash
  scripts/run-ci.sh
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

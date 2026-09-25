# Keep changes scoped
Do not refactor, rename, reformat, or change unrelated behavior outside the requested task. Every changed file must be necessary for the requested fix or its tests/docs.

# Preserve frontend canonical adapter boundaries
paths: src/**
Frontends must decode client wire formats into canonical Kinetix types. Outbound adapters must encode canonical types into upstream wire formats. Do not couple a frontend directly to a provider adapter.

# Wire format is not vendor identity
paths: src/**
Do not infer provider identity from `wire_format`. OpenAI-compatible, Anthropic, and Gemini wire behavior must remain protocol-driven unless explicitly implementing a verified vendor-specific rule.

# Do not silently discard protocol semantics
paths: src/frontends/** src/adapters/** src/pipeline.rs src/types.rs
A supported request field must be preserved, explicitly translated, explicitly rejected, or handled by a documented compatibility policy. Never silently drop or weaken a field just to make an upstream accept the request.

# Preserve unknown model metadata
paths: src/** dashboard/**
Missing or unknown model capability, pricing, reasoning, token-limit, modality, or execution metadata must remain unknown. Do not convert unknown values into zero, false, free, unsupported, or an invented default.

# Missing pricing is not zero pricing
paths: src/** dashboard/**
A missing input, output, cache, or thinking price means that price is unknown, not free. Cost/accounting code must not substitute `0` for an unknown consumed pricing dimension.

# Do not invent reasoning capabilities
paths: src/** dashboard/**
Do not invent reasoning levels, defaults, disabled values, or field mappings. Advertised reasoning metadata must match the values the selected adapter can actually send successfully.

# Reasoning mappings must be explicit
paths: src/adapters/** src/plugins/** src/types.rs
Canonical reasoning effort must map explicitly to the upstream representation. Do not silently map one effort level to another unless that mapping is a documented and tested compatibility rule.

# Specialized discovered models need explicit execution support
paths: src/**
A discovered model must not become executable merely because metadata is missing. Server-side validation must prevent stale or incomplete clients from bypassing specialized-model execution restrictions.

# Server invariants do not depend on the dashboard
paths: src/** dashboard/**
Security, credential, routing, discovery, pricing, and execution invariants must be enforced by the Rust backend. Frontend validation may improve UX but must never be the only enforcement.

# Imports obey normal validation
paths: src/**
Config/import paths must enforce the same semantic invariants as normal API, CLI, and dashboard writes. Never write imported credential, model, route, or discovery state directly around authoritative validation.

# Credential mode must match credential behavior
paths: src/** dashboard/**
`credential_mode = none` must not require or invoke credentials. `auth_flow` must have valid integration/plugin provenance and binding. Do not create API-key-shaped enrollment for credential-free or OAuth integrations.

# Never expose upstream credentials
paths: src/** dashboard/**
Never place upstream API keys, OAuth tokens, refresh tokens, decrypted credentials, admin credentials, or raw virtual-key secrets in logs, traces, errors, exports, client responses, or dashboard payloads.

# Preserve topology privacy
paths: src/**
Client-facing responses must not expose internal provider URLs, account labels, credential identity, internal keys, or routing topology. Only intentionally public opaque diagnostic identifiers may cross the downstream boundary.

# Preserve SSRF protections
paths: src/**
Any new path that accepts or uses administrator-controlled upstream URLs must pass through existing URL and SSRF validation. Do not create alternate HTTP execution paths that bypass those protections.

# Never fail over after stream commitment
paths: src/**
Once response headers or any client-visible response/SSE byte has been committed, do not retry another Route target. A later upstream failure must terminate the current response according to protocol semantics.

# Streaming changes need streaming semantics
paths: src/frontends/** src/adapters/** src/pipeline.rs src/sse.rs
Changes affecting responses must preserve event ordering, terminal events, usage, tool-call identity, fragmented tool arguments, cancellation, and error behavior in streaming mode. Sync success alone is insufficient.

# Tool call identity must remain stable
paths: src/frontends/** src/adapters/** src/types.rs
A tool/function call must keep a stable identity across streamed fragments and continuation turns. Do not regenerate IDs while translating or reassembling chunks.

# Session affinity requires explicit identity
paths: src/**
Do not invent or infer conversation identity for sticky/cache-aware routing. Session affinity may use only an explicit supported session identifier supplied by the client.

# Plugin boundaries fail closed
paths: src/plugins/** wit/**
Treat plugin input and output as untrusted. Invalid plugin envelopes, permissions, transforms, credentials, or host requests must fail closed without bypassing sandbox, host-policy, memory, or execution restrictions.

# Plugin contract changes require compatibility handling
paths: src/plugins/** wit/**
A change to the public WIT or versioned plugin request/response contract must include compatibility handling and tests. Do not silently break existing `.kxp` plugins; coordinate required guest SDK/WIT changes with `PrightCord/kinetix-plugins`.

# Database queries stay parameterized
paths: src/** migrations/**
Do not construct SQL containing user or configuration values through string concatenation or formatting. Use SQLx parameters and `.bind(...)` for dynamic values.

# Existing migrations are immutable
paths: migrations/**
Do not modify an already-shipped migration to change schema behavior. Add a new timestamped migration and explicitly handle existing rows and unknown/default values.

# Async paths must not block
paths: src/**
Do not perform blocking I/O or heavy synchronous work on Tokio async workers. Use asynchronous APIs or `spawn_blocking` where required, and keep queues/channels bounded on sustained data-plane paths.

# Bug fixes need regression tests
paths: src/** dashboard/** tests/** scripts/**
A bug fix must include a regression that would fail on the broken behavior and pass after the fix unless testing is genuinely impossible. Test the invariant at the authoritative layer, including negative cases when validation is involved.

# Protocol changes need compatibility evidence
paths: src/frontends/** src/adapters/** src/types.rs tests/fixtures/** scripts/**
Changes to OpenAI Chat, OpenAI Responses, Anthropic Messages, translation, reasoning, tools, vision, or streaming semantics must update the relevant fixture/matrix coverage, including sync and streaming paths where applicable.

# Protocol compatibility fixtures are authoritative
paths: tests/fixtures/protocol-v1-compatibility.json scripts/protocol-v1-matrix.py docs/protocol-v1-compatibility.md
`tests/fixtures/protocol-v1-compatibility.json` is the source of truth for the generated field compatibility matrix. Change the fixture/evidence and regenerate the document; do not hand-edit generated compatibility claims around the fixture.

# Dashboard changes must preserve backend semantics
paths: dashboard/**
Dashboard forms must represent backend-valid values without adding accidental browser restrictions. Do not make mutable backend configuration creation-only or constrain numeric precision beyond an actual backend/domain constraint.

# Dashboard changes must compile
paths: dashboard/**
A dashboard change must pass TypeScript checking and `npm run build`. Do not claim a dashboard task complete while the dashboard bundle fails to build.

# External behavior claims need evidence
paths: src/** dashboard/** docs/** README.md AGENTS.md
Do not encode a provider quirk, unsupported schema keyword, reasoning mapping, model capability, or default based only on another proxy. Use official documentation, observed upstream behavior, or explicit compatibility evidence.

# Public behavior changes require documentation
paths: src/** dashboard/** docs/** README.md AGENTS.md
When public APIs, configuration, endpoints, protocol behavior, architecture, test commands, or repository conventions change, update the relevant documentation and `AGENTS.md` in the same task.

# Local CI is the completion gate
Before claiming a code change is complete, run `scripts/run-ci.sh` unless the task explicitly specifies a narrower validation scope. Do not rely on remote GitHub Actions as the normal development validation loop.

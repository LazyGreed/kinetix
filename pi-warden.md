# Keep changes scoped
Do not refactor, rename, reformat, or change unrelated behavior outside the requested task.

# Preserve protocol boundaries
paths: src/**
Frontends decode client protocols into canonical Kinetix types; adapters encode canonical types into upstream protocols. Do not couple frontends directly to providers or infer provider identity from wire format.

# Do not invent or discard semantics
paths: src/** dashboard/**
Do not silently drop supported request semantics or invent model capabilities, reasoning mappings, pricing, limits, credential behavior, or defaults. Unknown values remain unknown unless backed by verified evidence.

# Backend invariants are authoritative
paths: src/** dashboard/**
Security, credential, routing, discovery, pricing, and execution invariants must be enforced by the backend. Import, API, CLI, and dashboard paths must not bypass authoritative validation.

# Protect secrets and topology
paths: src/** dashboard/**
Never expose upstream credentials, tokens, decrypted secrets, private provider URLs, account identity, or private routing topology in logs, traces, errors, exports, client responses, or dashboard payloads.

# Never fail over after response commitment
paths: src/**
Once any client-visible response or SSE byte has been committed, do not retry another Route target.

# Plugin boundaries fail closed
paths: src/plugins/** wit/**
Treat plugin input and output as untrusted. Invalid credentials, envelopes, transforms, permissions, or host requests must fail closed. Public plugin-contract changes require compatibility handling and tests.

# Preserve database safety
paths: src/** migrations/**
Use parameterized SQL. Never modify an already-shipped migration; add a new timestamped migration instead.

# Keep async paths non-blocking
paths: src/**
Do not perform blocking I/O or heavy synchronous work on Tokio workers. Keep sustained data-plane queues and channels bounded.

# Fixes need regression coverage
paths: src/** dashboard/** tests/** scripts/**
Bug fixes require a regression test when reasonably testable. Protocol changes must update the relevant compatibility evidence and cover streaming where applicable.

# Provider behavior needs evidence
paths: src/** dashboard/** docs/** README.md
Do not encode provider quirks, reasoning mappings, capabilities, schema behavior, or defaults from assumption alone. Prefer official documentation or observed upstream behavior; use 9Router, pi-free, OmniRoute, and similar projects as implementation references.

# Validate before completion
Before claiming a code change is complete, run `scripts/run-ci.sh` unless the task explicitly requires a narrower validation scope.

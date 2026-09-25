-- Host-owned opaque provider continuation state (e.g. Gemini
-- `thoughtSignature`). Persists only the minimum data required to restore a
-- provider's opaque continuation token onto the correct historical tool call
-- when a translated client protocol cannot round-trip it itself.
--
-- Raw client tool-call ids and raw session identifiers are never stored: only
-- SHA-256 hashes scoped by client identity. The opaque value itself is
-- encrypted at rest under a dedicated derived cipher (see src/crypto.rs).
CREATE TABLE opaque_provider_state (
    kind            TEXT NOT NULL,
    scope_hash      TEXT NOT NULL,
    tool_call_hash  TEXT NOT NULL,

    session_hash    TEXT NOT NULL DEFAULT '',
    provider_id     TEXT NOT NULL,
    family          TEXT NOT NULL,
    producer        TEXT NOT NULL,

    origin_model    TEXT NOT NULL DEFAULT '',
    tool_name_hash  TEXT NOT NULL DEFAULT '',

    value_enc       TEXT NOT NULL,

    created_at      TEXT NOT NULL,
    expires_at      TEXT NOT NULL,

    PRIMARY KEY (
        kind,
        scope_hash,
        tool_call_hash,
        provider_id,
        family,
        producer
    )
);

CREATE INDEX idx_opaque_provider_state_expiry
ON opaque_provider_state(expires_at);

CREATE INDEX idx_opaque_provider_state_lookup
ON opaque_provider_state(
    kind,
    scope_hash,
    tool_call_hash
);

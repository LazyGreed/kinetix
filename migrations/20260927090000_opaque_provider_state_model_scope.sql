-- Scope opaque provider continuation state to the exact originating model.
--
-- generateContent thought signatures are only guaranteed to round-trip onto
-- the SAME model that produced them; Google documents that switching models
-- requires dummy signatures rather than reusing the original one. The
-- original opaque_provider_state key (kind, scope_hash, tool_call_hash,
-- provider_id, family, producer) was therefore too coarse: a signature
-- captured on one Gemini model could be handed to a different Gemini model on
-- a later turn. Rebuild the table with origin_model folded into the primary
-- key so cross-model restoration can never happen implicitly.
--
-- Existing rows are preserved and their (possibly empty/unknown) origin_model
-- is carried through with COALESCE; an empty origin_model simply never matches
-- a real model-scoped target, which fails closed.
CREATE TABLE opaque_provider_state_model_scoped (
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
        producer,
        origin_model
    )
);

INSERT OR REPLACE INTO opaque_provider_state_model_scoped (
    kind, scope_hash, tool_call_hash, session_hash, provider_id, family,
    producer, origin_model, tool_name_hash, value_enc, created_at, expires_at
)
SELECT
    kind, scope_hash, tool_call_hash, session_hash, provider_id, family,
    producer, COALESCE(origin_model, ''), tool_name_hash, value_enc,
    created_at, expires_at
FROM opaque_provider_state
ORDER BY created_at DESC;

DROP TABLE opaque_provider_state;

ALTER TABLE opaque_provider_state_model_scoped RENAME TO opaque_provider_state;

CREATE INDEX idx_opaque_provider_state_expiry
ON opaque_provider_state(expires_at);

CREATE INDEX idx_opaque_provider_state_lookup
ON opaque_provider_state(
    kind,
    scope_hash,
    tool_call_hash
);

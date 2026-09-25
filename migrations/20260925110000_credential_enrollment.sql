-- Explicit credential enrollment semantics and plugin provenance.
ALTER TABLE providers ADD COLUMN credential_mode TEXT NOT NULL DEFAULT 'manual';
ALTER TABLE providers ADD COLUMN source_plugin_id TEXT;
ALTER TABLE providers ADD COLUMN source_integration_id TEXT;

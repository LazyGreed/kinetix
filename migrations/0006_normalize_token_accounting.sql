-- Canonical token accounting stores inclusive input/output totals while
-- preserving cache-write as an explicit breakdown dimension.
ALTER TABLE usage_logs ADD COLUMN cache_write_tokens INTEGER;
ALTER TABLE price_versions ADD COLUMN cache_write_per_1m REAL;

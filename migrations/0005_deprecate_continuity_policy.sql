-- Deprecate the legacy routes.continuity_policy control.
--
-- portability_policy is now the single executable cross-provider state policy.
-- Preserve legacy "error" behavior before all runtime/API surfaces stop reading
-- continuity_policy. The column remains for backwards-compatible schema reads
-- from older databases/tools but is no longer user-configurable.
UPDATE routes
SET portability_policy = 'reject'
WHERE continuity_policy = 'error'
  AND portability_policy != 'reject';

UPDATE routes
SET continuity_policy = 'strip'
WHERE continuity_policy != 'strip';

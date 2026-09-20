-- CR #59 r3 P2-1/P2-2: the execution RESULT digest (reported by the
-- executor) and the evidence ARTIFACT digest (sha256 of the submitted
-- bytes) are different data contracts. Record the executor-reported result
-- digest the evidence declares to bind, so strict completion verifies that
-- binding explicitly — missing or contradictory values fail closed instead
-- of being skipped or equated with an artifact digest. Legacy rows keep
-- NULL and fail closed at the strict gate; re-record evidence to bind them.
ALTER TABLE awr_team.evidence
    ADD COLUMN execution_result_digest TEXT;

UPDATE awr_team.schema_state SET version = 8 WHERE component = 'awr_team';

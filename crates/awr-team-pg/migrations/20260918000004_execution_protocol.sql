ALTER TABLE awr_team.executions DROP CONSTRAINT executions_state_check;
ALTER TABLE awr_team.executions
    ADD CONSTRAINT executions_state_check CHECK (state IN (
        'prepared', 'queued', 'accepted', 'running',
        'succeeded', 'failed', 'cancelled', 'unknown'
    ));

ALTER TABLE awr_team.executions
    ADD COLUMN cancel_requested BOOLEAN NOT NULL DEFAULT FALSE,
    ADD COLUMN fencing_class TEXT NOT NULL DEFAULT 'uncontrolled'
        CHECK (fencing_class IN ('hard_fence', 'queryable_idempotent', 'uncontrolled')),
    ADD COLUMN declared_scope_json JSONB NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN observed_paths_json JSONB,
    ADD COLUMN environment_digest TEXT,
    ADD COLUMN unknown_reason TEXT,
    ADD COLUMN scope_id TEXT NOT NULL DEFAULT 'main';

CREATE UNIQUE INDEX executions_effect_key
    ON awr_team.executions (tenant_id, project_id, effect_key)
    WHERE effect_key IS NOT NULL;
CREATE INDEX executions_unknown
    ON awr_team.executions (tenant_id, project_id, work_id)
    WHERE state = 'unknown';

CREATE TABLE awr_team.execution_receipts (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    execution_id TEXT NOT NULL,
    reporter_actor_id TEXT NOT NULL,
    receipt_kind TEXT NOT NULL
        CHECK (receipt_kind IN ('caller_asserted', 'trusted_executor', 'reconcile')),
    digest TEXT NOT NULL,
    payload_json JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, execution_id)
        REFERENCES awr_team.executions(tenant_id, project_id, id)
);

ALTER TABLE awr_team.outbox DROP CONSTRAINT outbox_state_check;
ALTER TABLE awr_team.outbox
    ADD CONSTRAINT outbox_state_check
    CHECK (state IN ('pending', 'sending', 'delivered', 'failed'));
ALTER TABLE awr_team.outbox
    ADD COLUMN action_kind TEXT NOT NULL DEFAULT 'execution.dispatch',
    ADD COLUMN aggregate_id TEXT,
    ADD COLUMN delivery_token TEXT;

CREATE UNIQUE INDEX outbox_one_open_dispatch
    ON awr_team.outbox (tenant_id, project_id, aggregate_id)
    WHERE state IN ('pending', 'sending') AND action_kind = 'execution.dispatch';

ALTER TABLE awr_team.execution_receipts ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.execution_receipts FORCE ROW LEVEL SECURITY;
CREATE POLICY execution_receipts_isolation ON awr_team.execution_receipts
    USING (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true))
    WITH CHECK (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true));

UPDATE awr_team.schema_state SET version = 4 WHERE component = 'awr_team';

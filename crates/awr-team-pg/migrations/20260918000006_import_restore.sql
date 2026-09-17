ALTER TABLE awr_team.import_jobs
    ADD COLUMN project_id TEXT,
    ADD COLUMN identity_map_json JSONB NOT NULL DEFAULT '{}'::jsonb,
    ADD COLUMN missing_evidence_json JSONB NOT NULL DEFAULT '[]'::jsonb,
    ADD COLUMN freeze_required BOOLEAN NOT NULL DEFAULT TRUE;

CREATE TABLE awr_team.backups (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    manifest_hash TEXT NOT NULL,
    coordinator_epoch TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    artifact_digests_json JSONB NOT NULL,
    source_digests_json JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id) REFERENCES awr_team.projects(tenant_id, id)
);

CREATE TABLE awr_team.restore_runs (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    backup_id TEXT NOT NULL,
    new_epoch TEXT NOT NULL,
    outbox_replayed BOOLEAN NOT NULL DEFAULT FALSE,
    state TEXT NOT NULL CHECK (state IN ('completed', 'failed', 'blocked')),
    report_json JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, backup_id)
        REFERENCES awr_team.backups(tenant_id, project_id, id)
);

ALTER TABLE awr_team.backups ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.backups FORCE ROW LEVEL SECURITY;
ALTER TABLE awr_team.restore_runs ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.restore_runs FORCE ROW LEVEL SECURITY;

CREATE POLICY backups_isolation ON awr_team.backups
    USING (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true))
    WITH CHECK (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true));
CREATE POLICY restore_runs_isolation ON awr_team.restore_runs
    USING (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true))
    WITH CHECK (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true));

UPDATE awr_team.schema_state SET version = 6 WHERE component = 'awr_team';

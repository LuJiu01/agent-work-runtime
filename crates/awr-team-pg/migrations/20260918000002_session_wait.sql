CREATE TABLE awr_team.wait_items (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    question TEXT NOT NULL,
    reply TEXT,
    state TEXT NOT NULL CHECK (state IN ('open', 'replied', 'cancelled')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, session_id)
        REFERENCES awr_team.sessions(tenant_id, project_id, id)
);
CREATE UNIQUE INDEX wait_items_one_open
    ON awr_team.wait_items (tenant_id, project_id, work_id)
    WHERE state = 'open';

CREATE TABLE awr_team.checkpoints (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    context_hash TEXT NOT NULL,
    contract_hash TEXT NOT NULL,
    observed_revision BIGINT NOT NULL,
    next_action TEXT NOT NULL,
    open_loops_json JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, session_id)
        REFERENCES awr_team.sessions(tenant_id, project_id, id)
);

ALTER TABLE awr_team.wait_items ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.wait_items FORCE ROW LEVEL SECURITY;
ALTER TABLE awr_team.checkpoints ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.checkpoints FORCE ROW LEVEL SECURITY;

CREATE POLICY wait_items_isolation ON awr_team.wait_items
    USING (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true))
    WITH CHECK (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true));
CREATE POLICY checkpoints_isolation ON awr_team.checkpoints
    USING (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true))
    WITH CHECK (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true));

UPDATE awr_team.schema_state SET version = 2 WHERE component = 'awr_team';

CREATE TABLE awr_team.resource_reservations (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    resource_kind TEXT NOT NULL CHECK (resource_kind IN ('file', 'prefix', 'named')),
    canonical_key TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('reserved', 'released', 'unknown')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, work_id)
        REFERENCES awr_team.work_items(tenant_id, project_id, id)
);
CREATE INDEX resource_reservations_lookup
    ON awr_team.resource_reservations (tenant_id, project_id, state, resource_kind, canonical_key);

CREATE TABLE awr_team.split_proposals (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    parent_work_id TEXT NOT NULL,
    child_work_ids JSONB NOT NULL,
    mapping_json JSONB NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('proposed', 'accepted', 'rejected')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, parent_work_id)
        REFERENCES awr_team.work_items(tenant_id, project_id, id)
);

CREATE TABLE awr_team.dependency_bindings (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    downstream_work_id TEXT NOT NULL,
    upstream_work_id TEXT NOT NULL,
    binding_hash TEXT NOT NULL,
    valid BOOLEAN NOT NULL,
    PRIMARY KEY (tenant_id, project_id, downstream_work_id, upstream_work_id),
    FOREIGN KEY (tenant_id, project_id, downstream_work_id)
        REFERENCES awr_team.work_items(tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, upstream_work_id)
        REFERENCES awr_team.work_items(tenant_id, project_id, id)
);

ALTER TABLE awr_team.resource_reservations ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.resource_reservations FORCE ROW LEVEL SECURITY;
ALTER TABLE awr_team.split_proposals ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.split_proposals FORCE ROW LEVEL SECURITY;
ALTER TABLE awr_team.dependency_bindings ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.dependency_bindings FORCE ROW LEVEL SECURITY;

CREATE POLICY resource_reservations_isolation ON awr_team.resource_reservations
    USING (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true))
    WITH CHECK (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true));
CREATE POLICY split_proposals_isolation ON awr_team.split_proposals
    USING (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true))
    WITH CHECK (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true));
CREATE POLICY dependency_bindings_isolation ON awr_team.dependency_bindings
    USING (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true))
    WITH CHECK (tenant_id = current_setting('awr.tenant_id', true)
       AND project_id = current_setting('awr.project_id', true));

UPDATE awr_team.schema_state SET version = 3 WHERE component = 'awr_team';

CREATE SCHEMA IF NOT EXISTS awr_team;

CREATE TABLE awr_team.schema_state (
    component TEXT PRIMARY KEY,
    version INTEGER NOT NULL CHECK (version >= 1)
);

INSERT INTO awr_team.schema_state(component, version) VALUES ('awr_team', 1);

CREATE TABLE awr_team.tenants (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'disabled')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp()
);

CREATE TABLE awr_team.actors (
    tenant_id TEXT NOT NULL REFERENCES awr_team.tenants(id),
    id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('human', 'agent', 'system')),
    display_name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'disabled')),
    PRIMARY KEY (tenant_id, id)
);

CREATE TABLE awr_team.credentials (
    tenant_id TEXT NOT NULL,
    id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    secret_hash TEXT NOT NULL,
    expires_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    PRIMARY KEY (tenant_id, id),
    FOREIGN KEY (tenant_id, actor_id) REFERENCES awr_team.actors(tenant_id, id)
);

CREATE TABLE awr_team.projects (
    tenant_id TEXT NOT NULL REFERENCES awr_team.tenants(id),
    id TEXT NOT NULL,
    key TEXT NOT NULL,
    mode TEXT NOT NULL CHECK (mode = 'team'),
    coordinator_epoch TEXT NOT NULL,
    authority_epoch BIGINT NOT NULL DEFAULT 0 CHECK (authority_epoch >= 0),
    active_snapshot_id TEXT,
    project_revision BIGINT NOT NULL DEFAULT 0 CHECK (project_revision >= 0),
    status TEXT NOT NULL CHECK (status IN ('active', 'frozen', 'importing', 'degraded')),
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, key)
);

CREATE TABLE awr_team.project_memberships (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin', 'worker', 'reviewer', 'reader')),
    membership_version BIGINT NOT NULL DEFAULT 1,
    PRIMARY KEY (tenant_id, project_id, actor_id),
    FOREIGN KEY (tenant_id, project_id) REFERENCES awr_team.projects(tenant_id, id),
    FOREIGN KEY (tenant_id, actor_id) REFERENCES awr_team.actors(tenant_id, id)
);

CREATE TABLE awr_team.work_scopes (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'readonly', 'archived')),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id) REFERENCES awr_team.projects(tenant_id, id)
);

CREATE TABLE awr_team.source_snapshots (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    manifest_digest TEXT NOT NULL,
    source_ref_json JSONB NOT NULL,
    artifact_id TEXT,
    parser_version TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id) REFERENCES awr_team.projects(tenant_id, id)
);

CREATE TABLE awr_team.source_proposals (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    base_epoch BIGINT NOT NULL,
    candidate_snapshot_id TEXT NOT NULL,
    preview_hash TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'approved', 'rejected', 'activated', 'superseded')),
    reason TEXT NOT NULL,
    author_actor_id TEXT NOT NULL,
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id) REFERENCES awr_team.projects(tenant_id, id),
    FOREIGN KEY (tenant_id, project_id, candidate_snapshot_id)
        REFERENCES awr_team.source_snapshots(tenant_id, project_id, id)
);

CREATE TABLE awr_team.source_approvals (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    proposal_id TEXT NOT NULL,
    candidate_digest TEXT NOT NULL,
    reviewer_actor_id TEXT NOT NULL,
    decision TEXT NOT NULL CHECK (decision IN ('approve', 'reject')),
    decided_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, proposal_id)
        REFERENCES awr_team.source_proposals(tenant_id, project_id, id)
);

CREATE TABLE awr_team.work_items (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    external_key TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    UNIQUE (tenant_id, project_id, external_key),
    FOREIGN KEY (tenant_id, project_id) REFERENCES awr_team.projects(tenant_id, id)
);

CREATE TABLE awr_team.work_contracts (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    snapshot_id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    contract_hash TEXT NOT NULL,
    definition_state TEXT NOT NULL CHECK (definition_state IN ('draft', 'enabled', 'archived')),
    title TEXT NOT NULL,
    contract_json JSONB NOT NULL,
    PRIMARY KEY (tenant_id, project_id, snapshot_id, scope_id, work_id),
    FOREIGN KEY (tenant_id, project_id, snapshot_id)
        REFERENCES awr_team.source_snapshots(tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, scope_id)
        REFERENCES awr_team.work_scopes(tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, work_id)
        REFERENCES awr_team.work_items(tenant_id, project_id, id)
);

CREATE TABLE awr_team.dependency_edges (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    snapshot_id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    from_work_id TEXT NOT NULL,
    to_work_id TEXT NOT NULL,
    relation TEXT NOT NULL,
    required BOOLEAN NOT NULL,
    PRIMARY KEY (tenant_id, project_id, snapshot_id, scope_id, from_work_id, to_work_id, relation),
    FOREIGN KEY (tenant_id, project_id, snapshot_id, scope_id, from_work_id)
        REFERENCES awr_team.work_contracts(tenant_id, project_id, snapshot_id, scope_id, work_id),
    FOREIGN KEY (tenant_id, project_id, snapshot_id, scope_id, to_work_id)
        REFERENCES awr_team.work_contracts(tenant_id, project_id, snapshot_id, scope_id, work_id)
);
CREATE INDEX dependency_edges_from ON awr_team.dependency_edges (tenant_id, project_id, snapshot_id, from_work_id);
CREATE INDEX dependency_edges_to ON awr_team.dependency_edges (tenant_id, project_id, snapshot_id, to_work_id);

CREATE TABLE awr_team.work_runtime (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    state TEXT NOT NULL,
    work_version BIGINT NOT NULL DEFAULT 0,
    last_fence BIGINT NOT NULL DEFAULT 0,
    selected_completion_id TEXT,
    recovery_blocked BOOLEAN NOT NULL DEFAULT FALSE,
    PRIMARY KEY (tenant_id, project_id, scope_id, work_id),
    FOREIGN KEY (tenant_id, project_id, work_id)
        REFERENCES awr_team.work_items(tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, scope_id)
        REFERENCES awr_team.work_scopes(tenant_id, project_id, id)
);

CREATE TABLE awr_team.sessions (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    predecessor_id TEXT,
    state TEXT NOT NULL CHECK (state IN ('active', 'ended', 'interrupted', 'incomplete')),
    session_version BIGINT NOT NULL DEFAULT 1,
    latest_checkpoint_id TEXT,
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id) REFERENCES awr_team.projects(tenant_id, id)
);
CREATE INDEX sessions_actor_state ON awr_team.sessions (tenant_id, project_id, actor_id, client_id, state);

CREATE TABLE awr_team.claims (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    fence BIGINT NOT NULL,
    lease_version BIGINT NOT NULL DEFAULT 1,
    acquired_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    expires_at TIMESTAMPTZ NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('active', 'released', 'expired', 'revoked', 'handed_off')),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, session_id)
        REFERENCES awr_team.sessions(tenant_id, project_id, id)
);
CREATE UNIQUE INDEX claims_one_active
    ON awr_team.claims (tenant_id, project_id, scope_id, work_id)
    WHERE state = 'active';
CREATE INDEX claims_expires ON awr_team.claims (tenant_id, project_id, state, expires_at);

CREATE TABLE awr_team.events (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    project_revision BIGINT NOT NULL,
    event_index INTEGER NOT NULL CHECK (event_index >= 0),
    event_type TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    work_id TEXT,
    payload_json JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    UNIQUE (tenant_id, project_id, project_revision, event_index),
    FOREIGN KEY (tenant_id, project_id) REFERENCES awr_team.projects(tenant_id, id)
);

CREATE TABLE awr_team.operations (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    client_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    op TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('committed', 'rejected')),
    committed_project_revision BIGINT,
    result_json JSONB NOT NULL,
    PRIMARY KEY (tenant_id, project_id, id),
    UNIQUE (tenant_id, project_id, actor_id, client_id, request_id),
    FOREIGN KEY (tenant_id, project_id) REFERENCES awr_team.projects(tenant_id, id)
);

CREATE TABLE awr_team.executions (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    session_id TEXT,
    claim_id TEXT,
    fence BIGINT NOT NULL,
    contract_hash TEXT NOT NULL,
    input_digest TEXT,
    executor_actor_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('prepared', 'running', 'succeeded', 'failed', 'unknown', 'cancelled')),
    effect_key TEXT,
    result_digest TEXT,
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, work_id)
        REFERENCES awr_team.work_items(tenant_id, project_id, id)
);
CREATE INDEX executions_work_state ON awr_team.executions (tenant_id, project_id, work_id, state);

CREATE TABLE awr_team.artifacts (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    object_key TEXT NOT NULL,
    sha256 TEXT NOT NULL,
    byte_length BIGINT NOT NULL CHECK (byte_length >= 0),
    media_type TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('uploading', 'finalized', 'retained', 'missing')),
    created_by TEXT NOT NULL,
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id) REFERENCES awr_team.projects(tenant_id, id)
);

CREATE TABLE awr_team.outbox (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    available_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    state TEXT NOT NULL CHECK (state IN ('pending', 'delivered', 'failed')),
    payload_json JSONB NOT NULL,
    delivery_attempts INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, project_id, id)
);
CREATE INDEX outbox_ready ON awr_team.outbox (state, available_at, id);

CREATE TABLE awr_team.import_jobs (
    tenant_id TEXT NOT NULL,
    id TEXT NOT NULL,
    import_key TEXT NOT NULL,
    manifest_hash TEXT NOT NULL,
    state TEXT NOT NULL,
    report_json JSONB NOT NULL,
    PRIMARY KEY (tenant_id, id),
    UNIQUE (tenant_id, import_key, manifest_hash)
);

DO $$
DECLARE t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'tenants','actors','credentials','projects','project_memberships','work_scopes',
        'source_snapshots','source_proposals','source_approvals','work_items','work_contracts',
        'dependency_edges','work_runtime','sessions','claims','events','operations','executions',
        'artifacts','outbox','import_jobs'
    ]
    LOOP
        EXECUTE format('ALTER TABLE awr_team.%I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('ALTER TABLE awr_team.%I FORCE ROW LEVEL SECURITY', t);
    END LOOP;
END $$;

CREATE POLICY tenants_isolation ON awr_team.tenants
    USING (id = current_setting('awr.tenant_id', true));
CREATE POLICY actors_isolation ON awr_team.actors
    USING (tenant_id = current_setting('awr.tenant_id', true));
CREATE POLICY credentials_isolation ON awr_team.credentials
    USING (tenant_id = current_setting('awr.tenant_id', true));
CREATE POLICY projects_isolation ON awr_team.projects
    USING (tenant_id = current_setting('awr.tenant_id', true)
       AND id = current_setting('awr.project_id', true))
    WITH CHECK (tenant_id = current_setting('awr.tenant_id', true)
       AND id = current_setting('awr.project_id', true));

DO $$
DECLARE t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'project_memberships','work_scopes','source_snapshots','source_proposals','source_approvals',
        'work_items','work_contracts','dependency_edges','work_runtime','sessions','claims','events',
        'operations','executions','artifacts','outbox'
    ]
    LOOP
        EXECUTE format(
            'CREATE POLICY %I_isolation ON awr_team.%I
             USING (tenant_id = current_setting(''awr.tenant_id'', true)
                AND project_id = current_setting(''awr.project_id'', true))
             WITH CHECK (tenant_id = current_setting(''awr.tenant_id'', true)
                AND project_id = current_setting(''awr.project_id'', true))',
            t, t);
    END LOOP;
END $$;

CREATE POLICY import_jobs_isolation ON awr_team.import_jobs
    USING (tenant_id = current_setting('awr.tenant_id', true));

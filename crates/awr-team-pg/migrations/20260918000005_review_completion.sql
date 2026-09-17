CREATE TABLE awr_team.evidence (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    execution_id TEXT,
    artifact_id TEXT,
    contract_hash TEXT NOT NULL,
    input_digest TEXT,
    output_digest TEXT,
    evidence_kind TEXT NOT NULL CHECK (evidence_kind IN ('report', 'artifact', 'review_bundle', 'confirmation')),
    trust_basis TEXT NOT NULL CHECK (trust_basis IN ('caller_asserted', 'trusted_executor', 'human_review')),
    digest TEXT NOT NULL,
    payload_json JSONB NOT NULL,
    created_by TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, work_id)
        REFERENCES awr_team.work_items(tenant_id, project_id, id)
);

CREATE TABLE awr_team.review_rounds (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    round_index INTEGER NOT NULL CHECK (round_index >= 1),
    bundle_hash TEXT NOT NULL,
    contract_hash TEXT NOT NULL,
    author_actor_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('open', 'approved', 'rejected', 'invalidated')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    UNIQUE (tenant_id, project_id, work_id, round_index),
    FOREIGN KEY (tenant_id, project_id, work_id)
        REFERENCES awr_team.work_items(tenant_id, project_id, id)
);

CREATE TABLE awr_team.review_decisions (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    review_round_id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    bundle_hash TEXT NOT NULL,
    reviewer_actor_id TEXT NOT NULL,
    decision TEXT NOT NULL CHECK (decision IN ('approve', 'reject')),
    reason TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, review_round_id)
        REFERENCES awr_team.review_rounds(tenant_id, project_id, id)
);

CREATE TABLE awr_team.completion_receipts (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    id TEXT NOT NULL,
    work_id TEXT NOT NULL,
    scope_id TEXT NOT NULL,
    contract_hash TEXT NOT NULL,
    result_digest TEXT NOT NULL,
    dependency_binding_hash TEXT NOT NULL,
    evidence_bundle_hash TEXT NOT NULL,
    policy TEXT NOT NULL,
    approved_by_json JSONB NOT NULL,
    accepted_at TIMESTAMPTZ NOT NULL DEFAULT clock_timestamp(),
    PRIMARY KEY (tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, work_id)
        REFERENCES awr_team.work_items(tenant_id, project_id, id)
);

CREATE TABLE awr_team.completion_evidence (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    completion_id TEXT NOT NULL,
    evidence_id TEXT NOT NULL,
    criterion_id TEXT NOT NULL,
    PRIMARY KEY (tenant_id, project_id, completion_id, evidence_id, criterion_id),
    FOREIGN KEY (tenant_id, project_id, completion_id)
        REFERENCES awr_team.completion_receipts(tenant_id, project_id, id),
    FOREIGN KEY (tenant_id, project_id, evidence_id)
        REFERENCES awr_team.evidence(tenant_id, project_id, id)
);

CREATE TABLE awr_team.completion_dependencies (
    tenant_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    completion_id TEXT NOT NULL,
    predecessor_work_id TEXT NOT NULL,
    predecessor_completion_id TEXT NOT NULL,
    PRIMARY KEY (tenant_id, project_id, completion_id, predecessor_work_id),
    FOREIGN KEY (tenant_id, project_id, completion_id)
        REFERENCES awr_team.completion_receipts(tenant_id, project_id, id)
);

ALTER TABLE awr_team.work_runtime
    ADD CONSTRAINT work_completed_needs_receipt
    CHECK (state <> 'completed' OR selected_completion_id IS NOT NULL);

ALTER TABLE awr_team.evidence ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.evidence FORCE ROW LEVEL SECURITY;
ALTER TABLE awr_team.review_rounds ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.review_rounds FORCE ROW LEVEL SECURITY;
ALTER TABLE awr_team.review_decisions ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.review_decisions FORCE ROW LEVEL SECURITY;
ALTER TABLE awr_team.completion_receipts ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.completion_receipts FORCE ROW LEVEL SECURITY;
ALTER TABLE awr_team.completion_evidence ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.completion_evidence FORCE ROW LEVEL SECURITY;
ALTER TABLE awr_team.completion_dependencies ENABLE ROW LEVEL SECURITY;
ALTER TABLE awr_team.completion_dependencies FORCE ROW LEVEL SECURITY;

DO $$
DECLARE t TEXT;
BEGIN
    FOREACH t IN ARRAY ARRAY[
        'evidence','review_rounds','review_decisions','completion_receipts',
        'completion_evidence','completion_dependencies'
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

UPDATE awr_team.schema_state SET version = 5 WHERE component = 'awr_team';

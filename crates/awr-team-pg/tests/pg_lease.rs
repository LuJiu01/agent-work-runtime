#![cfg(feature = "pg-tests")]
//! TEAM-P5 lease/claim/wait/handoff tests on real PostgreSQL.
//! Fixture isolation comes from tests/common (CR #39 P2-7): this file never
//! reads the runtime AWR_TEAM_DATABASE_URL and only cleans the database this
//! process created.

use awr_team_pg::{CommandRequest, LeaseStore, PgError, ReadStore, TeamStore};
use std::sync::MutexGuard;
use tokio_postgres::Client;

mod common;
use common::{fresh_team_schema, gate_db_name, test_config, test_database_url_raw, with_app_role};

const TENANT: &str = "tenant-a";
const PROJECT: &str = "project-a";
const ACTOR: &str = "actor-a";
const OTHER: &str = "actor-b";
const CLIENT: &str = "client-a";
const CLIENT_B: &str = "client-b";

async fn setup() -> (MutexGuard<'static, ()>, Client, LeaseStore, String) {
    let (guard, admin, db) = fresh_team_schema().await;
    admin
        .batch_execute(
            "INSERT INTO awr_team.tenants(id,name,status) VALUES ('tenant-a','A','active');
             INSERT INTO awr_team.actors(tenant_id,id,kind,display_name,status) VALUES
                ('tenant-a','actor-a','agent','A','active'),
                ('tenant-a','actor-b','human','B','active');
             INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status)
                VALUES ('tenant-a','project-a','alpha','team','epoch-1','active');
             INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status)
                VALUES ('tenant-a','project-a','main','main','active');
             INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key) VALUES
                ('tenant-a','project-a','work-a','W'),
                ('tenant-a','project-a','work-b','X');",
        )
        .await
        .unwrap();
    let store = LeaseStore::from_config(with_app_role(&test_config(), &db));
    (guard, admin, store, db)
}

#[tokio::test]
async fn only_one_active_claim_wins_and_unique_index_holds() {
    let (_lock, admin, store, _db) = setup().await;
    let left = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "conv-a", "main", "work-a")
        .await
        .unwrap();
    let right = store
        .start_session(TENANT, PROJECT, OTHER, CLIENT_B, "conv-b", "main", "work-a")
        .await
        .unwrap();
    store
        .claim(TENANT, PROJECT, &left.id, ACTOR, CLIENT, "w1", 60)
        .await
        .unwrap();
    let err = store
        .claim(TENANT, PROJECT, &right.id, OTHER, CLIENT_B, "w2", 60)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::ClaimHeld));
    let active: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.claims WHERE work_id='work-a' AND state='active'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(active, 1);
}

#[tokio::test]
async fn different_work_items_do_not_share_a_global_cas() {
    let (_lock, _, store, _db) = setup().await;
    let a = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c1", "main", "work-a")
        .await
        .unwrap();
    let b = store
        .start_session(TENANT, PROJECT, OTHER, CLIENT_B, "c2", "main", "work-b")
        .await
        .unwrap();
    store
        .claim(TENANT, PROJECT, &a.id, ACTOR, CLIENT, "w1", 60)
        .await
        .unwrap();
    store
        .claim(TENANT, PROJECT, &b.id, OTHER, CLIENT_B, "w2", 60)
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_claim_is_not_revived_and_row_is_kept() {
    let (_lock, admin, store, _db) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 1)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    store
        .expire_due(TENANT, PROJECT, "main", "work-a")
        .await
        .unwrap();
    let err = store
        .renew(TENANT, PROJECT, &claim.id, ACTOR, CLIENT, "late", 60)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::LeaseExpired));
    let state: String = admin
        .query_one(
            "SELECT state FROM awr_team.claims WHERE id=$1",
            &[&claim.id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(state, "expired");
}

#[tokio::test]
async fn renew_replay_does_not_move_expiry_and_handoff_invalidates_old_fence() {
    let (_lock, _, store, _db) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    let first = store
        .renew(TENANT, PROJECT, &claim.id, ACTOR, CLIENT, "hb1", 60)
        .await
        .unwrap();
    let replay = store
        .renew(TENANT, PROJECT, &claim.id, ACTOR, CLIENT, "hb1", 60)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(first.expires_at, replay.expires_at);
    assert_eq!(first.lease_version, replay.lease_version);
    store
        .require_fence(TENANT, PROJECT, "main", "work-a", ACTOR, claim.fence)
        .await
        .unwrap();
    let handed = store
        .handoff(TENANT, PROJECT, &claim.id, ACTOR, OTHER, CLIENT_B, "next")
        .await
        .unwrap();
    assert!(handed.fence > claim.fence);
    let err = store
        .require_fence(TENANT, PROJECT, "main", "work-a", ACTOR, claim.fence)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::StaleFence | PgError::LeaseExpired));
}

#[tokio::test]
async fn wait_does_not_renew_and_recovery_block_stops_new_claims() {
    let (_lock, _, store, _db) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    store
        .wait(TENANT, PROJECT, &session.id, ACTOR, "need review?")
        .await
        .unwrap();
    let rival = store
        .start_session(TENANT, PROJECT, OTHER, CLIENT_B, "c2", "main", "work-a")
        .await
        .unwrap();
    let wait_err = store
        .claim(
            TENANT,
            PROJECT,
            &rival.id,
            OTHER,
            CLIENT_B,
            "blocked-by-wait",
            60,
        )
        .await
        .unwrap_err();
    // Precisely WaitOpen: the wait gate alone must block the rival, not be
    // masked by the claim-held check (CR #39 test-strength note).
    assert!(matches!(wait_err, PgError::WaitOpen), "got {wait_err}");
    store
        .set_recovery_blocked(TENANT, PROJECT, "main", "work-b", true)
        .await
        .unwrap();
    let other = store
        .start_session(TENANT, PROJECT, OTHER, CLIENT_B, "c3", "main", "work-b")
        .await
        .unwrap();
    let err = store
        .claim(TENANT, PROJECT, &other.id, OTHER, CLIENT_B, "rb", 60)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::RecoveryBlocked));
}

#[tokio::test]
async fn cannot_release_someone_elses_claim() {
    let (_lock, _, store, _db) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    let err = store
        .release(TENANT, PROJECT, &claim.id, OTHER)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::Forbidden));
}

// CR #39 P2-1: idempotent replay binds op + target + ttl. Same key with a
// different target is a conflict; identical calls replay; a stored receipt
// is never fabricated from defaults.
#[tokio::test]
async fn idempotent_replay_validates_target_and_ttl() {
    let (_lock, _, store, _db) = setup().await;
    let a = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "ca", "main", "work-a")
        .await
        .unwrap();
    let b = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "cb", "main", "work-b")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &a.id, ACTOR, CLIENT, "req-1", 60)
        .await
        .unwrap();
    let replay = store
        .claim(TENANT, PROJECT, &a.id, ACTOR, CLIENT, "req-1", 60)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.id, claim.id);
    let conflict = store
        .claim(TENANT, PROJECT, &b.id, ACTOR, CLIENT, "req-1", 60)
        .await
        .unwrap_err();
    assert!(
        matches!(conflict, PgError::IdempotencyConflict),
        "got {conflict}"
    );
    let renewed = store
        .renew(TENANT, PROJECT, &claim.id, ACTOR, CLIENT, "req-2", 60)
        .await
        .unwrap();
    let conflict = store
        .renew(TENANT, PROJECT, &claim.id, ACTOR, CLIENT, "req-2", 600)
        .await
        .unwrap_err();
    assert!(
        matches!(conflict, PgError::IdempotencyConflict),
        "got {conflict}"
    );
    let replayed = store
        .renew(TENANT, PROJECT, &claim.id, ACTOR, CLIENT, "req-2", 60)
        .await
        .unwrap();
    assert!(replayed.replayed);
    assert_eq!(replayed.expires_at, renewed.expires_at);
}

// CR #39 P2-2: a claim whose time passed but whose state is still 'active'
// (expiry sweep not run) must NOT handoff, wait, or pass require_fence.
// The fixture backdates expires_at directly and calls the interfaces
// WITHOUT running expire_due first.
#[tokio::test]
async fn expired_but_unswept_claim_cannot_handoff_wait_or_fence() {
    let (_lock, admin, store, _db) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    admin
        .execute(
            "UPDATE awr_team.claims SET expires_at = clock_timestamp() - interval '1 second' WHERE id=$1",
            &[&claim.id],
        )
        .await
        .unwrap();
    let err = store
        .handoff(TENANT, PROJECT, &claim.id, ACTOR, OTHER, CLIENT_B, "next")
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::LeaseExpired), "handoff: got {err}");
    let successors: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.claims WHERE work_id='work-a' AND state='active' AND id<>$1",
            &[&claim.id],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(successors, 0, "rejected handoff created a successor claim");
    let fence: i64 = admin
        .query_one(
            "SELECT last_fence FROM awr_team.work_runtime WHERE work_id='work-a'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(fence, 1, "rejected handoff bumped the fence");
    let err = store
        .wait(TENANT, PROJECT, &session.id, ACTOR, "still there?")
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::LeaseExpired), "wait: got {err}");
    let err = store
        .require_fence(TENANT, PROJECT, "main", "work-a", ACTOR, claim.fence)
        .await
        .unwrap_err();
    assert!(
        matches!(err, PgError::LeaseExpired),
        "require_fence: got {err}"
    );
}

// CR #39 P2-3: renew keeps the session's client binding; the same actor via
// a different client is rejected, the bound client renews fine.
#[tokio::test]
async fn renew_keeps_the_session_client_binding() {
    let (_lock, _, store, _db) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    let err = store
        .renew(
            TENANT,
            PROJECT,
            &claim.id,
            ACTOR,
            CLIENT_B,
            "hb-other-client",
            60,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::Forbidden), "got {err}");
    store
        .renew(TENANT, PROJECT, &claim.id, ACTOR, CLIENT, "hb-bound", 60)
        .await
        .unwrap();
}

// CR #39 P2-4: two legitimate claims in different scopes do not confuse the
// fence check; each scope validates its own holder.
#[tokio::test]
async fn require_fence_is_scoped() {
    let (_lock, admin, store, _db) = setup().await;
    admin
        .batch_execute(
            "INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status)
             VALUES ('tenant-a','project-a','review','review','active');",
        )
        .await
        .unwrap();
    let a = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c1", "main", "work-a")
        .await
        .unwrap();
    let b = store
        .start_session(TENANT, PROJECT, OTHER, CLIENT_B, "c2", "review", "work-a")
        .await
        .unwrap();
    let main_claim = store
        .claim(TENANT, PROJECT, &a.id, ACTOR, CLIENT, "m1", 60)
        .await
        .unwrap();
    let review_claim = store
        .claim(TENANT, PROJECT, &b.id, OTHER, CLIENT_B, "r1", 60)
        .await
        .unwrap();
    store
        .require_fence(TENANT, PROJECT, "main", "work-a", ACTOR, main_claim.fence)
        .await
        .unwrap();
    store
        .require_fence(
            TENANT,
            PROJECT,
            "review",
            "work-a",
            OTHER,
            review_claim.fence,
        )
        .await
        .unwrap();
    // Wrong holder in the review scope must fail against the review claim,
    // proving the lookup is scoped (fences are per-scope counters and both
    // start at 1, so a same-value cross-scope check would pass legitimately).
    let err = store
        .require_fence(
            TENANT,
            PROJECT,
            "review",
            "work-a",
            ACTOR,
            review_claim.fence,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::StaleFence));
}

// CR #39 P2-5: acquire/release/handoff/wait advance the project revision
// and emit events in the same transaction; idempotent replays do not
// duplicate events.
#[tokio::test]
async fn lease_state_changes_emit_events_atomically() {
    let (_lock, admin, store, db) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    let replay = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    assert!(replay.replayed);
    let revision: i64 = admin
        .query_one(
            "SELECT project_revision FROM awr_team.projects WHERE id='project-a'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(revision, 1, "replayed claim bumped the revision");
    let events: Vec<(String, i64)> = admin
        .query(
            "SELECT event_type, project_revision FROM awr_team.events WHERE project_id='project-a' ORDER BY project_revision",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(events, vec![("claim.acquired".to_string(), 1)]);
    store
        .wait(TENANT, PROJECT, &session.id, ACTOR, "q?")
        .await
        .unwrap();
    store
        .handoff(TENANT, PROJECT, &claim.id, ACTOR, OTHER, CLIENT_B, "next")
        .await
        .unwrap();
    let events: Vec<(String, i64)> = admin
        .query(
            "SELECT event_type, project_revision FROM awr_team.events WHERE project_id='project-a' ORDER BY project_revision",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1)))
        .collect();
    assert_eq!(
        events,
        vec![
            ("claim.acquired".to_string(), 1),
            ("wait.opened".to_string(), 2),
            ("claim.handed_off".to_string(), 3),
        ]
    );
    // The event stream is visible through the standard read path with a cursor.
    let page = ReadStore::from_config(with_app_role(&test_config(), &db))
        .list_events(TENANT, PROJECT, None, 10)
        .await
        .unwrap();
    assert_eq!(page.events.len(), 3);
}

// CR #39 P2-6: fence/lease_version are decimal strings in responses and
// receipts; legacy numeric receipts still replay.
#[tokio::test]
async fn fence_and_lease_version_are_decimal_strings() {
    let (_lock, admin, store, _db) = setup().await;
    admin
        .batch_execute(
            "INSERT INTO awr_team.work_runtime(tenant_id,project_id,scope_id,work_id,state,work_version,last_fence)
             VALUES ('tenant-a','project-a','main','work-b','claimed',1,9007199254740992)",
        )
        .await
        .unwrap();
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-b")
        .await
        .unwrap();
    let claim = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "big", 60)
        .await
        .unwrap();
    assert_eq!(claim.fence, 9_007_199_254_740_993);
    let serialized = serde_json::to_value(&claim).unwrap();
    assert_eq!(serialized["fence"], serde_json::json!("9007199254740993"));
    assert_eq!(serialized["lease_version"], serde_json::json!("1"));
    let receipt: serde_json::Value = admin
        .query_one(
            "SELECT result_json FROM awr_team.operations WHERE request_id='big'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(receipt["fence"], serde_json::json!("9007199254740993"));
    // Legacy numeric receipt form still replays (compat policy).
    admin
        .execute(
            "UPDATE awr_team.operations
             SET result_json = jsonb_set(result_json, '{fence}', '9007199254740993'::jsonb)
             WHERE request_id='big'",
            &[],
        )
        .await
        .unwrap();
    let replay = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "big", 60)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.fence, claim.fence);
}
// CR #56 P2-1: cross-store request-key collisions are conflicts in BOTH
// directions, never a panic and never a phantom replay.
#[tokio::test]
async fn cross_store_request_key_collisions_conflict_safely() {
    let (_lock, admin, store, db) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    store
        .claim(
            TENANT,
            PROJECT,
            &session.id,
            ACTOR,
            CLIENT,
            "shared-key",
            60,
        )
        .await
        .unwrap();
    let commands = TeamStore::from_config(with_app_role(&test_config(), &db));
    let touch = |request_id: &str| CommandRequest {
        tenant_id: TENANT.into(),
        project_id: PROJECT.into(),
        actor_id: ACTOR.into(),
        client_id: CLIENT.into(),
        request_id: request_id.into(),
        op: "work.touch".into(),
        args: serde_json::json!({"work_id": "work-a", "scope_id": "main"}),
    };
    // lease receipt (has committed revision after the fix) -> touch must conflict
    let err = commands.execute(touch("shared-key")).await.unwrap_err();
    assert!(matches!(err, PgError::IdempotencyConflict), "got {err}");
    // touch receipt -> claim with the same key must also conflict
    commands.execute(touch("touch-first")).await.unwrap();
    let other = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c2", "main", "work-b")
        .await
        .unwrap();
    let err = store
        .claim(TENANT, PROJECT, &other.id, ACTOR, CLIENT, "touch-first", 60)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::IdempotencyConflict), "got {err}");
    // Legacy NULL-revision lease receipts must not panic either: they are
    // plain conflicts now.
    admin
        .execute(
            "UPDATE awr_team.operations SET committed_project_revision=NULL
             WHERE request_id='shared-key'",
            &[],
        )
        .await
        .unwrap();
    let err = commands.execute(touch("shared-key")).await.unwrap_err();
    assert!(matches!(err, PgError::IdempotencyConflict), "got {err}");
    let revision: i64 = admin
        .query_one(
            "SELECT project_revision FROM awr_team.projects WHERE id='project-a'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(revision, 2, "conflicts must not advance state");
}

// CR #56 P2-2: the reply event records the actual replier, and the session
// id is kept as its own field instead of impersonating an actor.
#[tokio::test]
async fn reply_event_records_the_actual_replier() {
    let (_lock, admin, store, _db) = setup().await;
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap();
    let wait_id = store
        .wait(TENANT, PROJECT, &session.id, ACTOR, "review?")
        .await
        .unwrap();
    store
        .reply(TENANT, PROJECT, &wait_id, OTHER, "looks good")
        .await
        .unwrap();
    let event_row = admin
        .query_one(
            "SELECT actor_id, payload_json FROM awr_team.events WHERE event_type='wait.replied'",
            &[],
        )
        .await
        .unwrap();
    let actor: String = event_row.get(0);
    let payload: serde_json::Value = event_row.get(1);
    assert_eq!(actor, OTHER, "event actor must be the replier");
    assert_ne!(actor, session.id, "event actor must not be the session id");
    assert_eq!(payload["session_id"], serde_json::json!(session.id));
}

// CR #56 P2-3: recovery-block transitions and the expiry sweep emit events;
// a no-op sweep emits nothing.
#[tokio::test]
async fn recovery_block_and_expiry_emit_events() {
    let (_lock, admin, store, _db) = setup().await;
    store
        .set_recovery_blocked(TENANT, PROJECT, "main", "work-b", true)
        .await
        .unwrap();
    store
        .set_recovery_blocked(TENANT, PROJECT, "main", "work-b", false)
        .await
        .unwrap();
    let events: Vec<String> = admin
        .query(
            "SELECT event_type FROM awr_team.events WHERE project_id='project-a' ORDER BY project_revision",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(
        events,
        vec![
            "work.recovery_blocked".to_string(),
            "work.recovery_unblocked".to_string()
        ]
    );
    // Expiry sweep reports exactly the claims that transitioned.
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "short", 1)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
    let swept = store
        .expire_due(TENANT, PROJECT, "main", "work-a")
        .await
        .unwrap();
    assert_eq!(swept, 1);
    let swept_again = store
        .expire_due(TENANT, PROJECT, "main", "work-a")
        .await
        .unwrap();
    assert_eq!(swept_again, 0);
    let expired_events: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.events WHERE event_type='claim.expired'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(expired_events, 1, "no-op sweep emitted a duplicate event");
}

// CR #56 (case 5): when the event insert fails, the whole claim rolls back.
#[tokio::test]
async fn failed_event_insert_rolls_back_the_claim() {
    let (_lock, admin, store, _db) = setup().await;
    admin
        .batch_execute(
            "CREATE OR REPLACE FUNCTION awr_team.fail_event_insert() RETURNS trigger
             LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected event failure'; END; $$;
             CREATE TRIGGER fail_event BEFORE INSERT ON awr_team.events
             FOR EACH ROW EXECUTE FUNCTION awr_team.fail_event_insert();",
        )
        .await
        .unwrap();
    let session = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "c", "main", "work-a")
        .await
        .unwrap();
    let err = store
        .claim(TENANT, PROJECT, &session.id, ACTOR, CLIENT, "c1", 60)
        .await
        .unwrap_err();
    assert!(matches!(err, PgError::Db(_)), "got {err}");
    admin
        .batch_execute(
            "DROP TRIGGER fail_event ON awr_team.events;
             DROP FUNCTION awr_team.fail_event_insert();",
        )
        .await
        .unwrap();
    let claims: i64 = admin
        .query_one("SELECT count(*) FROM awr_team.claims", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(claims, 0, "failed event insert left a claim");
    let revision: i64 = admin
        .query_one(
            "SELECT project_revision FROM awr_team.projects WHERE id='project-a'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(revision, 0, "failed event insert bumped the revision");
    let receipts: i64 = admin
        .query_one("SELECT count(*) FROM awr_team.operations", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(receipts, 0, "failed event insert left a receipt");
}

// CR #56 note: keep the concurrent preemption coverage alongside the
// sequential ClaimHeld assertion.
#[tokio::test]
async fn concurrent_claims_exactly_one_wins() {
    let (_lock, admin, store, _db) = setup().await;
    let left = store
        .start_session(TENANT, PROJECT, ACTOR, CLIENT, "ca", "main", "work-a")
        .await
        .unwrap();
    let right = store
        .start_session(TENANT, PROJECT, OTHER, CLIENT_B, "cb", "main", "work-a")
        .await
        .unwrap();
    let (a, b) = tokio::join!(
        store.claim(TENANT, PROJECT, &left.id, ACTOR, CLIENT, "ra", 60),
        store.claim(TENANT, PROJECT, &right.id, OTHER, CLIENT_B, "rb", 60)
    );
    let wins = [a.is_ok(), b.is_ok()].iter().filter(|x| **x).count();
    assert_eq!(
        wins, 1,
        "concurrent claims must settle on exactly one holder"
    );
    let active: i64 = admin
        .query_one(
            "SELECT count(*) FROM awr_team.claims WHERE work_id='work-a' AND state='active'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(active, 1);
}

// CR #56 P2-4: the replay driver's actual stdout carries fence as a string.
// The child receives the SAME raw connection string plus this run's database
// name (never a re-serialized URL), and the driver binary is located from
// Cargo's own artifact output, so custom target dirs and profiles work.
#[tokio::test]
async fn replay_driver_stdout_has_decimal_string_fence() {
    let (_lock, admin, _store, _db) = setup().await;
    admin
        .batch_execute(
            "INSERT INTO awr_team.tenants(id,name,status) VALUES ('tenant-t13','T13','active');
             INSERT INTO awr_team.actors(tenant_id,id,kind,display_name,status) VALUES
                ('tenant-t13','kimi-cli','agent','Kimi CLI','active');
             INSERT INTO awr_team.projects(tenant_id,id,key,mode,coordinator_epoch,status)
                VALUES ('tenant-t13','project-tc003','tc003','team','epoch-tc003','active');
             INSERT INTO awr_team.work_scopes(tenant_id,project_id,id,name,status)
                VALUES ('tenant-t13','project-tc003','main','main','active');
             INSERT INTO awr_team.work_items(tenant_id,project_id,id,external_key)
                VALUES ('tenant-t13','project-tc003','work-tc003','TC003');",
        )
        .await
        .unwrap();
    let db = gate_db_name().await;
    let executable = build_example_and_locate("tc_replay");
    let output = std::process::Command::new(executable)
        .args([
            "claim",
            "project-tc003",
            "work-tc003",
            "kimi-cli",
            "kimi",
            "conv-1",
        ])
        .env("AWR_TEAM_DATABASE_URL", test_database_url_raw())
        .env("TC_DB", &db)
        .output()
        .expect("run the tc_replay driver built by this Cargo invocation");
    assert!(
        output.status.success(),
        "driver failed: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stdout: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(
        stdout["fence"].is_string(),
        "driver stdout fence must be a decimal string: {stdout}"
    );
}

/// Build the example through Cargo itself and locate the executable from
/// the compiler-artifact JSON, so CARGO_TARGET_DIR, --target-dir and release
/// profiles all resolve to THIS build's output (CR #56 round 3). Fails
/// loudly if the artifact cannot be produced or found.
fn build_example_and_locate(example: &str) -> String {
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let output =
        std::process::Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args([
                "build",
                "-p",
                "awr-team-pg",
                "--example",
                example,
                "--message-format=json",
            ])
            .current_dir(format!("{manifest_dir}/../.."))
            .output()
            .expect("invoke cargo build for the example");
    assert!(
        output.status.success(),
        "cargo build --example {example} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] != "compiler-artifact" {
            continue;
        }
        let is_example = message["target"]["kind"]
            .as_array()
            .map(|k| k.iter().any(|v| v == "example"))
            .unwrap_or(false);
        if is_example && message["target"]["name"] == example {
            if let Some(executable) = message["executable"].as_str() {
                return executable.to_string();
            }
        }
    }
    panic!("cargo did not report an executable for example {example}");
}

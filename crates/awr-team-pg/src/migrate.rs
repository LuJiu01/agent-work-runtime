use crate::error::{PgError, PgResult};
use tokio_postgres::Client;

pub const EXPECTED_SCHEMA_VERSION: i32 = 1;
const MIGRATION: &str = include_str!("../migrations/20260917000001_init.sql");

pub async fn migrate(client: &Client) -> PgResult<()> {
    match check_schema(client).await {
        Ok(()) => return Ok(()),
        Err(PgError::SchemaIncompatible(message)) if message.contains("schema version") => {
            return Err(PgError::SchemaIncompatible(message));
        }
        Err(_) => {}
    }
    client.batch_execute(MIGRATION).await?;
    check_schema(client).await
}

pub async fn check_schema(client: &Client) -> PgResult<()> {
    let row = client
        .query_opt(
            "SELECT version FROM awr_team.schema_state WHERE component='awr_team'",
            &[],
        )
        .await;
    match row {
        Ok(Some(row)) => {
            let version: i32 = row.get(0);
            if version == EXPECTED_SCHEMA_VERSION {
                Ok(())
            } else {
                Err(PgError::SchemaIncompatible(format!(
                    "schema version {version}, expected {EXPECTED_SCHEMA_VERSION}"
                )))
            }
        }
        Ok(None) | Err(_) => Err(PgError::SchemaIncompatible(
            "schema_state missing; refuse to treat an empty or half-migrated database as ready"
                .into(),
        )),
    }
}

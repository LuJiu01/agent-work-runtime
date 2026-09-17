//! PostgreSQL coordination store for Team V1.
//! Personal SQLite runtime does not depend on this crate.
mod bootstrap;
mod error;
mod migrate;
mod path;
mod source;
mod tx;

pub use bootstrap::Bootstrap;
pub use error::{PgError, PgResult};
pub use migrate::{EXPECTED_SCHEMA_VERSION, check_schema, migrate};
pub use path::{
    MAX_FILE_BYTES, MAX_PACKAGE_BYTES, MAX_SOURCE_FILES, validate_package, validate_source_path,
};
pub use source::{CandidateRecord, CurrentSource, IngestRequest, SourceFile, SourceStore};
pub use tx::{CommandOutcome, CommandRequest, TeamStore};

pub const SCHEMA: &str = "awr_team";

pub async fn connect(url: &str) -> PgResult<tokio_postgres::Client> {
    let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls).await?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_contract_is_stable() {
        assert_eq!(SCHEMA, "awr_team");
        assert_eq!(EXPECTED_SCHEMA_VERSION, 1);
    }
}

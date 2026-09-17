//! PostgreSQL coordination store for Team V1.
//! Personal SQLite runtime does not depend on this crate.
mod bootstrap;
mod error;
mod execution;
mod graph;
mod lease;
mod migrate;
mod path;
mod read;
mod review;
mod runner;
mod source;
mod tx;

pub use bootstrap::Bootstrap;
pub use error::{PgError, PgResult};
pub use execution::{ExecutionRecord, ExecutionStore, OutboxDelivery, exactly_once_supported};
pub use graph::{
    DependencyEdge, GraphStore, SplitProposal, paths_conflict, require_main_scope,
    validate_required_graph,
};
pub use lease::{ClaimRecord, LeaseStore, SessionRecord};
pub use migrate::{EXPECTED_SCHEMA_VERSION, check_schema, migrate};
pub use path::{
    MAX_FILE_BYTES, MAX_PACKAGE_BYTES, MAX_SOURCE_FILES, validate_package, validate_source_path,
};
pub use read::{
    EventCursor, EventPage, EventRecord, PreparedWork, ReadStore, WorkGraph, capabilities,
    dispatch_query,
};
pub use review::{CompletionReceipt, EvidenceRecord, ReviewRound, ReviewStore};
pub use runner::{CrashPoint, ReferenceRunner, RunnerOutcome};
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
        assert_eq!(EXPECTED_SCHEMA_VERSION, 5);
    }
}

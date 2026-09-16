use awr_core::*;
use awr_runtime::{DeferCompactionRequest, InspectCompactionRequest};
use awr_store::Store;
use clap::Subcommand;
use std::path::{Path, PathBuf};

#[derive(Debug, Subcommand)]
pub enum CompactionCommand {
    /// Record a completed native compaction from bounded, attributed host JSON.
    Observe {
        #[arg(long)]
        input: PathBuf,
    },
    /// Read the latest assessment; observations/costs are included only on request.
    Inspect {
        #[arg(long)]
        session: Id,
        #[arg(long)]
        include_observation: bool,
    },
    /// Defer this observation's suggestion until the next compaction, without switching sessions.
    Defer {
        #[arg(long)]
        session: Id,
        #[arg(long)]
        observation_event_id: Id,
        #[arg(long)]
        expected_revision: Revision,
    },
}
pub fn run(root: &Path, command: &CompactionCommand, json_output: bool) -> Result<()> {
    let database = crate::source::runtime_dir(root, false)?.join("state.db");
    let value = match command {
        CompactionCommand::Inspect {
            session,
            include_observation,
        } => {
            let store = Store::read_snapshot(&database, 256 * 1024 * 1024)?;
            awr_runtime::inspect_compaction(
                &store,
                root,
                &InspectCompactionRequest {
                    session: *session,
                    include_observation: *include_observation,
                },
            )?
        }
        CompactionCommand::Observe { input } => {
            let input = if input.is_absolute() {
                input.clone()
            } else {
                root.join(input)
            };
            let request = serde_json::from_slice(&awr_source::read_capped(&input, 64 * 1024)?)
                .map_err(|_| Error::InvalidInput("compaction input requires session, expected_revision, observation and optional policy; see context-continuity.md".into()))?;
            let mut store = Store::open_existing(&database)?;
            awr_runtime::observe_compaction(&mut store, root, &request)?
        }
        CompactionCommand::Defer {
            session,
            observation_event_id,
            expected_revision,
        } => {
            let mut store = Store::open_existing(&database)?;
            awr_runtime::defer_compaction(
                &mut store,
                root,
                &DeferCompactionRequest {
                    session: *session,
                    observation_event_id: *observation_event_id,
                    expected_revision: *expected_revision,
                },
            )?
        }
    };
    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        println!(
            "Compaction: {}\nObservation: {}\nWhen: {}\nBasis: {}\nNext: {}\nRecheck: {}",
            value["state"],
            value["observation_event_id"],
            value["guidance"]["when"],
            value["guidance"]["basis"],
            value["guidance"]["next_action"],
            value["guidance"]["recheck"]
        );
        if let Some(observation) = value.get("observation") {
            println!(
                "Observation details: {}",
                serde_json::to_string_pretty(observation)?
            );
        }
    }
    Ok(())
}

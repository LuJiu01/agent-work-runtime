use clap::{Parser, Subcommand};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "awr-server", version, about = "AWR Team server skeleton")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply owner migrations, then refuse to start if schema is incompatible.
    Migrate,
    /// Check schema version without running migrations.
    Check,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    let url = match std::env::var("AWR_TEAM_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => {
            eprintln!(
                "{}",
                serde_json::json!({"code":"SchemaIncompatible","message":"AWR_TEAM_DATABASE_URL is required"})
            );
            return ExitCode::FAILURE;
        }
    };
    let pool = match awr_team_pg::connect(&url).await {
        Ok(pool) => pool,
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({"code":"SchemaIncompatible","message":error.to_string()})
            );
            return ExitCode::FAILURE;
        }
    };
    let result = match args.command {
        Command::Migrate => awr_team_pg::migrate(&pool).await,
        Command::Check => awr_team_pg::check_schema(&pool).await,
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({"code":"SchemaIncompatible","message":error.to_string()})
            );
            ExitCode::FAILURE
        }
    }
}

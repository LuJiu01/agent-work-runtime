use clap::{Parser, Subcommand};
use serde_json::{Value, json};
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
    /// Experimental Team v1 query entry. Unknown ops return Unsupported.
    Query {
        /// capabilities | work.prepare | work.graph | session.inspect | events.list
        #[arg(long)]
        op: String,
        /// JSON object with tenant_id, project_id and query fields.
        #[arg(long)]
        body: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();
    match args.command {
        Command::Query { op, body } => run_query(&op, body.as_deref()).await,
        Command::Migrate => schema_command(true).await,
        Command::Check => schema_command(false).await,
    }
}

fn fail(code: &str, message: impl ToString) -> ExitCode {
    eprintln!("{}", json!({"code": code, "message": message.to_string()}));
    ExitCode::FAILURE
}

async fn schema_command(migrate: bool) -> ExitCode {
    let url = match std::env::var("AWR_TEAM_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return fail("SchemaIncompatible", "AWR_TEAM_DATABASE_URL is required"),
    };
    let client = match awr_team_pg::connect(&url).await {
        Ok(client) => client,
        Err(error) => return fail("SchemaIncompatible", error.to_string()),
    };
    let result = if migrate {
        awr_team_pg::migrate(&client).await
    } else {
        awr_team_pg::check_schema(&client).await
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => fail("SchemaIncompatible", error.to_string()),
    }
}

fn required<'a>(body: &'a Value, key: &str) -> Result<&'a str, ExitCode> {
    body.get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| fail("InvalidInput", format!("{key} required")))
}

async fn run_query(op: &str, body: Option<&str>) -> ExitCode {
    if op == "capabilities" {
        println!("{}", awr_team_pg::capabilities());
        return ExitCode::SUCCESS;
    }
    if let Err(awr_team_pg::PgError::Unsupported(name)) = awr_team_pg::dispatch_query(op) {
        return fail("Unsupported", format!("unsupported query {name}"));
    }
    let url = match std::env::var("AWR_TEAM_DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return fail("SchemaIncompatible", "AWR_TEAM_DATABASE_URL is required"),
    };
    if let Err(error) = awr_team_pg::check_schema(&match awr_team_pg::connect(&url).await {
        Ok(client) => client,
        Err(error) => return fail("SchemaIncompatible", error.to_string()),
    })
    .await
    {
        return fail("SchemaIncompatible", error.to_string());
    }
    let parsed: Value = match body {
        None => return fail("InvalidInput", "query body is required"),
        Some(raw) => match serde_json::from_str(raw) {
            Ok(value) => value,
            Err(error) => return fail("InvalidInput", error.to_string()),
        },
    };
    let tenant = match required(&parsed, "tenant_id") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let project = match required(&parsed, "project_id") {
        Ok(value) => value,
        Err(code) => return code,
    };
    let store = awr_team_pg::ReadStore::new(url);
    let result = match op {
        "work.prepare" => {
            let work_id = match required(&parsed, "work_id") {
                Ok(value) => value,
                Err(code) => return code,
            };
            let max_bytes = parsed.get("max_context_bytes").and_then(Value::as_u64);
            store
                .prepare(tenant, project, work_id, max_bytes.map(|n| n as usize))
                .await
                .map(|value| serde_json::to_value(value).expect("prepare json"))
        }
        "work.graph" => store
            .graph(tenant, project)
            .await
            .map(|value| serde_json::to_value(value).expect("graph json")),
        "session.inspect" => {
            let session_id = match required(&parsed, "session_id") {
                Ok(value) => value,
                Err(code) => return code,
            };
            store.inspect_session(tenant, project, session_id).await
        }
        "events.list" => {
            let after = parsed.get("after").and_then(Value::as_str);
            let limit = parsed.get("limit").and_then(Value::as_i64).unwrap_or(50);
            store
                .list_events(tenant, project, after, limit)
                .await
                .map(|value| serde_json::to_value(value).expect("events json"))
        }
        other => return fail("Unsupported", format!("unsupported query {other}")),
    };
    match result {
        Ok(value) => {
            println!("{value}");
            ExitCode::SUCCESS
        }
        Err(awr_team_pg::PgError::Unsupported(name)) => fail("Unsupported", name),
        Err(awr_team_pg::PgError::SessionNotFound) => fail("SessionNotFound", "session not found"),
        Err(awr_team_pg::PgError::EpochChanged) => {
            fail("EPOCH_CHANGED", "coordinator epoch changed")
        }
        Err(awr_team_pg::PgError::CursorExpired) => fail("CURSOR_EXPIRED", "event cursor expired"),
        Err(error) => fail("QueryFailed", error.to_string()),
    }
}

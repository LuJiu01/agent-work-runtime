use awr_core::*;
use awr_runtime::{PrepareCompletionRequest, PrepareWorkRequest};
use clap::Args;
use serde_json::{Value, json};
use std::path::Path;

#[derive(Debug, Args)]
pub struct PrepareArgs {
    work: String,
    #[arg(long)]
    session: Option<Id>,
    #[arg(long)]
    branch: Option<String>,
    #[arg(long)]
    goal: Vec<String>,
    #[arg(long)]
    source_sha: Option<String>,
    #[arg(long)]
    budget: Option<usize>,
    /// Summary omits duplicate indexes; action adds one bounded conditional instruction.
    #[arg(long, default_value = "full", value_parser = ["full", "summary", "action"])]
    response_view: String,
}
#[derive(Debug, Args)]
pub struct CompletionArgs {
    work: String,
    #[arg(long)]
    report: String,
    #[arg(long)]
    evidence_key: String,
    #[arg(long)]
    source_sha: String,
    #[arg(long, default_value = "locally_verified")]
    level: String,
    #[arg(long)]
    branch: Option<String>,
}
fn output(mut value: Value, query: &crate::query::QueryProject, json_output: bool) -> Result<()> {
    query.check_revision()?;
    query.finish()?;
    for (k, v) in query.metadata().as_object().unwrap() {
        value[k] = v.clone();
    }
    let incomplete = value["context"]["completeness"]["complete"] == false;
    if incomplete {
        value["ok"] = json!(false);
    }
    if json_output {
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        if let Some(g) = value.get("guidance") {
            println!(
                "Stage: {}\nWhen: {}\nBasis: {}\nNext: {}\nRecheck: {}",
                value["stage"], g["when"], g["basis"], g["next_action"], g["recheck"]
            );
        } else {
            println!(
                "Stage: {}\nNext action: {}",
                value["stage"], value["next_action"]
            );
        }
        if let Some(context) = value["context"]["work_context"]["rendered_context"].as_str() {
            println!("{context}");
            if let Some(management) = value.get("management") {
                println!("Management: {}", serde_json::to_string_pretty(management)?);
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
    }
    if incomplete {
        Err(Error::ContextIncomplete(
            "resolve required context gaps before execution".into(),
        ))
    } else {
        Ok(())
    }
}
pub fn prepare(root: &Path, args: &PrepareArgs, json_output: bool) -> Result<()> {
    let mut query = crate::query::QueryProject::open_read(root, false)?;
    query.finish()?;
    let mut value = awr_runtime::prepare_work(
        &mut query.store,
        root,
        &PrepareWorkRequest {
            work: args.work.clone(),
            session: args.session,
            branch: args.branch.clone(),
            goals: args.goal.clone(),
            source_sha: args.source_sha.clone(),
            budget: args.budget,
        },
    )?;
    if args.response_view == "action" {
        value = awr_runtime::guide_prepared_work(&query.store, root, value, args.session)?;
    }
    if args.response_view != "full" {
        // Metadata and completeness are checked by output for both views.
        value["ok"] = json!(value["context"]["completeness"]["complete"] == true);
        if args.response_view == "summary" {
            value = awr_runtime::summarize_work_response(value);
        }
        if value.get("response_view").is_some() {
            value["response_view"]["full_result"] = json!({"command":"work prepare","work":args.work,"session":args.session,"branch":args.branch,"goals":args.goal,"source_sha":args.source_sha,"budget":args.budget,"response_view":"full","basis":"fresh query; compare project_revision and context_hash"});
        }
    }
    output(value, &query, json_output)
}
pub fn completion(root: &Path, args: &CompletionArgs, json_output: bool) -> Result<()> {
    let query = crate::query::QueryProject::open_read(root, false)?;
    query.finish()?;
    let value = awr_runtime::prepare_completion(
        &query.store,
        root,
        &PrepareCompletionRequest {
            work: args.work.clone(),
            report: args.report.clone(),
            evidence_key: args.evidence_key.clone(),
            source_sha: args.source_sha.clone(),
            level: serde_json::from_value(json!(args.level))
                .map_err(|_| Error::InvalidInput("invalid evidence level".into()))?,
            branch: args.branch.clone(),
        },
    )?;
    output(value, &query, json_output)
}

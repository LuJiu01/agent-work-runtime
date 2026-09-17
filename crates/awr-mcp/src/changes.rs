//! Source changes use their existing durable domain journal, not the MCP lifecycle
//! journal: adding lifecycle revisions would invalidate the reviewed source preview.
use crate::{
    operations::parse,
    project::{ReadProject, database},
};
use awr_core::*;
use awr_runtime::*;
use awr_store::Store;
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::path::Path;

pub(crate) const NAMES: [&str; 4] = [
    "awr_change_preview",
    "awr_change_apply",
    "awr_change_status",
    "awr_change_recover",
];

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Change {
    WorkEdit {
        work: String,
        fields: Value,
        source_fingerprint: Option<String>,
    },
    Create {
        title: String,
        #[serde(default)]
        source_id: Option<Id>,
        #[serde(default)]
        fields: Map<String, Value>,
    },
    Batch {
        change: BatchChange,
    },
    Edit {
        change: HostChange,
    },
}
impl Change {
    fn kind(&self) -> &'static str {
        match self {
            Self::WorkEdit { .. } => "work_edit",
            Self::Create { .. } => "create",
            Self::Batch { .. } => "batch",
            Self::Edit { .. } => "edit",
        }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preview {
    request_id: String,
    reason: String,
    change: Change,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Apply {
    request_id: String,
    reason: String,
    change: Change,
    expected_revision: Revision,
    expected_preview: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Status {
    request_id: String,
    kind: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recover {
    request_id: String,
    kind: String,
    expected_revision: Revision,
}

fn key(client: &str, kind: &str, request: &str) -> Result<String> {
    if !["create", "batch", "edit", "work_edit"].contains(&kind)
        || request.trim().is_empty()
        || request.len() > 256
        || request.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput(
            "source changes require kind create/batch/edit/work_edit and a stable request_id of 1..256 bytes"
                .into(),
        ));
    }
    Ok(format!(
        "mcp-source-{}",
        awr_source::fingerprint(&serde_json::to_vec(&(client, kind, request))?)
            .trim_start_matches("sha256:")
    ))
}
fn actor(client: &str) -> HostActor {
    HostActor {
        host: "awr-mcp".into(),
        subject: client.into(),
        origin: HostEditOrigin::DelegatedAgent,
    }
}
fn edit_allowed(change: &HostChange) -> Result<()> {
    if matches!(
        change,
        HostChange::Fields {
            kind: EntityKind::WorkItem,
            ..
        } | HostChange::ActivateDraft { .. }
    ) {
        Ok(())
    } else {
        Err(Error::MutationUnsupported("MCP edits expose work fields and draft activation; human confirmation is not an Agent action".into()))
    }
}
fn apply_change(
    store: &mut Store,
    root: &Path,
    request: Preview,
    client: &str,
    accept: Option<(Revision, &str)>,
) -> Result<(Value, Option<Error>)> {
    let kind = request.change.kind();
    let request_key = key(client, kind, &request.request_id)?;
    if request.reason.trim().is_empty() || request.reason.len() > 4096 {
        return Err(Error::InvalidInput(
            "change reason requires 1..4096 bytes".into(),
        ));
    }
    let (mut value, failure) = match request.change {
        Change::WorkEdit {
            work,
            fields,
            source_fingerprint,
        } => {
            let mut report = edit_work(
                store,
                root,
                HostSaveRequest {
                    version: 1,
                    request_key,
                    actor: actor(client),
                    reason: request.reason,
                    change: HostChange::Fields {
                        kind: EntityKind::WorkItem,
                        target: work.clone(),
                        fields: fields.clone(),
                        source_fingerprint: source_fingerprint.unwrap_or_default(),
                    },
                },
                accept,
            )?;
            if report.value["status"] == "preview" {
                let fingerprint = report.value["change"]["source_fingerprint"].clone();
                report.value["change"] = json!({"kind":"work_edit","work":work,"fields":fields,"source_fingerprint":fingerprint});
                report.value["guidance"]["next_action"] = json!(
                    "Call awr_change_apply with this change, the same request_id/reason, project_revision as expected_revision and preview_fingerprint as expected_preview"
                );
            }
            report.value["guidance"]["recheck"] = json!(
                "Changed source or uncertain outcome: query awr_change_status with kind work_edit before retrying"
            );
            (report.value, report.failure)
        }
        Change::Create {
            title,
            source_id,
            fields,
        } => {
            let report = create_work(
                store,
                root,
                CreateWorkInput {
                    version: 1,
                    request_key,
                    title,
                    source_id,
                    fields,
                },
                accept.is_some(),
                accept.map(|a| a.1),
                accept.map(|a| a.0),
            )?;
            (report.value, report.failure)
        }
        Change::Batch { change } => {
            let report = change_batch(
                store,
                root,
                BatchRequest {
                    version: 1,
                    request_key,
                    actor: actor(client),
                    reason: request.reason,
                    change,
                },
                accept.is_some(),
                accept.map(|a| a.1),
                accept.map(|a| a.0),
            )?;
            (report.value, report.failure)
        }
        Change::Edit { change } => {
            edit_allowed(&change)?;
            let input = HostSaveRequest {
                version: 1,
                request_key,
                actor: actor(client),
                reason: request.reason,
                change,
            };
            let report = match accept {
                Some((revision, preview)) => {
                    host_save(store, root, input, revision, Some(preview))?
                }
                None => host_preview(store, root, input)?,
            };
            (report.value, report.failure)
        }
    };
    value["request_id"] = json!(request.request_id);
    value["change_kind"] = json!(kind);
    Ok((value, failure))
}

fn outcome(mut value: Value, failure: Option<Error>) -> Result<CallToolResult> {
    value["journal"] = json!("source_change");
    value["outcome_query"] = json!("awr_change_status");
    value["recovery_tool"] = json!("awr_change_recover");
    if let Some(error) = &failure {
        value["ok"] = json!(false);
        value["error"] = json!(error.report());
    }
    ensure_public_value(&value)?;
    Ok(if failure.is_some() {
        CallToolResult::structured_error(value)
    } else {
        CallToolResult::structured(value)
    })
}

pub(crate) fn call(root: &Path, name: &str, args: Value, client: &str) -> Result<CallToolResult> {
    match name {
        "awr_change_preview" => {
            let input: Preview = parse(args)?;
            // A private in-memory snapshot makes previews truly read-only, including indexing.
            let mut view = ReadProject::open(root)?;
            let (mut value, failure) = apply_change(&mut view.store, root, input, client, None)?;
            view.finish(root)?;
            value["read_only"] = json!(true);
            value["runtime_write_performed"] = json!(false);
            value["source_write_performed"] = json!(false);
            outcome(value, failure)
        }
        "awr_change_apply" => {
            let input: Apply = parse(args)?;
            let mut store = Store::open_existing(&database(root)?)?;
            let (value, failure) = apply_change(
                &mut store,
                root,
                Preview {
                    request_id: input.request_id,
                    reason: input.reason,
                    change: input.change,
                },
                client,
                Some((input.expected_revision, &input.expected_preview)),
            )?;
            outcome(value, failure)
        }
        "awr_change_status" => {
            let input: Status = parse(args)?;
            let request_key = key(client, &input.kind, &input.request_id)?;
            // Recovery lookup remains available even when a partial write made sources stale.
            let store = Store::open_readonly(&database(root)?)?;
            let (mut value, failure) = match input.kind.as_str() {
                "create" => {
                    let r = creation_status(&store, root, &request_key)?;
                    (r.value, r.failure)
                }
                "batch" => {
                    let r = batch_status(&store, root, &request_key)?;
                    (r.value, r.failure)
                }
                _ => {
                    let r = host_status(&store, root, &request_key)?;
                    (r.value, r.failure)
                }
            };
            value["request_id"] = json!(input.request_id);
            value["change_kind"] = json!(input.kind);
            outcome(value, failure)
        }
        "awr_change_recover" => {
            let input: Recover = parse(args)?;
            let request_key = key(client, &input.kind, &input.request_id)?;
            let mut store = Store::open_existing(&database(root)?)?;
            let (mut value, failure) = match input.kind.as_str() {
                "create" => {
                    let r =
                        recover_creation(&mut store, root, &request_key, input.expected_revision)?;
                    (r.value, r.failure)
                }
                "batch" => {
                    let r = recover_batch(&mut store, root, &request_key, input.expected_revision)?;
                    (r.value, r.failure)
                }
                _ => {
                    let r = host_recover(&mut store, root, &request_key, input.expected_revision)?;
                    (r.value, r.failure)
                }
            };
            value["request_id"] = json!(input.request_id);
            value["change_kind"] = json!(input.kind);
            outcome(value, failure)
        }
        _ => Err(Error::Unsupported(name.into())),
    }
}

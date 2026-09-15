//! Optional presentation only. Domain checks and stored receipts always use full results.
use serde_json::{Value, json};

pub fn summarize_work_response(mut value: Value) -> Value {
    // Preserve failures (including partial writes and unknown outcomes) verbatim.
    if value["ok"] != true {
        return value;
    }
    let mut omitted = vec![];
    if value["stage"] == "prepared" {
        if let Some(pack) = value["context"]["work_context"].as_object_mut() {
            // These are indexes of the rendered packet, not its content or provenance.
            for field in ["selected_chunks", "selected_entities"] {
                if pack.remove(field).is_some() {
                    omitted.push(format!("context.work_context.{field}"));
                }
            }
        }
    } else if value["proposal"]["status"] == "applied" && value["write_outcome"] == "applied" {
        if let Some(patch) = value["proposal"]
            .as_object_mut()
            .and_then(|p| p.remove("patch"))
        {
            value["transition"] = patch["work_action"].clone();
            value["changes"] = patch["changes"].clone();
            value["target"] = patch["target"].clone();
            value["intent"] = patch["intent"].clone();
            omitted.push("proposal.patch".into());
            if let Some(payload) = value["event"]
                .as_object_mut()
                .and_then(|e| e.remove("payload"))
            {
                value["event"]["outcome"] = json!({"released_claim_ids":payload["released_claim_ids"],
                    "source_revision":payload["source_revision"],"target_revision":payload["target_revision"],
                    "after_fingerprint":payload["after_fingerprint"]});
                omitted.push("event.payload".into());
            }
        }
    }
    if !omitted.is_empty() {
        value["response_view"] = json!({"version":1,"view":"summary","omitted_fields":omitted});
    }
    value
}

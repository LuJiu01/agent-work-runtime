use serde_json::{Value, json};

/// Check the public projection contract, including every field outside the index.
pub fn assert_projection(legacy: &Value, compact: &Value, count: usize) {
    assert!(compact.get("response_budget").is_none());
    if count == 0 {
        assert_eq!(compact, legacy, "zero omissions must add no bookkeeping");
        return;
    }
    let path = "context.work_context.omitted_chunks";
    assert!(
        compact["context"]["work_context"]
            .get("omitted_chunks")
            .is_none()
    );
    assert_eq!(
        compact["response_view"]["detail_counts"],
        json!({path:count})
    );
    assert!(
        compact["response_view"]["omitted_fields"]
            .as_array()
            .unwrap()
            .contains(&json!(path))
    );
    assert!(compact["response_view"]["full_result"].is_object());
    let note = compact["response_view"]["detail_note"].as_str().unwrap();
    assert!(note.contains("whole-chunk budget selection"));
    assert!(note.contains("required context is complete"));
    assert!(note.contains("full_result only when investigating"));
    assert!(note.len() < 200);
    let mut restored = compact.clone();
    restored["context"]["work_context"]["omitted_chunks"] =
        legacy["context"]["work_context"]["omitted_chunks"].clone();
    restored["response_view"]["omitted_fields"] = legacy["response_view"]["omitted_fields"].clone();
    restored["response_view"]
        .as_object_mut()
        .unwrap()
        .remove("detail_counts");
    restored["response_view"]
        .as_object_mut()
        .unwrap()
        .remove("detail_note");
    assert_eq!(
        &restored, legacy,
        "only the optional omission index may change"
    );
    let tokens = |v: &Value| {
        awr_context::token_count(&format!("{}\n", serde_json::to_string_pretty(v).unwrap()))
    };
    assert!(
        tokens(compact) < tokens(legacy),
        "the concise projection must save serialized tokens"
    );
}

/// Compare real prepare views to full, allowing only the documented view omissions.
#[allow(dead_code)] // The runtime unit fixture exercises only assert_projection.
pub fn assert_prepared_view(full: &Value, concise: &Value, count: usize) {
    let path = "context.work_context.omitted_chunks";
    let mut original_view = concise.clone();
    if count > 0 {
        original_view["context"]["work_context"]["omitted_chunks"] =
            full["context"]["work_context"]["omitted_chunks"].clone();
        original_view["response_view"]["omitted_fields"]
            .as_array_mut()
            .unwrap()
            .retain(|p| p != path);
        for key in ["detail_counts", "detail_note"] {
            original_view["response_view"]
                .as_object_mut()
                .unwrap()
                .remove(key);
        }
    } else {
        assert!(concise["response_view"].get("detail_counts").is_none());
        assert!(concise["response_view"].get("detail_note").is_none());
    }
    assert_projection(&original_view, concise, count);

    let mut restored = original_view.clone();
    for field in original_view["response_view"]["omitted_fields"]
        .as_array()
        .unwrap()
    {
        let field = field.as_str().unwrap();
        assert!(matches!(
            field,
            "context.work_context.selected_chunks"
                | "context.work_context.selected_entities"
                | "context.completeness.source_versions"
                | "management.scope"
                | "management.next_action"
                | "management.work"
                | "management.work_id"
                | "management.work_revision"
                | "management.source_ref"
                | "management.branch_id"
                | "management.decision.optional_maintenance"
                | "next_action"
        ));
        let mut parts: Vec<_> = field.split('.').collect();
        let key = parts.pop().unwrap();
        let parent = format!("/{}", parts.join("/"));
        let value = full
            .pointer(&format!("/{}", field.replace('.', "/")))
            .unwrap();
        if field == "context.completeness.source_versions" {
            assert_eq!(
                value,
                &full["context"]["work_context"]["identity"]["source_versions"]
            );
        }
        if parts.is_empty() {
            restored
                .as_object_mut()
                .unwrap()
                .insert(key.into(), value.clone());
        } else {
            restored
                .pointer_mut(&parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(key.into(), value.clone());
        }
    }
    let mut expected = full.clone();
    for value in [&mut restored, &mut expected] {
        let object = value.as_object_mut().unwrap();
        object.remove("response_view");
        object.remove("guidance");
        // Explanation describes the chosen view; its independent tests check its semantics.
        object.remove(awr_runtime::ASSESSMENT_EXPLAIN_FIELD);
    }
    assert_eq!(
        restored, expected,
        "prepare views preserve all other fields"
    );
}

//! Field advice tied to the actual parser rejection, never submitted values.
use awr_core::{Error, Result};
use rmcp::model::CallToolResult;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use serde_path_to_error::Segment;

pub(crate) const SCHEMA_MISMATCH: &str = "tool arguments do not match the tool schema";

/// Keep field attribution alongside a rejection without changing the core error.
#[derive(Debug)]
pub(crate) enum Rejection {
    Input { message: String, detail: Value },
    Other(Box<Error>),
}
impl From<Error> for Rejection {
    fn from(error: Error) -> Self {
        Self::Other(Box::new(error))
    }
}
impl Rejection {
    pub(crate) fn input(
        message: impl Into<String>,
        path: &'static str,
        constraint: &'static str,
    ) -> Self {
        Self::Input {
            message: message.into(),
            detail: issue(path, constraint),
        }
    }
    pub(crate) fn into_result(self) -> Result<CallToolResult> {
        match self {
            Self::Input { message, detail } => {
                let mut value = serde_json::to_value(Error::InvalidInput(message).report())?;
                attach(&mut value, detail);
                Ok(CallToolResult::structured_error(value))
            }
            Self::Other(error) => Err(*error),
        }
    }
}

pub(crate) fn invalid_input(
    message: impl Into<String>,
    path: &'static str,
    constraint: &'static str,
) -> Result<CallToolResult> {
    Rejection::input(message, path, constraint).into_result()
}

// Tool adapters return a structured rejection at the existing parse site. This
// keeps source/admission ordering and records advice before request bookkeeping.
macro_rules! parse_tool {
    ($name:expr, $args:expr) => {
        match $crate::input_diagnostics::parse($name, $args) {
            Ok(value) => value,
            Err(rejection) => return rejection.into_result(),
        }
    };
}
pub(crate) use parse_tool;

pub(crate) fn parse<T: DeserializeOwned>(
    name: &str,
    args: Value,
) -> std::result::Result<T, Rejection> {
    awr_core::ensure_public_value(&args)?;
    serde_path_to_error::deserialize(args).map_err(|error| {
        let detail = crate::schema::tools()
            .into_iter()
            .find(|tool| tool.name == name)
            .and_then(|tool| parser_detail(&Value::Object((*tool.input_schema).clone()), &error));
        match detail {
            Some(detail) => Rejection::Input {
                message: SCHEMA_MISMATCH.into(),
                detail,
            },
            None => Error::InvalidInput(SCHEMA_MISMATCH.into()).into(),
        }
    })
}

fn attach(error: &mut Value, detail: Value) {
    error
        .as_object_mut()
        .unwrap()
        .extend(detail.as_object().unwrap().clone());
    if error.get("next_step").is_none() && error.get("next_action").is_none() {
        error["next_step"] = json!(
            "Correct this field using the tool's discovered input schema. After an uncertain write outcome, inspect the original request receipt before retrying."
        );
    }
}
fn issue(path: &str, constraint: &str) -> Value {
    json!({"invalid_field":if path.is_empty(){"/"}else{path},"constraint":constraint})
}
fn unexpected(schema: &Value, path: &str) -> Option<Value> {
    if schema["additionalProperties"] != false {
        return None;
    }
    let mut detail = issue(path, "unexpected_field");
    let names = schema["properties"].as_object()?.keys().collect::<Vec<_>>();
    if serde_json::to_vec(&names).ok()?.len() <= 512 {
        detail["expected_fields"] = json!(names);
    }
    Some(detail)
}
fn parser_detail(
    schema: &Value,
    error: &serde_path_to_error::Error<serde_json::Error>,
) -> Option<Value> {
    let mut schema = schema;
    let mut path = String::new();
    let message = error.inner().to_string();
    // Only follow the real failure path through public named fields. Never
    // display the raw Serde message/path: either may contain private data.
    for (depth, segment) in error.path().iter().enumerate() {
        if depth >= 6 {
            return None;
        }
        match segment {
            Segment::Map { key } => {
                if matches!(key.as_str(), "payload" | "fields") {
                    return None;
                }
                let fields = schema["properties"].as_object()?;
                let Some(field) = fields.get(key) else {
                    return if message.starts_with("unknown field ") {
                        unexpected(schema, &path)
                    } else {
                        None
                    };
                };
                schema = field;
                path.push('/');
                path.push_str(key);
            }
            Segment::Seq { index } if *index < 32 => {
                schema = schema.get("items")?;
                path.push('/');
                path.push_str(&index.to_string());
            }
            _ => return None,
        }
    }
    if message.starts_with("unknown field ") {
        return unexpected(schema, &path);
    }
    if let Some(required) = schema["required"].as_array() {
        for field in required.iter().filter_map(Value::as_str) {
            if message == format!("missing field `{field}`") {
                return Some(issue(&format!("{path}/{field}"), "required"));
            }
        }
    }
    // Lifecycle selectors are resolved before parsing. A missing session here
    // can be explained with the two advertised public selectors.
    if message == "missing field `session`" && schema.get("anyOf").is_some() {
        let mut detail = issue(&path, "required_selector");
        detail["expected_fields"] = json!(["conversation", "session"]);
        return Some(detail);
    }
    if path.is_empty() {
        return None;
    }
    Some(issue(
        &path,
        if message.starts_with("invalid type: ") {
            "type"
        } else if message.starts_with("unknown variant ") {
            "enum"
        } else {
            "parse"
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Input {
        #[allow(dead_code)]
        optional: Option<String>,
        count: usize,
    }
    fn detail<T: DeserializeOwned>(input: Value, schema: Value) -> Option<Value> {
        let error = serde_path_to_error::deserialize::<_, T>(input)
            .err()
            .unwrap();
        parser_detail(&schema, &error)
    }
    #[test]
    fn only_the_actual_parser_failure_is_diagnosed() {
        let schema = json!({"type":"object","properties":{"optional":{"type":"string","enum":["public"]},"count":{"type":"integer"}},"required":["count"],"additionalProperties":false});
        for optional in [Value::Null, json!("private-fixture-value")] {
            assert_eq!(
                detail::<Input>(json!({"optional":optional,"count":-1}), schema.clone()).unwrap(),
                issue("/count", "parse")
            );
        }
        assert_eq!(
            detail::<Input>(json!({"optional":null,"count":false}), schema.clone()).unwrap(),
            issue("/count", "type")
        );
        assert_eq!(
            detail::<Input>(json!({"optional":null}), schema.clone()).unwrap(),
            issue("/count", "required")
        );
        assert!(detail::<Input>(json!({"count":false}), json!({"type":"object","properties":{"optional":{"type":"string"}},"additionalProperties":false})).is_none());
        assert_eq!(
            parse::<Input>("unused", json!({"optional":null,"count":1}))
                .unwrap()
                .count,
            1
        );
        let result = detail::<Input>(
            json!({"count":1,"private-fixture-key":"private-fixture-value"}),
            schema,
        )
        .unwrap();
        assert_eq!(result["invalid_field"], "/");
        assert_eq!(result["constraint"], "unexpected_field");
        assert!(!result.to_string().contains("private-fixture"));
    }
    #[test]
    fn advice_preserves_core_errors_and_existing_guidance() {
        assert!(matches!(
            Rejection::from(Error::RuleViolation("denied".into())).into_result(),
            Err(Error::RuleViolation(_))
        ));
        let mut value = json!({"next_action":"Inspect the recorded outcome"});
        attach(&mut value, issue("/count", "type"));
        assert_eq!(value["next_action"], "Inspect the recorded outcome");
        assert!(value.get("next_step").is_none());
        let result = invalid_input("rejected", "/count", "type").unwrap();
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.structured_content.unwrap()["message"],
            "invalid input: rejected"
        );
        assert!(
            matches!(parse::<Input>("unused", json!({"count":false})).unwrap_err().into_result(), Err(Error::InvalidInput(message)) if message==SCHEMA_MISMATCH)
        );
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct Ending {
        session: awr_core::Id,
        outcome: awr_core::SessionOutcome,
        expected_revision: awr_core::Revision,
    }
    #[test]
    fn enums_and_selectors_are_diagnosed_from_real_rejections() {
        let rejected = parse::<Ending>("awr_session_end", json!({"session":awr_core::Id::new(),"expected_revision":1,"outcome":"private-fixture-value"})).unwrap_err().into_result().unwrap().structured_content.unwrap();
        assert_eq!(rejected["invalid_field"], "/outcome");
        assert_eq!(rejected["constraint"], "enum");
        assert!(!rejected.to_string().contains("private-fixture"));
        let missing = parse::<Ending>(
            "awr_session_end",
            json!({"expected_revision":1,"outcome":"ended"}),
        )
        .unwrap_err()
        .into_result()
        .unwrap()
        .structured_content
        .unwrap();
        assert_eq!(missing["constraint"], "required_selector");
        assert_eq!(
            missing["expected_fields"],
            json!(["conversation", "session"])
        );
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Basket {
        optional: Option<String>,
        items: Vec<Input>,
    }
    #[test]
    fn bounded_paths_never_fall_back_to_unrelated_enums() {
        let item =
            json!({"type":"object","properties":{"count":{"type":"integer"}},"required":["count"]});
        let schema = json!({"type":"object","properties":{"optional":{"type":"string","enum":["public"]},"items":{"type":"array","items":item}}});
        let mut items = vec![json!({"count":1}); 33];
        items[32]["count"] = json!(false);
        assert!(
            detail::<Basket>(
                json!({"optional":"private-fixture-value","items":items}),
                schema.clone()
            )
            .is_none()
        );
        items[31]["count"] = json!(false);
        assert_eq!(
            detail::<Basket>(
                json!({"optional":"private-fixture-value","items":items}),
                schema
            )
            .unwrap(),
            issue("/items/31/count", "type")
        );
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct Node {
        nested: Option<Box<Node>>,
        count: usize,
    }
    #[test]
    fn deep_parser_paths_are_omitted() {
        let mut value = json!({"count":false});
        let mut schema = json!({"type":"object","properties":{"count":{"type":"integer"}}});
        for _ in 0..6 {
            value = json!({"count":1,"nested":value});
            schema =
                json!({"type":"object","properties":{"count":{"type":"integer"},"nested":schema}});
        }
        assert!(detail::<Node>(value, schema).is_none());
    }

    #[derive(Deserialize)]
    #[allow(dead_code)]
    struct PrivateMap {
        #[serde(alias = "payload")]
        fields: std::collections::BTreeMap<String, Input>,
    }
    #[test]
    fn generic_maps_and_large_public_field_lists_remain_private() {
        for field in ["fields", "payload"] {
            let value = json!({field:{"private-fixture-key":{"count":false}}});
            let schema = json!({"type":"object","properties":{field:{"type":"object"}}});
            assert!(detail::<PrivateMap>(value, schema).is_none());
        }
        let fields = (0..32)
            .map(|i| {
                (
                    format!("public-field-with-a-long-name-{i}"),
                    json!({"type":"string"}),
                )
            })
            .collect::<serde_json::Map<_, _>>();
        let schema = json!({"type":"object","properties":fields,"additionalProperties":false});
        let result = detail::<Input>(
            json!({"count":1,"private-fixture-key":"private-fixture-value"}),
            schema,
        )
        .unwrap();
        assert!(result.get("expected_fields").is_none());
        assert!(!result.to_string().contains("private-fixture"));
    }

    #[test]
    fn nested_completion_errors_use_public_paths() {
        let error = parse::<crate::arguments::TransitionArgs>("awr_work_transition", json!({"work":"W","action":"complete","session":awr_core::Id::new(),"expected_revision":1,"reason":"Review","completion":{"version":1,"source_sha":"a".repeat(40),"acceptance":[{"criterion":"Reviewed","evidence":false}]}})).err().unwrap().into_result().unwrap().structured_content.unwrap();
        assert_eq!(error["invalid_field"], "/completion/acceptance/0/evidence");
        assert_eq!(error["constraint"], "type");
    }
}

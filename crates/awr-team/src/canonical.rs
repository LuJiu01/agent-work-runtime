use crate::error::{TeamError, TeamResult};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

pub const HASH_CODEC: &str = "awr-team-hash-v1";

/// Deterministic JSON: object keys sorted, no NaN/Infinity.
pub fn canonical_json(value: &Value) -> TeamResult<Vec<u8>> {
    let normalized = normalize(value)?;
    serde_json::to_vec(&normalized).map_err(|e| TeamError::InvalidContract(e.to_string()))
}

pub fn contract_hash(fields: &Value) -> TeamResult<String> {
    hash_named("contract", fields)
}

pub fn request_hash(fields: &Value) -> TeamResult<String> {
    hash_named("request", fields)
}

fn hash_named(kind: &str, fields: &Value) -> TeamResult<String> {
    let mut wrapper = Map::new();
    wrapper.insert("codec".into(), Value::String(HASH_CODEC.into()));
    wrapper.insert("kind".into(), Value::String(kind.into()));
    wrapper.insert("fields".into(), fields.clone());
    let bytes = canonical_json(&Value::Object(wrapper))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn normalize(value: &Value) -> TeamResult<Value> {
    match value {
        Value::Null | Value::Bool(_) | Value::String(_) => Ok(value.clone()),
        Value::Number(n) => {
            if n.as_f64().is_some_and(|f| !f.is_finite()) {
                return Err(TeamError::NonCanonicalNumber(n.to_string()));
            }
            Ok(Value::Number(n.clone()))
        }
        Value::Array(items) => Ok(Value::Array(
            items
                .iter()
                .map(normalize)
                .collect::<TeamResult<Vec<_>>>()?,
        )),
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), normalize(&map[&key])?);
            }
            Ok(Value::Object(out))
        }
    }
}

/// Reject unknown keys when a caller supplies an explicit allowed-field set.
pub fn reject_unknown_required_fields(
    object: &Map<String, Value>,
    allowed: &[&str],
) -> TeamResult<()> {
    for key in object.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(TeamError::UnknownRequiredField(key.clone()));
        }
    }
    for field in allowed {
        if !object.contains_key(*field) {
            return Err(TeamError::MissingRequiredField((*field).into()));
        }
    }
    Ok(())
}

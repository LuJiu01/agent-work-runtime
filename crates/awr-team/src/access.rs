use crate::error::{TeamError, TeamResult};
use crate::{PROTOCOL, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const SURFACES: [&str; 3] = ["http", "mcp", "cli"];

const WRITE_OPS: &[&str] = &[
    "work.claim",
    "claim.renew",
    "session.start",
    "execution.prepare",
    "work.complete",
    "review.decide",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthContext {
    pub tenant_id: String,
    pub project_id: String,
    pub actor_id: String,
    pub client_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteProfile {
    pub name: String,
    pub endpoint: String,
    pub project_key: String,
    pub credential_env: String,
    pub protocol_version: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Envelope {
    pub protocol_version: u32,
    pub request_id: String,
    pub op: String,
    pub args: Value,
}

impl RemoteProfile {
    pub fn validate(&self) -> TeamResult<()> {
        if self.protocol_version != PROTOCOL_VERSION {
            return Err(TeamError::ProtocolUnsupported);
        }
        if self.project_key.is_empty() {
            return Err(TeamError::ProjectRequired);
        }
        if !valid_env_ref(&self.credential_env) {
            return Err(TeamError::SecretRefInvalid);
        }
        if self.endpoint.contains("postgres://") || self.endpoint.contains("password=") {
            return Err(TeamError::SecretRefInvalid);
        }
        Ok(())
    }

    pub fn redacted(&self) -> Value {
        json!({
            "name": self.name,
            "endpoint": self.endpoint,
            "project_key": self.project_key,
            "credential_env": self.credential_env,
            "protocol_version": self.protocol_version,
        })
    }
}

pub fn parse_envelope(raw: &Value) -> TeamResult<Envelope> {
    let protocol_version = match raw.get("protocol_version") {
        None => return Err(TeamError::ProtocolUnsupported),
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0) as u32,
        Some(Value::String(s)) => {
            crate::decode_u64(s).map_err(|_| TeamError::ProtocolUnsupported)? as u32
        }
        _ => return Err(TeamError::ProtocolUnsupported),
    };
    if protocol_version != PROTOCOL_VERSION {
        return Err(TeamError::ProtocolUnsupported);
    }
    let request_id = raw
        .get("request_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(TeamError::MissingRequiredField("request_id".into()))?
        .to_owned();
    let op = raw
        .get("op")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(TeamError::MissingRequiredField("op".into()))?
        .to_owned();
    let args = raw.get("args").cloned().unwrap_or_else(|| json!({}));
    if let Some(obj) = args.as_object() {
        let allowed = known_args(&op);
        if !allowed.is_empty() {
            for key in obj.keys() {
                if !allowed.contains(&key.as_str()) {
                    return Err(TeamError::UnknownRequiredField(key.clone()));
                }
            }
        }
    }
    Ok(Envelope {
        protocol_version,
        request_id,
        op,
        args,
    })
}

pub fn authorize(auth: &AuthContext, body: &Value) -> TeamResult<()> {
    if auth.project_id.is_empty() {
        return Err(TeamError::ProjectRequired);
    }
    if let Some(project) = body.get("project_id").and_then(Value::as_str) {
        if project != auth.project_id {
            return Err(TeamError::AuthProjectMismatch);
        }
    }
    if let Some(tenant) = body.get("tenant_id").and_then(Value::as_str) {
        if tenant != auth.tenant_id {
            return Err(TeamError::AuthProjectMismatch);
        }
    }
    if let Some(args) = body.get("args") {
        if let Some(project) = args.get("project_id").and_then(Value::as_str) {
            if project != auth.project_id {
                return Err(TeamError::AuthProjectMismatch);
            }
        }
    }
    Ok(())
}

pub fn execute(
    surface: &str,
    envelope: Envelope,
    auth: &AuthContext,
    remote: Option<&RemoteProfile>,
    online: bool,
) -> TeamResult<Value> {
    if !SURFACES.contains(&surface) {
        return Err(TeamError::ProtocolUnsupported);
    }
    if envelope.protocol_version != PROTOCOL_VERSION {
        return Err(TeamError::ProtocolUnsupported);
    }
    let write = WRITE_OPS.contains(&envelope.op.as_str());
    match remote {
        None => {
            if write || envelope.op != "capabilities" {
                return Err(TeamError::OfflineWriteForbidden);
            }
        }
        Some(profile) => profile.validate()?,
    }
    if write && !online {
        return Err(TeamError::OfflineWriteForbidden);
    }
    if envelope.op == "capabilities" {
        return Ok(json!({
            "protocol": PROTOCOL,
            "protocol_version": PROTOCOL_VERSION,
            "authority_model": "approved-source-snapshot",
            "runtime_state_authority": "server",
            "offline_mutations": false,
            "arbitrary_external_exactly_once": false,
            "surface": surface,
            "project_id": auth.project_id,
        }));
    }
    if !online {
        return Ok(json!({
            "cached": true,
            "expired": true,
            "writable": false,
            "op": envelope.op,
            "surface": surface,
        }));
    }
    Ok(json!({
        "accepted": true,
        "op": envelope.op,
        "request_id": envelope.request_id,
        "project_id": auth.project_id,
        "surface": surface,
        "replayed": false,
    }))
}

pub fn same_error_on_all_surfaces(
    envelope: Envelope,
    auth: &AuthContext,
    remote: Option<&RemoteProfile>,
    online: bool,
) -> TeamResult<()> {
    let first = execute("http", envelope.clone(), auth, remote, online);
    for surface in SURFACES {
        let next = execute(surface, envelope.clone(), auth, remote, online);
        match (&first, &next) {
            (Ok(a), Ok(b)) => {
                let mut a = a.clone();
                let mut b = b.clone();
                a.as_object_mut().map(|m| m.remove("surface"));
                b.as_object_mut().map(|m| m.remove("surface"));
                if a != b {
                    return Err(TeamError::InvalidContract("surfaces diverged".into()));
                }
            }
            (Err(a), Err(b)) if a == b => {}
            _ => {
                return Err(TeamError::InvalidContract("surfaces diverged".into()));
            }
        }
    }
    first.map(|_| ())
}

fn known_args(op: &str) -> Vec<&'static str> {
    match op {
        "work.claim" => vec![
            "scope_id",
            "work_id",
            "session_id",
            "expected_work_version",
            "expected_contract_hash",
        ],
        _ => vec![],
    }
}

fn valid_env_ref(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some('A'..='Z') | Some('_') => {}
        _ => return false,
    }
    chars.all(|c| matches!(c, 'A'..='Z' | '0'..='9' | '_')) && !name.contains("SECRET_VALUE")
}

//! Read-only introspection; execution is owned by Agent, which holds identity
//! and journal/evidence state. ToolContext never discovers other sessions.
use serde_json::{json, Value};

pub fn schema() -> Value {
    json!({"type":"function","function":{
        "name":"session_info",
        "description":"Read this agent's current Sui session ID, run and journal paths, export command, and bounded recorded tool/request activity. Reports runtime observations, not delivery proof or permission grants. Does not read other sessions, credentials, or private reasoning.",
        "parameters":{"type":"object","properties":{},"additionalProperties":false}
    }})
}

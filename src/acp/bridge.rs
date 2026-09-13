//! Session-scoped MCP artifact-submission bridge (`sui acp-bridge`).
//!
//! External agents can't call Sui's intercepted `submit_result` tool, so
//! each ACP session advertises this stdio MCP server instead. It accepts
//! `submit_result(payload)` tool calls, shape-validates the payload so the
//! agent gets immediate feedback, and drops each submission as
//! `<dir>/<NNN>.json`. The mission driver re-validates authoritatively
//! when the turn ends — the file drop is a transport, not proof.
//!
//! Wire format: newline-delimited JSON-RPC (MCP stdio transport).

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// How many submissions a session may drop. A control role needs at most a
/// couple of resubmissions; beyond this the agent is looping on artifacts.
const MAX_ARTIFACTS: usize = 8;

/// The one tool the bridge serves.
pub fn tool_schema() -> Value {
    json!({
        "name": "submit_result",
        "description": "Submit your final structured deliverable as JSON in `payload`. The harness validates it; fix and resubmit on error.",
        "inputSchema": {
            "type": "object",
            "properties": { "payload": { "type": "object" } },
            "required": ["payload"],
            "additionalProperties": false
        }
    })
}

/// Shape-level validation shared by the bridge (early feedback) and the
/// mission driver (authoritative, re-run after the turn ends). `expect`:
/// plan | verdict | decision | any.
pub fn validate_payload(expect: &str, payload: &Value) -> Result<()> {
    if !payload.is_object() {
        bail!("payload must be an object");
    }
    match expect {
        "plan" => {
            let plan: crate::mission::plan::MissionPlan =
                serde_json::from_value(payload.clone()).context("payload is not a mission plan")?;
            crate::mission::plan::validate_shape(&plan)
        }
        "verdict" => match payload["verdict"].as_str() {
            Some("PASS") | Some("FAIL") => Ok(()),
            _ => bail!("verdict must be PASS or FAIL"),
        },
        "decision" => match payload["decision"].as_str() {
            Some("abort") => Ok(()),
            Some("retry") => {
                serde_json::from_value::<crate::mission::plan::TaskContract>(
                    payload["revised_task"].clone(),
                )
                .context("revised_task is not a contract")?;
                Ok(())
            }
            _ => bail!("decision must be retry or abort"),
        },
        _ => Ok(()), // "any" — evidence only; no contract to satisfy
    }
}

fn next_path(dir: &Path) -> Result<PathBuf> {
    let mut n = 0usize;
    for f in std::fs::read_dir(dir).context("artifact dir")? {
        if let Some(name) = f?.file_name().to_str().map(|s| s.to_string()) {
            if let Some(num) = name.strip_suffix(".json").and_then(|s| s.parse().ok()) {
                n = n.max(num);
            }
        }
    }
    let n = n + 1;
    if n > MAX_ARTIFACTS {
        bail!("artifact submission limit ({MAX_ARTIFACTS}) reached");
    }
    Ok(dir.join(format!("{n:03}.json")))
}

/// Read every artifact a session dropped, oldest first.
pub fn read_artifacts(dir: &Path) -> Vec<Value> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|f| f.ok())
                .map(|f| f.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| v["payload"].clone())
        .collect()
}

fn tool_error(msg: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }], "isError": true })
}

fn tool_ok(msg: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": msg }] })
}

fn respond(out: &mut impl Write, id: &Value, result: Value) {
    let msg = json!({ "jsonrpc": "2.0", "id": id, "result": result });
    let _ = out.write_all(serde_json::to_string(&msg).unwrap().as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

fn respond_err(out: &mut impl Write, id: &Value, code: i64, msg: &str) {
    let msg = json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": msg } });
    let _ = out.write_all(serde_json::to_string(&msg).unwrap().as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Blocking stdio loop — runs inside the agent-spawned subprocess.
/// Exits on stdin EOF (agent gone or MCP shutdown).
pub fn serve(dir: &Path, expect: &str) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // notifications carry no id — never respond
        let Some(id) = req.get("id").cloned() else {
            continue;
        };
        match req["method"].as_str().unwrap_or("") {
            "initialize" => respond(
                &mut out,
                &id,
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "sui-artifacts", "version": env!("CARGO_PKG_VERSION") }
                }),
            ),
            "ping" => respond(&mut out, &id, json!({})),
            "tools/list" => respond(&mut out, &id, json!({ "tools": [tool_schema()] })),
            "tools/call" => {
                let name = req["params"]["name"].as_str().unwrap_or("");
                if name != "submit_result" {
                    respond_err(&mut out, &id, -32602, "unknown tool");
                    continue;
                }
                let payload = req["params"]["arguments"]["payload"].clone();
                match validate_payload(expect, &payload) {
                    Err(e) => respond(
                        &mut out,
                        &id,
                        tool_error(&format!(
                            "status: error\npayload rejected: {e:#}\nfix and resubmit"
                        )),
                    ),
                    Ok(()) => match next_path(dir).and_then(|p| {
                        std::fs::write(
                            &p,
                            serde_json::to_string(&json!({
                                "received_ts": std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis(),
                                "payload": payload,
                            }))
                            .unwrap_or_default(),
                        )
                        .with_context(|| format!("write {}", p.display()))
                    }) {
                        Ok(()) => {
                            respond(&mut out, &id, tool_ok("status: success\nresult accepted"))
                        }
                        Err(e) => {
                            respond(&mut out, &id, tool_error(&format!("status: error\n{e:#}")))
                        }
                    },
                }
            }
            _ => respond_err(&mut out, &id, -32601, "method not found"),
        }
    }
    Ok(())
}

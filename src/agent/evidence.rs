//! Bounded runtime observations, independent of generated summaries. A tool
//! exit is evidence of that execution, never a mission gate or delivery proof.
use serde_json::{json, Value};
use std::collections::{BTreeMap, VecDeque};

#[derive(Default)]
pub(super) struct Evidence {
    records: VecDeque<Value>,
    total: u64,
    tools: BTreeMap<String, u64>,
    failed_requests: u64,
}

impl Evidence {
    pub fn observe(&mut self, kind: &str, data: &Value) {
        let record = match kind {
            "tool" => {
                let name = data["name"].as_str().unwrap_or("unknown");
                let name = if super::KNOWN_TOOLS.contains(&name) || name == "submit_result" {
                    name
                } else {
                    "unknown"
                };
                *self.tools.entry(name.into()).or_default() += 1;
                let mut record = json!({
                    "kind": "tool",
                    "name": name,
                    "executed": data["executed"],
                    "status": data["status"],
                    "exit_code": data["exit_code"],
                    "capture_truncated": data["truncated"],
                    "args_sha256": crate::context::sha256_hex(data["args"].as_str().unwrap_or("").as_bytes()),
                    "result_sha256": crate::context::sha256_hex(data["result"].as_str().unwrap_or("").as_bytes()),
                });
                if name == "bash" {
                    if let Ok(args) =
                        serde_json::from_str::<Value>(data["args"].as_str().unwrap_or(""))
                    {
                        if let Some(command) = args["command"].as_str() {
                            let command = crate::export::Redactor::new(vec![]).text(command);
                            record["command"] = json!(bounded(&command, 320));
                        }
                    }
                }
                record
            }
            "request" if data["error_class"].is_string() => {
                self.failed_requests += 1;
                json!({"kind": "request_failure", "request_id": data["request_id"],
                    "error_class": data["error_class"], "diagnostic": data["diagnostic"],
                    "elapsed_ms": data["timing"]["request_total_ms"]})
            }
            _ => return,
        };
        self.total += 1;
        self.records.push_back(record);
        while self.records.len() > 16 {
            self.records.pop_front();
        }
    }

    pub fn snapshot(&self) -> Value {
        let mut records = self.records.clone();
        loop {
            let snapshot = json!({
                "observation": "recorded_activity_not_delivery_proof",
                "tool_results": self.tools,
                "failed_requests": self.failed_requests,
                "omitted_records": self.total.saturating_sub(records.len() as u64),
                "records": records,
            });
            if snapshot.to_string().len() <= 6000 || records.is_empty() {
                return snapshot;
            }
            records.pop_front();
        }
    }
}

fn bounded(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        text.to_string()
    } else {
        let end = crate::context::floor_char_boundary(text, limit);
        format!("{}…<truncated>", &text[..end])
    }
}

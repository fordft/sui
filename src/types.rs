use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// Raw JSON string, possibly streamed in fragments before completion.
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionCall,
}

/// OpenAI-compatible chat message. Internally tagged on `role`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_calls: Option<Vec<ToolCall>>,
        /// Provider-required reasoning state (e.g. DeepSeek thinking mode).
        /// Preserved verbatim across tool turns; ignored by providers that
        /// do not use it.
        #[serde(skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        /// Raw Responses-API items (e.g. encrypted reasoning) captured from
        /// a codex-oauth turn, replayed verbatim on the next request while
        /// `store:false`. Never serialized into chat-completions bodies.
        #[serde(skip)]
        response_items: Vec<serde_json::Value>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// Provider-reported usage. Every field optional: None means "not reported",
/// which is NOT the same as zero. `complete` is false when the stream ended
/// before a usage chunk arrived (e.g. interrupted stream).
#[derive(Debug, Clone, Default, Serialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub complete: bool,
}

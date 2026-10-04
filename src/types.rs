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
        content: UserContent,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: Option<u64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub complete: bool,
    #[serde(default)]
    pub estimated: bool,
}

/// Text stays a JSON string (old journals/providers remain compatible).
/// Image observations use the standard chat-completions content-part array.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    pub detail: String,
}

impl From<String> for UserContent {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}
impl From<&str> for UserContent {
    fn from(text: &str) -> Self {
        Self::Text(text.into())
    }
}
impl UserContent {
    pub fn image(label: String, url: String) -> Self {
        Self::Parts(vec![
            ContentPart::Text { text: label },
            ContentPart::ImageUrl {
                image_url: ImageUrl {
                    url,
                    detail: "auto".into(),
                },
            },
        ])
    }

    /// Conservative image reserve; an estimate, never provider usage/cost.
    pub fn estimated_chars(&self) -> usize {
        match self {
            Self::Text(s) => s.len(),
            Self::Parts(parts) => parts
                .iter()
                .map(|p| match p {
                    ContentPart::Text { text } => text.len(),
                    ContentPart::ImageUrl { .. } => 16_384,
                })
                .sum(),
        }
    }

    pub fn responses_parts(&self) -> Vec<serde_json::Value> {
        match self {
            Self::Text(text) => vec![serde_json::json!({"type": "input_text", "text": text})],
            Self::Parts(parts) => parts.iter().map(|p| match p {
                ContentPart::Text { text } => serde_json::json!({"type": "input_text", "text": text}),
                ContentPart::ImageUrl { image_url } => serde_json::json!({
                    "type": "input_image", "image_url": image_url.url, "detail": image_url.detail,
                }),
            }).collect(),
        }
    }
}

#[cfg(test)]
mod image_tests {
    use super::*;

    #[test]
    fn old_text_wire_and_image_history_remain_stable() {
        let old = r#"{"role":"user","content":"hello"}"#;
        let message: Message = serde_json::from_str(old).unwrap();
        assert_eq!(serde_json::to_string(&message).unwrap(), old);
        let mut history = vec![
            message,
            Message::User {
                content: UserContent::image(
                    "screen".into(),
                    format!("data:image/png;base64,{}", "a".repeat(100000)),
                ),
            },
        ];
        let prefix = serde_json::to_string(&history).unwrap();
        let decoded: Vec<Message> = serde_json::from_str(&prefix).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), prefix);
        let estimate = crate::context::estimate_tokens(&crate::context::Compiled::view(&history));
        assert!(
            (4096..4200).contains(&estimate),
            "image reserve must not count base64 as text"
        );
        history.push(Message::User {
            content: "next".into(),
        });
        assert!(serde_json::to_string(&history)
            .unwrap()
            .starts_with(&prefix[..prefix.len() - 1]));
    }
}

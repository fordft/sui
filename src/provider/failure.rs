//! Safe provider diagnostics and retry decisions. Never copy arbitrary error
//! payloads: gateways can echo credentials in messages, codes, or HTTP bodies.
use futures_util::StreamExt;
use serde_json::Value;
use std::fmt;

#[derive(Debug)]
pub(crate) struct Failure {
    pub class: &'static str,
    pub code: &'static str,
    pub retryable: bool,
    status: Option<u16>,
}

impl Failure {
    /// Retain only recognized codes from a bounded HTTP error envelope. This
    /// distinguishes permanent quota/auth faults from transient HTTP 429s.
    pub async fn response(response: reqwest::Response) -> Self {
        let mut failure = Self::http(response.status().as_u16());
        if !failure.retryable {
            return failure;
        }
        let mut stream = response.bytes_stream();
        let Ok(bytes) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let mut bytes = Vec::new();
            while let Some(Ok(chunk)) = stream.next().await {
                if bytes.len().saturating_add(chunk.len()) > 16_384 {
                    break;
                }
                bytes.extend_from_slice(&chunk);
            }
            bytes
        })
        .await
        else {
            return failure;
        };
        if let Ok(body) = serde_json::from_slice::<Value>(&bytes) {
            let code = Self::stream(body.get("error").unwrap_or(&body));
            if code.code != "unknown_provider_error" {
                failure.code = code.code;
                failure.retryable &= code.retryable;
            }
        }
        failure
    }

    pub fn http(status: u16) -> Self {
        Self {
            class: "http_error",
            code: "http_status",
            retryable: matches!(status, 408 | 429 | 500..=599),
            status: Some(status),
        }
    }

    pub fn stream(error: &Value) -> Self {
        // Both fields are untrusted; retain only recognized diagnostic codes.
        let code = [error["code"].as_str(), error["type"].as_str()]
            .into_iter()
            .flatten()
            .find_map(known_code)
            .unwrap_or("unknown_provider_error");
        Self {
            class: "stream_error",
            code,
            retryable: matches!(
                code,
                "unknown_provider_error"
                    | "server_error"
                    | "internal_server_error"
                    | "rate_limit_exceeded"
                    | "rate_limit_error"
                    | "overloaded_error"
                    | "service_unavailable"
                    | "timeout"
            ),
            status: None,
        }
    }

    pub fn transport(error: reqwest::Error) -> Self {
        Self {
            class: "transport_error",
            code: if error.is_timeout() {
                "timeout"
            } else if error.is_connect() {
                "connection_failed"
            } else {
                "stream_read_failed"
            },
            retryable: error.is_timeout()
                || error.is_connect()
                || error.is_body()
                || error.is_request(),
            status: None,
        }
    }

    pub fn deadline() -> Self {
        Self {
            class: "deadline_exceeded",
            code: "request_deadline",
            retryable: true,
            status: None,
        }
    }

    pub fn interrupted() -> Self {
        Self {
            class: "stream_interrupted",
            code: "missing_terminal_event",
            retryable: true,
            status: None,
        }
    }

    pub fn incomplete(reason: &str) -> Self {
        Self {
            class: "stream_error",
            code: match reason {
                "max_output_tokens" | "length" => "max_output_tokens",
                "content_filter" => "content_filter",
                _ => "incomplete_response",
            },
            retryable: false,
            status: None,
        }
    }
}

fn known_code(code: &str) -> Option<&'static str> {
    Some(match code {
        "server_error" => "server_error",
        "internal_server_error" => "internal_server_error",
        "rate_limit_exceeded" => "rate_limit_exceeded",
        "rate_limit_error" => "rate_limit_error",
        "overloaded_error" => "overloaded_error",
        "service_unavailable" => "service_unavailable",
        "timeout" => "timeout",
        "invalid_api_key" => "invalid_api_key",
        "authentication_error" => "authentication_error",
        "permission_denied" => "permission_denied",
        "permission_error" => "permission_error",
        "model_not_found" => "model_not_found",
        "invalid_request_error" => "invalid_request_error",
        "invalid_request" => "invalid_request",
        "unsupported_parameter" => "unsupported_parameter",
        "context_length_exceeded" => "context_length_exceeded",
        "insufficient_quota" => "insufficient_quota",
        "billing_hard_limit_reached" => "billing_hard_limit_reached",
        "content_policy_violation" => "content_policy_violation",
        _ => return None,
    })
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(status) = self.status {
            write!(f, "provider http {status}")?;
            if self.code != "http_status" {
                write!(f, ": {}", self.code)?;
            }
            Ok(())
        } else {
            write!(f, "provider {}: {}", self.class, self.code)
        }
    }
}

impl std::error::Error for Failure {}

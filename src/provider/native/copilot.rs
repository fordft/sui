use super::*;

async fn catalog(client: &reqwest::Client, token: &auth::AccountToken) -> Result<Value> {
    auth::json_response(
        auth::copilot_headers(
            client
                .get(format!("{}/models", token.base))
                .bearer_auth(&token.access),
        )
        .send()
        .await
        .map_err(crate::provider::failure::Failure::transport)?,
        "Copilot model catalog",
    )
    .await
}
fn entries(value: &Value) -> impl Iterator<Item = &Value> {
    let has_picker = value["data"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v["model_picker_enabled"] == true));
    value["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|v| {
            v["capabilities"]["type"]
                .as_str()
                .is_none_or(|t| t == "chat")
        })
        .filter(move |v| !has_picker || v["model_picker_enabled"].as_bool().unwrap_or(true))
}
pub(super) async fn stream(
    request: Request<'_, '_>,
    on_delta: impl FnMut(&str),
    on_reasoning: impl FnMut(&str),
) -> Result<StreamOutcome> {
    let token = auth::account_token(LoginProvider::Copilot, request.client).await?;
    stream_with_token(request, &token, on_delta, on_reasoning).await
}
async fn stream_with_token(
    request: Request<'_, '_>,
    token: &auth::AccountToken,
    on_delta: impl FnMut(&str),
    on_reasoning: impl FnMut(&str),
) -> Result<StreamOutcome> {
    let catalog = request
        .catalog
        .get_or_try_init(|| catalog(request.client, token))
        .await?;
    let entry = entries(catalog)
        .find(|e| e["id"] == request.model)
        .context("selected Copilot model is unavailable; choose a model from its catalog")?;
    let endpoints = entry["supported_endpoints"].as_array();
    let supports =
        |name: &str| endpoints.is_none_or(|a| a.iter().any(|v| v.as_str() == Some(name)));
    // Older Copilot catalogs omit endpoint metadata. Match jcode's known
    // Responses-only GPT-5.6 route in that case; explicit metadata wins.
    let responses_only =
        endpoints.is_none() && request.model.to_ascii_lowercase().starts_with("gpt-5.6");
    let start = Instant::now();
    let response = if supports("/chat/completions") && !responses_only {
        let body = json!({"model":request.model,"messages":request.messages,"tools":request.tools,
            "stream":true,"stream_options":{"include_usage":true}});
        auth::copilot_headers(
            request
                .client
                .post(format!("{}/chat/completions", token.base))
                .bearer_auth(&token.access),
        )
        .header("X-Initiator", "agent")
        .json(&body)
        .send()
        .await
        .map_err(failure::Failure::transport)?
    } else if supports("/responses") {
        let (instructions, input) = crate::codex::build_input(request.messages);
        let body = json!({"model":request.model,"instructions":instructions,"input":input,
            "tools":crate::codex::build_tools(request.tools),"stream":true,"store":false});
        let response = auth::copilot_headers(
            request
                .client
                .post(format!("{}/responses", token.base))
                .bearer_auth(&token.access),
        )
        .header("X-Initiator", "agent")
        .json(&body)
        .send()
        .await
        .map_err(failure::Failure::transport)?;
        if !response.status().is_success() {
            return Err(failure::Failure::response(response).await.into());
        }
        return responses::read(response, start, on_delta, on_reasoning).await;
    } else {
        bail!("selected Copilot model has no supported coding endpoint");
    };
    if !response.status().is_success() {
        return Err(failure::Failure::response(response).await.into());
    }
    super::super::read_chat(response, start, on_delta, on_reasoning).await
}
pub(super) async fn models() -> Result<Vec<ModelInfo>> {
    let client = auth::client()?;
    let token = auth::account_token(LoginProvider::Copilot, &client).await?;
    let value = catalog(&client, &token).await?;
    let mut models: Vec<_> = entries(&value)
        .filter_map(|v| {
            let mut entry = model(v["id"].as_str()?.into());
            entry.context_length =
                v["capabilities"]["limits"]["max_context_window_tokens"].as_u64();
            entry.tools_claimed = v["capabilities"]["supports"]["tool_calls"].as_bool();
            Some(entry)
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn catalog_routes_chat_and_responses_with_copilot_headers() {
        for (id, path, responses) in [
            ("chat-model", "/chat/completions", false),
            ("response-model", "/responses", true),
        ] {
            let catalog = json!({"data":[{"id":id,"model_picker_enabled":true,
                "supported_endpoints":[path],"capabilities":{"type":"chat"}}]})
            .to_string();
            let reply = if responses {
                format!(
                    "data: {}\n\ndata: {}\n\n",
                    json!({"type":"response.output_text.delta","delta":"42"}),
                    json!({"type":"response.completed","response":{"model":id,"status":"completed","output":[],
                        "usage":{"input_tokens":20,"output_tokens":2}}})
                )
            } else {
                format!(
                    "data: {}\n\n",
                    json!({"model":id,"choices":[{"delta":{"content":"42"},"finish_reason":"stop"}]})
                )
            };
            let (base, captured, task) = crate::test_http::server(vec![
                (200, "application/json", catalog),
                (200, "text/event-stream", reply),
            ]);
            let token = auth::AccountToken {
                access: "copilot-fixture-token".into(),
                base,
                project: None,
            };
            let client = auth::client().unwrap();
            let cache = tokio::sync::OnceCell::new();
            let messages = [Message::User {
                content: "Return 42.".into(),
            }];
            let compiled = crate::context::Compiled::view(&messages);
            let request = Request {
                client: &client,
                base: "copilot://oauth",
                api_key: None,
                model: id,
                session_id: "session",
                messages: &compiled,
                tools: &[],
                catalog: &cache,
            };
            let out = stream_with_token(request, &token, |_| {}, |_| {})
                .await
                .unwrap();
            assert_eq!(out.content, "42");
            assert_eq!(out.finish_reason.as_deref(), Some("stop"));
            task.join().unwrap();
            let requests = captured.lock().unwrap();
            assert!(requests[0].0.starts_with("GET /models"));
            assert!(requests[1].0.starts_with(&format!("POST {path}")));
            assert!(requests[1]
                .0
                .to_ascii_lowercase()
                .contains("copilot-integration-id: vscode-chat"));
            assert!(requests[1]
                .0
                .to_ascii_lowercase()
                .contains("authorization: bearer copilot-fixture-token"));
            let body: Value = serde_json::from_str(&requests[1].1).unwrap();
            assert_eq!(body["model"], id);
            assert_eq!(body.get("input").is_some(), responses);
        }
    }
}

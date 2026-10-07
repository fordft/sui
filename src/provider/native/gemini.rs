use super::*;
use crate::types::{ContentPart, UserContent};

fn push(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(previous) = contents.last_mut().filter(|m| m["role"] == role) {
        previous["parts"].as_array_mut().unwrap().extend(parts);
    } else {
        contents.push(json!({"role":role,"parts":parts}));
    }
}
fn schema(value: &Value) -> Result<Value> {
    match value {
        Value::Object(fields) => {
            if fields.contains_key("$ref") {
                bail!("Gemini tool schemas must inline references");
            }
            let mut clean = serde_json::Map::new();
            for (key, value) in fields {
                if matches!(
                    key.as_str(),
                    "additionalProperties" | "$schema" | "$id" | "$defs" | "default"
                ) {
                    continue;
                }
                let value = if key == "properties" {
                    let properties = value
                        .as_object()
                        .context("invalid tool schema properties")?;
                    Value::Object(
                        properties
                            .iter()
                            .map(|(name, v)| Ok((name.clone(), schema(v)?)))
                            .collect::<Result<serde_json::Map<_, _>>>()?,
                    )
                } else {
                    schema(value)?
                };
                clean.insert(key.clone(), value);
            }
            Ok(Value::Object(clean))
        }
        Value::Array(values) => Ok(Value::Array(
            values.iter().map(schema).collect::<Result<_>>()?,
        )),
        other => Ok(other.clone()),
    }
}
pub(super) fn body(request: &Request<'_, '_>) -> Result<Value> {
    let mut system = Vec::new();
    let mut contents = Vec::new();
    let mut calls = BTreeMap::new();
    for message in request.messages.iter() {
        match message {
            Message::System { content } => system.push(json!({"text":content})),
            Message::User { content } => {
                let parts = match content {
                    UserContent::Text(text) => vec![json!({"text":text})],
                    UserContent::Parts(parts) => parts
                        .iter()
                        .map(|part| match part {
                            ContentPart::Text { text } => Ok(json!({"text":text})),
                            ContentPart::ImageUrl { image_url } => {
                                let (mime, data) = image_source(&image_url.url)?;
                                Ok(json!({"inlineData":{"mimeType":mime,"data":data}}))
                            }
                        })
                        .collect::<Result<Vec<_>>>()?,
                };
                push(&mut contents, "user", parts);
            }
            Message::Assistant {
                content,
                tool_calls,
                response_items,
                ..
            } => {
                let parts = if let Some(replay) =
                    response_items.iter().find(|p| p["type"] == "gemini_parts")
                {
                    replay["parts"]
                        .as_array()
                        .context("invalid Gemini replay parts")?
                        .clone()
                } else {
                    let mut parts = Vec::new();
                    if let Some(text) = content.as_ref().filter(|s| !s.is_empty()) {
                        parts.push(json!({"text":text}));
                    }
                    for call in tool_calls.iter().flatten() {
                        parts.push(json!({"functionCall":{"name":call.function.name,"id":call.id,
                            "args":serde_json::from_str::<Value>(&call.function.arguments).context("invalid tool arguments in history")?}}));
                    }
                    parts
                };
                let mut wire_calls = parts.iter().filter_map(|part| part.get("functionCall"));
                for call in tool_calls.iter().flatten() {
                    let wire = wire_calls
                        .next()
                        .context("Gemini replay is missing a tool call")?;
                    calls.insert(
                        call.id.clone(),
                        (
                            call.function.name.clone(),
                            wire["id"].as_str().map(str::to_string),
                        ),
                    );
                }
                push(&mut contents, "model", parts);
            }
            Message::Tool {
                tool_call_id,
                content,
            } => {
                let (name, id) = calls
                    .get(tool_call_id)
                    .context("Gemini tool result has no matching call")?;
                // Older Gemini models omit call IDs. Keep generated runtime
                // IDs internal and match the provider's original wire ID.
                let mut response = json!({"name":name,"response":{"output":content}});
                if let Some(id) = id {
                    response["id"] = json!(id);
                }
                push(
                    &mut contents,
                    "user",
                    vec![json!({"functionResponse":response})],
                );
            }
        }
    }
    let declarations: Vec<_> = request
        .tools
        .iter()
        .map(|tool| {
            Ok(json!({
                "name":tool["function"]["name"], "description":tool["function"]["description"],
                "parameters":schema(&tool["function"]["parameters"])?,
            }))
        })
        .collect::<Result<_>>()?;
    let mut body = json!({"contents":contents,"generationConfig":{"maxOutputTokens":8192}});
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts":system});
    }
    if !declarations.is_empty() {
        body["tools"] = json!([{"functionDeclarations":declarations}]);
        body["toolConfig"] = json!({"functionCallingConfig":{"mode":"AUTO"}});
    }
    Ok(body)
}
async fn assist(client: &reqwest::Client, token: &auth::AccountToken) -> Result<Value> {
    let project = std::env::var("GOOGLE_CLOUD_PROJECT")
        .ok()
        .filter(|s| !s.is_empty());
    let mut body = json!({"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}});
    if let Some(project) = project {
        body["cloudaicompanionProject"] = json!(project);
    }
    auth::json_response(
        client
            .post(format!("{}:loadCodeAssist", token.base))
            .bearer_auth(&token.access)
            .json(&body)
            .send()
            .await
            .map_err(failure::Failure::transport)?,
        "Gemini Code Assist setup",
    )
    .await
}
async fn project(client: &reqwest::Client, token: &auth::AccountToken) -> Result<String> {
    if let Some(project) = &token.project {
        return Ok(project.clone());
    }
    let setup = assist(client, token).await?;
    let configured = std::env::var("GOOGLE_CLOUD_PROJECT")
        .ok()
        .filter(|s| !s.is_empty());
    if let Some(project) = setup["cloudaicompanionProject"]
        .as_str()
        .map(str::to_string)
        .or_else(|| {
            setup["cloudaicompanionProject"]["id"]
                .as_str()
                .map(str::to_string)
        })
        .or_else(|| {
            setup["currentTier"]
                .is_object()
                .then(|| configured.clone())
                .flatten()
        })
    {
        auth::save_gemini_project(&project).await?;
        return Ok(project);
    }
    if setup["currentTier"].is_object() {
        bail!("Gemini account requires GOOGLE_CLOUD_PROJECT");
    }
    let tiers = setup["allowedTiers"]
        .as_array()
        .context("Gemini account has no eligible Code Assist tier")?;
    let tier = tiers
        .iter()
        .find(|t| t["isDefault"] == true)
        .or_else(|| tiers.iter().find(|t| t["id"] == "free-tier"))
        .context("Gemini account has no eligible Code Assist tier")?;
    let id = tier["id"].as_str().context("missing Gemini tier id")?;
    let mut body = json!({"tierId":id,"metadata":{"ideType":"IDE_UNSPECIFIED","platform":"PLATFORM_UNSPECIFIED","pluginType":"GEMINI"}});
    if id != "free-tier" {
        body["cloudaicompanionProject"] =
            json!(configured.context("Gemini tier requires GOOGLE_CLOUD_PROJECT")?);
    }
    let mut operation = auth::json_response(
        client
            .post(format!("{}:onboardUser", token.base))
            .bearer_auth(&token.access)
            .json(&body)
            .send()
            .await
            .map_err(failure::Failure::transport)?,
        "Gemini Code Assist onboarding",
    )
    .await?;
    for _ in 0..24 {
        if operation["done"] == true {
            if operation["error"].is_object() {
                bail!("Gemini Code Assist onboarding failed");
            }
            let project = operation["response"]["cloudaicompanionProject"]["id"]
                .as_str()
                .context("Gemini onboarding returned no project")?
                .to_string();
            auth::save_gemini_project(&project).await?;
            return Ok(project);
        }
        let name = operation["name"]
            .as_str()
            .filter(|n| n.starts_with("operations/") && !n.contains(".."))
            .context("invalid Gemini onboarding operation")?
            .to_string();
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        operation = auth::json_response(
            client
                .get(format!("{}/{name}", token.base))
                .bearer_auth(&token.access)
                .send()
                .await
                .map_err(crate::provider::failure::Failure::transport)?,
            "Gemini onboarding status",
        )
        .await?;
    }
    bail!("Gemini onboarding timed out; retry")
}
pub(super) async fn stream(
    request: Request<'_, '_>,
    oauth: bool,
    mut on_delta: impl FnMut(&str),
    mut on_reasoning: impl FnMut(&str),
) -> Result<StreamOutcome> {
    let mut body = body(&request)?;
    let http = if oauth {
        let token = auth::account_token(LoginProvider::Gemini, request.client).await?;
        let project = project(request.client, &token).await?;
        body["session_id"] = json!(request.session_id);
        let envelope = json!({"model":request.model,"project":project,
            "user_prompt_id":format!("{:032x}",rand::random::<u128>()),"request":body});
        request
            .client
            .post(format!("{}:streamGenerateContent?alt=sse", token.base))
            .bearer_auth(token.access)
            .json(&envelope)
    } else {
        let model = request
            .model
            .strip_prefix("models/")
            .unwrap_or(request.model);
        let mut url =
            reqwest::Url::parse(&format!("{}/models/", request.base.trim_end_matches('/')))?;
        url.path_segments_mut()
            .map_err(|_| anyhow::anyhow!("invalid Gemini endpoint"))?
            .pop_if_empty()
            .push(&format!("{model}:streamGenerateContent"));
        url.query_pairs_mut().append_pair("alt", "sse");
        request
            .client
            .post(url)
            .header(
                "x-goog-api-key",
                request
                    .api_key
                    .context("Gemini needs sign-in or an API key")?,
            )
            .json(&body)
    };
    let start = Instant::now();
    let response = http.send().await.map_err(failure::Failure::transport)?;
    let mut out = empty();
    let mut first = None;
    let mut parts = Vec::new();
    let call_prefix = format!("gemini-{:032x}", rand::random::<u128>());
    events(response, |event| {
        let event = event.get("response").unwrap_or(&event);
        if event["error"].is_object() {
            return Err(failure::Failure::stream(&event["error"]).into());
        }
        if let Some(model) = event["modelVersion"].as_str() {
            out.returned_model = Some(model.into());
        }
        if event["usageMetadata"].is_object() {
            let u = &event["usageMetadata"];
            out.usage = Some(Usage {
                input_tokens: u["promptTokenCount"].as_u64(),
                cache_read_tokens: u["cachedContentTokenCount"].as_u64(),
                cache_write_tokens: None,
                // Total output includes thinking. A missing thinking bucket
                // is unknown; Google's reported total can still prove it.
                output_tokens: u["candidatesTokenCount"]
                    .as_u64()
                    .and_then(|n| n.checked_add(u["thoughtsTokenCount"].as_u64()?))
                    .or_else(|| {
                        u["totalTokenCount"]
                            .as_u64()?
                            .checked_sub(u["promptTokenCount"].as_u64()?)
                    }),
                complete: false,
                estimated: false,
            });
        }
        let candidate = &event["candidates"][0];
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if parts.len() >= 4096 {
                bail!("too many Gemini response parts");
            }
            parts.push(part.clone());
            if let Some(text) = part["text"].as_str() {
                first.get_or_insert_with(|| start.elapsed().as_millis());
                if part["thought"] == true {
                    append(out.reasoning_content.get_or_insert_with(String::new), text)?;
                    on_reasoning(text);
                } else {
                    append(&mut out.content, text)?;
                    on_delta(text);
                }
            }
            if part["functionCall"].is_object() {
                if out.tool_calls.len() >= 256 {
                    bail!("too many Gemini tool calls");
                }
                let call = &part["functionCall"];
                first.get_or_insert_with(|| start.elapsed().as_millis());
                let id = call["id"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{call_prefix}-{}", out.tool_calls.len()));
                out.tool_calls.push(ToolCall {
                    id,
                    kind: "function".into(),
                    function: FunctionCall {
                        name: call["name"]
                            .as_str()
                            .context("Gemini tool call missing name")?
                            .into(),
                        arguments: call.get("args").unwrap_or(&json!({})).to_string(),
                    },
                });
            }
        }
        if let Some(reason) = candidate["finishReason"].as_str() {
            out.finish_reason = Some(
                match reason {
                    "STOP" if !out.tool_calls.is_empty() => "tool_calls",
                    "STOP" => "stop",
                    "MAX_TOKENS" => "length",
                    "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" => {
                        "content_filter"
                    }
                    other => other,
                }
                .into(),
            );
        }
        if event["promptFeedback"]["blockReason"].is_string() {
            out.finish_reason = Some("content_filter".into());
        }
        Ok(())
    })
    .await?;
    if parts.iter().any(|p| {
        p.get("thoughtSignature").is_some()
            || p.get("functionCall").is_some()
            || p["thought"] == true
    }) {
        if serde_json::to_vec(&parts)?.len() > 4_194_304 {
            bail!("Gemini replay state exceeds 4 MiB");
        }
        out.response_items = vec![json!({"type":"gemini_parts","parts":parts})];
    }
    Ok(finish(out, start, first))
}
fn collect_models(value: &Value, out: &mut std::collections::BTreeSet<String>) {
    match value {
        Value::String(id)
            if id.starts_with("gemini-")
                && id.len() <= 128
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_')) =>
        {
            out.insert(id.clone());
        }
        Value::Array(values) => {
            for v in values {
                collect_models(v, out);
            }
        }
        Value::Object(fields) => {
            for v in fields.values() {
                collect_models(v, out);
            }
        }
        _ => {}
    }
}
pub(super) async fn models(base: &str, key: Option<&str>, oauth: bool) -> Result<Vec<ModelInfo>> {
    let client = auth::client()?;
    if oauth {
        let token = auth::account_token(LoginProvider::Gemini, &client).await?;
        let value = assist(&client, &token).await?;
        let mut ids = std::collections::BTreeSet::new();
        collect_models(&value, &mut ids);
        // Code Assist often omits its model catalog. Suggestions have no
        // capability/price claims; users can always enter an exact model ID.
        if ids.is_empty() {
            ids.extend(["gemini-2.5-pro", "gemini-2.5-flash"].map(str::to_string));
        }
        return Ok(ids.into_iter().map(model).collect());
    }
    let mut models = Vec::new();
    let mut page = None;
    for _ in 0..16 {
        let mut request = client
            .get(format!("{}/models", base.trim_end_matches('/')))
            .header("x-goog-api-key", key.context("Gemini needs an API key")?)
            .query(&[("pageSize", "100")]);
        if let Some(page) = &page {
            request = request.query(&[("pageToken", page)]);
        }
        let value = auth::json_response(
            request
                .send()
                .await
                .map_err(crate::provider::failure::Failure::transport)?,
            "Gemini model catalog",
        )
        .await?;
        for item in value["models"].as_array().into_iter().flatten() {
            let Some(id) = item["name"].as_str() else {
                continue;
            };
            if !item["supportedGenerationMethods"]
                .as_array()
                .is_some_and(|a| a.iter().any(|s| s == "generateContent"))
            {
                continue;
            }
            let mut entry = model(id.trim_start_matches("models/").into());
            entry.context_length = item["inputTokenLimit"].as_u64();
            models.push(entry);
        }
        page = value["nextPageToken"]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        if page.is_none() {
            models.sort_by(|a, b| a.id.cmp(&b.id));
            return Ok(models);
        }
    }
    bail!("Gemini catalog exceeds 1600 models")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn oauth_free_tier_onboarding_resolves_and_persists_the_project() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let _home = crate::test_http::AuthHome::new();
        auth::save_credentials(
            LoginProvider::Gemini,
            &auth::Credentials {
                access_token: "gemini-access".into(),
                refresh_token: "gemini-refresh".into(),
                expires_at: auth::now() + 3600,
                project: None,
                api_base: None,
            },
        )
        .unwrap();
        let (base,captured,task) = crate::test_http::server(vec![
            (200,"application/json",json!({"allowedTiers":[{"id":"free-tier","isDefault":true}]}).to_string()),
            (200,"application/json",json!({"done":true,"response":{"cloudaicompanionProject":{"id":"managed-project"}}}).to_string()),
        ]);
        let token = auth::AccountToken {
            access: "gemini-access".into(),
            base: format!("{base}/v1internal"),
            project: None,
        };
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            assert_eq!(
                project(&auth::client().unwrap(), &token).await.unwrap(),
                "managed-project"
            );
            let token = auth::account_token(LoginProvider::Gemini, &auth::client().unwrap())
                .await
                .unwrap();
            assert_eq!(token.project.as_deref(), Some("managed-project"));
        });
        task.join().unwrap();
        let requests = captured.lock().unwrap();
        assert!(requests[0].0.starts_with("POST /v1internal:loadCodeAssist"));
        assert!(requests[1].0.starts_with("POST /v1internal:onboardUser"));
        let body: Value = serde_json::from_str(&requests[1].1).unwrap();
        assert_eq!(body["tierId"], "free-tier");
        assert!(body.get("cloudaicompanionProject").is_none());
    }
}

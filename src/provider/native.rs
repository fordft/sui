//! Copilot's native account adapter. The agent loop consumes one StreamOutcome.
mod copilot;
use super::*;
use crate::auth::{self, LoginProvider};

pub(super) struct Request<'a, 'm> {
    pub client: &'a reqwest::Client,
    pub model: &'a str,
    pub messages: &'a crate::context::Compiled<'m>,
    pub tools: &'a [Value],
    pub catalog: &'a tokio::sync::OnceCell<Value>,
}
pub(super) async fn stream(
    request: Request<'_, '_>,
    on_delta: impl FnMut(&str),
    on_reasoning: impl FnMut(&str),
) -> Result<StreamOutcome> {
    copilot::stream(request, on_delta, on_reasoning).await
}
pub(super) async fn models() -> Result<Vec<ModelInfo>> {
    copilot::models().await
}
fn model(id: String) -> ModelInfo {
    ModelInfo {
        id,
        context_length: None,
        price_in: None,
        price_out: None,
        tools_claimed: None,
    }
}

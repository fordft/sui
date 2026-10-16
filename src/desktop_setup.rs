//! Provider operations before a workspace connection exists. Credential-bearing
//! transfer frames are private to the Electron main process and SSH stdio.
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum Request {
    Profiles,
    Models {
        name: String,
        #[serde(default)]
        refresh: bool,
    },
    Login {
        name: String,
        #[serde(default)]
        registration: Option<crate::chatgpt::Account>,
        #[serde(default)]
        transfer: bool,
    },
    Registration {
        name: String,
    },
    Import {
        name: String,
        record: Value,
    },
    Select {
        name: String,
        model: String,
    },
    Logout {
        name: String,
    },
}
fn emit(value: &Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}
pub async fn run() -> Result<()> {
    let mut bytes = Vec::new();
    std::io::Read::take(std::io::stdin().lock(), 128 * 1024 + 1).read_until(b'\n', &mut bytes)?;
    if bytes.len() > 128 * 1024 {
        bail!("provider request exceeds 128 KiB");
    }
    let request: Request =
        serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid provider request"))?;
    let result = match request {
        Request::Profiles => {
            let profiles = crate::config::profiles(None)?;
            json!({"type":"profiles","truncated":profiles.len() > 200,"items":profiles.into_iter().take(200).map(|(name,p)| {
                let account = (p.kind.as_deref() == Some("chatgpt")).then(|| crate::chatgpt::summary(&name));
                json!({"name":name,"model":p.model,"kind":p.kind,"account":account})
            }).collect::<Vec<_>>()})
        }
        Request::Models { name, refresh } => {
            let profile = crate::config::resolve_profile(&name, None)?;
            if profile.transport == crate::config::Transport::Chatgpt {
                let catalog = crate::chatgpt::catalog(&name, refresh).await?;
                json!({"type":"models","truncated":catalog.items.len() > 200,"items":catalog.items.into_iter().take(200).collect::<Vec<_>>(),"cached":catalog.cached,"fetched_at":catalog.fetched_at,"warning":catalog.warning})
            } else {
                let models = crate::provider::list_models_with_transport(
                    &profile.base_url,
                    profile.api_key.as_deref(),
                    profile.transport,
                )
                .await?;
                json!({"type":"models","truncated":models.len()>200,"items":models.into_iter().take(200).map(|m|json!({"id":m.id,"name":m.id})).collect::<Vec<_>>()})
            }
        }
        Request::Login {
            name,
            transfer,
            registration,
        } => {
            let login = crate::chatgpt::Login::begin(&name, registration).await?;
            // This URL may include an ID-token hint. Main opens it directly;
            // it must never be emitted as a renderer event or diagnostic.
            emit(&json!({"type":"authorize","url":login.url}))?;
            let result = login.finish(transfer).await?;
            if !transfer {
                // Model selection belongs to the composer, independent of sign-in.
                let existing = crate::config::profiles(None)?
                    .get(&name)
                    .and_then(|p| p.model.clone())
                    .unwrap_or_default();
                crate::config::save_profile(&name, "", &existing, None, None, Some("chatgpt"))?;
            }
            result
        }
        Request::Registration { name } => crate::chatgpt::registration(&name)?,
        Request::Import { name, record } => {
            let result = crate::chatgpt::import(&name, record).await?;
            let existing = crate::config::profiles(None)?
                .get(&name)
                .and_then(|p| p.model.clone())
                .unwrap_or_default();
            crate::config::save_profile(&name, "", &existing, None, None, Some("chatgpt"))?;
            result
        }
        Request::Select { name, model } => {
            let profile = crate::config::resolve_profile(&name, None)?;
            if profile.transport != crate::config::Transport::Chatgpt {
                bail!("model selection requires a ChatGPT profile");
            }
            let models = crate::chatgpt::models(&name).await?;
            if !models.iter().any(|m| m.0 == model) {
                bail!("choose a model available to this ChatGPT account");
            }
            crate::config::save_profile(&name, "", &model, None, None, Some("chatgpt"))?;
            json!({"type":"profile_saved","name":name})
        }
        Request::Logout { name } => crate::chatgpt::logout(&name).await?,
    };
    emit(&result).context("send provider result")
}

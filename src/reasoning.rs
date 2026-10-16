//! Request-level reasoning controls. Model IDs remain opaque wire identifiers;
//! recognized OpenAI families supply conservative choices for the Desktop UI.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Effort {
    Default,
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}
impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
    pub fn wire(self) -> Option<&'static str> {
        (self != Self::Default).then(|| self.as_str())
    }
}

pub fn choices(profile: &crate::config::Profile) -> Vec<Effort> {
    use crate::config::Transport;
    use Effort::*;
    let mut choices = vec![Default];
    if matches!(
        profile.transport,
        Transport::CodexOauth | Transport::Chatgpt | Transport::OpenaiResponses
    ) {
        let model = profile.model.rsplit('/').next().unwrap_or(&profile.model);
        if model.starts_with("gpt-6.1-sol")
            || model.starts_with("gpt-6-astra")
            || model.starts_with("gpt-6-luna")
        {
            choices.extend([Low, Medium, High, Xhigh, Max]);
        } else if model.starts_with("gpt-6-sol")
            || model.starts_with("gpt-5.5")
            || model.starts_with("gpt-5.6")
        {
            choices.extend([None, Low, Medium, High, Xhigh, Max]);
        } else if profile.transport == Transport::CodexOauth {
            // Legacy Codex already uses high; do not advertise other unknown
            // levels merely because an arbitrary model ID was configured.
            choices.push(High);
        }
        if let Some(configured) = profile.reasoning_effort {
            if !choices.contains(&configured) {
                choices.push(configured);
            }
        }
    }
    choices
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Profile, Transport};
    use crate::provider::Provider;

    #[test]
    fn model_choices_and_session_identity_preserve_provider_defaults() {
        let mut profile = Profile {
            name: "test".into(),
            transport: Transport::OpenaiResponses,
            model: "route/gpt-6.1-sol".into(),
            base_url: "http://localhost/v1".into(),
            image_input: false,
            api_key: None,
            prompt_cache_key: None,
            pricing: None,
            reasoning_effort: None,
        };
        assert_eq!(
            choices(&profile),
            vec![
                Effort::Default,
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::Xhigh,
                Effort::Max
            ]
        );
        let original = Provider::from_profile(&profile).resume_fingerprint();
        profile.reasoning_effort = Some(Effort::Default);
        assert_eq!(
            original,
            Provider::from_profile(&profile).resume_fingerprint()
        );
        profile.reasoning_effort = Some(Effort::Medium);
        assert_ne!(
            original,
            Provider::from_profile(&profile).resume_fingerprint()
        );
        profile.model = "unknown-model".into();
        profile.reasoning_effort = None;
        assert_eq!(choices(&profile), vec![Effort::Default]);
        profile.transport = Transport::CodexOauth;
        assert_eq!(choices(&profile), vec![Effort::Default, Effort::High]);
    }
}

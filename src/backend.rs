//! Backend selection at the mission execution boundary.
//!
//! `Backend::Native` is Sui's own provider/tool loop (the whole `Agent`
//! machinery). `Backend::Acp` is an external coding agent over ACP stdio
//! — it owns its internal model/tool loop; Sui still owns contracts,
//! worktrees, gates, and acceptance. An external agent is NEVER modeled
//! as an OpenAI-compatible provider URL.

use crate::config::{AcpSpec, Profile};

#[derive(Debug, Clone)]
pub enum Backend {
    /// Sui's native API-backed agent loop on this provider profile.
    Native(Profile),
    /// External coding agent over ACP stdio (trusted executable spec).
    Acp(AcpSpec),
}

impl Backend {
    pub fn native(p: Profile) -> Self {
        Backend::Native(p)
    }

    /// Display label for logs/UI: profile name or `acp:<agent>`.
    pub fn label(&self) -> String {
        match self {
            Backend::Native(p) => format!("native:{}", p.name),
            Backend::Acp(s) => format!("acp:{}", s.name),
        }
    }

    /// Effective model identifier, when known.
    pub fn model(&self) -> &str {
        match self {
            Backend::Native(p) => &p.model,
            Backend::Acp(s) => s.model.as_deref().unwrap_or("agent-default"),
        }
    }
}

pub mod agent;
pub mod charter;
pub mod codex;
pub mod config;
pub mod context;
pub mod events;
pub mod export;
pub mod journal;
pub mod mission;
pub mod permission;
pub mod provider;
pub mod session;
pub mod skills;
pub mod tools;
pub mod tui;
pub mod types;
pub mod web;

/// Serializes unit tests that mutate process env (SUI_HOME / CODEX_HOME /
/// API keys) — env is process-global, so every such test must hold this.
#[cfg(test)]
pub(crate) static TEST_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

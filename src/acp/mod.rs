//! ACP (Agent Client Protocol) backend plane.
//!
//! External coding agents (Devin via `devin acp`, Codex via codex-acp)
//! run as trusted subprocesses over JSON-RPC stdio. They own their own
//! model/tool loops; Sui owns task contracts, worktrees, scheduling, and
//! every deterministic gate. See module docs in each file.

pub mod bridge;
pub mod driver;
pub mod norm;

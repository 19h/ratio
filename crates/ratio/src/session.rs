//! Session persistence — saves and loads session IDs so `--resume` works.
//!
//! Writes a `.ratio-session.json` file in the working directory containing
//! the session IDs for both agents and the current orchestration phase.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name for the persisted session state.
const SESSION_FILE: &str = ".ratio-session.json";

/// Persisted session state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// The ACP session ID of the reviewer agent.
    pub reviewer_session_id: String,
    /// The ACP session ID of the worker agent.
    pub worker_session_id: String,
    /// Which agent was last active: "reviewer" or "worker".
    pub last_active_agent: String,
    /// The orchestration phase at save time: "planning", "working",
    /// "reviewing", "revising", "approved", "failed", etc.
    pub phase: String,
    /// The current review cycle number.
    pub cycle: usize,
    /// The goal (for validation on resume).
    pub goal: String,
}

impl SessionState {
    /// Path to the session file for a given working directory.
    pub fn path(cwd: &Path) -> PathBuf {
        cwd.join(SESSION_FILE)
    }

    /// Save session state to disk.
    pub fn save(&self, cwd: &Path) -> anyhow::Result<()> {
        let path = Self::path(cwd);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Load session state from disk, if it exists.
    pub fn load(cwd: &Path) -> anyhow::Result<Option<Self>> {
        let path = Self::path(cwd);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let state: Self = serde_json::from_str(&content)?;
        Ok(Some(state))
    }

    /// Remove the session file.
    pub fn remove(cwd: &Path) {
        let path = Self::path(cwd);
        let _ = std::fs::remove_file(path);
    }
}

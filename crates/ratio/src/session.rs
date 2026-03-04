//! Session persistence — saves and loads session IDs so `--resume` works.
//!
//! Writes a `.ratio-session.json` file in the working directory containing
//! the session IDs for both agents and the current orchestration phase.
//!
//! A companion `.ratio-ui-state.json` file stores the TUI state (todos and
//! log entries) so they survive across resume cycles.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// File name for the persisted session state.
const SESSION_FILE: &str = ".ratio-session.json";
/// File name for the persisted UI state (todos + logs).
const UI_STATE_FILE: &str = ".ratio-ui-state.json";

/// A saved stakeholder session — name + ACP session ID.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedStakeholderSession {
    /// The stakeholder's index in the config `[[stakeholders]]` array.
    pub index: usize,
    /// The stakeholder's display name (for matching on resume).
    pub name: String,
    /// The ACP session ID of this stakeholder's opencode process.
    pub session_id: String,
}

/// Persisted session state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    /// The ACP session ID of the reviewer agent.
    pub reviewer_session_id: String,
    /// The ACP session ID of the worker agent.
    pub worker_session_id: String,
    /// Which agent was last active: "reviewer", "worker", or
    /// "stakeholder:<index>" (e.g. "stakeholder:0").
    pub last_active_agent: String,
    /// The orchestration phase at save time: "planning", "working",
    /// "reviewing", "revising", "approved", "failed", etc.
    pub phase: String,
    /// The current review cycle number.
    pub cycle: usize,
    /// The goal (for validation on resume).
    pub goal: String,
    /// Saved session IDs for each stakeholder that was active.
    /// Defaults to empty for backward compatibility with old session files.
    #[serde(default)]
    pub stakeholder_sessions: Vec<SavedStakeholderSession>,
}

/// Persisted UI state — todos and log entries that survive across resumes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UIState {
    pub todos: Vec<SavedTodoItem>,
    pub logs: Vec<SavedLogEntry>,
}

/// A serialisable todo item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedTodoItem {
    pub content: String,
    pub status: String,
    pub priority: String,
}

/// A serialisable log entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedLogEntry {
    pub timestamp: String,
    pub level: String,
    pub message: String,
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
        UIState::remove(cwd);
    }
}

impl UIState {
    /// Path to the UI state file for a given working directory.
    pub fn path(cwd: &Path) -> PathBuf {
        cwd.join(UI_STATE_FILE)
    }

    /// Save UI state to disk.
    pub fn save(&self, cwd: &Path) -> anyhow::Result<()> {
        let path = Self::path(cwd);
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    /// Load UI state from disk, if it exists.
    pub fn load(cwd: &Path) -> Option<Self> {
        let path = Self::path(cwd);
        let content = std::fs::read_to_string(path).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Remove the UI state file.
    pub fn remove(cwd: &Path) {
        let path = Self::path(cwd);
        let _ = std::fs::remove_file(path);
    }
}

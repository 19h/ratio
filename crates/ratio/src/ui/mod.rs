//! Terminal user interface module.
//!
//! Provides a professional, interactive TUI built on ratatui and crossterm.
//! Features:
//! - Real-time streaming of agent output
//! - Phase indicator and review cycle tracker
//! - Tool call activity feed
//! - Log pane with orchestrator messages
//! - Emergency kill switch (Ctrl+K or Ctrl+C double-tap)
//! - Scrollable panes with keyboard navigation

pub mod app;
pub mod events;
pub mod render;
pub mod widgets;

pub use app::App;
pub use events::EventLoop;

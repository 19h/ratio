//! Input event handling and the main TUI event loop.
//!
//! Uses crossterm's async `EventStream` so that terminal input polling
//! does NOT block the single-threaded tokio runtime — this is critical
//! because the orchestrator, ACP I/O, and event forwarding tasks all
//! run on the same `LocalSet`.

use std::time::{Duration, Instant};

use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyModifiers};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::orchestrator::OrchestratorEvent;

use super::app::{App, FocusedPane};

/// Minimum interval between Ctrl+C presses to count as a "double tap".
const DOUBLE_TAP_WINDOW: Duration = Duration::from_millis(800);

/// Actions the event loop can produce.
pub enum Action {
    /// Redraw the UI.
    Redraw,
    /// The user wants to quit (graceful).
    Quit,
    /// The user wants to kill all agents immediately.
    Kill,
}

/// The combined event loop that drives the TUI.
pub struct EventLoop {
    orch_rx: mpsc::UnboundedReceiver<OrchestratorEvent>,
    abort_tx: mpsc::UnboundedSender<()>,
    term_events: EventStream,
    last_ctrl_c: Option<Instant>,
}

impl EventLoop {
    pub fn new(
        orch_rx: mpsc::UnboundedReceiver<OrchestratorEvent>,
        abort_tx: mpsc::UnboundedSender<()>,
    ) -> Self {
        Self {
            orch_rx,
            abort_tx,
            term_events: EventStream::new(),
            last_ctrl_c: None,
        }
    }

    /// Run one tick of the event loop. Returns an action to take.
    ///
    /// Uses `tokio::select!` to race between:
    /// 1. Orchestrator events
    /// 2. Terminal input events (async via EventStream)
    /// 3. A short sleep to force periodic redraws
    ///
    /// This is fully async and does NOT block the runtime.
    pub async fn tick(&mut self, app: &mut App) -> Action {
        // Drain all immediately-available orchestrator events first.
        while let Ok(evt) = self.orch_rx.try_recv() {
            app.handle_event(evt);
        }

        // Now race: wait for either a terminal event, an orchestrator event,
        // or a timeout (for periodic redraw).
        tokio::select! {
            // Terminal input (async, non-blocking).
            maybe_event = self.term_events.next() => {
                if let Some(Ok(Event::Key(key))) = maybe_event {
                    return self.handle_key(app, key);
                }
            }
            // Orchestrator event.
            Some(evt) = self.orch_rx.recv() => {
                app.handle_event(evt);
                // Drain any more that arrived.
                while let Ok(evt) = self.orch_rx.try_recv() {
                    app.handle_event(evt);
                }
            }
            // Periodic redraw (50ms).
            _ = tokio::time::sleep(Duration::from_millis(50)) => {}
        }

        Action::Redraw
    }

    fn handle_key(&mut self, app: &mut App, key: KeyEvent) -> Action {
        // ── Emergency kill: Ctrl+K ──────────────────────────────
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('k') {
            app.abort_requested = true;
            let _ = self.abort_tx.send(());
            return Action::Kill;
        }

        // ── Ctrl+C double-tap to kill ───────────────────────────
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            let now = Instant::now();
            if let Some(last) = self.last_ctrl_c {
                if now.duration_since(last) < DOUBLE_TAP_WINDOW {
                    app.abort_requested = true;
                    let _ = self.abort_tx.send(());
                    return Action::Kill;
                }
            }
            self.last_ctrl_c = Some(now);
            app.ctrl_c_count += 1;
            return Action::Redraw;
        } else {
            app.ctrl_c_count = 0;
        }

        // ── Quit: q when finished ───────────────────────────────
        if key.code == KeyCode::Char('q') && app.finished {
            return Action::Quit;
        }

        // ── Tab / Shift+Tab: cycle focused pane ─────────────────
        if key.code == KeyCode::Tab {
            if key.modifiers.contains(KeyModifiers::SHIFT) {
                app.focused = app.focused.prev();
            } else {
                app.focused = app.focused.next();
            }
            return Action::Redraw;
        }
        if key.code == KeyCode::BackTab {
            app.focused = app.focused.prev();
            return Action::Redraw;
        }

        // ── Scrolling ───────────────────────────────────────────
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => app.scroll_up(1),
            KeyCode::Down | KeyCode::Char('j') => app.scroll_down(1),
            KeyCode::PageUp => app.scroll_up(20),
            KeyCode::PageDown => app.scroll_down(20),
            KeyCode::Home => {
                match app.focused {
                    FocusedPane::Reviewer => {
                        app.reviewer_scroll = 0;
                        app.auto_scroll_reviewer = false;
                    }
                    FocusedPane::Worker => {
                        app.worker_scroll = 0;
                        app.auto_scroll_worker = false;
                    }
                    FocusedPane::Tools => {
                        app.tool_scroll = 0;
                        app.auto_scroll_tools = false;
                    }
                    FocusedPane::Log => {
                        app.log_scroll = 0;
                        app.auto_scroll_log = false;
                    }
                }
            }
            KeyCode::End => app.scroll_to_bottom(),
            _ => {}
        }

        Action::Redraw
    }
}

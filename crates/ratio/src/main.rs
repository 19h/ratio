//! RA — entry point.
//!
//! Parses CLI arguments, loads configuration, spawns both the reviewer and
//! worker agent subprocesses, and runs the TUI event loop alongside the
//! orchestration engine.

use std::io;
use std::path::PathBuf;

use clap::Parser;
use crossterm::ExecutableCommand;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::prelude::*;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

use agent_client_protocol as acp;
use ra::config::{CliOverrides, Config};
use ra::orchestrator::{
    LiveStakeholder, Orchestrator, OrchestratorEvent, UserMessage, UserMessageTarget,
};
use ra::protocol::AgentEvent;
use ra::session::SessionState;
use ra::subprocess::{AgentRole, spawn_agent};
use ra::ui::app::AgentSource;
use ra::ui::events::Action;
use ra::ui::render;
use ra::ui::{App, EventLoop};

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "ra",
    about = "LLM agent orchestrator — reviewer-driven code generation via ACP",
    long_about = "RA orchestrates two LLM agents (reviewer + worker) through \
                  iterative review cycles. The reviewer formulates work instructions, \
                  the worker executes them, and the reviewer validates the output — \
                  enforcing user-specified tools, approaches, and constraints.",
    version
)]
struct Cli {
    /// The goal to accomplish (detailed natural-language description).
    #[arg(short, long)]
    goal: Option<String>,

    /// Path to a TOML configuration file.
    #[arg(short, long, value_name = "FILE")]
    config: Option<PathBuf>,

    /// Working directory for both agents.
    #[arg(short = 'C', long, value_name = "DIR")]
    cwd: Option<PathBuf>,

    /// LLM model for the worker agent.
    #[arg(long, value_name = "MODEL")]
    worker_model: Option<String>,

    /// LLM model for the reviewer agent.
    #[arg(long, value_name = "MODEL")]
    reviewer_model: Option<String>,

    /// Maximum number of review cycles (0 = unlimited, default).
    #[arg(long, value_name = "N")]
    max_cycles: Option<usize>,

    /// Run in headless mode (no TUI, output to stdout).
    #[arg(long)]
    headless: bool,

    /// Log all ACP protocol messages to stderr (headless mode only).
    #[arg(long)]
    debug: bool,

    /// Resume a previous session from saved state.
    #[arg(long)]
    resume: bool,
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    // Initialize tracing (logs go to file, not terminal).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ra=info".parse().unwrap()),
        )
        .with_writer(|| {
            let path = std::env::temp_dir().join("ra.log");
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap_or_else(|_| {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open("/dev/null")
                        .unwrap()
                })
        })
        .init();

    let cli = Cli::parse();

    // Load configuration.
    let mut config = if let Some(ref path) = cli.config {
        Config::from_file(path)?
    } else {
        Config::default()
    };

    config.apply_overrides(&CliOverrides {
        goal: cli.goal,
        cwd: cli.cwd,
        worker_model: cli.worker_model,
        reviewer_model: cli.reviewer_model,
        max_iterations: cli.max_cycles,
    });

    config.validate()?;

    let resume = cli.resume;

    if cli.headless {
        run_headless(config, resume, cli.debug).await
    } else {
        run_tui(config, resume).await
    }
}

// ---------------------------------------------------------------------------
// TUI mode
// ---------------------------------------------------------------------------

async fn run_tui(config: Config, resume: bool) -> anyhow::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run_tui_inner(&mut terminal, config, resume).await;

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    result
}

async fn run_tui_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: Config,
    resume: bool,
) -> anyhow::Result<()> {
    let local_set = tokio::task::LocalSet::new();

    local_set
        .run_until(async move {
            // ── Channels ────────────────────────────────────────
            let (orch_event_tx, orch_event_rx) = mpsc::unbounded_channel::<OrchestratorEvent>();
            let (worker_event_tx, worker_event_rx) = mpsc::unbounded_channel::<AgentEvent>();
            let (reviewer_event_tx, reviewer_event_rx) = mpsc::unbounded_channel::<AgentEvent>();
            let (user_msg_tx, user_msg_rx) = mpsc::unbounded_channel::<UserMessage>();
            let (abort_tx, abort_rx) = mpsc::unbounded_channel::<()>();

            // ── App state ───────────────────────────────────────
            let mut app = App::new(config.goal.clone(), config.cwd.clone());

            // ── Load saved session if resuming ──────────────────
            let saved_session = if resume {
                match SessionState::load(&config.cwd)? {
                    Some(state) => {
                        app.current_cycle = state.cycle;
                        app.restore_ui_state();
                        Some(state)
                    }
                    None => {
                        anyhow::bail!(
                            "No saved session found at {}",
                            SessionState::path(&config.cwd).display()
                        );
                    }
                }
            } else {
                None
            };

            // ── Spawn reviewer agent ────────────────────────────
            let (mut reviewer_conn, mut reviewer_proc, reviewer_io) = spawn_agent(
                AgentRole::Reviewer,
                &config.reviewer,
                &config.cwd,
                reviewer_event_tx,
            )?;

            // Drain reviewer stderr to tracing (prevents pipe buffer deadlock).
            if let Some(stderr) = reviewer_proc.stderr.take() {
                tokio::task::spawn_local(drain_stderr(stderr, "reviewer", false));
            }

            tokio::task::spawn_local(async move {
                if let Err(e) = reviewer_io.await {
                    tracing::error!("Reviewer ACP I/O error: {e}");
                }
            });

            if let Some(ref state) = saved_session {
                reviewer_conn
                    .load_existing_session(state.reviewer_session_id.clone(), &config.cwd)
                    .await?;
            } else {
                reviewer_conn.handshake(&config.cwd).await?;
            }

            // Set the reviewer model if configured.
            if !config.reviewer.model.is_empty() {
                reviewer_conn.set_model(&config.reviewer.model).await?;
            }

            // ── Spawn worker agent ──────────────────────────────
            let (mut worker_conn, mut worker_proc, worker_io) = spawn_agent(
                AgentRole::Worker,
                &config.worker,
                &config.cwd,
                worker_event_tx,
            )?;

            // Drain worker stderr to tracing (prevents pipe buffer deadlock).
            if let Some(stderr) = worker_proc.stderr.take() {
                tokio::task::spawn_local(drain_stderr(stderr, "worker", false));
            }

            tokio::task::spawn_local(async move {
                if let Err(e) = worker_io.await {
                    tracing::error!("Worker ACP I/O error: {e}");
                }
            });

            if let Some(ref state) = saved_session {
                worker_conn
                    .load_existing_session(state.worker_session_id.clone(), &config.cwd)
                    .await?;
            } else {
                worker_conn.handshake(&config.cwd).await?;
            }

            // Set the worker model if configured.
            if !config.worker.model.is_empty() {
                worker_conn.set_model(&config.worker.model).await?;
            }

            // ── Spawn stakeholder agents ─────────────────────────
            let mut live_stakeholders = Vec::new();
            let mut stakeholder_event_rxs = Vec::new();
            for (i, sh_cfg) in config.stakeholders.iter().enumerate() {
                let agent_cfg = sh_cfg.agent.as_ref().unwrap_or(&config.reviewer);
                let (sh_event_tx, sh_event_rx) = mpsc::unbounded_channel();
                match spawn_agent(
                    AgentRole::Reviewer, // stakeholders use reviewer role (read-only)
                    agent_cfg,
                    &config.cwd,
                    sh_event_tx,
                ) {
                    Ok((mut sh_conn, mut sh_proc, sh_io)) => {
                        if let Some(stderr) = sh_proc.stderr.take() {
                            tokio::task::spawn_local(drain_stderr(stderr, "stakeholder", false));
                        }
                        tokio::task::spawn_local(async move {
                            if let Err(e) = sh_io.await {
                                tracing::error!("Stakeholder I/O error: {e}");
                            }
                        });
                        if let Err(e) = sh_conn.handshake(&config.cwd).await {
                            tracing::warn!("Stakeholder '{}' handshake failed: {e}", sh_cfg.name);
                            continue;
                        }
                        if let Some(ref ac) = sh_cfg.agent {
                            if !ac.model.is_empty() {
                                let _ = sh_conn.set_model(&ac.model).await;
                            }
                        }
                        live_stakeholders.push(LiveStakeholder {
                            index: i,
                            name: sh_cfg.name.clone(),
                            conn: sh_conn,
                            proc: sh_proc,
                        });
                        stakeholder_event_rxs.push(sh_event_rx);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to spawn stakeholder '{}': {e}", sh_cfg.name);
                    }
                }
            }

            // Register stakeholder names with the TUI so it can create streams.
            {
                let names: Vec<String> = live_stakeholders.iter().map(|s| s.name.clone()).collect();
                app.register_stakeholders(&names);
            }

            // ── Spawn orchestrator ──────────────────────────────
            let orch_tx = orch_event_tx.clone();
            let config_clone = config.clone();
            let resume_phase = saved_session.as_ref().map(|s| s.phase.clone());
            let resume_agent = saved_session.as_ref().map(|s| s.last_active_agent.clone());
            tokio::task::spawn_local(async move {
                let mut orchestrator = Orchestrator::new(config_clone, orch_tx);
                if let Some(ref phase) = resume_phase {
                    let continue_msg = format!(
                        "Continue where you left off. The session was interrupted during the \
                         '{phase}' phase. Pick up from where you stopped and complete the task."
                    );
                    match resume_agent.as_deref() {
                        Some("worker") => {
                            let _ = worker_conn.prompt(&continue_msg).await;
                        }
                        _ => {
                            let _ = reviewer_conn.prompt(&continue_msg).await;
                        }
                    }
                }
                let _ = orchestrator
                    .run(
                        &reviewer_conn,
                        &worker_conn,
                        &mut reviewer_proc,
                        &mut worker_proc,
                        &mut live_stakeholders,
                        stakeholder_event_rxs,
                        worker_event_rx,
                        reviewer_event_rx,
                        user_msg_rx,
                        abort_rx,
                    )
                    .await;
            });

            // ── TUI event loop ──────────────────────────────────
            let mut event_loop = EventLoop::new(orch_event_rx, abort_tx);

            loop {
                terminal.draw(|frame| {
                    render::render(frame, &mut app);
                })?;

                // Drain queued user messages and forward to orchestrator.
                while let Some(msg) = app.message_queue.pop_front() {
                    let target = match msg.target {
                        AgentSource::Worker => UserMessageTarget::Worker,
                        AgentSource::Reviewer => UserMessageTarget::Reviewer,
                        AgentSource::Stakeholder(idx, _) => UserMessageTarget::Stakeholder(idx),
                    };

                    if let Err(e) = user_msg_tx.send(UserMessage {
                        target,
                        text: msg.text,
                        immediate: msg.immediate,
                    }) {
                        tracing::warn!("Failed to forward user message to orchestrator: {e}");
                    }
                }

                match event_loop.tick(&mut app).await {
                    Action::Redraw => {}
                    Action::Quit => break,
                    Action::Kill => {
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        break;
                    }
                }
            }

            anyhow::Ok(())
        })
        .await
}

// ---------------------------------------------------------------------------
// Headless mode
// ---------------------------------------------------------------------------

async fn run_headless(config: Config, resume: bool, debug: bool) -> anyhow::Result<()> {
    let local_set = tokio::task::LocalSet::new();

    local_set
        .run_until(async move {
            let (orch_event_tx, mut orch_event_rx) = mpsc::unbounded_channel::<OrchestratorEvent>();
            let (worker_event_tx, worker_event_rx) = mpsc::unbounded_channel::<AgentEvent>();
            let (reviewer_event_tx, reviewer_event_rx) = mpsc::unbounded_channel::<AgentEvent>();
            let (_user_msg_tx, user_msg_rx) = mpsc::unbounded_channel::<UserMessage>();
            let (_abort_tx, abort_rx) = mpsc::unbounded_channel::<()>();

            let saved_session = if resume {
                match SessionState::load(&config.cwd)? {
                    Some(state) => Some(state),
                    None => {
                        anyhow::bail!(
                            "No saved session found at {}",
                            SessionState::path(&config.cwd).display()
                        );
                    }
                }
            } else {
                None
            };

            // Spawn reviewer.
            let (mut reviewer_conn, mut reviewer_proc, reviewer_io) = spawn_agent(
                AgentRole::Reviewer,
                &config.reviewer,
                &config.cwd,
                reviewer_event_tx,
            )?;

            // Drain reviewer stderr (to terminal in --debug, otherwise silently).
            if let Some(stderr) = reviewer_proc.stderr.take() {
                tokio::task::spawn_local(drain_stderr(stderr, "reviewer", debug));
            }

            tokio::task::spawn_local(async move {
                if let Err(e) = reviewer_io.await {
                    eprintln!("[ra] Reviewer I/O error: {e}");
                }
            });

            if let Some(ref state) = saved_session {
                reviewer_conn
                    .load_existing_session(state.reviewer_session_id.clone(), &config.cwd)
                    .await?;
            } else {
                reviewer_conn.handshake(&config.cwd).await?;
            }

            // Set the reviewer model if configured.
            if !config.reviewer.model.is_empty() {
                reviewer_conn.set_model(&config.reviewer.model).await?;
            }

            // Spawn worker.
            let (mut worker_conn, mut worker_proc, worker_io) = spawn_agent(
                AgentRole::Worker,
                &config.worker,
                &config.cwd,
                worker_event_tx,
            )?;

            // Drain worker stderr (to terminal in --debug, otherwise silently).
            if let Some(stderr) = worker_proc.stderr.take() {
                tokio::task::spawn_local(drain_stderr(stderr, "worker", debug));
            }

            tokio::task::spawn_local(async move {
                if let Err(e) = worker_io.await {
                    eprintln!("[ra] Worker I/O error: {e}");
                }
            });

            if let Some(ref state) = saved_session {
                worker_conn
                    .load_existing_session(state.worker_session_id.clone(), &config.cwd)
                    .await?;
            } else {
                worker_conn.handshake(&config.cwd).await?;
            }

            // Set the worker model if configured.
            if !config.worker.model.is_empty() {
                worker_conn.set_model(&config.worker.model).await?;
            }

            // ── Spawn stakeholder agents ─────────────────────────
            let mut live_stakeholders = Vec::new();
            let mut stakeholder_event_rxs = Vec::new();
            for (i, sh_cfg) in config.stakeholders.iter().enumerate() {
                let agent_cfg = sh_cfg.agent.as_ref().unwrap_or(&config.reviewer);
                let (sh_event_tx, sh_event_rx) = mpsc::unbounded_channel();
                match spawn_agent(
                    AgentRole::Reviewer, // stakeholders use reviewer role (read-only)
                    agent_cfg,
                    &config.cwd,
                    sh_event_tx,
                ) {
                    Ok((mut sh_conn, mut sh_proc, sh_io)) => {
                        if let Some(stderr) = sh_proc.stderr.take() {
                            tokio::task::spawn_local(drain_stderr(stderr, "stakeholder", debug));
                        }
                        tokio::task::spawn_local(async move {
                            if let Err(e) = sh_io.await {
                                eprintln!("[ra] Stakeholder I/O error: {e}");
                            }
                        });
                        if let Err(e) = sh_conn.handshake(&config.cwd).await {
                            eprintln!("[ra] Stakeholder '{}' handshake failed: {e}", sh_cfg.name);
                            continue;
                        }
                        if let Some(ref ac) = sh_cfg.agent {
                            if !ac.model.is_empty() {
                                let _ = sh_conn.set_model(&ac.model).await;
                            }
                        }
                        live_stakeholders.push(LiveStakeholder {
                            index: i,
                            name: sh_cfg.name.clone(),
                            conn: sh_conn,
                            proc: sh_proc,
                        });
                        stakeholder_event_rxs.push(sh_event_rx);
                    }
                    Err(e) => {
                        eprintln!("[ra] Failed to spawn stakeholder '{}': {e}", sh_cfg.name);
                    }
                }
            }

            // Subscribe to raw ACP streams before moving connections
            // into the orchestrator (--debug mode).
            if debug {
                eprintln!("[ra] debug: ACP protocol logging enabled");

                let mut worker_stream = worker_conn.subscribe();
                tokio::task::spawn_local(async move {
                    while let Ok(msg) = worker_stream.recv().await {
                        let json = format_stream_message(&msg);
                        eprintln!("[acp:worker] {json}");
                    }
                });

                let mut reviewer_stream = reviewer_conn.subscribe();
                tokio::task::spawn_local(async move {
                    while let Ok(msg) = reviewer_stream.recv().await {
                        let json = format_stream_message(&msg);
                        eprintln!("[acp:reviewer] {json}");
                    }
                });
            }

            // Spawn orchestrator.
            let orch_tx = orch_event_tx.clone();
            let config_clone = config.clone();
            let resume_phase = saved_session.as_ref().map(|s| s.phase.clone());
            let resume_agent = saved_session.as_ref().map(|s| s.last_active_agent.clone());
            tokio::task::spawn_local(async move {
                let mut orchestrator = Orchestrator::new(config_clone, orch_tx);
                if let Some(ref phase) = resume_phase {
                    let continue_msg = format!(
                        "Continue where you left off. The session was interrupted during the \
                         '{phase}' phase. Pick up from where you stopped and complete the task."
                    );
                    match resume_agent.as_deref() {
                        Some("worker") => {
                            let _ = worker_conn.prompt(&continue_msg).await;
                        }
                        _ => {
                            let _ = reviewer_conn.prompt(&continue_msg).await;
                        }
                    }
                }
                let _ = orchestrator
                    .run(
                        &reviewer_conn,
                        &worker_conn,
                        &mut reviewer_proc,
                        &mut worker_proc,
                        &mut live_stakeholders,
                        stakeholder_event_rxs,
                        worker_event_rx,
                        reviewer_event_rx,
                        user_msg_rx,
                        abort_rx,
                    )
                    .await;
            });

            // Print events to stdout.
            while let Some(evt) = orch_event_rx.recv().await {
                match evt {
                    OrchestratorEvent::PhaseChanged(phase) => {
                        eprintln!("[ra] phase: {phase:?}");
                    }
                    OrchestratorEvent::WorkerEvent(AgentEvent::TextChunk(text)) => {
                        print!("{text}");
                    }
                    OrchestratorEvent::ReviewerEvent(AgentEvent::TextChunk(text)) => {
                        eprint!("{text}");
                    }
                    OrchestratorEvent::Log(level, msg) => {
                        eprintln!("[ra] [{level:?}] {msg}");
                    }
                    OrchestratorEvent::CycleCompleted(record) => {
                        eprintln!(
                            "[ra] cycle {} completed: {:?}",
                            record.cycle, record.verdict
                        );
                    }
                    OrchestratorEvent::StakeholderEvent(
                        _idx,
                        name,
                        AgentEvent::TextChunk(text),
                    ) => {
                        eprint!("[{name}] {text}");
                    }
                    OrchestratorEvent::Finished(phase) => {
                        eprintln!("[ra] finished: {phase:?}");
                        break;
                    }
                    _ => {}
                }
            }

            anyhow::Ok(())
        })
        .await
}

// ---------------------------------------------------------------------------
// Debug helpers
// ---------------------------------------------------------------------------

/// Drain a subprocess stderr stream line by line.
///
/// When `to_stderr` is true (--debug mode), lines are printed to our stderr
/// with a `[stderr:<role>]` prefix. Otherwise they are silently discarded.
/// Either way, reading prevents the pipe buffer from filling up and deadlocking
/// the subprocess.
async fn drain_stderr(stderr: tokio::process::ChildStderr, role: &'static str, to_stderr: bool) {
    let reader = tokio::io::BufReader::new(stderr);
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if to_stderr {
            eprintln!("[stderr:{role}] {line}");
        } else {
            tracing::debug!("[{role} stderr] {line}");
        }
    }
}

/// Format an ACP stream message as a compact string for debug logging.
fn format_stream_message(msg: &acp::StreamMessage) -> String {
    use acp::{StreamMessageContent, StreamMessageDirection};

    let dir = match msg.direction {
        StreamMessageDirection::Incoming => "recv",
        StreamMessageDirection::Outgoing => "send",
    };

    match &msg.message {
        StreamMessageContent::Request { id, method, params } => {
            let params_str = params
                .as_ref()
                .map(|p| serde_json::to_string(p).unwrap_or_else(|_| format!("{p:?}")))
                .unwrap_or_default();
            format!("{dir} request id={id:?} method={method} params={params_str}")
        }
        StreamMessageContent::Response { id, result } => {
            let result_str = match result {
                Ok(Some(val)) => serde_json::to_string(val).unwrap_or_else(|_| format!("{val:?}")),
                Ok(None) => "null".to_string(),
                Err(e) => format!("error: {e:?}"),
            };
            format!("{dir} response id={id:?} result={result_str}")
        }
        StreamMessageContent::Notification { method, params } => {
            let params_str = params
                .as_ref()
                .map(|p| serde_json::to_string(p).unwrap_or_else(|_| format!("{p:?}")))
                .unwrap_or_default();
            format!("{dir} notification method={method} params={params_str}")
        }
    }
}

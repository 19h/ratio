//! Ratio — entry point.
//!
//! Parses CLI arguments, loads configuration, spawns both the reviewer and
//! worker agent subprocesses, and runs the TUI event loop alongside the
//! orchestration engine.

use std::io;
use std::path::PathBuf;

use clap::Parser;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use crossterm::ExecutableCommand;
use ratatui::prelude::*;
use tokio::sync::mpsc;

use ratio::config::{CliOverrides, Config};
use ratio::orchestrator::{Orchestrator, OrchestratorEvent};
use ratio::protocol::AgentEvent;
use ratio::subprocess::{AgentRole, spawn_agent};
use ratio::ui::{App, EventLoop};
use ratio::ui::events::Action;
use ratio::ui::render;

// ---------------------------------------------------------------------------
// CLI definition
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "ratio",
    about = "LLM agent orchestrator — reviewer-driven code generation via ACP",
    long_about = "Ratio orchestrates two LLM agents (reviewer + worker) through \
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

    /// Maximum number of review cycles before giving up.
    #[arg(long, value_name = "N")]
    max_cycles: Option<usize>,

    /// Run in headless mode (no TUI, output to stdout).
    #[arg(long)]
    headless: bool,
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
                .add_directive("ratio=info".parse().unwrap()),
        )
        .with_writer(|| {
            let path = std::env::temp_dir().join("ratio.log");
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

    if cli.headless {
        run_headless(config).await
    } else {
        run_tui(config).await
    }
}

// ---------------------------------------------------------------------------
// TUI mode
// ---------------------------------------------------------------------------

async fn run_tui(config: Config) -> anyhow::Result<()> {
    enable_raw_mode()?;
    io::stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run_tui_inner(&mut terminal, config).await;

    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;

    result
}

async fn run_tui_inner(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    config: Config,
) -> anyhow::Result<()> {
    let local_set = tokio::task::LocalSet::new();

    local_set
        .run_until(async move {
            // ── Channels ────────────────────────────────────────
            let (orch_event_tx, orch_event_rx) =
                mpsc::unbounded_channel::<OrchestratorEvent>();
            let (worker_event_tx, worker_event_rx) =
                mpsc::unbounded_channel::<AgentEvent>();
            let (reviewer_event_tx, reviewer_event_rx) =
                mpsc::unbounded_channel::<AgentEvent>();
            let (abort_tx, abort_rx) = mpsc::unbounded_channel::<()>();

            // ── App state ───────────────────────────────────────
            let mut app = App::new(
                config.goal.clone(),
                config.orchestration.max_review_cycles,
            );

            // ── Spawn reviewer agent ────────────────────────────
            let (mut reviewer_conn, mut reviewer_proc, reviewer_io) =
                spawn_agent(AgentRole::Reviewer, &config.reviewer, &config.cwd, reviewer_event_tx)?;

            tokio::task::spawn_local(async move {
                if let Err(e) = reviewer_io.await {
                    tracing::error!("Reviewer ACP I/O error: {e}");
                }
            });

            reviewer_conn.handshake(&config.cwd).await?;

            // ── Spawn worker agent ──────────────────────────────
            let (mut worker_conn, mut worker_proc, worker_io) =
                spawn_agent(AgentRole::Worker, &config.worker, &config.cwd, worker_event_tx)?;

            tokio::task::spawn_local(async move {
                if let Err(e) = worker_io.await {
                    tracing::error!("Worker ACP I/O error: {e}");
                }
            });

            worker_conn.handshake(&config.cwd).await?;

            // ── Spawn orchestrator ──────────────────────────────
            let orch_tx = orch_event_tx.clone();
            let config_clone = config.clone();
            tokio::task::spawn_local(async move {
                let mut orchestrator = Orchestrator::new(config_clone, orch_tx);
                let _ = orchestrator
                    .run(
                        &reviewer_conn,
                        &worker_conn,
                        &mut reviewer_proc,
                        &mut worker_proc,
                        worker_event_rx,
                        reviewer_event_rx,
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

async fn run_headless(config: Config) -> anyhow::Result<()> {
    let local_set = tokio::task::LocalSet::new();

    local_set
        .run_until(async move {
            let (orch_event_tx, mut orch_event_rx) =
                mpsc::unbounded_channel::<OrchestratorEvent>();
            let (worker_event_tx, worker_event_rx) =
                mpsc::unbounded_channel::<AgentEvent>();
            let (reviewer_event_tx, reviewer_event_rx) =
                mpsc::unbounded_channel::<AgentEvent>();
            let (_abort_tx, abort_rx) = mpsc::unbounded_channel::<()>();

            // Spawn reviewer.
            let (mut reviewer_conn, mut reviewer_proc, reviewer_io) =
                spawn_agent(AgentRole::Reviewer, &config.reviewer, &config.cwd, reviewer_event_tx)?;

            tokio::task::spawn_local(async move {
                if let Err(e) = reviewer_io.await {
                    eprintln!("[ratio] Reviewer I/O error: {e}");
                }
            });

            reviewer_conn.handshake(&config.cwd).await?;

            // Spawn worker.
            let (mut worker_conn, mut worker_proc, worker_io) =
                spawn_agent(AgentRole::Worker, &config.worker, &config.cwd, worker_event_tx)?;

            tokio::task::spawn_local(async move {
                if let Err(e) = worker_io.await {
                    eprintln!("[ratio] Worker I/O error: {e}");
                }
            });

            worker_conn.handshake(&config.cwd).await?;

            // Spawn orchestrator.
            let orch_tx = orch_event_tx.clone();
            let config_clone = config.clone();
            tokio::task::spawn_local(async move {
                let mut orchestrator = Orchestrator::new(config_clone, orch_tx);
                let _ = orchestrator
                    .run(
                        &reviewer_conn,
                        &worker_conn,
                        &mut reviewer_proc,
                        &mut worker_proc,
                        worker_event_rx,
                        reviewer_event_rx,
                        abort_rx,
                    )
                    .await;
            });

            // Print events to stdout.
            while let Some(evt) = orch_event_rx.recv().await {
                match evt {
                    OrchestratorEvent::PhaseChanged(phase) => {
                        eprintln!("[ratio] phase: {phase:?}");
                    }
                    OrchestratorEvent::WorkerEvent(AgentEvent::TextChunk(text)) => {
                        print!("{text}");
                    }
                    OrchestratorEvent::ReviewerEvent(AgentEvent::TextChunk(text)) => {
                        eprint!("{text}");
                    }
                    OrchestratorEvent::Log(level, msg) => {
                        eprintln!("[ratio] [{level:?}] {msg}");
                    }
                    OrchestratorEvent::CycleCompleted(record) => {
                        eprintln!(
                            "[ratio] cycle {} completed: {:?}",
                            record.cycle, record.verdict
                        );
                    }
                    OrchestratorEvent::Finished(phase) => {
                        eprintln!("[ratio] finished: {phase:?}");
                        break;
                    }
                    _ => {}
                }
            }

            anyhow::Ok(())
        })
        .await
}

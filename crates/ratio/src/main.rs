use std::{
    io::{self, IsTerminal, Write},
    process::ExitCode,
    sync::mpsc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use ra::{
    budget::Budget,
    config::{Cli, Config, ControlOperation},
    events::{GenerationControl, Reporter},
    orchestrator::Orchestrator,
    session::{Phase, Session, Store},
    ui,
};
use ratio_protocol::{ControlAction, ControlRequest, Evaluation, RunStop};

mod control;

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("ra: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<u8> {
    let cli = Cli::parse();
    if let Some(run_id) = &cli.metrics {
        let reader = Store::reader(&Config::history_workspace(&cli)?)?;
        println!("{}", serde_json::to_string(&reader.metrics(run_id)?)?);
        return Ok(0);
    }
    if let Some(run_id) = &cli.otlp_trace {
        let reader = Store::reader(&Config::history_workspace(&cli)?)?;
        println!("{}", reader.otlp_trace(run_id)?);
        return Ok(0);
    }
    if let Some(run_id) = &cli.evaluate {
        let reader = Store::reader(&Config::history_workspace(&cli)?)?;
        let rules: Evaluation = match &cli.evaluation {
            Some(path) => serde_json::from_reader(std::fs::File::open(path)?)
                .with_context(|| format!("decode {}", path.display()))?,
            None => Evaluation::default(),
        };
        let report = reader.evaluate(run_id, &rules)?;
        println!("{report}");
        return Ok(if report["passed"] == true { 0 } else { 4 });
    }
    if let Some(run_id) = &cli.attach {
        ensure!(
            io::stdin().is_terminal() && io::stderr().is_terminal(),
            "--attach requires a terminal"
        );
        let workspace = Config::history_workspace(&cli)?;
        return ui::attach(Store::reader(&workspace)?, run_id, |request| {
            let workspace = &workspace;
            async move { control::request_control(workspace, &request).await }
        })
        .await;
    }
    if let Some(operation) = cli.control {
        let workspace = Config::history_workspace(&cli)?;
        let action = match operation {
            ControlOperation::Status => ControlAction::Status,
            ControlOperation::Pause => ControlAction::Pause,
            ControlOperation::Resume => ControlAction::Resume,
            ControlOperation::Cancel => ControlAction::Cancel,
            ControlOperation::Guide => ControlAction::Guide,
        };
        let reply = control::request_control(
            &workspace,
            &ControlRequest {
                action,
                guidance: cli.guidance,
                run_id: None,
            },
        )
        .await?;
        println!("{}", serde_json::to_string(&reply)?);
        return Ok(u8::from(!reply.accepted));
    }
    if cli.detached {
        let workspace = Config::history_workspace(&cli)?;
        let (run, log) = control::launch(&workspace).await?;
        println!("{}", serde_json::json!({"run": run, "log": log}));
        return Ok(0);
    }
    if cli.list_runs || cli.events.is_some() {
        let reader = Store::reader(&Config::history_workspace(&cli)?)?;
        let mut output = io::stdout().lock();
        if let Some(run_id) = cli.events {
            ensure!(reader.run(&run_id)?.is_some(), "unknown Ratio run {run_id}");
            let mut after = cli.after;
            loop {
                let events = reader.events(&run_id, after, 256)?;
                let Some(last) = events.last() else {
                    if !cli.follow
                        || reader.latest_event(&run_id)?.is_some_and(|event| {
                            terminal_phase(&event.phase) || event.workflow.stopped.is_some()
                        })
                    {
                        break;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(150)) => continue,
                        result = tokio::signal::ctrl_c() => { result?; return Ok(130) }
                    }
                };
                after = last.sequence;
                for event in events {
                    serde_json::to_writer(&mut output, &event)?;
                    writeln!(output)?;
                }
                output.flush()?;
            }
        } else {
            for run in reader.runs()? {
                serde_json::to_writer(&mut output, &run)?;
                writeln!(output)?;
            }
        }
        return Ok(0);
    }
    let explicit_goal = cli.goal.clone();
    let run_id = cli.run_id.clone();
    let fork = cli.fork.clone();
    let managed = cli.managed_run;
    let config = Config::load(cli)?;
    let store = Store::open(&config.cwd)?;
    let budget = Budget::new(&config);
    let resumed = if config.resume {
        let session = store.load_run(run_id.as_deref())?;
        if let Some(goal) = &explicit_goal {
            ensure!(
                *goal == session.goal,
                "--resume requires the same goal as the checkpoint"
            );
        }
        Some(session)
    } else if let Some(source) = fork {
        Some(store.load_run(Some(&source))?.fork(explicit_goal))
    } else {
        None
    };
    let interactive =
        !managed && !config.headless && io::stdin().is_terminal() && io::stderr().is_terminal();
    let (generation, control_state) = tokio::sync::watch::channel(GenerationControl::default());
    let (guide, guidance) = mpsc::sync_channel(16);
    let listener = managed
        .then(|| control::Listener::bind(&config.cwd))
        .transpose()?;
    let mut session = if interactive {
        resumed
    } else {
        Some(match resumed {
            Some(session) => session,
            None => {
                let goal = config
                    .goal
                    .as_ref()
                    .filter(|text| !text.trim().is_empty())
                    .context("supply --goal or set goal in the config file")?;
                Session::new(goal.clone())
            }
        })
    };
    let active_run = session
        .as_ref()
        .map(|session| session.run_id().to_owned())
        .unwrap_or_default();
    if managed {
        if !config.resume {
            store.archive_old_checkpoint()?;
        }
        store.save(session.as_ref().expect("managed run has a session"))?;
    }
    let run = async {
        if interactive {
            return ui::run(&config, &store, session.take(), budget.clone()).await;
        }
        let session = session.take().expect("headless session initialized");
        let reporter = if managed {
            Reporter::Managed(control_state)
        } else if config.headless {
            Reporter::Silent
        } else {
            Reporter::Terminal
        };
        let orchestrator = Orchestrator::new(&config, &store, reporter, budget.clone())?;
        let session = orchestrator
            .run(session, managed.then_some(guidance))
            .await?;
        Ok(ui::Outcome::Finished(Box::new(session)))
    };
    let controls = async {
        if let Some(listener) = listener {
            control::serve(listener, &active_run, generation, guide).await
        } else {
            std::future::pending::<Result<()>>().await
        }
    };
    let outcome = tokio::select! {
        result = run => result,
        result = controls => {
            result.map(|_| ui::Outcome::Interrupted)
        }
        signal = tokio::signal::ctrl_c() => {
            signal.context("listen for interrupt").map(|_| ui::Outcome::Interrupted)
        }
    };
    if outcome.is_err() && !active_run.is_empty() {
        store.mark_stopped(&active_run, RunStop::Failed)?;
    }
    let outcome = outcome?;
    if matches!(outcome, ui::Outcome::Interrupted) {
        budget.save()?;
        if !active_run.is_empty() {
            store.mark_stopped(&active_run, RunStop::Interrupted)?;
        }
    }
    let session = match outcome {
        ui::Outcome::Finished(session) => session,
        ui::Outcome::Interrupted => {
            eprintln!("ra: interrupted.");
            return Ok(130);
        }
        ui::Outcome::BudgetExhausted(limit) => {
            println!("Run budget exhausted: {limit}");
            return Ok(3);
        }
    };
    println!("{}", session.assessment);
    Ok(match session.phase {
        Phase::Approved => 0,
        Phase::Rejected => 2,
        Phase::Exhausted => 3,
        _ => unreachable!("orchestrator returns a completed session"),
    })
}

fn terminal_phase(phase: &str) -> bool {
    matches!(phase, "Approved" | "Rejected" | "Exhausted")
}

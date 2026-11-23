use std::{
    io::{self, IsTerminal, Read, Write},
    process::ExitCode,
    sync::mpsc,
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use ra::{
    budget::Budget,
    config::{Cli, Config, ControlOperation, PlanFormat},
    control,
    events::{GenerationControl, LiveGenerations, Reporter},
    orchestrator::Orchestrator,
    session::{Phase, Session, Store},
    ui,
};
use ratio_protocol::{ControlAction, ControlRequest, Evaluation, RunStop};

mod web;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut runtime = if cli.web {
        let mut builder = tokio::runtime::Builder::new_multi_thread();
        // Bulk read, projection, and encoding lanes can occupy 16 blocking threads.
        // Leave room for runner actions and the journal observer at that limit.
        builder.worker_threads(2).max_blocking_threads(24);
        builder
    } else {
        tokio::runtime::Builder::new_current_thread()
    };
    match runtime
        .enable_all()
        .build()
        .map_err(anyhow::Error::from)
        .and_then(|runtime| runtime.block_on(run(cli)))
    {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("ra: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(mut cli: Cli) -> Result<u8> {
    if !cli.train_web_dictionaries.is_empty() {
        web::train_dictionaries(&cli.train_web_dictionaries)?;
        return Ok(0);
    }
    if cli.web {
        return web::serve(
            Config::history_workspace(&cli)?,
            cli.web_port,
            cli.web_share_port,
            cli.web_share_origin.as_deref(),
        )
        .await;
    }
    if cli.goal_stdin {
        let mut request = String::new();
        io::stdin().take(65537).read_to_string(&mut request)?;
        ensure!(
            request.len() <= 65536 && !request.trim().is_empty(),
            "stdin requires a nonempty UTF-8 request up to 65536 bytes"
        );
        cli.goal = Some(request.trim().to_owned());
    } else if let Some(request) = cli.request.take() {
        cli.goal = Some(request);
    }
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
        ensure!(
            cli.worker_request.is_none() || matches!(operation, ControlOperation::AddWorker),
            "--worker-request requires --control add-worker"
        );
        let workspace = Config::history_workspace(&cli)?;
        let action = match operation {
            ControlOperation::Status => ControlAction::Status,
            ControlOperation::Pause => ControlAction::Pause,
            ControlOperation::Resume => ControlAction::Resume,
            ControlOperation::Cancel => ControlAction::Cancel,
            ControlOperation::Guide => ControlAction::Guide,
            ControlOperation::AddWorker => ControlAction::AddWorker,
        };
        let reply = control::request_control(
            &workspace,
            &ControlRequest {
                action,
                guidance: if action == ControlAction::AddWorker {
                    cli.worker_request
                } else {
                    cli.guidance
                },
                run_id: None,
            },
        )
        .await?;
        println!("{}", serde_json::to_string(&reply)?);
        return Ok(u8::from(!reply.accepted));
    }
    if cli.detached {
        let workspace = Config::history_workspace(&cli)?;
        let (run, log) =
            control::launch(&workspace, cli.goal.as_deref().filter(|_| cli.goal_stdin)).await?;
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
    let dashboard_managed = cli.dashboard_managed;
    let plan_only = cli.plan_only;
    let plan_format = cli.plan_format;
    let check_mcp = cli.check_mcp.clone();
    let check_provider = cli.check_provider;
    let config = Config::load(cli)?;
    if check_provider {
        let api = ra::api::Api::new(&config, Reporter::Silent, Budget::new(&config))?;
        let report = api.check(&config).await;
        println!("{report}");
        let passed = report["checks"]
            .as_array()
            .is_some_and(|checks| checks.iter().all(|check| check["ok"] == true));
        return Ok(u8::from(!passed));
    }
    if let Some(server) = check_mcp {
        println!(
            "{}",
            serde_json::to_string_pretty(&ra::tools::mcp::check(&config, &server).await?)?
        );
        return Ok(0);
    }
    let budget = Budget::new(&config);
    if plan_only {
        let draft = ra::setup::Draft::seed(&config);
        ensure!(!draft.goal.trim().is_empty(), "--plan-only needs a request");
        let api = ra::api::Api::new(&config, Reporter::Terminal, budget)?;
        let proposal =
            ra::setup::refine(&config, &api, &draft, "Propose the team for this request.").await?;
        if proposal
            .execution
            .as_ref()
            .is_some_and(|execution| execution.pending_ownership.is_some())
        {
            eprintln!(
                "Draft write paths overlap. The proposal carries them forward for read-only coordination."
            );
        }
        match plan_format.unwrap_or(PlanFormat::Json) {
            PlanFormat::Json => println!("{}", serde_json::to_string_pretty(&proposal)?),
            PlanFormat::Toml => print!("{}", config.export_campaign(&proposal)?),
        }
        return Ok(0);
    }
    let store = Store::open(&config.cwd)?;
    let resumed = if config.resume {
        let session = store.load_run(run_id.as_deref())?;
        if let Some(goal) = &explicit_goal {
            ensure!(
                *goal == session.goal || session.request.as_ref() == Some(goal),
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
    ensure!(
        !config.review_plan || interactive,
        "--review-plan requires a terminal"
    );
    let (generation, control_state) = tokio::sync::watch::channel(GenerationControl::default());
    let live_generations = LiveGenerations::default();
    let (guide, guidance) = mpsc::sync_channel(16);
    let listener = (!interactive)
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
                let mut session = Session::new(goal.clone());
                if !config.fixed_team && config.workers.is_empty() {
                    session.phase = Phase::ProposeTeam;
                }
                session
            }
        })
    };
    let active_run = session
        .as_ref()
        .map(|session| session.run_id().to_owned())
        .unwrap_or_default();
    if !interactive {
        if !config.resume {
            store.archive_old_checkpoint()?;
        }
        store.save(session.as_ref().expect("managed run has a session"))?;
    }
    let observer = if interactive {
        None
    } else {
        control::Listener::bind_telemetry(&config.cwd).ok()
    };
    let telemetry_state = generation.subscribe();
    let telemetry = async {
        if let Some(observer) = observer {
            let _ = control::serve_telemetry(
                observer,
                &active_run,
                dashboard_managed,
                telemetry_state,
                &live_generations,
                &budget,
            )
            .await;
        }
        std::future::pending::<()>().await;
    };
    let run = async {
        if interactive {
            return ui::run(&config, &store, session.take(), budget.clone()).await;
        }
        let session = session.take().expect("headless session initialized");
        let reporter = Reporter::Managed(control_state, live_generations.clone());
        let orchestrator = Orchestrator::new(&config, &store, reporter, budget.clone())?;
        let session = match orchestrator.run(session, Some(guidance)).await {
            Err(error) if error.is::<ra::events::CheckpointExit>() => {
                return Ok(ui::Outcome::Interrupted);
            }
            result => result?,
        };
        Ok(ui::Outcome::Finished(Box::new(session)))
    };
    let controls = async {
        if let Some(listener) = listener {
            control::serve(
                listener,
                &active_run,
                dashboard_managed,
                generation,
                guide,
                live_generations.clone(),
                &store,
                &budget,
            )
            .await
        } else {
            std::future::pending::<Result<()>>().await
        }
    };
    let outcome = tokio::select! {
        result = run => result,
        _ = telemetry => unreachable!("telemetry does not stop the runner"),
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

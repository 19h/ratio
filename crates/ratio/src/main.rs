use std::{
    io::{self, IsTerminal},
    process::ExitCode,
};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use ra::{
    budget::Budget,
    config::{Cli, Config},
    events::Reporter,
    orchestrator::Orchestrator,
    session::{Phase, Session, Store},
    ui,
};

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
    let explicit_goal = cli.goal.clone();
    let config = Config::load(cli)?;
    let store = Store::open(&config.cwd)?;
    let budget = Budget::new(&config);
    let resumed = if config.resume {
        let session = store.load()?;
        if let Some(goal) = &explicit_goal {
            ensure!(
                *goal == session.goal,
                "--resume requires the same goal as the checkpoint"
            );
        }
        Some(session)
    } else {
        None
    };
    let interactive = !config.headless && io::stdin().is_terminal() && io::stderr().is_terminal();
    let run = async {
        if interactive {
            return ui::run(&config, &store, resumed, budget.clone()).await;
        }
        let session = match resumed {
            Some(session) => session,
            None => {
                let goal = config
                    .goal
                    .as_ref()
                    .filter(|text| !text.trim().is_empty())
                    .context("supply --goal or set goal in the config file")?;
                Session::new(goal.clone())
            }
        };
        let reporter = if config.headless {
            Reporter::Silent
        } else {
            Reporter::Terminal
        };
        let orchestrator = Orchestrator::new(&config, &store, reporter, budget.clone())?;
        let session = orchestrator.run(session, None).await?;
        Ok(ui::Outcome::Finished(Box::new(session)))
    };
    let outcome = tokio::select! {
        result = run => result?,
        signal = tokio::signal::ctrl_c() => {
            signal.context("listen for interrupt")?;
            ui::Outcome::Interrupted
        }
    };
    if matches!(outcome, ui::Outcome::Interrupted) {
        budget.save()?;
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

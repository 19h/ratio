use std::{
    cell::RefCell,
    fs::File,
    io::Write,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{Journal, config::Config};

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct Ledger {
    pub(crate) requests: u64,
    pub(crate) tokens: u64,
    pub(crate) elapsed_ms: u64,
}

struct State {
    ledger: Ledger,
    path: Option<PathBuf>,
    journal: Option<(Rc<RefCell<Journal>>, String)>,
    since: Option<Instant>,
    exhausted: Option<&'static str>,
}

#[derive(Clone)]
pub struct Budget {
    state: Rc<RefCell<State>>,
    active: watch::Sender<bool>,
    requests: u64,
    tokens: u64,
    elapsed_ms: u64,
}

impl Budget {
    pub fn new(config: &Config) -> Self {
        let limits = &config.orchestration;
        Self {
            state: Rc::new(RefCell::new(State {
                ledger: Ledger::default(),
                path: None,
                journal: None,
                since: Some(Instant::now()),
                exhausted: None,
            })),
            active: watch::channel(true).0,
            requests: limits.max_requests,
            tokens: limits.max_total_tokens,
            elapsed_ms: limits.max_run_secs.saturating_mul(1000),
        }
    }

    pub(crate) fn bind(
        &self,
        path: PathBuf,
        resume: bool,
        journal: Rc<RefCell<Journal>>,
        run_id: &str,
    ) -> Result<()> {
        let mut state = self.state.borrow_mut();
        if resume {
            state.ledger = if let Some(bytes) = journal.borrow().budget(run_id)? {
                serde_json::from_slice(&bytes).context("decode journaled run budget ledger")?
            } else {
                serde_json::from_reader(File::open(&path).context("run budget ledger is missing")?)
                    .context("read format-2 run budget ledger")?
            };
            state.since = Some(Instant::now());
        }
        state.journal = Some((journal, run_id.into()));
        state.path = Some(path);
        state.save()
    }

    pub fn reserve(&self, tokens: u64) -> Result<()> {
        let mut state = self.state.borrow_mut();
        let limit = if state.elapsed() >= self.elapsed_ms {
            Some("active time")
        } else if state.ledger.requests >= self.requests {
            Some("provider attempts")
        } else if tokens > self.tokens.saturating_sub(state.ledger.tokens) {
            Some("metered tokens")
        } else {
            None
        };
        if let Some(limit) = limit {
            state.exhausted = Some(limit);
            anyhow::bail!("run budget exhausted: {limit}");
        }
        state.ledger.requests += 1;
        state.ledger.tokens += tokens;
        state.save()
    }

    pub fn reconcile(&self, reserved: u64, actual: u64) -> Result<()> {
        let mut state = self.state.borrow_mut();
        state.ledger.tokens = state
            .ledger
            .tokens
            .saturating_sub(reserved)
            .saturating_add(actual);
        state.save()
    }

    pub fn remaining(&self) -> Duration {
        Duration::from_millis(
            self.elapsed_ms
                .saturating_sub(self.state.borrow().elapsed()),
        )
    }

    pub(crate) fn pause(&self) -> Result<Pause<'_>> {
        ensure!(
            !self.remaining().is_zero(),
            "run budget exhausted: active time"
        );
        let mut state = self.state.borrow_mut();
        debug_assert!(
            state.since.is_some(),
            "pause requires a quiescent orchestrator"
        );
        state.save()?;
        state.since = None;
        self.active.send_replace(false);
        Ok(Pause(self))
    }

    pub(crate) async fn until_exhausted(&self) {
        let mut active = self.active.subscribe();
        loop {
            let running = *active.borrow_and_update();
            if running {
                let remaining = self.remaining();
                if remaining.is_zero() {
                    return;
                }
                tokio::select! {
                    _ = active.changed() => {},
                    _ = tokio::time::sleep(remaining) => {},
                }
            } else {
                let _ = active.changed().await;
            }
        }
    }

    pub fn exhaustion(&self) -> Option<&'static str> {
        self.state
            .borrow()
            .exhausted
            .or_else(|| self.remaining().is_zero().then_some("active time"))
    }

    pub fn save(&self) -> Result<()> {
        self.state.borrow_mut().save()
    }

    pub(crate) fn save_if_active(&self) -> Result<()> {
        let mut state = self.state.borrow_mut();
        if state.since.is_some() {
            state.save()?;
        }
        Ok(())
    }
}

pub(crate) struct Pause<'a>(&'a Budget);

impl Drop for Pause<'_> {
    fn drop(&mut self) {
        self.0.state.borrow_mut().since = Some(Instant::now());
        self.0.active.send_replace(true);
    }
}

impl State {
    fn elapsed(&self) -> u64 {
        self.ledger
            .elapsed_ms
            .saturating_add(self.since.map_or(0, |since| {
                u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
            }))
    }

    fn save(&mut self) -> Result<()> {
        self.ledger.elapsed_ms = self.elapsed();
        if self.since.is_some() {
            self.since = Some(Instant::now());
        }
        let bytes = serde_json::to_vec(&self.ledger)?;
        if let Some((journal, run_id)) = &self.journal {
            journal.borrow().save_budget(run_id, &bytes)?;
        }
        if let Some(path) = &self.path {
            let mut file =
                tempfile::NamedTempFile::new_in(path.parent().context("budget ledger directory")?)?;
            file.write_all(&bytes)?;
            file.flush()?;
            file.as_file().sync_all()?;
            file.persist(path).context("replace run budget ledger")?;
        }
        Ok(())
    }
}

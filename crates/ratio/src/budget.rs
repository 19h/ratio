use std::{
    cell::RefCell,
    fs::File,
    io::Write,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;

#[derive(Default, Deserialize, Serialize)]
struct Ledger {
    requests: u64,
    tokens: u64,
    elapsed_ms: u64,
}

struct State {
    ledger: Ledger,
    path: Option<PathBuf>,
    since: Instant,
    exhausted: Option<&'static str>,
}

#[derive(Clone)]
pub struct Budget {
    state: Rc<RefCell<State>>,
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
                since: Instant::now(),
                exhausted: None,
            })),
            requests: limits.max_requests,
            tokens: limits.max_total_tokens,
            elapsed_ms: limits.max_run_secs.saturating_mul(1000),
        }
    }

    pub fn bind(&self, path: PathBuf, resume: bool) -> Result<()> {
        let mut state = self.state.borrow_mut();
        if resume && path.exists() {
            state.ledger =
                serde_json::from_reader(File::open(&path)?).context("read run budget ledger")?;
            state.since = Instant::now();
        } else if resume {
            anyhow::bail!("run budget ledger is missing");
        }
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

    pub fn exhaustion(&self) -> Option<&'static str> {
        self.state
            .borrow()
            .exhausted
            .or_else(|| self.remaining().is_zero().then_some("active time"))
    }

    pub fn save(&self) -> Result<()> {
        self.state.borrow_mut().save()
    }
}

impl State {
    fn elapsed(&self) -> u64 {
        self.ledger
            .elapsed_ms
            .saturating_add(u64::try_from(self.since.elapsed().as_millis()).unwrap_or(u64::MAX))
    }

    fn save(&mut self) -> Result<()> {
        self.ledger.elapsed_ms = self.elapsed();
        self.since = Instant::now();
        if let Some(path) = &self.path {
            let mut file =
                tempfile::NamedTempFile::new_in(path.parent().context("budget ledger directory")?)?;
            serde_json::to_writer(&mut file, &self.ledger)?;
            file.flush()?;
            file.as_file().sync_all()?;
            file.persist(path).context("replace run budget ledger")?;
        }
        Ok(())
    }
}

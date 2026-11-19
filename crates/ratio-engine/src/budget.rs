use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    fs::File,
    io::Write,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use ratio_protocol::LiveTokenUsage;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::{Journal, config::Config};

const OUTPUT_RATE_WINDOW: Duration = Duration::from_secs(2);
const OUTPUT_RATE_BUCKET: Duration = Duration::from_millis(100);

#[derive(Default, Deserialize, Serialize)]
pub(crate) struct Ledger {
    pub(crate) requests: u64,
    pub(crate) tokens: u64,
    pub(crate) elapsed_ms: u64,
}

struct State {
    ledger: Ledger,
    token_progress: BTreeMap<u64, AttemptTokens>,
    output_rate: OutputRate,
    path: Option<PathBuf>,
    journal: Option<(Rc<RefCell<Journal>>, String)>,
    since: Option<Instant>,
    exhausted: Option<&'static str>,
}

struct AttemptTokens {
    reserved: u64,
    observed: u64,
    output: usize,
}

#[derive(Default)]
struct OutputRate {
    buckets: VecDeque<(Instant, u64)>,
}

impl OutputRate {
    fn record(&mut self, now: Instant, tokens: u64) {
        if tokens == 0 {
            return;
        }
        while self
            .buckets
            .front()
            .is_some_and(|(time, _)| now.duration_since(*time) >= OUTPUT_RATE_WINDOW)
        {
            self.buckets.pop_front();
        }
        if let Some((_, count)) = self
            .buckets
            .back_mut()
            .filter(|(time, _)| now.duration_since(*time) < OUTPUT_RATE_BUCKET)
        {
            *count = count.saturating_add(tokens);
        } else {
            self.buckets.push_back((now, tokens));
        }
    }

    fn per_second(&self, now: Instant) -> u64 {
        self.buckets
            .iter()
            .filter(|(time, _)| now.duration_since(*time) < OUTPUT_RATE_WINDOW)
            .fold(0u64, |total, (_, count)| total.saturating_add(*count))
            .div_ceil(OUTPUT_RATE_WINDOW.as_secs())
    }
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
                token_progress: BTreeMap::new(),
                output_rate: OutputRate::default(),
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

    pub(crate) fn reserve_completion(
        &self,
        prompt: usize,
        output: usize,
    ) -> Result<TokenProgress<'_>> {
        let reserved = prompt.saturating_add(output) as u64;
        self.reserve(reserved)?;
        let mut state = self.state.borrow_mut();
        let id = state.ledger.requests;
        state.token_progress.insert(
            id,
            AttemptTokens {
                reserved,
                observed: prompt as u64,
                output: 0,
            },
        );
        Ok(TokenProgress {
            budget: self,
            id,
            prompt,
        })
    }

    pub fn live_token_usage(&self) -> LiveTokenUsage {
        let state = self.state.borrow();
        let mut reserved = 0u64;
        let mut estimated = 0u64;
        for attempt in state.token_progress.values() {
            reserved = reserved.saturating_add(attempt.reserved);
            estimated = estimated.saturating_add(attempt.observed);
        }
        LiveTokenUsage {
            tokens: state
                .ledger
                .tokens
                .saturating_sub(reserved)
                .saturating_add(estimated),
            estimated: !state.token_progress.is_empty(),
            output_tokens_per_second: state.output_rate.per_second(Instant::now()),
        }
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

pub(crate) struct TokenProgress<'a> {
    budget: &'a Budget,
    id: u64,
    prompt: usize,
}

impl TokenProgress<'_> {
    pub(crate) fn observe(&self, reported_prompt: Option<usize>, output: usize) {
        let mut state = self.budget.state.borrow_mut();
        let Some(attempt) = state.token_progress.get_mut(&self.id) else {
            return;
        };
        attempt.observed = reported_prompt
            .unwrap_or(self.prompt)
            .saturating_add(output) as u64;
        let received = output.saturating_sub(attempt.output) as u64;
        attempt.output = output;
        state.output_rate.record(Instant::now(), received);
    }

    pub(crate) fn reconcile(&self, actual: u64) -> Result<()> {
        let attempt = self
            .budget
            .state
            .borrow_mut()
            .token_progress
            .remove(&self.id)
            .context("model attempt is already reconciled")?;
        self.budget.reconcile(attempt.reserved, actual)
    }
}

impl Drop for TokenProgress<'_> {
    fn drop(&mut self) {
        self.budget
            .state
            .borrow_mut()
            .token_progress
            .remove(&self.id);
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

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;

    #[test]
    fn output_rate_combines_recent_chunks_and_decays_to_zero() {
        let start = Instant::now();
        let mut rate = OutputRate::default();
        rate.record(start, 30);
        rate.record(start + Duration::from_millis(20), 10);
        rate.record(start + Duration::from_secs(1), 60);
        assert_eq!(rate.per_second(start + Duration::from_secs(1)), 50);
        assert_eq!(rate.per_second(start + Duration::from_secs(2)), 30);
        assert_eq!(rate.per_second(start + Duration::from_secs(3)), 0);
    }

    #[test]
    fn live_total_replaces_only_the_reservations_for_active_attempts() {
        let workspace = tempfile::tempdir().unwrap();
        let profile = workspace.path().join("ratio.toml");
        std::fs::write(&profile, "").unwrap();
        let cli = crate::config::Cli::parse_from([
            "ra",
            "--config",
            profile.to_str().unwrap(),
            "--model",
            "test",
            "--context-window",
            "8192",
        ]);
        let budget = Budget::new(&Config::load(cli).unwrap());
        budget.reserve(100).unwrap();

        let first = budget.reserve_completion(30, 40).unwrap();
        let second = budget.reserve_completion(20, 60).unwrap();
        assert_eq!(
            budget.live_token_usage(),
            LiveTokenUsage {
                tokens: 150,
                estimated: true,
                output_tokens_per_second: 0,
            }
        );
        first.observe(None, 5);
        second.observe(Some(15), 2);
        assert_eq!(budget.live_token_usage().tokens, 152);
        assert_eq!(budget.state.borrow().ledger.tokens, 250);

        first.reconcile(32).unwrap();
        assert_eq!(budget.live_token_usage().tokens, 149);
        drop(first);
        drop(second);
        let usage = budget.live_token_usage();
        assert_eq!(usage.tokens, 212);
        assert!(!usage.estimated);
    }
}

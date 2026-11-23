use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    fs::File,
    future::pending,
    io::Write,
    num::NonZeroU64,
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, ensure};
use ratio_protocol::{LiveTokenUsage, TokenBreakdown, TokenCount};
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
    #[serde(default)]
    token_breakdown: TokenBreakdown,
    #[serde(default)]
    breakdown_attempts: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allowance: Option<Allowance>,
}

#[derive(Deserialize, Serialize)]
struct Allowance {
    cycle: usize,
    requests: u64,
    tokens: u64,
    elapsed_ms: u64,
}

impl Ledger {
    pub(crate) fn token_breakdown(&self, active: u64) -> TokenBreakdown {
        let mut breakdown = self.token_breakdown;
        if self.requests > self.breakdown_attempts.saturating_add(active) {
            breakdown.input.partial = true;
            breakdown.cached_input.partial = true;
            breakdown.output.partial = true;
        }
        breakdown
    }
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
    input_estimate: u64,
    streamed: u64,
    reported: TokenValues,
}

#[derive(Clone, Copy, Default)]
pub(crate) struct TokenValues {
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    pub output: Option<u64>,
}

impl AttemptTokens {
    fn update_reported(&mut self, values: TokenValues) {
        self.reported.input = values.input.or(self.reported.input);
        self.reported.cached_input = values.cached_input.or(self.reported.cached_input);
        self.reported.output = values.output.or(self.reported.output);
    }

    fn breakdown(&self, output_estimate: u64) -> TokenBreakdown {
        let count = |reported: Option<u64>, estimated: Option<u64>| TokenCount {
            tokens: reported.or(estimated),
            estimated: reported.is_none() && estimated.is_some(),
            partial: reported.is_none() && estimated.is_none(),
        };
        TokenBreakdown {
            input: count(self.reported.input, Some(self.input_estimate)),
            cached_input: count(self.reported.cached_input, None),
            output: count(
                self.reported.output,
                Some(self.streamed.max(output_estimate)),
            ),
        }
    }
}

fn add_breakdown(total: &mut TokenBreakdown, counts: TokenBreakdown) {
    for (total, count) in [
        (&mut total.input, counts.input),
        (&mut total.cached_input, counts.cached_input),
        (&mut total.output, counts.output),
    ] {
        if let Some(tokens) = count.tokens {
            total.tokens = Some(total.tokens.unwrap_or_default().saturating_add(tokens));
        }
        total.estimated |= count.estimated;
        total.partial |= count.partial;
    }
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
    requests: Option<NonZeroU64>,
    tokens: Option<NonZeroU64>,
    elapsed_ms: Option<u64>,
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
            requests: NonZeroU64::new(limits.max_requests),
            tokens: NonZeroU64::new(limits.max_total_tokens),
            elapsed_ms: NonZeroU64::new(limits.max_run_secs)
                .map(|seconds| seconds.get().saturating_mul(1000)),
        }
    }

    pub(crate) fn bind(
        &self,
        path: PathBuf,
        resume: bool,
        journal: Rc<RefCell<Journal>>,
        run_id: &str,
        continuation_cycle: Option<usize>,
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
        if let Some(cycle) = continuation_cycle
            && state
                .ledger
                .allowance
                .as_ref()
                .is_none_or(|allowance| allowance.cycle != cycle)
        {
            state.ledger.allowance = Some(Allowance {
                cycle,
                requests: state.ledger.requests,
                tokens: state.ledger.tokens,
                elapsed_ms: state.ledger.elapsed_ms,
            });
        }
        state.journal = Some((journal, run_id.into()));
        state.path = Some(path);
        state.save()
    }

    pub(crate) fn ensure_available(&self, tokens: u64) -> Result<()> {
        let mut state = self.state.borrow_mut();
        let base = state.ledger.allowance.as_ref();
        let used_requests = state
            .ledger
            .requests
            .saturating_sub(base.map_or(0, |base| base.requests));
        let used_tokens = state
            .ledger
            .tokens
            .saturating_sub(base.map_or(0, |base| base.tokens));
        let limit = if self
            .elapsed_ms
            .is_some_and(|limit| state.allowance_elapsed() >= limit)
        {
            Some("active time")
        } else if self
            .requests
            .is_some_and(|limit| used_requests >= limit.get())
        {
            Some("provider attempts")
        } else if self
            .tokens
            .is_some_and(|limit| tokens > limit.get().saturating_sub(used_tokens))
        {
            Some("metered tokens")
        } else {
            None
        };
        if let Some(limit) = limit {
            state.exhausted = Some(limit);
            anyhow::bail!("run budget exhausted: {limit}");
        }
        Ok(())
    }

    pub fn reserve(&self, tokens: u64) -> Result<()> {
        self.ensure_available(tokens)?;
        let mut state = self.state.borrow_mut();
        state.ledger.requests = state.ledger.requests.saturating_add(1);
        state.ledger.tokens = state.ledger.tokens.saturating_add(tokens);
        state.save()
    }

    pub fn reconcile(&self, reserved: u64, actual: u64) -> Result<()> {
        let mut state = self.state.borrow_mut();
        state.charge_actual(reserved, actual);
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
                input_estimate: prompt as u64,
                streamed: 0,
                reported: TokenValues::default(),
            },
        );
        Ok(TokenProgress { budget: self, id })
    }

    pub fn live_token_usage(&self) -> LiveTokenUsage {
        let state = self.state.borrow();
        let mut reserved = 0u64;
        let mut estimated = 0u64;
        let mut breakdown = state
            .ledger
            .token_breakdown(state.token_progress.len() as u64);
        for attempt in state.token_progress.values() {
            reserved = reserved.saturating_add(attempt.reserved);
            let counts = attempt.breakdown(0);
            estimated = estimated
                .saturating_add(counts.input.tokens.unwrap_or_default())
                .saturating_add(counts.output.tokens.unwrap_or_default());
            add_breakdown(&mut breakdown, counts);
        }
        LiveTokenUsage {
            tokens: state
                .ledger
                .tokens
                .saturating_sub(reserved)
                .saturating_add(estimated),
            estimated: !state.token_progress.is_empty(),
            breakdown,
            output_tokens_per_second: state.output_rate.per_second(Instant::now()),
        }
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.elapsed_ms.map(|limit| {
            Duration::from_millis(limit.saturating_sub(self.state.borrow().allowance_elapsed()))
        })
    }

    pub(crate) fn pause(&self) -> Result<Pause<'_>> {
        ensure!(
            self.remaining()
                .is_none_or(|remaining| !remaining.is_zero()),
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
        while let Some(remaining) = self.remaining() {
            if *active.borrow_and_update() {
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
        pending().await
    }

    pub fn exhaustion(&self) -> Option<&'static str> {
        self.state.borrow().exhausted.or_else(|| {
            self.remaining()
                .filter(Duration::is_zero)
                .map(|_| "active time")
        })
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
}

impl TokenProgress<'_> {
    pub(crate) fn observe(&self, reported: TokenValues, output: usize) {
        let mut state = self.budget.state.borrow_mut();
        let Some(attempt) = state.token_progress.get_mut(&self.id) else {
            return;
        };
        attempt.update_reported(reported);
        let output = output as u64;
        let received = output.saturating_sub(attempt.streamed);
        attempt.streamed = output;
        state.output_rate.record(Instant::now(), received);
    }

    pub(crate) fn finish(
        &self,
        reported: TokenValues,
        actual: Option<u64>,
        output_estimate: u64,
    ) -> Result<()> {
        let mut state = self.budget.state.borrow_mut();
        let mut attempt = state
            .token_progress
            .remove(&self.id)
            .context("model attempt is already reconciled")?;
        attempt.update_reported(reported);
        add_breakdown(
            &mut state.ledger.token_breakdown,
            attempt.breakdown(output_estimate),
        );
        state.ledger.breakdown_attempts += 1;
        if let Some(actual) = actual {
            state.charge_actual(attempt.reserved, actual);
        }
        state.save()
    }

    pub(crate) fn reject(&self) -> Result<()> {
        self.finish(
            TokenValues {
                input: Some(0),
                cached_input: Some(0),
                output: Some(0),
            },
            Some(0),
            0,
        )
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
    fn charge_actual(&mut self, reserved: u64, actual: u64) {
        self.ledger.tokens = self
            .ledger
            .tokens
            .saturating_sub(reserved)
            .saturating_add(actual);
    }

    fn allowance_elapsed(&self) -> u64 {
        self.elapsed().saturating_sub(
            self.ledger
                .allowance
                .as_ref()
                .map_or(0, |base| base.elapsed_ms),
        )
    }

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
        let usage = budget.live_token_usage();
        assert_eq!(usage.tokens, 150);
        assert_eq!(usage.breakdown.input.tokens, Some(50));
        assert!(usage.breakdown.input.estimated && usage.breakdown.input.partial);
        assert_eq!(usage.breakdown.cached_input.tokens, None);
        first.observe(TokenValues::default(), 5);
        second.observe(
            TokenValues {
                input: Some(15),
                cached_input: Some(8),
                output: None,
            },
            2,
        );
        let usage = budget.live_token_usage();
        assert_eq!(usage.tokens, 152);
        assert_eq!(usage.breakdown.output.tokens, Some(7));
        assert_eq!(usage.breakdown.cached_input.tokens, Some(8));
        assert!(usage.breakdown.cached_input.partial);
        assert_eq!(budget.state.borrow().ledger.tokens, 250);

        first
            .finish(
                TokenValues {
                    input: Some(28),
                    cached_input: Some(2),
                    output: Some(4),
                },
                Some(32),
                0,
            )
            .unwrap();
        assert_eq!(budget.live_token_usage().tokens, 149);
        drop(first);
        drop(second);
        let usage = budget.live_token_usage();
        assert_eq!(usage.tokens, 212);
        assert!(!usage.estimated);
        assert_eq!(usage.breakdown.input.tokens, Some(28));
        assert_eq!(usage.breakdown.cached_input.tokens, Some(2));
        assert_eq!(usage.breakdown.output.tokens, Some(4));
        assert!(usage.breakdown.output.partial);
    }
}

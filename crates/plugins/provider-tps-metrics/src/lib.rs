#![warn(missing_docs)]
//! Provider-model throughput and prompt-cache metrics for Respan Gateway.
//!
//! The plugin observes conversation calls at the provider edge. It reports
//! effective gateway-observed throughput, not the provider's internal model
//! throughput. Each provider call is aggregated when its end hook runs, so the
//! response path does not contend on the global metrics map for every event.

use parking_lot::Mutex;
use respan_gateway_conversation::{Input, Output, Usage};
use respan_gateway_core::{
    FaultKind, Filter, Plugin, ProviderContext, Result, StreamingResponse, async_trait,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A provider-model metric snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricSnapshot {
    /// The provider name seen by the gateway.
    pub provider: String,
    /// The model sent in the conversation input.
    pub model: String,
    /// Every provider call observed, including failed and abandoned calls.
    pub calls: u64,
    /// Calls that completed successfully.
    pub successful_calls: u64,
    /// Calls that ended with an error or were abandoned.
    pub failed_calls: u64,
    /// Calls abandoned because no consumer waited for them, such as a lost
    /// race branch or a disconnected client. This is included in
    /// `failed_calls`.
    pub abandoned_calls: u64,
    /// Successful calls that reported a non-empty usage value.
    pub usage_calls: u64,
    /// All input tokens reported by successful calls.
    pub input_tokens: u64,
    /// Non-cached input tokens for calls with known cache counts.
    pub processed_input_tokens: Option<u64>,
    /// Average input tokens per successful call that reported input usage.
    pub average_context_tokens: Option<f64>,
    /// Input tokens reported as read from cache for calls with known counts.
    pub cached_input_tokens: Option<u64>,
    /// All output tokens reported by successful calls.
    pub output_tokens: u64,
    /// Effective input tokens per second, or `None` without a usable timing
    /// sample.
    pub input_tps: Option<f64>,
    /// Output tokens per second, or `None` without a usable timing sample.
    pub output_tps: Option<f64>,
    /// Token-weighted cached input ratio for calls with known cache counts.
    pub cache_hit_rate: Option<f64>,
    /// Input-bearing successful calls with known cache counts.
    pub cache_known_calls: u64,
    /// Input-bearing successful calls considered for cache coverage.
    pub cache_total_calls: u64,
    /// Successful calls with input usage and a first output delta.
    pub input_timed_calls: u64,
    /// Successful calls with output usage and a non-zero output window.
    pub output_timed_calls: u64,
}

/// Shared provider-model metrics collected by [`ProviderMetrics`].
#[derive(Debug, Default)]
pub struct Metrics {
    stats: Mutex<HashMap<(String, String), Aggregate>>,
}

impl Metrics {
    /// Returns a stable, sorted snapshot of all provider-model metrics.
    pub fn snapshot(&self) -> Vec<MetricSnapshot> {
        let stats = self.stats.lock();
        let mut snapshots: Vec<_> = stats
            .iter()
            .map(|((provider, model), aggregate)| aggregate.snapshot(provider, model))
            .collect();
        snapshots.sort_unstable_by(|a, b| {
            a.provider
                .cmp(&b.provider)
                .then_with(|| a.model.cmp(&b.model))
        });
        snapshots
    }

    #[cfg(test)]
    fn record(&self, provider: String, model: String, sample: CallSample, success: bool) {
        self.record_outcome(provider, model, sample, success, false);
    }

    fn record_outcome(
        &self,
        provider: String,
        model: String,
        sample: CallSample,
        success: bool,
        abandoned: bool,
    ) {
        let mut stats = self.stats.lock();
        let aggregate = stats.entry((provider, model)).or_default();
        aggregate.calls = aggregate.calls.saturating_add(1);
        if !success {
            aggregate.failed_calls = aggregate.failed_calls.saturating_add(1);
            if abandoned {
                aggregate.abandoned_calls = aggregate.abandoned_calls.saturating_add(1);
            }
            return;
        }

        aggregate.successful_calls = aggregate.successful_calls.saturating_add(1);
        if !sample.usage.is_empty() {
            aggregate.usage_calls = aggregate.usage_calls.saturating_add(1);
        }
        if sample.usage.input_tokens > 0 {
            aggregate.context_calls = aggregate.context_calls.saturating_add(1);
        }
        aggregate.input_tokens = aggregate
            .input_tokens
            .saturating_add(sample.usage.input_tokens);
        aggregate.output_tokens = aggregate
            .output_tokens
            .saturating_add(sample.usage.output_tokens);

        if sample.usage.input_tokens > 0 {
            aggregate.cache_total_calls = aggregate.cache_total_calls.saturating_add(1);
            if sample.cache_known {
                aggregate.cache_known_calls = aggregate.cache_known_calls.saturating_add(1);
                aggregate.cache_input_tokens = aggregate
                    .cache_input_tokens
                    .saturating_add(sample.usage.input_tokens);
                aggregate.cached_input_tokens = aggregate.cached_input_tokens.saturating_add(
                    sample
                        .usage
                        .cached_input_tokens
                        .min(sample.usage.input_tokens),
                );
            }
        }

        if sample.streaming.is_streaming()
            && sample.usage.input_tokens > 0
            && let Some(first) = sample.first_delta_at
            && let Some(window) = first.checked_duration_since(sample.started_at)
            && window > Duration::ZERO
        {
            aggregate.input_timed_tokens = aggregate
                .input_timed_tokens
                .saturating_add(sample.usage.input_tokens);
            aggregate.input_seconds += window.as_secs_f64();
            aggregate.input_timed_calls = aggregate.input_timed_calls.saturating_add(1);
        }

        if sample.streaming.is_streaming()
            && sample.usage.output_tokens > 0
            && let Some(first) = sample.first_delta_at
            && let Some(last) = sample.last_delta_at
            && let Some(window) = last.checked_duration_since(first)
            && window > Duration::ZERO
        {
            aggregate.output_timed_tokens = aggregate
                .output_timed_tokens
                .saturating_add(sample.usage.output_tokens);
            aggregate.output_seconds += window.as_secs_f64();
            aggregate.output_timed_calls = aggregate.output_timed_calls.saturating_add(1);
        }
    }
}

#[derive(Debug, Default)]
struct Aggregate {
    calls: u64,
    successful_calls: u64,
    failed_calls: u64,
    abandoned_calls: u64,
    usage_calls: u64,
    context_calls: u64,
    input_tokens: u64,
    cached_input_tokens: u64,
    cache_input_tokens: u64,
    cache_known_calls: u64,
    cache_total_calls: u64,
    output_tokens: u64,
    output_timed_tokens: u64,
    input_timed_tokens: u64,
    input_seconds: f64,
    output_seconds: f64,
    input_timed_calls: u64,
    output_timed_calls: u64,
}

impl Aggregate {
    fn snapshot(&self, provider: &str, model: &str) -> MetricSnapshot {
        MetricSnapshot {
            provider: provider.to_owned(),
            model: model.to_owned(),
            calls: self.calls,
            successful_calls: self.successful_calls,
            failed_calls: self.failed_calls,
            abandoned_calls: self.abandoned_calls,
            usage_calls: self.usage_calls,
            input_tokens: self.input_tokens,
            processed_input_tokens: (self.cache_known_calls > 0).then_some(
                self.cache_input_tokens
                    .saturating_sub(self.cached_input_tokens),
            ),
            average_context_tokens: average(self.input_tokens, self.context_calls),
            cached_input_tokens: (self.cache_known_calls > 0).then_some(self.cached_input_tokens),
            output_tokens: self.output_tokens,
            input_tps: rate(self.input_timed_tokens, self.input_seconds),
            output_tps: rate(self.output_timed_tokens, self.output_seconds),
            cache_hit_rate: ratio(self.cached_input_tokens, self.cache_input_tokens),
            cache_known_calls: self.cache_known_calls,
            cache_total_calls: self.cache_total_calls,
            input_timed_calls: self.input_timed_calls,
            output_timed_calls: self.output_timed_calls,
        }
    }
}

fn rate(tokens: u64, seconds: f64) -> Option<f64> {
    (seconds > 0.0).then(|| tokens as f64 / seconds)
}

fn ratio(numerator: u64, denominator: u64) -> Option<f64> {
    (denominator > 0).then(|| numerator as f64 / denominator as f64)
}

fn average(total: u64, samples: u64) -> Option<f64> {
    (samples > 0).then(|| total as f64 / samples as f64)
}

#[derive(Debug)]
struct CallSample {
    started_at: Instant,
    first_delta_at: Option<Instant>,
    last_delta_at: Option<Instant>,
    usage: Usage,
    cache_known: bool,
    streaming: StreamingResponse,
}

impl CallSample {
    fn new() -> Self {
        Self {
            started_at: Instant::now(),
            first_delta_at: None,
            last_delta_at: None,
            usage: Usage::default(),
            cache_known: true,
            streaming: StreamingResponse::default(),
        }
    }
}

#[derive(Debug)]
struct CallState(Mutex<CallSample>);

#[derive(Debug)]
struct Observe(Arc<CallState>);

impl Filter<Output> for Observe {
    fn edit(&mut self, event: Output, out: &mut Vec<Output>) -> Result<()> {
        let now = Instant::now();
        let mut sample = self.0.0.lock();
        match &event {
            Output::Delta { .. } => {
                sample.first_delta_at.get_or_insert(now);
                sample.last_delta_at = Some(now);
            }
            Output::Usage(usage) => {
                if usage.input_tokens > 0
                    && !usage.cached_input_tokens_reported
                    && usage.cached_input_tokens == 0
                {
                    sample.cache_known = false;
                }
                sample.usage += usage;
            }
            _ => {}
        }
        drop(sample);
        out.push(event);
        Ok(())
    }
}

/// A provider-edge plugin that collects conversation input/output TPS and
/// cache hit metrics by provider and model.
#[derive(Debug)]
pub struct ProviderMetrics {
    metrics: Arc<Metrics>,
    providers: HashMap<String, String>,
}

impl ProviderMetrics {
    /// Creates an empty metrics collector.
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(Metrics::default()),
            providers: HashMap::new(),
        }
    }

    /// Returns the shared metrics store used by this plugin.
    pub fn metrics(&self) -> Arc<Metrics> {
        self.metrics.clone()
    }

    /// Maps an internal gateway provider name to the public provider name in
    /// snapshots. Unmapped providers keep their internal name.
    #[must_use]
    pub fn provider_names(mut self, providers: HashMap<String, String>) -> Self {
        self.providers = providers;
        self
    }
}

impl Default for ProviderMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Plugin for ProviderMetrics {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<()> {
        if !px.is_kind::<Input>() {
            return Ok(());
        }

        let state = Arc::new(CallState(Mutex::new(CallSample::new())));
        px.extensions_mut().insert(state.0.lock().streaming.clone());
        px.extensions_mut().insert(state.clone());
        px.filter(Observe(state))
    }

    async fn on_provider_end(&self, px: &ProviderContext, end: &Result<()>) {
        let Some(state) = px.extensions().get::<Arc<CallState>>().cloned() else {
            return;
        };
        let sample = {
            let sample = state.0.lock();
            CallSample {
                started_at: sample.started_at,
                first_delta_at: sample.first_delta_at,
                last_delta_at: sample.last_delta_at,
                usage: sample.usage,
                cache_known: sample.cache_known,
                streaming: sample.streaming.clone(),
            }
        };
        let provider = self
            .providers
            .get(px.provider())
            .cloned()
            .unwrap_or_else(|| px.provider().to_owned());
        let model = px
            .input::<Input>()
            .ok()
            .and_then(Input::model)
            .unwrap_or("<unknown>")
            .to_owned();
        let abandoned = end
            .as_ref()
            .err()
            .is_some_and(|fault| fault.is(FaultKind::Abandoned));
        self.metrics
            .record_outcome(provider, model, sample, end.is_ok(), abandoned);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        input: u64,
        cached: u64,
        output: u64,
        input_seconds: f64,
        output_seconds: f64,
    ) -> CallSample {
        let now = Instant::now();
        CallSample {
            started_at: now,
            first_delta_at: Some(now + Duration::from_secs_f64(input_seconds)),
            last_delta_at: Some(now + Duration::from_secs_f64(input_seconds + output_seconds)),
            usage: Usage {
                input_tokens: input,
                cached_input_tokens: cached,
                cached_input_tokens_reported: cached > 0,
                output_tokens: output,
                ..Usage::default()
            },
            cache_known: cached > 0 || input == 0,
            streaming: {
                let shape = StreamingResponse::default();
                shape.mark_streaming();
                shape
            },
        }
    }

    #[test]
    fn observe_accumulates_usage_and_passes_events_through() {
        let state = Arc::new(CallState(Mutex::new(CallSample::new())));
        let mut filter = Observe(state.clone());
        let events = vec![
            Output::text(0, "hello"),
            Output::usage(Usage::new(10, 2)),
            Output::usage(Usage {
                input_tokens: 5,
                output_tokens: 3,
                cached_input_tokens: 4,
                ..Usage::default()
            }),
        ];
        let mut out = Vec::new();
        for event in events.clone() {
            filter.edit(event, &mut out).expect("observation succeeds");
        }

        let sample = state.0.lock();
        assert_eq!(out, events);
        assert_eq!(sample.usage.input_tokens, 15);
        assert_eq!(sample.usage.output_tokens, 5);
        assert_eq!(sample.usage.cached_input_tokens, 4);
        assert!(sample.first_delta_at.is_some());
        assert_eq!(sample.first_delta_at, sample.last_delta_at);
    }

    #[test]
    fn aggregates_rates_and_cache_hit_rate() {
        let metrics = Metrics::default();
        metrics.record(
            "openai".into(),
            "gpt-4o".into(),
            sample(100, 25, 40, 2.0, 4.0),
            true,
        );
        let snapshot = &metrics.snapshot()[0];
        assert_eq!(snapshot.processed_input_tokens, Some(75));
        assert_eq!(snapshot.cached_input_tokens, Some(25));
        assert_eq!(snapshot.input_tokens, 100);
        assert_eq!(snapshot.output_tokens, 40);
        assert_eq!(snapshot.input_tps, Some(50.0));
        assert_eq!(snapshot.output_tps, Some(10.0));
        assert_eq!(snapshot.cache_hit_rate, Some(0.25));
        assert_eq!(snapshot.average_context_tokens, Some(100.0));
        assert_eq!(snapshot.cache_known_calls, 1);
        assert_eq!(snapshot.cache_total_calls, 1);
    }

    #[test]
    fn known_zero_cache_counts_are_included_in_the_rate() {
        let metrics = Metrics::default();
        let mut sample = sample(100, 0, 20, 1.0, 2.0);
        sample.cache_known = true;
        sample.usage.cached_input_tokens_reported = true;
        metrics.record("p".into(), "m".into(), sample, true);
        let snapshot = &metrics.snapshot()[0];
        assert_eq!(snapshot.cached_input_tokens, Some(0));
        assert_eq!(snapshot.processed_input_tokens, Some(100));
        assert_eq!(snapshot.cache_hit_rate, Some(0.0));
        assert_eq!(snapshot.cache_known_calls, 1);
        assert_eq!(snapshot.cache_total_calls, 1);
    }

    #[test]
    fn usage_events_are_summed_and_cache_counts_are_bounded() {
        let metrics = Metrics::default();
        let mut sample = sample(0, 0, 0, 1.0, 1.0);
        sample.usage = Usage {
            input_tokens: 10,
            cached_input_tokens: 20,
            output_tokens: 3,
            ..Usage::default()
        };
        metrics.record("p".into(), "m".into(), sample, true);
        let snapshot = &metrics.snapshot()[0];
        assert_eq!(snapshot.cached_input_tokens, Some(10));
        assert_eq!(snapshot.processed_input_tokens, Some(0));
        assert_eq!(snapshot.cache_hit_rate, Some(1.0));
    }

    #[test]
    fn unknown_cache_counts_do_not_become_zero_hits_or_input_rate() {
        let metrics = Metrics::default();
        metrics.record("p".into(), "m".into(), sample(100, 0, 20, 1.0, 2.0), true);
        let snapshot = &metrics.snapshot()[0];
        assert_eq!(snapshot.cached_input_tokens, None);
        assert_eq!(snapshot.processed_input_tokens, None);
        assert_eq!(snapshot.cache_hit_rate, None);
        assert_eq!(snapshot.input_tps, Some(100.0));
        assert_eq!(snapshot.output_tps, Some(10.0));
        assert_eq!(snapshot.average_context_tokens, Some(100.0));
    }

    #[test]
    fn untimed_successes_do_not_inflate_timed_rates() {
        let metrics = Metrics::default();
        metrics.record("p".into(), "m".into(), sample(100, 25, 40, 2.0, 4.0), true);
        metrics.record(
            "p".into(),
            "m".into(),
            sample(1_000, 250, 400, 0.0, 0.0),
            true,
        );
        let snapshot = &metrics.snapshot()[0];
        assert_eq!(snapshot.input_tps, Some(50.0));
        assert_eq!(snapshot.output_tps, Some(10.0));
        assert_eq!(snapshot.input_timed_calls, 1);
        assert_eq!(snapshot.output_timed_calls, 1);
    }

    #[test]
    fn whole_response_deltas_do_not_create_timing_samples() {
        let metrics = Metrics::default();
        let mut whole = sample(100, 25, 40, 2.0, 4.0);
        whole.streaming = StreamingResponse::default();
        metrics.record("p".into(), "m".into(), whole, true);
        let snapshot = &metrics.snapshot()[0];
        assert_eq!(snapshot.input_tokens, 100);
        assert_eq!(snapshot.output_tokens, 40);
        assert_eq!(snapshot.input_tps, None);
        assert_eq!(snapshot.output_tps, None);
        assert_eq!(snapshot.input_timed_calls, 0);
        assert_eq!(snapshot.output_timed_calls, 0);
    }

    #[test]
    fn failed_calls_are_counted_but_not_in_rates() {
        let metrics = Metrics::default();
        metrics.record("p".into(), "m".into(), sample(10, 0, 5, 1.0, 1.0), false);
        let snapshot = &metrics.snapshot()[0];
        assert_eq!(snapshot.calls, 1);
        assert_eq!(snapshot.successful_calls, 0);
        assert_eq!(snapshot.failed_calls, 1);
        assert_eq!(snapshot.abandoned_calls, 0);
        assert_eq!(snapshot.input_tokens, 0);
        assert_eq!(snapshot.input_tps, None);
    }

    #[test]
    fn abandoned_calls_are_separated_from_failures() {
        let metrics = Metrics::default();
        metrics.record_outcome(
            "p".into(),
            "m".into(),
            sample(10, 5, 5, 1.0, 1.0),
            false,
            true,
        );
        let snapshot = &metrics.snapshot()[0];
        assert_eq!(snapshot.failed_calls, 1);
        assert_eq!(snapshot.abandoned_calls, 1);
        assert_eq!(snapshot.successful_calls, 0);
    }

    #[test]
    fn snapshots_are_sorted_by_provider_and_model() {
        let metrics = Metrics::default();
        metrics.record("z".into(), "a".into(), CallSample::new(), true);
        metrics.record("a".into(), "z".into(), CallSample::new(), true);
        metrics.record("a".into(), "a".into(), CallSample::new(), true);
        let keys: Vec<_> = metrics
            .snapshot()
            .into_iter()
            .map(|s| (s.provider, s.model))
            .collect();
        assert_eq!(
            keys,
            vec![
                ("a".into(), "a".into()),
                ("a".into(), "z".into()),
                ("z".into(), "a".into())
            ]
        );
    }
}

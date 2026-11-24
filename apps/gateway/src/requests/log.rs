//! The log itself: the kept requests, their totals, and the line each
//! prints.

use parking_lot::{Mutex, MutexGuard};
use std::collections::{HashMap, VecDeque};
use std::io::IsTerminal;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

use axum::extract::Request;
use respan_gateway_core::Extensions;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio::sync::broadcast;

use super::KEPT;
use super::record::{RequestRecord, clip, rfc3339};
use crate::config::Config;
use crate::history::Filter;
use crate::logging::{Level, LogFormat, escape_controls, paint};

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Totals {
    pub requests: u64,
    pub errors: u64,
    pub in_flight: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The sum of the requests' `cost_usd`; present once one has a cost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

/// A provider's finished requests, with the tokens they moved and the
/// windows they moved them in. A rate keeps only its own samples: tokens
/// with a window of their own, so a whole-body answer or an untimed call
/// cannot dilute it.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderStats {
    pub requests: u64,
    pub errors: u64,
    total_ms: u64,
    /// Tokens successful calls reported.
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Of `input_tokens`, what the providers said was read from a cache.
    pub cached_input_tokens: u64,
    /// Streaming samples' tokens and first-token windows, in milliseconds.
    input_timed_tokens: u64,
    input_timed_ms: u64,
    input_timed_calls: u64,
    /// Streaming samples' tokens and output windows, after the first token.
    output_timed_tokens: u64,
    output_timed_ms: u64,
    output_timed_calls: u64,
}

impl ProviderStats {
    pub fn avg_latency_ms(&self) -> Option<u64> {
        (self.requests > 0).then(|| self.total_ms / self.requests)
    }

    /// Input tokens per second of each call's wait for its first token, or
    /// `None` without a streaming sample. A gateway observation: it counts
    /// encoding, network, queueing and cache work, not the provider's own
    /// prefill speed.
    pub fn avg_input_tps(&self) -> Option<f64> {
        self.rate(self.input_timed_tokens, self.input_timed_ms)
    }

    /// Output tokens per second after the first token, or `None` without a
    /// sample past it.
    pub fn avg_output_tps(&self) -> Option<f64> {
        self.rate(self.output_timed_tokens, self.output_timed_ms)
    }

    /// Of successful calls' input tokens, the share read from a cache, or
    /// `None` without tokens. A provider that does not report cache counts
    /// reads as a zero here; its calls belong on a provider that does, not
    /// in a cross-provider comparison.
    pub fn cache_hit_rate(&self) -> Option<f64> {
        (self.input_tokens > 0).then(|| self.cached_input_tokens as f64 / self.input_tokens as f64)
    }

    fn rate(&self, tokens: u64, ms: u64) -> Option<f64> {
        (ms > 0).then(|| tokens as f64 / (ms as f64 / 1_000.0))
    }

    /// Adds a finished request. A failed one counts toward `requests` and
    /// `errors` and nothing else; a successful one's tokens count toward
    /// the sums, and a streaming one's toward the timed rates.
    pub(super) fn absorb(&mut self, record: &RequestRecord, failed: bool) {
        self.requests += 1;
        self.total_ms += record.duration_ms.unwrap_or_default();
        if failed {
            self.errors += 1;
            return;
        }
        let Some(usage) = record.usage else {
            return;
        };
        self.input_tokens = self.input_tokens.saturating_add(usage.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(usage.output_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(usage.cached_input_tokens.min(usage.input_tokens));
        // A stream's first token came at `ttfb_ms`; a whole-body answer has
        // no honest window, and a session's `101` opens one, not ends one.
        let (Some(duration_ms), Some(ttfb_ms)) = (record.duration_ms, record.ttfb_ms) else {
            return;
        };
        if !record.stream || record.status == Some(101) || ttfb_ms == 0 || ttfb_ms >= duration_ms {
            return;
        }
        if usage.input_tokens > 0 {
            self.input_timed_tokens = self.input_timed_tokens.saturating_add(usage.input_tokens);
            self.input_timed_ms += ttfb_ms;
            self.input_timed_calls += 1;
        }
        if usage.output_tokens > 0 {
            self.output_timed_tokens = self.output_timed_tokens.saturating_add(usage.output_tokens);
            self.output_timed_ms += duration_ms - ttfb_ms;
            self.output_timed_calls += 1;
        }
    }
}

/// A request in the log, shared by the edge and the plugin.
#[derive(Debug)]
pub struct Entry {
    record: Mutex<RequestRecord>,
    started: Instant,
    finished: AtomicBool,
    /// Set by the plugin once it sees the request: its end hook is then
    /// heard, which ends a session (its `101` is only its start).
    pub(super) recorded: AtomicBool,
    /// Set when the plugin's end hook ran.
    pub(super) ended: AtomicBool,
}

impl Entry {
    pub(super) fn record(&self) -> MutexGuard<'_, RequestRecord> {
        self.record.lock()
    }

    pub fn snapshot(&self) -> RequestRecord {
        self.record().clone()
    }

    pub(super) fn elapsed_ms(&self) -> u64 {
        self.started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX)
    }
}

/// The request's entry, in its extensions.
#[derive(Debug, Clone)]
pub(super) struct Tracked(pub(super) Arc<Entry>);

#[derive(Debug)]
struct Kept {
    /// The most requests held in `recent`.
    capacity: usize,
    format: LogFormat,
    recent: VecDeque<Arc<Entry>>,
    totals: Totals,
    providers: HashMap<String, ProviderStats>,
}

#[derive(Debug)]
pub struct RequestLog {
    next_id: AtomicU64,
    kept: Mutex<Kept>,
    events: broadcast::Sender<Arc<str>>,
}

impl RequestLog {
    /// A log as `config` says: keeping its last `request_log.history` requests
    /// in memory, printing each as its `log` says.
    pub fn new(config: &Config) -> Arc<Self> {
        let log = Arc::new(Self {
            next_id: AtomicU64::new(1),
            kept: Mutex::new(Kept {
                capacity: config.plugins.request_log.history.unwrap_or(KEPT),
                format: config.log,
                recent: VecDeque::new(),
                totals: Totals::default(),
                providers: HashMap::new(),
            }),
            events: broadcast::channel(256).0,
        });
        log.configure(config);
        log
    }

    /// Applies a reloaded `config`: what is kept stays, up to its
    /// `request_log.history`.
    pub fn configure(&self, config: &Config) {
        let mut kept = self.kept();
        kept.capacity = config.plugins.request_log.history.unwrap_or(KEPT);
        kept.format = config.log;
        while kept.recent.len() > kept.capacity {
            kept.recent.pop_front();
        }
    }

    /// How many requests it keeps: `request_log.history`.
    pub fn capacity(&self) -> usize {
        self.kept().capacity
    }

    fn kept(&self) -> MutexGuard<'_, Kept> {
        self.kept.lock()
    }

    /// The last `limit` requests that match `filter`, newest first.
    pub fn recent(&self, limit: usize, filter: &Filter) -> Vec<RequestRecord> {
        let kept = self.kept();
        let all = kept.recent.iter().rev().map(|e| e.snapshot());
        all.filter(|r| filter.matches(r)).take(limit).collect()
    }

    /// The kept requests that match `filter`, oldest first.
    pub fn export(&self, filter: &Filter) -> Vec<RequestRecord> {
        let kept = self.kept();
        let all = kept.recent.iter().map(|e| e.snapshot());
        all.filter(|r| filter.matches(r)).collect()
    }

    pub fn get(&self, id: &str) -> Option<RequestRecord> {
        let kept = self.kept();
        let entry = kept.recent.iter().find(|e| e.record().id == id);
        entry.map(|e| e.snapshot())
    }

    pub fn totals(&self) -> Totals {
        self.kept().totals
    }

    pub fn provider_stats(&self, provider: &str) -> ProviderStats {
        self.kept()
            .providers
            .get(provider)
            .copied()
            .unwrap_or_default()
    }

    /// Each request as it starts, changes and finishes, as JSON.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<str>> {
        self.events.subscribe()
    }

    pub(super) fn publish(&self, entry: &Entry) {
        if let Ok(json) = serde_json::to_string(&*entry.record()) {
            let _ = self.events.send(json.into());
        }
    }

    pub(super) fn start(&self, req: &Request) -> Arc<Entry> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let query_model = req.uri().query().and_then(|q| {
            form_urlencoded::parse(q.as_bytes())
                .find(|(name, _)| name == "model")
                .map(|(_, model)| clip(&model))
        });
        let record = RequestRecord {
            id: format!("r_{id:06}"),
            started_at: rfc3339(SystemTime::now()),
            method: req.method().to_string(),
            path: clip(req.uri().path()),
            format: None,
            kind: None,
            model: query_model,
            provider: None,
            client: None,
            status: None,
            duration_ms: None,
            ttfb_ms: None,
            stream: false,
            usage: None,
            cost_usd: None,
            steps: Vec::new(),
            error: None,
        };
        let entry = Arc::new(Entry {
            record: Mutex::new(record),
            started: Instant::now(),
            finished: AtomicBool::new(false),
            recorded: AtomicBool::new(false),
            ended: AtomicBool::new(false),
        });
        {
            let mut kept = self.kept();
            kept.recent.push_back(entry.clone());
            while kept.recent.len() > kept.capacity {
                kept.recent.pop_front();
            }
            kept.totals.requests += 1;
            kept.totals.in_flight += 1;
        }
        self.publish(&entry);
        entry
    }

    pub(super) fn finish(&self, entry: &Entry, error: Option<String>) {
        if entry.finished.swap(true, Ordering::SeqCst) {
            return;
        }
        let record = {
            let mut record = entry.record();
            record.duration_ms = Some(entry.elapsed_ms());
            if record.error.is_none() {
                record.error = error;
            }
            record.clone()
        };
        let failed = record.error.is_some() || record.status.is_some_and(|s| s >= 400);
        let format = {
            let mut kept = self.kept();
            let totals = &mut kept.totals;
            totals.in_flight = totals.in_flight.saturating_sub(1);
            totals.errors += u64::from(failed);
            if let Some(usage) = record.usage {
                totals.input_tokens += usage.input_tokens;
                totals.output_tokens += usage.output_tokens;
            }
            if let Some(cost) = record.cost_usd {
                *totals.cost_usd.get_or_insert(0.0) += cost;
            }
            if let Some(provider) = &record.provider {
                kept.providers
                    .entry(provider.clone())
                    .or_default()
                    .absorb(&record, failed);
            }
            kept.format
        };
        self.publish(entry);
        if format != LogFormat::Off {
            self.print(format, &record);
        }
    }
}

impl RequestLog {
    /// Writes a finished request to stderr: a warning if it was refused, an
    /// error if it failed.
    fn print(&self, format: LogFormat, record: &RequestRecord) {
        let level = match record.status {
            _ if record.error.is_some() => Level::Error,
            Some(s) if s >= 500 => Level::Error,
            Some(s) if s >= 400 => Level::Warn,
            _ => Level::Info,
        };
        let fields = match serde_json::to_value(record) {
            Ok(Value::Object(fields)) => fields,
            _ => Map::new(),
        };
        if format == LogFormat::Json {
            eprintln!("{}", format.line(level, "request", fields));
            return;
        }
        let width = std::io::stderr()
            .is_terminal()
            .then(|| terminal_size::terminal_size().map(|(w, _)| usize::from(w.0)))
            .flatten();
        let status = record.status.map_or("-".to_owned(), |s| s.to_string());
        let lines = lay_out(parts(record), width).into_iter().enumerate();
        let text: Vec<String> = lines
            .map(|(i, line)| {
                // Each part escaped, so a client's text styles nothing.
                let message: String = (line.iter())
                    .map(|p| {
                        format!(
                            "{}{}",
                            p.before,
                            paint(&escape_controls(&p.text), p.style, true)
                        )
                    })
                    .collect();
                match i {
                    0 => format.tagged(level, &status, &message),
                    _ => format!("{:PREFIX$}{message}", ""),
                }
            })
            .collect();
        eprintln!("{}", text.join("\n"));
    }
}

/// A piece of a request's line: what goes before it, its text, and its
/// style on a terminal (an SGR code, or none).
#[derive(Debug, Clone, PartialEq)]
struct Part {
    before: &'static str,
    text: String,
    style: &'static str,
    /// It may go on a line of its own: the details, the id, the error.
    tail: bool,
}

/// The columns a text line's time and tag take, before its message:
/// `HH:MM:SS.mmmZ`, a space, a five-column tag and a space.
const PREFIX: usize = 20;

/// One finished request, after its status: how long it took, what it asked
/// of whom, then the details, its id, and why it failed.
fn parts(r: &RequestRecord) -> Vec<Part> {
    let mut parts = Vec::new();
    // A tail part may go on a line of its own.
    let mut push = |before, text: String, style, tail| {
        parts.push(Part {
            before,
            text,
            style,
            tail,
        })
    };
    let took = r.duration_ms.map_or_else(|| "-".into(), duration);
    push("", format!("{took:>6}"), "", false);
    push("  ", r.method.clone(), "1", false);
    push(" ", r.path.clone(), "", false);
    match (&r.model, &r.provider) {
        (Some(model), provider) => {
            push("  ", model.clone(), "36", false);
            // A model that names its provider says it already.
            let named = |p: &str| model.strip_prefix(p).is_some_and(|m| m.starts_with('/'));
            if let Some(provider) = provider.as_deref().filter(|p| !named(p)) {
                push(" ", "→".into(), "2", false);
                push(" ", provider.to_owned(), "36", false);
            }
        }
        (None, Some(provider)) => {
            push("  ", "→".into(), "2", false);
            push(" ", provider.clone(), "36", false);
        }
        (None, None) => {}
    }
    let mut details = Vec::new();
    if let Some(ttfb) = r.ttfb_ms.filter(|_| r.stream) {
        details.push(format!("first token {}", duration(ttfb)));
    }
    if let Some(u) = r.usage {
        if u.input_tokens + u.output_tokens > 0 || u.seconds.is_none() {
            details.push(format!(
                "{} in / {} out tokens",
                u.input_tokens, u.output_tokens
            ));
        }
        if let Some(seconds) = u.seconds {
            details.push(format!("{seconds} s of audio"));
        }
    }
    if let Some(cost) = r.cost_usd.filter(|c| *c > 0.0) {
        details.push(usd(cost));
    }
    if !r.steps.is_empty() {
        let tools: usize = r.steps.iter().map(|s| s.tools.len()).sum();
        let steps = r.steps.len();
        let s = |n: usize| if n == 1 { "" } else { "s" };
        details.push(format!(
            "{steps} step{}, {tools} tool call{}",
            s(steps),
            s(tools)
        ));
    }
    if let Some(client) = &r.client {
        details.push(format!("client {client}"));
    }
    if !details.is_empty() {
        push("  ", details.join(" · "), "2", true);
    }
    push("  ", r.id.clone(), "2", true);
    if let Some(error) = &r.error {
        push("  ", error.clone(), "31", true);
    }
    parts
}

/// The line of `parts`, unstyled.
#[cfg(test)]
fn line(r: &RequestRecord) -> String {
    parts(r)
        .iter()
        .map(|p| format!("{}{}", p.before, p.text))
        .collect()
}

/// `parts` as the lines they take in `width` columns, if a terminal has
/// that many: one, if it fits; else the request on the first, its details
/// and id below, as many a line as fit, and its error on lines of its own,
/// wrapped at words. Unbounded, a line a request.
fn lay_out(parts: Vec<Part>, width: Option<usize>) -> Vec<Vec<Part>> {
    let columns = |p: &Part| p.before.chars().count() + p.text.chars().count();
    let wide = PREFIX + parts.iter().map(columns).sum::<usize>();
    let Some(width) = width.filter(|w| wide > *w && *w > PREFIX + 20) else {
        return vec![parts];
    };
    let (head, tail): (Vec<Part>, Vec<Part>) = parts.into_iter().partition(|p| !p.tail);
    let room = width - PREFIX;
    let mut lines = vec![head];
    let mut open: Vec<Part> = Vec::new();
    for part in tail {
        let used: usize = open.iter().map(columns).sum();
        let error = part.style == "31";
        if !open.is_empty() && (error || used + columns(&part) > room) {
            lines.push(std::mem::take(&mut open));
        }
        if !error && part.text.chars().count() <= room {
            let before = if open.is_empty() { "" } else { part.before };
            open.push(Part { before, ..part });
            continue;
        }
        // Too long for a line, or an error: its words, wrapped.
        let mut line = String::new();
        for word in part.text.split(' ') {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > room {
                let text = std::mem::take(&mut line);
                lines.push(vec![Part {
                    before: "",
                    text,
                    ..part.clone()
                }]);
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(vec![Part {
            before: "",
            text: line,
            ..part
        }]);
    }
    if !open.is_empty() {
        lines.push(open);
    }
    lines
}

fn duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.2}s", ms as f64 / 1000.0)
    }
}

/// `line` with its control characters escaped, so one request is one line.
/// `cost`, in US dollars: cents, or two significant digits below a cent,
/// so a call's fraction of a cent does not read as nothing.
fn usd(cost: f64) -> String {
    if cost >= 0.01 {
        return format!("${cost:.2}");
    }
    let places = (1.0 - cost.log10().floor()).clamp(2.0, 12.0) as usize;
    format!("${cost:.places$}")
}

/// Adds what a call to a provider cost to the cost of the request in
/// `extensions`, as the pricing plugin charges it.
pub fn charge(extensions: &Extensions, charge: &respan_gateway_plugin_pricing::Charge) {
    if let Some(Tracked(entry)) = extensions.get::<Tracked>() {
        *entry.record().cost_usd.get_or_insert(0.0) += charge.usd;
    }
}

/// Names the client whose key the request in `extensions` carried, in its
/// record.
pub fn attribute(extensions: &Extensions, client: &str) {
    if let Some(Tracked(entry)) = extensions.get::<Tracked>() {
        entry.record().client = Some(client.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::requests::record::Usage;

    #[test]
    fn a_fraction_of_a_cent_keeps_two_digits() {
        assert_eq!(usd(0.00000255), "$0.0000026");
        assert_eq!(usd(0.000029), "$0.000029");
        assert_eq!(usd(0.004), "$0.0040");
        assert_eq!(usd(1.234), "$1.23");
    }

    #[test]
    fn a_client_cannot_forge_a_line() {
        assert_eq!(
            escape_controls("r_1 POST /x 200 1ms a\nr_2 forged\u{1b}[2J"),
            "r_1 POST /x 200 1ms a\\nr_2 forged\\u{1b}[2J"
        );
    }

    fn timed(
        stream: bool,
        status: Option<u16>,
        ttfb_ms: Option<u64>,
        duration_ms: u64,
        input: u64,
        cached: u64,
        output: u64,
    ) -> RequestRecord {
        RequestRecord {
            id: String::new(),
            started_at: String::new(),
            method: String::new(),
            path: String::new(),
            format: None,
            kind: None,
            model: None,
            provider: None,
            client: None,
            stream,
            status,
            ttfb_ms,
            duration_ms: Some(duration_ms),
            usage: Some(Usage {
                input_tokens: input,
                output_tokens: output,
                cached_input_tokens: cached,
                seconds: None,
            }),
            cost_usd: None,
            steps: Vec::new(),
            error: None,
        }
    }

    #[test]
    fn rates_average_only_their_own_samples() {
        let mut stats = ProviderStats::default();
        stats.absorb(&timed(true, Some(200), Some(230), 812, 12, 0, 40), false);
        stats.absorb(&timed(true, Some(200), Some(120), 620, 8, 0, 20), false);
        // A whole-body answer: tokens, but no honest window.
        stats.absorb(&timed(false, Some(200), None, 900, 1_000, 500, 0), false);
        // A failed call's tokens never reached the client: no sum, no rate.
        stats.absorb(&timed(true, Some(500), Some(50), 100, 9, 0, 9), true);
        // A session's `101` opens a stream; it is not a sample, and its
        // usage belongs to the session's requests, not to one call's rate.
        stats.absorb(&timed(true, Some(101), Some(10), 5_000, 5, 0, 5), false);
        assert_eq!(stats.requests, 5);
        assert_eq!(stats.errors, 1);
        assert_eq!(stats.input_tokens, 1_025);
        assert_eq!(stats.cached_input_tokens, 500);
        assert_eq!(stats.output_tokens, 65);
        assert_eq!(stats.cache_hit_rate(), Some(500.0 / 1_025.0));
        // First-token windows: 230 ms for 12 tokens, 120 ms for 8.
        assert_eq!(stats.avg_input_tps(), Some(20.0 / 0.350));
        // Output windows: 812-230 ms for 40, 620-120 ms for 20.
        assert_eq!(stats.avg_output_tps(), Some(60.0 / 1.082));
    }

    #[test]
    fn a_rate_is_absent_without_samples_and_cache_of_zero_reads_zero() {
        let mut stats = ProviderStats::default();
        assert_eq!(stats.avg_input_tps(), None);
        assert_eq!(stats.avg_output_tps(), None);
        assert_eq!(stats.cache_hit_rate(), None);
        stats.absorb(&timed(true, Some(200), Some(100), 500, 10, 0, 20), false);
        assert_eq!(stats.cache_hit_rate(), Some(0.0));
        // A first token with no window left, or none measured, is no sample.
        stats.absorb(&timed(true, Some(200), Some(0), 500, 10, 0, 20), false);
        stats.absorb(&timed(true, Some(200), Some(500), 500, 10, 0, 20), false);
        assert_eq!(stats.input_timed_calls, 1);
    }

    #[test]
    fn a_cache_count_cannot_exceed_the_input_it_belongs_to() {
        let mut stats = ProviderStats::default();
        stats.absorb(&timed(true, Some(200), Some(100), 500, 10, 90, 20), false);
        assert_eq!(stats.cached_input_tokens, 10);
        assert_eq!(stats.cache_hit_rate(), Some(1.0));
    }

    #[test]
    fn a_line_says_what_matters() {
        let record = RequestRecord {
            id: "r_000001".into(),
            started_at: String::new(),
            method: "POST".into(),
            path: "/v1/chat/completions".into(),
            format: Some("openai-chat".into()),
            kind: Some("chat".into()),
            model: Some("gpt-4o-mini".into()),
            provider: Some("openai".into()),
            client: None,
            status: Some(200),
            duration_ms: Some(812),
            ttfb_ms: Some(230),
            stream: true,
            usage: Some(Usage {
                input_tokens: 12,
                output_tokens: 40,
                cached_input_tokens: 0,
                seconds: None,
            }),
            cost_usd: None,
            steps: Vec::new(),
            error: None,
        };
        assert_eq!(
            line(&record),
            " 812ms  POST /v1/chat/completions  gpt-4o-mini → openai  first token 230ms · 12 in / 40 out tokens  r_000001"
        );
        let looped = RequestRecord {
            client: Some("ops".into()),
            stream: false,
            usage: None,
            duration_ms: Some(2340),
            error: Some("the provider returned 500".into()),
            steps: vec![super::super::record::Step {
                index: 1,
                duration_ms: 2000,
                input_tokens: 0,
                output_tokens: 0,
                finish: None,
                tools: Vec::new(),
            }],
            ..record
        };
        assert_eq!(
            line(&looped),
            " 2.34s  POST /v1/chat/completions  gpt-4o-mini → openai  1 step, 0 tool calls · client ops  r_000001  the provider returned 500"
        );
    }

    #[test]
    fn a_line_too_wide_puts_its_details_below() {
        let part = |before, text: &str, style, tail| Part {
            before,
            text: text.into(),
            style,
            tail,
        };
        let parts = vec![
            part("", " 406ms", "", false),
            part("  ", "POST", "1", false),
            part(" ", "/v1/chat/completions", "", false),
            part("  ", "client ops", "2", true),
            part("  ", "r_000002", "2", true),
            part(
                "  ",
                "Function tools with reasoning_effort are not supported",
                "31",
                true,
            ),
        ];
        assert_eq!(lay_out(parts.clone(), None).len(), 1, "unbounded");
        assert_eq!(lay_out(parts.clone(), Some(400)).len(), 1, "it fits");
        let lines = lay_out(parts, Some(60));
        let text: Vec<String> = (lines.iter())
            .map(|l| {
                l.iter()
                    .map(|p| format!("{}{}", p.before, p.text))
                    .collect()
            })
            .collect();
        assert_eq!(
            text,
            [
                " 406ms  POST /v1/chat/completions",
                "client ops  r_000002",
                "Function tools with reasoning_effort are",
                "not supported",
            ]
        );
        assert!(text.iter().all(|l| l.chars().count() <= 60 - PREFIX));
    }
}

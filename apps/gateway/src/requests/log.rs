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

/// A provider's finished requests.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderStats {
    pub requests: u64,
    pub errors: u64,
    total_ms: u64,
}

impl ProviderStats {
    pub fn avg_latency_ms(&self) -> Option<u64> {
        (self.requests > 0).then(|| self.total_ms / self.requests)
    }
}

/// A client key's finished requests, with what they spent.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientStats {
    pub requests: u64,
    pub errors: u64,
    total_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The sum of the requests' `cost_usd`; present once one has a cost.
    pub cost_usd: Option<f64>,
}

impl ClientStats {
    pub fn avg_latency_ms(&self) -> Option<u64> {
        (self.requests > 0).then(|| self.total_ms / self.requests)
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
    clients: HashMap<String, ClientStats>,
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
                clients: HashMap::new(),
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

    /// Each client key's finished requests, by name.
    pub fn clients(&self) -> Vec<(String, ClientStats)> {
        let kept = self.kept();
        let mut clients: Vec<_> = kept.clients.iter().map(|(n, s)| (n.clone(), *s)).collect();
        clients.sort_by(|a, b| a.0.cmp(&b.0));
        clients
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
                let stats = kept.providers.entry(provider.clone()).or_default();
                stats.requests += 1;
                stats.errors += u64::from(failed);
                stats.total_ms += record.duration_ms.unwrap_or_default();
            }
            if let Some(client) = &record.client {
                let stats = kept.clients.entry(client.clone()).or_default();
                stats.requests += 1;
                stats.errors += u64::from(failed);
                stats.total_ms += record.duration_ms.unwrap_or_default();
                if let Some(usage) = record.usage {
                    stats.input_tokens += usage.input_tokens;
                    stats.output_tokens += usage.output_tokens;
                }
                if let Some(cost) = record.cost_usd {
                    *stats.cost_usd.get_or_insert(0.0) += cost;
                }
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

    #[test]
    fn what_each_client_spent_is_summed_per_name() {
        let config = crate::Config::parse("").unwrap_or_else(|e| panic!("{e}"));
        let log = RequestLog::new(&config);
        let request = || {
            axum::extract::Request::builder()
                .method(axum::http::Method::POST)
                .uri("/v1/chat/completions")
                .body(axum::body::Body::empty())
                .unwrap_or_else(|e| panic!("{e}"))
        };

        // Two finished by ops (named by the edge, as a client key's are),
        // one of them failed, and one by app.
        let entry = log.start(&request());
        let mut extensions = respan_gateway_core::Extensions::new();
        extensions.insert(Tracked(entry.clone()));
        attribute(&extensions, "ops");
        entry.record().usage = Some(Usage {
            input_tokens: 10,
            output_tokens: 40,
            ..Usage::default()
        });
        entry.record().cost_usd = Some(0.0042);
        log.finish(&entry, None);

        let entry = log.start(&request());
        entry.record().client = Some("ops".into());
        entry.record().status = Some(500);
        entry.record().usage = Some(Usage {
            input_tokens: 5,
            ..Usage::default()
        });
        log.finish(&entry, Some("the provider returned 500".to_owned()));

        let entry = log.start(&request());
        entry.record().client = Some("app".into());
        entry.record().usage = Some(Usage {
            input_tokens: 1,
            output_tokens: 2,
            ..Usage::default()
        });
        log.finish(&entry, None);

        let clients = log.clients();
        assert_eq!(
            clients
                .iter()
                .map(|(name, _)| name.as_str())
                .collect::<Vec<_>>(),
            ["app", "ops"]
        );
        let (_, ops) = &clients[1];
        assert_eq!(ops.requests, 2);
        assert_eq!(ops.errors, 1);
        assert_eq!(ops.input_tokens, 15);
        assert_eq!(ops.output_tokens, 40);
        assert_eq!(ops.cost_usd, Some(0.0042));
        assert!(ops.avg_latency_ms().is_some());
        let (_, app) = &clients[0];
        assert_eq!(app.requests, 1);
        assert_eq!(app.errors, 0);
        assert_eq!(app.cost_usd, None);
    }
}

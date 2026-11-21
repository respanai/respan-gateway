#![warn(missing_docs)]
//! `Respan`: Respan observability. Each client request is a trace, sent to
//! Respan's OTLP endpoint: a root span for the request, and a span for each
//! call it made, to a model (a `chat` span, with its model, token usage and,
//! unless `content` is off, its messages and reply) or to an MCP server, as
//! the tool loop's calls are (a `tool` span for a `tools/call`, with the
//! tool's name and, unless `content` is off, its arguments and result).
//!
//! Spans are sent off the request path: queued, then posted in batches (by
//! [`BATCH`] spans or every [`EVERY`]) as OTLP/HTTP JSON by a task of their
//! own. While Respan is slow or down, the queue holds at most [`QUEUE`]
//! traces and drops what is beyond, counted, rather than slow a request.
//! What is queued is sent when the plugin is dropped. How sending goes is
//! its [`Health`], for its host to show and to say when it starts failing.
//!
//! A host registers [`Respan`] before its other plugins, which opens a
//! request's trace and sends it, and [`Respan::calls`] after them, which
//! records each call.

use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use respan_gateway_conversation::{Assembled, Content, Input, Message, Output, Role};
use respan_gateway_core::{
    ClientContext, Fault, Filter, Plugin, ProviderContext, Response, Result, async_trait,
};
use respan_gateway_http_transport::{Network, reqwest};
use respan_gateway_mcp as mcp;
use serde_json::{Value, json};
use tokio::sync::mpsc;

/// Respan's OTLP trace endpoint.
pub const URL: &str = "https://api.respan.ai/api/v2/traces";
/// How many spans are posted at most at once.
pub const BATCH: usize = 100;
/// How long a span waits at most before it is posted.
pub const EVERY: Duration = Duration::from_secs(2);
/// How many traces wait at most to be posted.
pub const QUEUE: usize = 4096;

/// The plugin: one per gateway, its task posting what it queues.
pub struct Respan {
    queue: mpsc::Sender<Vec<Value>>,
    content: bool,
    health: Arc<Health>,
    traced: Option<Arc<Traced>>,
}

type Traced = dyn Fn(&ClientContext) -> bool + Send + Sync;

type Change = Box<dyn Fn(Option<&str>) + Send + Sync>;

/// How sending to Respan goes: what was sent, rejected, lost, and the last
/// success and error.
#[derive(Default)]
pub struct Health {
    sent: AtomicU64,
    rejected: AtomicU64,
    failed: AtomicU64,
    dropped: AtomicU64,
    last: Mutex<Last>,
    on_change: std::sync::OnceLock<Change>,
}

#[derive(Debug, Default, Clone)]
struct Last {
    ok_at: Option<SystemTime>,
    error: Option<(SystemTime, String)>,
    failing: bool,
}

/// [`Health`] as it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Spans Respan accepted.
    pub sent: u64,
    /// Spans Respan answered it rejected.
    pub rejected: u64,
    /// Spans whose post failed: Respan unreachable, or refusing it.
    pub failed: u64,
    /// Traces dropped unsent, the queue full while Respan was slow or down.
    pub dropped: u64,
    /// When a post last succeeded.
    pub ok_at: Option<SystemTime>,
    /// The last error, and when: a failed post, or spans rejected.
    pub error: Option<(SystemTime, String)>,
    /// Whether the last post failed.
    pub failing: bool,
}

impl std::fmt::Debug for Health {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Health").field(&self.report()).finish()
    }
}

impl Health {
    /// How it is now.
    pub fn report(&self) -> Report {
        let last = self.last.lock().clone();
        Report {
            sent: self.sent.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            failed: self.failed.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            ok_at: last.ok_at,
            error: last.error,
            failing: last.failing,
        }
    }

    /// Calls `change` when sending starts failing, with why, and when it
    /// works again, with `None`; and when Respan rejects spans for a reason
    /// not said before. A second is ignored.
    pub fn on_change(&self, change: impl Fn(Option<&str>) + Send + Sync + 'static) {
        let _ = self.on_change.set(Box::new(change));
    }

    /// A post of `count` spans that Respan took, rejecting `rejected` of
    /// them for `why`.
    fn sent(&self, count: u64, rejected: u64, why: Option<String>) {
        self.sent
            .fetch_add(count - rejected.min(count), Ordering::Relaxed);
        self.rejected.fetch_add(rejected, Ordering::Relaxed);
        let mut last = self.last.lock();
        let recovered = std::mem::replace(&mut last.failing, false);
        last.ok_at = Some(SystemTime::now());
        let new = why.filter(|why| last.error.as_ref().is_none_or(|(_, e)| e != why));
        if let Some(why) = &new {
            last.error = Some((SystemTime::now(), why.clone()));
        }
        drop(last);
        if let Some(change) = self.on_change.get() {
            if let Some(why) = &new {
                change(Some(why));
            } else if recovered {
                change(None);
            }
        }
    }

    /// A post of `count` spans that failed, for `why`.
    fn failed(&self, count: u64, why: String) {
        self.failed.fetch_add(count, Ordering::Relaxed);
        let mut last = self.last.lock();
        let started = !std::mem::replace(&mut last.failing, true);
        last.error = Some((SystemTime::now(), why.clone()));
        drop(last);
        if let (true, Some(change)) = (started, self.on_change.get()) {
            change(Some(&why));
        }
    }
}

impl std::fmt::Debug for Respan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Respan")
            .field("content", &self.content)
            .finish_non_exhaustive()
    }
}

impl Respan {
    /// Traces of service `service` sent to `url` with `key`, messages and
    /// replies among them unless `content` is false, through
    /// [`Network::shared`]. Its task runs on the current Tokio runtime.
    pub fn new(key: &str, url: &str, service: &str, content: bool) -> Result<Self> {
        let http = Self::client(Network::shared())?;
        Self::with_client(key, url, service, content, http)
    }

    /// The client it sends with through `network`: thirty seconds a batch.
    pub fn client(network: &Network) -> Result<reqwest::Client> {
        network.client("Respan", |builder| {
            Ok(builder.timeout(Duration::from_secs(30)))
        })
    }

    /// Like [`new`](Self::new), sending with `http`: one of your own, or
    /// [`client`](Self::client)'s through a network.
    pub fn with_client(
        key: &str,
        url: &str,
        service: &str,
        content: bool,
        http: reqwest::Client,
    ) -> Result<Self> {
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| Fault::config("respan: needs a Tokio runtime to send from"))?;
        let url =
            reqwest::Url::parse(url).map_err(|e| Fault::config(format!("respan: url: {e}")))?;
        let (queue, traces) = mpsc::channel(QUEUE);
        let health = Arc::new(Health::default());
        let poster = Poster {
            http,
            url,
            key: format!("Bearer {key}"),
            service: service.to_owned(),
            health: health.clone(),
        };
        runtime.spawn(poster.run(traces));
        Ok(Self {
            queue,
            content,
            health,
            traced: None,
        })
    }

    /// Traces only the requests `traced` accepts, such as its host's
    /// clients' and not its own; every request if unset.
    pub fn only(mut self, traced: impl Fn(&ClientContext) -> bool + Send + Sync + 'static) -> Self {
        self.traced = Some(Arc::new(traced));
        self
    }

    /// How sending goes, which its host keeps to show.
    pub fn health(&self) -> Arc<Health> {
        self.health.clone()
    }

    /// The plugin that records each call of a trace, registered last.
    pub fn calls(&self) -> Calls {
        Calls {
            content: self.content,
        }
    }
}

/// A request's trace, in its extensions, which its calls' copy.
#[derive(Clone)]
struct Trace(Arc<Mutex<TraceState>>);

struct TraceState {
    trace_id: String,
    root_id: String,
    start: SystemTime,
    calls: Vec<Call>,
}

/// One call of a request, in its own extensions.
#[derive(Clone)]
struct Call(Arc<Mutex<CallState>>);

struct CallState {
    span_id: String,
    provider: String,
    format: String,
    start: SystemTime,
    end: Option<SystemTime>,
    error: Option<String>,
    /// Whether it is of the conversation kind: a call to a model.
    chat: bool,
    model: Option<String>,
    prompt: Vec<SpanMessage>,
    /// The tools offered: name, description, and parameters as JSON.
    tools: Vec<(String, Option<String>, String)>,
    said: Assembled,
    /// What it asked an MCP server, if it is a call to one.
    rpc: Option<Rpc>,
}

/// A call's JSON-RPC request to an MCP server (or notification), and its
/// answer.
#[derive(Debug, Default)]
struct Rpc {
    /// The request's id; none for a notification, which has no answer.
    id: Option<Value>,
    method: String,
    /// The tool a `tools/call` calls.
    tool: Option<String>,
    /// Its parameters (a tool's arguments), unless content is off.
    input: Option<Value>,
    /// Its answer's result or error, unless content is off.
    output: Option<Value>,
}

#[async_trait]
impl Plugin for Respan {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>> {
        if self.traced.as_ref().is_some_and(|traced| !traced(cx)) {
            return Ok(None);
        }
        let trace = TraceState {
            trace_id: id(16),
            root_id: id(8),
            start: SystemTime::now(),
            calls: Vec::new(),
        };
        cx.extensions_mut()
            .insert(Trace(Arc::new(Mutex::new(trace))));
        Ok(None)
    }

    async fn on_client_end(&self, cx: &ClientContext, end: &Result<()>) {
        let Some(Trace(trace)) = cx.extensions().get::<Trace>() else {
            return;
        };
        let spans = spans(&trace.lock(), cx.format(), end, self.content);
        if self.queue.try_send(spans).is_err() {
            self.health.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The calls of a [`Respan`] trace: a span for each call to a model or a
/// tool server, as sent. Register it last, so it sees each call as the
/// plugins before it left it, and [`Respan`] first, so it sees each request
/// before any plugin answers it, as a tool loop does.
#[derive(Debug, Clone, Copy)]
pub struct Calls {
    content: bool,
}

#[async_trait]
impl Plugin for Calls {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<()> {
        let Some(Trace(trace)) = px.extensions().get::<Trace>().cloned() else {
            return Ok(());
        };
        let chat = px.is_kind::<Input>();
        let call = Call(Arc::new(Mutex::new(CallState {
            span_id: id(8),
            provider: px.provider().to_owned(),
            format: px.format().to_owned(),
            start: SystemTime::now(),
            end: None,
            error: None,
            chat,
            model: None,
            prompt: Vec::new(),
            tools: Vec::new(),
            said: Assembled::default(),
            rpc: None,
        })));
        trace.lock().calls.push(call.clone());
        px.extensions_mut().insert(call.clone());
        if px.is_kind::<mcp::Input>() {
            return rpc(px, call, self.content);
        }
        if !chat {
            return Ok(());
        }
        let (asked, content) = (call.clone(), self.content);
        px.filter(move |input: &mut Input| {
            let mut state = asked.0.lock();
            state.model.clone_from(&input.model);
            if content {
                state.prompt = input.messages.iter().flat_map(messages_of).collect();
                state.tools = (input.tools.iter())
                    .map(|t| {
                        (
                            t.name.clone(),
                            t.description.clone(),
                            t.parameters.to_string(),
                        )
                    })
                    .collect();
            }
            Ok(())
        })?;
        px.filter(Said { call, content })
    }

    async fn on_provider_end(&self, px: &ProviderContext, end: &Result<()>) {
        if let Some(Call(call)) = px.extensions().get::<Call>() {
            let mut state = call.lock();
            state.end = Some(SystemTime::now());
            if let Err(fault) = end {
                state.error = Some(fault.message().to_owned());
            }
        }
    }
}

/// Records a call to an MCP server: its request, and the answer to it.
fn rpc(px: &mut ProviderContext, call: Call, content: bool) -> Result<()> {
    let asked = call.clone();
    px.filter(move |input: &mut mcp::Input| {
        // Its request, else a notification (as `initialized` is).
        let sent = input.messages.iter();
        let request = sent.clone().find(|m| m.request_id().is_some());
        let Some(request) = request.or_else(|| sent.clone().find(|m| m.method().is_some())) else {
            return Ok(());
        };
        let params = request.params().cloned().unwrap_or(Value::Null);
        let method = request.method().unwrap_or_default().to_owned();
        let tool = (method == "tools/call")
            .then(|| params["name"].as_str().map(str::to_owned))
            .flatten();
        let input = match &tool {
            Some(_) => params["arguments"].clone(),
            None => params,
        };
        let input = Some(input).filter(|i| content && !i.is_null());
        asked.0.lock().rpc = Some(Rpc {
            id: request.request_id(),
            method,
            tool,
            input,
            output: None,
        });
        Ok(())
    })?;
    px.filter(move |output: &mut mcp::Output| {
        let mut state = call.0.lock();
        let state = &mut *state;
        let Some(rpc) = state.rpc.as_mut() else {
            return Ok(());
        };
        let answer = output.messages.iter().find_map(|m| match m {
            mcp::Message::Response { outcome, .. }
                if rpc.id.is_some() && m.response_id() == rpc.id =>
            {
                Some(outcome)
            }
            _ => None,
        });
        let (output, error) = match answer {
            None => return Ok(()),
            Some(mcp::Outcome::Error(e)) => (json!({ "error": e }), Some(e.message.clone())),
            // A tool says it failed in its result, not as an error.
            Some(mcp::Outcome::Success(result)) => {
                let failed = result["isError"] == true;
                (
                    result.clone(),
                    failed.then(|| "the tool answered an error".to_owned()),
                )
            }
        };
        if content {
            rpc.output = Some(output);
        }
        state.error = state.error.take().or(error);
        Ok(())
    })
}

/// Folds what a model said, for its span: without content, only which
/// model it was and its usage.
struct Said {
    call: Call,
    content: bool,
}

impl Filter<Output> for Said {
    fn edit(&mut self, output: Output, out: &mut Vec<Output>) -> Result<()> {
        if self.content || matches!(output, Output::Start(_) | Output::Usage(_)) {
            // What cannot be folded only goes unrecorded.
            let _ = self.call.0.lock().said.push(&output);
        }
        out.push(output);
        Ok(())
    }
}

/// A trace's spans: its root, then each call's.
fn spans(trace: &TraceState, format: &str, end: &Result<()>, content: bool) -> Vec<Value> {
    let now = SystemTime::now();
    let root = vec![
        attr("traceloop.span.kind", "workflow"),
        attr("traceloop.workflow.name", format),
        attr("gateway.format", format),
    ];
    let mut spans = vec![span(
        &trace.trace_id,
        &trace.root_id,
        None,
        format,
        trace.start,
        now,
        end.as_ref().err().map(|f| f.message()),
        root,
    )];
    for call in &trace.calls {
        let c = call.0.lock();
        let mut attrs = vec![
            attr("gateway.provider", &c.provider),
            attr("gateway.format", &c.format),
        ];
        if c.chat {
            attrs.push(attr("traceloop.span.kind", "chat"));
            attrs.push(attr("llm.request.type", "chat"));
            attrs.push(attr("gen_ai.system", &c.provider));
            if let Some(model) = &c.model {
                attrs.push(attr("gen_ai.request.model", model));
            }
            if let Some(start) = &c.said.start {
                attrs.push(attr("gen_ai.response.model", &start.model));
            }
            let usage = &c.said.usage;
            if usage.input_tokens + usage.output_tokens > 0 {
                attrs.push(int("gen_ai.usage.prompt_tokens", usage.input_tokens));
                attrs.push(int("gen_ai.usage.completion_tokens", usage.output_tokens));
                attrs.push(int(
                    "gen_ai.usage.total_tokens",
                    usage.input_tokens + usage.output_tokens,
                ));
            }
            if content {
                for (n, said) in c.prompt.iter().enumerate() {
                    said.attrs(&format!("gen_ai.prompt.{n}"), &mut attrs);
                }
                reply(&c.said).attrs("gen_ai.completion.0", &mut attrs);
                for (m, (name, description, parameters)) in c.tools.iter().enumerate() {
                    let at = format!("llm.request.functions.{m}");
                    attrs.push(attr(&format!("{at}.name"), name));
                    if let Some(description) = description {
                        attrs.push(attr(&format!("{at}.description"), description));
                    }
                    attrs.push(attr(&format!("{at}.parameters"), parameters));
                }
            }
        } else if let Some(rpc) = &c.rpc {
            let kind = if rpc.tool.is_some() { "tool" } else { "task" };
            attrs.push(attr("traceloop.span.kind", kind));
            attrs.push(attr("mcp.method", &rpc.method));
            let entity = rpc.tool.as_deref().unwrap_or(&rpc.method);
            attrs.push(attr("traceloop.entity.name", entity));
            if let Some(input) = &rpc.input {
                attrs.push(attr("traceloop.entity.input", &input.to_string()));
            }
            if let Some(output) = &rpc.output {
                attrs.push(attr("traceloop.entity.output", &output.to_string()));
            }
        } else {
            attrs.push(attr("traceloop.span.kind", "task"));
        }
        let name = match (&c.model, &c.rpc) {
            (Some(model), _) if c.chat => model.clone(),
            (_, Some(rpc)) => {
                let entity = rpc.tool.as_deref().unwrap_or(&rpc.method);
                format!("{}.{entity}", c.provider)
            }
            _ => c.provider.clone(),
        };
        spans.push(span(
            &trace.trace_id,
            &c.span_id,
            Some(&trace.root_id),
            &name,
            c.start,
            c.end.unwrap_or(now),
            c.error.as_deref(),
            attrs,
        ));
    }
    spans
}

/// An OTLP span in JSON.
#[allow(clippy::too_many_arguments)]
fn span(
    trace_id: &str,
    span_id: &str,
    parent: Option<&str>,
    name: &str,
    start: SystemTime,
    end: SystemTime,
    error: Option<&str>,
    attributes: Vec<Value>,
) -> Value {
    let nanos = |t: SystemTime| {
        let since = t.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_default();
        since.as_nanos().to_string()
    };
    let status = match error {
        Some(message) => json!({ "code": 2, "message": message }),
        None => json!({ "code": 1 }),
    };
    json!({
        "traceId": trace_id,
        "spanId": span_id,
        "parentSpanId": parent.unwrap_or_default(),
        "name": name,
        "kind": 1,
        "startTimeUnixNano": nanos(start),
        "endTimeUnixNano": nanos(end),
        "attributes": attributes,
        "status": status,
    })
}

fn attr(key: &str, value: &str) -> Value {
    json!({ "key": key, "value": { "stringValue": value } })
}

fn int(key: &str, value: u64) -> Value {
    json!({ "key": key, "value": { "intValue": value.to_string() } })
}

fn role(role: &Role) -> String {
    match role {
        Role::System => "system".into(),
        Role::Developer => "developer".into(),
        Role::User => "user".into(),
        Role::Assistant => "assistant".into(),
        // WARN: a native role is its format's alone to name; written as `other`.
        _ => "other".into(),
    }
}

/// One message of a span, as OpenLLMetry writes it: its role and text, the
/// tools it called, and the call it answers if it is a tool's.
#[derive(Debug, Clone, Default, PartialEq)]
struct SpanMessage {
    role: String,
    content: String,
    /// Each call's id, tool and arguments (JSON text).
    calls: Vec<(String, String, String)>,
    /// The call a tool's result answers.
    answers: Option<String>,
}

impl SpanMessage {
    /// Its attributes under `at`, such as `gen_ai.prompt.0`.
    fn attrs(&self, at: &str, attrs: &mut Vec<Value>) {
        attrs.push(attr(&format!("{at}.role"), &self.role));
        if !self.content.is_empty() || self.calls.is_empty() {
            attrs.push(attr(&format!("{at}.content"), &self.content));
        }
        if let Some(id) = &self.answers {
            attrs.push(attr(&format!("{at}.tool_call_id"), id));
        }
        for (m, (id, name, arguments)) in self.calls.iter().enumerate() {
            let call = format!("{at}.tool_calls.{m}");
            attrs.push(attr(&format!("{call}.id"), id));
            attrs.push(attr(&format!("{call}.name"), name));
            attrs.push(attr(&format!("{call}.arguments"), arguments));
        }
    }
}

/// A message as the span's messages: each tool's result its own `tool`
/// message, then the message's text and calls, if it has any.
fn messages_of(message: &Message) -> Vec<SpanMessage> {
    let mut said: Vec<SpanMessage> = (message.content.iter())
        .filter_map(|c| match c {
            Content::ToolResult { id, content, .. } => Some(SpanMessage {
                role: "tool".into(),
                content: text(content),
                answers: Some(id.clone()),
                ..SpanMessage::default()
            }),
            _ => None,
        })
        .collect();
    let own = SpanMessage {
        role: role(&message.role),
        content: text(&message.content),
        calls: calls(&message.content),
        answers: None,
    };
    if said.is_empty() || !own.content.is_empty() || !own.calls.is_empty() {
        said.push(own);
    }
    said
}

/// The tool calls among `content`: each one's id, tool and arguments.
fn calls(content: &[Content]) -> Vec<(String, String, String)> {
    (content.iter())
        .filter_map(|c| match c {
            Content::ToolCall {
                id,
                name,
                arguments,
                ..
            } => Some((id.clone(), name.clone(), arguments.clone())),
            _ => None,
        })
        .collect()
}

/// A message's text, with a mark for each medium; its tool calls and
/// results are said apart ([`SpanMessage`]).
fn text(content: &[Content]) -> String {
    let part = |c: &Content| match c {
        Content::Text { text, .. } => text.clone(),
        Content::Image(_) => "[image]".into(),
        Content::Audio(_) => "[audio]".into(),
        Content::Document(_) => "[document]".into(),
        _ => String::new(),
    };
    let parts: Vec<String> = content.iter().map(part).filter(|p| !p.is_empty()).collect();
    parts.join("\n")
}

/// What a model said: its text and its tool calls.
fn reply(said: &Assembled) -> SpanMessage {
    SpanMessage {
        role: "assistant".into(),
        content: said.text(),
        calls: calls(&said.replay()),
        answers: None,
    }
}

/// `bytes` random bytes, at most 16, as hex: a trace's (16) or a span's
/// (8) id.
fn id(bytes: usize) -> String {
    let mut hex = uuid::Uuid::new_v4().simple().to_string();
    hex.truncate(bytes * 2);
    hex
}

/// Posts what is queued, in batches.
struct Poster {
    http: reqwest::Client,
    url: reqwest::Url,
    key: String,
    service: String,
    health: Arc<Health>,
}

impl Poster {
    async fn run(self, mut traces: mpsc::Receiver<Vec<Value>>) {
        let mut held: Vec<Value> = Vec::new();
        let mut tick = tokio::time::interval(EVERY);
        loop {
            tokio::select! {
                trace = traces.recv() => match trace {
                    Some(spans) => {
                        held.extend(spans);
                        if held.len() >= BATCH {
                            self.post(std::mem::take(&mut held)).await;
                        }
                    }
                    // The plugin is gone: what it queued goes, then the task.
                    None => {
                        self.post(held).await;
                        return;
                    }
                },
                _ = tick.tick() => {
                    if !held.is_empty() {
                        self.post(std::mem::take(&mut held)).await;
                    }
                }
            }
        }
    }

    /// Posts `spans`, [`BATCH`] at a time.
    async fn post(&self, mut spans: Vec<Value>) {
        while !spans.is_empty() {
            let rest = spans.split_off(spans.len().min(BATCH));
            self.send(spans).await;
            spans = rest;
        }
    }

    /// Posts `spans` as one batch, and notes how that went.
    async fn send(&self, spans: Vec<Value>) {
        let count = spans.len() as u64;
        let body = json!({ "resourceSpans": [{
            "resource": { "attributes": [attr("service.name", &self.service)] },
            "scopeSpans": [{ "scope": { "name": "respan-gateway" }, "spans": spans }],
        }] });
        let sent = self
            .http
            .post(self.url.clone())
            .header("authorization", &self.key)
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .await;
        let answer = match sent {
            Ok(answer) => answer,
            Err(e) => {
                // A reqwest error says the URL and why, never the headers.
                let why = format!("could not reach Respan: {e}");
                return self.health.failed(count, why);
            }
        };
        let status = answer.status();
        let text = answer.text().await.unwrap_or_default();
        if !status.is_success() {
            let said: Value = serde_json::from_str(&text).unwrap_or_default();
            let said = (said["message"].as_str().or(said["error"].as_str()))
                .or(said["detail"].as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| text.chars().take(200).collect());
            return self
                .health
                .failed(count, format!("Respan answered {status}: {said}"));
        }
        // OTLP's partial success: some spans taken, the rest rejected.
        let said: Value = serde_json::from_str(&text).unwrap_or_default();
        let partial = &said["partialSuccess"];
        let rejected = match &partial["rejectedSpans"] {
            Value::String(n) => n.parse().unwrap_or(0),
            n => n.as_u64().unwrap_or(0),
        };
        let why = (rejected > 0).then(|| {
            let message = partial["errorMessage"].as_str().unwrap_or_default();
            format!("Respan rejected {rejected} of {count} spans: {message}")
        });
        self.health.sent(count, rejected, why);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::{Block, Delta, Start, Usage};

    #[test]
    fn a_trace_is_a_root_span_and_a_chat_span_per_call() {
        let mut said = Assembled::default();
        for output in [
            Output::Start(Start::new("r", "gpt-4o-mini-2024")),
            Output::block_start(0, Block::Text),
            Output::Delta {
                block: 0,
                delta: Delta::Text("Hello".into()),
            },
            Output::Usage(Usage::new(9, 1)),
        ] {
            said.push(&output).expect("folded");
        }
        let call = Call(Arc::new(Mutex::new(CallState {
            span_id: "b".repeat(16),
            provider: "openai".into(),
            format: "openai-chat".into(),
            start: SystemTime::UNIX_EPOCH,
            end: Some(SystemTime::UNIX_EPOCH + Duration::from_millis(5)),
            error: None,
            chat: true,
            model: Some("gpt-4o-mini".into()),
            prompt: vec![SpanMessage {
                role: "user".into(),
                content: "Say hi".into(),
                ..SpanMessage::default()
            }],
            tools: Vec::new(),
            said,
            rpc: None,
        })));
        let trace = TraceState {
            trace_id: "a".repeat(32),
            root_id: "c".repeat(16),
            start: SystemTime::UNIX_EPOCH,
            calls: vec![call],
        };
        let spans = spans(&trace, "anthropic-messages", &Ok(()), true);
        let attr_of = |span: &Value, key: &str| {
            let attrs = span["attributes"].as_array().expect("attributes");
            let found = attrs
                .iter()
                .find(|a| a["key"] == key)
                .map(|a| a["value"].clone());
            found.unwrap_or(Value::Null)
        };
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0]["parentSpanId"], "");
        let chat = &spans[1];
        assert_eq!(chat["parentSpanId"], "c".repeat(16));
        assert_eq!(chat["endTimeUnixNano"], "5000000");
        assert_eq!(attr_of(chat, "llm.request.type")["stringValue"], "chat");
        assert_eq!(
            attr_of(chat, "gen_ai.response.model")["stringValue"],
            "gpt-4o-mini-2024"
        );
        assert_eq!(attr_of(chat, "gen_ai.usage.total_tokens")["intValue"], "10");
        assert_eq!(
            attr_of(chat, "gen_ai.prompt.0.content")["stringValue"],
            "Say hi"
        );
        assert_eq!(
            attr_of(chat, "gen_ai.completion.0.content")["stringValue"],
            "Hello"
        );

        let quiet = super::spans(&trace, "openai-chat", &Ok(()), false);
        assert_eq!(attr_of(&quiet[1], "gen_ai.prompt.0.content"), Value::Null);
        assert_eq!(
            attr_of(&quiet[1], "gen_ai.usage.total_tokens")["intValue"],
            "10"
        );
    }

    #[test]
    fn a_failed_request_and_a_tool_call_say_so() {
        let call = Call(Arc::new(Mutex::new(CallState {
            span_id: "d".repeat(16),
            provider: "files".into(),
            format: "mcp".into(),
            start: SystemTime::UNIX_EPOCH,
            end: None,
            error: Some("down".into()),
            chat: false,
            model: None,
            prompt: Vec::new(),
            tools: Vec::new(),
            said: Assembled::default(),
            rpc: Some(Rpc {
                id: Some(json!(3)),
                method: "tools/call".into(),
                tool: Some("read".into()),
                input: Some(json!({ "path": "a.txt" })),
                output: Some(json!({ "content": [], "isError": true })),
            }),
        })));
        let trace = TraceState {
            trace_id: "a".repeat(32),
            root_id: "c".repeat(16),
            start: SystemTime::UNIX_EPOCH,
            calls: vec![call],
        };
        let failed: Result<()> = Err(Fault::upstream("the provider returned 500"));
        let spans = spans(&trace, "openai-chat", &failed, true);
        assert_eq!(
            spans[0]["status"],
            json!({ "code": 2, "message": "the provider returned 500" })
        );
        assert_eq!(spans[1]["status"]["code"], 2);
        let kind = spans[1]["attributes"]
            .as_array()
            .and_then(|a| a.iter().find(|a| a["key"] == "traceloop.span.kind"))
            .map(|a| a["value"]["stringValue"].clone());
        assert_eq!(kind, Some(json!("tool")));
        let attr_of = |key: &str| {
            let attrs = spans[1]["attributes"].as_array().expect("attributes");
            let found = attrs.iter().find(|a| a["key"] == key);
            found.map(|a| a["value"]["stringValue"].clone())
        };
        assert_eq!(spans[1]["name"], "files.read");
        assert_eq!(attr_of("traceloop.entity.name"), Some(json!("read")));
        assert_eq!(
            attr_of("traceloop.entity.input"),
            Some(json!(r#"{"path":"a.txt"}"#))
        );
        assert!(attr_of("traceloop.entity.output").is_some());
    }

    #[test]
    fn tool_calls_and_their_results_are_said_as_openllmetry_says_them() {
        let mut asked = Message::new(Role::Assistant, "Looking.");
        asked.content.push(Content::tool_call(
            "call_1",
            "files__read",
            r#"{"path":"a"}"#,
        ));
        let mut answered = Message::new(Role::User, "");
        answered.content = vec![Content::tool_result(
            "call_1",
            vec![Content::from("hi")],
            false,
        )];
        let prompt: Vec<SpanMessage> = [asked, answered].iter().flat_map(messages_of).collect();
        let mut said = Assembled::default();
        for output in [
            Output::Start(Start::new("r", "m")),
            Output::block_start(
                0,
                Block::ToolCall {
                    id: "call_2".into(),
                    name: "files__read".into(),
                },
            ),
            Output::Delta {
                block: 0,
                delta: Delta::Arguments(r#"{"path":"b"}"#.into()),
            },
        ] {
            said.push(&output).expect("folded");
        }
        let mut attrs = Vec::new();
        for (n, message) in prompt.iter().enumerate() {
            message.attrs(&format!("gen_ai.prompt.{n}"), &mut attrs);
        }
        reply(&said).attrs("gen_ai.completion.0", &mut attrs);
        let found = |key: &str| {
            let attr = attrs.iter().find(|a| a["key"] == key);
            attr.map(|a| a["value"]["stringValue"].clone())
        };
        assert_eq!(found("gen_ai.prompt.0.content"), Some(json!("Looking.")));
        assert_eq!(
            found("gen_ai.prompt.0.tool_calls.0.id"),
            Some(json!("call_1"))
        );
        assert_eq!(
            found("gen_ai.prompt.0.tool_calls.0.name"),
            Some(json!("files__read"))
        );
        assert_eq!(found("gen_ai.prompt.1.role"), Some(json!("tool")));
        assert_eq!(found("gen_ai.prompt.1.tool_call_id"), Some(json!("call_1")));
        assert_eq!(found("gen_ai.prompt.1.content"), Some(json!("hi")));
        assert_eq!(prompt.len(), 2, "the empty user message says nothing more");
        // The reply's call is a call, not text.
        assert_eq!(found("gen_ai.completion.0.content"), None);
        assert_eq!(
            found("gen_ai.completion.0.tool_calls.0.arguments"),
            Some(json!(r#"{"path":"b"}"#))
        );
    }
}

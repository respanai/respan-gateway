#![warn(missing_docs)]
//! The tool loop: the gateway runs tools inside a chat request, or a live
//! session (see `live` for how a session's differs).
//!
//! [`ToolLoop`] is a plugin over the conversation kind, so it serves every chat
//! format and provider, translated or not. It offers the model the tools of a
//! [`Tools`] runner beside the client's own, calls the provider, and while the
//! reply calls only the runner's tools it runs them (in parallel), appends the
//! calls and their results to the conversation and calls again. The client
//! sees one reply.
//!
//! It stops at a reply that calls no runner tool, which goes to the client;
//! at a reply that calls a *client* tool (the client runs its own tools; the
//! runner's calls in that reply are left out of what it sees); at
//! [`max_steps`](ToolLoop::max_steps), where the last call forbids tools and a
//! reply that still calls one is refused; and at the wall-clock
//! [`timeout`](ToolLoop::timeout).
//!
//! The reply is one stream: a step's text and client tool calls flow to the
//! client as they arrive, while the runner's tool calls are held back and run;
//! the next step's blocks follow, renumbered, in the same reply. Usage is
//! summed over the steps and sent once, before the finish.
//!
//! What the model said is replayed to it as the conversation kind holds it:
//! the text, the reasoning and the calls of a step, in order, each with the
//! fields its format kept beside it, so a signed `thinking` block of
//! Anthropic or a `thoughtSignature` of Gemini goes back in the assistant
//! message the next step starts from, and the provider accepts the step. A
//! block's fields are its [`BlockFields`], and its signature follows it.
//! Blocks only natives carry (a provider-run tool's) are not replayed.
//!
//! Native events of a step reach the client as its blocks do. A block's
//! ([`BlockFields`], [`BlockNative`]) is renumbered with it, and dropped with
//! it if the runner holds the block; one of a block no event opens (a
//! provider-run tool's) gets a number of its own. Events about the step as a
//! whole (`ping`, a stop sequence, response fields) are those of the first
//! step; the client has no use for those of the steps after.
//!
//! [`BlockFields`]: respan_gateway_conversation::Output::BlockFields
//! [`BlockNative`]: respan_gateway_conversation::Output::BlockNative
//!
//! A tool's failure is the model's to read, not the gateway's: a result with
//! `is_error` set. Only the provider's failure, a limit, or a malformed reply
//! fails the request.

mod filter;
mod live;
mod steps;
mod tools;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use respan_gateway_conversation::{Finish, Input, Tool, ToolChoice, Usage};
use respan_gateway_core::futures::StreamExt;
use respan_gateway_core::futures::stream::{self};
use respan_gateway_core::{ClientContext, Fault, Plugin, Response, async_trait};
use serde_json::Value;
use tokio::time::{Instant, timeout};

pub use filter::{ToolFilter, ToolScope, ToolSwitches};
use steps::{Phase, Run, advance};

/// What a tool said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Its answer, as the model reads it.
    pub text: String,
    /// Whether the call failed; the model sees it and may try again.
    pub is_error: bool,
}

impl Outcome {
    /// A tool's answer.
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    /// A failure, as the model reads it.
    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

/// The tools the gateway runs, such as an MCP client's. It is asked once per
/// request for that request's [`Toolbox`].
pub trait Tools: Send + Sync + 'static {
    /// The tools for the request `cx`; it may keep what it needs of `cx`,
    /// such as calls to make.
    fn open(&self, cx: &mut ClientContext) -> Arc<dyn Toolbox>;
}

/// The tools one request may use.
#[async_trait]
pub trait Toolbox: Send + Sync + 'static {
    /// The tools to offer, with names unique among themselves and unlikely to
    /// clash with a client's (such as `github__search`). A tool the client
    /// also names is not offered. Tools it cannot list are left out.
    async fn list(&self) -> Vec<Tool>;

    /// Runs the tool `name` of [`list`](Self::list) with `arguments`. A
    /// failure is an [`Outcome::error`]. Bounded by the tool loop's
    /// [`tool_timeout`](ToolLoop::tool_timeout).
    async fn call(&self, name: &str, arguments: Value) -> Outcome;
}

/// Put on every call the tool loop makes to the model, as the extension
/// `px.extensions().get::<Step>()`: the step, from 1.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Step(pub u32);

/// A mark for the calls a [`Toolbox`] makes through the gateway to run a
/// tool (put it on them with [`Call::with_extension`](respan_gateway_core::Call::with_extension)), so a plugin can tell
/// them from the request's own provider calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolHop;

/// One tool call, as it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolReport {
    /// The tool, as the model named it.
    pub name: String,
    /// How long it took.
    pub took: Duration,
    /// Whether it failed (a timeout included).
    pub is_error: bool,
    /// The bytes of its answer the model saw.
    pub bytes: usize,
}

/// One step of the loop: a call to the model and the tools it asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct StepReport {
    /// The step, from 1.
    pub index: u32,
    /// How long the model took, to the end of its reply.
    pub took: Duration,
    /// What the model reported for this step alone.
    pub usage: Usage,
    /// Why the model stopped, if it said.
    pub finish: Option<Finish>,
    /// The tools it asked the gateway to run, in the order it asked.
    pub tools: Vec<ToolReport>,
    /// How long running them took, together.
    pub tools_took: Duration,
}

/// Where a request's steps are reported.
pub type Sink = Arc<dyn Fn(StepReport) + Send + Sync>;

type Scope = dyn Fn(&ClientContext, Option<&str>) -> bool + Send + Sync;
type Flags = dyn Fn(&str) -> bool + Send + Sync;
type Observe = dyn Fn(&ClientContext) -> Option<Sink> + Send + Sync;

#[derive(Debug, Clone, Copy)]
struct Limits {
    max_steps: u32,
    timeout: Duration,
    tool_timeout: Duration,
    max_result: usize,
    parallel: usize,
    max_results: usize,
}

/// The plugin: see the [module](self). Register it after the plugins that
/// route and edit requests, since it answers the request itself.
pub struct ToolLoop {
    tools: Arc<dyn Tools>,
    limits: Limits,
    scope: Option<Arc<Scope>>,
    observe: Option<Arc<Observe>>,
    flags: Option<Arc<Flags>>,
}

impl std::fmt::Debug for ToolLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolLoop")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl ToolLoop {
    /// A tool loop running `tools` for every chat request that has a target:
    /// 8 steps, 5 minutes, 60 seconds a tool, 64 KiB an answer, 4 calls at
    /// once, 256 KiB of answers in all.
    pub fn new(tools: impl Tools) -> Self {
        Self {
            tools: Arc::new(tools),
            limits: Limits {
                max_steps: 8,
                timeout: Duration::from_secs(300),
                tool_timeout: Duration::from_secs(60),
                max_result: 64 << 10,
                parallel: 4,
                max_results: 256 << 10,
            },
            scope: None,
            observe: None,
            flags: None,
        }
    }

    /// At most `steps` calls to the model per request (at least 1). The last
    /// of several forbids tools; a reply that still calls the runner's fails
    /// the request.
    #[must_use]
    pub fn max_steps(mut self, steps: u32) -> Self {
        self.limits.max_steps = steps.max(1);
        self
    }

    /// The whole loop's wall-clock limit; past it the request fails with a 504
    /// (`tool_loop_timeout`).
    #[must_use]
    pub fn timeout(mut self, limit: Duration) -> Self {
        self.limits.timeout = limit;
        self
    }

    /// The limit of one tool call; past it the model gets an error result.
    #[must_use]
    pub fn tool_timeout(mut self, limit: Duration) -> Self {
        self.limits.tool_timeout = limit;
        self
    }

    /// The most bytes of one tool's answer the model sees; the rest is cut
    /// off, saying so.
    #[must_use]
    pub fn max_result(mut self, bytes: usize) -> Self {
        self.limits.max_result = bytes;
        self
    }

    /// At most `calls` tool calls of one step run at once (at least 1); the
    /// rest wait their turn. The answers keep the order asked.
    #[must_use]
    pub fn parallel(mut self, calls: usize) -> Self {
        self.limits.parallel = calls.max(1);
        self
    }

    /// The most bytes of tool answers the model sees in one request, over
    /// all its steps; once spent, a call is answered that its result was
    /// left out. Tool output is untrusted model input, and this bounds how
    /// much of it one request can pile into the conversation.
    #[must_use]
    pub fn max_results(mut self, bytes: usize) -> Self {
        self.limits.max_results = bytes;
        self
    }

    /// Runs the loop only for requests `scope` accepts, which see the
    /// request and the model it names, as routing left them: a chat's, or
    /// `None` for a live session, which names it in its inputs (a host
    /// that put it in the request's extensions reads it there). Others
    /// pass.
    #[must_use]
    pub fn when(
        mut self,
        scope: impl Fn(&ClientContext, Option<&str>) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.scope = Some(Arc::new(scope));
        self
    }

    /// Says which providers can be sent a tool result flagged as a failure.
    /// Most formats have no such flag, and refuse a flagged result rather than
    /// lose it; so by default a failed result is sent as text beginning
    /// `Error: `, and flagged as well only for a provider `flags` accepts.
    #[must_use]
    pub fn flag_errors_for(mut self, flags: impl Fn(&str) -> bool + Send + Sync + 'static) -> Self {
        self.flags = Some(Arc::new(flags));
        self
    }

    /// Reports each step of a request to the sink `observe` gives for it.
    #[must_use]
    pub fn observe(
        mut self,
        observe: impl Fn(&ClientContext) -> Option<Sink> + Send + Sync + 'static,
    ) -> Self {
        self.observe = Some(Arc::new(observe));
        self
    }
}

impl ToolLoop {
    /// Runs the loop over a live session, if it has tools to offer.
    async fn live(&self, cx: &mut ClientContext) -> Result<(), Fault> {
        if !self.scope.as_ref().is_none_or(|scope| scope(cx, None)) {
            return Ok(());
        }
        let toolbox = self.tools.open(cx);
        let listed = timeout(self.limits.tool_timeout, toolbox.list()).await;
        let offered = listed.unwrap_or_default();
        if offered.is_empty() {
            return Ok(());
        }
        live::run(cx, toolbox, offered, self.limits)
    }
}

#[async_trait]
impl Plugin for ToolLoop {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        if cx.is_kind::<respan_gateway_realtime::Input>() {
            return self.live(cx).await.map(|()| None);
        }
        if !cx.is_kind::<Input>() {
            return Ok(None);
        }
        let Ok(input) = cx.input::<Input>().cloned() else {
            return Ok(None);
        };
        let Some(target) = cx.target().map(str::to_owned) else {
            return Ok(None);
        };
        let no_tools = input.options.tool_choice == Some(ToolChoice::None);
        let in_scope = (self.scope.as_ref()).is_none_or(|scope| scope(cx, input.model()));
        if no_tools || !in_scope {
            return Ok(None);
        }
        let toolbox = self.tools.open(cx);
        let listed = timeout(self.limits.tool_timeout, toolbox.list()).await;
        let (mut input, mut gateway) = (input, HashSet::new());
        let client: HashSet<String> = input.tools.iter().map(|t| t.name.clone()).collect();
        for tool in listed.unwrap_or_default() {
            if !client.contains(&tool.name) && gateway.insert(tool.name.clone()) {
                input.tools.push(tool);
            }
        }
        if gateway.is_empty() {
            return Ok(None);
        }
        let flagged = self.flags.as_ref().is_some_and(|flags| flags(&target));
        let run = Run {
            flagged,
            call: cx.call(target),
            toolbox,
            gateway,
            input,
            limits: self.limits,
            deadline: Instant::now() + self.limits.timeout,
            report: self.observe.as_ref().and_then(|observe| observe(cx)),
            steps: 0,
            next_block: 0,
            usage: Usage::default(),
            budget: self.limits.max_results,
            phase: Phase::Open,
        };
        let events = stream::unfold(run, advance).flat_map(stream::iter);
        Ok(Some(Response::stream(events)))
    }
}

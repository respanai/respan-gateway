//! The loop of one request: a step's reply read as it streams, and the
//! steps that follow it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use respan_gateway_conversation::{
    Assembled, Block, Content, Finish, Input, Message, Output, Role, ToolChoice, Usage,
};
use respan_gateway_core::futures::StreamExt;
use respan_gateway_core::futures::stream::BoxStream;
use respan_gateway_core::{Call, ErrorCode, Fault, Native};
use respan_gateway_http_faults::Status;
use tokio::time::{Instant, timeout_at};

use super::tools::{limit, run_tools, spend};
use super::{Limits, Outcome, Sink, Step, StepReport, ToolReport, Toolbox};

pub(super) struct Run {
    /// Whether the provider takes a tool result flagged as a failure.
    pub(super) flagged: bool,
    pub(super) call: Call,
    pub(super) toolbox: Arc<dyn Toolbox>,
    /// The names of the tools the runner added.
    pub(super) gateway: HashSet<String>,
    /// The names of the client's tools, which stay its own.
    pub(super) client: HashSet<String>,
    /// The conversation so far, with the runner's tools.
    pub(super) input: Input,
    pub(super) limits: Limits,
    pub(super) deadline: Instant,
    pub(super) report: Option<Sink>,
    /// The calls to the model made.
    pub(super) steps: u32,
    /// The number the next step's first block gets.
    pub(super) next_block: u32,
    /// The usage of the steps that ended.
    pub(super) usage: Usage,
    /// The bytes of tool answers the request may still add.
    pub(super) budget: usize,
    pub(super) phase: Phase,
}

pub(super) enum Phase {
    /// The next step is yet to be called.
    Open,
    Reading(Box<Reading>),
    Done,
}

/// A step's reply, as it is read.
pub(super) struct Reading {
    pub(super) events: BoxStream<'static, Result<Output, Fault>>,
    /// What the model said, whole, for the conversation.
    pub(super) said: Assembled,
    /// The blocks of the runner's tool calls, which the client sees only once
    /// they ran, each with its result ([`shown`]).
    pub(super) held: HashSet<u32>,
    /// Whether the model called a tool the client must run.
    pub(super) client_calls: bool,
    /// The number each block the client sees has in its reply, where blocks
    /// are numbered from 0 without gaps, over all the steps.
    pub(super) numbers: HashMap<u32, u32>,
    /// The number the next block the client sees gets.
    pub(super) next: u32,
    /// The fields of a block said before it opens, until it does.
    pub(super) pending: HashMap<u32, Vec<Native>>,
    pub(super) finish: Option<Finish>,
    pub(super) usage: Usage,
    pub(super) started: Instant,
}

pub(super) type Chunk = Vec<Result<Output, Fault>>;

/// The next events for the client, until the loop is done.
pub(super) async fn advance(mut run: Run) -> Option<(Chunk, Run)> {
    loop {
        match std::mem::replace(&mut run.phase, Phase::Done) {
            Phase::Done => return None,
            Phase::Open => match run.open().await {
                Ok(reading) => run.phase = Phase::Reading(Box::new(reading)),
                Err(fault) => return Some((vec![Err(fault)], run)),
            },
            Phase::Reading(mut reading) => {
                let next = timeout_at(run.deadline, reading.events.next()).await;
                let event = match next {
                    Err(_) => return Some((vec![Err(run.timed_out())], run)),
                    Ok(Some(Err(fault))) => return Some((vec![Err(fault)], run)),
                    Ok(Some(Ok(event))) => event,
                    Ok(None) => match run.end_step(*reading).await {
                        Err(fault) => return Some((vec![Err(fault)], run)),
                        Ok(out) => return Some((out, run)),
                    },
                };
                let first = run.steps == 1;
                match reading.take(event, first, &|name| run.is_gateway(name)) {
                    Err(fault) => return Some((vec![Err(fault)], run)),
                    Ok(out) => {
                        run.phase = Phase::Reading(reading);
                        if !out.is_empty() {
                            return Some((out.into_iter().map(Ok).collect(), run));
                        }
                    }
                }
            }
        }
    }
}

impl Reading {
    /// The number the client's reply gives the provider's `block`.
    fn number(&mut self, block: u32) -> u32 {
        let next = &mut self.next;
        *self.numbers.entry(block).or_insert_with(|| {
            *next += 1;
            *next - 1
        })
    }

    /// What the client is sent for `event`, of the step's reply. The runner's
    /// tool calls, usage and the finish are held: they are decided at the
    /// step's end.
    fn take(
        &mut self,
        event: Output,
        first: bool,
        is_gateway: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<Output>, Fault> {
        self.said.push(&event)?;
        Ok(match event {
            Output::Start(start) if first => vec![Output::Start(start)],
            Output::Start(_) => Vec::new(),
            Output::BlockStart { block, kind } => {
                let pending = self.pending.remove(&block).unwrap_or_default();
                if let Block::ToolCall { name, .. } = &kind {
                    if is_gateway(name) {
                        self.held.insert(block);
                        return Ok(Vec::new());
                    }
                    self.client_calls = true;
                }
                let number = self.number(block);
                let mut out: Vec<Output> = pending
                    .into_iter()
                    .map(|native| Output::block_fields(number, native))
                    .collect();
                out.push(Output::block_start(number, kind));
                out
            }
            Output::Delta { block, delta } if !self.held.contains(&block) => {
                let block = self.numbers.get(&block).copied().unwrap_or(block);
                vec![Output::Delta { block, delta }]
            }
            Output::BlockEnd { block } if !self.held.contains(&block) => {
                let block = self.numbers.get(&block).copied().unwrap_or(block);
                vec![Output::block_end(block)]
            }
            // A signature goes with its block: renumbered, or held with it.
            Output::Signature { block, data } if block.is_none_or(|b| !self.held.contains(&b)) => {
                let block = block.map(|b| self.numbers.get(&b).copied().unwrap_or(b));
                vec![Output::signature(block, data)]
            }
            Output::Delta { .. } | Output::BlockEnd { .. } | Output::Signature { .. } => Vec::new(),
            Output::Usage(usage) => {
                self.usage += usage;
                Vec::new()
            }
            Output::Finish(finish) => {
                self.finish = Some(finish);
                Vec::new()
            }
            Output::BlockFields { block, .. } | Output::BlockNative { block, .. }
                if self.held.contains(&block) =>
            {
                Vec::new()
            }
            // Fields of a block that is yet to open.
            Output::BlockFields { block, native } if !self.numbers.contains_key(&block) => {
                self.pending.entry(block).or_default().push(native);
                Vec::new()
            }
            Output::BlockFields { block, native } => {
                vec![Output::block_fields(self.number(block), native)]
            }
            // Of a block that is open, or one only such events carry.
            Output::BlockNative { block, native } => {
                vec![Output::block_native(self.number(block), native)]
            }
            Output::Native(native) if first => vec![Output::Native(native)],
            Output::Native(_) => Vec::new(),
            // An event a later kind adds is the client's to see.
            other => vec![other],
        })
    }
}

impl Run {
    /// Whether a call of `name` is the runner's to run: a tool it offered,
    /// or one it runs that the client has none of.
    fn is_gateway(&self, name: &str) -> bool {
        self.gateway.contains(name) || (!self.client.contains(name) && self.toolbox.runs(name))
    }

    fn timed_out(&self) -> Fault {
        let message = format!(
            "the tool_loop loop ran past its {} s limit",
            self.limits.timeout.as_secs()
        );
        Fault::new(message)
            .with(ErrorCode("tool_loop_timeout".into()))
            .with(Status(
                respan_gateway_http::http::StatusCode::GATEWAY_TIMEOUT,
            ))
    }

    /// Calls the model for the next step.
    async fn open(&mut self) -> Result<Reading, Fault> {
        if Instant::now() >= self.deadline {
            return Err(self.timed_out());
        }
        self.steps += 1;
        let started = Instant::now();
        let mut input = self.input.clone();
        if self.steps == self.limits.max_steps && self.steps > 1 {
            input.options.tool_choice = Some(ToolChoice::None);
        }
        let call = self.call.clone().with_input(input);
        let call = call.with_extension(Step(self.steps));
        let response = match timeout_at(self.deadline, call).await {
            Ok(response) => response?,
            Err(_) => return Err(self.timed_out()),
        };
        Ok(Reading {
            events: response.outputs::<Output>(),
            said: Assembled::default(),
            held: HashSet::new(),
            client_calls: false,
            numbers: HashMap::new(),
            next: self.next_block,
            pending: HashMap::new(),
            finish: None,
            usage: Usage::default(),
            started,
        })
    }

    /// A step's reply ended: the last events for the client if the loop is
    /// done, else, once the tools ran and the next step is ready, each call
    /// the runner ran followed by its result.
    async fn end_step(&mut self, reading: Reading) -> Result<Chunk, Fault> {
        self.usage += reading.usage;
        let calls: Vec<(String, String, String)> = reading
            .said
            .blocks()
            .filter_map(|b| match &b.kind {
                Block::ToolCall { id, name } if self.is_gateway(name) => {
                    Some((id.clone(), name.clone(), b.text.clone()))
                }
                _ => None,
            })
            .collect();
        let complete = matches!(
            reading.finish,
            None | Some(Finish::ToolUse) | Some(Finish::Stop)
        );
        let goes_on = !calls.is_empty() && !reading.client_calls && complete;
        let took = reading.started.elapsed();
        if !goes_on {
            self.say(StepReport {
                index: self.steps,
                took,
                usage: reading.usage,
                finish: reading.finish.clone(),
                tools: Vec::new(),
                tools_took: Duration::ZERO,
            });
            let mut last = Vec::new();
            if !self.usage.is_empty() {
                last.push(Ok(Output::Usage(self.usage)));
            }
            last.extend(reading.finish.map(|finish| Ok(Output::Finish(finish))));
            return Ok(last);
        }
        if self.steps >= self.limits.max_steps {
            let message = format!(
                "the model was still calling tools after {} steps",
                self.limits.max_steps
            );
            return Err(limit("tool_loop_max_steps", message));
        }
        let ran = Instant::now();
        let asked = calls.clone();
        let mut results = run_tools(&self.toolbox, self.limits, self.deadline, calls).await;
        spend(&mut self.budget, &mut results);
        let (shown, next_block) = shown(reading.next, &asked, &results);
        let mut said = reading.said.replay();
        let mut assistant = Message::new(Role::Assistant, said.remove(0));
        assistant.content.append(&mut said);
        let flagged = self.flagged;
        let mut answers = results.iter().map(|(id, outcome, _)| {
            let failed = outcome.is_error;
            let text = match (failed, flagged) {
                (true, false) => format!("Error: {}", outcome.text),
                _ => outcome.text.clone(),
            };
            Content::tool_result(id, vec![Content::from(text)], failed && flagged)
        });
        let mut user = Message::new(
            Role::User,
            answers.next().unwrap_or_else(|| Content::from("")),
        );
        user.content.extend(answers);
        self.input.messages.push(assistant);
        self.input.messages.push(user);
        // A forced call was for the first word; after it the model chooses.
        if matches!(
            self.input.options.tool_choice,
            Some(ToolChoice::Required | ToolChoice::Tool(_))
        ) {
            self.input.options.tool_choice = Some(ToolChoice::Auto);
        }
        self.next_block = next_block;
        self.say(StepReport {
            index: self.steps,
            took,
            usage: reading.usage,
            finish: reading.finish,
            tools: results.into_iter().map(|(_, _, report)| report).collect(),
            tools_took: ran.elapsed(),
        });
        self.phase = Phase::Open;
        Ok(shown.into_iter().map(Ok).collect())
    }

    fn say(&self, report: StepReport) {
        if let Some(sink) = &self.report {
            sink(report);
        }
    }
}

/// What the client is shown of the calls `asked` that ran, answering
/// `results`, numbered from `first`: each call ([`Block::GatewayCall`]),
/// then its result ([`Block::ToolResult`]) as the model read it, a failure's
/// without the `Error: ` its flag says; and the number the next block gets.
fn shown(
    first: u32,
    asked: &[(String, String, String)],
    results: &[(String, Outcome, ToolReport)],
) -> (Vec<Output>, u32) {
    let mut out = Vec::new();
    let mut block = first;
    for ((id, name, arguments), (_, outcome, _)) in asked.iter().zip(results) {
        let call = Block::GatewayCall {
            id: id.clone(),
            name: name.clone(),
        };
        let arguments = Some(arguments.as_str()).filter(|a| !a.trim().is_empty());
        out.push(Output::block_start(block, call));
        out.push(Output::arguments(block, arguments.unwrap_or("{}")));
        out.push(Output::block_end(block));
        let result = Block::ToolResult {
            id: id.clone(),
            is_error: outcome.is_error,
        };
        out.push(Output::block_start(block + 1, result));
        if !outcome.text.is_empty() {
            out.push(Output::result(block + 1, outcome.text.as_str()));
        }
        out.push(Output::block_end(block + 1));
        block += 2;
    }
    (out, block)
}

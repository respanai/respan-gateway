//! The tool loop over a live session (the realtime kind): the runner's
//! tools are offered in each configuration of the session that sets tools,
//! and the model's calls of them are run by the gateway while the session
//! goes on, their answers sent back to the model, which is then asked to
//! respond. The client sees neither the calls nor their answers.
//!
//! A response that calls the runner's tools ends as it came, saying
//! nothing more; once its calls are answered, and no other response is
//! being made, the model is asked for the next. A response the user spoke
//! over, or one that also called a client's tool (the client answers it,
//! and asks for the next itself), gets its calls answered but asks for
//! none.
//!
//! What a format says of a call only in its own fields (OpenAI's items of
//! a function call, its argument deltas, the call in `response.done`) is
//! left out with the call, by the format, as the loop cannot read them
//! ([`Calls`]).

use parking_lot::Mutex;
use std::sync::Arc;

use respan_gateway_core::futures::channel::mpsc;
use respan_gateway_core::futures::future::ready;
use respan_gateway_core::futures::stream::{self, StreamExt};
use respan_gateway_core::{ClientContext, EventStream, Fault, Native};
use respan_gateway_realtime::{Input, Output, Session, Status, Tool, ToolResult};
use respan_gateway_realtime_ws::Calls;
use tokio::time::Instant;

use super::tools::{run_tools, spend};
use super::{Limits, Toolbox};

/// What the session's inputs carry besides the client's.
enum Job {
    /// Run the runner's tool `name` for call `id`.
    Run {
        id: String,
        name: String,
        arguments: String,
    },
    /// Ask the model to respond.
    Respond,
}

/// What the session's two directions share.
struct State {
    /// The runner's tools, by the name the model has, but those the client
    /// names too.
    offered: Vec<Tool>,
    /// Their names, and the ids of their calls.
    calls: Calls,
    /// Calls being run.
    running: usize,
    /// A response is being made.
    active: bool,
    /// The response being made called the runner's tools, or a client's.
    called: bool,
    client_called: bool,
    /// The model is asked to respond once the calls are answered.
    respond: bool,
    /// What is left for tools' answers in the session.
    budget: usize,
    /// No configuration came yet.
    first: bool,
    /// The start of the response being made, held until it says something.
    start: Option<Output>,
    /// A response was not seen, and the next has not started: what a format
    /// says of its end only in its own events (Gemini's `turnComplete`
    /// after a call) is not seen either.
    unseen: bool,
}

/// Runs the loop over the live session of `cx`, offering `offered`.
pub(super) fn run(
    cx: &mut ClientContext,
    toolbox: Arc<dyn Toolbox>,
    offered: Vec<Tool>,
    limits: Limits,
) -> Result<(), Fault> {
    let names = offered.iter().map(|t| t.name.clone()).collect();
    let state = Arc::new(Mutex::new(State {
        offered,
        calls: Calls {
            names,
            ids: Default::default(),
        },
        running: 0,
        active: false,
        called: false,
        client_called: false,
        respond: false,
        budget: limits.max_results,
        first: true,
        start: None,
        unseen: false,
    }));
    let (jobs, queued) = mpsc::unbounded::<Job>();
    let shared = state.clone();
    cx.wrap(move |inputs: EventStream<Input>| {
        let offering = shared.clone();
        let client = inputs.map(move |input| {
            let mut input = input?;
            if let Input::Session(session) = &mut input {
                offer(&mut offering.lock(), session);
            }
            Ok(Some(input))
        });
        // Ends with the client's inputs, whatever calls are left.
        let client = client.chain(stream::once(ready(Ok(None))));
        let answers = queued
            .map(move |job| answer(job, toolbox.clone(), limits, shared.clone()))
            .buffer_unordered(limits.parallel)
            .flat_map(stream::iter)
            .map(|input| Ok(Some(input)));
        stream::select(client, answers)
            .take_while(|input| ready(!matches!(input, Ok(None))))
            .filter_map(|input| ready(input.transpose()))
    })?;
    cx.wrap(move |outputs: EventStream<Output>| {
        outputs.flat_map(move |output| {
            stream::iter(match output {
                Ok(output) => shown(&mut state.lock(), &jobs, output)
                    .into_iter()
                    .map(Ok)
                    .collect(),
                Err(fault) => vec![Err(fault)],
            })
        })
    })
}

/// Offers the runner's tools in a configuration that sets tools, and in the
/// first, which sets the provider's default of none.
fn offer(state: &mut State, session: &mut Session) {
    let first = std::mem::replace(&mut state.first, false);
    let tools = match &mut session.tools {
        Some(tools) => tools,
        None if first => session.tools.insert(Vec::new()),
        None => return,
    };
    // A tool the client names is the client's.
    for tool in tools.iter() {
        state.calls.names.remove(&tool.name);
    }
    let names = &state.calls.names;
    tools.extend(
        state
            .offered
            .iter()
            .filter(|t| names.contains(&t.name))
            .cloned(),
    );
}

/// What the client sees of `output`. A response's start waits for what it
/// says (its usage aside), so a response that says nothing but calls of
/// the runner's tools is not seen at all: the one that answers them is.
fn shown(state: &mut State, jobs: &mpsc::UnboundedSender<Job>, output: Output) -> Vec<Output> {
    let starts = matches!(output, Output::ResponseStart { .. });
    let ends = matches!(output, Output::ResponseDone { .. });
    let called = state.called;
    match &output {
        Output::ResponseStart { .. } => state.unseen = false,
        Output::Native(_) if state.unseen => return Vec::new(),
        _ => {}
    }
    let Some(output) = see(state, jobs, output) else {
        return Vec::new();
    };
    if starts {
        state.start = Some(output);
        return Vec::new();
    }
    if matches!(output, Output::Usage(_)) {
        return vec![output];
    }
    match state.start.take() {
        Some(_) if ends && called => {
            state.unseen = true;
            Vec::new()
        }
        Some(start) => vec![start, output],
        None => vec![output],
    }
}

/// What the client may see of `output`, if anything; a call of the
/// runner's tools is run.
fn see(state: &mut State, jobs: &mpsc::UnboundedSender<Job>, mut output: Output) -> Option<Output> {
    match &mut output {
        Output::Opened(session) | Output::Updated(session) => {
            if let Some(tools) = &mut session.tools {
                tools.retain(|t| !state.calls.names.contains(&t.name));
            }
        }
        Output::ResponseStart { native, .. } => {
            state.active = true;
            (state.called, state.client_called) = (false, false);
            state.calls.fields(native);
        }
        Output::ToolCall {
            id,
            name,
            arguments,
            ..
        } if state.calls.names.contains(name.as_str()) => {
            state.calls.ids.insert(id.clone());
            state.running += 1;
            state.called = true;
            let job = Job::Run {
                id: std::mem::take(id),
                name: std::mem::take(name),
                arguments: std::mem::take(arguments),
            };
            let _ = jobs.unbounded_send(job);
            return None;
        }
        Output::ToolCall { .. } => state.client_called = true,
        Output::ResponseDone { status, native } => {
            state.active = false;
            if std::mem::take(&mut state.called) {
                state.respond = *status == Status::Completed && !state.client_called;
            }
            if state.respond && state.running == 0 {
                state.respond = false;
                let _ = jobs.unbounded_send(Job::Respond);
            }
            state.calls.fields(native);
        }
        Output::Native(native) => {
            if state.calls.event(native) {
                return None;
            }
        }
        _ => {}
    }
    Some(output)
}

/// What the model is sent for `job`.
async fn answer(
    job: Job,
    toolbox: Arc<dyn Toolbox>,
    limits: Limits,
    state: Arc<Mutex<State>>,
) -> Vec<Input> {
    let (id, name, arguments) = match job {
        Job::Respond => {
            return vec![Input::Respond {
                native: Native::default(),
            }];
        }
        Job::Run {
            id,
            name,
            arguments,
        } => (id, name, arguments),
    };
    let deadline = Instant::now() + limits.tool_timeout;
    let mut results = run_tools(&toolbox, limits, deadline, vec![(id, name, arguments)]).await;
    let mut state = state.lock();
    spend(&mut state.budget, &mut results);
    let mut sent = Vec::new();
    for (id, outcome, _) in results {
        let output = match outcome.is_error {
            true => format!("Error: {}", outcome.text),
            false => outcome.text,
        };
        sent.push(Input::ToolResult(ToolResult::new(id, output)));
    }
    state.running = state.running.saturating_sub(1);
    if state.running == 0 && state.respond && !state.active {
        state.respond = false;
        sent.push(Input::Respond {
            native: Native::default(),
        });
    }
    sent
}

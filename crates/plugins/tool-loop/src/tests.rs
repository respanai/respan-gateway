//! A chat request's loop against a scripted model, as its client sees it.

use std::collections::VecDeque;
use std::sync::Arc;

use parking_lot::Mutex;
use respan_gateway_conversation::{
    Assembled, Block, Content, Finish, Input, Message, Output, Role, Tool, Usage,
};
use respan_gateway_core::futures::channel::{mpsc, oneshot};
use respan_gateway_core::futures::{FutureExt, SinkExt, StreamExt, sink, stream};
use respan_gateway_core::{
    ClientContext, Connection, EventSink, EventStream, Extensions, Fault, Format, Gateway,
    Provider, async_trait,
};
use serde_json::{Value, json};

use super::{Outcome, ToolLoop, Toolbox, Tools};

/// A model that answers each call with the next of its replies, and keeps
/// what each call sent it.
#[derive(Default)]
struct Script {
    replies: Mutex<VecDeque<Vec<Output>>>,
    sent: Mutex<Vec<Input>>,
}

/// A provider of the kind itself: its transport is the model.
struct Scripted;

impl Format<Arc<Script>, Output, Input> for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }

    fn bind(&self, script: Arc<Script>) -> Result<Connection<Output, Input>, Fault> {
        let (send, receive) = oneshot::channel();
        let incoming = async move { receive.await.map_err(|_| Fault::abandoned()) };
        let outgoing = EventSink::preparing(move |flow: &mut EventStream<Input>| {
            let input = flow.value().transpose()?.cloned();
            let input = input.ok_or_else(|| Fault::bad_request("no conversation"))?;
            script.sent.lock().push(input);
            let reply = script.replies.lock().pop_front().unwrap_or_default();
            let outputs = EventStream::stream(stream::iter(reply.into_iter().map(Ok)));
            Ok(sink::unfold(
                Some((send, outputs)),
                |state, item: Result<Input, Fault>| async move {
                    item?;
                    if let Some((send, outputs)) = state {
                        let _ = send.send(outputs);
                    }
                    Ok::<_, Fault>(None)
                },
            ))
        });
        Ok((incoming.boxed(), outgoing))
    }
}

/// A client of the kind itself: its request, and where its reply's events go.
type Asking = (Input, mpsc::UnboundedSender<Result<Output, Fault>>);

struct Client;

impl Format<Asking, Input, Output> for Client {
    fn name(&self) -> &str {
        "client"
    }

    fn matches(&self, _: &Asking) -> bool {
        true
    }

    fn bind(&self, (input, replies): Asking) -> Result<Connection<Input, Output>, Fault> {
        let incoming = async move { Ok(EventStream::single(input)) }.boxed();
        let outgoing = EventSink::new(replies.sink_map_err(|_| Fault::abandoned()));
        Ok((incoming, outgoing))
    }
}

/// The gateway's tools: `gh__search` answers what it is asked, `fs__read`
/// fails, and `gh__issue`, not offered, it runs when called, as a router
/// runs the tools it routes to.
struct Runner;

#[async_trait]
impl Toolbox for Runner {
    async fn list(&self) -> Vec<Tool> {
        let tool = |name| Tool::new(name, json!({ "type": "object" }));
        vec![tool("gh__search"), tool("fs__read")]
    }

    async fn call(&self, name: &str, arguments: Value) -> Outcome {
        match name {
            "gh__search" => Outcome::ok(format!("found {}", arguments["q"].as_str().unwrap_or(""))),
            "gh__issue" => Outcome::ok("issue 7"),
            _ => Outcome::error("no access"),
        }
    }

    fn runs(&self, name: &str) -> bool {
        name == "gh__issue"
    }
}

struct Offer;

impl Tools for Offer {
    fn open(&self, _: &mut ClientContext) -> Arc<dyn Toolbox> {
        Arc::new(Runner)
    }
}

/// What the client gets for `input` when the model answers `replies` in
/// turn, and what each call sent the model.
async fn asked(input: Input, replies: Vec<Vec<Output>>) -> (Vec<Output>, Vec<Input>) {
    let script = Arc::new(Script::default());
    script.replies.lock().extend(replies);
    let model = Arc::clone(&script);
    let gateway = Gateway::new()
        .provider(Provider::new("model", Scripted, move |_| {
            Arc::clone(&model)
        }))
        .and_then(|g| g.client::<Input, Output, _>(Client))
        .and_then(|g| g.target("model"))
        .expect("a gateway")
        .plugin(ToolLoop::new(Offer));
    let (sent, received) = mpsc::unbounded();
    let asking: Asking = (input, sent);
    let answered = gateway.handle(asking, Extensions::default()).await;
    answered.expect("answered");
    let events: Vec<Output> = received.map(|e| e.expect("an event")).collect().await;
    let calls = script.sent.lock().clone();
    (events, calls)
}

fn question(text: &str) -> Input {
    let mut input = Input::new([Message::new(Role::User, text)]);
    input.model = Some("m".into());
    input
}

fn text(block: u32, text: &str) -> [Output; 3] {
    [
        Output::block_start(block, Block::Text),
        Output::text(block, text),
        Output::block_end(block),
    ]
}

fn call(block: u32, id: &str, name: &str, arguments: &str) -> [Output; 3] {
    let kind = Block::ToolCall {
        id: id.into(),
        name: name.into(),
    };
    [
        Output::block_start(block, kind),
        Output::arguments(block, arguments),
        Output::block_end(block),
    ]
}

/// The client's events of a call the gateway ran, numbered `block`, and of
/// its result after it.
fn ran(
    block: u32,
    id: &str,
    name: &str,
    arguments: &str,
    answer: &str,
    is_error: bool,
) -> Vec<Output> {
    let call = Block::GatewayCall {
        id: id.into(),
        name: name.into(),
    };
    let result = Block::ToolResult {
        id: id.into(),
        is_error,
    };
    vec![
        Output::block_start(block, call),
        Output::arguments(block, arguments),
        Output::block_end(block),
        Output::block_start(block + 1, result),
        Output::result(block + 1, answer),
        Output::block_end(block + 1),
    ]
}

fn reply(id: &str, blocks: &[[Output; 3]], usage: Usage, finish: Finish) -> Vec<Output> {
    let mut events = vec![Output::start(id, "m")];
    events.extend(blocks.iter().flatten().cloned());
    events.push(Output::usage(usage));
    events.push(Output::finished(finish));
    events
}

#[tokio::test]
async fn a_call_the_gateway_ran_reaches_the_client_after_it_ran_with_its_result() {
    let looked = [
        text(0, "Looking."),
        call(1, "c1", "gh__search", "{\"q\":\"rust\"}"),
    ];
    let replies = vec![
        reply("r1", &looked, Usage::new(10, 2), Finish::ToolUse),
        reply(
            "r2",
            &[text(0, "Found it.")],
            Usage::new(20, 3),
            Finish::Stop,
        ),
    ];
    let (events, calls) = asked(question("find rust"), replies).await;
    let mut expected = vec![Output::start("r1", "m")];
    expected.extend(text(0, "Looking."));
    expected.extend(ran(
        1,
        "c1",
        "gh__search",
        "{\"q\":\"rust\"}",
        "found rust",
        false,
    ));
    expected.extend(text(3, "Found it."));
    expected.push(Output::usage(Usage::new(30, 5)));
    expected.push(Output::finished(Finish::Stop));
    assert_eq!(events, expected);
    // The conversation the client's reply holds is the one the model was
    // sent: what the client sends back takes the next turn on from there.
    let mut said = Assembled::default();
    events.iter().try_for_each(|e| said.push(e)).expect("folds");
    let turns = said.messages();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].messages[1..], turns[..2]);
    assert_eq!(turns[2], Message::new(Role::Assistant, "Found it."));
}

#[tokio::test]
async fn each_call_is_followed_by_its_result_in_the_order_the_model_asked() {
    let asked_for = [
        call(0, "c1", "gh__search", "{\"q\":\"a\"}"),
        call(1, "c2", "fs__read", ""),
    ];
    let replies = vec![
        reply("r1", &asked_for, Usage::new(1, 1), Finish::ToolUse),
        reply("r2", &[text(0, "ok")], Usage::new(1, 1), Finish::Stop),
    ];
    let (events, calls) = asked(question("look"), replies).await;
    let mut expected = vec![Output::start("r1", "m")];
    expected.extend(ran(
        0,
        "c1",
        "gh__search",
        "{\"q\":\"a\"}",
        "found a",
        false,
    ));
    expected.extend(ran(2, "c2", "fs__read", "{}", "no access", true));
    expected.extend(text(4, "ok"));
    expected.push(Output::usage(Usage::new(2, 2)));
    expected.push(Output::finished(Finish::Stop));
    assert_eq!(events, expected);
    // The model read the failure as text, as its provider takes no flag.
    let results = &calls[1].messages[2].content;
    let answer = |id: &str, text: &str| Content::tool_result(id, vec![Content::from(text)], false);
    assert_eq!(
        results[..],
        [answer("c1", "found a"), answer("c2", "Error: no access")]
    );
}

#[tokio::test]
async fn the_gateways_calls_beside_a_clients_are_left_out() {
    let mut input = question("both");
    input
        .tools
        .push(Tool::new("mine", json!({ "type": "object" })));
    let both = [
        call(0, "c1", "gh__search", "{\"q\":\"a\"}"),
        call(1, "c2", "mine", "{}"),
    ];
    let replies = vec![reply("r1", &both, Usage::new(1, 1), Finish::ToolUse)];
    let (events, calls) = asked(input, replies).await;
    let mut expected = vec![Output::start("r1", "m")];
    expected.extend(call(0, "c2", "mine", "{}"));
    expected.push(Output::usage(Usage::new(1, 1)));
    expected.push(Output::finished(Finish::ToolUse));
    assert_eq!(events, expected);
    assert_eq!(calls.len(), 1, "the client runs its own tool first");
}

#[tokio::test]
async fn a_tool_the_toolbox_runs_but_did_not_offer_is_run() {
    let asked_for = [call(0, "c1", "gh__issue", "{\"n\":7}")];
    let replies = vec![
        reply("r1", &asked_for, Usage::new(1, 1), Finish::ToolUse),
        reply("r2", &[text(0, "ok")], Usage::new(1, 1), Finish::Stop),
    ];
    let (events, calls) = asked(question("issue 7"), replies).await;
    let mut expected = vec![Output::start("r1", "m")];
    expected.extend(ran(0, "c1", "gh__issue", "{\"n\":7}", "issue 7", false));
    expected.extend(text(2, "ok"));
    expected.push(Output::usage(Usage::new(2, 2)));
    expected.push(Output::finished(Finish::Stop));
    assert_eq!(events, expected);
    // It is run, not offered.
    let offered: Vec<&str> = calls[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(offered, ["gh__search", "fs__read"]);
    // A tool of that name the client has stays the client's.
    let mut input = question("issue 7");
    input
        .tools
        .push(Tool::new("gh__issue", json!({ "type": "object" })));
    let replies = vec![reply("r1", &asked_for, Usage::new(1, 1), Finish::ToolUse)];
    let (events, calls) = asked(input, replies).await;
    let mut expected = vec![Output::start("r1", "m")];
    expected.extend(call(0, "c1", "gh__issue", "{\"n\":7}"));
    expected.push(Output::usage(Usage::new(1, 1)));
    expected.push(Output::finished(Finish::ToolUse));
    assert_eq!(events, expected);
    assert_eq!(calls.len(), 1);
}

#[tokio::test]
async fn the_steps_shown_number_their_blocks_on_from_each_other() {
    let replies = vec![
        reply(
            "r1",
            &[call(0, "c1", "gh__search", "{\"q\":\"a\"}")],
            Usage::new(1, 1),
            Finish::ToolUse,
        ),
        reply(
            "r2",
            &[
                text(0, "Then:"),
                call(1, "c2", "gh__search", "{\"q\":\"b\"}"),
            ],
            Usage::new(1, 1),
            Finish::ToolUse,
        ),
        reply("r3", &[text(0, "Both.")], Usage::new(1, 1), Finish::Stop),
    ];
    let (events, calls) = asked(question("a, then b"), replies).await;
    let mut expected = vec![Output::start("r1", "m")];
    expected.extend(ran(
        0,
        "c1",
        "gh__search",
        "{\"q\":\"a\"}",
        "found a",
        false,
    ));
    expected.extend(text(2, "Then:"));
    expected.extend(ran(
        3,
        "c2",
        "gh__search",
        "{\"q\":\"b\"}",
        "found b",
        false,
    ));
    expected.extend(text(5, "Both."));
    expected.push(Output::usage(Usage::new(3, 3)));
    expected.push(Output::finished(Finish::Stop));
    assert_eq!(events, expected);
    // The client's reply, read as messages, is the conversation the last
    // step was sent, and its answer.
    let mut said = Assembled::default();
    events.iter().try_for_each(|e| said.push(e)).expect("folds");
    let turns = said.messages();
    assert_eq!(calls[2].messages[1..], turns[..4]);
    assert_eq!(turns[4], Message::new(Role::Assistant, "Both."));
}

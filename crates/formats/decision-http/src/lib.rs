#![warn(missing_docs)]
//! The decision kind over HTTP ([`Decisions`]): a `POST` to a path ending
//! in `/decisions` or `/systemone`, answered whole in JSON. A provider is
//! called at `/decisions`, unless its vendor's dialect moves the call
//! ([`respan_gateway_http::dialect`]).
//!
//! A request is `{"model", "state", "questions": {id: question}}`, each
//! question `{"type", "instructions", "criteria"}`: `noul` with criteria
//! `{"true", "false"}`, `choice` with criteria `{option: description}`,
//! `score` with criteria a list of levels. A response is
//! `{"model", "answers": {id: answer}, "usage"}`, each answer
//! `{"type", ..}`: `noul` the probability of yes, `choice` with
//! `probabilities` and `confidence`, `score` with `probabilities` by level
//! and `legend`. A question or an answer of another type, such as `number`
//! or `box`, is kept whole as the format's own and written back where it
//! was; a format that cannot ask it drops it, and refuses a request it can
//! ask none of.

use respan_gateway_conversation_http::fields::{invalid, merge_missing, rest_of, set};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::{Schema, read_provider_error};
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format, Native};
use respan_gateway_decision::{Answer, Ask, Input, Output, Question, Start, Usage, Verdict};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{self, Method, response::Parts as ResponseHead};
use respan_gateway_http::path::{path_ends_with, set_path};
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Whole, native::Json};
use respan_gateway_http::{Connect, Upstream};
use serde_json::{Map, Value, json};

mod gemini;
mod openai;

pub use gemini::GeminiDecisions;
pub use openai::OpenAiDecisions;

/// What only format `F` of this crate reads of a value: no other crate can name it.
struct Own<F>(std::marker::PhantomData<fn() -> F>);

/// A format's own natives, which code that names the format reaches.
pub trait Natives: Sized + 'static {
    /// Its own fields in `native`: none if it holds another format's.
    fn fields(native: &Native) -> &Map<String, Value> {
        native.fields::<Own<Self>>()
    }
    /// Its own fields in `native`, edited by `edit`: another format's dropped.
    fn with_fields(native: &Native, edit: impl FnOnce(&mut Map<String, Value>)) -> Native {
        native.with_fields::<Own<Self>>(edit)
    }
}
impl Natives for Decisions {}
impl Natives for OpenAiDecisions {}

const NAME: &str = "decisions";
const PATH: &str = "/decisions";
/// TypeSafe's name for the endpoint, which its clients call.
const SYSTEM_ONE_PATH: &str = "/systemone";
/// The question types the kind has a form for; any other is the format's own.
const KNOWN_TYPES: [&str; 3] = ["noul", "choice", "score"];

type Key = crate::Own<Decisions>;

/// The decisions contract, as Respan or a compatible vendor serves it: a
/// client's `POST` to a path ending in `/decisions` or `/systemone`, and a
/// provider called at `/decisions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Decisions;

impl Decisions {
    /// The format's name, `decisions`.
    pub const NAME: &'static str = NAME;
}

impl Format<ClientIo, Input, Output> for Decisions {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        let path = |p| path_ends_with(request.uri(), p);
        request.method() == Method::POST && (path(PATH) || path(SYSTEM_ONE_PATH))
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(io.with_error(refusal).bind(
            |_| {
                Box::new(|raw: Bytes, out: &mut Vec<Input>| {
                    out.push(decode_request(&raw)?);
                    Ok(())
                })
            },
            |reply| {
                let mut head = reply.head();
                let json = HeaderValue::from_static("application/json");
                head.headers.insert(CONTENT_TYPE, json);
                let request = reply.request().body().clone();
                let encoder =
                    All::new(move |e: &[Output]| encode_response(e, &Asked::new(&request)));
                Ok(reply.write(head, Box::new(encoder)))
            },
        ))
    }
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for Decisions {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for Decisions {
    fn name(&self) -> &str {
        NAME
    }

    /// A call: a `POST` of the whole request to `/decisions`.
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(io.with_refusal(read_provider_error).bind(
            move |head, input: Option<&Input>| {
                let input = input.ok_or_else(|| {
                    Fault::config(format!("{NAME} does not accept streaming inputs"))
                })?;
                head.method = Method::POST;
                set_path(head, PATH)?;
                let json = HeaderValue::from_static("application/json");
                head.headers.insert(CONTENT_TYPE, json);
                let body = to_bytes(&encode_request(input)?)?;
                Ok(Box::new(move |_: &Input, out: &mut Vec<Bytes>| {
                    out.push(body.clone());
                    Ok(())
                }) as Box<dyn Encoder<Input>>)
            },
            |_, _| Some(Box::new(Whole::new(decode_response)) as Box<dyn Decoder<Output>>),
        ))
    }
}

/// A refusal of media evidence, which only Gemini's format has a form for.
fn refuse_media(input: &Input, format: &str) -> Result<(), Fault> {
    match gemini::media_of(input) {
        Some(media) => Err(Fault::bad_request(format!(
            "{format} cannot take {media} evidence"
        ))),
        None => Ok(()),
    }
}

/// A client's request.
fn decode_request(raw: &[u8]) -> Result<Input, Fault> {
    let Ok(Value::Object(mut map)) = serde_json::from_slice(raw) else {
        return Err(invalid("body", "must be a JSON object"));
    };
    let state = map
        .remove("state")
        .ok_or_else(|| invalid("state", "is required"))?;
    if !(state.is_string() || state.is_object() || state.is_array()) {
        return Err(invalid("state", "must be a string, an object or an array"));
    }
    let Some(Value::Object(questions)) = map.remove("questions") else {
        return Err(invalid("questions", "must be an object of questions"));
    };
    let questions = questions.into_iter().map(|(id, q)| decode_question(id, q));
    let mut input = Input::new(state, questions.collect::<Result<Vec<_>, _>>()?);
    input.model = match map.remove("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(model)) => Some(model),
        Some(_) => return Err(invalid("model", "must be a string")),
    };
    input.native = Native::json::<Key>(map);
    Ok(input)
}

/// Question `id`, as `v` says it.
fn decode_question(id: String, v: Value) -> Result<Question, Fault> {
    let param = format!("questions.{id}");
    let Value::Object(mut q) = v else {
        return Err(invalid(&param, "must be an object"));
    };
    let is_native =
        (q.get("type").and_then(Value::as_str)).is_some_and(|kind| !KNOWN_TYPES.contains(&kind));
    if is_native {
        let ask = Ask::Native(Native::json::<Key>(q));
        return Ok(Question::new(id, ask));
    }
    let criteria = q.remove("criteria").unwrap_or_default();
    let ask = match q.remove("type").as_ref().and_then(Value::as_str) {
        // The kind has no form for a noul's criteria; any sent ride as the format's own.
        Some("noul") => {
            if !criteria.is_null() {
                q.insert("criteria".into(), criteria);
            }
            Ask::Noul
        }
        Some("choice") => match criteria {
            Value::Object(options) => Ask::Choice {
                options: (options.into_iter())
                    .map(|(name, about)| (name, Some(about).filter(|v| !v.is_null())))
                    .collect(),
            },
            _ => {
                return Err(invalid(
                    &param,
                    "has criteria other than an object of options",
                ));
            }
        },
        Some("score") => match criteria {
            Value::Array(levels) => Ask::Score { levels },
            _ => return Err(invalid(&param, "has criteria other than a list of levels")),
        },
        _ => return Err(invalid(&param, "has no type")),
    };
    let mut question = Question::new(id, ask);
    question.instructions = q.remove("instructions").filter(|v| !v.is_null());
    question.native = Native::json::<Key>(q);
    Ok(question)
}

/// The call of `input`.
fn encode_request(input: &Input) -> Result<Map<String, Value>, Fault> {
    refuse_media(input, NAME)?;
    let mut questions = Map::new();
    for q in &input.questions {
        if let Ask::Native(native) = &q.ask
            && let Some(question) = native.value::<Key>()
        {
            questions.insert(q.id.clone(), question.clone());
            continue;
        }
        let mut v = Map::new();
        let (kind, criteria) = match &q.ask {
            Ask::Noul => ("noul", None),
            Ask::Choice { options } => {
                // WARN: options of one name, such as openai-decisions' `true`
                // and `"true"` both asked, are one key.
                let options = options
                    .iter()
                    .map(|(name, about)| (name.clone(), about.clone().unwrap_or(Value::Null)));
                ("choice", Some(Value::Object(options.collect())))
            }
            Ask::Score { levels } => ("score", Some(Value::from(levels.clone()))),
            // WARN: decisions has no form for another format's question;
            // it is dropped and goes unanswered.
            _ => continue,
        };
        v.insert("type".into(), kind.into());
        set(&mut v, "instructions", q.instructions.clone());
        set(&mut v, "criteria", criteria);
        merge_missing(&mut v, q.native.fields::<Key>());
        questions.insert(q.id.clone(), v.into());
    }
    if questions.is_empty() && !input.questions.is_empty() {
        return Err(Fault::bad_request(format!(
            "{NAME} has no form for any question asked"
        )));
    }
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    v.insert("state".into(), input.state.clone());
    v.insert("questions".into(), questions.into());
    merge_missing(&mut v, input.native.fields::<Key>());
    Ok(v)
}

/// What a contract of this crate names differently in a response.
struct Wire {
    /// The format's name.
    name: &'static str,
    /// Whether the body names its `model`, which is the response's.
    model: bool,
    /// The usage keys of the input and output token counts.
    input_tokens: &'static str,
    output_tokens: &'static str,
    /// Whether usage carries a `total_tokens` the kind has no place for.
    total_tokens: bool,
}

/// The contract of `decisions`.
const WIRE: Wire = Wire {
    name: NAME,
    model: true,
    input_tokens: "input_tokens",
    output_tokens: "output_tokens",
    total_tokens: false,
};

/// The events of a provider's whole answer.
fn decode_response(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v: Value = serde_json::from_slice(&raw).unwrap_or_default();
    read_response(&v, &WIRE, out)
}

/// The events of the response `v` of a contract named by `wire`.
fn read_response(v: &Value, wire: &Wire, out: &mut Vec<Output>) -> Result<(), Fault> {
    let Some(answers) = v["answers"].as_object() else {
        return Err(Fault::upstream(format!(
            "the provider's success is not a response of {}",
            wire.name
        )));
    };
    let model = v["model"].as_str().filter(|_| wire.model);
    let mut start = Start::new(model.map(Into::into));
    let taken: &[&str] = if wire.model {
        &["model", "answers", "usage"]
    } else {
        &["answers", "usage"]
    };
    start.native = Native::json::<Key>(rest_of(v, taken));
    out.push(Output::Start(start));
    for (id, a) in answers {
        match decode_answer::<Key>(id, a) {
            Some(answer) => out.push(Output::Answer(answer)),
            None => out.push(Output::Native(Native::json::<Key>(
                json!({ "answers": { id: a } }),
            ))),
        }
    }
    if v["usage"].is_object() {
        let count = |key: &str| match &v["usage"][key] {
            Value::Null => Ok(0),
            n => n.as_u64().ok_or_else(|| {
                Fault::upstream(format!("a usage count is not a whole number: {n}"))
            }),
        };
        out.push(Output::Usage(Usage::new(
            count(wire.input_tokens)?,
            count(wire.output_tokens)?,
        )));
        let extras = rest_of(&v["usage"], &[wire.input_tokens, wire.output_tokens]);
        if !extras.is_empty() {
            out.push(Output::Native(Native::json::<Key>(
                json!({ "usage": extras }),
            )));
        }
    }
    Ok(())
}

/// Answer `id`, if `a` is of a type the kind has a form for, its other
/// fields format `K`'s own.
fn decode_answer<K: 'static>(id: &str, a: &Value) -> Option<Answer> {
    let number = |key: &str| a[key].as_f64();
    let (verdict, taken): (_, &[&str]) = match a["type"].as_str()? {
        "noul" => (Verdict::Noul(number("noul")?), &["noul"]),
        "choice" => (
            Verdict::Choice {
                choice: a["choice"].as_str()?.to_owned(),
                probabilities: a["probabilities"]
                    .as_object()?
                    .iter()
                    .map(|(name, p)| Some((name.clone(), p.as_f64()?)))
                    .collect::<Option<_>>()?,
            },
            &["choice", "probabilities"],
        ),
        "score" => {
            let by_level = a["probabilities"].as_object()?;
            let level = |i: usize| by_level.get(&i.to_string())?.as_f64();
            (
                Verdict::Score {
                    score: number("score")?,
                    probabilities: (0..by_level.len()).map(level).collect::<Option<_>>()?,
                },
                &["score", "probabilities"],
            )
        }
        _ => return None,
    };
    let mut answer = Answer::new(id, verdict);
    answer.confidence = number("confidence");
    let taken = [&["type", "confidence"], taken].concat();
    answer.native = Native::json::<K>(rest_of(a, &taken));
    Some(answer)
}

/// The questions of a client's request, read from its body when an answer needs them.
struct Asked<'a> {
    body: &'a [u8],
    questions: std::cell::OnceCell<Value>,
}

impl<'a> Asked<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self {
            body,
            questions: std::cell::OnceCell::new(),
        }
    }

    /// The levels the score question `id` asked.
    fn levels(&self, id: &str) -> Option<&Vec<Value>> {
        let questions = self.questions.get_or_init(|| {
            let mut request: Value = serde_json::from_slice(self.body).unwrap_or_default();
            request
                .get_mut("questions")
                .map(Value::take)
                .unwrap_or_default()
        });
        questions[id]["criteria"].as_array()
    }
}

/// A score level as text: a text itself, or the `label` of a level that is an object.
fn level_text(level: &Value) -> Option<&Value> {
    match level {
        Value::String(_) => Some(level),
        l if l["label"].is_string() => Some(&l["label"]),
        _ => None,
    }
}

/// A score's `legend` unless it has one: the levels its question asked, by position.
fn add_legend(v: &mut Map<String, Value>, answer: &Answer, asked: &Asked) {
    if !matches!(answer.verdict, Verdict::Score { .. }) || v.contains_key("legend") {
        return;
    }
    if let Some(levels) = asked.levels(&answer.id) {
        let legend = levels.iter().enumerate();
        let legend =
            legend.map(|(i, level)| (i.to_string(), level_text(level).unwrap_or(level).clone()));
        v.insert("legend".into(), Value::Object(legend.collect()));
    }
}

/// A whole response of `events` to a client that asked the questions `asked`.
fn encode_response(events: &[Output], asked: &Asked) -> Result<Bytes, Fault> {
    write_response(events, &WIRE, |answer| {
        let mut written = encode_answer::<Key>(answer)?;
        add_legend(&mut written, answer, asked);
        Ok(written)
    })
}

/// A whole response of `events` of a contract named by `wire`, its answers written by `answer`.
fn write_response(
    events: &[Output],
    wire: &Wire,
    answer: impl Fn(&Answer) -> Result<Map<String, Value>, Fault>,
) -> Result<Bytes, Fault> {
    let (mut body, mut answers, mut usage, mut extras) = (Map::new(), Map::new(), None, Map::new());
    for event in events {
        match event {
            Output::Start(start) => {
                if wire.model {
                    set(&mut body, "model", start.model.clone());
                }
                merge_missing(&mut body, start.native.fields::<Key>());
            }
            Output::Answer(a) => {
                answers.insert(a.id.clone(), answer(a)?.into());
            }
            Output::Usage(u) => *usage.get_or_insert_with(Usage::default) += u,
            Output::Native(n) => {
                let n = n.value::<Key>().unwrap_or_default();
                if let Some(more) = n["answers"].as_object() {
                    merge_missing(&mut answers, more);
                }
                if let Some(more) = n["usage"].as_object() {
                    merge_missing(&mut extras, more);
                }
            }
            _ => {}
        }
    }
    body.insert("answers".into(), answers.into());
    if let Some(usage) = usage {
        let mut u = Map::new();
        u.insert(wire.input_tokens.into(), usage.input_tokens.into());
        u.insert(wire.output_tokens.into(), usage.output_tokens.into());
        merge_missing(&mut u, &extras);
        if wire.total_tokens {
            let total = usage.input_tokens + usage.output_tokens;
            u.entry("total_tokens").or_insert_with(|| total.into());
        }
        body.insert("usage".into(), u.into());
    }
    to_bytes(&body)
}

/// An answer as the contract writes it, with format `K`'s own fields.
fn encode_answer<K: 'static>(answer: &Answer) -> Result<Map<String, Value>, Fault> {
    let mut v = Map::new();
    match &answer.verdict {
        Verdict::Noul(p) => {
            v.insert("type".into(), "noul".into());
            v.insert("noul".into(), (*p).into());
        }
        Verdict::Choice {
            choice,
            probabilities,
        } => {
            v.insert("type".into(), "choice".into());
            v.insert("choice".into(), choice.clone().into());
            let by_option = probabilities.iter().map(|(o, p)| (o.clone(), (*p).into()));
            v.insert("probabilities".into(), Value::Object(by_option.collect()));
        }
        Verdict::Score {
            score,
            probabilities,
        } => {
            v.insert("type".into(), "score".into());
            v.insert("score".into(), (*score).into());
            let by_level = probabilities.iter().enumerate();
            let by_level = by_level.map(|(i, p)| (i.to_string(), (*p).into()));
            v.insert("probabilities".into(), Value::Object(by_level.collect()));
        }
        _ => {
            return Err(Fault::new(format!(
                "{NAME} cannot write answer '{}'",
                answer.id
            )));
        }
    }
    set(&mut v, "confidence", answer.confidence);
    merge_missing(&mut v, answer.native.fields::<K>());
    Ok(v)
}

/// `v` as JSON bytes.
fn to_bytes(v: &Map<String, Value>) -> Result<Bytes, Fault> {
    serde_json::to_vec(v)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("a body could not be written").with_source(e))
}

/// A refusal in OpenAI's error schema, as Chat Completions writes it.
fn refusal(fault: &Fault, request: &http::Request<Bytes>, head: &mut ResponseHead) -> Bytes {
    OpenAiChat.refusal(fault, request, head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;

    /// Regression: this format's own copy dropped `param` and wrote an
    /// error under a success status.
    #[test]
    fn a_refusal_is_written_as_chat_completions_writes_one() {
        let request = http::Request::new(Bytes::new());
        let mut head = http::Response::new(()).into_parts().0;
        head.status = http::StatusCode::BAD_REQUEST;
        let body = refusal(&Fault::bad_request("no model"), &request, &mut head);
        let v: Value = serde_json::from_slice(&body).unwrap_or_default();
        assert!(v["error"].get("param").is_some(), "{v}");
        head.status = http::StatusCode::OK;
        let body = refusal(&Fault::bad_request("no model"), &request, &mut head);
        assert_eq!(head.status, http::StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(&body[..], b"no model");
    }

    #[test]
    fn a_request_of_each_question_type_is_read_and_written_back() {
        let raw = json!({
            "model": "jev-latest",
            "state": {"subject": "Refund", "body": "Charged twice"},
            "questions": {
                "spam": {"type": "noul", "instructions": "Is this spam?", "criteria": {"true": "Unsolicited", "maybe": "y"}},
                "tone": {"type": "choice", "criteria": {"angry": null, "calm": {"x": 1}}},
                "urgency": {"type": "score", "instructions": "Urgency?", "criteria": ["Low", {"label": "High"}]},
            },
            "extra": true,
        });
        let input = decode_request(raw.to_string().as_bytes()).expect("input");
        assert_eq!(input.model(), Some("jev-latest"));
        assert_eq!(input.questions.len(), 3);
        assert!(
            matches!(&input.questions[1].ask, Ask::Choice { options } if options[0].1.is_none())
        );
        assert_eq!(Value::Object(encode_request(&input).expect("call")), raw);
        let score = Verdict::Score {
            score: 0.5,
            probabilities: vec![0.5, 0.5],
        };
        let body = raw.to_string();
        let written = encode_response(
            &[Output::Answer(Answer::new("urgency", score))],
            &Asked::new(body.as_bytes()),
        )
        .expect("body");
        let written: Value = serde_json::from_slice(&written).unwrap_or_default();
        assert_eq!(
            written["answers"]["urgency"]["legend"],
            json!({"0": "Low", "1": "High"})
        );
    }

    #[test]
    fn a_request_without_state_or_with_an_odd_question_is_refused() {
        let refused = |raw: Value| {
            decode_request(raw.to_string().as_bytes()).is_err_and(|f| f.is(FaultKind::Client))
        };
        assert!(refused(json!({"questions": {}})));
        assert!(refused(json!({"state": 1, "questions": {}})));
        assert!(refused(
            json!({"state": "s", "questions": {"q": {"instructions": "No type?"}}})
        ));
        assert!(refused(
            json!({"state": "s", "questions": {"q": {"type": "score", "criteria": {}}}})
        ));
    }

    #[test]
    fn a_question_of_a_providers_own_type_is_asked_as_it_came() {
        let raw = json!({
            "state": "s",
            "questions": {
                "when": {"type": "date", "instructions": "When did it ship?", "range": ["2026-01-01", null]},
                "spam": {"type": "noul"},
                "days_late": {"type": "number", "criteria": {"min": 0, "max": 10}},
                "cat": {"type": "box", "instructions": "The cat"},
            },
        });
        let input = decode_request(raw.to_string().as_bytes()).expect("input");
        for i in [0, 2, 3] {
            assert!(
                matches!(&input.questions[i].ask, Ask::Native(n) if n.value::<Key>().is_some())
            );
        }
        assert_eq!(Value::Object(encode_request(&input).expect("call")), raw);
    }

    #[test]
    fn a_response_is_written_back_as_it_came() {
        let raw = json!({
            "model": "jev-1.13.0",
            "answers": {
                "spam": {"type": "noul", "noul": 0.98},
                "tone": {"type": "choice", "choice": "angry", "probabilities": {"angry": 0.8, "calm": 0.2}, "confidence": 0.9},
                "urgency": {"type": "score", "score": 1.7, "confidence": 0.9, "legend": {"0": "Can wait", "1": "This week", "2": "Today"}, "probabilities": {"0": 0.1, "1": 0.1, "2": 0.8}},
                "future": {"type": "vibes", "vibe": "immaculate"},
                "days_late": {"type": "number", "number": 4, "text": null, "confidence": 0.99},
                "cat": {"type": "box", "box": {"top": 0, "bottom": 0.6, "left": 0.2, "right": 1, "unit": "fraction"}},
            },
            "usage": {"input_tokens": 42, "output_tokens": 0, "cached": 1},
            "request_id": "r",
        });
        let mut events = Vec::new();
        decode_response(Bytes::from(raw.to_string()), &mut events).expect("events");
        assert!(events.contains(&Output::Usage(Usage::new(42, 0))));
        assert!(events.iter().any(|e| matches!(e,
            Output::Answer(Answer { verdict: Verdict::Score { probabilities, .. }, .. })
                if probabilities == &[0.1, 0.1, 0.8])));
        let written: Value =
            serde_json::from_slice(&encode_response(&events, &Asked::new(b"{}")).expect("body"))
                .unwrap_or_default();
        assert_eq!(written, raw);
        let score = Verdict::Score {
            score: 1.7,
            probabilities: vec![0.1, 0.1, 0.8],
        };
        let asked = json!({"questions": {"urgency": {"type": "score", "criteria": ["Can wait", "This week", "Today"]}}});
        let asked = asked.to_string();
        let others = [Output::Answer(Answer::new("urgency", score))];
        let written = encode_response(&others, &Asked::new(asked.as_bytes())).expect("body");
        let written: Value = serde_json::from_slice(&written).unwrap_or_default();
        assert_eq!(
            written["answers"]["urgency"]["legend"],
            raw["answers"]["urgency"]["legend"]
        );
    }

    #[test]
    fn a_success_that_is_no_response_is_the_providers_fault() {
        let refused = decode_response(Bytes::from_static(b"{\"x\":1}"), &mut Vec::new());
        assert!(refused.is_err_and(|f| f.is(FaultKind::Provider)));
    }
}

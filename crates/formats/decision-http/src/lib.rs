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
//! and `legend`. An answer of a type the kind has no form for is kept as
//! the format's own and written back where it was.

use respan_gateway_conversation_http::fields::{
    invalid, kept, merge_missing, native_of, rest_of, set,
};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_conversation_http::{Schema, read_provider_error};
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, ErrorCode, Fault, Format};
use respan_gateway_decision::{Answer, Ask, Input, Output, Question, Start, Usage, Verdict};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_TYPE, HeaderValue};
use respan_gateway_http::http::{self, Method, response::Parts as ResponseHead};
use respan_gateway_http::path::{path_ends_with, set_path};
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Whole};
use respan_gateway_http::{Connect, Upstream};
use serde_json::{Map, Value, json};

const NAME: &str = "decisions";
const PATH: &str = "/decisions";
/// TypeSafe's name for the endpoint, which its clients call.
const SYSTEM_ONE_PATH: &str = "/systemone";

/// The decisions contract, as Respan or a compatible vendor serves it: a
/// client's `POST` to a path ending in `/decisions` or `/systemone`, and a
/// provider called at `/decisions`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Decisions;

impl Decisions {
    /// The format's name, `decisions`, which tags its native fields.
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
                Ok(reply.write(head, Box::new(All::new(|e: &[Output]| encode_response(e)))))
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
    input.native = native_of(NAME, map);
    Ok(input)
}

/// Question `id`, as `v` says it.
fn decode_question(id: String, v: Value) -> Result<Question, Fault> {
    let param = format!("questions.{id}");
    let Value::Object(mut q) = v else {
        return Err(invalid(&param, "must be an object"));
    };
    let criteria = q.remove("criteria").unwrap_or_default();
    let ask = match q.remove("type").as_ref().and_then(Value::as_str) {
        Some("noul") => match criteria {
            Value::Null => Ask::Noul {
                yes: None,
                no: None,
            },
            Value::Object(mut sides) => {
                let yes = sides.remove("true").filter(|v| !v.is_null());
                let no = sides.remove("false").filter(|v| !v.is_null());
                if !sides.is_empty() {
                    return Err(invalid(
                        &param,
                        "has criteria other than {\"true\", \"false\"}",
                    ));
                }
                Ask::Noul { yes, no }
            }
            _ => {
                return Err(invalid(
                    &param,
                    "has criteria other than {\"true\", \"false\"}",
                ));
            }
        },
        Some("choice") => match criteria {
            Value::Object(options) => Ask::Choice {
                options: options
                    .into_iter()
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
        _ => {
            return Err(invalid(
                &param,
                "has a type other than noul, choice or score",
            ));
        }
    };
    let mut question = Question::new(id, ask);
    question.instructions = q.remove("instructions").filter(|v| !v.is_null());
    question.native = native_of(NAME, q);
    Ok(question)
}

/// The call of `input`.
fn encode_request(input: &Input) -> Result<Map<String, Value>, Fault> {
    let mut questions = Map::new();
    for q in &input.questions {
        let mut v = Map::new();
        let (kind, criteria) = match &q.ask {
            Ask::Noul { yes, no } => {
                let mut sides = Map::new();
                set(&mut sides, "true", yes.clone());
                set(&mut sides, "false", no.clone());
                ("noul", (!sides.is_empty()).then_some(Value::Object(sides)))
            }
            Ask::Choice { options } => {
                let options = options
                    .iter()
                    .map(|(name, about)| (name.clone(), about.clone().unwrap_or(Value::Null)));
                ("choice", Some(Value::Object(options.collect())))
            }
            Ask::Score { levels } => ("score", Some(Value::from(levels.clone()))),
            _ => {
                return Err(Fault::bad_request(format!(
                    "{NAME} cannot ask question '{}'",
                    q.id
                )));
            }
        };
        v.insert("type".into(), kind.into());
        set(&mut v, "instructions", q.instructions.clone());
        set(&mut v, "criteria", criteria);
        merge_missing(&mut v, &kept(NAME, &q.native)?);
        questions.insert(q.id.clone(), v.into());
    }
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    v.insert("state".into(), input.state.clone());
    v.insert("questions".into(), questions.into());
    merge_missing(&mut v, &kept(NAME, &input.native)?);
    Ok(v)
}

/// The events of a provider's whole answer.
fn decode_response(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v: Value = serde_json::from_slice(&raw).unwrap_or_default();
    let Some(answers) = v["answers"].as_object() else {
        return Err(Fault::upstream(format!(
            "the provider's success is not a response of {NAME}"
        )));
    };
    let mut start = Start::new(v["model"].as_str().map(Into::into));
    start.native = native_of(NAME, rest_of(&v, &["model", "answers", "usage"]));
    out.push(Output::Start(start));
    for (id, a) in answers {
        match decode_answer(id, a) {
            Some(answer) => out.push(Output::Answer(answer)),
            None => out.push(Output::native(NAME, json!({ "answers": { id: a } }))),
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
            count("input_tokens")?,
            count("output_tokens")?,
        )));
        let extras = rest_of(&v["usage"], &["input_tokens", "output_tokens"]);
        if !extras.is_empty() {
            out.push(Output::native(NAME, json!({ "usage": extras })));
        }
    }
    Ok(())
}

/// Answer `id`, if `a` is of a type the kind has a form for.
fn decode_answer(id: &str, a: &Value) -> Option<Answer> {
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
    answer.native = native_of(NAME, rest_of(a, &taken));
    Some(answer)
}

/// A whole response of `events`.
fn encode_response(events: &[Output]) -> Result<Bytes, Fault> {
    let (mut body, mut answers, mut usage, mut extras) = (Map::new(), Map::new(), None, Map::new());
    for event in events {
        match event {
            Output::Start(start) => {
                set(&mut body, "model", start.model.clone());
                merge_missing(&mut body, &kept(NAME, &start.native)?);
            }
            Output::Answer(answer) => {
                answers.insert(answer.id.clone(), encode_answer(answer)?.into());
            }
            Output::Usage(u) => *usage.get_or_insert_with(Usage::default) += u,
            Output::Native(n) if &*n.format != NAME => {
                return Err(Fault::new(format!(
                    "an event of format {} cannot be written in {NAME}",
                    n.format
                ))
                .with(ErrorCode("untranslatable".into())));
            }
            Output::Native(n) => {
                if let Some(more) = n.value["answers"].as_object() {
                    merge_missing(&mut answers, more);
                }
                if let Some(more) = n.value["usage"].as_object() {
                    merge_missing(&mut extras, more);
                }
            }
            _ => {}
        }
    }
    body.insert("answers".into(), answers.into());
    if let Some(usage) = usage {
        let mut u = Map::new();
        u.insert("input_tokens".into(), usage.input_tokens.into());
        u.insert("output_tokens".into(), usage.output_tokens.into());
        merge_missing(&mut u, &extras);
        body.insert("usage".into(), u.into());
    }
    to_bytes(&body)
}

/// An answer as the contract writes it.
fn encode_answer(answer: &Answer) -> Result<Map<String, Value>, Fault> {
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
    merge_missing(&mut v, &kept(NAME, &answer.native)?);
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
                "spam": {"type": "noul", "instructions": "Is this spam?", "criteria": {"true": "Unsolicited"}},
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
    }

    #[test]
    fn a_request_without_state_or_with_an_odd_question_is_refused() {
        let refused = |raw: Value| {
            decode_request(raw.to_string().as_bytes()).is_err_and(|f| f.is(FaultKind::Client))
        };
        assert!(refused(json!({"questions": {}})));
        assert!(refused(json!({"state": 1, "questions": {}})));
        assert!(refused(
            json!({"state": "s", "questions": {"q": {"type": "vibes"}}})
        ));
        assert!(refused(
            json!({"state": "s", "questions": {"q": {"type": "noul", "criteria": {"true": "x", "maybe": "y"}}}})
        ));
        assert!(refused(
            json!({"state": "s", "questions": {"q": {"type": "score", "criteria": {}}}})
        ));
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
            serde_json::from_slice(&encode_response(&events).expect("body")).unwrap_or_default();
        assert_eq!(written, raw);
    }

    #[test]
    fn a_success_that_is_no_response_is_the_providers_fault() {
        let refused = decode_response(Bytes::from_static(b"{\"x\":1}"), &mut Vec::new());
        assert!(refused.is_err_and(|f| f.is(FaultKind::Provider)));
    }
}

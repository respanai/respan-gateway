//! OpenAI's decisions contract, [`OpenAiDecisions`]: evidence as `input`,
//! questions and their answers as ordered lists.

use super::*;
use respan_gateway_core::NativeFields;
use respan_gateway_decision::ChoiceValue;
use std::borrow::Cow;

const OPENAI: &str = "openai-decisions";
/// The most questions a request asks.
const MAX_QUESTIONS: usize = 400;

/// OpenAI's decisions contract, as Azure OpenAI serves it: a client's
/// `POST` to its path (`/v1/decisions` unless [`OpenAiDecisions::serving`]
/// says another), and a provider called at `/decisions`.
///
/// A request is `{"model", "input", "questions": [..]}`: `input` the
/// evidence, a text or user messages of `input_text` and `input_image`
/// parts, which is the kind's `state`; each question `{"type", "name",
/// "instructions"}`, a `predicate`, a `choice` of `choices` (each a string
/// or boolean `value` and a `description`), or a `score` of `levels`. A
/// question's id is its `name`, or its position for one unnamed or named as
/// one before it, and a provider is sent each question named for its id.
/// A response is `{"model", "answers": [..], "usage"}`, an answer per
/// question, in order, echoing its name: a predicate's `probability`, a
/// choice's `choice` and `probabilities`, a score's `score` and
/// `probabilities` by level, or a `refusal`. An answer is read against the
/// question it answers, and written to a client from the client's own
/// request, so its names and levels are as it asked them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiDecisions {
    path: Cow<'static, str>,
}

impl Default for OpenAiDecisions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl OpenAiDecisions {
    /// Its clients served at `/v1/decisions`.
    pub const DEFAULT: Self = Self {
        path: Cow::Borrowed("/v1/decisions"),
    };

    /// The format's name, `openai-decisions`, which tags its native fields.
    pub const NAME: &'static str = OPENAI;

    /// Its clients served at a path ending in `path`, as when another format
    /// has `/v1/decisions`.
    pub fn serving(path: impl Into<String>) -> Self {
        Self {
            path: Cow::Owned(path.into()),
        }
    }

    /// An answer as this contract writes it, its name and its score's
    /// labels as `question`, as a client asked it, says them.
    pub fn answer_value(answer: &Answer, question: &Value) -> Result<Value, Fault> {
        let mut v = kept(OPENAI, &redundant(&answer.native))?;
        v.insert("type".into(), verdict_type(&answer.verdict)?.into());
        if let Some(name) = question.get("name") {
            v.insert("name".into(), name.clone());
        }
        match &answer.verdict {
            Verdict::Noul(p) => _ = v.insert("probability".into(), (*p).into()),
            Verdict::Choice {
                choice,
                probabilities,
            } => {
                v.insert("choice".into(), choice_json(choice));
                let by_value = probabilities
                    .iter()
                    .map(|(c, p)| json!({ "value": choice_json(c), "probability": p }));
                v.insert("probabilities".into(), by_value.collect());
            }
            Verdict::Score {
                score,
                probabilities,
            } => {
                v.insert("score".into(), (*score).into());
                let levels = question["levels"].as_array();
                let by_level = probabilities.iter().enumerate().map(|(i, p)| {
                    let label = levels.and_then(|l| l.get(i)).map(|l| l["label"].clone());
                    json!({ "label": label.unwrap_or_default(), "value": i, "probability": p })
                });
                v.insert("probabilities".into(), by_level.collect());
            }
            _ => {}
        }
        set(&mut v, "confidence", answer.confidence);
        Ok(Value::Object(v))
    }
}

impl Format<ClientIo, Input, Output> for OpenAiDecisions {
    fn name(&self) -> &str {
        OPENAI
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST && path_ends_with(request.uri(), &self.path)
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
                // An answer is written as the client asked its question.
                let asked = serde_json::from_slice::<Value>(reply.request().body());
                let asked = asked.unwrap_or_default()["questions"].clone();
                let encoder = All::new(move |e: &[Output]| encode_response(e, &asked));
                Ok(reply.write(head, Box::new(encoder)))
            },
        ))
    }
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for OpenAiDecisions {
    fn name(&self) -> &str {
        OPENAI
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, self.clone(), move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenAiDecisions {
    fn name(&self) -> &str {
        OPENAI
    }

    /// A call: a `POST` of the whole request to `/decisions`, its answer
    /// read against the questions it sent.
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(io.with_refusal(read_provider_error).bind(
            move |head, input: Option<&Input>| {
                let input = input.ok_or_else(|| {
                    Fault::config(format!("{OPENAI} does not accept streaming inputs"))
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
            |_, sent| {
                let sent = serde_json::from_slice::<Value>(sent.body()).unwrap_or_default();
                let asked = sent["questions"].as_array().cloned().unwrap_or_default();
                Some(
                    Box::new(Whole::new(move |raw: Bytes, out: &mut Vec<Output>| {
                        decode_response(&raw, &asked, out)
                    })) as Box<dyn Decoder<Output>>,
                )
            },
        ))
    }
}

/// `native`, but for what another format says that this one writes from
/// its client's request anyway, such as a score's `legend` in `decisions`,
/// its levels again.
fn redundant(native: &Option<NativeFields>) -> Option<NativeFields> {
    let mut native = native.clone();
    if let Some(n) = native.as_mut().filter(|n| &*n.format == NAME) {
        n.fields.remove("legend");
    }
    native.filter(|n| !n.is_empty())
}

/// A choice's value, a string or a boolean.
fn choice_of(v: &Value) -> Option<ChoiceValue> {
    match v {
        Value::String(s) => Some(ChoiceValue::String(s.clone())),
        Value::Bool(b) => Some(ChoiceValue::Boolean(*b)),
        _ => None,
    }
}

/// A choice's value as JSON.
fn choice_json(v: &ChoiceValue) -> Value {
    match v {
        ChoiceValue::String(s) => s.clone().into(),
        ChoiceValue::Boolean(b) => (*b).into(),
    }
}

/// The type of an answer of `verdict`.
fn verdict_type(verdict: &Verdict) -> Result<&'static str, Fault> {
    Ok(match verdict {
        Verdict::Noul(_) => "predicate",
        Verdict::Choice { .. } => "choice",
        Verdict::Score { .. } => "score",
        Verdict::Refusal => "refusal",
        _ => return Err(Fault::new(format!("{OPENAI} cannot write this answer"))),
    })
}

/// The ids of `questions`: each one's name, or its position for one
/// unnamed or named as one before it.
fn ids(questions: &[Value]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::with_capacity(questions.len());
    for (i, q) in questions.iter().enumerate() {
        let name = q["name"].as_str().filter(|n| !ids.iter().any(|id| id == n));
        let mut id = name.map_or_else(|| i.to_string(), str::to_owned);
        while ids.contains(&id) {
            id.push('_');
        }
        ids.push(id);
    }
    ids
}

/// Whether `v` is evidence: a text, or user messages of text and image
/// parts, each image at a URL.
fn is_evidence(v: &Value) -> bool {
    let part = |p: &Value| match p["type"].as_str() {
        Some("input_text") => p["text"].is_string(),
        Some("input_image") => p["image_url"].is_string() && p.get("file_id").is_none(),
        _ => false,
    };
    let message = |m: &Value| {
        let content = &m["content"];
        let parts = content.as_array().is_some_and(|ps| ps.iter().all(part));
        m["role"] == "user"
            && m.get("type").is_none_or(|t| t == "message")
            && (content.is_string() || parts)
    };
    v.is_string() || v.as_array().is_some_and(|ms| ms.iter().all(message))
}

/// A client's request.
fn decode_request(raw: &[u8]) -> Result<Input, Fault> {
    let Ok(Value::Object(mut map)) = serde_json::from_slice(raw) else {
        return Err(invalid("body", "must be a JSON object"));
    };
    if map.get("stream").is_some_and(|s| s != &Value::Bool(false)) {
        return Err(invalid("stream", "is not supported"));
    }
    map.remove("stream");
    let state = map
        .remove("input")
        .ok_or_else(|| invalid("input", "is required"))?;
    if !is_evidence(&state) {
        let what = "must be a text or user messages of text and image URL parts";
        return Err(invalid("input", what));
    }
    let Some(Value::Array(asked)) = map.remove("questions") else {
        return Err(invalid("questions", "must be a list of questions"));
    };
    if asked.is_empty() || asked.len() > MAX_QUESTIONS {
        let what = format!("must hold 1 to {MAX_QUESTIONS} questions");
        return Err(invalid("questions", &what));
    }
    let questions = ids(&asked).into_iter().zip(asked);
    let questions = questions.map(|(id, q)| decode_question(id, q));
    let mut input = Input::new(state, questions.collect::<Result<Vec<_>, _>>()?);
    input.model = match map.remove("model") {
        Some(Value::String(model)) => Some(model),
        _ => return Err(invalid("model", "must be a string")),
    };
    input.native = native_of(OPENAI, map);
    Ok(input)
}

/// Question `id`, as `v` says it.
fn decode_question(id: String, v: Value) -> Result<Question, Fault> {
    let Value::Object(mut q) = v else {
        return Err(invalid("questions", "must hold objects"));
    };
    let param = |field: &str| format!("questions.{id}.{field}");
    match q.remove("name") {
        None | Some(Value::Null | Value::String(_)) => {}
        Some(_) => return Err(invalid(&param("name"), "must be a string or null")),
    }
    let instructions = q.remove("instructions").unwrap_or_default();
    if !(instructions.is_string() || instructions.is_array()) {
        return Err(invalid(&param("instructions"), "must be a text or parts"));
    }
    let ask = match q.remove("type").as_ref().and_then(Value::as_str) {
        Some("predicate") => Ask::Noul {
            yes: None,
            no: None,
        },
        Some("choice") => {
            let Some(Value::Array(choices)) = q.remove("choices") else {
                return Err(invalid(&param("choices"), "must be a list"));
            };
            let mut options: Vec<(ChoiceValue, Option<Value>)> = Vec::new();
            for c in choices {
                let value = choice_of(&c["value"]).ok_or_else(|| {
                    invalid(&param("choices"), "must hold string or boolean values")
                })?;
                if options.iter().any(|(v, _)| v == &value) {
                    return Err(invalid(&param("choices"), "must hold unique values"));
                }
                let about = c.get("description").filter(|d| !d.is_null()).cloned();
                if about.as_ref().is_some_and(|d| !d.is_string()) {
                    return Err(invalid(&param("choices"), "must hold text descriptions"));
                }
                options.push((value, about));
            }
            if !(2..=255).contains(&options.len()) {
                return Err(invalid(&param("choices"), "must hold 2 to 255 choices"));
            }
            Ask::Choice { options }
        }
        Some("score") => match q.remove("levels") {
            Some(Value::Array(levels)) if levels.iter().all(|l| l["label"].is_string()) => {
                Ask::Score { levels }
            }
            _ => {
                return Err(invalid(
                    &param("levels"),
                    "must be a list of labeled levels",
                ));
            }
        },
        _ => {
            let what = "must be predicate, choice or score";
            return Err(invalid(&param("type"), what));
        }
    };
    let mut question = Question::new(id, ask);
    question.instructions = Some(instructions);
    question.native = native_of(OPENAI, q);
    Ok(question)
}

/// The call of `input`: its state as the evidence, as JSON text if it is
/// none, as of a client of another format; each question named for its id.
fn encode_request(input: &Input) -> Result<Map<String, Value>, Fault> {
    let mut questions = Vec::with_capacity(input.questions.len());
    for q in &input.questions {
        let mut v = Map::new();
        let refused = |what: &str| {
            Fault::bad_request(format!("{OPENAI} cannot ask question '{}': {what}", q.id))
        };
        match &q.ask {
            Ask::Noul {
                yes: None,
                no: None,
            } => _ = v.insert("type".into(), "predicate".into()),
            Ask::Noul { .. } => return Err(refused("a predicate has no criteria")),
            Ask::Choice { options } => {
                v.insert("type".into(), "choice".into());
                let choices = options.iter().map(|(value, about)| {
                    let mut c = Map::new();
                    c.insert("value".into(), choice_json(value));
                    let about = about.as_ref().map(|a| match a {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    });
                    set(&mut c, "description", about);
                    Value::Object(c)
                });
                v.insert("choices".into(), choices.collect());
            }
            Ask::Score { levels } => {
                v.insert("type".into(), "score".into());
                let levels = levels.iter().map(|l| match l {
                    Value::String(label) => Ok(json!({ "label": label })),
                    l if l["label"].is_string() => Ok(l.clone()),
                    _ => Err(refused("a level has no label")),
                });
                v.insert("levels".into(), levels.collect::<Result<_, _>>()?);
            }
            _ => return Err(refused("of a type it has none of")),
        }
        v.insert("name".into(), q.id.clone().into());
        v.insert(
            "instructions".into(),
            q.instructions.clone().unwrap_or_else(|| "".into()),
        );
        merge_missing(&mut v, &kept(OPENAI, &q.native)?);
        questions.push(Value::Object(v));
    }
    let state = match &input.state {
        evidence if is_evidence(evidence) => evidence.clone(),
        other => other.to_string().into(),
    };
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    v.insert("input".into(), state);
    v.insert("questions".into(), questions.into());
    merge_missing(&mut v, &kept(OPENAI, &input.native)?);
    Ok(v)
}

/// A probability: a number from 0 to 1.
fn probability(v: &Value) -> Option<f64> {
    v.as_f64().filter(|p| (0.0..=1.0).contains(p))
}

/// The events of a provider's whole answer to the questions `asked`, each
/// answer the question's at its position, of its type or a refusal, and
/// naming it as it was sent; another is the provider's fault.
fn decode_response(raw: &[u8], asked: &[Value], out: &mut Vec<Output>) -> Result<(), Fault> {
    let bad = |what: &str| Fault::upstream(format!("the provider's {OPENAI} response {what}"));
    let v: Value = serde_json::from_slice(raw).map_err(|_| bad("is not JSON"))?;
    let answers = v["answers"]
        .as_array()
        .ok_or_else(|| bad("has no answers"))?;
    if answers.len() != asked.len() {
        return Err(bad("does not answer each question once"));
    }
    let mut start = Start::new(v["model"].as_str().map(Into::into));
    start.native = native_of(OPENAI, rest_of(&v, &["model", "answers", "usage"]));
    out.push(Output::Start(start));
    for (a, q) in answers.iter().zip(asked) {
        let id = q["name"].as_str().unwrap_or_default();
        if a["name"] != q["name"] {
            return Err(bad(&format!("answers '{id}' out of turn")));
        }
        let ty = a["type"].as_str().unwrap_or_default();
        if ty != "refusal" && q["type"] != ty {
            return Err(bad(&format!("answers '{id}' as another type")));
        }
        let unreadable = || bad(&format!("answers '{id}' unreadably"));
        let p = |v: &Value| probability(&v["probability"]).ok_or_else(unreadable);
        let entries = || a["probabilities"].as_array().ok_or_else(unreadable);
        let (verdict, taken): (_, &[&str]) = match ty {
            "refusal" => (Verdict::Refusal, &[]),
            "predicate" => (Verdict::Noul(p(a)?), &["probability"]),
            "choice" => {
                let values = q["choices"].as_array().into_iter().flatten();
                let values: Vec<_> = values.filter_map(|c| choice_of(&c["value"])).collect();
                let probabilities = entries()?
                    .iter()
                    .map(|e| Ok((choice_of(&e["value"]).ok_or_else(unreadable)?, p(e)?)))
                    .collect::<Result<Vec<_>, Fault>>()?;
                let choice = choice_of(&a["choice"]).filter(|c| values.contains(c));
                let in_order = probabilities.iter().map(|(v, _)| v).eq(&values);
                let choice = choice.filter(|_| in_order).ok_or_else(unreadable)?;
                let verdict = Verdict::Choice {
                    choice,
                    probabilities,
                };
                (verdict, &["choice", "probabilities"])
            }
            "score" => {
                let levels = q["levels"].as_array().map_or(0, Vec::len);
                let probabilities = entries()?.iter().map(p).collect::<Result<Vec<_>, _>>()?;
                let score = a["score"]
                    .as_f64()
                    .filter(|_| probabilities.len() == levels);
                let score = score.ok_or_else(unreadable)?;
                let verdict = Verdict::Score {
                    score,
                    probabilities,
                };
                (verdict, &["score", "probabilities"])
            }
            _ => return Err(bad(&format!("answers '{id}' as no type it has"))),
        };
        let mut answer = Answer::new(id, verdict);
        answer.confidence = match ty {
            "choice" | "score" => Some(probability(&a["confidence"]).ok_or_else(unreadable)?),
            _ => None,
        };
        let taken = [&["type", "name", "confidence"], taken].concat();
        answer.native = native_of(OPENAI, rest_of(a, &taken));
        out.push(Output::Answer(answer));
    }
    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
        let count = |n: &Value| match n {
            Value::Null => Ok(0),
            n => n.as_u64().ok_or_else(|| bad("counts usage unreadably")),
        };
        let mut usage = Usage::new(count(&u["input_tokens"])?, count(&u["output_tokens"])?);
        usage.cached_input_tokens = count(&u["input_tokens_details"]["cached_tokens"])?;
        let written = &u["input_tokens_details"]["cache_write_tokens"];
        usage.cache_write_input_tokens = count(written)?;
        usage.reasoning_tokens = count(&u["output_tokens_details"]["reasoning_tokens"])?;
        out.push(Output::Usage(usage));
    }
    Ok(())
}

/// A whole response of `events` to a client that asked the questions
/// `asked`: an answer to each, in its order.
fn encode_response(events: &[Output], asked: &Value) -> Result<Bytes, Fault> {
    let asked = asked.as_array().map(Vec::as_slice).unwrap_or_default();
    let (mut body, mut usage) = (Map::new(), None);
    let mut answers: Vec<&Answer> = Vec::new();
    for event in events {
        match event {
            Output::Start(start) => {
                set(&mut body, "model", start.model.clone());
                merge_missing(&mut body, &kept(OPENAI, &start.native)?);
            }
            Output::Answer(answer) => answers.push(answer),
            Output::Usage(u) => *usage.get_or_insert_with(Usage::default) += *u,
            Output::Native(n) if &*n.format != OPENAI => {
                return Err(Fault::new(format!(
                    "an event of format {} cannot be written in {OPENAI}",
                    n.format
                ))
                .with(ErrorCode("untranslatable".into())));
            }
            _ => {}
        }
    }
    let mut written = Vec::with_capacity(asked.len());
    for (id, q) in ids(asked).iter().zip(asked) {
        let answer = answers.iter().find(|a| &a.id == id);
        let answer =
            answer.ok_or_else(|| Fault::upstream(format!("the provider did not answer '{id}'")))?;
        written.push(OpenAiDecisions::answer_value(answer, q)?);
    }
    body.insert("answers".into(), written.into());
    if let Some(u) = usage {
        let v = json!({
            "input_tokens": u.input_tokens,
            "input_tokens_details": {
                "cached_tokens": u.cached_input_tokens,
                "cache_write_tokens": u.cache_write_input_tokens,
            },
            "output_tokens": u.output_tokens,
            "output_tokens_details": { "reasoning_tokens": u.reasoning_tokens },
            "total_tokens": u.input_tokens + u.output_tokens,
        });
        body.insert("usage".into(), v);
    }
    to_bytes(&body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;

    fn events_of(raw: &Value, asked: &[Value]) -> Result<Vec<Output>, Fault> {
        let mut events = Vec::new();
        decode_response(raw.to_string().as_bytes(), asked, &mut events)?;
        Ok(events)
    }

    #[test]
    fn a_question_of_another_formats_own_type_is_refused() {
        let date = json!({"type": "date", "instructions": "When?"});
        let ask = Ask::Native(NativeValue::new("decisions", date));
        let input = Input::new(json!("s"), vec![Question::new("when", ask)]);
        assert!(encode_request(&input).is_err_and(|f| f.is(FaultKind::Client)));
    }

    /// OpenAI's decisions contract has no number type.
    #[test]
    fn a_number_question_is_refused() {
        let input = Input::new(json!("s"), vec![Question::new("days_late", Ask::Number)]);
        assert!(encode_request(&input).is_err_and(|f| f.is(FaultKind::Client)));
    }

    #[test]
    fn a_request_is_read_and_written_back() {
        let raw = json!({
            "model": "deployment",
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "hello"},
                {"type": "input_image", "image_url": "https://example.com/a.png"},
            ]}],
            "questions": [
                {"type": "predicate", "name": "damaged", "instructions": "a"},
                {"type": "choice", "name": "refund", "instructions": "b",
                 "choices": [{"value": true}, {"value": "true", "description": "the string"}]},
                {"type": "score", "name": "severity", "instructions": "c",
                 "levels": [{"label": "low"}, {"label": "high"}]},
            ],
        });
        let input = decode_request(raw.to_string().as_bytes()).expect("input");
        assert_eq!(input.questions[0].id, "damaged");
        assert!(
            matches!(&input.questions[1].ask, Ask::Choice { options } if options[0].0 != options[1].0)
        );
        assert_eq!(Value::Object(encode_request(&input).expect("call")), raw);
    }

    #[test]
    fn a_question_unnamed_or_named_twice_is_known_by_its_position() {
        let asked = [
            json!({"name": "same"}),
            json!({"name": "same"}),
            json!({}),
            json!({"name": "2"}),
        ];
        assert_eq!(ids(&asked), ["same", "1", "2", "3"]);
        // A position another question is named for is told apart.
        assert_eq!(ids(&[json!({"name": "1"}), json!({})]), ["1", "1_"]);
    }

    #[test]
    fn a_request_of_another_format_is_asked_by_its_ids() {
        // As a client of `decisions` asks it: state as JSON, keyed questions.
        let raw = json!({
            "model": "deployment",
            "state": {"subject": "Refund"},
            "questions": {
                "tone": {"type": "choice", "criteria": {"angry": null, "calm": {"x": 1}}},
                "urgency": {"type": "score", "criteria": ["Low", {"label": "High"}]},
            },
        });
        let input = super::super::decode_request(raw.to_string().as_bytes()).expect("input");
        let call = Value::Object(encode_request(&input).expect("call"));
        assert_eq!(
            call,
            json!({
                "model": "deployment",
                "input": "{\"subject\":\"Refund\"}",
                "questions": [
                    {"type": "choice", "choices": [{"value": "angry"}, {"value": "calm", "description": "{\"x\":1}"}],
                     "name": "tone", "instructions": ""},
                    {"type": "score", "levels": [{"label": "Low"}, {"label": "High"}],
                     "name": "urgency", "instructions": ""},
                ],
            })
        );
        let answers = json!({"model": "deployment", "answers": [
            {"type": "choice", "name": "tone", "choice": "calm", "confidence": 0.7,
             "probabilities": [{"value": "angry", "probability": 0.3}, {"value": "calm", "probability": 0.7}]},
            {"type": "score", "name": "urgency", "score": 0.9, "confidence": 0.9,
             "probabilities": [{"label": "Low", "value": 0, "probability": 0.1}, {"label": "High", "value": 1, "probability": 0.9}]},
        ]});
        let asked = call["questions"].as_array().cloned().unwrap_or_default();
        let events = events_of(&answers, &asked).expect("events");
        let written = super::super::encode_response(&events).expect("body");
        let written: Value = serde_json::from_slice(&written).unwrap_or_default();
        assert_eq!(written["answers"]["tone"]["choice"], "calm");
        assert_eq!(
            written["answers"]["urgency"]["probabilities"],
            json!({"0": 0.1, "1": 0.9})
        );
    }

    #[test]
    fn a_client_is_answered_by_a_provider_of_another_format() {
        let asked = json!([
            {"type": "predicate", "name": "spam", "instructions": "Is it spam?"},
            {"type": "score", "instructions": "Urgency?", "levels": [{"label": "low"}, {"label": "high"}]},
        ]);
        let raw = json!({"model": "jev-latest", "input": "Charged twice", "questions": asked});
        let input = decode_request(raw.to_string().as_bytes()).expect("input");
        let call = Value::Object(super::super::encode_request(&input).expect("call"));
        assert_eq!(
            call["questions"]["1"]["criteria"],
            json!([{"label": "low"}, {"label": "high"}])
        );
        let answers = json!({"model": "jev-1", "answers": {
            "spam": {"type": "noul", "noul": 0.9},
            "1": {"type": "score", "score": 1.0, "confidence": 0.6, "probabilities": {"0": 0.4, "1": 0.6},
                  "legend": {"0": {"label": "low"}, "1": {"label": "high"}}},
        }});
        let mut events = Vec::new();
        super::super::decode_response(Bytes::from(answers.to_string()), &mut events)
            .expect("events");
        let written = encode_response(&events, &asked).expect("body");
        let written: Value = serde_json::from_slice(&written).unwrap_or_default();
        assert_eq!(
            written["answers"],
            json!([
                {"type": "predicate", "name": "spam", "probability": 0.9},
                {"type": "score", "score": 1.0, "confidence": 0.6, "probabilities": [
                    {"label": "low", "value": 0, "probability": 0.4}, {"label": "high", "value": 1, "probability": 0.6}]},
            ])
        );
    }

    #[test]
    fn a_response_is_written_as_the_client_asked() {
        let asked = json!([
            {"type": "predicate", "instructions": "a"},
            {"type": "choice", "name": "x", "instructions": "b", "choices": [{"value": true}, {"value": "true"}]},
            {"type": "score", "name": null, "instructions": "c", "levels": [{"label": "low"}, {"label": "high"}]},
        ]);
        let input = json!({"model": "m", "input": "hi", "questions": asked});
        let input = decode_request(input.to_string().as_bytes()).expect("input");
        let call = encode_request(&input).expect("call");
        let sent = call["questions"].as_array().cloned().unwrap_or_default();
        let raw = json!({"model": "m", "request_id": "r", "answers": [
            {"type": "refusal", "name": "0", "reason": "no"},
            {"type": "choice", "name": "x", "choice": true, "confidence": 0.8,
             "probabilities": [{"value": true, "probability": 0.8}, {"value": "true", "probability": 0.2}]},
            {"type": "score", "name": "2", "score": 0.8, "confidence": 0.9,
             "probabilities": [{"label": "low", "value": 0, "probability": 0.2}, {"label": "high", "value": 1, "probability": 0.8}]},
        ], "usage": {"input_tokens": 3, "output_tokens": 0, "input_tokens_details": {"cached_tokens": 1}}});
        let events = events_of(&raw, &sent).expect("events");
        let written = encode_response(&events, &asked).expect("body");
        let written: Value = serde_json::from_slice(&written).unwrap_or_default();
        assert_eq!(
            written,
            json!({"model": "m", "request_id": "r", "answers": [
                {"type": "refusal", "reason": "no"},
                {"type": "choice", "name": "x", "choice": true, "confidence": 0.8,
                 "probabilities": [{"value": true, "probability": 0.8}, {"value": "true", "probability": 0.2}]},
                {"type": "score", "name": null, "score": 0.8, "confidence": 0.9,
                 "probabilities": [{"label": "low", "value": 0, "probability": 0.2}, {"label": "high", "value": 1, "probability": 0.8}]},
            ], "usage": {"input_tokens": 3, "output_tokens": 0, "total_tokens": 3,
                 "input_tokens_details": {"cached_tokens": 1, "cache_write_tokens": 0},
                 "output_tokens_details": {"reasoning_tokens": 0}}})
        );
    }

    #[test]
    fn an_answer_out_of_turn_of_another_type_or_unreadable_is_the_providers_fault() {
        let asked = [
            json!({"type": "predicate", "name": "first"}),
            json!({"type": "predicate", "name": "second"}),
        ];
        let answer =
            |name: &str, ty: &str, p: f64| json!({"type": ty, "name": name, "probability": p});
        for answers in [
            json!([answer("first", "predicate", 0.5)]),
            json!([
                answer("second", "predicate", 0.5),
                answer("first", "predicate", 0.5)
            ]),
            json!([
                answer("first", "score", 0.5),
                answer("second", "predicate", 0.5)
            ]),
            json!([
                answer("first", "predicate", 1.5),
                answer("second", "predicate", 0.5)
            ]),
        ] {
            let refused = events_of(&json!({"model": "m", "answers": answers}), &asked);
            assert!(refused.is_err_and(|f| f.is(FaultKind::Provider)));
        }
    }

    #[test]
    fn a_request_unlike_the_contract_is_refused() {
        let refused = |raw: Value| {
            decode_request(raw.to_string().as_bytes()).is_err_and(|f| f.is(FaultKind::Client))
        };
        let predicate = json!({"type": "predicate", "instructions": "a"});
        assert!(refused(json!({"model": "m", "questions": [predicate]})));
        assert!(refused(
            json!({"model": "m", "input": "x", "questions": []})
        ));
        assert!(refused(
            json!({"model": "m", "input": "x", "stream": true, "questions": [predicate]})
        ));
        assert!(refused(
            json!({"model": "m", "input": [{"role": "user", "content": [
            {"type": "input_image", "file_id": "f"}]}], "questions": [predicate]})
        ));
        assert!(refused(json!({"model": "m", "input": "x", "questions": [
            {"type": "choice", "instructions": "a", "choices": [{"value": "a"}, {"value": "a"}]}]})));
        assert!(refused(json!({"model": "m", "input": "x", "questions": [
            {"type": "score", "instructions": "a", "levels": ["low"]}]})));
    }

    #[test]
    fn another_formats_fields_are_refused() {
        let mut input = Input::new("x", vec![]);
        input.native = Some(NativeFields::new(
            "decisions",
            Map::from_iter([("extra".into(), true.into())]),
        ));
        assert!(encode_request(&input).is_err());
    }
}

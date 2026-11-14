//! Google's decisions contract, [`GeminiDecisions`]: evidence as `input`,
//! content blocks for media, `criteria` by question type.

use super::*;
use respan_gateway_conversation_http::gemini::GeminiGenerate;
use respan_gateway_http::KeyIn;

const GEMINI: &str = "gemini-decisions";

type Key = crate::Own<GeminiDecisions>;

/// Google's decisions contract, as the Gemini API serves it: a client's
/// `POST` to `/v1beta/decisions`, and a provider called at `/decisions`
/// under a base URL that includes the version, with its key in
/// `x-goog-api-key`.
///
/// A request is `{"model", "input", "questions": {id: question}}`: `input`
/// a text, a content block or a list of blocks, which is the kind's `state`
/// (adjacent text blocks are one text); each question `{"type",
/// "instructions", "criteria"}` (`instructions` a text, or text blocks read as
/// one): `noul` with optional criteria
/// `{"true_description", "false_description"}`, `choice` with `{option:
/// description}`, `score` with a list of levels. A response is `{"answers":
/// {id: answer}, "usage"}`, each answer as `decisions` has it plus a
/// `confidence`, and a score's `legend`, the levels it was asked; the usage
/// `total_input_tokens` and `total_output_tokens` beside token counts the
/// kind has no place for. A request blocked by Google's input filters is a
/// 400 with the reason.
///
/// An `input` of media (an image, audio, video or a document) stays this
/// format's own: the kind carries its text, and another format refuses a
/// request that has media, 400, before any call. A question's criteria for
/// a `noul`, and a question or answer of a type the kind has no form for,
/// are kept and written back as `decisions` keeps them. An answer's
/// `confidence`, when a provider gives none, is the probability of its top
/// outcome, as Google defines it. A request of `openai-decisions` has its
/// user messages sent as text, or as content blocks when one holds an
/// image given as a base64 `data:` URL; an image otherwise, or question
/// instructions that are not text, are refused, 400, before any call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiDecisions;

impl GeminiDecisions {
    /// The format's name, `gemini-decisions`.
    pub const NAME: &'static str = GEMINI;
    /// The path its clients are served at.
    pub const PATH: &'static str = "/v1beta/decisions";
}

impl Natives for GeminiDecisions {}

/// The type of the first block of media in the evidence `input` carries
/// for this format alone, if any.
pub(crate) fn media_of(input: &Input) -> Option<&str> {
    let blocks = input.native.fields::<Key>().get("input")?;
    let blocks = blocks.as_array().into_iter().flatten();
    blocks
        .filter_map(|b| b["type"].as_str())
        .find(|t| *t != "text")
}

impl Format<ClientIo, Input, Output> for GeminiDecisions {
    fn name(&self) -> &str {
        GEMINI
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST && path_ends_with(request.uri(), Self::PATH)
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

/// Its spec's: the key in `x-goog-api-key`, over HTTP.
impl Upstream for GeminiDecisions {
    fn name(&self) -> &str {
        GEMINI
    }

    fn key(&self) -> KeyIn {
        KeyIn::Header("x-goog-api-key")
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for GeminiDecisions {
    fn name(&self) -> &str {
        GEMINI
    }

    /// A call: a `POST` of the whole request to `/decisions`.
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(io.with_refusal(read_provider_error).bind(
            move |head, input: Option<&Input>| {
                let input = input.ok_or_else(|| {
                    Fault::config(format!("{GEMINI} does not accept streaming inputs"))
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

/// The evidence of `input` as the kind's state, and its blocks if any is
/// media: adjacent text blocks are one text, as Google reads them.
fn read_input(input: Value) -> Result<(Value, Option<Value>), Fault> {
    let blocks = match input {
        Value::String(_) => return Ok((input, None)),
        Value::Object(_) => vec![input],
        Value::Array(blocks) if !blocks.is_empty() => blocks,
        _ => return Err(invalid("input", "must be a text or content blocks")),
    };
    let mut text = String::new();
    let mut media = false;
    for block in &blocks {
        match (block["type"].as_str(), block["text"].as_str()) {
            (Some("text"), Some(t)) => text.push_str(t),
            (Some("text"), None) => return Err(invalid("input", "has a text block with no text")),
            (Some(_), _) => media = true,
            _ => return Err(invalid("input", "must hold blocks with a type")),
        }
    }
    Ok((text.into(), media.then(|| blocks.into())))
}

/// A client's request.
fn decode_request(raw: &[u8]) -> Result<Input, Fault> {
    let Ok(Value::Object(mut map)) = serde_json::from_slice(raw) else {
        return Err(invalid("body", "must be a JSON object"));
    };
    let evidence = map
        .remove("input")
        .ok_or_else(|| invalid("input", "is required"))?;
    let (state, media) = read_input(evidence)?;
    let Some(Value::Object(questions)) = map.remove("questions") else {
        return Err(invalid("questions", "must be an object of questions"));
    };
    if questions.is_empty() {
        return Err(invalid("questions", "must be an object of questions"));
    }
    let questions = questions.into_iter().map(|(id, q)| decode_question(id, q));
    let mut input = Input::new(state, questions.collect::<Result<Vec<_>, _>>()?);
    input.model = match map.remove("model") {
        Some(Value::String(model)) => Some(model),
        _ => return Err(invalid("model", "must be a string")),
    };
    if let Some(blocks) = media {
        map.insert("input".into(), blocks);
    }
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
    let Some(instructions) = q.remove("instructions").filter(|v| !v.is_null()) else {
        return Err(invalid(&param, "has no instructions"));
    };
    let instructions = instructions_text(instructions)
        .ok_or_else(|| invalid(&param, "has instructions that are not text"))?;
    let criteria = q.remove("criteria").unwrap_or_default();
    let ask = match q.remove("type").as_ref().and_then(Value::as_str) {
        // The kind has no form for a noul's criteria; any sent ride as the format's own.
        Some("noul") => {
            if !criteria.is_null() {
                q.insert("criteria".into(), criteria);
            }
            Ask::Noul
        }
        Some("choice") => {
            let Value::Object(options) = criteria else {
                let what = "has criteria other than an object of options";
                return Err(invalid(&param, what));
            };
            let mut asked = Vec::with_capacity(options.len());
            for (name, about) in options {
                let about = match about {
                    Value::Null => None,
                    Value::String(text) => Some(text).filter(|t| !t.is_empty()).map(Value::from),
                    _ => return Err(invalid(&param, "has an option described by no text")),
                };
                asked.push((name, about));
            }
            Ask::Choice { options: asked }
        }
        Some("score") => match criteria {
            Value::Array(levels) if levels.iter().all(Value::is_string) => Ask::Score { levels },
            _ => return Err(invalid(&param, "has criteria other than a list of levels")),
        },
        _ => return Err(invalid(&param, "has no type")),
    };
    let mut question = Question::new(id, ask);
    question.instructions = Some(instructions);
    question.native = Native::json::<Key>(q);
    Ok(question)
}

/// The text of a block, if it is a text block.
fn text_of(block: &Value) -> Option<&str> {
    (block["type"] == "text")
        .then(|| block["text"].as_str())
        .flatten()
}

/// Question instructions, a text, a text block or a list of them, as one text.
fn instructions_text(instructions: Value) -> Option<Value> {
    match &instructions {
        Value::String(_) => Some(instructions),
        Value::Object(_) => text_of(&instructions).map(Value::from),
        Value::Array(blocks) if !blocks.is_empty() => (blocks.iter().map(text_of))
            .collect::<Option<String>>()
            .map(Value::from),
        _ => None,
    }
}

/// The evidence `state` as Google's `input`: a text as is; OpenAI's user
/// messages as text, or as content blocks if one holds an image; other JSON
/// as its text, as Google asks of structured data.
fn evidence_of(state: &Value) -> Result<Value, Fault> {
    let Some(messages) = state
        .as_array()
        .filter(|messages| !messages.is_empty() && openai::is_evidence(state))
    else {
        return Ok(match state {
            Value::String(_) => state.clone(),
            other => other.to_string().into(),
        });
    };
    let mut blocks: Vec<Value> = Vec::new();
    for (i, message) in messages.iter().enumerate() {
        // Google joins adjacent text blocks with no separator.
        if i > 0 {
            blocks.push(json!({"type": "text", "text": "\n"}));
        }
        let content = &message["content"];
        let parts = match content {
            Value::String(_) => std::slice::from_ref(content),
            _ => content.as_array().map_or(&[][..], Vec::as_slice),
        };
        for part in parts {
            blocks.push(match part["image_url"].as_str() {
                // OpenAI's image part carries no mime type, which Google's `uri` form expects.
                Some(url) => {
                    let inline = url.strip_prefix("data:").and_then(|rest| {
                        let (params, data) = rest.split_once(',')?;
                        let mime = params.strip_suffix(";base64")?.split(';').next()?;
                        (!mime.is_empty() && !data.is_empty()).then_some((mime, data))
                    });
                    let (mime, data) = inline.ok_or_else(|| {
                        let what = "an image only as a base64 data: URL with a mime type";
                        Fault::bad_request(format!("{GEMINI} takes {what}"))
                    })?;
                    json!({"type": "image", "data": data, "mime_type": mime})
                }
                None => json!({"type": "text", "text": part.get("text").unwrap_or(part)}),
            });
        }
    }
    if blocks.is_empty() {
        return Err(Fault::bad_request(format!(
            "{GEMINI} cannot take empty evidence"
        )));
    }
    if blocks.iter().all(|b| b["type"] == "text") {
        let text: String = blocks.iter().filter_map(text_of).collect();
        return Ok(text.into());
    }
    Ok(blocks.into())
}

/// The call of `input`.
fn encode_request(input: &Input) -> Result<Map<String, Value>, Fault> {
    let mut questions = Map::new();
    for q in &input.questions {
        if let Ask::Native(native) = &q.ask
            && let Some(question) = native.value::<Key>()
        {
            questions.insert(q.id.clone(), question.clone());
            continue;
        }
        let refused = |what: &str| {
            Fault::bad_request(format!("{GEMINI} cannot ask question '{}': {what}", q.id))
        };
        let (kind, criteria) = match &q.ask {
            Ask::Noul => ("noul", None),
            Ask::Choice { options } => {
                let options = options.iter().map(|(name, about)| {
                    let about = match about {
                        None => String::new(),
                        Some(Value::String(text)) => text.clone(),
                        Some(other) => other.to_string(),
                    };
                    (name.clone(), Value::from(about))
                });
                ("choice", Some(Value::Object(options.collect())))
            }
            Ask::Score { levels } => {
                let levels = levels.iter().map(|l| {
                    level_text(l)
                        .cloned()
                        .ok_or_else(|| refused("a level has no text"))
                });
                ("score", Some(levels.collect::<Result<Value, _>>()?))
            }
            // WARN: Gemini has no form for another format's question; it is
            // dropped and goes unanswered.
            _ => continue,
        };
        let Some(instructions) = q.instructions.clone() else {
            return Err(refused("it has no instructions"));
        };
        let instructions = instructions_text(instructions)
            .ok_or_else(|| refused("its instructions are not text"))?;
        let mut v = Map::new();
        v.insert("type".into(), kind.into());
        v.insert("instructions".into(), instructions);
        set(&mut v, "criteria", criteria);
        merge_missing(&mut v, q.native.fields::<Key>());
        questions.insert(q.id.clone(), v.into());
    }
    if questions.is_empty() && !input.questions.is_empty() {
        return Err(Fault::bad_request(format!(
            "{GEMINI} has no form for any question asked"
        )));
    }
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    v.insert("input".into(), evidence_of(&input.state)?);
    v.insert("questions".into(), questions.into());
    // Media, which only this format's clients send, is sent with the state's
    // text: the client's own blocks while the state is still as it sent them.
    let own = input.native.fields::<Key>();
    if let Some(Value::Array(blocks)) = own.get("input") {
        let sent: String = blocks.iter().filter_map(text_of).collect();
        let blocks: Vec<Value> = if input.state.as_str() == Some(sent.as_str()) {
            blocks.clone()
        } else {
            let text = json!({"type": "text", "text": v["input"]});
            let media = blocks.iter().filter(|b| text_of(b).is_none());
            std::iter::once(text).chain(media.cloned()).collect()
        };
        v.insert("input".into(), blocks.into());
    }
    merge_missing(&mut v, own);
    Ok(v)
}

/// The contract of Google's decisions: its usage keys, and no `model` written.
const WIRE: Wire = Wire {
    input_tokens: "total_input_tokens",
    output_tokens: "total_output_tokens",
    model: false,
    total_tokens: true,
};

/// The events of a provider's whole answer.
fn decode_response(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v: Value = serde_json::from_slice(&raw).unwrap_or_default();
    // Blocked input comes with no answers.
    if let Some(reason) = v["prompt_feedback"]["block_reason"].as_str() {
        let said = v["prompt_feedback"]["block_reason_message"].as_str();
        let said = said.map_or_else(|| format!("the input was blocked: {reason}"), str::to_owned);
        return Err(Fault::bad_request(said));
    }
    read_response::<Key>(&v, &WIRE, GEMINI, out)
}

/// A whole response of `events` to a client that asked the questions `asked`.
fn encode_response(events: &[Output], asked: &Asked) -> Result<Bytes, Fault> {
    write_response::<Key>(events, &WIRE, |answer| answer_value(answer, asked))
}

/// An answer as Google writes it: its `confidence` and a score's `legend`
/// as the provider gave them, else as Google defines them, from the
/// probabilities and the levels the client asked.
fn answer_value(answer: &Answer, asked: &Asked) -> Result<Map<String, Value>, Fault> {
    let mut v = encode_answer::<Key>(answer)?;
    if !v.contains_key("confidence") {
        let confidence = match &answer.verdict {
            Verdict::Noul(p) => p.max(1.0 - p),
            Verdict::Choice { probabilities, .. } => {
                (probabilities.iter().map(|(_, p)| *p)).fold(0.0, f64::max)
            }
            Verdict::Score { probabilities, .. } => {
                probabilities.iter().copied().fold(0.0, f64::max)
            }
            _ => return Ok(v),
        };
        v.insert("confidence".into(), confidence.into());
    }
    add_legend(&mut v, answer, asked);
    Ok(v)
}

/// A refusal in Google's error schema, as `generateContent` writes it.
fn refusal(fault: &Fault, request: &http::Request<Bytes>, head: &mut ResponseHead) -> Bytes {
    GeminiGenerate.refusal(fault, request, head)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::FaultKind;

    fn request() -> Value {
        json!({
            "model": "gemini-decision-1",
            "input": "The blue mug I ordered on Monday arrived cracked on Wednesday.",
            "questions": {
                "is_damaged": {"type": "noul", "instructions": "Did the item arrive damaged?", "criteria": {"true_description": "Broken"}},
                "dept": {"type": "choice", "instructions": "Which department?", "criteria": {"billing": "Payments", "shipping": "Delivery", "other": ""}},
                "sev": {"type": "score", "instructions": "How severe?", "criteria": ["Cosmetic", "Workaround", "Blocked"]},
            },
        })
    }

    /// A body Google answered the questions of [`request`] with.
    fn response() -> Value {
        json!({
            "answers": {
                "dept": {"confidence": 0.9070535064954768, "choice": "shipping", "probabilities": {"other": 0.08460778314934696, "billing": 0.008338710355176195, "shipping": 0.9070535064954768}, "type": "choice"},
                "sev": {"confidence": 0.7043774474250762, "score": 1.5987554347262594, "probabilities": {"0": 0.10562201269881678, "1": 0.19000053987610696, "2": 0.7043774474250762}, "legend": {"2": "Blocked", "0": "Cosmetic", "1": "Workaround"}, "type": "score"},
                "is_damaged": {"confidence": 0.99, "noul": 0.99, "type": "noul"},
            },
            "usage": {"total_tokens": 199, "total_input_tokens": 199, "input_tokens_by_modality": [{"modality": "text", "tokens": 199}], "total_output_tokens": 0},
        })
    }

    #[test]
    fn a_request_and_its_response_are_written_back_as_they_came() {
        let input = decode_request(request().to_string().as_bytes()).expect("input");
        assert_eq!(
            Value::Object(encode_request(&input).expect("call")),
            request()
        );
        let mut events = Vec::new();
        decode_response(Bytes::from(response().to_string()), &mut events).expect("events");
        assert!(events.contains(&Output::Usage(Usage::new(199, 0))));
        let body = request().to_string();
        let written = encode_response(&events, &Asked::new(body.as_bytes())).expect("body");
        let written: Value = serde_json::from_slice(&written).unwrap_or_default();
        assert_eq!(written, response());
        let mut extra = response();
        extra["answers"]["is_damaged"]["extra"] = json!(1);
        let mut events = Vec::new();
        decode_response(Bytes::from(extra.to_string()), &mut events).expect("events");
        let to_gemini = encode_response(&events, &Asked::new(body.as_bytes())).expect("body");
        let to_gemini: Value = serde_json::from_slice(&to_gemini).unwrap_or_default();
        assert_eq!(to_gemini["answers"]["is_damaged"]["extra"], 1);
        let to_decisions = crate::encode_response(&events, &Asked::new(b"{}")).expect("body");
        let to_decisions: Value = serde_json::from_slice(&to_decisions).unwrap_or_default();
        assert_eq!(to_decisions["answers"]["is_damaged"].get("extra"), None);
    }

    #[test]
    fn an_answer_of_another_format_is_written_with_the_confidence_and_legend_google_defines() {
        let mut answer = Answer::new(
            "sev",
            Verdict::Score {
                score: 1.5,
                probabilities: vec![0.1, 0.2, 0.7],
            },
        );
        let body = request().to_string();
        let asked = Asked::new(body.as_bytes());
        let written = answer_value(&answer, &asked).expect("answer");
        assert_eq!(written["confidence"], 0.7);
        assert_eq!(
            written["legend"],
            json!({"0": "Cosmetic", "1": "Workaround", "2": "Blocked"})
        );
        answer.verdict = Verdict::Noul(0.03);
        assert_eq!(
            answer_value(&answer, &asked).expect("answer")["confidence"],
            0.97
        );
    }

    #[test]
    fn media_evidence_is_kept_for_google_and_refused_by_the_other_formats() {
        let mut raw = request();
        raw["input"] = json!([
            {"type": "text", "text": "Customer photo."},
            {"type": "image", "data": "aGk=", "mime_type": "image/png", "resolution": "low"},
        ]);
        let input = decode_request(raw.to_string().as_bytes()).expect("input");
        assert_eq!(input.state, json!("Customer photo."));
        assert_eq!(Value::Object(encode_request(&input).expect("call")), raw);
        assert!(crate::encode_request(&input).is_err_and(|f| f.is(FaultKind::Client)));
        let mut redacted = input.clone();
        redacted.state = json!("[redacted]");
        assert_eq!(
            encode_request(&redacted).expect("call")["input"],
            json!([{"type": "text", "text": "[redacted]"}, raw["input"][1]])
        );
        let texts = json!([{"type": "text", "text": "a "}, {"type": "text", "text": "b"}]);
        assert_eq!(read_input(texts).expect("text").0, json!("a b"));
    }

    #[test]
    fn evidence_or_instructions_of_another_format_google_would_get_as_json_are_not_sent() {
        let mut question = Question::new("q", Ask::Noul);
        question.instructions = Some(json!("i"));
        let sent = |state: Value| {
            let input = Input::new(state, vec![question.clone()]);
            encode_request(&input).map(|v| v["input"].clone())
        };
        // As OpenAI's decoder keeps user messages.
        let user = |parts: Value| json!([{"role": "user", "content": parts}]);
        let text = json!({"type": "input_text", "text": "hi"});
        assert_eq!(sent(user(json!([text]))).expect("call"), json!("hi"));
        let image = |url: &str| json!({"type": "input_image", "image_url": url});
        let inline = image("data:image/png;base64,aGk=");
        let state =
            json!([{"role": "user", "content": "a"}, {"role": "user", "content": [text, inline]}]);
        assert_eq!(
            sent(state).expect("call"),
            json!([{"type": "text", "text": "a"}, {"type": "text", "text": "\n"},
                {"type": "text", "text": "hi"},
                {"type": "image", "data": "aGk=", "mime_type": "image/png"}])
        );
        let params = image("data:image/png;charset=x;base64,aGk=");
        assert_eq!(
            sent(user(json!([params]))).expect("call")[0]["mime_type"],
            "image/png"
        );
        for url in [
            "data:;base64,aGk=",
            "data:image/png;base64,",
            "data:image/svg+xml,<svg/>",
        ] {
            assert!(sent(user(json!([image(url)]))).is_err());
        }
        assert!(sent(user(json!([]))).is_err());
        assert_eq!(sent(json!([])).expect("call"), json!("[]"));
        let remote = sent(user(json!([image("https://example.com/a.png")])));
        assert!(remote.is_err_and(|f| f.is(FaultKind::Client)));
        let record = json!([{"content": [{"type": "input_image", "sku": "1"}]}]);
        assert_eq!(
            sent(record.clone()).expect("call"),
            json!(record.to_string())
        );
        let blocks = json!([{"type": "text", "text": "Is it "}, {"type": "text", "text": "spam?"}]);
        let asking = |i: Value| json!({"state": "s", "questions": {"q": {"type": "noul", "instructions": i}}});
        let input = crate::decode_request(asking(blocks).to_string().as_bytes()).expect("input");
        let sent = encode_request(&input).map(|v| v["questions"]["q"]["instructions"].clone());
        assert_eq!(sent.expect("call"), json!("Is it spam?"));
        let image = json!([{"type": "image", "data": "aGk="}]);
        let input = crate::decode_request(asking(image).to_string().as_bytes()).expect("input");
        assert!(encode_request(&input).is_err_and(|f| f.is(FaultKind::Client)));
    }

    #[test]
    fn a_malformed_question_or_a_block_by_google_is_refused() {
        let refused = |raw: Value| {
            decode_request(raw.to_string().as_bytes()).is_err_and(|f| f.is(FaultKind::Client))
        };
        let asking = |q: Value| json!({"model": "m", "input": "s", "questions": {"q": q}});
        assert!(refused(asking(
            json!({"type": "choice", "instructions": "i", "criteria": ["a"]})
        )));
        assert!(refused(asking(
            json!({"type": "score", "instructions": "i", "criteria": {}})
        )));
        assert!(refused(asking(json!({"type": "noul"}))));
        let blocks = json!([{"type": "text", "text": "a "}, {"type": "text", "text": "b"}]);
        let asked = asking(json!({"type": "noul", "instructions": blocks}));
        let input = decode_request(asked.to_string().as_bytes()).expect("input");
        assert_eq!(input.questions[0].instructions, Some(json!("a b")));
        let image = json!([{"type": "image", "data": "aGk="}]);
        assert!(refused(asking(
            json!({"type": "noul", "instructions": image})
        )));
        assert!(refused(
            json!({"model": "m", "input": "s", "questions": {}})
        ));
        let blocked = json!({"prompt_feedback": {"block_reason": "safety"}, "answers": {}});
        let blocked = decode_response(Bytes::from(blocked.to_string()), &mut Vec::new());
        assert!(blocked.is_err_and(|f| f.is(FaultKind::Client)));
    }
}

//! OpenAI Audio Transcriptions and Translations: `POST
//! /audio/transcriptions` and `POST /audio/translations`, a form, answered
//! in the `response_format` asked (JSON, or text the provider rendered) or,
//! if asked, as a stream of events.

use super::multipart::{self, Writer};
use super::{
    Fields, Form, as_usage_extras, bind_client, bytes_of, event_json, file_name, foreign, invalid,
    is_true, json_bytes, kept, merge_missing, missing, native_of, post_to, read_usage, rest_of,
    seconds_json, sends, set, sse_event, streams, take_number, take_string, unreadable,
    usage_extras, whole_json, write_field, write_usage,
};
use crate::dialect::{self, Spec};
use crate::edge::ended_events;
use respan_gateway_conversation::Usage;
use respan_gateway_core::Provider;
use respan_gateway_core::{Connection, Fault, Format, NativeValue};
use respan_gateway_http::bytes::{Bytes, BytesMut};
use respan_gateway_http::content;
use respan_gateway_http::http::{self, Method, request::Parts as RequestHead};
use respan_gateway_http::path::path_ends_with;
use respan_gateway_http::{All, ClientIo, Decoder, Encoder, ProviderIo, Whole};
use respan_gateway_http::{Connect, Upstream};
use respan_gateway_media::transcription::{Delta, Input, Output, Segment, Task, Transcript, Word};
use serde_json::{Map, Value};

const NAME: &str = "openai-transcriptions";
const TRANSCRIPTIONS: &str = "/audio/transcriptions";
const TRANSLATIONS: &str = "/audio/translations";

/// OpenAI Audio Transcriptions and Translations, as OpenAI or a compatible
/// vendor (Groq, a self-hosted Whisper server) serves them: a `POST` of a
/// form to a path ending in `/audio/transcriptions` or `/audio/translations`,
/// the audio its `file`.
///
/// `response_format` and the fields the kind has no place for stay native.
/// A JSON answer (`json`, `verbose_json`, `diarized_json`) decodes into a
/// [`Transcript`]; an answer of text (`text`, `srt`, `vtt`) into
/// [`Output::Rendered`], kept as the provider rendered it; a stream into
/// [`Output::Delta`]s and the transcript of its `transcript.text.done`,
/// failing `truncated` without one. Usage of `{"type": "tokens"}` is
/// [`Output::Usage`], of `{"type": "duration"}` [`Output::Seconds`].
///
/// A client is answered in the `response_format` it asked for: rendered
/// text as it came, as `text/plain`, or, if the provider gave JSON, the
/// transcript's text, or SubRip or WebVTT of its segments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenAiTranscriptions;

impl OpenAiTranscriptions {
    /// The format's name, `openai-transcriptions`, which tags its native
    /// fields.
    pub const NAME: &'static str = NAME;
}

impl Format<ClientIo, Input, Output> for OpenAiTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn matches(&self, io: &ClientIo) -> bool {
        let request = io.request();
        request.method() == Method::POST
            && [TRANSCRIPTIONS, TRANSLATIONS]
                .iter()
                .any(|path| path_ends_with(request.uri(), path))
    }

    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(bind_client(io, decode_request, response_encoder))
    }
}

/// Its spec's: a bearer key, over HTTP, beside its base URL.
impl Upstream for OpenAiTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for OpenAiTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(dialect::bind(*self, io))
    }
}

/// Its spec's parts, which a vendor's dialect runs around.
impl Spec for OpenAiTranscriptions {
    type Input = Input;
    type Output = Output;
    const NAME: &'static str = NAME;

    fn streams(&self, io: &ProviderIo) -> bool {
        let paths = [TRANSCRIPTIONS, TRANSLATIONS];
        streams(io, &paths, |f| is_true(f.get("stream")))
    }

    fn post(
        &self,
        head: &mut RequestHead,
        input: &Input,
        stream: bool,
    ) -> Result<Box<dyn Encoder<Input>>, Fault> {
        post(head, input, stream)
    }

    fn read_response(
        &self,
        head: &http::response::Parts,
        _: &http::Request<Bytes>,
    ) -> Option<Box<dyn Decoder<Output>>> {
        Some(read_response(head))
    }
}

/// A client's request, a translation if to `/audio/translations`.
fn decode_request(head: &RequestHead, form: &Form) -> Result<Input, Fault> {
    let Fields { mut map, files } = form.fields()?;
    let mut audio = None;
    for (name, media) in files {
        match name.as_str() {
            "file" if audio.is_none() => audio = Some(media),
            _ => return Err(invalid(&name, "is not a file this endpoint takes")),
        }
    }
    let mut input = Input::new(audio.ok_or_else(|| missing("file"))?);
    if path_ends_with(&head.uri, TRANSLATIONS) {
        input.task = Task::Translate;
    }
    input.model = take_string(&mut map, "model")?;
    input.language = take_string(&mut map, "language")?;
    input.prompt = take_string(&mut map, "prompt")?;
    input.temperature = take_number(&mut map, "temperature")?;
    map.remove("stream");
    input.native = native_of(NAME, map);
    Ok(input)
}

/// The call of `input`: a form of its audio and fields.
fn post(
    head: &mut RequestHead,
    input: &Input,
    stream: bool,
) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let path = match input.task {
        Task::Translate => TRANSLATIONS,
        _ => TRANSCRIPTIONS,
    };
    let audio = bytes_of(&input.audio)
        .ok_or_else(|| Fault::bad_request("audio at a URL cannot be sent as a file"))?;
    let mut form = Writer::with_boundary(multipart::random_boundary());
    post_to(head, path, &form.content_type())?;
    let name = file_name(&input.audio, "audio");
    form.file("file", &name, &input.audio.mime_type, audio);
    let mut v = Map::new();
    set(&mut v, "model", input.model.clone());
    set(&mut v, "language", input.language.clone());
    set(&mut v, "prompt", input.prompt.clone());
    set(&mut v, "temperature", input.temperature);
    merge_missing(&mut v, &kept(NAME, &input.native)?);
    if stream {
        v.insert("stream".into(), true.into());
    }
    for (name, value) in &v {
        write_field(&mut form, name, value);
    }
    Ok(sends(form.finish()))
}

/// The decoder of a provider's answer of `head`: a stream of events, a JSON
/// body, or rendered text.
fn read_response(head: &http::response::Parts) -> Box<dyn Decoder<Output>> {
    if content::is_stream(head) {
        return ended_events(stream_event);
    }
    let content_type = content::content_type(head).unwrap_or_default();
    let json = content_type.to_ascii_lowercase().contains("json");
    Box::new(Whole::new(
        move |raw: Bytes, out: &mut Vec<Output>| match json {
            true => decode_json(raw, out),
            false => {
                out.push(Output::Rendered(String::from_utf8_lossy(&raw).into_owned()));
                Ok(())
            }
        },
    ))
}

/// The events of a whole JSON body.
pub(crate) fn decode_json(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let v = whole_json(NAME, raw, |v| v["text"].is_string())?;
    let mut transcript = Transcript::new(v["text"].as_str().unwrap_or_default());
    transcript.language = v["language"].as_str().map(Into::into);
    transcript.duration = v["duration"].as_f64();
    for s in v["segments"].as_array().into_iter().flatten() {
        let mut segment = Segment::new(time(&s["start"])?, time(&s["end"])?, text(&s["text"]));
        segment.speaker = s["speaker"].as_str().map(Into::into);
        segment.native = native_of(NAME, rest_of(s, &["start", "end", "text", "speaker"]));
        transcript.segments.push(segment);
    }
    for w in v["words"].as_array().into_iter().flatten() {
        let mut word = Word::new(text(&w["word"]), time(&w["start"])?, time(&w["end"])?);
        word.native = native_of(NAME, rest_of(w, &["word", "start", "end"]));
        transcript.words.push(word);
    }
    let typed = ["text", "language", "duration", "segments", "words", "usage"];
    transcript.native = native_of(NAME, rest_of(&v, &typed));
    out.push(Output::Transcript(transcript));
    read_usage_into(&v["usage"], out)
}

/// A time in seconds; none is 0.
fn time(v: &Value) -> Result<f64, Fault> {
    match v {
        Value::Null => Ok(0.0),
        v => v
            .as_f64()
            .ok_or_else(|| unreadable(NAME, "holds a time that is no number")),
    }
}

/// A text; none is empty.
fn text(v: &Value) -> String {
    v.as_str().unwrap_or_default().to_owned()
}

/// The events of a usage object, if `u` is one: seconds of audio, or
/// tokens.
fn read_usage_into(u: &Value, out: &mut Vec<Output>) -> Result<(), Fault> {
    if !u.is_object() {
        return Ok(());
    }
    let extras = match u["type"].as_str() {
        Some("duration") => {
            let seconds = u["seconds"].as_f64();
            let seconds =
                seconds.ok_or_else(|| unreadable(NAME, "bills a duration that is no number"))?;
            out.push(Output::Seconds(seconds));
            rest_of(u, &["type", "seconds"])
        }
        _ => {
            let (usage, extras) = read_usage(u, &["type"])?;
            out.push(Output::Usage(usage));
            extras
        }
    };
    out.extend(usage_extras(NAME, extras).map(Output::Native));
    Ok(())
}

/// The events of one stream event; `done` once it said its end.
fn stream_event(done: &mut bool, raw: &[u8], out: &mut Vec<Output>) -> Result<(), Fault> {
    let Some(v) = event_json(NAME, raw)? else {
        return Ok(());
    };
    match v["type"].as_str() {
        Some("transcript.text.delta") => {
            let mut delta = Delta::new(text(&v["delta"]));
            delta.native = native_of(NAME, rest_of(&v, &["type", "delta"]));
            out.push(Output::Delta(delta));
        }
        Some("transcript.text.done") => {
            let mut transcript = Transcript::new(text(&v["text"]));
            transcript.native = native_of(NAME, rest_of(&v, &["type", "text", "usage"]));
            out.push(Output::Transcript(transcript));
            *done = true;
            read_usage_into(&v["usage"], out)?;
        }
        _ => out.push(Output::Native(NativeValue::new(NAME, v))),
    }
    Ok(())
}

/// The encoder of a response to a client's `request`, in the framing and
/// `response_format` it asked for.
fn response_encoder(
    _request: &http::Request<Bytes>,
    asked: &Map<String, Value>,
    head: &mut http::response::Parts,
) -> Box<dyn Encoder<Output>> {
    if is_true(asked.get("stream")) {
        super::content_type(head, "text/event-stream");
        return Box::new(StreamEncoder::default());
    }
    let format = asked.get("response_format").and_then(Value::as_str);
    match format {
        Some(format @ ("text" | "srt" | "vtt")) => {
            let format = format.to_owned();
            super::content_type(head, "text/plain; charset=utf-8");
            Box::new(All::new(move |events: &[Output]| render(events, &format)))
        }
        _ => {
            super::content_type(head, "application/json");
            Box::new(All::new(encode_json))
        }
    }
}

/// A response's events, folded.
#[derive(Debug, Default)]
struct Folded {
    transcript: Option<Transcript>,
    deltas: String,
    rendered: Option<String>,
    tokens: Option<Usage>,
    seconds: Option<f64>,
    extras: Map<String, Value>,
}

impl Folded {
    /// Fold `event`; an event of another format is refused, and an event a
    /// body has no place for is dropped.
    fn push(&mut self, event: &Output) -> Result<(), Fault> {
        match event {
            Output::Delta(delta) => self.deltas.push_str(&delta.text),
            Output::Transcript(t) => self.transcript = Some(t.clone()),
            Output::Rendered(text) => self.rendered = Some(text.clone()),
            Output::Usage(u) => *self.tokens.get_or_insert_with(Usage::default) += u,
            Output::Seconds(s) => *self.seconds.get_or_insert(0.0) += s,
            Output::Native(n) if &*n.format != NAME => return Err(foreign(NAME, n)),
            Output::Native(n) => {
                if let Some(more) = as_usage_extras(&n.value) {
                    merge_missing(&mut self.extras, more);
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn of(events: &[Output]) -> Result<Self, Fault> {
        let mut folded = Self::default();
        events.iter().try_for_each(|e| folded.push(e))?;
        Ok(folded)
    }

    /// All of the transcript's text.
    fn text(&self) -> String {
        match (&self.transcript, &self.rendered) {
            (Some(t), _) => t.text.clone(),
            (None, Some(rendered)) => rendered.clone(),
            (None, None) => self.deltas.clone(),
        }
    }

    /// The usage reported, as OpenAI writes it: tokens, else seconds.
    fn usage(&mut self) -> Option<Value> {
        let mut u = match (self.tokens, self.seconds) {
            (Some(tokens), _) => {
                let mut u = Map::new();
                u.insert("type".into(), "tokens".into());
                u.extend(write_usage(&tokens));
                u
            }
            (None, Some(seconds)) => {
                let mut u = Map::new();
                u.insert("type".into(), "duration".into());
                u.insert("seconds".into(), seconds_json(seconds));
                u
            }
            (None, None) => return None,
        };
        merge_missing(&mut u, &std::mem::take(&mut self.extras));
        Some(u.into())
    }
}

/// A whole JSON response of `events`.
fn encode_json(events: &[Output]) -> Result<Bytes, Fault> {
    let mut folded = Folded::of(events)?;
    let mut body = Map::new();
    body.insert("text".into(), folded.text().into());
    let usage = folded.usage();
    if let Some(t) = &folded.transcript {
        set(&mut body, "language", t.language.clone());
        set(&mut body, "duration", t.duration.map(seconds_json));
        if !t.segments.is_empty() {
            let segments: Result<Vec<Value>, Fault> = t.segments.iter().map(segment_json).collect();
            body.insert("segments".into(), segments?.into());
        }
        if !t.words.is_empty() {
            let words: Result<Vec<Value>, Fault> = t.words.iter().map(word_json).collect();
            body.insert("words".into(), words?.into());
        }
    }
    set(&mut body, "usage", usage);
    if let Some(t) = &folded.transcript {
        merge_missing(&mut body, &kept(NAME, &t.native)?);
    }
    json_bytes(&body)
}

fn segment_json(s: &Segment) -> Result<Value, Fault> {
    let mut v = Map::new();
    v.insert("start".into(), s.start.into());
    v.insert("end".into(), s.end.into());
    v.insert("text".into(), s.text.clone().into());
    set(&mut v, "speaker", s.speaker.clone());
    merge_missing(&mut v, &kept(NAME, &s.native)?);
    Ok(v.into())
}

fn word_json(w: &Word) -> Result<Value, Fault> {
    let mut v = Map::new();
    v.insert("word".into(), w.word.clone().into());
    v.insert("start".into(), w.start.into());
    v.insert("end".into(), w.end.into());
    merge_missing(&mut v, &kept(NAME, &w.native)?);
    Ok(v.into())
}

/// A response of `events` rendered as `format` asks: as the provider
/// rendered it, else from the transcript.
fn render(events: &[Output], format: &str) -> Result<String, Fault> {
    let folded = Folded::of(events)?;
    if let Some(rendered) = folded.rendered {
        return Ok(rendered);
    }
    let text = folded.text();
    let cues: Vec<(f64, f64, &str)> = match &folded.transcript {
        Some(t) if !t.segments.is_empty() => t
            .segments
            .iter()
            .map(|s| (s.start, s.end, s.text.trim()))
            .collect(),
        t => {
            let end = t.as_ref().and_then(|t| t.duration).unwrap_or(0.0);
            vec![(0.0, end, text.trim())]
        }
    };
    let mut out = String::new();
    match format {
        "srt" => {
            for (i, (start, end, text)) in cues.iter().enumerate() {
                let (start, end) = (timestamp(*start, ','), timestamp(*end, ','));
                out.push_str(&format!("{}\n{start} --> {end}\n{text}\n\n", i + 1));
            }
        }
        "vtt" => {
            out.push_str("WEBVTT\n\n");
            for (start, end, text) in cues {
                let (start, end) = (timestamp(start, '.'), timestamp(end, '.'));
                out.push_str(&format!("{start} --> {end}\n{text}\n\n"));
            }
        }
        _ => out = text,
    }
    Ok(out)
}

/// `seconds` as a cue's time, `HH:MM:SS` and milliseconds after `mark`.
fn timestamp(seconds: f64, mark: char) -> String {
    let millis = (seconds.max(0.0) * 1000.0).round() as u64;
    let (h, m, s, ms) = (
        millis / 3_600_000,
        millis / 60_000 % 60,
        millis / 1000 % 60,
        millis % 1000,
    );
    format!("{h:02}:{m:02}:{s:02}{mark}{ms:03}")
}

/// Writes a stream of events: each delta at once, and the end, with the
/// usage reported, when the stream ends well.
#[derive(Debug, Default)]
struct StreamEncoder {
    folded: Folded,
}

impl StreamEncoder {
    fn encode_into(&mut self, event: &Output, out: &mut BytesMut) -> Result<(), Fault> {
        match event {
            Output::Delta(delta) => {
                let mut v = Map::new();
                v.insert("type".into(), "transcript.text.delta".into());
                v.insert("delta".into(), delta.text.clone().into());
                merge_missing(&mut v, &kept(NAME, &delta.native)?);
                out.extend_from_slice(sse_event(&v.into()).as_bytes());
            }
            Output::Native(n) if &*n.format == NAME && as_usage_extras(&n.value).is_none() => {
                out.extend_from_slice(sse_event(&n.value).as_bytes());
            }
            _ => {}
        }
        self.folded.push(event)
    }

    fn end_into(&mut self, end: &Result<(), Fault>, out: &mut BytesMut) -> Result<(), Fault> {
        end.clone()?;
        let mut v = Map::new();
        v.insert("type".into(), "transcript.text.done".into());
        v.insert("text".into(), self.folded.text().into());
        set(&mut v, "usage", self.folded.usage());
        if let Some(t) = &self.folded.transcript {
            merge_missing(&mut v, &kept(NAME, &t.native)?);
        }
        out.extend_from_slice(sse_event(&v.into()).as_bytes());
        Ok(())
    }
}

impl Encoder<Output> for StreamEncoder {
    fn encode(&mut self, event: &Output, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::edge::frame(frames, |out| self.encode_into(event, out))
    }

    fn end(&mut self, end: Result<(), Fault>, frames: &mut Vec<Bytes>) -> Result<(), Fault> {
        crate::edge::frame(frames, |out| self.end_into(&end, out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_transcript_renders_as_subrip_and_webvtt() {
        let mut t = Transcript::new("Hi there. Bye.");
        t.segments = vec![
            Segment::new(0.0, 1.5, " Hi there."),
            Segment::new(1.5, 3661.25, " Bye."),
        ];
        let events = [Output::Transcript(t)];
        assert_eq!(
            render(&events, "srt").ok().as_deref(),
            Some(
                "1\n00:00:00,000 --> 00:00:01,500\nHi there.\n\n2\n00:00:01,500 --> 01:01:01,250\nBye.\n\n"
            )
        );
        assert_eq!(
            render(&events, "vtt").ok().as_deref(),
            Some(
                "WEBVTT\n\n00:00:00.000 --> 00:00:01.500\nHi there.\n\n00:00:01.500 --> 01:01:01.250\nBye.\n\n"
            )
        );
        assert_eq!(
            render(&events, "text").ok().as_deref(),
            Some("Hi there. Bye.")
        );
        let rendered = [Output::Rendered("1\nkept\n".into())];
        assert_eq!(render(&rendered, "srt").ok().as_deref(), Some("1\nkept\n"));
    }
}

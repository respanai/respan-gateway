//! Soniox's asynchronous REST API behind one completed transcription call.

use std::sync::Arc;
use std::time::Duration;

use futures::channel::oneshot;
use futures::lock::Mutex;
use futures::{FutureExt, SinkExt, TryStreamExt, sink, stream};
use respan_gateway_conversation::MediaSource;
use respan_gateway_conversation_http::{StreamResponse, read_provider_error};
use respan_gateway_core::{
    Connection, ErrorCode, EventSink, EventStream, Extensions, Fault, Format, Plugin, Provider,
    ProviderContext, Result, async_trait,
};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{self, HeaderMap, Method, StatusCode, header::CONTENT_TYPE};
use respan_gateway_http::raw::Original;
use respan_gateway_http::{Connect, Decoder, Upstream, Whole};
use respan_gateway_http_faults::Status;
use respan_gateway_media::transcription::{Input, Output, Task, Transcript};
use serde_json::{Value, json};

use crate::edge::sends;
use crate::openai::{asked, multipart};

#[cfg(test)]
mod tests;

const NAME: &str = "soniox-transcriptions";
const TIMEOUT: Duration = Duration::from_secs(600);
const POLL_INTERVAL: Duration = Duration::from_secs(1);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

/// Soniox file transcription, translated into the existing transcription kind.
///
/// One call uploads its file, creates a job, waits up to ten minutes, and
/// returns its completed transcript. Only JSON and text responses are served;
/// streaming and sampling parameters are refused before upload. Translation
/// returns English tokens, without the original foreign-language transcript.
/// Every internal request uses the caller-supplied transport and credentials.
/// Owned jobs and files are removed by the provider's end hook, including when
/// the client leaves. Cleanup is bounded: a still-processing job cannot be
/// deleted, and a creation whose answer was lost may have an unknown remote ID.
#[derive(Clone, Copy, Debug, Default)]
pub struct SonioxTranscriptions;

impl SonioxTranscriptions {
    /// The provider format's name.
    pub const NAME: &'static str = NAME;
}

#[derive(Clone, Default)]
struct Owned(Arc<Mutex<Resources>>);

#[derive(Default)]
struct Resources {
    file: Option<String>,
    job: Option<String>,
    terminal: bool,
}

struct Io {
    connect: Connect,
    extensions: Extensions,
    owned: Owned,
}

impl Upstream for SonioxTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn provider(&self, name: String, connect: Connect) -> Provider {
        let cleanup = Cleanup(connect.clone());
        Provider::new(name, *self, move |extensions: Extensions| Io {
            owned: extensions.get::<Owned>().cloned().unwrap_or_default(),
            connect: connect.clone(),
            extensions,
        })
        .plugin(cleanup)
    }
}

impl Format<Io, Output, Input> for SonioxTranscriptions {
    fn name(&self) -> &str {
        NAME
    }

    fn bind(&self, io: Io) -> Result<Connection<Output, Input>> {
        let (send, receive) = oneshot::channel();
        let extensions = io.extensions.clone();
        let outgoing = EventSink::preparing(move |source: &mut EventStream<Input>| {
            let input = source
                .value()
                .transpose()?
                .ok_or_else(|| Fault::bad_request("Soniox requires one uploaded audio file"))?;
            let prepared = prepare(input, &extensions)?;
            source.stop();
            send.send(prepared).map_err(|_| Fault::abandoned())?;
            Ok(sink::drain().sink_map_err(|never| match never {}))
        });
        let incoming = async move {
            let prepared = receive.await.map_err(|_| Fault::abandoned())?;
            let result = tokio::time::timeout(TIMEOUT, transcribe(&io, prepared)).await;
            let outputs = match result {
                Ok(result) => result?,
                Err(_) => return Err(timeout()),
            };
            Ok(EventStream::stream(stream::iter(
                outputs.into_iter().map(Ok),
            )))
        };
        Ok((incoming.boxed(), outgoing))
    }
}

fn timeout() -> Fault {
    Fault::upstream("Soniox transcription timed out").with(Status(StatusCode::GATEWAY_TIMEOUT))
}

struct Prepared {
    upload: Bytes,
    content_type: String,
    request: Value,
    translate: bool,
}

fn prepare(input: &Input, extensions: &Extensions) -> Result<Prepared> {
    let original = asked(extensions.get::<Original>());
    let streaming = extensions
        .get::<StreamResponse>()
        .map(|flag| flag.0)
        .unwrap_or_else(|| {
            matches!(original.get("stream"), Some(Value::Bool(true)))
                || original
                    .get("stream")
                    .and_then(Value::as_str)
                    .is_some_and(|s| s.eq_ignore_ascii_case("true"))
        });
    if streaming || input.temperature.is_some() {
        return Err(Fault::bad_request(
            "Soniox supports non-streamed transcription or translation without temperature",
        ));
    }
    if let Some(native) = &input.native {
        for (key, value) in &native.fields {
            match key.as_str() {
                "response_format" if matches!(value.as_str(), Some("json" | "text")) => {}
                _ => {
                    return Err(Fault::bad_request(format!(
                        "Soniox does not support '{key}' with this value"
                    )));
                }
            }
        }
    }
    let model = input
        .model
        .as_deref()
        .filter(|model| !model.is_empty())
        .ok_or_else(|| Fault::bad_request("Soniox requires a transcription model"))?;
    let MediaSource::Bytes(audio) = &input.audio.source else {
        return Err(Fault::bad_request(
            "Soniox requires audio uploaded as a file",
        ));
    };
    if audio.is_empty() {
        return Err(Fault::bad_request("Soniox requires a nonempty audio file"));
    }
    let mut form = multipart::Writer::with_boundary(multipart::random_boundary());
    form.file(
        "file",
        input.audio.name.as_deref().unwrap_or("audio"),
        &input.audio.mime_type,
        audio,
    );
    let content_type = form.content_type();
    let translate = input.task == Task::Translate;
    let mut request = json!({"model": model});
    if translate {
        request["translation"] = json!({"type": "one_way", "target_language": "en"});
    }
    if let Some(language) = &input.language {
        request["language_hints"] = json!([language]);
    }
    if let Some(prompt) = &input.prompt {
        request["context"] = prompt.clone().into();
    }
    Ok(Prepared {
        upload: form.finish(),
        content_type,
        request,
        translate,
    })
}

async fn transcribe(io: &Io, mut prepared: Prepared) -> Result<Vec<Output>> {
    let uploaded = call(
        io,
        Method::POST,
        "/files".into(),
        prepared.upload,
        Some(prepared.content_type),
    )
    .await?
    .json()?;
    let file = resource_id(&uploaded)?;
    io.owned.0.lock().await.file = Some(file.clone());
    prepared.request["file_id"] = file.into();
    let created = json_call(
        io,
        Method::POST,
        "/transcriptions".into(),
        Some(prepared.request),
    )
    .await?
    .json()?;
    let job = resource_id(&created)?;
    io.owned.0.lock().await.job = Some(job.clone());
    let mut state = created;
    loop {
        match state.get("status").and_then(Value::as_str) {
            Some("completed") => break,
            Some("error") => {
                io.owned.0.lock().await.terminal = true;
                return Err(
                    Fault::upstream("Soniox transcription failed").with(ErrorCode(
                        state
                            .get("error_type")
                            .and_then(Value::as_str)
                            .unwrap_or("soniox_transcription_failed")
                            .to_owned()
                            .into(),
                    )),
                );
            }
            Some("queued" | "processing") => {}
            _ => {
                return Err(Fault::upstream(
                    "Soniox returned an invalid transcription status",
                ));
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
        state = json_call(io, Method::GET, format!("/transcriptions/{job}"), None)
            .await?
            .json()?;
    }
    io.owned.0.lock().await.terminal = true;
    let transcript = json_call(
        io,
        Method::GET,
        format!("/transcriptions/{job}/transcript"),
        None,
    )
    .await?
    .json()?;
    let text = if prepared.translate {
        translated_text(&transcript)?
    } else {
        transcript
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| Fault::upstream("Soniox returned a transcript without text"))?
            .to_owned()
    };
    let millis = state
        .get("audio_duration_ms")
        .and_then(Value::as_u64)
        .ok_or_else(|| Fault::upstream("Soniox returned a transcript without its duration"))?;
    let seconds = millis as f64 / 1000.0;
    let mut transcript = Transcript::new(text);
    transcript.duration = Some(seconds);
    Ok(vec![
        Output::Seconds(seconds),
        Output::Transcript(transcript),
    ])
}

fn translated_text(transcript: &Value) -> Result<String> {
    let invalid = || Fault::upstream("Soniox returned an invalid English translation");
    let tokens = transcript
        .get("tokens")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let mut text = String::new();
    for token in tokens {
        let status = token.get("translation_status").and_then(Value::as_str);
        if status == Some("translation")
            || (status == Some("none")
                && token.get("language").and_then(Value::as_str) == Some("en"))
        {
            text.push_str(
                token
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(invalid)?,
            );
        }
    }
    if !tokens.is_empty() && text.is_empty() {
        return Err(invalid());
    }
    Ok(text)
}

fn resource_id(value: &Value) -> Result<String> {
    value
        .get("id")
        .and_then(Value::as_str)
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .map(|id| id.to_string())
        .ok_or_else(|| Fault::upstream("Soniox returned an invalid resource ID"))
}

#[derive(Clone)]
struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Reply {
    fn json(self) -> Result<Value> {
        if !self.status.is_success() {
            return Err(self.fault());
        }
        serde_json::from_slice(&self.body)
            .map_err(|error| Fault::upstream("Soniox returned invalid JSON").with_source(error))
    }

    fn fault(self) -> Fault {
        let mut response = http::Response::new(self.body);
        *response.status_mut() = self.status;
        *response.headers_mut() = self.headers;
        read_provider_error(response)
    }
}

async fn json_call(io: &Io, method: Method, path: String, value: Option<Value>) -> Result<Reply> {
    let (body, content_type) = match value {
        Some(value) => (
            Bytes::from(value.to_string()),
            Some("application/json".into()),
        ),
        None => (Bytes::new(), None),
    };
    call(io, method, path, body, content_type).await
}

async fn call(
    io: &Io,
    method: Method,
    path: String,
    body: Bytes,
    content_type: Option<String>,
) -> Result<Reply> {
    let provider = (io.connect)(io.extensions.clone()).with_success(|_, _| true);
    let (opening, mut outgoing) = provider.bind(
        move |head, _: Option<&Bytes>| {
            head.method = method;
            head.uri = path
                .parse()
                .map_err(|_| Fault::internal("invalid Soniox API path"))?;
            if let Some(content_type) = content_type {
                head.headers.insert(
                    CONTENT_TYPE,
                    content_type
                        .parse()
                        .map_err(|_| Fault::internal("invalid Soniox content type"))?,
                );
            }
            Ok(sends(body))
        },
        |head, _| {
            let (status, headers) = (head.status, head.headers.clone());
            Some(Box::new(Whole::new(move |body, out: &mut Vec<Reply>| {
                out.push(Reply {
                    status,
                    headers: headers.clone(),
                    body,
                });
                Ok(())
            })) as Box<dyn Decoder<Reply>>)
        },
    );
    let mut source = EventStream::single(Bytes::new());
    outgoing.prepare(&mut source)?;
    let (answer, delivered) = futures::join!(opening, outgoing.close());
    delivered?;
    let mut replies = answer?;
    replies
        .try_next()
        .await?
        .ok_or_else(|| Fault::upstream("Soniox returned no response"))
}

struct Cleanup(Connect);

#[async_trait]
impl Plugin for Cleanup {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<()> {
        px.extensions_mut().insert(Owned::default());
        Ok(())
    }

    async fn on_provider_end(&self, px: &ProviderContext, _: &Result<()>) {
        let Some(owned) = px.extensions().get::<Owned>() else {
            return;
        };
        let io = Io {
            connect: self.0.clone(),
            extensions: px.extensions().clone(),
            owned: owned.clone(),
        };
        if !matches!(
            tokio::time::timeout(CLEANUP_TIMEOUT, cleanup(&io)).await,
            Ok(Ok(()))
        ) {
            let resources = owned.0.lock().await;
            eprintln!(
                "Soniox cleanup incomplete: file={:?}, transcription={:?}",
                resources.file, resources.job
            );
        }
    }
}

async fn cleanup(io: &Io) -> Result<()> {
    let (file, job, terminal) = {
        let resources = io.owned.0.lock().await;
        (
            resources.file.clone(),
            resources.job.clone(),
            resources.terminal,
        )
    };
    if let Some(job) = job {
        let path = format!("/transcriptions/{job}");
        let mut terminal = terminal;
        loop {
            if !terminal {
                let state = json_call(io, Method::GET, path.clone(), None).await?;
                if state.status == StatusCode::NOT_FOUND {
                    break;
                }
                let state = state.json()?;
                terminal = matches!(
                    state.get("status").and_then(Value::as_str),
                    Some("completed" | "error")
                );
            }
            if terminal {
                let deleted = json_call(io, Method::DELETE, path.clone(), None).await?;
                if deleted.status.is_success() || deleted.status == StatusCode::NOT_FOUND {
                    break;
                }
                if deleted.status != StatusCode::CONFLICT {
                    return Err(deleted.fault());
                }
                terminal = false;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        io.owned.0.lock().await.job = None;
    }
    if let Some(file) = file {
        let deleted = json_call(io, Method::DELETE, format!("/files/{file}"), None).await?;
        if !deleted.status.is_success() && deleted.status != StatusCode::NOT_FOUND {
            return Err(deleted.fault());
        }
        io.owned.0.lock().await.file = None;
    }
    Ok(())
}

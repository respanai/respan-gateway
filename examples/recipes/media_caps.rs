//! Caps and a meter over the media kinds: images, audio and embeddings.
//!
//! Each kind is a neutral `Input` and `Output` of its own, so one plugin,
//! registered like any other, polices them without a word of OpenAI's wire:
//!
//! - an image request is refused above a count and an edge length;
//! - a transcription upload is refused above a size, and a client key past
//!   its budget of audio seconds is refused before the provider is called;
//! - the tokens of every embedding and the seconds of every transcription
//!   the provider reports are metered per client key, from the kinds'
//!   `Usage` and `Seconds` events.
//!
//! The same plugin serves a Gemini or a self-hosted provider of the kinds
//! as is: it never reads the bytes of a protocol. A real meter would live in
//! a store (see the `billing` example); here it is a map.
//!
//! Run it with `cargo run -p respan-gateway-examples --example media_caps`.
//! Look for the embedding's tokens and the transcription's seconds in the
//! meter, then the refusals: five images, a 2 MiB upload, and a second
//! transcription once the key's audio budget is spent.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

use mockllm::{Answer, FakeOpenAi};
use respan_gateway_conversation::MediaSource;
use respan_gateway_core::{
    ClientContext, ErrorCode, Fault, Gateway, Plugin, Provider, Response, async_trait,
};
use respan_gateway_http::http::header::{AUTHORIZATION, CONTENT_TYPE};
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, Request, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::{Body, Credential};
use respan_gateway_http_faults::Status;
use respan_gateway_http_transport::HttpTransport;
use respan_gateway_media::{embedding, image, transcription};
use respan_gateway_media_http::openai::{OpenAiEmbeddings, OpenAiImages, OpenAiTranscriptions};
use serde_json::{Value, json};

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let fake = FakeOpenAi::start().await;
    let meter = Meter::default();
    let caps = MediaCaps {
        max_images: 2,
        max_edge: 1024,
        max_audio_bytes: 1 << 20,
        audio_budget: 2.0,
        meter: meter.clone(),
    };
    let gateway = gateway(&fake.base_url(), caps)?;

    fake.answer(Answer::embedding(vec![0.1, 0.2]));
    let embed = json!({ "model": "text-embedding-3-small", "input": "hello there" });
    let (status, _) = send(&gateway, json_request("/v1/embeddings", &embed)).await;
    println!("embedding: {status}; used {:?}", meter.used("gw-acme"));

    fake.answer(Answer::transcript("hello from the gateway"));
    let (status, _) = send(&gateway, upload(4096)).await;
    println!("transcription: {status}; used {:?}", meter.used("gw-acme"));

    let images = json!({ "model": "gpt-image-1", "prompt": "a fox", "n": 5 });
    let request = json_request("/v1/images/generations", &images);
    let (status, body) = send(&gateway, request).await;
    println!("five images: {status}\n  {body}");

    let (status, body) = send(&gateway, upload(2 << 20)).await;
    println!("a 2 MiB upload: {status}\n  {body}");

    let (status, body) = send(&gateway, upload(4096)).await;
    println!("another transcription: {status}\n  {body}");
    println!("provider calls made: {}", fake.requests().len());
    Ok(())
}

// The meter.

/// What a client key has used so far.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Used {
    embedding_tokens: u64,
    audio_seconds: f64,
}

/// Usage by client key. Clones share the map.
#[derive(Clone, Default)]
struct Meter(Arc<Mutex<HashMap<String, Used>>>);

impl Meter {
    fn used(&self, key: &str) -> Used {
        self.0.lock().get(key).copied().unwrap_or_default()
    }

    fn add(&self, key: &str, add: impl FnOnce(&mut Used)) {
        add(self.0.lock().entry(key.to_owned()).or_default());
    }
}

// The caps.

/// A request over a cap is refused before any provider is called.
struct MediaCaps {
    /// Images in one request.
    max_images: u32,
    /// The longest edge of an image, in pixels.
    max_edge: u32,
    /// One transcription upload, in bytes.
    max_audio_bytes: usize,
    /// Seconds of audio a client key may have transcribed in all.
    audio_budget: f64,
    meter: Meter,
}

#[async_trait]
impl Plugin for MediaCaps {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let credential = cx.extensions().get::<Credential>();
        let key = credential
            .and_then(Credential::bearer)
            .unwrap_or("anonymous");
        let key = key.to_owned();
        let meter = self.meter.clone();
        if cx.is_kind::<image::Input>() {
            self.check_image(cx.input::<image::Input>()?)?;
        } else if cx.is_kind::<transcription::Input>() {
            self.check_audio(&key, cx.input::<transcription::Input>()?)?;
            cx.filter(move |out: &mut transcription::Output| {
                if let transcription::Output::Seconds(seconds) = out {
                    meter.add(&key, |used| used.audio_seconds += *seconds);
                }
                Ok(())
            })?;
        } else if cx.is_kind::<embedding::Input>() {
            cx.filter(move |out: &mut embedding::Output| {
                if let embedding::Output::Usage(usage) = out {
                    meter.add(&key, |used| used.embedding_tokens += usage.input_tokens);
                }
                Ok(())
            })?;
        }
        Ok(None)
    }
}

impl MediaCaps {
    fn check_image(&self, i: &image::Input) -> Result<(), Fault> {
        let count = i.count.unwrap_or(1);
        if count > self.max_images {
            let message = format!(
                "at most {} images per request, not {count}",
                self.max_images
            );
            return Err(refuse(StatusCode::BAD_REQUEST, "too_many_images", message));
        }
        let edge = match i.size.as_deref() {
            None | Some("auto") => 0,
            Some(size) => {
                let sides = size.split_once('x');
                let sides = sides.and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
                let (w, h): (u32, u32) = sides.ok_or_else(|| {
                    let message = format!("unknown image size {size}");
                    refuse(StatusCode::BAD_REQUEST, "bad_image_size", message)
                })?;
                w.max(h)
            }
        };
        if edge > self.max_edge {
            let message = format!("images are at most {0}x{0} pixels", self.max_edge);
            return Err(refuse(StatusCode::BAD_REQUEST, "image_too_large", message));
        }
        Ok(())
    }

    fn check_audio(&self, key: &str, i: &transcription::Input) -> Result<(), Fault> {
        let bytes = match &i.audio.source {
            MediaSource::Bytes(bytes) => bytes.len(),
            MediaSource::Url(_) => 0,
        };
        if bytes > self.max_audio_bytes {
            let message = format!("uploads are at most {} bytes", self.max_audio_bytes);
            return Err(refuse(
                StatusCode::PAYLOAD_TOO_LARGE,
                "audio_too_large",
                message,
            ));
        }
        if self.meter.used(key).audio_seconds >= self.audio_budget {
            let message = "this key's audio budget is spent".to_owned();
            return Err(refuse(
                StatusCode::TOO_MANY_REQUESTS,
                "audio_budget_spent",
                message,
            ));
        }
        Ok(())
    }
}

fn refuse(status: StatusCode, code: &'static str, message: String) -> Fault {
    Fault::new(message)
        .with(ErrorCode(code.into()))
        .with(Status(status))
}

// The gateway.

/// Sends each kind to OpenAI's provider of it; with a router, this is its
/// job.
struct OpenAiByKind;

#[async_trait]
impl Plugin for OpenAiByKind {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let target = match () {
            () if cx.is_kind::<image::Input>() => "openai:images",
            () if cx.is_kind::<transcription::Input>() => "openai:transcriptions",
            () => "openai:embeddings",
        };
        cx.set_target(target);
        Ok(None)
    }
}

fn gateway(base: &str, caps: MediaCaps) -> Result<Gateway, Fault> {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer sk-test"));
    let transport = HttpTransport::new(base)?.with_headers(headers)?.connect();
    Gateway::new()
        .provider(Provider::new(
            "openai:images",
            OpenAiImages,
            transport.clone(),
        ))?
        .provider(Provider::new(
            "openai:transcriptions",
            OpenAiTranscriptions,
            transport.clone(),
        ))?
        .provider(Provider::new(
            "openai:embeddings",
            OpenAiEmbeddings,
            transport,
        ))?
        .client::<image::Input, _, _>(OpenAiImages)?
        .client::<transcription::Input, _, _>(OpenAiTranscriptions)?
        .client::<embedding::Input, _, _>(OpenAiEmbeddings)?
        .plugin(caps)
        .plugin(OpenAiByKind)
        .target("openai:embeddings")
}

// A client, as the key `gw-acme`.

fn post(path: &'static str, body: impl Into<Body>, content_type: &'static str) -> Request<Body> {
    let mut req = Request::new(body.into());
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static(path);
    let headers = req.headers_mut();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer gw-acme"));
    req
}

fn json_request(path: &'static str, body: &Value) -> Request<Body> {
    post(path, Body::full(body.to_string()), "application/json")
}

/// A transcription upload: a form with `bytes` of audio.
fn upload(bytes: usize) -> Request<Body> {
    let head = "--b\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n\
        --b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
        Content-Type: audio/wav\r\n\r\n";
    let mut body = head.as_bytes().to_vec();
    body.extend(std::iter::repeat_n(b'a', bytes));
    body.extend_from_slice(b"\r\n--b--\r\n");
    post(
        "/v1/audio/transcriptions",
        Body::full(body),
        "multipart/form-data; boundary=b",
    )
}

/// The status and body of the response to `req`.
async fn send(gateway: &Gateway, req: Request<Body>) -> (StatusCode, String) {
    let (resp, _) = respan_gateway_http::handle(gateway, req, 32 << 20).await;
    let status = resp.status();
    let body = match resp.into_body().collect().await {
        Ok(body) => String::from_utf8_lossy(&body.to_bytes()).into_owned(),
        Err(fault) => fault.to_string(),
    };
    (status, body)
}

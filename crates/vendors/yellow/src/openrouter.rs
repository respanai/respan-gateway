//! OpenRouter: Chat Completions and media, for many vendors' models.

use base64::Engine as _;
use respan_gateway_conversation::{Media, MediaSource, Usage};
use respan_gateway_conversation_http::fields::{kept, native_of};
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{request::Parts as RequestHead, response::Parts as ResponseHead};
use respan_gateway_http::path::set_path;
use respan_gateway_media::{image, speech, video};
use respan_gateway_media_http::openai::{
    OpenAiEmbeddings, OpenAiImages, OpenAiSpeech, OpenAiTranscriptions, OpenAiVideos,
};
use respan_gateway_vendor_kit::{Dialect, Dialected, Rewrite, Vendor};
use serde_json::{Map, Value, json};

const BASE_URL: &str = "https://openrouter.ai/api/v1";

/// OpenRouter. Its models' names are `<vendor>/<model>`: a configured
/// provider named `<vendor>` takes the name first.
pub const VENDOR: Vendor = Vendor {
    id: "openrouter",
    key_env: &["OPENROUTER_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (
            &Dialected::new(OpenAiImages, OpenRouterImages),
            Some(BASE_URL),
        ),
        (&OpenAiEmbeddings, Some(BASE_URL)),
        (
            &Dialected::new(OpenAiSpeech, OpenRouterSpeech),
            Some(BASE_URL),
        ),
        (&OpenAiTranscriptions, Some(BASE_URL)),
        (
            &Dialected::new(OpenAiVideos, OpenRouterVideos),
            Some(BASE_URL),
        ),
    ],
    ..Vendor::DEFAULT
};

/// Images at `/images`, with reference images, no mask, and OpenRouter's
/// token counts, timestamps and media types. Streams stay incremental.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterImages;

impl Dialect for OpenRouterImages {
    type Input = image::Input;
    type Output = image::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<image::Input>>> {
        Some(Box::new(|input: &mut image::Input| {
            if input.mask.is_some() {
                return Err(Fault::bad_request("OpenRouter images take no mask"));
            }
            let mut fields = kept(OpenAiImages::NAME, &input.native)?;
            if !input.images.is_empty() {
                fields.insert(
                    "input_references".into(),
                    input.images.iter().map(reference).collect(),
                );
                input.images.clear();
            }
            input.native = native_of(OpenAiImages::NAME, fields);
            Ok(())
        }))
    }

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        set_path(head, "/images")?;
        Ok(None)
    }

    fn outputs(&self) -> Option<Box<dyn Filter<image::Output>>> {
        Some(Box::new(ImageOutput::default()))
    }
}

fn reference(media: &Media) -> Value {
    let url = match &media.source {
        MediaSource::Url(url) => url.clone(),
        MediaSource::Bytes(bytes) => format!(
            "data:{};base64,{}",
            media.mime_type,
            base64::engine::general_purpose::STANDARD.encode(bytes)
        ),
    };
    json!({"type": "image_url", "image_url": {"url": url}})
}

#[derive(Default)]
struct ImageOutput {
    shared: Map<String, Value>,
    usage: Option<Usage>,
}

impl ImageOutput {
    fn flush(&mut self, out: &mut Vec<image::Output>) {
        if let Some(usage) = self.usage.take() {
            out.push(image::Output::Usage(usage));
        }
    }
}

impl Filter<image::Output> for ImageOutput {
    fn edit(
        &mut self,
        mut value: image::Output,
        out: &mut Vec<image::Output>,
    ) -> Result<(), Fault> {
        if let image::Output::Usage(usage) = value {
            self.flush(out);
            self.usage = Some(usage);
            return Ok(());
        }
        if let image::Output::Native(native) = &value
            && let Some(usage) = self.usage.as_mut()
            && let Some(fields) = native.value["usage"].as_object()
        {
            for (key, count) in [
                ("prompt_tokens", &mut usage.input_tokens),
                ("completion_tokens", &mut usage.output_tokens),
            ] {
                if let Some(value) = fields.get(key) {
                    *count = value.as_u64().ok_or_else(|| {
                        Fault::upstream("OpenRouter image usage is not a whole number")
                    })?;
                }
            }
        }
        self.flush(out);
        match &mut value {
            image::Output::Start(start) => {
                self.shared = kept(OpenAiImages::NAME, &start.native)?;
                start.created = start
                    .created
                    .or_else(|| self.shared.get("created").and_then(Value::as_u64));
            }
            image::Output::Image(image) => {
                let fields = kept(OpenAiImages::NAME, &image.native)?;
                if let Some(mime) = fields
                    .get("media_type")
                    .or_else(|| self.shared.get("media_type"))
                    .and_then(Value::as_str)
                {
                    image.media.mime_type = mime.to_owned();
                }
            }
            image::Output::Partial(image) => {
                let fields = kept(OpenAiImages::NAME, &image.native)?;
                if let Some(mime) = fields
                    .get("media_type")
                    .or_else(|| self.shared.get("media_type"))
                    .and_then(Value::as_str)
                {
                    image.media.mime_type = mime.to_owned();
                }
            }
            _ => {}
        }
        out.push(value);
        Ok(())
    }

    fn end(&mut self, _: &Result<(), Fault>, out: &mut Vec<image::Output>) -> Result<(), Fault> {
        self.flush(out);
        Ok(())
    }
}

/// Speech is raw audio. Ask explicitly for MP3 when omitted, matching
/// OpenAI's default, and let the client format frame events if requested.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterSpeech;

impl Dialect for OpenRouterSpeech {
    type Input = speech::Input;
    type Output = speech::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<speech::Input>>> {
        Some(Box::new(|input: &mut speech::Input| {
            input.audio_format.get_or_insert_with(|| "mp3".into());
            Ok(())
        }))
    }

    fn request(&self, _: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        Ok(Some(Box::new(|body| crate::without(body, "stream_format"))))
    }
}

/// Video creation, retrieval and content; OpenRouter's integer `duration`
/// is the kind's `seconds`, and a reference is its first frame.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterVideos;

impl Dialect for OpenRouterVideos {
    type Input = video::Input;
    type Output = video::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<video::Input>>> {
        Some(Box::new(|input: &mut video::Input| match input {
            video::Input::Create(create) => {
                let mut fields = kept(OpenAiVideos::NAME, &create.native)?;
                if let Some(seconds) = create.seconds {
                    if fields
                        .get("duration")
                        .is_some_and(|v| v.as_u64() != Some(u64::from(seconds)))
                    {
                        return Err(Fault::bad_request(
                            "OpenRouter duration conflicts with seconds",
                        ));
                    }
                    fields.insert("duration".into(), seconds.into());
                    create.seconds = None;
                }
                if let Some(reference_image) = create.reference.take() {
                    let mut frame = reference(&reference_image);
                    frame["frame_type"] = "first_frame".into();
                    fields.insert("frame_images".into(), json!([frame]));
                }
                create.native = native_of(OpenAiVideos::NAME, fields);
                Ok(())
            }
            video::Input::Retrieve { .. }
            | video::Input::Content {
                variant: video::Variant::Video,
                ..
            } => Ok(()),
            _ => Err(Fault::bad_request(
                "OpenRouter videos support create, retrieve and video content only",
            )),
        }))
    }

    fn response(&self, sent: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        if !head.status.is_success() || sent.uri.path().ends_with("/content") {
            return None;
        }
        Some(Box::new(|raw: Bytes| {
            let mut fields: Map<String, Value> = serde_json::from_slice(&raw).map_err(|e| {
                Fault::upstream("OpenRouter returned an invalid video object").with_source(e)
            })?;
            if let Some(duration) = fields.get("duration").cloned() {
                fields.insert("seconds".into(), duration);
            }
            match fields.get("status").and_then(Value::as_str) {
                Some("pending") => {
                    fields.insert("status".into(), "queued".into());
                }
                Some("cancelled" | "expired") => {
                    if fields.get("error").is_none_or(Value::is_null) {
                        fields.insert("error".into(), fields["status"].clone());
                    }
                    fields.insert("status".into(), "failed".into());
                }
                _ => {}
            }
            if let Some(message) = fields.get("error").and_then(Value::as_str) {
                fields.insert("error".into(), json!({"message": message}));
            }
            serde_json::to_vec(&fields)
                .map(Bytes::from)
                .map_err(|e| Fault::internal("could not write video object").with_source(e))
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registers_chat_and_all_five_media_kinds() {
        for name in [
            "openai-chat",
            "openai-images",
            "openai-embeddings",
            "openai-speech",
            "openai-transcriptions",
            "openai-videos",
        ] {
            assert!(VENDOR.format(name).is_some(), "missing {name}");
        }
    }

    use respan_gateway_core::{Gateway, futures};
    use respan_gateway_http::{Body, http, http_body_util::BodyExt};
    use std::sync::{Arc, Mutex};

    type Calls = Arc<Mutex<Vec<(http::request::Parts, Bytes)>>>;

    fn gateway(format: &str, mime: &'static str, reply: impl Into<Bytes>) -> (Gateway, Calls) {
        let calls: Calls = Arc::default();
        let seen = calls.clone();
        let reply = reply.into();
        let connect = respan_gateway_http::connect(
            move |call: http::Request<Body>| {
                let seen = seen.clone();
                let reply = reply.clone();
                async move {
                    let (head, body) = call.into_parts();
                    let bytes = body.collect().await?.to_bytes();
                    seen.lock().expect("capture").push((head, bytes));
                    let chunks = reply
                        .chunks(7)
                        .map(Bytes::copy_from_slice)
                        .map(Ok)
                        .collect::<Vec<_>>();
                    let body = Body::stream(futures::stream::iter(chunks));
                    Ok((
                        http::Response::builder()
                            .header("content-type", mime)
                            .body(body)
                            .expect("answer"),
                        None,
                    ))
                }
            },
            1 << 20,
        );
        let (format, _) = VENDOR.format(format).expect("registered format");
        let gateway = Gateway::new()
            .provider(format.provider("openrouter".into(), Arc::new(connect))).expect("provider")
            .client::<image::Input, image::Output, _>(OpenAiImages).expect("images")
            .client::<respan_gateway_media::embedding::Input, respan_gateway_media::embedding::Output, _>(OpenAiEmbeddings).expect("embeddings")
            .client::<respan_gateway_media::speech::Input, respan_gateway_media::speech::Output, _>(OpenAiSpeech).expect("speech")
            .client::<respan_gateway_media::transcription::Input, respan_gateway_media::transcription::Output, _>(OpenAiTranscriptions).expect("transcriptions")
            .client::<video::Input, video::Output, _>(OpenAiVideos).expect("videos")
            .target("openrouter").expect("target");
        (gateway, calls)
    }

    async fn send(
        gateway: &Gateway,
        method: http::Method,
        path: &str,
        mime: &str,
        body: impl Into<Bytes>,
    ) -> (http::StatusCode, Bytes, Result<(), Fault>) {
        let request = http::Request::builder()
            .method(method)
            .uri(path)
            .header("content-type", mime)
            .body(Body::full(body))
            .expect("request");
        let (response, done) = respan_gateway_http::handle(gateway, request, 1 << 20).await;
        let status = response.status();
        let (body, done) = futures::join!(response.into_body().collect(), done);
        let bytes = match body {
            Ok(body) => body.to_bytes(),
            Err(_) => {
                assert!(done.is_err(), "body failure must fail completion");
                Bytes::new()
            }
        };
        (status, bytes, done)
    }

    async fn post(
        gateway: &Gateway,
        path: &str,
        body: Value,
    ) -> (http::StatusCode, Bytes, Result<(), Fault>) {
        send(
            gateway,
            http::Method::POST,
            path,
            "application/json",
            body.to_string(),
        )
        .await
    }

    #[tokio::test]
    async fn images_generate_and_edit_at_images_and_keep_usage() {
        let reply = json!({"created":42,"data":[{"b64_json":"aW1hZ2U=","media_type":"image/svg+xml"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"cost":0.04}});
        let (gateway, calls) = gateway(OpenAiImages::NAME, "application/json", reply.to_string());
        for (path, request) in [
            (
                "/v1/images/generations",
                json!({"model":"test/image","prompt":"draw"}),
            ),
            (
                "/v1/images/edits",
                json!({"model":"test/image","prompt":"edit","images":[{"image_url":"data:image/jpeg;base64,aW1hZ2U="},{"image_url":"https://example.test/ref.png"}]}),
            ),
        ] {
            let (status, bytes, done) = post(&gateway, path, request).await;
            done.expect("completed");
            assert_eq!(status, http::StatusCode::OK);
            let reply: Value = serde_json::from_slice(&bytes).expect("JSON reply");
            assert_eq!(reply["usage"]["input_tokens"], 2);
            assert_eq!(reply["usage"]["output_tokens"], 3);
            assert_eq!(reply["usage"]["cost"], 0.04);
        }
        let calls = calls.lock().expect("calls");
        assert!(calls.iter().all(|(head, _)| head.uri.path() == "/images"));
        let edit: Value = serde_json::from_slice(&calls[1].1).expect("edit");
        assert_eq!(
            edit["input_references"][0]["image_url"]["url"],
            "data:image/jpeg;base64,aW1hZ2U="
        );
        assert_eq!(
            edit["input_references"][1]["image_url"]["url"],
            "https://example.test/ref.png"
        );
        assert!(edit.get("images").is_none());
    }

    #[tokio::test]
    async fn image_stream_keeps_partials_mime_timestamp_usage_and_truncation() {
        let partial = json!({"type":"image_generation.partial_image","partial_image_index":0,"b64_json":"eA=="});
        let completed = json!({"type":"image_generation.completed","created":42,"b64_json":"aW1hZ2U=","media_type":"image/svg+xml","usage":{"prompt_tokens":2,"completion_tokens":3,"cost":0.04}});
        let stream = format!(
            "{}{}data: [DONE]\n\n",
            respan_gateway_sse::event(None, &partial),
            respan_gateway_sse::event(None, &completed)
        );
        let (gateway, calls) = gateway(OpenAiImages::NAME, "text/event-stream", stream);
        let (status, bytes, done) = post(
            &gateway,
            "/v1/images/generations",
            json!({"prompt":"draw","stream":true}),
        )
        .await;
        assert_eq!(status, http::StatusCode::OK);
        done.expect("stream completed");
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("partial_image"));
        assert!(text.contains("image/svg+xml"));
        assert!(text.contains("0.04"));
        assert!(text.contains("\"input_tokens\":2"));
        let request: Value =
            serde_json::from_slice(&calls.lock().expect("calls")[0].1).expect("call");
        assert_eq!(request["stream"], true);

        let (gateway, _) = self::gateway(
            OpenAiImages::NAME,
            "text/event-stream",
            respan_gateway_sse::event(None, &partial),
        );
        let (_, _, done) = post(
            &gateway,
            "/v1/images/generations",
            json!({"prompt":"draw","stream":true}),
        )
        .await;
        assert!(done.is_err(), "a partial image is no completed stream");
    }

    #[test]
    fn image_output_reads_media_type_and_created_from_native_fields() {
        let mut output = ImageOutput::default();
        let mut out = Vec::new();
        let mut start = image::Start::new(None);
        start.native = native_of(
            OpenAiImages::NAME,
            serde_json::from_value(json!({"created":42,"media_type":"image/svg+xml"}))
                .expect("fields"),
        );
        output
            .edit(image::Output::Start(start), &mut out)
            .expect("start");
        output
            .edit(
                image::Output::Image(image::Image::new(Media::new(
                    "image/png",
                    MediaSource::Bytes(Bytes::new()),
                ))),
                &mut out,
            )
            .expect("image");
        assert!(matches!(&out[0], image::Output::Start(start) if start.created == Some(42)));
        assert!(
            matches!(&out[1], image::Output::Image(image) if image.media.mime_type == "image/svg+xml")
        );
    }

    #[tokio::test]
    async fn embeddings_use_the_spec_endpoint_and_preserve_cost() {
        let (gateway, calls) = gateway(OpenAiEmbeddings::NAME, "application/json", json!({"object":"list","model":"test/embed","data":[{"index":0,"embedding":[0.1,0.2]}],"usage":{"prompt_tokens":2,"total_tokens":2,"cost":0.01}}).to_string());
        let (status, bytes, done) = post(
            &gateway,
            "/v1/embeddings",
            json!({"model":"test/embed","input":"hello"}),
        )
        .await;
        done.expect("completed");
        assert_eq!(status, http::StatusCode::OK);
        let reply: Value = serde_json::from_slice(&bytes).expect("reply");
        assert_eq!(reply["data"][0]["embedding"], json!([0.1, 0.2]));
        assert_eq!(reply["usage"]["cost"], 0.01);
        assert_eq!(calls.lock().expect("calls")[0].0.uri.path(), "/embeddings");
    }

    #[tokio::test]
    async fn speech_relays_raw_audio_and_explicit_output_format() {
        let (gateway, calls) = gateway(OpenAiSpeech::NAME, "audio/mpeg", "audio bytes");
        let (status, bytes, done) = post(
            &gateway,
            "/v1/audio/speech",
            json!({"model":"test/speech","input":"hello","voice":"alloy","response_format":"mp3"}),
        )
        .await;
        done.expect("completed");
        assert_eq!(status, http::StatusCode::OK);
        assert_eq!(bytes, "audio bytes");
        let calls = calls.lock().expect("calls");
        assert_eq!(calls[0].0.uri.path(), "/audio/speech");
        let body: Value = serde_json::from_slice(&calls[0].1).expect("call");
        assert_eq!(body["response_format"], "mp3");
    }

    #[tokio::test]
    async fn speech_defaults_to_mp3_and_client_frames_requested_events() {
        let (gateway, calls) = gateway(OpenAiSpeech::NAME, "audio/mpeg", "audio bytes");
        let (_, bytes, done) = post(
            &gateway,
            "/v1/audio/speech",
            json!({"model":"test/speech","input":"hello","stream_format":"sse"}),
        )
        .await;
        done.expect("completed");
        assert!(String::from_utf8_lossy(&bytes).contains("speech.audio.done"));
        let calls = calls.lock().expect("calls");
        let body: Value = serde_json::from_slice(&calls[0].1).expect("call");
        assert_eq!(body["response_format"], "mp3");
        assert!(body.get("stream_format").is_none());
    }

    #[tokio::test]
    async fn speech_formats_keep_the_requested_mime_and_client_stream_framing() {
        for (format, expected, mime) in [
            (None, "mp3", "audio/mpeg"),
            (Some(Value::Null), "mp3", "audio/mpeg"),
            (Some(json!("pcm")), "pcm", "audio/pcm"),
            (Some(json!("wav")), "wav", "audio/wav"),
        ] {
            for streamed in [false, true] {
                let (gateway, calls) = gateway(OpenAiSpeech::NAME, mime, "audio bytes");
                let mut body = json!({"input":"hello"});
                if let Some(format) = &format {
                    body["response_format"] = format.clone();
                }
                if streamed {
                    body["stream_format"] = "sse".into();
                }
                let request = http::Request::post("/v1/audio/speech")
                    .header("content-type", "application/json")
                    .body(Body::full(body.to_string()))
                    .expect("request");
                let (response, done) =
                    respan_gateway_http::handle(&gateway, request, 1 << 20).await;
                assert_eq!(response.status(), http::StatusCode::OK);
                assert_eq!(
                    response.headers()["content-type"],
                    if streamed { "text/event-stream" } else { mime }
                );
                let (bytes, done) = futures::join!(response.into_body().collect(), done);
                done.expect("completed");
                let bytes = bytes.expect("audio").to_bytes();
                if streamed {
                    assert!(String::from_utf8_lossy(&bytes).contains("speech.audio.done"));
                } else {
                    assert_eq!(bytes, "audio bytes");
                }
                let calls = calls.lock().expect("calls");
                let body: Value = serde_json::from_slice(&calls[0].1).expect("call");
                assert_eq!(body["response_format"], expected);
                assert!(body.get("stream_format").is_none());
            }
        }
    }

    #[tokio::test]
    async fn transcription_uploads_openai_multipart_supported_by_openrouter() {
        let (gateway, calls) = gateway(
            OpenAiTranscriptions::NAME,
            "application/json",
            json!({"text":"hello","usage":{"type":"duration","seconds":1.5,"cost":0.02}})
                .to_string(),
        );
        let form = "--boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ntest/transcribe\r\n--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"audio.wav\"\r\nContent-Type: audio/wav\r\n\r\naudio bytes\r\n--boundary--\r\n";
        let (status, bytes, done) = send(
            &gateway,
            http::Method::POST,
            "/v1/audio/transcriptions",
            "multipart/form-data; boundary=boundary",
            form,
        )
        .await;
        done.expect("completed");
        assert_eq!(status, http::StatusCode::OK);
        let reply: Value = serde_json::from_slice(&bytes).expect("reply");
        assert_eq!(reply["text"], "hello");
        assert_eq!(reply["usage"]["cost"], 0.02);
        let calls = calls.lock().expect("calls");
        assert_eq!(calls[0].0.uri.path(), "/audio/transcriptions");
        assert!(
            calls[0].0.headers["content-type"]
                .to_str()
                .expect("mime")
                .starts_with("multipart/form-data")
        );
        assert!(String::from_utf8_lossy(&calls[0].1).contains("audio bytes"));
    }

    #[tokio::test]
    async fn video_create_uses_integer_duration_and_first_frame() {
        let (gateway, calls) = gateway(
            OpenAiVideos::NAME,
            "application/json",
            json!({"id":"job/1","status":"pending","duration":4}).to_string(),
        );
        let form = "--boundary\r\nContent-Disposition: form-data; name=\"seconds\"\r\n\r\n4\r\n--boundary\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\na wave\r\n--boundary\r\nContent-Disposition: form-data; name=\"input_reference\"; filename=\"ref.png\"\r\nContent-Type: image/png\r\n\r\nimage\r\n--boundary--\r\n";
        let (status, bytes, done) = send(
            &gateway,
            http::Method::POST,
            "/v1/videos",
            "multipart/form-data; boundary=boundary",
            form,
        )
        .await;
        done.expect("completed");
        assert_eq!(status, http::StatusCode::OK);
        let reply: Value = serde_json::from_slice(&bytes).expect("reply");
        assert_eq!(reply["seconds"], "4");
        assert_eq!(reply["status"], "queued");
        let calls = calls.lock().expect("calls");
        assert_eq!(calls[0].0.uri.path(), "/videos");
        let body: Value = serde_json::from_slice(&calls[0].1).expect("call");
        assert_eq!(body["duration"], 4);
        assert!(body.get("seconds").is_none());
        assert_eq!(
            body["frame_images"][0]["image_url"]["url"],
            "data:image/png;base64,aW1hZ2U="
        );
        assert_eq!(body["frame_images"][0]["frame_type"], "first_frame");
    }

    #[tokio::test]
    async fn video_retrieval_normalizes_terminal_states_and_keeps_reported_cost() {
        for (state, expected) in [
            ("pending", "queued"),
            ("in_progress", "in_progress"),
            ("completed", "completed"),
            ("failed", "failed"),
            ("cancelled", "failed"),
            ("expired", "failed"),
        ] {
            let (gateway, calls) = gateway(OpenAiVideos::NAME, "application/json", json!({"id":"job/1","status":state,"duration":4,"error":"provider explanation","usage":{"cost":0.42}}).to_string());
            let (status, bytes, done) = send(
                &gateway,
                http::Method::GET,
                "/v1/videos/job%2F1",
                "application/json",
                "",
            )
            .await;
            done.expect("completed");
            assert_eq!(status, http::StatusCode::OK);
            let reply: Value = serde_json::from_slice(&bytes).expect("reply");
            assert_eq!(reply["status"], expected);
            assert_eq!(reply["seconds"], "4");
            assert_eq!(reply["error"]["message"], "provider explanation");
            assert_eq!(reply["usage"]["cost"], 0.42);
            assert_eq!(
                calls.lock().expect("calls")[0].0.uri.path(),
                "/videos/job%2F1"
            );
        }
    }

    #[tokio::test]
    async fn video_content_is_raw_and_unsupported_operations_are_unsent() {
        let (gateway, calls) = gateway(OpenAiVideos::NAME, "video/mp4", "video bytes");
        let (_, bytes, done) = send(
            &gateway,
            http::Method::GET,
            "/v1/videos/job/content",
            "application/json",
            "",
        )
        .await;
        done.expect("completed");
        assert_eq!(bytes, "video bytes");
        assert_eq!(
            calls.lock().expect("calls")[0].0.uri.path(),
            "/videos/job/content"
        );
        for (method, path, body) in [
            (http::Method::GET, "/v1/videos", ""),
            (http::Method::DELETE, "/v1/videos/job", ""),
            (
                http::Method::POST,
                "/v1/videos/job/remix",
                r#"{"prompt":"change"}"#,
            ),
            (
                http::Method::GET,
                "/v1/videos/job/content?variant=thumbnail",
                "",
            ),
            (
                http::Method::GET,
                "/v1/videos/job/content?variant=spritesheet",
                "",
            ),
            (
                http::Method::POST,
                "/v1/videos",
                r#"{"prompt":"draw","seconds":"4","duration":8}"#,
            ),
        ] {
            let (status, _, _) = send(&gateway, method, path, "application/json", body).await;
            assert_eq!(status, http::StatusCode::BAD_REQUEST, "{path}");
        }
        assert_eq!(calls.lock().expect("calls").len(), 1);
    }

    #[tokio::test]
    async fn opaque_video_job_followups_reach_the_creating_provider() {
        video_owner_after_dispatch(false).await;
    }

    #[tokio::test]
    async fn video_job_uses_actual_provider_after_dispatch() {
        video_owner_after_dispatch(true).await;
    }

    async fn video_owner_after_dispatch(late_dispatch: bool) {
        use respan_gateway_core::{ClientContext, Plugin, Response, async_trait};

        struct RouteCreate(bool);
        #[async_trait]
        impl Plugin for RouteCreate {
            async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
                if matches!(cx.input::<video::Input>(), Ok(video::Input::Create(_))) {
                    if self.0 {
                        return Ok(Some(cx.call("openrouter").await?));
                    }
                    cx.set_target("openrouter");
                }
                Ok(None)
            }
        }

        let calls = Arc::new(Mutex::new(Vec::new()));
        let connector = |name: &'static str| {
            let calls = calls.clone();
            respan_gateway_http::connect(
                move |request: http::Request<Body>| {
                    let calls = calls.clone();
                    async move {
                        let path = request.uri().path().to_owned();
                        calls.lock().expect("calls").push((name, path.clone()));
                        let body = if path.ends_with("/content") {
                            "video bytes".to_owned()
                        } else {
                            json!({"id":"abc123", "status":"pending"}).to_string()
                        };
                        Ok((http::Response::new(Body::full(body)), None))
                    }
                },
                1 << 20,
            )
        };
        let (format, _) = VENDOR.format(OpenAiVideos::NAME).expect("format");
        let gateway = Gateway::new()
            .provider(format.provider("openrouter".into(), Arc::new(connector("openrouter"))))
            .expect("provider")
            .provider(format.provider("other".into(), Arc::new(connector("other"))))
            .expect("provider")
            .client::<video::Input, video::Output, _>(OpenAiVideos)
            .expect("client");
        let jobs = video::VideoJobs::new()
            .provider("openrouter", "openrouter", "video_")
            .provider("other", "other", "veo_");
        let gateway = if late_dispatch {
            gateway.plugin(jobs).plugin(RouteCreate(true))
        } else {
            gateway.plugin(RouteCreate(false)).plugin(jobs)
        };
        let gateway = gateway.target("other").expect("target");
        let (_, body, done) =
            post(&gateway, "/v1/videos", json!({"prompt":"wave","seconds":4})).await;
        done.expect("created");
        let created: Value = serde_json::from_slice(&body).expect("job");
        assert_eq!(created["id"], "openrouter~abc123");
        for path in [
            "/v1/videos/openrouter~abc123",
            "/v1/videos/openrouter~abc123/content",
        ] {
            let (_, _, done) =
                send(&gateway, http::Method::GET, path, "application/json", "").await;
            done.expect("follow-up");
        }
        assert_eq!(
            *calls.lock().expect("calls"),
            [
                ("openrouter", "/videos".to_owned()),
                ("openrouter", "/videos/abc123".to_owned()),
                ("openrouter", "/videos/abc123/content".to_owned()),
            ]
        );
    }

    #[tokio::test]
    async fn video_page_after_opaque_id_reaches_owner_unwrapped() {
        use respan_gateway_http::Upstream;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let connector = |name: &'static str| {
            let calls = calls.clone();
            respan_gateway_http::connect(
                move |request: http::Request<Body>| {
                    let calls = calls.clone();
                    async move {
                        calls
                            .lock()
                            .expect("calls")
                            .push((name, request.uri().to_string()));
                        let body = json!({"object":"list", "data":[
                        {"id":"opaque-next", "status":"queued"},
                        {"id":"video_123", "status":"queued"}
                    ], "has_more":false});
                        Ok((http::Response::new(Body::full(body.to_string())), None))
                    }
                },
                1 << 20,
            )
        };
        let gateway = Gateway::new()
            .provider(OpenAiVideos.provider("owner".into(), Arc::new(connector("owner"))))
            .expect("owner")
            .provider(OpenAiVideos.provider("other".into(), Arc::new(connector("other"))))
            .expect("other")
            .client::<video::Input, video::Output, _>(OpenAiVideos)
            .expect("client")
            .plugin(
                video::VideoJobs::new()
                    .provider("owner", "owner", "video_")
                    .provider("other", "other", "veo_"),
            )
            .target("other")
            .expect("target");
        let (_, body, done) = send(
            &gateway,
            http::Method::GET,
            "/v1/videos?after=owner~abc123",
            "application/json",
            "",
        )
        .await;
        done.expect("page");
        let page: Value = serde_json::from_slice(&body).expect("page json");
        assert_eq!(page["data"][0]["id"], "owner~opaque-next");
        assert_eq!(page["data"][1]["id"], "video_123");
        assert_eq!(
            *calls.lock().expect("calls"),
            [("owner", "/videos?after=abc123".to_owned())]
        );
    }

    #[tokio::test]
    async fn image_masks_are_refused_before_the_http_call() {
        let (gateway, calls) = gateway(OpenAiImages::NAME, "application/json", "{}");
        let (status, _, _) = post(&gateway, "/v1/images/edits", json!({"prompt":"edit","images":[{"image_url":"https://example.test/ref.png"}],"mask":{"image_url":"data:image/png;base64,eA=="}})).await;
        assert_eq!(status, http::StatusCode::BAD_REQUEST);
        assert!(calls.lock().expect("calls").is_empty());
    }
}

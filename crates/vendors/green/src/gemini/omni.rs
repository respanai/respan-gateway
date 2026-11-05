//! Gemini Omni 1.2 video generation through background Interactions.

fn is_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use respan_gateway_core::{ErrorCode, Fault, Filter, NativeFields};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::dialect::Dialect;
use respan_gateway_http::http::{request::Parts as RequestHead, response::Parts as ResponseHead};
use respan_gateway_http::{KeyIn, Rewrite};
use respan_gateway_media::video::{Create, Input, Output, Variant};
use serde_json::{Value, json};

const NAME: &str = "gemini-omni";
const PREFIX: &str = "omni_";

/// Gemini Omni's Interactions dialect for OpenAI Videos.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeminiOmni;

impl GeminiOmni {
    /// Namespace of provider-native usage fields.
    pub const NAME: &'static str = NAME;
    /// The exact supported model identity, without Google's optional prefix.
    pub const MODEL: &'static str = "gemini-omni-1.2-flash";
    /// Whether the model is the supported Omni identity.
    pub fn supports(model: &str) -> bool {
        model.strip_prefix("models/").unwrap_or(model) == Self::MODEL
    }
    /// Wrap an interaction ID as a video job ID.
    pub fn job_id(id: &str) -> String {
        format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(id))
    }
    /// Decode a safe interaction ID from a video job ID.
    pub fn interaction(id: &str) -> Option<String> {
        let raw = URL_SAFE_NO_PAD.decode(id.strip_prefix(PREFIX)?).ok()?;
        let id = String::from_utf8(raw).ok()?;
        is_segment(&id).then_some(id)
    }
}

impl Dialect for GeminiOmni {
    type Input = Input;
    type Output = Output;

    fn key(&self) -> Option<KeyIn> {
        Some(KeyIn::Header("x-goog-api-key"))
    }

    fn inputs(&self) -> Option<Box<dyn Filter<Input>>> {
        Some(Box::new(|input: &mut Input| {
            match input {
                Input::Create(create) => {
                    create_body(create)?;
                }
                Input::Retrieve { id }
                | Input::Content {
                    id,
                    variant: Variant::Video,
                    ..
                } => {
                    GeminiOmni::interaction(id)
                        .ok_or_else(|| Fault::not_found("no Gemini Omni video has this id"))?;
                }
                _ => return Err(unsupported("answer this video operation")),
            }
            Ok(())
        }))
    }

    fn outputs(&self) -> Option<Box<dyn Filter<Output>>> {
        Some(Box::new(|output: &mut Output| {
            if let Output::Job(job) = output {
                let fields = job.native.take().map(|n| n.fields).unwrap_or_default();
                job.native = Some(NativeFields::new(NAME, fields));
            }
            Ok(())
        }))
    }

    fn request(&self, head: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        let path = head.uri.path();
        if path == "/videos" {
            head.uri = "/interactions"
                .parse()
                .map_err(|e| Fault::internal("an interaction path is invalid").with_source(e))?;
            return Ok(Some(Box::new(|body| {
                let value: Value = serde_json::from_slice(&body)
                    .map_err(|e| Fault::internal("a video request is not JSON").with_source(e))?;
                let mut create = Create::new(value["prompt"].as_str().unwrap_or_default());
                create.model = value["model"].as_str().map(str::to_owned);
                create.seconds = value["seconds"].as_str().and_then(|n| n.parse().ok());
                create.size = value["size"].as_str().map(str::to_owned);
                create_body(&create)
            })));
        }
        let id = path
            .strip_prefix("/videos/")
            .and_then(|p| p.strip_suffix("/content").or(Some(p)))
            .and_then(GeminiOmni::interaction)
            .ok_or_else(|| Fault::not_found("no Gemini Omni video has this id"))?;
        head.uri = format!("/interactions/{id}")
            .parse()
            .map_err(|e| Fault::internal("an interaction path is invalid").with_source(e))?;
        Ok(None)
    }

    fn response(&self, sent: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        if !head.status.is_success() {
            return None;
        }
        let content = sent.uri.path().ends_with("/content");
        Some(Box::new(move |raw| {
            let value: Value = serde_json::from_slice(&raw)
                .map_err(|_| Fault::upstream("the interaction is not JSON"))?;
            let job = job_of(&value)?;
            if content {
                if job["status"] != "completed" {
                    return Err(Fault::bad_request("the Gemini Omni video is not ready")
                        .with(ErrorCode("video_not_ready".into())));
                }
                return video_bytes(&value);
            }
            let value = job;
            serde_json::to_vec(&value)
                .map(Bytes::from)
                .map_err(|e| Fault::internal("an interaction could not be encoded").with_source(e))
        }))
    }
}

fn unsupported(what: &str) -> Fault {
    Fault::bad_request(format!("Gemini Omni cannot {what}")).with(ErrorCode("unsupported".into()))
}

fn create_body(create: &Create) -> Result<Bytes, Fault> {
    let model = create
        .model
        .as_deref()
        .filter(|m| GeminiOmni::supports(m))
        .ok_or_else(|| Fault::bad_request("Gemini Omni needs model gemini-omni-1.2-flash"))?;
    if create.reference.is_some() {
        return Err(unsupported("use a reference image"));
    }
    if create.native.as_ref().is_some_and(|n| !n.fields.is_empty()) {
        return Err(unsupported("use additional video options"));
    }
    let seconds = create.seconds.unwrap_or(10);
    if !(3..=15).contains(&seconds) {
        return Err(Fault::bad_request(
            "Gemini Omni duration must be between 3 and 15 seconds",
        ));
    }
    let mut format = json!({"type": "video", "duration": format!("{seconds}s")});
    if let Some(size) = create.size.as_deref() {
        let (aspect, resolution) = match size {
            "640x360" => ("16:9", "360p"),
            "360x640" => ("9:16", "360p"),
            "1280x720" => ("16:9", "720p"),
            "720x1280" => ("9:16", "720p"),
            "1920x1080" => ("16:9", "1080p"),
            "1080x1920" => ("9:16", "1080p"),
            _ => return Err(Fault::bad_request("unsupported Gemini Omni video size")),
        };
        format["aspect_ratio"] = aspect.into();
        format["resolution"] = resolution.into();
    }
    let body = json!({"model": model.strip_prefix("models/").unwrap_or(model), "input": create.prompt,
        "background": true, "response_modalities": ["video"], "response_format": format});
    serde_json::to_vec(&body)
        .map(Bytes::from)
        .map_err(|e| Fault::internal("an interaction could not be encoded").with_source(e))
}

fn video_bytes(value: &Value) -> Result<Bytes, Fault> {
    let mut videos = value["steps"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|step| step["type"] == "model_output")
        .flat_map(|step| step["content"].as_array().into_iter().flatten())
        .filter(|content| content["type"] == "video");
    let video = videos
        .next()
        .ok_or_else(|| Fault::upstream("the completed interaction holds no video"))?;
    if videos.next().is_some() {
        return Err(Fault::upstream("the interaction holds multiple videos"));
    }
    if video["mime_type"]
        .as_str()
        .is_some_and(|mime| mime != "video/mp4")
    {
        return Err(Fault::upstream("the interaction video is not MP4"));
    }
    let encoded = video["data"]
        .as_str()
        .ok_or_else(|| Fault::upstream("the interaction video has no inline data"))?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| Fault::upstream("the interaction video is not valid base64"))?;
    if bytes.is_empty() {
        return Err(Fault::upstream("the interaction video is empty"));
    }
    Ok(bytes.into())
}

fn job_of(value: &Value) -> Result<Value, Fault> {
    let id = value["id"]
        .as_str()
        .filter(|id| is_segment(id))
        .ok_or_else(|| Fault::upstream("the interaction has no safe id"))?;
    let status = match value["status"].as_str() {
        Some("in_progress") => "in_progress",
        Some("completed") => {
            video_bytes(value)?;
            "completed"
        }
        Some("failed" | "cancelled") => "failed",
        _ => return Err(Fault::upstream("the interaction has an unsupported status")),
    };
    let mut job = json!({"id": GeminiOmni::job_id(id), "status": status, "model": value["model"]});
    if status == "failed" {
        job["error"] = json!({"code": value["error"]["code"].as_str(),
            "message": value["error"]["message"].as_str().unwrap_or("the Gemini Omni interaction failed")});
    }
    if let Some(usage) = value.get("usage") {
        job["usage"] = usage.clone();
    }
    Ok(job)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt, TryStreamExt};
    use respan_gateway_core::Format;
    use respan_gateway_core::{EventStream, Extensions};
    use respan_gateway_http::Body;
    use respan_gateway_http::dialect::Dialected;
    use respan_gateway_http::http::{self, Method};
    use respan_gateway_media::video::Status;
    use respan_gateway_media_http::openai::OpenAiVideos;

    fn create() -> Create {
        let mut create = Create::new("a paper boat");
        create.model = Some(GeminiOmni::MODEL.into());
        create
    }
    fn completed() -> Value {
        json!({"id": "v1_abc", "status": "completed", "model": GeminiOmni::MODEL,
            "steps": [{"type": "model_output", "content": [{"type": "video", "mime_type": "video/mp4", "data": "dmlkZW8="}]}],
            "usage": {"total_input_tokens": 12, "total_output_tokens": 300}})
    }
    #[test]
    fn request_validates_duration_model_size_and_options() {
        let mut input = create();
        input.size = Some("360x640".into());
        let value: Value =
            serde_json::from_slice(&create_body(&input).expect("body")).expect("JSON");
        assert_eq!(
            value,
            json!({"model": GeminiOmni::MODEL, "input": "a paper boat", "background": true,
            "response_modalities": ["video"], "response_format": {"type": "video", "duration": "10s", "aspect_ratio": "9:16", "resolution": "360p"}})
        );
        for seconds in [3, 15] {
            input.seconds = Some(seconds);
            assert!(create_body(&input).is_ok());
        }
        for seconds in [0, 2, 16] {
            input.seconds = Some(seconds);
            assert!(create_body(&input).is_err());
        }
        input = create();
        input.model = Some("gemini-omni-1.2-flash-extra".into());
        assert!(create_body(&input).is_err());
        input = create();
        input.size = Some("100x100".into());
        assert!(create_body(&input).is_err());
        input = create();
        input.native = Some(NativeFields::new(
            "openai-videos",
            serde_json::Map::from_iter([("seed".into(), json!(1))]),
        ));
        assert!(create_body(&input).is_err());
    }
    #[test]
    fn job_ids_cannot_escape_the_interactions_path() {
        assert_eq!(
            GeminiOmni::interaction(&GeminiOmni::job_id("v1_abc")).as_deref(),
            Some("v1_abc")
        );
        for id in ["../secret", "abc?key=x", "a/b", "", ".", ".."] {
            assert!(GeminiOmni::interaction(&GeminiOmni::job_id(id)).is_none());
        }
        assert!(GeminiOmni::interaction("veo_abc").is_none());
    }
    #[test]
    fn only_model_outputs_supply_video_and_usage_is_preserved() {
        let value = completed();
        let job = job_of(&value).expect("job");
        assert_eq!(job["status"], "completed");
        assert_eq!(job["usage"], value["usage"]);
        assert_eq!(
            video_bytes(&value).expect("bytes"),
            Bytes::from_static(b"video")
        );
        let mut bad = value;
        bad["steps"][0]["type"] = "user_input".into();
        assert!(job_of(&bad).is_err());
        for data in ["", "!!", "data:video/mp4;base64,dmlkZW8="] {
            let mut bad = completed();
            bad["steps"][0]["content"][0]["data"] = data.into();
            assert!(job_of(&bad).is_err());
        }
        let mut url = completed();
        url["steps"][0]["content"][0] = json!({"type": "video", "uri": "https://evil.test/video"});
        assert!(job_of(&url).is_err());
    }
    #[test]
    fn statuses_and_missing_usage_remain_distinct() {
        for (status, expected) in [
            ("in_progress", "in_progress"),
            ("failed", "failed"),
            ("cancelled", "failed"),
        ] {
            let value = json!({"id": "abc", "status": status, "error": {"code": "blocked", "message": "no"}});
            let job = job_of(&value).expect("job");
            assert_eq!(job["status"], expected);
            assert!(job.get("usage").is_none());
            if expected == "failed" {
                assert_eq!(job["error"]["message"], "no");
            }
        }
        assert!(job_of(&json!({"id": "abc", "status": "unknown"})).is_err());
        assert!(job_of(&json!({"id": "abc", "status": "completed"})).is_err());
    }
    #[test]
    fn provider_refusals_are_left_for_the_format_to_read() {
        let sent = http::Request::get("/videos")
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let mut response = http::Response::new(()).into_parts().0;
        response.status = http::StatusCode::TOO_MANY_REQUESTS;
        assert!(GeminiOmni.response(&sent, &mut response).is_none());
        assert_eq!(response.status, 429);
    }
    async fn exchange(
        input: Input,
        answer: Value,
        method: Method,
        path: &str,
    ) -> Result<Vec<Output>, Fault> {
        let path = path.to_owned();
        let factory = respan_gateway_http::connect(
            move |request| {
                let answer = answer.clone();
                assert_eq!(request.method(), method);
                assert_eq!(request.uri().path(), path);
                async move {
                    let body = request.into_body().read_answer(4096).await?;
                    if !body.is_empty() {
                        let value: Value = serde_json::from_slice(&body).expect("JSON");
                        assert_eq!(
                            value,
                            json!({"model": GeminiOmni::MODEL, "input": "a paper boat", "background": true, "response_modalities": ["video"], "response_format": {"type": "video", "duration": "10s"}})
                        );
                    }
                    Ok((http::Response::new(Body::full(answer.to_string())), None))
                }
            },
            8192,
        );
        let (opening, mut outgoing) =
            Dialected::new(OpenAiVideos, GeminiOmni).bind(factory(Extensions::new()))?;
        let mut source = EventStream::single(input);
        outgoing.prepare(&mut source)?;
        let send = async move {
            while let Some(event) = source.next().await {
                outgoing.send(event).await?;
            }
            outgoing.close().await
        };
        let receive = async move { opening.await?.try_collect::<Vec<_>>().await };
        let (_, events) = futures::try_join!(send, receive)?;
        Ok(events)
    }
    #[tokio::test]
    async fn mocked_create_poll_and_content_use_the_provider_boundary() {
        let started = exchange(
            Input::Create(create()),
            json!({"id": "v1_abc", "status": "in_progress"}),
            Method::POST,
            "/interactions",
        )
        .await
        .expect("created");
        let Output::Job(job) = &started[0] else {
            panic!("job")
        };
        let polled = exchange(
            Input::Retrieve { id: job.id.clone() },
            completed(),
            Method::GET,
            "/interactions/v1_abc",
        )
        .await
        .expect("polled");
        let Output::Job(done) = &polled[0] else {
            panic!("job")
        };
        assert_eq!(done.status, Status::Completed);
        assert_eq!(
            done.native.as_ref().expect("usage").fields["usage"]["total_output_tokens"],
            300
        );
        let content = exchange(
            Input::Content {
                id: job.id.clone(),
                variant: Variant::Video,
                url: Some("https://evil.test".into()),
            },
            completed(),
            Method::GET,
            "/interactions/v1_abc",
        )
        .await
        .expect("content");
        assert_eq!(content, vec![Output::Content(Bytes::from_static(b"video"))]);
        assert!(
            exchange(
                Input::Content {
                    id: job.id.clone(),
                    variant: Variant::Video,
                    url: None
                },
                json!({"id": "v1_abc", "status": "in_progress"}),
                Method::GET,
                "/interactions/v1_abc"
            )
            .await
            .is_err()
        );
    }
}

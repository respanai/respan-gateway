//! MiniMax's OpenAI-compatible chat and H3 image API.

use respan_gateway_conversation::MediaSource;
use respan_gateway_conversation_http::fields::media_url;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{Fault, Filter};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::{request::Parts as RequestHead, response::Parts as ResponseHead};
use respan_gateway_media::image;
use respan_gateway_media_http::openai::OpenAiImages;
use respan_gateway_vendor_kit::{Dialect, Dialected, Rewrite, Vendor};
use serde_json::{Value, json};

const BASE_URL: &str = "https://api.minimax.io/v1";

/// MiniMax's OpenAI-compatible chat and image endpoints.
pub const VENDOR: Vendor = Vendor {
    id: "minimax",
    key_env: &["MINIMAX_API_KEY"],
    formats: &[
        (&OpenAiChat, Some(BASE_URL)),
        (&Dialected::new(OpenAiImages, MiniMaxImages), Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// MiniMax images: OpenAI requests and successful responses, with per-image
/// errors retained as native metadata. Upstream streaming is disabled; a
/// streaming client receives shared SSE framing after the JSON answer arrives.
#[derive(Debug, Clone, Copy)]
pub struct MiniMaxImages;

impl Dialect for MiniMaxImages {
    type Input = image::Input;
    type Output = image::Output;

    fn inputs(&self) -> Option<Box<dyn Filter<image::Input>>> {
        Some(Box::new(validate))
    }

    fn request(&self, _: &mut RequestHead) -> Result<Option<Rewrite>, Fault> {
        // WARN: MiniMax edits take no mask; it is dropped.
        let body = |body| crate::without(crate::without(body, "stream")?, "mask");
        Ok(Some(Box::new(body)))
    }

    fn response(&self, _: &RequestHead, head: &mut ResponseHead) -> Option<Rewrite> {
        head.status
            .is_success()
            .then(|| Box::new(successful_images) as Rewrite)
    }
}

fn validate(input: &mut image::Input) -> Result<(), Fault> {
    if input.count.is_some_and(|n| !(1..=10).contains(&n)) {
        return Err(Fault::bad_request(
            "MiniMax image count must be between 1 and 10",
        ));
    }
    if input.images.len() > 16 {
        return Err(Fault::bad_request(
            "MiniMax edits accept at most 16 reference images",
        ));
    }
    let fields = OpenAiImages::fields(&input.native);
    if fields.get("background").and_then(Value::as_str) == Some("transparent") {
        return Err(Fault::bad_request(
            "MiniMax images do not support transparent backgrounds",
        ));
    }
    // MiniMax accepts data URLs, including for SDK multipart uploads. Keep
    // the upstream body JSON so the request rewrite also covers file edits.
    for media in &mut input.images {
        media.source = MediaSource::Url(media_url(media));
    }
    Ok(())
}

/// Leave ordinary responses to the shared decoder; only remove failed rows
/// that the OpenAI spec cannot represent, retaining their original positions.
fn successful_images(body: Bytes) -> Result<Bytes, Fault> {
    let unreadable = |e| Fault::upstream("MiniMax image response is not JSON").with_source(e);
    let mut response: Value = serde_json::from_slice(&body).map_err(unreadable)?;
    let Some(data) = response.get_mut("data").and_then(Value::as_array_mut) else {
        return Ok(body);
    };
    if !data.iter().any(|item| item.get("error").is_some()) {
        return Ok(body);
    }
    let mut failures = Vec::new();
    for (index, item) in data.iter_mut().enumerate() {
        if let Some(error) = item.get("error") {
            failures.push(json!({"image_index": index, "error": error}));
        } else if let Some(fields) = item.as_object_mut() {
            fields.insert("image_index".into(), index.into());
        }
    }
    data.retain(|item| item.get("error").is_none());
    if data.is_empty() {
        return Err(Fault::upstream(
            "MiniMax image response contains no successful images",
        ));
    }
    response["failed_images"] = failures.into();
    serde_json::to_vec(&response)
        .map(Bytes::from)
        .map_err(unreadable)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::Media;
    use respan_gateway_core::{
        ClientContext, Gateway, Native, Plugin, Provider, Response, Result, async_trait,
    };
    use respan_gateway_http::{
        Body,
        http::{self, Method, Request},
        http_body_util::BodyExt,
    };
    use std::sync::{Arc, Mutex};

    struct Select;
    #[async_trait]
    impl Plugin for Select {
        async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>> {
            cx.set_target("minimax".to_owned());
            Ok(None)
        }
    }

    fn receipt(data: Value) -> Value {
        json!({"created":1792308695,"model":"MiniMax-H3-Image","output_format":"jpeg",
            "data":data,"request_id":"mock-request",
            "usage":{"generated_images":1,"total_tokens":7000,"input_tokens":3000,"output_tokens":4000,
                "input_tokens_details":{"text_tokens":1000,"image_tokens":2000},
                "output_tokens_details":{"image_tokens":4000}}})
    }

    /// Exercise both HTTP edges, not the rewrite alone. No network or keys.
    async fn send(path: &str, body: Value, status: u16, response: Value) -> (u16, String, Value) {
        let request = Request::builder()
            .method(Method::POST)
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::full(body.to_string()))
            .expect("request");
        exchange(request, status, response).await
    }

    async fn exchange(
        request: Request<Body>,
        status: u16,
        response: Value,
    ) -> (u16, String, Value) {
        let observed = Arc::new(Mutex::new(Value::Null));
        let capture = observed.clone();
        let connect = respan_gateway_http::connect(
            move |request: Request<Body>| {
                let capture = capture.clone();
                let response = response.clone();
                async move {
                    let path = request.uri().path().to_owned();
                    let bytes = BodyExt::collect(request.into_body())
                        .await
                        .expect("body")
                        .to_bytes();
                    *capture.lock().expect("capture") = json!({"path":path,"body":serde_json::from_slice::<Value>(&bytes).expect("JSON request")});
                    Ok((
                        http::Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(Body::full(response.to_string()))
                            .expect("response"),
                        None,
                    ))
                }
            },
            1 << 20,
        );
        let gateway = Gateway::new()
            .client::<image::Input, image::Output, _>(OpenAiImages)
            .expect("client")
            .provider(Provider::new(
                "minimax",
                Dialected::new(OpenAiImages, MiniMaxImages),
                connect,
            ))
            .expect("provider")
            .plugin(Select);
        let (response, _) = respan_gateway_http::handle(&gateway, request, 1 << 20).await;
        let status = response.status().as_u16();
        let bytes = BodyExt::collect(response.into_body())
            .await
            .expect("body")
            .to_bytes();
        gateway.idle().await;
        let sent = observed.lock().expect("capture").clone();
        let returned = String::from_utf8(bytes.to_vec()).expect("text");
        (status, returned, sent)
    }

    #[tokio::test]
    async fn generation_and_edit_keep_images_native_parameters_and_usage() {
        for (path, extra) in [
            (
                "/v1/images/generations",
                json!({"aspect_ratio":"16:9","area":"2K"}),
            ),
            (
                "/v1/images/edits",
                json!({"images":[{"image_url":"https://example.com/cat.jpg"}]}),
            ),
        ] {
            let mut body = json!({"model":"MiniMax-H3-Image","prompt":"a fox"});
            body.as_object_mut()
                .expect("object")
                .extend(extra.as_object().expect("object").clone());
            let (status, returned, sent) = send(
                path,
                body,
                200,
                receipt(json!([{"url":"https://example.com/result.jpg","revised_prompt":"a fox"}])),
            )
            .await;
            assert_eq!(status, 200, "{returned}");
            let returned: Value = serde_json::from_str(&returned).expect("JSON");
            assert_eq!(returned["data"][0]["url"], "https://example.com/result.jpg");
            assert_eq!(returned["usage"]["input_tokens"], 3000);
            assert_eq!(
                returned["usage"]["input_tokens_details"]["image_tokens"],
                2000
            );
            assert_eq!(
                returned["usage"]["output_tokens_details"]["image_tokens"],
                4000
            );
            assert_eq!(returned["request_id"], "mock-request");
            assert_eq!(sent["body"]["model"], "MiniMax-H3-Image");
            assert!(sent["body"].get("stream").is_none());
            if path.ends_with("edits") {
                assert_eq!(sent["path"], "/images/edits");
                assert_eq!(
                    sent["body"]["images"][0]["image_url"],
                    "https://example.com/cat.jpg"
                );
            } else {
                assert_eq!(sent["path"], "/images/generations");
                assert_eq!(sent["body"]["area"], "2K");
                assert_eq!(sent["body"]["aspect_ratio"], "16:9");
            }
        }
    }

    #[tokio::test]
    async fn partial_failures_keep_original_indices_and_one_usage_receipt() {
        let data = json!([{"error":{"code":"1027","message":"blocked"}}, {"url":"https://example.com/1.jpg"}]);
        let (status, body, _) = send(
            "/v1/images/generations",
            json!({"model":"MiniMax-H3-Image","prompt":"two foxes","n":2}),
            200,
            receipt(data),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let body: Value = serde_json::from_str(&body).expect("JSON");
        assert_eq!(body["data"].as_array().expect("images").len(), 1);
        assert_eq!(body["data"][0]["image_index"], 1);
        assert_eq!(body["failed_images"][0]["image_index"], 0);
        assert_eq!(body["failed_images"][0]["error"]["code"], "1027");
        assert_eq!(body["usage"]["output_tokens"], 4000);
    }

    #[tokio::test]
    async fn client_streams_use_shared_completion_events_and_keep_usage() {
        for path in ["/v1/images/generations", "/v1/images/edits"] {
            let mut request =
                json!({"model":"MiniMax-H3-Image","prompt":"two foxes","n":2,"stream":true});
            if path.ends_with("edits") {
                request["images"] = json!([{"image_url":"https://example.com/cat.jpg"}]);
            }
            let (status, body, sent) = send(path, request, 200, receipt(json!([
                {"url":"https://example.com/0.jpg"}, {"error":{"code":"1027","message":"blocked"}}
            ]))).await;
            assert_eq!(status, 200, "{body}");
            assert!(sent["body"].get("stream").is_none());
            let events: Vec<Value> = body
                .lines()
                .filter_map(|line| line.strip_prefix("data: "))
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect();
            let completed: Vec<_> = events
                .iter()
                .filter(|v| {
                    v["type"]
                        .as_str()
                        .is_some_and(|s| s.ends_with(".completed"))
                })
                .collect();
            assert_eq!(completed.len(), 1, "{body}");
            assert_eq!(completed[0]["usage"]["output_tokens"], 4000, "{body}");
            assert_eq!(
                completed[0]["usage"]["input_tokens_details"]["image_tokens"], 2000,
                "{body}"
            );
            assert!(!body.contains("minimax.image_"), "{body}");
            assert!(body.contains("failed_images"), "{body}");
        }
    }

    #[tokio::test]
    async fn base64_uses_the_shared_decoder() {
        let mut response = receipt(json!([{"b64_json":"/9j/"}]));
        response
            .as_object_mut()
            .expect("object")
            .shift_remove("output_format");
        let (status, body, _) = send(
            "/v1/images/generations",
            json!({"model":"MiniMax-H3-Image","prompt":"fox","response_format":"b64_json"}),
            200,
            response,
        )
        .await;
        assert_eq!(status, 200, "{body}");
        let body: Value = serde_json::from_str(&body).expect("JSON");
        assert_eq!(body["data"][0]["b64_json"], "/9j/");
    }

    #[tokio::test]
    async fn upstream_errors_and_all_failed_responses_do_not_become_successes() {
        for status in [402, 422, 429, 500] {
            let (actual, body, _) = send("/v1/images/generations", json!({"model":"MiniMax-H3-Image","prompt":"fox"}), status,
                json!({"error":{"message":"rejected","type":"server_error"},"data":[{"error":{"code":"1027","message":"blocked"}}]})).await;
            assert_eq!(actual, status, "{body}");
        }
        let (status, body, _) = send(
            "/v1/images/generations",
            json!({"model":"MiniMax-H3-Image","prompt":"fox"}),
            200,
            receipt(json!([{"error":{"code":"1027","message":"blocked"}}])),
        )
        .await;
        assert_eq!(status, 502, "{body}");
    }

    #[tokio::test]
    async fn sdk_multipart_edit_becomes_a_nonstreaming_json_edit() {
        let body = concat!(
            "--mock-boundary\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nMiniMax-H3-Image\r\n",
            "--mock-boundary\r\nContent-Disposition: form-data; name=\"prompt\"\r\n\r\nedit it\r\n",
            "--mock-boundary\r\nContent-Disposition: form-data; name=\"stream\"\r\n\r\ntrue\r\n",
            "--mock-boundary\r\nContent-Disposition: form-data; name=\"image\"; filename=\"cat.png\"\r\nContent-Type: image/png\r\n\r\nPNG\r\n",
            "--mock-boundary\r\nContent-Disposition: form-data; name=\"mask\"; filename=\"mask.png\"\r\nContent-Type: image/png\r\n\r\nPNG\r\n",
            "--mock-boundary--\r\n"
        );
        let request = Request::builder()
            .method(Method::POST)
            .uri("/v1/images/edits")
            .header(
                "content-type",
                "multipart/form-data; boundary=mock-boundary",
            )
            .body(Body::full(body))
            .expect("request");
        let (status, body, sent) = exchange(
            request,
            200,
            receipt(json!([{"url":"https://example.com/result.jpg"}])),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(sent["path"], "/images/edits");
        assert_eq!(
            sent["body"]["images"][0]["image_url"],
            "data:image/png;base64,UE5H"
        );
        assert!(sent["body"].get("stream").is_none());
        assert!(sent["body"].get("mask").is_none(), "a mask is dropped");
        assert!(body.contains("image_edit.completed"), "{body}");
    }

    #[tokio::test]
    async fn invalid_input_is_refused_before_http() {
        for extra in [json!({"n":11}), json!({"background":"transparent"})] {
            let mut request = json!({"model":"MiniMax-H3-Image","prompt":"fox"});
            request
                .as_object_mut()
                .expect("object")
                .extend(extra.as_object().expect("object").clone());
            let (status, body, sent) =
                send("/v1/images/edits", request, 200, receipt(json!([]))).await;
            assert_eq!(status, 400, "{body}");
            assert!(sent.is_null(), "no request should be sent");
        }
    }

    #[test]
    fn ordinary_responses_are_untouched_and_malformed_json_is_refused() {
        let body =
            Bytes::from(receipt(json!([{"url":"https://example.com/result.jpg"}])).to_string());
        assert_eq!(successful_images(body.clone()).expect("response"), body);
        assert!(successful_images(Bytes::from_static(b"{")).is_err());
        for invalid in ["null", "42", "[]", "true"] {
            let body = Bytes::from(invalid);
            assert_eq!(
                successful_images(body.clone()).expect("left to shared decoder"),
                body
            );
        }
    }

    #[test]
    fn validation_respects_native_identity_and_converts_file_references() {
        let mut input = image::Input::new("fox");
        let transparent = json!({"background": "transparent"});
        input.native = Native::json::<()>(transparent.clone());
        validate(&mut input).expect("foreign native fields ignored");
        input.native = OpenAiImages::native(transparent);
        assert!(validate(&mut input).is_err());
        input.native = Native::default();
        for (count, accepted) in [(0, false), (1, true), (10, true), (11, false)] {
            input.count = Some(count);
            assert_eq!(validate(&mut input).is_ok(), accepted);
        }
        input.count = None;
        let media = Media::new(
            "image/jpeg",
            MediaSource::Bytes(Bytes::from_static(b"\xff\xd8\xff")),
        );
        input.images = vec![media.clone(); 16];
        validate(&mut input).expect("16 references");
        assert!(
            matches!(&input.images[0].source, MediaSource::Url(url) if url == "data:image/jpeg;base64,/9j/")
        );
        input.images.push(media.clone());
        assert!(validate(&mut input).is_err());
    }
}

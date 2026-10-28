//! Amazon Bedrock: Claude's Messages at its Invoke API, in a region's
//! `bedrock-runtime`, authenticated by a Bedrock API key or signed (SigV4)
//! by an AWS key pair.
//!
//! The Messages body goes to `/model/{id}/invoke`, or
//! `invoke-with-response-stream`, without its `model` and `stream`, with
//! Bedrock's `anthropic_version` and the `anthropic-beta` header's betas as
//! `anthropic_beta`. A whole answer is Anthropic's own; a stream is an AWS
//! event stream whose `chunk` events each carry one Anthropic event, read
//! as Anthropic's SSE.

use std::sync::Arc;
use std::time::SystemTime;

use aws_credential_types::Credentials;
use aws_sigv4::http_request::{SignableBody, SignableRequest, SigningSettings, sign};
use aws_sigv4::sign::v4;
use aws_smithy_runtime_api::client::identity::Identity;
use base64::Engine as _;
use respan_gateway_conversation::{Input, Output};
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_core::Fault;
use respan_gateway_eventstream::{Message, Parser};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::header::{CONTENT_LENGTH, CONTENT_TYPE, HeaderName, HeaderValue};
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::http::response::Parts as ResponseHead;
use respan_gateway_http::path::escape;
use serde_json::{Map, Value, json};

use respan_gateway_vendor_kit::{
    BoxFuture, Dialect, Dialected, KeyIn, Models, Rewrite, Sign, Transcode, Vendor, VendorProvider,
};

/// Where a region's runtime is: `AWS_REGION`'s, unless a host says.
const BASE_URL: &str = "https://bedrock-runtime.{AWS_REGION}.amazonaws.com";

/// The name Bedrock signs as, not its host's `bedrock-runtime`.
const SIGNING_NAME: &str = "bedrock";

/// The Messages version Bedrock takes.
const ANTHROPIC_VERSION: &str = "bedrock-2023-05-31";

const BETA_HEADER: &str = "anthropic-beta";
const EVENT_STREAM: &str = "application/vnd.amazon.eventstream";

/// Amazon Bedrock: its key a Bedrock API key, else each call signed by the
/// environment's AWS key pair.
pub const VENDOR: Vendor = Vendor {
    id: "bedrock",
    key_env: &["AWS_BEARER_TOKEN_BEDROCK"],
    formats: &[(&Dialected::new(AnthropicMessages, Bedrock), Some(BASE_URL))],
    models: no_models,
    signer: Some(environment_signer),
    ..Vendor::DEFAULT
};

/// Bedrock's Messages, as it departs from Anthropic's.
#[derive(Debug, Clone, Copy)]
pub struct Bedrock;

impl Dialect for Bedrock {
    type Input = Input;
    type Output = Output;

    fn key(&self) -> Option<KeyIn> {
        Some(KeyIn::Bearer)
    }

    fn request(
        &self,
        head: &mut RequestHead,
        body: Option<&Bytes>,
    ) -> Result<Option<Rewrite>, Fault> {
        let call: Map<String, Value> = body
            .and_then(|body| serde_json::from_slice(body).ok())
            .ok_or_else(|| Fault::internal("a Messages call has a whole JSON body"))?;
        let model = call["model"].as_str().unwrap_or_default();
        if model.is_empty() {
            return Err(Fault::bad_request("a Bedrock call names its model"));
        }
        let invoke = match call.get("stream") == Some(&Value::Bool(true)) {
            true => "invoke-with-response-stream",
            false => "invoke",
        };
        head.uri = format!("/model/{}/{invoke}", escape(model))
            .parse()
            .map_err(|_| Fault::bad_request("the model's id makes no path"))?;
        let betas: Vec<Value> = (head.headers.get_all(BETA_HEADER).iter())
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::trim)
            .filter(|beta| !beta.is_empty())
            .map(Value::from)
            .collect();
        head.headers.remove(BETA_HEADER);
        head.headers.remove("anthropic-version");
        Ok(Some(Box::new(move |_| {
            let mut call = call;
            call.remove("model");
            call.remove("stream");
            call.entry("anthropic_version")
                .or_insert(ANTHROPIC_VERSION.into());
            if !betas.is_empty() {
                call.entry("anthropic_beta").or_insert(betas.into());
            }
            serde_json::to_vec(&call)
                .map(Bytes::from)
                .map_err(|e| Fault::internal(e.to_string()))
        })))
    }

    fn stream(&self, _: &RequestHead, head: &mut ResponseHead) -> Option<Transcode> {
        let content_type = head.headers.get(CONTENT_TYPE)?.to_str().ok()?;
        if !content_type.starts_with(EVENT_STREAM) {
            return None;
        }
        head.headers
            .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
        head.headers.remove(CONTENT_LENGTH);
        let mut parser = Parser::new(None);
        Some(Box::new(move |piece| {
            let sse: String = parser.push(piece)?.iter().filter_map(as_sse).collect();
            Ok(Bytes::from(sse))
        }))
    }
}

/// One message of Bedrock's stream as Anthropic's SSE: a `chunk`'s
/// event, or an exception as an `error` event.
fn as_sse(message: &Message) -> Option<String> {
    let event = match message.text(":message-type") {
        Some("event") if message.text(":event-type") == Some("chunk") => {
            let payload: Value = serde_json::from_slice(&message.payload).ok()?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(payload["bytes"].as_str()?)
                .ok()?;
            serde_json::from_slice::<Value>(&bytes).ok()?
        }
        Some("exception" | "error") => {
            let kind = (message.text(":exception-type"))
                .or_else(|| message.text(":error-code"))
                .unwrap_or("error");
            let said: Value = serde_json::from_slice(&message.payload).unwrap_or_default();
            let text = said["message"].as_str().unwrap_or(kind);
            json!({ "type": "error", "error": { "type": kind, "message": text } })
        }
        _ => return None,
    };
    let name = event["type"].as_str()?.to_owned();
    Some(format!("event: {name}\ndata: {event}\n\n"))
}

/// Bedrock's models are its account's: none are listed.
fn no_models(_: &VendorProvider) -> BoxFuture<'_, Models> {
    Box::pin(async { Ok(Vec::new()) })
}

/// The environment's AWS key pair (`AWS_ACCESS_KEY_ID`,
/// `AWS_SECRET_ACCESS_KEY`, `AWS_SESSION_TOKEN`) in `AWS_REGION`, if set.
fn environment_signer() -> Option<Arc<dyn Sign>> {
    let env = |name| std::env::var(name).ok().filter(|v: &String| !v.is_empty());
    Some(signer(
        env("AWS_ACCESS_KEY_ID")?,
        env("AWS_SECRET_ACCESS_KEY")?,
        env("AWS_SESSION_TOKEN"),
        env("AWS_REGION")?,
    ))
}

/// The signer of Bedrock calls in `region` by an AWS key pair, for a host
/// with a tenant's own (`CallSigner`).
pub fn signer(
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    region: String,
) -> Arc<dyn Sign> {
    let credentials = Credentials::new(
        access_key_id,
        secret_access_key,
        session_token,
        None,
        SIGNING_NAME,
    );
    Arc::new(SigV4 {
        identity: credentials.into(),
        region,
    })
}

/// SigV4 by an AWS key pair, in a region.
struct SigV4 {
    identity: Identity,
    region: String,
}

impl Sign for SigV4 {
    fn sign(&self, head: &mut RequestHead, body: &[u8]) -> Result<(), Fault> {
        let refused = |e: &dyn std::fmt::Display| Fault::internal(format!("SigV4: {e}"));
        let params = v4::SigningParams::builder()
            .identity(&self.identity)
            .region(&self.region)
            .name(SIGNING_NAME)
            .time(SystemTime::now())
            .settings(SigningSettings::default())
            .build()
            .map_err(|e| refused(&e))?
            .into();
        let headers = (head.headers.iter())
            .filter_map(|(name, value)| Some((name.as_str(), value.to_str().ok()?)));
        let uri = head.uri.to_string();
        let request = SignableRequest::new(
            head.method.as_str(),
            uri.as_str(),
            headers,
            SignableBody::Bytes(body),
        )
        .map_err(|e| refused(&e))?;
        let (signed, _) = sign(request, &params)
            .map_err(|e| refused(&e))?
            .into_parts();
        for (name, value) in signed.headers() {
            let name = HeaderName::try_from(name).map_err(|e| refused(&e))?;
            let mut value = HeaderValue::try_from(value).map_err(|e| refused(&e))?;
            value.set_sensitive(true);
            head.headers.insert(name, value);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_eventstream::Header;
    use respan_gateway_http::http;

    fn call(stream: bool) -> (RequestHead, Bytes) {
        let body = json!({ "model": "anthropic.claude-haiku-5-5-v1:0", "stream": stream,
            "max_tokens": 8, "messages": [{ "role": "user", "content": "hi" }] });
        let head = http::Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("anthropic-version", "2023-06-01")
            .header(BETA_HEADER, "a-1, b-2")
            .body(())
            .expect("a call");
        (head.into_parts().0, Bytes::from(body.to_string()))
    }

    #[test]
    fn a_call_is_its_models_invoke_with_bedrocks_body() {
        let (mut head, body) = call(true);
        let rewrite = Bedrock.request(&mut head, Some(&body)).expect("a call");
        assert_eq!(
            head.uri.path(),
            "/model/anthropic.claude-haiku-5-5-v1%3A0/invoke-with-response-stream"
        );
        assert!(!head.headers.contains_key(BETA_HEADER));
        assert!(!head.headers.contains_key("anthropic-version"));
        let sent = rewrite.expect("a rewrite")(body).expect("a body");
        let sent: Value = serde_json::from_slice(&sent).expect("JSON");
        assert_eq!(sent.get("model"), None);
        assert_eq!(sent.get("stream"), None);
        assert_eq!(sent["anthropic_version"], ANTHROPIC_VERSION);
        assert_eq!(sent["anthropic_beta"], json!(["a-1", "b-2"]));
        let (mut head, body) = call(false);
        Bedrock.request(&mut head, Some(&body)).expect("a call");
        assert!(head.uri.path().ends_with("/invoke"));
    }

    #[test]
    fn a_key_pair_signs_the_call_as_sent() {
        let (mut head, body) = call(false);
        head.uri = "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/invoke"
            .parse()
            .expect("a URL");
        let signer = signer(
            "AKID".into(),
            "secret".into(),
            Some("token".into()),
            "us-east-1".into(),
        );
        signer.sign(&mut head, &body).expect("signed");
        let auth = head.headers["authorization"].to_str().expect("text");
        assert!(
            auth.starts_with("AWS4-HMAC-SHA256 Credential=AKID/"),
            "{auth}"
        );
        assert!(auth.contains("/us-east-1/bedrock/aws4_request"), "{auth}");
        assert!(head.headers.contains_key("x-amz-date"));
        assert_eq!(head.headers["x-amz-security-token"], "token");
    }

    fn message(headers: &[(&str, &str)], payload: Value) -> Bytes {
        let headers = headers.iter();
        let headers = headers.map(|(n, v)| (n.to_string(), Header::String(v.to_string())));
        Message {
            headers: headers.collect(),
            payload: Bytes::from(payload.to_string()),
        }
        .encode()
    }

    fn chunk(event: &Value) -> Bytes {
        let bytes = base64::engine::general_purpose::STANDARD.encode(event.to_string());
        let headers = [(":message-type", "event"), (":event-type", "chunk")];
        message(&headers, json!({ "bytes": bytes }))
    }

    fn transcoded(pieces: &[Bytes]) -> (ResponseHead, String) {
        let answer = http::Response::builder()
            .header(CONTENT_TYPE, EVENT_STREAM)
            .body(())
            .expect("an answer");
        let mut head = answer.into_parts().0;
        let sent = http::Request::new(()).into_parts().0;
        let mut transcode = Bedrock.stream(&sent, &mut head).expect("a transcoder");
        let mut sse = Vec::new();
        for piece in pieces {
            sse.extend_from_slice(&transcode(piece.clone()).expect("read"));
        }
        (head, String::from_utf8(sse).expect("text"))
    }

    #[test]
    fn a_stream_is_anthropics_sse_wherever_the_network_cuts_it() {
        let start = json!({ "type": "message_start", "message": { "id": "m" } });
        let stop = json!({ "type": "message_stop" });
        let wire = [chunk(&start), chunk(&stop)].concat();
        let pieces: Vec<Bytes> = wire.chunks(7).map(Bytes::copy_from_slice).collect();
        let (head, sse) = transcoded(&pieces);
        assert_eq!(head.headers[CONTENT_TYPE], "text/event-stream");
        assert_eq!(
            sse,
            format!("event: message_start\ndata: {start}\n\nevent: message_stop\ndata: {stop}\n\n")
        );
    }

    #[test]
    fn an_exception_in_the_stream_is_anthropics_error_event() {
        let headers = [
            (":message-type", "exception"),
            (":exception-type", "throttlingException"),
        ];
        let (_, sse) = transcoded(&[message(&headers, json!({ "message": "slow down" }))]);
        let error = json!({ "type": "error",
            "error": { "type": "throttlingException", "message": "slow down" } });
        assert_eq!(sse, format!("event: error\ndata: {error}\n\n"));
    }

    #[test]
    fn an_answer_that_is_no_event_stream_is_read_as_it_came() {
        let mut head = http::Response::new(()).into_parts().0;
        head.headers
            .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        let sent = http::Request::new(()).into_parts().0;
        assert!(Bedrock.stream(&sent, &mut head).is_none());
    }
}

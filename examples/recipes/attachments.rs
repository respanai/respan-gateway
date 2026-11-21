//! Converting attachments before the model sees them: a PDF sent as an
//! OpenAI `file` part becomes text, but only in calls to a text-only model.
//! The conversion is a provider-edge filter, so it is per call: when the
//! text-only model fails, the fallback that reads PDFs gets the client's
//! original document, written in its own format (OpenAI's `file` part,
//! Anthropic's `document` block).
//!
//! The demo converts in process with a stand-in that reads the text of a
//! simple uncompressed PDF, so it runs anywhere. For real PDFs, set
//! `PDFTOTEXT=1` to run Poppler's `pdftotext` in a subprocess instead (see
//! [`pdftotext`]): untrusted bytes are parsed outside the gateway, and the
//! process is killed on timeout or cancellation. Input and output are
//! bounded, and every conversion of a request shares one time budget.
//!
//! Run it with `cargo run -p respan-gateway-examples --example attachments`.
//! It asks for a summary of a one-page PDF twice, with the fallback model
//! in OpenAI's format and then in Anthropic's. The text-only model fails
//! both times; look at what each model was sent: text for the first, the
//! PDF itself for the second.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mockllm::{Answer, FakeAnthropic, FakeOpenAi};
use respan_gateway_conversation::{Content, Input, MediaSource, Output};
use respan_gateway_conversation_http::anthropic::AnthropicMessages;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{
    ClientContext, ErrorCode, Fault, Gateway, Plugin, Provider, ProviderContext, Response,
    async_trait,
};
use respan_gateway_http::Body;
use respan_gateway_http::http::{HeaderMap, HeaderValue, Method, Request, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http_faults::{Status, retryable};
use respan_gateway_http_transport::HttpTransport;
use respan_gateway_plugin_fallback::{Backoff, Until, first_success};
use serde_json::{Value, json};
use tokio::time::Instant;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    for reader in [Reader::OpenAi, Reader::Anthropic] {
        let text_only = FakeOpenAi::start().await;
        text_only.answer(Answer::error(503).message("unavailable"));
        let (openai, anthropic) = (FakeOpenAi::start().await, FakeAnthropic::start().await);
        openai.answer(Answer::text("Revenue is up."));
        anthropic.answer(Answer::text("Revenue is up."));
        let reads_pdf = match reader {
            Reader::OpenAi => reader.provider(&openai.base_url())?,
            Reader::Anthropic => reader.provider(&anthropic.base_url())?,
        };
        let gw = Gateway::new()
            .provider(Reader::OpenAi.named("text-only", &text_only.base_url())?)?
            .provider(reads_pdf)?
            .client::<Input, _, _>(OpenAiChat)?
            .plugin(PdfToText::new(&["text-only"]))
            .plugin(Fallback);

        let (status, answer) = ask(&gw).await;
        let format = match reader {
            Reader::OpenAi => "OpenAI",
            Reader::Anthropic => "Anthropic",
        };
        println!("with the PDF reader in {format}'s format:");
        for (name, sent) in [("text-only", text_only.requests())]
            .into_iter()
            .chain([("reads-pdf", openai.requests())])
            .chain([("reads-pdf", anthropic.requests())])
        {
            for sent in sent {
                let body: Value = serde_json::from_slice(&sent.raw).unwrap_or_default();
                println!("  {name} was sent: {}", parts(&body));
            }
        }
        println!("  client got {status}: {answer}\n");
    }
    Ok(())
}

/// Converts PDFs to text in calls to the `text_only` providers.
struct PdfToText {
    text_only: &'static [&'static str],
    /// How long all of a request's conversions may take together.
    budget: Duration,
    /// The largest PDF read, and the most text kept of one.
    max: usize,
}

/// When the request's conversion budget runs out.
#[derive(Clone, Copy)]
struct Deadline(Instant);

impl PdfToText {
    fn new(text_only: &'static [&'static str]) -> Self {
        let (budget, max) = (Duration::from_secs(10), 50 << 20);
        Self {
            text_only,
            budget,
            max,
        }
    }

    async fn convert(&self, pdf: Vec<u8>, until: Instant) -> Result<String, Fault> {
        if pdf.len() > self.max {
            return Err(refusal(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                "PDF too large",
            ));
        }
        if !pdf.starts_with(b"%PDF-") {
            return Err(Fault::bad_request("not a PDF"));
        }
        let work = async {
            match std::env::var_os("PDFTOTEXT") {
                Some(_) => pdftotext(&pdf, self.max).await,
                None => Ok(stand_in(&pdf)),
            }
        };
        let slow = || {
            let message = "the PDF took too long to read";
            refusal(StatusCode::GATEWAY_TIMEOUT, "conversion_timeout", message)
        };
        tokio::time::timeout_at(until, work)
            .await
            .map_err(|_| slow())?
    }
}

/// The bytes of `part` if it is a PDF.
fn pdf(part: &Content) -> Option<&[u8]> {
    match part {
        Content::Document(media) if media.mime_type == "application/pdf" => match &media.source {
            MediaSource::Bytes(bytes) => Some(bytes),
            MediaSource::Url(_) => None,
        },
        _ => None,
    }
}

#[async_trait]
impl Plugin for PdfToText {
    /// One budget for the request, across its calls.
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        cx.extensions_mut()
            .insert(Deadline(Instant::now() + self.budget));
        Ok(None)
    }

    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        if !self.text_only.contains(&px.provider()) || !px.is_kind::<Input>() {
            return Ok(());
        }
        let until = px.extensions().get::<Deadline>().map(|d| d.0);
        let until = until.unwrap_or_else(|| Instant::now() + self.budget);
        let chat = px.input::<Input>()?;
        let pdfs: Vec<Vec<u8>> = (chat.messages.iter().flat_map(|m| &m.content))
            .filter_map(|part| pdf(part).map(<[u8]>::to_vec))
            .collect();
        // Filters are synchronous: convert here, then swap the parts in.
        let mut texts = Vec::with_capacity(pdfs.len());
        for bytes in pdfs {
            texts.push(self.convert(bytes, until).await?);
        }
        let mut texts = texts.into_iter();
        px.filter(move |chat: &mut Input| {
            for part in chat.messages.iter_mut().flat_map(|m| &mut m.content) {
                if pdf(part).is_some()
                    && let Some(text) = texts.next()
                {
                    *part = Content::from(format!("<document>\n{text}</document>"));
                }
            }
            Ok(())
        })
    }
}

/// A stand-in converter for the demo: the strings a simple, uncompressed
/// PDF shows (`(...) Tj`), one line each. Real PDFs need [`pdftotext`].
fn stand_in(pdf: &[u8]) -> String {
    let pdf = String::from_utf8_lossy(pdf);
    let shown = pdf
        .split(") Tj")
        .filter_map(|s| Some(s.rsplit_once('(')?.1));
    shown.map(|s| format!("{s}\n")).collect()
}

/// The real converter: Poppler's `pdftotext`, outside the gateway process,
/// killed when dropped (on timeout or cancellation), its text bounded by
/// `limit`.
async fn pdftotext(pdf: &[u8], limit: usize) -> Result<String, Fault> {
    use std::process::Stdio;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut child = tokio::process::Command::new("pdftotext")
        .args(["-enc", "UTF-8", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| Fault::config("install Poppler's pdftotext").with_source(e))?;
    let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
    let unreadable = |e| Fault::bad_request("the PDF could not be read").with_source(e);
    // Dropping stdin when written closes it, so the converter sees the end.
    let write = async move {
        let mut stdin = stdin.ok_or_else(|| Fault::internal("no stdin"))?;
        stdin.write_all(pdf).await.map_err(unreadable)
    };
    let read = async {
        let mut text = Vec::new();
        let stdout = stdout.ok_or_else(|| Fault::internal("no stdout"))?;
        let mut stdout = stdout.take(limit as u64 + 1);
        stdout.read_to_end(&mut text).await.map_err(unreadable)?;
        if text.len() > limit {
            return Err(refusal(
                StatusCode::PAYLOAD_TOO_LARGE,
                "too_large",
                "PDF text too large",
            ));
        }
        String::from_utf8(text).map_err(|_| Fault::bad_request("the PDF text is not UTF-8"))
    };
    let (_, text) = tokio::try_join!(write, read)?;
    let done = child.wait().await.map_err(unreadable)?;
    match done.success() {
        true => Ok(text),
        false => Err(Fault::bad_request("the PDF is malformed or encrypted")),
    }
}

fn refusal(status: StatusCode, code: &'static str, message: &str) -> Fault {
    Fault::new(message.to_owned())
        .with(ErrorCode(code.into()))
        .with(Status(status))
}

/// The text-only model, then the one that reads PDFs.
struct Fallback;

#[async_trait]
impl Plugin for Fallback {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let calls = ["text-only", "reads-pdf"].map(|p| cx.call(p));
        let until = Until::content(Output::is_content);
        first_success(calls, &Backoff::once(), until, retryable)
            .await
            .map(Some)
    }
}

/// The format the model that reads PDFs speaks.
#[derive(Clone, Copy)]
enum Reader {
    OpenAi,
    Anthropic,
}

impl Reader {
    fn provider(self, base_url: &str) -> Result<Provider, Fault> {
        self.named("reads-pdf", base_url)
    }

    /// A provider `name` at `base_url`, with a test key.
    fn named(self, name: &str, base_url: &str) -> Result<Provider, Fault> {
        let mut key = HeaderMap::new();
        let transport =
            |key| Ok::<_, Fault>(HttpTransport::new(base_url)?.with_headers(key)?.connect());
        Ok(match self {
            Reader::OpenAi => {
                key.insert("authorization", HeaderValue::from_static("Bearer sk-test"));
                Provider::new(name, OpenAiChat, transport(key)?)
            }
            Reader::Anthropic => {
                key.insert("x-api-key", HeaderValue::from_static("sk-ant-test"));
                Provider::new(name, AnthropicMessages, transport(key)?)
            }
        })
    }
}

/// Ask for a summary of a PDF; the status and the answer's text.
async fn ask(gw: &Gateway) -> (StatusCode, String) {
    let file = json!({ "filename": "q3.pdf",
        "file_data": format!("data:application/pdf;base64,{}", STANDARD.encode(report_pdf())) });
    let chat = json!({ "model": "gpt", "messages": [{ "role": "user", "content": [
        { "type": "text", "text": "Summarise this." },
        { "type": "file", "file": file }] }] });
    let mut req = Request::new(Body::full(chat.to_string()));
    *req.method_mut() = Method::POST;
    *req.uri_mut() = "/v1/chat/completions".parse().expect("a path");
    let (resp, _) = respan_gateway_http::handle(gw, req, 1 << 20).await;
    let status = resp.status();
    let body = resp.into_body().collect().await.map(|b| b.to_bytes());
    let body: Value = serde_json::from_slice(&body.unwrap_or_default()).unwrap_or_default();
    let text = body["choices"][0]["message"]["content"].as_str();
    (status, text.unwrap_or("(none)").to_owned())
}

/// The parts of the first message a model was sent, in short.
fn parts(body: &Value) -> String {
    let content = body["messages"][0]["content"].as_array();
    let short = |part: &Value| match part["type"].as_str().unwrap_or_default() {
        "text" => format!("text {:?}", part["text"].as_str().unwrap_or_default()),
        other => format!("{other} (a PDF)"),
    };
    let parts: Vec<String> = content.into_iter().flatten().map(short).collect();
    parts.join(", ")
}

/// A one-page PDF saying "Quarterly revenue is up.".
fn report_pdf() -> Vec<u8> {
    let text = "BT /F1 12 Tf 10 100 Td (Quarterly revenue is up.) Tj ET";
    let objects = [
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_owned(),
        "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 300 200] \
         /Resources << /Font << /F1 4 0 R >> >> /Contents 5 0 R >>"
            .to_owned(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
        format!("<< /Length {} >>\nstream\n{text}\nendstream", text.len()),
    ];
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
    }
    let (xref, size) = (pdf.len(), objects.len() + 1);
    pdf.push_str(&format!("xref\n0 {size}\n0000000000 65535 f \n"));
    for offset in offsets {
        pdf.push_str(&format!("{offset:010} 00000 n \n"));
    }
    pdf.push_str(&format!(
        "trailer\n<< /Size {size} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n"
    ));
    pdf.into_bytes()
}

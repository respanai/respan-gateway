//! Groq: Chat Completions, transcriptions and speech.

use respan_gateway_conversation::{Input, Output, Usage};
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::openai::{self, OpenAiChat};
use respan_gateway_core::{Filter, Native, Result};
use serde_json::{Map, Value, json};

use respan_gateway_media_http::openai::{OpenAiSpeech, OpenAiTranscriptions};

use respan_gateway_vendor_kit::{Dialect, Dialected, Vendor};

/// Where its API is.
const BASE_URL: &str = "https://api.groq.com/openai/v1";

/// Groq.
pub const VENDOR: Vendor = Vendor {
    id: "groq",
    key_env: &["GROQ_API_KEY"],
    formats: &[
        (&Dialected::new(OpenAiChat, Groq), Some(BASE_URL)),
        (&OpenAiTranscriptions, Some(BASE_URL)),
        (&OpenAiSpeech, Some(BASE_URL)),
    ],
    ..Vendor::DEFAULT
};

/// Groq's Chat Completions: the usage under `x_groq` is the response's.
#[derive(Debug, Clone, Copy)]
pub struct Groq;

/// For a client of any format, Chat Completions' too: the usage counts for
/// each.
impl Dialect for Groq {
    type Input = Input;
    type Output = Output;

    fn outputs(&self) -> Option<Box<dyn Filter<Output>>> {
        Some(Box::new(XGroqUsage::default()))
    }
}

/// Takes the usage out of `x_groq`, in the start's fields or a later
/// chunk's, and reports what it adds to the usage reported so far, once a
/// chunk's own `usage` has passed: as `usage` counts when both say it.
#[derive(Default)]
struct XGroqUsage {
    reported: Usage,
    /// The usage `x_groq` said last, until the chunk's own has passed.
    said: Option<Value>,
}

impl XGroqUsage {
    fn report(&mut self, out: &mut Vec<Output>) -> Result<()> {
        let Some(said) = self.said.take() else {
            return Ok(());
        };
        let (total, extra) = openai::read_usage(&said)?;
        let more = total.saturating_sub(&self.reported);
        if !more.is_empty() {
            self.reported += &more;
            out.push(Output::Usage(more));
        }
        if !extra.is_empty() {
            let usage = |f: &mut Map<String, Value>| _ = f.insert("usage".into(), json!(extra));
            out.push(Output::Native(OpenAiChat::with_fields(
                &Native::default(),
                usage,
            )));
        }
        Ok(())
    }
}

impl Filter<Output> for XGroqUsage {
    fn edit(&mut self, output: Output, out: &mut Vec<Output>) -> Result<()> {
        let output = match output {
            Output::Usage(usage) => {
                self.reported += &usage;
                out.push(Output::Usage(usage));
                return Ok(());
            }
            Output::Start(mut start) => {
                self.report(out)?;
                if !OpenAiChat::fields(&start.native).is_empty() {
                    let said = &mut self.said;
                    start.native =
                        OpenAiChat::with_fields(&start.native, |f| *said = take_usage(f));
                }
                // The chunk's own usage comes after its start.
                out.push(Output::Start(start));
                return Ok(());
            }
            Output::Native(native) if !OpenAiChat::fields(&native).is_empty() => {
                self.report(out)?;
                let mut fields = OpenAiChat::fields(&native).clone();
                let response = fields.get_mut("response").and_then(Value::as_object_mut);
                if let Some(response) = response {
                    self.said = take_usage(response);
                    if response.is_empty() {
                        return self.report(out);
                    }
                }
                Output::Native(OpenAiChat::with_fields(&native, |f| *f = fields))
            }
            output => output,
        };
        self.report(out)?;
        out.push(output);
        Ok(())
    }

    fn end(&mut self, end: &Result<()>, out: &mut Vec<Output>) -> Result<()> {
        match end {
            Ok(()) => self.report(out),
            Err(_) => Ok(()),
        }
    }
}

/// The usage under `x_groq` in `fields`, taken out; `x_groq` goes too once
/// it has nothing else.
fn take_usage(fields: &mut Map<String, Value>) -> Option<Value> {
    let x_groq = fields.get_mut("x_groq")?.as_object_mut()?;
    let usage = x_groq.shift_remove("usage")?;
    if x_groq.is_empty() {
        fields.shift_remove("x_groq");
    }
    Some(usage)
}

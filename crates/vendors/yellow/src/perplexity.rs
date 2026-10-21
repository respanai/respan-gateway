//! Perplexity: its Agent API, as OpenAI's Responses, in its dialect.

use respan_gateway_conversation::Output;
use respan_gateway_conversation_http::Schema;
use respan_gateway_conversation_http::dialect::{Call, Dialect, Dialected};
use respan_gateway_conversation_http::responses::OpenAiResponses;
use respan_gateway_core::{Filter, NativeValue, Result};
use serde_json::Value;

use respan_gateway_vendor_kit::Vendor;

/// Where its API is.
const BASE_URL: &str = "https://api.perplexity.ai/v1";

/// Perplexity. Its models' names are `<vendor>/<model>`: a configured
/// provider named `<vendor>` takes the name first.
pub const VENDOR: Vendor = Vendor {
    id: "perplexity",
    key_env: &["PERPLEXITY_API_KEY"],
    formats: &[(&Dialected::new(OpenAiResponses, Perplexity), Some(BASE_URL))],
    ..Vendor::DEFAULT
};

/// The output items only Perplexity sends: what the tools it runs itself
/// found.
const RESULTS: &[&str] = &[
    "search_results",
    "image_search_results",
    "fetch_url_results",
    "finance_results",
    "people_search_results",
    "sandbox_results",
    "tool_search_output",
];

/// Perplexity's Responses: what the tools it runs itself found, as output
/// items of their own, and a stream's `response.reasoning.*` and
/// `response.skill.loaded` events only describe the answer.
#[derive(Debug, Clone, Copy)]
pub struct Perplexity;

/// For a client of another format, which would refuse them, they are
/// dropped; a client of Responses gets them as they came.
impl Dialect for Perplexity {
    fn output(&self, call: &Call) -> Option<Box<dyn Filter<Output>>> {
        call.translated
            .then(|| Box::new(Described::default()) as Box<dyn Filter<Output>>)
    }
}

/// Drops the events that only describe the answer.
#[derive(Default)]
struct Described {
    /// The output indexes of the result items dropped, whose later events
    /// go too.
    results: Vec<u64>,
}

impl Described {
    /// Whether `v`, a native event of Responses, is Perplexity's alone: its
    /// `response.reasoning.*` (not the spec's `response.reasoning_text.*`
    /// or `response.reasoning_summary_*`), `response.skill.loaded`, or an
    /// event of a result item.
    fn describes(&mut self, v: &Value) -> bool {
        let kind = v["type"].as_str().unwrap_or_default();
        if kind.starts_with("response.reasoning.") || kind == "response.skill.loaded" {
            return true;
        }
        let index = v["output_index"].as_u64();
        let item = v["item"]["type"].as_str();
        if item.is_some_and(|t| RESULTS.contains(&t)) {
            self.results.extend(index);
            return true;
        }
        index.is_some_and(|i| self.results.contains(&i))
    }
}

impl Filter<Output> for Described {
    fn edit(&mut self, output: Output, out: &mut Vec<Output>) -> Result<()> {
        if let Output::Native(NativeValue { format, value }) = &output
            && &**format == OpenAiResponses::NAME
            && self.describes(value)
        {
            return Ok(());
        }
        out.push(output);
        Ok(())
    }
}

//! The plugin that fills in what the gateway knows of a request, and the
//! sink that records a tool loop's steps.

use respan_gateway_core::FaultKind;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use respan_gateway_conversation as conversation;
use respan_gateway_core::{ClientContext, Fault, Plugin, ProviderContext, async_trait};
use respan_gateway_plugin_fallback::Attempt;
use respan_gateway_plugin_routing::Model;
use respan_gateway_plugin_tool_loop::{Sink, StepReport, ToolHop};

use super::count;
use super::log::{RequestLog, Tracked};
use super::record::{Step, ToolCall, clip};
use crate::formats::KnownFormat;

/// The plugin that fills in what the gateway knows of a request.
pub struct Recorder {
    log: Arc<RequestLog>,
    /// Each format's kind, and how its requests name their model.
    formats: HashMap<String, (&'static str, Model)>,
    /// Each core provider's configured name.
    providers: HashMap<String, String>,
}

impl std::fmt::Debug for Recorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder").finish_non_exhaustive()
    }
}

impl Recorder {
    pub fn new(
        log: Arc<RequestLog>,
        formats: &[KnownFormat],
        providers: HashMap<String, String>,
    ) -> Self {
        let formats = formats
            .iter()
            .map(|f| (f.name.clone(), (f.kind, f.model.clone())))
            .collect();
        Self {
            log,
            formats,
            providers,
        }
    }
}

#[async_trait]
impl Plugin for Recorder {
    async fn on_client(
        &self,
        cx: &mut ClientContext,
    ) -> Result<Option<respan_gateway_core::Response>, Fault> {
        let Some(Tracked(entry)) = cx.extensions().get::<Tracked>().cloned() else {
            return Ok(None);
        };
        entry.recorded.store(true, Ordering::SeqCst);
        {
            let mut record = entry.record();
            record.format = Some(cx.format().to_owned());
            if let Some((kind, model)) = self.formats.get(cx.format()) {
                record.kind = Some((*kind).to_owned());
                let model = model.read(cx).map(|m| clip(&m));
                record.model = model.or(record.model.take());
            }
        }
        self.log.publish(&entry);
        count::all(cx, &entry)?;
        Ok(None)
    }

    /// A session ends here, not at its `101`: its duration is the
    /// session's, and its usage the sum of its responses'.
    async fn on_client_end(&self, cx: &ClientContext, end: &Result<(), Fault>) {
        let Some(Tracked(entry)) = cx.extensions().get::<Tracked>() else {
            return;
        };
        let live = {
            let record = entry.record();
            entry.ended.store(true, Ordering::SeqCst);
            record.status == Some(101)
        };
        if live {
            let error = match end {
                Err(fault) if !fault.is(FaultKind::Abandoned) => Some(clip(&fault.to_string())),
                _ => None,
            };
            self.log.finish(entry, error);
        }
    }

    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        // A tool's server is not the request's provider.
        if px.extensions().get::<ToolHop>().is_some() {
            return Ok(());
        }
        if let Some(Tracked(entry)) = px.extensions().get::<Tracked>() {
            let provider = self.providers.get(px.provider()).cloned();
            let provider = provider.unwrap_or_else(|| px.provider().to_owned());
            // Named as sent; once a fallback or race returns, its winner.
            if let Some(attempt) = px.extensions().get::<Attempt>() {
                entry.attempt(attempt.clone(), provider.clone());
            }
            entry.record().provider = Some(provider);
        }
        Ok(())
    }
}

/// Where the tool loop reports a request's steps: into its entry in `log`.
pub fn steps(log: Arc<RequestLog>) -> impl Fn(&ClientContext) -> Option<Sink> + Send + Sync {
    move |cx| {
        let Tracked(entry) = cx.extensions().get::<Tracked>().cloned()?;
        let log = log.clone();
        Some(Arc::new(move |report: StepReport| {
            entry.record().steps.push(step(&report));
            log.publish(&entry);
        }))
    }
}

fn step(report: &StepReport) -> Step {
    let millis = |d: std::time::Duration| d.as_millis().try_into().unwrap_or(u64::MAX);
    let finish = report.finish.as_ref().map(|f| match f {
        conversation::Finish::Stop => "stop".to_owned(),
        conversation::Finish::Length => "length".to_owned(),
        conversation::Finish::ToolUse => "tool_use".to_owned(),
        conversation::Finish::Filtered => "filtered".to_owned(),
        _ => "other".to_owned(),
    });
    Step {
        index: report.index,
        duration_ms: millis(report.took),
        input_tokens: report.usage.input_tokens,
        output_tokens: report.usage.output_tokens,
        finish,
        tools: (report.tools.iter())
            .map(|t| ToolCall {
                name: clip(&t.name),
                duration_ms: millis(t.took),
                error: t.is_error,
                bytes: t.bytes,
            })
            .collect(),
    }
}

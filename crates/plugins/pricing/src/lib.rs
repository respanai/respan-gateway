#![warn(missing_docs)]
//! `Pricing`: what each call to a provider cost, charged as it reports its
//! usage.
//!
//! A call is priced at what its provider charges for the model it sent: a
//! route's pick, each step of a tool loop, a retry and a race's loser are
//! each a call of their own, so a request costs the sum of its calls. Each
//! usage report a call's provider sends is charged as it passes, before the
//! client hears it, to the host's [`on_charge`](Pricing::on_charge) with the
//! call's extensions, which tell it whose request it was.
//!
//! A call's price is its model's, in the host's [`Catalog`]. It measures;
//! what a host does with a charge, such as a budget, is its own.

use std::sync::Arc;

use respan_gateway_conversation as conversation;
use std::any::Any;
use std::collections::HashMap;

use conversation::Usage;
use respan_gateway_core::{
    ClientContext, Extensions, Fault, Filter, Plugin, ProviderContext, async_trait,
};
use respan_gateway_decision as decision;
use respan_gateway_media::{embedding, image, rerank, speech, transcription, video};
use respan_gateway_realtime as realtime;
pub use respan_gateway_sdk::{Catalog, Price};

/// What one usage report of a call cost.
#[derive(Debug, Clone, PartialEq)]
pub struct Charge {
    /// The core provider called.
    pub provider: String,
    /// The model it was sent.
    pub model: String,
    /// The tokens the report says were used; none for seconds of audio.
    pub usage: Usage,
    /// What that cost, in US dollars.
    pub usd: f64,
}

type OnCharge = dyn Fn(&Extensions, &Charge) + Send + Sync;
type ModelOf = dyn Fn(&ProviderContext) -> Option<String> + Send + Sync;

/// The plugin: see the [module](self). Register it after the plugins that
/// choose a call's model, such as a router, so it reads the model sent.
pub struct Pricing {
    catalog: Arc<Catalog>,
    /// The provider of each core provider, as the catalog names it.
    providers: HashMap<String, String>,
    on_charge: Option<Arc<OnCharge>>,
    model_of: Option<Arc<ModelOf>>,
}

impl std::fmt::Debug for Pricing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pricing").finish_non_exhaustive()
    }
}

impl Pricing {
    /// Calls priced at their models' prices in `catalog`.
    pub fn new(catalog: Arc<Catalog>) -> Self {
        Self {
            catalog,
            providers: HashMap::new(),
            on_charge: None,
            model_of: None,
        }
    }

    /// Reads each core provider (by the name a call has) as its provider in
    /// the catalog, in `providers`; a core provider it does not name is its
    /// own.
    #[must_use]
    pub fn provider_names(mut self, providers: HashMap<String, String>) -> Self {
        self.providers = providers;
        self
    }

    /// Each charge given to `on_charge`, with its call's extensions.
    #[must_use]
    pub fn on_charge(
        mut self,
        on_charge: impl Fn(&Extensions, &Charge) + Send + Sync + 'static,
    ) -> Self {
        self.on_charge = Some(Arc::new(on_charge));
        self
    }

    /// How a call's model is read when its input names none, as a live
    /// session's, whose model its handshake named.
    #[must_use]
    pub fn model_of(
        mut self,
        model_of: impl Fn(&ProviderContext) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.model_of = Some(Arc::new(model_of));
        self
    }
}

#[async_trait]
impl Plugin for Pricing {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        let Some(on_charge) = self.on_charge.clone() else {
            return Ok(());
        };
        let model = model_sent(px).or_else(|| self.model_of.as_ref().and_then(|f| f(px)));
        let Some(model) = model else {
            return Ok(());
        };
        let provider = px.provider().to_owned();
        let named = self.providers.get(&provider).unwrap_or(&provider);
        let Some(price) = self.catalog.get(named, &model).and_then(|m| m.price) else {
            return Ok(());
        };
        let extensions = px.extensions().clone();
        on_usage(
            px,
            Arc::new(move |used| {
                let (usage, usd) = match used {
                    Used::Usage(usage) => (usage, charge(&price, &usage)),
                    Used::Seconds(seconds) => match price.per_second {
                        Some(rate) => (Usage::default(), seconds * rate),
                        None => return,
                    },
                };
                let charge = Charge {
                    provider: provider.clone(),
                    model: model.clone(),
                    usage,
                    usd,
                };
                on_charge(&extensions, &charge);
            }),
        )
    }
}

/// What one report of a response says it used.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Used {
    /// Tokens, as any kind reports them.
    Usage(Usage),
    /// Seconds of audio, as a transcription's provider may bill.
    Seconds(f64),
}

/// An edge whose outputs can be read as they pass: a request's, as its
/// client gets them, or one call's, as its provider sends them.
pub trait Edge {
    /// Whether its outputs are `T`.
    fn outputs_are<T: Any>(&self) -> bool;
    /// Runs `filter` over its outputs of `T`.
    fn read<T: Clone + Send + Sync + 'static>(
        &mut self,
        filter: impl Filter<T>,
    ) -> Result<(), Fault>;
}

impl Edge for ClientContext {
    fn outputs_are<T: Any>(&self) -> bool {
        self.is_kind::<T>()
    }
    fn read<T: Clone + Send + Sync + 'static>(
        &mut self,
        filter: impl Filter<T>,
    ) -> Result<(), Fault> {
        self.filter(filter)
    }
}

impl Edge for ProviderContext {
    fn outputs_are<T: Any>(&self) -> bool {
        self.is_kind::<T>()
    }
    fn read<T: Clone + Send + Sync + 'static>(
        &mut self,
        filter: impl Filter<T>,
    ) -> Result<(), Fault> {
        self.filter(filter)
    }
}

/// Gives `into` each usage report of `edge`'s outputs, of every kind, as it
/// passes.
pub fn on_usage(edge: &mut impl Edge, into: Arc<dyn Fn(Used) + Send + Sync>) -> Result<(), Fault> {
    watch(edge, &into, |o: &conversation::Output| match o {
        conversation::Output::Usage(u) => used(u),
        _ => None,
    })?;
    watch(edge, &into, |o: &image::Output| match o {
        image::Output::Usage(u) => used(u),
        _ => None,
    })?;
    watch(edge, &into, |o: &transcription::Output| match o {
        transcription::Output::Usage(u) => used(u),
        transcription::Output::Seconds(seconds) => Some(Used::Seconds(*seconds)),
        _ => None,
    })?;
    watch(edge, &into, |o: &speech::Output| match o {
        speech::Output::Usage(u) => used(u),
        _ => None,
    })?;
    watch(edge, &into, |o: &embedding::Output| match o {
        embedding::Output::Usage(u) => used(u),
        _ => None,
    })?;
    watch(edge, &into, |o: &rerank::Output| match o {
        rerank::Output::Usage(u) => used(u),
        _ => None,
    })?;
    watch(edge, &into, |o: &realtime::Output| match o {
        realtime::Output::Usage(u) => used(&u.tokens),
        _ => None,
    })?;
    watch(edge, &into, |o: &decision::Output| match o {
        decision::Output::Usage(u) => used(u),
        _ => None,
    })
}

fn used(usage: &Usage) -> Option<Used> {
    Some(Used::Usage(*usage))
}

/// What `usage` cost at `price`, in US dollars.
pub fn charge(price: &Price, usage: &Usage) -> f64 {
    let cached = usage.cached_input_tokens.min(usage.input_tokens);
    let written = (usage.cache_write_input_tokens).min(usage.input_tokens - cached);
    let plain = usage.input_tokens - cached - written;
    let dollars = plain as f64 * price.input
        + cached as f64 * price.cached_input.unwrap_or(price.input)
        + written as f64 * price.cache_write.unwrap_or(price.input)
        + usage.output_tokens as f64 * price.output;
    dollars / 1e6
}

/// Gives `into` each report `read` finds in `edge`'s outputs, if they are `T`.
fn watch<T: Clone + Send + Sync + 'static>(
    edge: &mut impl Edge,
    into: &Arc<dyn Fn(Used) + Send + Sync>,
    read: fn(&T) -> Option<Used>,
) -> Result<(), Fault> {
    if !edge.outputs_are::<T>() {
        return Ok(());
    }
    let into = into.clone();
    edge.read(move |output: &mut T| {
        if let Some(used) = read(output) {
            into(used);
        }
        Ok(())
    })
}

/// The model a call sends, by its kind's input; none for a live session's.
pub fn model_sent(px: &ProviderContext) -> Option<String> {
    fn of<T: 'static>(px: &ProviderContext, model: fn(&T) -> Option<&str>) -> Option<String> {
        px.input::<T>().ok().and_then(model).map(str::to_owned)
    }
    of(px, conversation::Input::model)
        .or_else(|| of(px, image::Input::model))
        .or_else(|| of(px, transcription::Input::model))
        .or_else(|| of(px, speech::Input::model))
        .or_else(|| of(px, embedding::Input::model))
        .or_else(|| of(px, rerank::Input::model))
        .or_else(|| of(px, video::Input::model))
        .or_else(|| of(px, decision::Input::model))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_input_costs_its_own_rate() {
        let price = Price::new(2.0, 8.0, Some(0.5));
        let usage = Usage {
            cached_input_tokens: 400_000,
            ..Usage::new(1_000_000, 500_000)
        };
        let cost = charge(&price, &usage);
        assert!((cost - (0.6 * 2.0 + 0.4 * 0.5 + 0.5 * 8.0)).abs() < 1e-9);
        let plain = Price {
            cached_input: None,
            ..price
        };
        assert!((charge(&plain, &usage) - 6.0).abs() < 1e-9);
    }

    /// Regression: tokens written to a cache, which Anthropic bills above
    /// the input rate, were charged at it.
    #[test]
    fn cache_writes_cost_their_own_rate() {
        let price = Price {
            cache_write: Some(2.5),
            ..Price::new(2.0, 8.0, Some(0.5))
        };
        let usage = Usage {
            cached_input_tokens: 200_000,
            cache_write_input_tokens: 300_000,
            ..Usage::new(1_000_000, 0)
        };
        let cost = charge(&price, &usage);
        assert!((cost - (0.5 * 2.0 + 0.2 * 0.5 + 0.3 * 2.5)).abs() < 1e-9);
    }
}

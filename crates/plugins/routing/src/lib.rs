#![warn(missing_docs)]
//! Which provider a request goes to. Of the providers serving its kind, a
//! request for a model goes, in order, to:
//!
//! 1. the candidates of a [`Rule`] named exactly that model;
//! 2. the provider its model names as `provider/model`, if configured,
//!    sent `model`;
//! 3. the candidates of the first glob [`Rule`] that matches, such as
//!    `llama-*`;
//! 4. the provider whose model listing names it (in the [`Catalog`]); of
//!    several, the first added;
//! 5. the kind's default: the target if it serves the kind, else the first
//!    provider that does.
//!
//! A rule orders its candidates by its [`Pick`]: as listed, cheapest,
//! fastest, by weight or at random, from what the providers' listings say
//! ([`Catalog`]) and what the router saw of its own calls ([`Stats`]).
//! [`Dispatch`] then sends the request to them as the rule says: the first,
//! the rest as fallback, or all raced.
//!
//! One provider is one core provider per format it speaks. A request goes
//! to its provider's core provider of the format the provider sends its
//! model in ([`Routes::format_for`]), else of the client's own format, else
//! its first of the kind: the formats of a kind read and write the same
//! neutral types, so any of them can answer a client of another.

mod rules;
mod stats;

use std::collections::HashMap;
use std::sync::Arc;

use respan_gateway_core::{
    ClientContext, Fault, FaultKind, Plugin, ProviderContext, Response, async_trait,
};
use respan_gateway_plugin_fallback::{Attempt, first_success, race};

use respan_gateway_sdk::Model as Card;
pub use respan_gateway_sdk::{Ask, Catalog, FormatFor};
pub use rules::{Candidate, Pick, Rule, glob};
pub use stats::{Seen, Stats};

/// A format a provider speaks, and its core provider of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Speaks {
    /// The format's name.
    pub format: String,
    /// The format's kind, such as `chat`.
    pub kind: &'static str,
    /// The core provider registered for it.
    pub internal: String,
}

/// Who serves what, and the routes to them.
#[derive(Default)]
pub struct Routes {
    /// Each provider and its formats, in the order added.
    providers: Vec<(String, Vec<Speaks>)>,
    /// The format a provider sends a request for a model in, if it says.
    formats_for: HashMap<String, FormatFor>,
    /// The rules, in the order added.
    rules: Vec<Arc<Rule>>,
    target: Option<String>,
    stats: Stats,
    catalog: Arc<Catalog>,
}

impl std::fmt::Debug for Routes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Routes")
            .field("providers", &self.providers)
            .field("rules", &self.rules)
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// Why a request goes where it does.
#[derive(Debug, Clone)]
pub enum By {
    /// This rule.
    Rule(Arc<Rule>),
    /// Its model's `provider/` prefix.
    Named,
    /// The provider's model listing.
    Listed,
    /// The kind's default.
    Default,
}

/// A provider a request may go to. A call to it carries it as an
/// extension, so the router sends it its model and records how it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chosen {
    /// The provider, as added.
    pub provider: String,
    /// The core provider of the format it is sent in.
    pub internal: String,
    /// The model it is sent, if any: the one asked for, unless a route or a
    /// `provider/` prefix says another.
    pub model: Option<String>,
}

/// Where a request goes, in the order to try.
#[derive(Debug, Clone)]
pub struct Planned {
    /// Why.
    pub by: By,
    /// The providers to try, in order; never empty.
    pub to: Vec<Chosen>,
    /// The candidates passed over, and why.
    pub passed: Vec<(String, String)>,
}

impl Routes {
    /// No providers and no routes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds provider `name`, speaking each of `speaks`.
    pub fn provider(mut self, name: impl Into<String>, speaks: Vec<Speaks>) -> Self {
        self.providers.push((name.into(), speaks));
        self
    }

    /// Lets `provider` say which of its formats a request goes in, by the
    /// request and the card of the model it is sent; a format it does not
    /// speak of the request's kind is not taken.
    pub fn format_for(mut self, provider: impl Into<String>, format_for: FormatFor) -> Self {
        self.formats_for.insert(provider.into(), format_for);
        self
    }

    /// Adds `rule`, after those added before.
    pub fn rule(mut self, rule: Rule) -> Self {
        self.rules.push(Arc::new(rule));
        self
    }

    /// Makes `provider` the default of each kind it serves.
    pub fn target(mut self, provider: impl Into<String>) -> Self {
        self.target = Some(provider.into());
        self
    }

    /// Reads what models can do and cost from `catalog`.
    pub fn with_catalog(mut self, catalog: Arc<Catalog>) -> Self {
        self.catalog = catalog;
        self
    }

    /// What it knows of models.
    pub fn catalog(&self) -> &Arc<Catalog> {
        &self.catalog
    }

    /// What the router saw of its calls.
    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Whether `provider` speaks `format`.
    pub fn speaks(&self, provider: &str, format: &str) -> bool {
        let speaks = self.providers.iter().find(|(name, _)| name == provider);
        speaks.is_some_and(|(_, speaks)| speaks.iter().any(|s| s.format == format))
    }

    /// The rules, in the order tried among their sort.
    pub fn rules(&self) -> &[Arc<Rule>] {
        &self.rules
    }

    /// Where a request of `kind`, from a client of `format`, asking for
    /// `model`, goes first; `None` if no provider serves it.
    pub fn route(&self, kind: &str, format: &str, model: Option<&str>) -> Option<Chosen> {
        let ask = Ask {
            kind,
            format,
            model,
            client: None,
        };
        self.plan(&ask)?.to.into_iter().next()
    }

    /// Where `ask` goes, in the order to try; `None` if no provider serves it.
    pub fn plan(&self, ask: &Ask) -> Option<Planned> {
        let mut passed = Vec::new();
        let planned = self.plan_passing(ask, &mut passed)?;
        Some(Planned { passed, ..planned })
    }

    fn plan_passing(&self, ask: &Ask, passed: &mut Vec<(String, String)>) -> Option<Planned> {
        let one = |by: By, provider: &str, model: Option<&str>| {
            let model = model.or(ask.model);
            let card = model.map(|m| self.card(provider, m));
            let internal = self.internal(provider, ask, card.as_ref())?;
            Some(Planned {
                by,
                to: vec![Chosen {
                    provider: provider.to_owned(),
                    internal: internal.to_owned(),
                    model: model.map(str::to_owned),
                }],
                passed: Vec::new(),
            })
        };
        if let Some(model) = ask.model {
            let exact = self
                .rules
                .iter()
                .filter(|r| !r.is_glob() && r.name == model);
            for rule in exact {
                if let Some(planned) = self.by_rule(rule, ask, passed) {
                    return Some(planned);
                }
            }
            let named = model.split_once('/');
            let named = named.and_then(|(p, rest)| one(By::Named, p, Some(rest)));
            if named.is_some() {
                return named;
            }
            let globs = self
                .rules
                .iter()
                .filter(|r| r.is_glob() && r.matches(model));
            for rule in globs {
                if let Some(planned) = self.by_rule(rule, ask, passed) {
                    return Some(planned);
                }
            }
            let listed = self.lister(ask, model);
            if let Some(planned) = listed.and_then(|p| one(By::Listed, p, None)) {
                return Some(planned);
            }
        }
        self.target
            .iter()
            .chain(self.providers.iter().map(|(name, _)| name))
            .find_map(|provider| one(By::Default, provider, None))
    }

    /// `provider`'s card of `model`: its list's, else a bare one.
    fn card(&self, provider: &str, model: &str) -> Card {
        (self.catalog.get(provider, model)).unwrap_or_else(|| Card::new(model))
    }

    /// The first provider whose listing names `model` and that serves
    /// `ask`'s kind.
    fn lister(&self, ask: &Ask, model: &str) -> Option<&str> {
        (self.providers.iter())
            .map(|(name, _)| name.as_str())
            .find(|name| {
                let card = self.catalog.get(name, model);
                card.is_some_and(|card| self.internal(name, ask, Some(&card)).is_some())
            })
    }

    /// Where `rule` sends `ask`, if it applies and a candidate can take it.
    fn by_rule(
        &self,
        rule: &Arc<Rule>,
        ask: &Ask,
        passed: &mut Vec<(String, String)>,
    ) -> Option<Planned> {
        let mut pass = |candidate: String, why: String| {
            passed.push((format!("{} → {candidate}", rule.name), why));
        };
        if !rule.applies(ask.kind, ask.client) {
            pass("*".to_owned(), "not for this request".to_owned());
            return None;
        }
        let mut usable = Vec::new();
        for candidate in &rule.to {
            let model = candidate.model_for(ask.model);
            let card = model.as_deref().map(|m| self.card(&candidate.provider, m));
            let Some(internal) = self.internal(&candidate.provider, ask, card.as_ref()) else {
                pass(candidate.to_string(), format!("serves no {}", ask.kind));
                continue;
            };
            let lacks = card.and_then(|m| rule.needs.iter().find(|n| m.lacks(n)).cloned());
            if let Some(need) = lacks {
                pass(candidate.to_string(), format!("lacks {need}"));
                continue;
            }
            let chosen = Chosen {
                provider: candidate.provider.clone(),
                internal: internal.to_owned(),
                model,
            };
            usable.push((candidate, chosen));
        }
        fn of<'a>((c, chosen): &'a (&Candidate, Chosen)) -> (&'a Candidate, &'a str) {
            (c, chosen.model.as_deref().unwrap_or_default())
        }
        let ordered = rules::order(rule.pick, usable, of, &self.stats, &self.catalog);
        let mut to: Vec<Chosen> = ordered.into_iter().map(|(_, chosen)| chosen).collect();
        if !rule.sends_many() {
            to.truncate(1);
        }
        (!to.is_empty()).then(|| Planned {
            by: By::Rule(rule.clone()),
            to,
            passed: Vec::new(),
        })
    }

    /// The core provider of `provider` that takes `ask`, sent the model of
    /// `card`: of the format it says for the model, else of the client's,
    /// else its first of the kind.
    fn internal(&self, provider: &str, ask: &Ask, card: Option<&Card>) -> Option<&str> {
        let (_, speaks) = self.providers.iter().find(|(name, _)| name == provider)?;
        let of_kind = || speaks.iter().filter(|s| s.kind == ask.kind);
        let said = (self.formats_for.get(provider).zip(card))
            .and_then(|(format_for, card)| format_for(ask, card))
            .and_then(|format| of_kind().find(|s| s.format == format));
        (said.or_else(|| of_kind().find(|s| s.format == ask.format)))
            .or_else(|| of_kind().next())
            .map(|s| s.internal.as_str())
    }

    /// The default provider of each kind, as `(kind, provider)`.
    pub fn defaults(&self) -> Vec<(&'static str, String)> {
        let mut firsts: Vec<&Speaks> = Vec::new();
        for speaks in self.providers.iter().flat_map(|(_, speaks)| speaks) {
            if firsts.iter().all(|s| s.kind != speaks.kind) {
                firsts.push(speaks);
            }
        }
        let default = |s: &Speaks| {
            let ask = Ask {
                kind: s.kind,
                format: &s.format,
                model: None,
                client: None,
            };
            Some((s.kind, self.plan(&ask)?.to.swap_remove(0).provider))
        };
        firsts.into_iter().filter_map(default).collect()
    }
}

type Read = dyn Fn(&ClientContext) -> Result<Option<String>, Fault> + Send + Sync;
type Write = dyn Fn(&mut ClientContext, String) -> Result<(), Fault> + Send + Sync;
type CallWrite = dyn Fn(&mut ProviderContext, String) -> Result<(), Fault> + Send + Sync;

/// How a format's requests name their model.
#[derive(Clone)]
pub struct Model {
    read: Arc<Read>,
    write: Arc<Write>,
    /// Rewrites one call's, so each candidate of a route gets its own.
    call: Option<Arc<CallWrite>>,
}

impl std::fmt::Debug for Model {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Model").finish_non_exhaustive()
    }
}

impl Model {
    /// The model of inputs of type `I`, read by `read` and rewritten by
    /// `write`, such as `Model::of(Input::model, Input::set_model)`. A
    /// request whose input is not an `I` is refused when routed, not
    /// routed to the default.
    pub fn of<I: Clone + Send + Sync + 'static>(
        read: fn(&I) -> Option<&str>,
        write: fn(&mut I, String),
    ) -> Self {
        Self {
            read: Arc::new(move |cx| Ok(read(cx.input::<I>()?).map(str::to_owned))),
            write: Arc::new(move |cx, model| {
                cx.filter(move |input: &mut I| {
                    write(input, model.clone());
                    Ok(())
                })
            }),
            call: Some(Arc::new(move |px, model| {
                // A provider of another input type keeps the client's model.
                if !px.is_kind::<I>() {
                    return Ok(());
                }
                px.filter(move |input: &mut I| {
                    write(input, model.clone());
                    Ok(())
                })
            })),
        }
    }

    /// The model `read` reads of a request, and `write` asks for, for a
    /// format whose inputs do not name it at once, such as a session that
    /// names it in its handshake.
    pub fn new(
        read: impl Fn(&ClientContext) -> Option<String> + Send + Sync + 'static,
        write: impl Fn(&mut ClientContext, String) -> Result<(), Fault> + Send + Sync + 'static,
    ) -> Self {
        Self {
            read: Arc::new(move |cx| Ok(read(cx))),
            write: Arc::new(write),
            call: None,
        }
    }

    /// For requests that name no model.
    pub fn none() -> Self {
        Self {
            read: Arc::new(|_| Ok(None)),
            write: Arc::new(|_, _| Ok(())),
            call: None,
        }
    }

    /// The model `cx`'s request asks for, if any.
    pub fn read(&self, cx: &ClientContext) -> Option<String> {
        (self.read)(cx).ok().flatten()
    }
}

type Client = dyn Fn(&ClientContext) -> Option<String> + Send + Sync;

/// The plugin that targets each request of a format it routes by
/// [`Routes`]. A request of another format keeps its target, and so does
/// one a plugin before it targeted; a gateway with a default target
/// ([`Gateway::target`]) tells it with [`Routing::over`], else that
/// default would pass for a plugin's choice.
///
/// A request a [`Rule`] routes carries its [`Planned`], for [`Dispatch`] to
/// send; each call carries its [`Chosen`], which this plugin sends its model
/// and records in [`Routes::stats`].
///
/// [`Gateway::target`]: respan_gateway_core::Gateway::target
///
/// ```
/// use std::sync::Arc;
/// use respan_gateway_conversation::Input;
/// use respan_gateway_plugin_routing::{Model, Routes, Routing, Speaks};
///
/// const CHAT: &str = "chat";
/// let speaks = |format: &str, internal: &str| Speaks {
///     format: format.into(),
///     kind: CHAT,
///     internal: internal.into(),
/// };
/// let routes = Routes::new()
///     .provider("openai", vec![speaks("openai-chat", "openai")])
///     .provider("anthropic", vec![speaks("anthropic-messages", "anthropic")])
///     .target("openai");
/// let route = routes.route(CHAT, "openai-chat", Some("anthropic/claude-haiku-5-5"));
/// assert_eq!(route.map(|r| r.internal).as_deref(), Some("anthropic"));
/// let routing = Routing::new(Arc::new(routes))
///     .format("openai-chat", CHAT, Model::of(Input::model, Input::set_model));
/// ```
pub struct Routing {
    routes: Arc<Routes>,
    formats: HashMap<String, (&'static str, Model)>,
    default: Option<String>,
    client: Option<Arc<Client>>,
}

impl std::fmt::Debug for Routing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Routing")
            .field("routes", &self.routes)
            .field("default", &self.default)
            .finish_non_exhaustive()
    }
}

impl Routing {
    /// Routing by `routes`, of no format yet.
    pub fn new(routes: Arc<Routes>) -> Self {
        Self {
            routes,
            formats: HashMap::new(),
            default: None,
            client: None,
        }
    }

    /// Tells it `provider` is the gateway's default target, not a
    /// plugin's choice.
    pub fn over(mut self, provider: impl Into<String>) -> Self {
        self.default = Some(provider.into());
        self
    }

    /// Routes the requests of format `name`, of `kind`, by their `model`.
    pub fn format(mut self, name: impl Into<String>, kind: &'static str, model: Model) -> Self {
        self.formats.insert(name.into(), (kind, model));
        self
    }

    /// Reads the name of a request's client key with `client`, for rules
    /// of some clients alone.
    pub fn client(
        mut self,
        client: impl Fn(&ClientContext) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.client = Some(Arc::new(client));
        self
    }
}

#[async_trait]
impl Plugin for Routing {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let Some((kind, model)) = self.formats.get(cx.format()) else {
            return Ok(None);
        };
        let asked = (model.read)(cx)?;
        let client = self.client.as_ref().and_then(|client| client(cx));
        let ask = Ask {
            kind,
            format: cx.format(),
            model: asked.as_deref(),
            client: client.as_deref(),
        };
        let Some(mut planned) = self.routes.plan(&ask) else {
            return Err(Fault::not_found(format!(
                "no provider serves {kind} requests; add one to the config"
            )));
        };
        // A format whose calls cannot each be sent a model of their own
        // goes to its first candidate alone.
        if model.call.is_none() {
            planned.to.truncate(1);
        }
        // A target a plugin before chose keeps the request as it is.
        if cx.target().is_some() && cx.target() != self.default.as_deref() {
            return Ok(None);
        }
        let first = planned.to[0].clone();
        cx.set_target(first.internal.clone());
        cx.extensions_mut().insert(first.clone());
        if matches!(planned.by, By::Rule(_)) {
            cx.extensions_mut().insert(Arc::new(planned));
        }
        // A route or a `provider/` prefix sends another name.
        if let Some(sent) = first.model.filter(|sent| Some(sent) != asked.as_ref()) {
            (model.write)(cx, sent)?;
        }
        Ok(None)
    }

    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        let Some(chosen) = px.extensions().get::<Chosen>().cloned() else {
            return Ok(());
        };
        let call = self.formats.get(px.client().format());
        let call = call.and_then(|(_, model)| model.call.clone());
        match (call, chosen.model) {
            (Some(call), Some(sends)) if px.provider() == chosen.internal => call(px, sends),
            _ => Ok(()),
        }
    }

    async fn on_provider_end(&self, px: &ProviderContext, end: &Result<(), Fault>) {
        let Some(chosen) = px.extensions().get::<Chosen>() else {
            return;
        };
        if px.provider() != chosen.internal {
            return;
        }
        let head = px.extensions().get::<Attempt>().and_then(Attempt::head);
        let kind = end.as_ref().err().map(Fault::kind);
        // A call dropped for another, as a hedge's losers are, says little.
        if kind == Some(FaultKind::Abandoned) && head.is_none() {
            return;
        }
        let failed = matches!(kind, Some(FaultKind::Provider | FaultKind::Gateway));
        let model = chosen.model.as_deref().unwrap_or_default();
        self.routes
            .stats
            .record(&chosen.provider, model, head, failed);
    }
}

/// Whether a failed attempt may be retried: [`first_success`]'s classifier.
type Classifier = Arc<dyn Fn(&Fault, bool) -> bool + Send + Sync>;

/// The plugin that sends a request a [`Rule`] routed to its candidates as
/// the rule says: each in turn until one succeeds, retried as its backoff
/// allows, or raced; a request whose target a plugin after [`Routing`]
/// changed is left alone.
///
/// It answers the request itself, so register it after every plugin that
/// targets or edits the request, and after any that would answer it
/// instead, such as a tool loop.
#[derive(Clone)]
pub struct Dispatch {
    should_retry: Classifier,
}

impl std::fmt::Debug for Dispatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Dispatch").finish_non_exhaustive()
    }
}

impl Dispatch {
    /// Dispatch retrying the failures `should_retry` says may be, such as
    /// `respan_gateway_http_faults::retryable`.
    pub fn new(should_retry: impl Fn(&Fault, bool) -> bool + Send + Sync + 'static) -> Self {
        Self {
            should_retry: Arc::new(should_retry),
        }
    }
}

#[async_trait]
impl Plugin for Dispatch {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        let Some(planned) = cx.extensions().get::<Arc<Planned>>().cloned() else {
            return Ok(None);
        };
        let By::Rule(rule) = &planned.by else {
            return Ok(None);
        };
        if cx.target() != Some(planned.to[0].internal.as_str()) {
            return Ok(None);
        }
        let calls: Vec<_> = (planned.to.iter())
            .map(|chosen| {
                cx.call(chosen.internal.as_str())
                    .with_extension(chosen.clone())
            })
            .collect();
        let until = rule.until.clone();
        let answer = match rule.hedge {
            Some(stagger) => race(calls, stagger, until).await,
            None => {
                let should_retry = |fault: &Fault, opened: bool| (self.should_retry)(fault, opened);
                first_success(calls, &rule.backoff, until, should_retry).await
            }
        };
        answer.map(Some)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    const CHAT: &str = "chat";
    const REALTIME: &str = "realtime";
    const PASSTHROUGH: &str = "passthrough";

    fn speaks(format: &str, kind: &'static str, internal: &str) -> Speaks {
        Speaks {
            format: format.into(),
            kind,
            internal: internal.into(),
        }
    }

    fn routes(target: Option<&str>, rules: &[(&str, &str)]) -> Routes {
        let mut routes = Routes::new()
            .provider("groq", vec![speaks("openai-chat", CHAT, "groq")])
            .provider(
                "openai",
                vec![
                    speaks("openai-chat", CHAT, "openai"),
                    speaks("openai-realtime", REALTIME, "openai:openai-realtime"),
                ],
            )
            .provider(
                "anthropic",
                vec![speaks("anthropic-messages", CHAT, "anthropic")],
            );
        for (name, to) in rules {
            let to = to.split(',').map(Candidate::parse).collect();
            routes = routes.rule(Rule::new(*name, to));
        }
        match target {
            Some(target) => routes.target(target),
            None => routes,
        }
    }

    fn to(routes: &Routes, format: &str, model: &str) -> (String, String, Option<String>) {
        let kind = match format {
            "openai-realtime" => REALTIME,
            _ => CHAT,
        };
        let route = routes.route(kind, format, Some(model));
        let route = route.unwrap_or_else(|| panic!("no route for {model}"));
        (route.provider, route.internal, route.model)
    }

    #[test]
    fn a_provider_prefix_names_the_provider_and_is_taken_off() {
        let routes = routes(None, &[]);
        let groq = to(&routes, "openai-chat", "groq/llama-3.3-70b-versatile");
        assert_eq!(
            groq,
            (
                "groq".into(),
                "groq".into(),
                Some("llama-3.3-70b-versatile".into())
            )
        );
        // Not a provider: the whole name is the model.
        let other = to(&routes, "openai-chat", "meta-llama/x");
        assert_eq!(other.0, "groq", "the first provider of the kind");
        assert_eq!(other.2.as_deref(), Some("meta-llama/x"));
    }

    #[test]
    fn rules_then_the_default() {
        let routes = routes(
            Some("anthropic"),
            &[("gpt-4*", "groq"), ("gpt-4o*", "anthropic")],
        );
        assert_eq!(
            to(&routes, "openai-chat", "gpt-4o-mini").0,
            "groq",
            "the first rule that matches"
        );
        assert_eq!(
            to(&routes, "anthropic-messages", "gpt-3.5").0,
            "anthropic",
            "no rule: the target"
        );
        assert_eq!(
            to(&routes, "openai-chat", "llama").0,
            "anthropic",
            "the target"
        );
        let untargeted = super::tests::routes(None, &[]);
        assert_eq!(
            to(&untargeted, "openai-chat", "llama").0,
            "groq",
            "the first"
        );
    }

    #[test]
    fn an_exact_rule_beats_a_named_provider_which_beats_a_glob() {
        let routes = routes(
            None,
            &[
                ("*", "anthropic"),
                ("smart", "anthropic/claude-opus,openai/gpt-5"),
            ],
        );
        let smart = to(&routes, "openai-chat", "smart");
        assert_eq!(
            smart,
            (
                "anthropic".into(),
                "anthropic".into(),
                Some("claude-opus".into())
            )
        );
        assert_eq!(to(&routes, "openai-chat", "openai/gpt-4o").0, "openai");
        assert_eq!(
            to(&routes, "openai-chat", "gpt-4o").0,
            "anthropic",
            "the glob"
        );
    }

    #[test]
    fn a_rule_plans_each_candidate_with_its_model() {
        let routes = routes(
            None,
            &[("smart", "groq/{model}-fast,anthropic/claude,openai")],
        );
        let ask = |format| Ask {
            kind: CHAT,
            format,
            model: Some("smart"),
            client: None,
        };
        let planned = routes.plan(&ask("openai-chat")).expect("planned");
        assert!(matches!(&planned.by, By::Rule(r) if r.name == "smart"));
        let sends: Vec<_> = planned
            .to
            .iter()
            .map(|c| (c.provider.as_str(), c.model.as_deref()))
            .collect();
        assert_eq!(
            sends,
            [
                ("groq", Some("smart-fast")),
                ("anthropic", Some("claude")),
                ("openai", Some("smart"))
            ]
        );
        assert_eq!(
            planned.to[2].model.as_deref(),
            Some("smart"),
            "sent as asked"
        );
    }

    #[test]
    fn a_rule_passes_over_what_cannot_serve_and_may_not_fall_back() {
        let mut rule = Rule::new(
            "voice",
            vec![Candidate::parse("anthropic"), Candidate::parse("openai")],
        );
        rule.fallback = false;
        let routes = routes(None, &[]).rule(rule);
        let ask = Ask {
            kind: REALTIME,
            format: "openai-realtime",
            model: Some("voice"),
            client: None,
        };
        let planned = routes.plan(&ask).expect("planned");
        assert_eq!(planned.to.len(), 1);
        assert_eq!(planned.to[0].provider, "openai");
        assert_eq!(
            planned.passed,
            [(
                "voice → anthropic".to_owned(),
                "serves no realtime".to_owned()
            )]
        );
    }

    #[test]
    fn a_rule_of_some_clients_and_kinds_passes_the_rest_on() {
        let mut rule = Rule::new("gpt-*", vec![Candidate::parse("groq")]);
        rule.clients = vec!["ops".into()];
        rule.kinds = vec!["chat".into()];
        let routes = routes(None, &[]).rule(rule);
        let ask = |client| Ask {
            kind: CHAT,
            format: "openai-chat",
            model: Some("gpt-4o"),
            client,
        };
        let first = |a| routes.plan(&a).expect("planned").to[0].provider.clone();
        assert_eq!(first(ask(Some("ops"))), "groq");
        assert_eq!(first(ask(Some("dev"))), "groq", "the first provider");
        assert_eq!(first(ask(None)), "groq");
    }

    #[test]
    fn needs_pass_over_a_model_known_to_lack_them() {
        let listing = Catalog::default();
        for (provider, id, can) in [
            ("groq", "x", respan_gateway_sdk::Capability::Reasoning),
            ("openai", "y", respan_gateway_sdk::Capability::Tools),
        ] {
            let mut model = respan_gateway_sdk::Model::new(id);
            model.can(can);
            listing.set(provider, [model]);
        }
        let mut rule = Rule::new(
            "agent",
            vec![Candidate::parse("groq/x"), Candidate::parse("openai/y")],
        );
        rule.needs = vec!["tools".into()];
        let routes = routes(None, &[]).rule(rule).with_catalog(Arc::new(listing));
        assert_eq!(to(&routes, "openai-chat", "agent").0, "openai");
    }

    #[test]
    fn a_client_is_sent_in_its_own_format_else_translated() {
        let routes = routes(Some("anthropic"), &[]);
        let (_, internal, _) = to(&routes, "anthropic-messages", "openai/gpt-4o");
        assert_eq!(internal, "openai", "OpenAI's first chat format");
        let (_, internal, _) = to(&routes, "openai-chat", "anthropic/claude");
        assert_eq!(internal, "anthropic");
    }

    #[test]
    fn a_provider_says_its_format_by_the_models_card() {
        let messages = speaks("anthropic-messages", CHAT, "agg:anthropic-messages");
        let routes = Routes::new()
            .provider("agg", vec![speaks("openai-chat", CHAT, "agg"), messages])
            .provider("live", vec![speaks("openai-realtime", REALTIME, "live")])
            // Its Messages serves only Claude; a card it cannot read, as usual.
            .format_for("agg", |ask, card| {
                let maker = card.maker.as_deref()?;
                (ask.format == "anthropic-messages" && maker != "anthropic")
                    .then_some("openai-chat")
            })
            // A format it does not speak of the kind is not taken.
            .format_for("live", |_, _| Some("openai-chat"));
        let listing = Catalog::default();
        let card = |id: &str, maker: &str| {
            let mut model = respan_gateway_sdk::Model::new(id);
            model.maker = Some(maker.into());
            model
        };
        listing.set("agg", [card("claude", "anthropic"), card("gpt", "openai")]);
        let routes = routes.with_catalog(Arc::new(listing));
        let internal = |format, model| to(&routes, format, model).1;
        assert_eq!(
            internal("anthropic-messages", "claude"),
            "agg:anthropic-messages"
        );
        assert_eq!(internal("anthropic-messages", "gpt"), "agg");
        assert_eq!(internal("openai-chat", "claude"), "agg");
        assert_eq!(
            internal("anthropic-messages", "unlisted"),
            "agg:anthropic-messages"
        );
        assert_eq!(internal("openai-realtime", "x"), "live");
    }

    #[test]
    fn only_providers_of_the_kind_are_candidates() {
        let routes = routes(Some("anthropic"), &[]);
        // Anthropic serves no realtime, so the target is passed over.
        let realtime = to(&routes, "openai-realtime", "anthropic/gpt-realtime");
        let asked = Some("anthropic/gpt-realtime".to_owned());
        assert_eq!(
            realtime,
            ("openai".into(), "openai:openai-realtime".into(), asked),
            "the default, sent the whole name"
        );
        assert_eq!(routes.route(PASSTHROUGH, "passthrough", None), None);
    }

    #[test]
    fn a_session_crosses_formats_within_its_kind() {
        const LIVE: &str = "realtime";
        const RESPONSES: &str = "responses";
        let openai = vec![
            speaks("openai-realtime", LIVE, "openai:openai-realtime"),
            speaks("responses-ws", RESPONSES, "openai:responses-ws"),
        ];
        let routes = Routes::new()
            .provider("openai", openai)
            .provider("gemini", vec![speaks("gemini-live", LIVE, "gemini")])
            .target("gemini");
        let internal = |kind, format, model| routes.route(kind, format, model).map(|r| r.internal);
        let to_openai = internal(LIVE, "gemini-live", Some("openai/gpt-realtime"));
        assert_eq!(to_openai.as_deref(), Some("openai:openai-realtime"));
        let to_gemini = internal(LIVE, "openai-realtime", Some("gemini/gemini-live"));
        assert_eq!(to_gemini.as_deref(), Some("gemini"));
        assert_eq!(
            internal(LIVE, "gemini-live", None).as_deref(),
            Some("gemini")
        );
        // Gemini speaks no Responses, so even a model it is named for stays
        // with the provider that does.
        let responses = internal(RESPONSES, "responses-ws", Some("gemini/gemini-live"));
        assert_eq!(responses.as_deref(), Some("openai:responses-ws"));
    }

    #[test]
    fn each_kind_has_its_default() {
        let routes = routes(Some("anthropic"), &[]);
        assert_eq!(
            routes.defaults(),
            [
                ("chat", "anthropic".to_owned()),
                ("realtime", "openai".to_owned())
            ]
        );
    }

    #[test]
    fn a_listed_model_goes_to_its_first_lister() {
        let listing = Catalog::default();
        let list = |ids: &[&str]| {
            ids.iter()
                .map(|id| respan_gateway_sdk::Model::new(*id))
                .collect::<Vec<_>>()
        };
        listing.set("groq", list(&["kimi-k2", "gpt-oss-120b"]));
        listing.set("openai", list(&["gpt-oss-120b"]));
        let routes = routes(Some("anthropic"), &[("smart", "anthropic/claude")])
            .with_catalog(Arc::new(listing));
        let ask = |model| Ask {
            kind: CHAT,
            format: "openai-chat",
            model: Some(model),
            client: None,
        };
        let kimi = routes.plan(&ask("kimi-k2")).expect("planned");
        assert!(matches!(kimi.by, By::Listed));
        assert_eq!(kimi.to[0].provider, "groq");
        assert_eq!(
            kimi.to[0].model.as_deref(),
            Some("kimi-k2"),
            "sent as asked"
        );
        // Both list it: the first added.
        assert_eq!(to(&routes, "openai-chat", "gpt-oss-120b").0, "groq");
        // A route comes first; an unlisted name falls to the default.
        assert_eq!(to(&routes, "openai-chat", "smart").0, "anthropic");
        let unlisted = routes.plan(&ask("gpt-4o")).expect("planned");
        assert!(matches!(unlisted.by, By::Default));
        assert_eq!(unlisted.to[0].provider, "anthropic");
        // A lister that serves no realtime is passed over.
        let live = Ask {
            kind: REALTIME,
            format: "openai-realtime",
            model: Some("kimi-k2"),
            client: None,
        };
        assert!(matches!(
            routes.plan(&live).expect("planned").by,
            By::Default
        ));
    }
}

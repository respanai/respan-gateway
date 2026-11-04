//! The config's `routes` as the router's rules: each candidate's provider
//! checked, and its fallback, retries and hedge as the rule's attempts.

use std::time::Duration;

use respan_gateway_core::Fault;
use respan_gateway_plugin_fallback::{Backoff, Until};
use respan_gateway_plugin_routing::{Candidate, Pick, Rule};

use crate::auth::Client;
use crate::compose::Upstream;
use crate::config::{Commit, Config, RouteConfig};
use crate::formats::KnownFormat;

/// The most tries per candidate.
const MAX_TRIES: u32 = 10;
/// The first wait before a retry, unless the route says.
const BACKOFF: Duration = Duration::from_millis(250);
/// The longest wait before a retry, unless the route says.
const MAX_BACKOFF: Duration = Duration::from_secs(10);

/// The rules of `config`'s routes, in order; `Err` for a route that names
/// what is not configured, or for a retired setting, saying how it is now
/// written.
pub fn rules(
    config: &Config,
    upstreams: &[Upstream],
    clients: &[Client],
    formats: &[KnownFormat],
) -> Result<Vec<Rule>, Fault> {
    retired(config)?;
    let routes = config.routes.iter();
    routes
        .map(|(name, route)| {
            rule(name, route, upstreams, clients, formats)
                .map_err(|e| Fault::config(format!("routes: {name}: {e}")))
        })
        .collect()
}

fn rule(
    name: &str,
    route: &RouteConfig,
    upstreams: &[Upstream],
    clients: &[Client],
    formats: &[KnownFormat],
) -> Result<Rule, String> {
    if name.is_empty() {
        return Err("a route needs a model name, or a glob such as `llama-*`".into());
    }
    if route.to.is_empty() {
        return Err("to: names no provider".into());
    }
    let mut to: Vec<Candidate> = Vec::new();
    for target in &route.to {
        let mut candidate = Candidate::parse(&target.target);
        if !upstreams.iter().any(|u| u.name == candidate.provider) {
            let names: Vec<&str> = upstreams.iter().map(|u| u.name.as_str()).collect();
            return Err(format!(
                "to: unknown provider '{}'; providers: [{}]",
                candidate.provider,
                names.join(", ")
            ));
        }
        if candidate.model.as_deref() == Some("") {
            return Err(format!(
                "to: '{}' names no model after the '/'",
                target.target
            ));
        }
        if to
            .iter()
            .any(|c| c.provider == candidate.provider && c.model == candidate.model)
        {
            return Err(format!("to: '{}' is listed twice", target.target));
        }
        if target.weight == 0 && route.pick == Pick::Weighted {
            return Err(format!("to: '{}': a weight must be above 0", target.target));
        }
        candidate.weight = target.weight;
        to.push(candidate);
    }
    for client in &route.when.clients {
        if !clients.iter().any(|c| c.name == *client) {
            let names: Vec<&str> = clients.iter().map(|c| c.name.as_str()).collect();
            return Err(format!(
                "when: clients: unknown client key '{client}'; named keys: [{}]",
                names.join(", ")
            ));
        }
    }
    for kind in &route.when.kinds {
        if !formats.iter().any(|f| f.kind == kind) {
            let mut kinds: Vec<&str> = formats.iter().map(|f| f.kind).collect();
            kinds.dedup();
            return Err(format!(
                "when: kinds: unknown kind '{kind}'; kinds: [{}]",
                kinds.join(", ")
            ));
        }
    }
    let tries = route.tries.unwrap_or(1);
    if !(1..=MAX_TRIES).contains(&tries) {
        return Err(format!("tries: {tries}; 1 to {MAX_TRIES}"));
    }
    let base = route.backoff.unwrap_or(BACKOFF);
    let max = route.max_backoff.unwrap_or(MAX_BACKOFF);
    if base > max {
        return Err(format!(
            "backoff: {base:?} is longer than max_backoff: {max:?}"
        ));
    }
    if route.hedge.is_some() && route.fallback == Some(false) {
        return Err("hedge races the candidates, so cannot go with `fallback = false`".into());
    }
    let mut rule = Rule::new(name, to);
    rule.pick = route.pick;
    rule.needs = route.needs.clone();
    rule.clients = route.when.clients.clone();
    rule.kinds = route.when.kinds.clone();
    rule.fallback = route.fallback.unwrap_or(true);
    rule.hedge = route.hedge;
    rule.backoff = Backoff::new(tries, base, max);
    rule.until = match route.until {
        Commit::Head => Until::Head,
        Commit::End => Until::End,
    };
    Ok(rule)
}

/// Refuses the settings routes replaced, saying how each is written now.
fn retired(config: &Config) -> Result<(), Fault> {
    let plugins = &config.plugins;
    let moved = [
        (
            plugins.routing.models.is_some(),
            "plugins: routing: models is now the top-level `routes`: \
             `models = [{ prefix = \"llama\", provider = \"groq\" }]` is \
             `[routes.\"llama*\"]` with `to = \"groq\"`",
        ),
        (
            plugins.fallback.is_some(),
            "plugins: fallback is now a route's own: `[routes.\"gpt-*\"]` with \
             `to = [\"openai\", \"azure\"]` falls back in order; `tries`, `backoff`, \
             `max_backoff` and `until` are the route's too",
        ),
        (
            config.mcp.is_some(),
            "mcp is now tools: `[tools.github]` is `[tools.github]`, with the same fields",
        ),
        (
            config.prices.is_some(),
            "prices are now each model's own: `[prices.\"gpt-4o\"]` is \
             `[models.\"openai/gpt-4o\"]` with `pricing = { input = 2.5, output = 10 }`",
        ),
        (
            plugins.hedge.is_some(),
            "plugins: hedge is now a route's own: `[routes.\"gpt-*\"]` with \
             `to = [\"openai\", \"azure\"]` and `hedge = \"200ms\"`",
        ),
    ];
    match moved.into_iter().find(|(set, _)| *set) {
        Some((_, how)) => Err(Fault::config(how)),
        None => Ok(()),
    }
}

//! What a started gateway tells the person who started it.

use std::net::SocketAddr;

use super::{Built, DEFAULT_LISTEN};
use crate::VERSION;
use crate::formats::kind;

impl Built {
    /// What it serves, for a person starting it: the first line says
    /// where it listens.
    pub(super) fn banner(&self, addr: SocketAddr) -> String {
        let mut lines = vec![format!("respan-gateway {VERSION} listening on {addr}")];
        let shown = match addr.ip().is_unspecified() {
            true => SocketAddr::new(DEFAULT_LISTEN.ip(), addr.port()),
            false => addr,
        };
        let base = format!("http://{shown}");
        // A label, then each value on a line of its own.
        let mut row = |label: &str, values: Vec<String>| {
            for (i, value) in values.into_iter().enumerate() {
                let label = if i == 0 { label } else { "" };
                lines.push(format!("  {label:<10} {value}"));
            }
        };
        row("config", vec![self.config.source.to_string()]);
        let upstreams = &self.plan.upstreams;
        let width = upstreams
            .iter()
            .map(|u| u.name.len())
            .max()
            .unwrap_or_default();
        let providers = upstreams.iter().map(|u| {
            let formats: Vec<&str> = u.entries.iter().map(|e| e.format.as_str()).collect();
            format!("{:<width$}  {}", u.name, formats.join(", "))
        });
        let providers: Vec<String> = providers.collect();
        match (providers.is_empty(), self.plan.mcp.is_empty()) {
            (true, true) => row("providers", vec!["none".to_owned()]),
            (true, false) => {}
            (false, _) => row("providers", providers),
        }
        let width = self.plan.mcp.iter().map(|s| s.name.len()).max();
        let servers: Vec<String> = (self.plan.mcp.iter())
            .map(|s| {
                let (name, kind, at) = (&s.name, s.transport_label(), s.endpoint());
                format!("{name:<w$}  {kind:<6}  {at}", w = width.unwrap_or_default())
            })
            .collect();
        if !servers.is_empty() {
            row("mcp", servers);
        }
        let mut endpoints: Vec<String> = self
            .plan
            .serve
            .iter()
            .filter_map(|name| self.formats.iter().find(|f| f.name == *name))
            .flat_map(|f| f.endpoints.iter().map(|e| (*e).to_owned()))
            .collect();
        if !self.config.relay {
            endpoints.push("GET /v1/models".to_owned());
            if self.plan.serves("gemini-generate") {
                endpoints.push("GET /v1beta/models".to_owned());
            }
        }
        endpoints.push("GET /healthz".to_owned());
        endpoints.push("GET /metrics".to_owned());
        row("endpoints", endpoints);
        if self.config.ui {
            row("web ui", vec![format!("{base}/ui/")]);
        }
        let auth = match self.plan.keys.len() {
            0 => "off: no client keys".to_owned(),
            1 => "on: 1 client key".to_owned(),
            n => format!("on: {n} client keys"),
        };
        row("auth", vec![auth]);
        if let Some(example) = self.example(&base) {
            row("try", vec![example]);
        }
        lines.join("\n")
    }

    /// A chat request to the default chat provider, as curl sends it.
    fn example(&self, base: &str) -> Option<String> {
        if !self.plan.serves("openai-chat") {
            return None;
        }
        self.plan.routes.route(kind::CHAT, "openai-chat", None)?;
        let key = match self.plan.keys.is_empty() {
            true => "",
            false => " -H 'authorization: Bearer <key>'",
        };
        Some(format!(
            "curl {base}/v1/chat/completions{key} -H 'content-type: application/json' \
             -d '{{\"model\":\"<model>\",\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}'"
        ))
    }
}

//! Tenants whose config changes all day: tenant policy lives outside the
//! gateway's static setup, in a map a control plane edits while requests
//! run, and each call carries the tenant's own provider key.
//!
//! - Each edit carries a version; an older one arriving late is ignored, so
//!   updates can come from any event stream, one tenant at a time.
//! - A request reads its tenant once, in `on_client`, and keeps that
//!   snapshot in an extension: every call it makes (fallbacks, judges)
//!   sees the same version, and nothing is written back from the request
//!   path.
//! - Each call gets the tenant's key for its provider as [`CallHeaders`];
//!   the transports hold no keys, and the gateway key the client sent never
//!   leaves.
//!
//! Run it with `cargo run -p respan-gateway-examples --example tenants`.
//! Look for acme's first request reaching `openai` with its v1 key, the
//! second reaching `azure` with its v2 key, a late v1 edit ignored, and an
//! unknown gateway key refused with 401.

use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

use mockllm::FakeOpenAi;
use respan_gateway_conversation::Input;
use respan_gateway_conversation_http::openai::OpenAiChat;
use respan_gateway_core::{
    ClientContext, ErrorCode, Fault, Gateway, Plugin, Provider, ProviderContext, Response,
    async_trait,
};
use respan_gateway_http::http::header::AUTHORIZATION;
use respan_gateway_http::http::{self, HeaderMap, HeaderValue, StatusCode};
use respan_gateway_http::http_body_util::BodyExt;
use respan_gateway_http::{Body, Credential, call::CallHeaders};
use respan_gateway_http_faults::Status;
use respan_gateway_http_transport::HttpTransport;

#[tokio::main]
async fn main() -> Result<(), Fault> {
    let (openai, azure) = (FakeOpenAi::start().await, FakeOpenAi::start().await);
    let tenants = Tenants::default();
    let gateway = Gateway::new()
        .provider(Provider::new(
            "openai",
            OpenAiChat,
            HttpTransport::new(openai.base_url())?.connect(),
        ))?
        .provider(Provider::new(
            "azure",
            OpenAiChat,
            HttpTransport::new(azure.base_url())?.connect(),
        ))?
        .client::<Input, _, _>(OpenAiChat)?
        .plugin(tenants.clone());

    tenants.upsert("gw-acme", tenant(1, "openai", "sk-acme-1")?);
    println!(
        "v1, acme on openai: HTTP {}",
        send(&gateway, "gw-acme").await?
    );

    // The control plane moves acme to Azure while the gateway runs.
    tenants.upsert("gw-acme", tenant(2, "azure", "az-acme-2")?);
    println!(
        "v2, acme on azure: HTTP {}",
        send(&gateway, "gw-acme").await?
    );

    // A stale edit, delivered late, does not undo the move.
    let applied = tenants.upsert("gw-acme", tenant(1, "openai", "sk-acme-1")?);
    println!("late v1 edit applied: {applied}");

    println!("unknown key: HTTP {}", send(&gateway, "gw-nobody").await?);

    println!("\nwhat the providers saw:");
    for (name, fake) in [("openai", &openai), ("azure", &azure)] {
        for request in fake.requests() {
            println!("  {name}: {:?}", request.header("authorization"));
        }
    }
    Ok(())
}

// The tenant map.

/// One tenant's policy, at one version.
#[derive(Debug, Clone, Default)]
struct Tenant {
    version: u64,
    target: String,
    /// Headers for each provider it may call, such as its own API key.
    keys: HashMap<String, HeaderMap>,
}

/// Tenants by gateway key. Clones share the map.
#[derive(Clone, Default)]
struct Tenants(Arc<RwLock<HashMap<String, Arc<Tenant>>>>);

impl Tenants {
    /// Apply an edit from the control plane; `false` if it is no newer than
    /// the version held.
    fn upsert(&self, key: &str, tenant: Tenant) -> bool {
        let mut all = self.0.write();
        if all
            .get(key)
            .is_some_and(|held| held.version >= tenant.version)
        {
            return false;
        }
        all.insert(key.to_owned(), Arc::new(tenant));
        true
    }

    fn get(&self, key: &str) -> Option<Arc<Tenant>> {
        self.0.read().get(key).cloned()
    }
}

#[async_trait]
impl Plugin for Tenants {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        // The gateway key is ours: take it off before anything is sent.
        let credential = cx.extensions_mut().remove::<Credential>();
        let key = credential.as_ref().and_then(Credential::bearer);
        let unknown = || {
            Fault::new("unknown key")
                .with(ErrorCode("unauthorized".into()))
                .with(Status(StatusCode::UNAUTHORIZED))
        };
        let tenant = self.get(key.unwrap_or_default()).ok_or_else(unknown)?;
        cx.set_target(tenant.target.clone());
        cx.extensions_mut().insert(tenant);
        Ok(None)
    }

    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        let keys = px
            .client()
            .extensions()
            .get::<Arc<Tenant>>()
            .and_then(|t| t.keys.get(px.provider()));
        // No key, no call: never fall back to someone else's credentials.
        let keys = keys.cloned().ok_or_else(|| {
            Fault::new("no key for this provider")
                .with(ErrorCode("no_key".into()))
                .with(Status(StatusCode::FORBIDDEN))
        })?;
        px.extensions_mut().insert(CallHeaders(keys));
        Ok(())
    }
}

/// A tenant at `version` that calls `target` with `key`.
fn tenant(version: u64, target: &str, key: &str) -> Result<Tenant, Fault> {
    let headers = HeaderMap::from_iter([(AUTHORIZATION, bearer(key)?)]);
    Ok(Tenant {
        version,
        target: target.into(),
        keys: HashMap::from([(target.to_owned(), headers)]),
    })
}

// A client.

fn bearer(key: &str) -> Result<HeaderValue, Fault> {
    HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|e| respan_gateway_http_faults::built_wrong(e.into()))
}

/// A chat request with `gateway_key`, read to its end; its status.
async fn send(gateway: &Gateway, gateway_key: &str) -> Result<StatusCode, Fault> {
    let body = r#"{"model":"gpt-x","messages":[{"role":"user","content":"hi"}]}"#;
    let mut req = http::Request::new(Body::full(body));
    *req.method_mut() = http::Method::POST;
    *req.uri_mut() = http::Uri::from_static("/v1/chat/completions");
    req.headers_mut()
        .insert(AUTHORIZATION, bearer(gateway_key)?);
    let (resp, _) = respan_gateway_http::handle(gateway, req, 32 << 20).await;
    let status = resp.status();
    resp.into_body().collect().await?;
    Ok(status)
}

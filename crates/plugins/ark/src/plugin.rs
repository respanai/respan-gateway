//! Respan client/provider hooks; both share the same scanner and request proofs.
use crate::{
    request::RequestChecks,
    runtime::{Ark, ProviderGuard},
    scan::preflight,
};
use respan_gateway_conversation::Input;
use respan_gateway_core::{ClientContext, Fault, Plugin, ProviderContext, Response, async_trait};

#[async_trait]
impl Plugin for Ark {
    async fn on_client(&self, cx: &mut ClientContext) -> Result<Option<Response>, Fault> {
        if !cx.is_kind::<Input>() {
            return Ok(None);
        }
        let ledger = cx
            .extensions()
            .get::<RequestChecks>()
            .cloned()
            .unwrap_or_default();
        cx.extensions_mut().insert(ledger.clone());
        let input = cx.input::<Input>()?;
        if !preflight(input, &self.0.config)? {
            return Ok(None);
        }
        let checked = self.inspect_request(input.clone(), ledger).await?;
        cx.filter(move |input: &mut Input| {
            *input = checked.clone();
            Ok(())
        })?;
        Ok(None)
    }
}
#[async_trait]
impl Plugin for ProviderGuard {
    async fn on_provider(&self, px: &mut ProviderContext) -> Result<(), Fault> {
        if !px.is_kind::<Input>() {
            return Ok(());
        }
        let ledger = px
            .extensions()
            .get::<RequestChecks>()
            .cloned()
            .unwrap_or_default();
        px.extensions_mut().insert(ledger.clone());
        let input = px.input::<Input>()?;
        if !preflight(input, &self.0.0.config)? {
            return Ok(());
        }
        let checked = self.0.inspect_request(input.clone(), ledger).await?;
        px.filter(move |input: &mut Input| {
            *input = checked.clone();
            Ok(())
        })
    }
}

use async_trait::async_trait;
use rust_provider_kit_core::{
    ProviderAccountInspection, ProviderCredentialLease, ProviderDescriptor, ProviderFailure,
    ProviderInstant, ProviderModelCatalogResult, ProviderTurnRequest,
};

use crate::http_transport::{ProviderHttpRequest, ProviderHttpTransport};
use crate::sse::ServerSentEvent;
use crate::wire::ProviderDecodedEvent;

pub(crate) trait ProviderStreamDecoder: Send {
    fn consume(
        &mut self,
        event: &ServerSentEvent,
    ) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure>;
    fn finish(&mut self) -> Result<Vec<ProviderDecodedEvent>, ProviderFailure>;
}

#[async_trait]
pub(crate) trait ProviderAdapter: Send + Sync {
    fn descriptor(&self) -> &ProviderDescriptor;
    async fn make_execution_request(
        &self,
        request: &ProviderTurnRequest,
        credential: &ProviderCredentialLease,
    ) -> Result<ProviderHttpRequest, ProviderFailure>;
    fn make_decoder(
        &self,
        request: &ProviderTurnRequest,
    ) -> Result<Box<dyn ProviderStreamDecoder>, ProviderFailure>;
    /// Adapter-specific context to append to an HTTP failure message.
    ///
    /// The shared HTTP mapping knows the status and response body, while an
    /// adapter is the only layer that knows facts about the request identity
    /// it declared. Adapters return context only when it is directly relevant
    /// to the route and status; callers must not interpret it as a diagnosis.
    fn failure_context(&self, _status: u16) -> Option<String> {
        None
    }
    async fn inspect(
        &self,
        credential: &ProviderCredentialLease,
        transport: &dyn ProviderHttpTransport,
        now: ProviderInstant,
    ) -> Result<ProviderAccountInspection, ProviderFailure>;
    async fn models(
        &self,
        credential: &ProviderCredentialLease,
        transport: &dyn ProviderHttpTransport,
        now: ProviderInstant,
    ) -> Result<ProviderModelCatalogResult, ProviderFailure>;
}

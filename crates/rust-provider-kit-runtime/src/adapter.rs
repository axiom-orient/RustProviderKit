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

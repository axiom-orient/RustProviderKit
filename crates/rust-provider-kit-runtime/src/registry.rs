use std::collections::BTreeMap;
use std::sync::Arc;

use rust_provider_kit_core::{
    BuiltInProviderId, ProviderDescriptor, ProviderFailure, ProviderFailureCode, ProviderId,
};
use url::Url;

use crate::adapter::ProviderAdapter;
use crate::adapters::{
    AnthropicMessagesAdapter, AnthropicMessagesKind, GeminiGenerateContentAdapter,
    OpenAiChatAdapter, OpenAiChatKind, OpenAiResponsesAdapter, OpenAiResponsesKind,
};

#[derive(Clone)]
pub(crate) struct BuiltInProviderRegistry {
    adapters: BTreeMap<ProviderId, Arc<dyn ProviderAdapter>>,
}
impl std::fmt::Debug for BuiltInProviderRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BuiltInProviderRegistry")
            .field("provider_count", &self.adapters.len())
            .finish()
    }
}

impl BuiltInProviderRegistry {
    pub(crate) fn new() -> Result<Self, ProviderFailure> {
        let values: Vec<Arc<dyn ProviderAdapter>> = vec![
            Arc::new(OpenAiResponsesAdapter::new(OpenAiResponsesKind::Codex)?),
            Arc::new(OpenAiResponsesAdapter::new(OpenAiResponsesKind::OpenAi)?),
            Arc::new(AnthropicMessagesAdapter::new(
                AnthropicMessagesKind::Anthropic,
            )?),
            Arc::new(GeminiGenerateContentAdapter::new()?),
            Arc::new(OpenAiChatAdapter::new(OpenAiChatKind::OpenRouter)?),
            Arc::new(OpenAiChatAdapter::new(OpenAiChatKind::DeepSeek)?),
            Arc::new(OpenAiChatAdapter::new(OpenAiChatKind::Qwen)?),
            Arc::new(OpenAiChatAdapter::new(OpenAiChatKind::Kimi)?),
            Arc::new(AnthropicMessagesAdapter::new(AnthropicMessagesKind::Zai)?),
            Arc::new(AnthropicMessagesAdapter::new(
                AnthropicMessagesKind::MiniMax,
            )?),
        ];
        let expected_count = values.len();
        let adapters = values
            .into_iter()
            .map(|adapter| (adapter.descriptor().id().clone(), adapter))
            .collect::<BTreeMap<_, _>>();
        if adapters.len() != expected_count || adapters.len() != BuiltInProviderId::all().len() {
            return Err(ProviderFailure::new(
                ProviderFailureCode::InternalInvariant,
                "built-in provider registry contains duplicate or missing identifiers",
            ));
        }
        Ok(Self { adapters })
    }
    #[must_use]
    pub(crate) fn descriptors(&self) -> Vec<ProviderDescriptor> {
        self.adapters
            .values()
            .map(|adapter| adapter.descriptor().clone())
            .collect()
    }
    pub(crate) fn adapter(
        &self,
        id: &ProviderId,
    ) -> Result<Arc<dyn ProviderAdapter>, ProviderFailure> {
        self.adapters.get(id).cloned().ok_or_else(|| {
            ProviderFailure::new(
                ProviderFailureCode::ProviderUnsupported,
                format!("unsupported provider: {}", id.as_str()),
            )
        })
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ProviderEndpointCatalog;
impl ProviderEndpointCatalog {
    pub(crate) fn open_ai() -> Result<Url, ProviderFailure> {
        endpoint("https://api.openai.com")
    }
    pub(crate) fn codex() -> Result<Url, ProviderFailure> {
        endpoint("https://chatgpt.com/backend-api/codex")
    }
    pub(crate) fn anthropic() -> Result<Url, ProviderFailure> {
        endpoint("https://api.anthropic.com")
    }
    pub(crate) fn gemini() -> Result<Url, ProviderFailure> {
        endpoint("https://generativelanguage.googleapis.com/v1beta")
    }
    pub(crate) fn open_router() -> Result<Url, ProviderFailure> {
        endpoint("https://openrouter.ai/api/v1")
    }
    pub(crate) fn deep_seek() -> Result<Url, ProviderFailure> {
        endpoint("https://api.deepseek.com")
    }
    pub(crate) fn qwen() -> Result<Url, ProviderFailure> {
        endpoint("https://portal.qwen.ai/v1")
    }
    pub(crate) fn kimi() -> Result<Url, ProviderFailure> {
        endpoint("https://api.kimi.com/coding/v1")
    }
    pub(crate) fn zai_anthropic() -> Result<Url, ProviderFailure> {
        endpoint("https://api.z.ai/api/anthropic")
    }
    pub(crate) fn zai_models() -> Result<Url, ProviderFailure> {
        endpoint("https://api.z.ai/api/paas/v4")
    }
    /// MiniMax Anthropic-compatible execution base.
    pub(crate) fn mini_max() -> Result<Url, ProviderFailure> {
        endpoint("https://api.minimax.io/anthropic")
    }

    /// MiniMax model-catalog base. MiniMax does not expose `/v1/models`
    /// below its Anthropic-compatible execution prefix.
    pub(crate) fn mini_max_models() -> Result<Url, ProviderFailure> {
        endpoint("https://api.minimax.io")
    }
}
fn endpoint(value: &str) -> Result<Url, ProviderFailure> {
    Url::parse(value).map_err(|_| {
        ProviderFailure::new(
            ProviderFailureCode::InternalInvariant,
            "built-in provider endpoint is invalid",
        )
    })
}

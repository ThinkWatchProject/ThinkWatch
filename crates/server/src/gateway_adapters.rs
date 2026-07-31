//! Building gateway adapters from a stored provider row.
//!
//! Separate from `app::load_providers_into_router` because three call
//! sites need it: the router build, the import-time protocol probe, and
//! the runtime relearn path that reacts to an upstream rejecting a
//! dialect. All three must construct adapters identically — a probe
//! that talks to the upstream differently from the live path proves
//! nothing.

use std::sync::Arc;

use think_watch_common::models::Provider;

/// Everything needed to build an adapter for a provider, decrypted once
/// per router rebuild. Adapters are built per `(provider, protocol)`
/// rather than per provider, because a single provider record can serve
/// several wire dialects — see
/// [`think_watch_gateway::providers::protocol::UpstreamProtocol`].
pub(crate) struct ProviderMaterials {
    pub(crate) name: String,
    pub(crate) provider_type: String,
    pub(crate) base_url: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) api_version: Option<String>,
    /// `"access_key:secret_key"`, or empty for IMDSv2 mode. Only read
    /// by the Bedrock adapter.
    pub(crate) bedrock_credentials: String,
}

impl ProviderMaterials {
    /// Decrypt a stored provider row into adapter inputs.
    ///
    /// Failures degrade rather than abort: a header that won't decrypt
    /// is dropped (logged by `decrypt_headers_from_config`) and an
    /// undecryptable AWS secret falls back to IMDSv2 mode, matching the
    /// router build's long-standing behaviour of keeping the gateway up
    /// with a degraded provider instead of refusing to boot.
    pub(crate) fn from_provider(provider: &Provider, encryption_key: &str) -> Self {
        let headers = crate::handlers::providers::decrypt_headers_from_config(
            &provider.config_json,
            encryption_key,
            &provider.name,
        )
        .into_iter()
        .map(|h| (h.key, h.value))
        .collect();

        // `access_key_id` is not sensitive; `secret_access_key` is
        // wrapped as `{"$enc": ...}` at rest.
        let access_key = provider
            .config_json
            .get("aws_access_key_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let secret_key = provider
            .config_json
            .get("aws_secret_access_key")
            .map(|v| {
                crate::handlers::providers::decrypt_secret_from_json(v, encryption_key)
                    .unwrap_or_else(|e| {
                        tracing::error!(
                            provider = %provider.name,
                            "Failed to decrypt aws_secret_access_key — treating as IMDSv2 mode: {e}"
                        );
                        String::new()
                    })
            })
            .unwrap_or_default();
        let bedrock_credentials = if access_key.is_empty() && secret_key.is_empty() {
            String::new() // IMDSv2 mode
        } else {
            format!("{access_key}:{secret_key}")
        };

        Self {
            name: provider.name.clone(),
            provider_type: provider.provider_type.clone(),
            base_url: provider.base_url.clone(),
            headers,
            api_version: provider
                .config_json
                .get("api_version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            bedrock_credentials,
        }
    }
}

/// Build the adapter that speaks `protocol` to this provider.
///
/// Every protocol is reachable from every provider record: the dialect
/// is a property of the route, not of the provider row, so an
/// OpenAI-compatible aggregator can serve `anthropic.*` over
/// `/v1/messages` without the admin creating a second provider.
pub(crate) fn build_adapter(
    protocol: think_watch_gateway::providers::protocol::UpstreamProtocol,
    m: &ProviderMaterials,
) -> Arc<dyn think_watch_gateway::providers::DynAiProvider> {
    use think_watch_gateway::providers::protocol::UpstreamProtocol;
    use think_watch_gateway::providers::{
        anthropic::AnthropicProvider, azure_openai::AzureOpenAiProvider, bedrock::BedrockProvider,
        custom::CustomProvider, google::GoogleProvider, openai::OpenAiProvider,
        openai_responses::OpenAiResponsesProvider,
    };

    match protocol {
        UpstreamProtocol::AnthropicMessages => Arc::new(
            AnthropicProvider::new(m.base_url.clone()).with_custom_headers(m.headers.clone()),
        ),
        UpstreamProtocol::GoogleGenerate => {
            Arc::new(GoogleProvider::new(m.base_url.clone()).with_custom_headers(m.headers.clone()))
        }
        UpstreamProtocol::BedrockNative => Arc::new(
            BedrockProvider::new(m.base_url.clone(), m.bedrock_credentials.clone())
                .with_custom_headers(m.headers.clone()),
        ),
        UpstreamProtocol::OpenAiResponses => Arc::new(
            OpenAiResponsesProvider::new(m.base_url.clone()).with_custom_headers(m.headers.clone()),
        ),
        // Chat Completions has two shapes: Azure rewrites the path
        // around a deployment + api-version, everyone else is plain
        // OpenAI. `custom` keeps its own adapter only so the provider's
        // name shows up in logs instead of the literal "openai".
        UpstreamProtocol::OpenAiChat => match m.provider_type.as_str() {
            "azure_openai" => Arc::new(
                AzureOpenAiProvider::new(m.base_url.clone(), m.api_version.clone())
                    .with_custom_headers(m.headers.clone()),
            ),
            "openai" => Arc::new(
                OpenAiProvider::new(m.base_url.clone()).with_custom_headers(m.headers.clone()),
            ),
            _ => Arc::new(
                CustomProvider::new(m.name.clone(), m.base_url.clone())
                    .with_custom_headers(m.headers.clone()),
            ),
        },
    }
}

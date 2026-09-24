//! Building an upstream from a stored provider row.
//!
//! Separate from `app::load_providers_into_router` because the router
//! build and the import-time protocol probe both need it, and they must
//! build it identically — a probe that talks to the upstream differently
//! from the live path proves nothing.

use std::sync::Arc;

use think_watch_gateway::proxy::transport::{Shape, Signer, Upstream};

use think_watch_common::models::Provider;

/// Everything needed to build a provider's upstream, decrypted once per
/// router rebuild.
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

/// Build the upstream for a provider.
///
/// One per provider, not one per dialect: a provider record can serve
/// several wire formats (an aggregator answering `anthropic.*` on
/// `/v1/messages` and everything else on `/v1/chat/completions`), but
/// the host and the credentials are the same for all of them. Which
/// format a request goes out in is decided per route.
pub(crate) fn build_upstream(m: &ProviderMaterials) -> Arc<Upstream> {
    let shape = match m.provider_type.as_str() {
        "azure_openai" => Shape::Azure {
            api_version: m
                .api_version
                .clone()
                .unwrap_or_else(|| AZURE_DEFAULT_API_VERSION.to_string()),
        },
        "bedrock" => {
            // `access_key:secret_key`, or empty for IMDSv2 — the instance
            // role then supplies rotating credentials.
            let (access_key_id, secret_access_key) = match m.bedrock_credentials.split_once(':') {
                Some((a, s)) if !a.is_empty() => (Some(a.to_string()), Some(s.to_string())),
                _ => (None, None),
            };
            Shape::Bedrock {
                signer: Arc::new(Signer {
                    // The provider row keeps the region in `base_url`.
                    region: m.base_url.clone(),
                    access_key_id,
                    secret_access_key,
                }),
            }
        }
        _ => Shape::Standard,
    };
    Arc::new(Upstream::new(
        &m.base_url,
        m.headers.clone(),
        shape,
        &m.name,
    ))
}

/// The API version an Azure deployment is addressed with when the
/// provider row names none.
const AZURE_DEFAULT_API_VERSION: &str = "2024-12-01-preview";

//! Building an upstream from a stored provider row.
//!
//! Separate from `app::load_providers_into_router` because the router
//! build and the import-time protocol probe both need it, and they must
//! build it identically — a probe that talks to the upstream differently
//! from the live path proves nothing.

use std::sync::Arc;

use think_watch_gateway::proxy::transport::{Credential, Shape, Signer, Upstream};

use think_watch_common::models::Provider;

/// Everything needed to build a provider's upstream, decrypted once per
/// router rebuild.
pub(crate) struct ProviderMaterials {
    pub(crate) name: String,
    pub(crate) provider_type: String,
    pub(crate) base_url: String,
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) api_version: Option<String>,
    /// What a Bedrock provider signs with: its access keys, the instance
    /// role (IMDSv2), or keys that cannot be used. A provider with a
    /// Bedrock API key (an `Authorization` header) has no keys and is
    /// never signed. Only read by the Bedrock adapter.
    pub(crate) aws: Credential,
}

impl ProviderMaterials {
    /// Decrypt a stored provider row into adapter inputs.
    ///
    /// Failures degrade rather than abort, keeping the gateway up with a
    /// degraded provider instead of refusing to boot: a header that won't
    /// decrypt is dropped (logged by `decrypt_headers_from_config`), and a
    /// Bedrock provider whose secret won't decrypt refuses its requests
    /// until the keys are saved again. It does not fall back to the
    /// instance role: that would sign with a different identity than the
    /// one configured.
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
            .map(|v| crate::handlers::providers::decrypt_secret_from_json(v, encryption_key))
            .transpose();
        // No access key id: the instance role signs (IMDSv2)
        let aws = match secret_key {
            Ok(secret) => Credential::from_keys(access_key, secret.unwrap_or_default()),
            Err(e) if !access_key.is_empty() => {
                tracing::error!(
                    provider = %provider.name,
                    "Failed to decrypt aws_secret_access_key — requests are refused until the keys are saved again: {e}"
                );
                Credential::Unusable(
                    "the stored secret access key could not be decrypted; save the provider's keys again"
                        .into(),
                )
            }
            Err(_) => Credential::InstanceRole,
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
            aws,
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
            // Without keys the instance role (IMDSv2) supplies rotating
            // credentials. A provider with a Bedrock API key in its
            // headers is never signed, whatever is set here.
            Shape::Bedrock {
                // The provider row keeps the region in `base_url`.
                signer: Arc::new(Signer::new(m.base_url.clone(), m.aws.clone())),
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

#[cfg(test)]
mod tests {
    use serde_json::json;
    use think_watch_common::json_secret::JsonSecret;

    use super::*;

    fn bedrock_row(config: serde_json::Value) -> Provider {
        Provider {
            id: uuid::Uuid::nil(),
            name: "br".into(),
            display_name: "br".into(),
            provider_type: "bedrock".into(),
            base_url: "us-east-1".into(),
            is_active: true,
            config_json: config,
            created_at: chrono::Utc::now(),
            deleted_at: None,
        }
    }

    #[test]
    fn a_secret_that_will_not_decrypt_refuses_instead_of_switching_identity() {
        let key = hex::encode([0u8; 32]);
        let row = |secret: serde_json::Value| {
            bedrock_row(json!({"aws_access_key_id": "AKIA", "aws_secret_access_key": secret}))
        };

        // Encrypted under another key: the instance role must not stand in
        let foreign = JsonSecret::encrypt("s3cret", &hex::encode([1u8; 32]))
            .unwrap()
            .to_json();
        let m = ProviderMaterials::from_provider(&row(foreign), &key);
        assert!(matches!(m.aws, Credential::Unusable(_)), "{:?}", m.aws);

        let ours = JsonSecret::encrypt("s3cret", &key).unwrap().to_json();
        let m = ProviderMaterials::from_provider(&row(ours), &key);
        assert_eq!(
            m.aws,
            Credential::Keys {
                access_key_id: "AKIA".into(),
                secret_access_key: "s3cret".into(),
            }
        );

        // No keys at all: the instance role, as before
        let m = ProviderMaterials::from_provider(&bedrock_row(json!({})), &key);
        assert_eq!(m.aws, Credential::InstanceRole);
    }
}

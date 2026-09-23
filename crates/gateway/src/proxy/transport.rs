//! Sending a converted request upstream.
//!
//! `tw-dialect` produces a [`Prepared`] — final bytes, a path, a query —
//! and this is where that goes out on the wire. Everything specific to a
//! vendor lives in [`Shape`]: how the URL is spelled and, for Bedrock,
//! what has to be signed.
//!
//! **The body is not touched here.** A converted body is already the
//! thing the upstream is meant to see, and for Bedrock it is the thing
//! the signature covers — change a byte after signing and the request is
//! rejected.

use std::sync::Arc;

use reqwest::RequestBuilder;
use tw_dialect::convert::Prepared;
use tw_upstream::sigv4::Signer;

use crate::providers::traits::{CallCtx, GatewayError};

/// How one upstream spells its URLs.
///
/// Most of them take the path the dialect produced. Two do not: Azure
/// puts the model in the path as a deployment and the API version in the
/// query, and Bedrock derives its host from a region and signs every
/// request.
pub enum Shape {
    /// `base_url` + whatever path the dialect produced.
    Standard,
    /// `{base}/openai/deployments/{deployment}/chat/completions?api-version=…`
    ///
    /// The deployment name is the upstream model: Azure has no model
    /// field in the body, it is addressed by URL.
    Azure { api_version: String },
    /// `https://bedrock-runtime.{region}.amazonaws.com{path}`, signed.
    ///
    /// The dialect already wrote `/model/{id}/converse[-stream]` into
    /// the path, so only the host is added here.
    Bedrock { signer: Arc<Signer> },
}

/// One upstream, ready to be sent to.
pub struct Upstream {
    pub client: reqwest::Client,
    pub base_url: String,
    /// Header templates from the provider row, `{{…}}` unresolved.
    pub headers: Vec<(String, String)>,
    pub shape: Shape,
}

impl Upstream {
    /// Build the request. Signing happens last, over the final bytes.
    pub async fn request(
        &self,
        prepared: &Prepared,
        ctx: &CallCtx,
    ) -> Result<RequestBuilder, GatewayError> {
        let url = self.url(prepared);

        let mut req = self
            .client
            .post(&url)
            .header("content-type", "application/json");

        for (k, v) in &self.headers {
            req = req.header(k, substitute(v, ctx));
        }
        if let Some(trace) = &ctx.trace_id {
            req = req.header("x-trace-id", trace.as_str());
        }

        if let Shape::Bedrock { signer } = &self.shape {
            // Last, and over `prepared.body` exactly as it will be sent.
            let signed = signer
                .sign(&self.client, &url, &prepared.body)
                .await
                .map_err(|e| GatewayError::ProviderError(e.to_string()))?;
            for (k, v) in signed {
                req = req.header(k, v);
            }
        }

        Ok(req.body(prepared.body.clone()))
    }

    fn url(&self, prepared: &Prepared) -> String {
        match &self.shape {
            Shape::Standard => {
                tw_upstream::upstream_url(&self.base_url, &prepared.path, prepared.query.as_deref())
            }
            Shape::Azure { api_version } => {
                // Azure ignores the dialect's path: the model is a
                // deployment in the URL, not a field in the body.
                let base = self.base_url.trim_end_matches('/');
                let deployment = deployment_of(prepared);
                format!(
                    "{base}/openai/deployments/{deployment}/chat/completions?api-version={api_version}"
                )
            }
            Shape::Bedrock { .. } => {
                let path = prepared.path.trim_start_matches('/');
                format!("https://bedrock-runtime.{}/{path}", self.base_url)
            }
        }
    }
}

/// Azure addresses a model by deployment name in the URL. The dialect
/// wrote the model into the body, so read it back out.
fn deployment_of(prepared: &Prepared) -> String {
    serde_json::from_slice::<serde_json::Value>(&prepared.body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_default()
}

/// Resolve `{{…}}` placeholders in a header value from the caller's
/// attributes. An absent key resolves to empty rather than staying
/// literal — an upstream that receives `X-User: {{user_id}}` is worse
/// than one that receives `X-User:`, because the literal looks like a
/// working config.
fn substitute(template: &str, ctx: &CallCtx) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find("}}") else {
            break;
        };
        let key = &rest[start + 2..start + end];
        out.push_str(ctx.attrs.get(key).map(String::as_str).unwrap_or_default());
        rest = &rest[start + end + 2..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(path: &str, body: &str) -> Prepared {
        // A Prepared is only ever built by the dialect layer, so reach
        // through it rather than fabricating the struct.
        let target = tw_dialect::ir::Target {
            dialect: tw_dialect::ir::Dialect::Chat,
            official: false,
            default_max_tokens: 1024,
        };
        let mut p = tw_dialect::convert::encode(
            &tw_dialect::ir::Request {
                model: "gpt-4o".into(),
                ..Default::default()
            },
            &target,
        );
        p.path = path.to_string();
        p.body = body.as_bytes().to_vec();
        p
    }

    fn up(base: &str, shape: Shape) -> Upstream {
        Upstream {
            client: reqwest::Client::new(),
            base_url: base.into(),
            headers: vec![],
            shape,
        }
    }

    #[test]
    fn a_standard_upstream_takes_the_path_the_dialect_produced() {
        let u = up("https://api.openai.com", Shape::Standard);
        assert_eq!(
            u.url(&prepared("/v1/chat/completions", "{}")),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn azure_addresses_the_model_by_deployment_in_the_url() {
        // Azure has no model field: the dialect's path is discarded and
        // the model becomes part of the URL
        let u = up(
            "https://x.openai.azure.com/",
            Shape::Azure {
                api_version: "2024-02-01".into(),
            },
        );
        assert_eq!(
            u.url(&prepared(
                "/v1/chat/completions",
                r#"{"model":"my-deploy"}"#
            )),
            "https://x.openai.azure.com/openai/deployments/my-deploy/chat/completions?api-version=2024-02-01"
        );
    }

    #[test]
    fn bedrock_only_adds_the_host_because_the_dialect_wrote_the_path() {
        let u = up(
            "us-east-1.amazonaws.com",
            Shape::Bedrock {
                signer: Arc::new(Signer {
                    region: "us-east-1".into(),
                    access_key_id: None,
                    secret_access_key: None,
                }),
            },
        );
        assert_eq!(
            u.url(&prepared("/model/anthropic.claude-v2/converse", "{}")),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-v2/converse"
        );
    }

    #[test]
    fn a_placeholder_with_no_value_resolves_to_nothing_not_to_itself() {
        // `X-User: {{user_id}}` reaching an upstream looks like a
        // working config; `X-User:` looks like what it is
        let ctx = CallCtx::new(None, None, None);
        assert_eq!(substitute("v={{missing}};", &ctx), "v=;");
    }

    #[test]
    fn a_placeholder_is_filled_from_the_caller() {
        let mut ctx = CallCtx::new(None, Some("u-1".into()), None);
        ctx.attrs.insert("user_id".into(), "u-1".into());
        assert_eq!(substitute("{{user_id}}/x", &ctx), "u-1/x");
    }
}

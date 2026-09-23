//! Sending a request upstream.
//!
//! What arrives here is already the bytes the upstream is meant to see —
//! either the caller's own body forwarded as-is, or one `tw-dialect`
//! converted. Everything vendor-specific lives in [`Shape`]: how the URL
//! is spelled and, for Bedrock, what gets signed.
//!
//! **The body is not touched here.** For Bedrock it is the thing the
//! signature covers — change a byte after signing and the request is
//! rejected.

use std::sync::Arc;
use std::time::Duration;

use tw_types::{CallCtx, GatewayError};
pub use tw_upstream::sigv4::Signer;

/// Sent to an Anthropic upstream when neither the caller nor the provider
/// row names a version. Without one the API refuses the request.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The HTTP client every upstream call goes through.
///
/// **Different from the desktop gateway on purpose.** Desktop sets no
/// overall timeout, because one user's six-minute task should not be cut
/// off by something in the middle. Here there are many tenants and a
/// stuck upstream pins a connection forever, so 300 seconds bounds it —
/// generous for a slow completion, final for a hung one.
///
/// Redirects are refused. `base_url` is typed in by an admin, and a
/// compromised provider answering `302 Location: http://169.254.169.254/`
/// would otherwise walk gateway traffic into the instance metadata
/// service.
pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(300))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("reqwest client builder cannot fail on stable inputs")
}

/// How one upstream spells its URLs.
pub enum Shape {
    /// `base_url` + the path the dialect produced.
    Standard,
    /// `{base}/openai/deployments/{deployment}/chat/completions?api-version=…`
    ///
    /// Azure addresses a model by deployment name in the URL; there is no
    /// model field it reads from the body.
    Azure { api_version: String },
    /// `https://bedrock-runtime.{region}.amazonaws.com` + the path, signed.
    ///
    /// The provider row keeps the region in `base_url`. The dialect
    /// already wrote `/model/{id}/converse[-stream]` into the path.
    Bedrock { signer: Arc<Signer> },
}

/// One upstream, ready to be sent to.
pub struct Upstream {
    pub client: reqwest::Client,
    /// Trailing slashes trimmed on construction — a pasted URL often
    /// carries one, and `https://host//v1/…` is a bare 404.
    pub base_url: String,
    /// Header templates from the provider row, `{{…}}` unresolved.
    pub headers: Vec<(String, String)>,
    pub shape: Shape,
    /// Shown in error messages, e.g. "Anthropic returned 500".
    pub label: String,
}

impl Upstream {
    pub fn new(base_url: &str, headers: Vec<(String, String)>, shape: Shape, label: &str) -> Self {
        Self {
            client: client(),
            base_url: base_url.trim_end_matches('/').to_string(),
            headers,
            shape,
            label: label.to_string(),
        }
    }

    /// Is this the vendor's own endpoint rather than a relay?
    ///
    /// The conversion layer needs to know: official endpoints are
    /// stricter about parameters (Anthropic's no longer accepts sampling
    /// knobs beyond `temperature`, OpenAI's reasoning models only take
    /// `max_completion_tokens`), while relays are usually lenient.
    pub fn is_official(&self) -> bool {
        match &self.shape {
            Shape::Bedrock { .. } | Shape::Azure { .. } => true,
            Shape::Standard => {
                let host = self
                    .base_url
                    .split("://")
                    .nth(1)
                    .unwrap_or(&self.base_url)
                    .split(['/', ':'])
                    .next()
                    .unwrap_or_default();
                matches!(
                    host,
                    "api.openai.com" | "api.anthropic.com" | "generativelanguage.googleapis.com"
                )
            }
        }
    }

    /// Send `body` to `path`, and turn a non-2xx answer into an error.
    ///
    /// Signing happens last, over the exact bytes being sent.
    pub async fn send(
        &self,
        body: Vec<u8>,
        path: &str,
        query: Option<&str>,
        dialect: tw_dialect::ir::Dialect,
        extra: &[(String, String)],
        ctx: &CallCtx,
    ) -> Result<reqwest::Response, GatewayError> {
        let url = self.url(&body, path, query);

        let mut req = self
            .client
            .post(&url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, tw_types::substitute_template(v, &ctx.attrs));
        }
        for (k, v) in extra {
            req = req.header(k, v);
        }
        // Anthropic refuses a request without a version header.
        if dialect == tw_dialect::ir::Dialect::Anthropic
            && !self
                .headers
                .iter()
                .chain(extra)
                .any(|(k, _)| k.eq_ignore_ascii_case("anthropic-version"))
        {
            req = req.header("anthropic-version", ANTHROPIC_VERSION);
        }
        if let Some(trace) = &ctx.trace_id {
            req = req.header("x-trace-id", trace.as_str());
        }
        if let Shape::Bedrock { signer } = &self.shape {
            let signed = signer
                .sign(&self.client, &url, &body)
                .await
                .map_err(|e| GatewayError::ProviderError(e.to_string()))?;
            for (k, v) in signed {
                req = req.header(k, v);
            }
        }

        let resp = req
            .body(body)
            .send()
            .await
            .map_err(|e| GatewayError::NetworkError(e.to_string()))?;
        check_status(resp, &self.label).await
    }

    fn url(&self, body: &[u8], path: &str, query: Option<&str>) -> String {
        match &self.shape {
            // Azure only reshapes chat completions; anything else it is
            // asked for goes where the dialect put it.
            Shape::Azure { api_version } if path.ends_with("/chat/completions") => {
                let deployment = model_in(body);
                format!(
                    "{}/openai/deployments/{deployment}/chat/completions?api-version={api_version}",
                    self.base_url
                )
            }
            Shape::Bedrock { signer } => format!(
                "https://bedrock-runtime.{}.amazonaws.com/{}",
                signer.region,
                path.trim_start_matches('/')
            ),
            _ => tw_upstream::upstream_url(&self.base_url, path, query),
        }
    }
}

/// Turn a non-2xx upstream answer into the error the caller sees.
///
/// 429 keeps the upstream's `Retry-After` so a client's retry policy does
/// not hammer the same quota window. 401/403 become an auth error. Any
/// other failure carries the upstream's body, **truncated**: error bodies
/// have carried stack traces, AWS account ids and full debug strings,
/// and forwarding them verbatim turns the gateway into a leak. The full
/// body goes to the log.
async fn check_status(
    resp: reqwest::Response,
    label: &str,
) -> Result<reqwest::Response, GatewayError> {
    let status = resp.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after_secs = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(tw_types::parse_retry_after_seconds);
        return Err(GatewayError::UpstreamRateLimited { retry_after_secs });
    }
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        return Err(GatewayError::UpstreamAuthError);
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!(provider = label, status = %status, body = %body, "upstream returned non-2xx");
        const CLIENT_MAX: usize = 512;
        let shown = if body.len() > CLIENT_MAX {
            // Char-boundary safe: provider errors are often not ASCII
            let mut end = CLIENT_MAX;
            while end > 0 && !body.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}…[truncated]", &body[..end])
        } else {
            body
        };
        return Err(GatewayError::ProviderError(format!(
            "{label} returned {status}: {shown}"
        )));
    }
    Ok(resp)
}

/// The model a converted chat body names — Azure needs it in the URL.
fn model_in(body: &[u8]) -> String {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("model").and_then(|m| m.as_str()).map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up(base: &str, shape: Shape) -> Upstream {
        Upstream::new(base, vec![], shape, "test")
    }

    #[test]
    fn a_standard_upstream_takes_the_path_the_dialect_produced() {
        let u = up("https://api.openai.com/", Shape::Standard);
        assert_eq!(
            u.url(b"{}", "/v1/chat/completions", None),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn a_query_is_carried_through() {
        let u = up("https://g.example", Shape::Standard);
        assert_eq!(
            u.url(
                b"{}",
                "/v1beta/models/m:streamGenerateContent",
                Some("alt=sse")
            ),
            "https://g.example/v1beta/models/m:streamGenerateContent?alt=sse"
        );
    }

    #[test]
    fn azure_addresses_the_model_by_deployment_in_the_url() {
        let u = up(
            "https://x.openai.azure.com/",
            Shape::Azure {
                api_version: "2024-02-01".into(),
            },
        );
        assert_eq!(
            u.url(br#"{"model":"my-deploy"}"#, "/v1/chat/completions", None),
            "https://x.openai.azure.com/openai/deployments/my-deploy/chat/completions?api-version=2024-02-01"
        );
    }

    #[test]
    fn bedrock_builds_its_host_from_the_region() {
        // The provider row keeps the region in base_url; the host is built from it.
        let u = up(
            "us-east-1",
            Shape::Bedrock {
                signer: Arc::new(Signer {
                    region: "us-east-1".into(),
                    access_key_id: None,
                    secret_access_key: None,
                }),
            },
        );
        assert_eq!(
            u.url(b"{}", "/model/anthropic.claude-v2/converse", None),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-v2/converse"
        );
    }
}

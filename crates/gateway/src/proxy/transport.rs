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

pub use crate::bedrock::sigv4::{Credential, Signer};
use crate::call_ctx::CallCtx;
use crate::error::GatewayError;

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
    /// `https://bedrock-runtime.{region}.amazonaws.com` + the path,
    /// SigV4-signed unless the provider sends a Bedrock API key.
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
    /// Names the upstream in error messages.
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
            Shape::Standard => tw_dialect::official::is_official_host(&self.base_url),
        }
    }

    /// Send `body` to `path`, and turn a non-2xx answer into an error.
    pub async fn send(
        &self,
        body: Vec<u8>,
        path: &str,
        query: Option<&str>,
        dialect: tw_dialect::ir::Dialect,
        extra: &[(String, String)],
        ctx: &CallCtx,
    ) -> Result<reqwest::Response, GatewayError> {
        let resp = self
            .request(body, path, query, dialect, extra, ctx)
            .await?
            .send()
            .await
            .map_err(transport_error)?;
        check_status(resp, &self.label).await
    }

    /// The request [`send`](Self::send) sends, built but not sent.
    ///
    /// Signing happens last, over the exact bytes being sent.
    async fn request(
        &self,
        body: Vec<u8>,
        path: &str,
        query: Option<&str>,
        dialect: tw_dialect::ir::Dialect,
        extra: &[(String, String)],
        ctx: &CallCtx,
    ) -> Result<reqwest::RequestBuilder, GatewayError> {
        let url = self.url(&body, path, query);

        let mut req = self
            .client
            .post(&url)
            .header("content-type", "application/json");
        for (k, v) in &self.headers {
            req = req.header(k, crate::call_ctx::substitute_template(v, &ctx.attrs));
        }
        for (k, v) in extra {
            req = req.header(k, v);
        }
        // Anthropic refuses a request without a version header. One the
        // provider row or the caller set wins.
        if dialect == tw_dialect::ir::Dialect::Anthropic
            && !self
                .headers
                .iter()
                .chain(extra)
                .any(|(k, _)| k.eq_ignore_ascii_case("anthropic-version"))
        {
            req = req.header("anthropic-version", tw_dialect::official::ANTHROPIC_VERSION);
        }
        if let Some(trace) = &ctx.trace_id {
            req = req.header("x-trace-id", trace.as_str());
        }
        if let Some(signer) = self.signer() {
            let signed = signer
                .sign(&self.client, &reqwest::Method::POST, &url, Some(&body))
                .await
                .map_err(|e| GatewayError::ProviderError(e.to_string()))?;
            for (k, v) in signed {
                req = req.header(k, v);
            }
        }

        Ok(req.body(body))
    }

    /// Who signs this upstream's requests, if anyone does.
    ///
    /// Bedrock takes two kinds of credential. A Bedrock API key is a
    /// bearer token the provider row sends in its own `Authorization`
    /// header: it is the whole credential, there is nothing to sign, and
    /// signing anyway would add a second `authorization` header, which
    /// AWS rejects. Without one, every request is SigV4-signed — those
    /// to the region's control plane too, such as its model listings.
    pub fn signer(&self) -> Option<&Signer> {
        match &self.shape {
            Shape::Bedrock { signer }
                if !tw_bedrock::carries_api_key(self.headers.iter().map(|(k, _)| k.as_str())) =>
            {
                Some(signer)
            }
            _ => None,
        }
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
            // The model id goes into the path escaped: an ARN's `/` would
            // otherwise add a path segment.
            Shape::Bedrock { signer } => tw_bedrock::endpoint::runtime_url(
                &tw_bedrock::endpoint::runtime_base(&signer.region),
                path,
            ),
            _ => tw_dialect::url::upstream_url(&self.base_url, path, query),
        }
    }
}

/// A request that never got an answer: the upstream timed out, or the
/// connection could not be made or broke. Either way it says nothing
/// about the request, and another route may well answer it.
pub(crate) fn transport_error(e: reqwest::Error) -> GatewayError {
    if e.is_timeout() {
        GatewayError::ProviderTimeout(e.to_string())
    } else {
        GatewayError::NetworkError(e.to_string())
    }
}

/// Turn a non-2xx upstream answer into the error the caller sees.
///
/// 429 keeps the upstream's `Retry-After` so a client's retry policy does
/// not hammer the same quota window. 401/403 become an auth error: the
/// gateway's own credential for this upstream was refused, which is
/// about the route, not the caller.
///
/// Every other status is kept as it is, in `ProviderHttpError`. Whether
/// it is the upstream failing (5xx, 408) or the upstream refusing this
/// request (any other 4xx) decides failover and the circuit breaker —
/// see `routing::is_upstream_failure`.
///
/// The upstream's body goes to the caller **truncated**: error bodies
/// have carried stack traces, AWS account ids and full debug strings,
/// and forwarding them verbatim turns the gateway into a leak. An auth
/// error keeps its truncated body too, but not in the text the caller
/// sees. The full body goes to the log.
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
            .and_then(crate::error::parse_retry_after_seconds);
        return Err(GatewayError::UpstreamRateLimited { retry_after_secs });
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
        let (status, message) = (status.as_u16(), format!("{label}: {shown}"));
        return Err(if status == 401 || status == 403 {
            GatewayError::UpstreamAuthError { status, message }
        } else {
            GatewayError::ProviderHttpError { status, message }
        });
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
    fn only_the_vendors_own_host_counts_as_official() {
        assert!(up("https://api.anthropic.com/", Shape::Standard).is_official());
        assert!(up("https://api.deepseek.com/anthropic", Shape::Standard).is_official());
        // A relay cannot dress up as the vendor through its path or user info.
        assert!(!up("https://relay.example/api.openai.com", Shape::Standard).is_official());
        assert!(!up("https://api.openai.com@relay.example", Shape::Standard).is_official());
        let azure = Shape::Azure {
            api_version: "2024-02-01".into(),
        };
        assert!(up("https://x.openai.azure.com", azure).is_official());
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
                signer: Arc::new(Signer::new("us-east-1", Credential::InstanceRole)),
            },
        );
        assert_eq!(
            u.url(b"{}", "/model/anthropic.claude-v2/converse", None),
            "https://bedrock-runtime.us-east-1.amazonaws.com/model/anthropic.claude-v2/converse"
        );
    }

    #[test]
    fn a_bedrock_arn_keeps_its_slash_inside_the_model_segment() {
        let u = up(
            "us-east-2",
            Shape::Bedrock {
                signer: Arc::new(Signer::new("us-east-2", Credential::InstanceRole)),
            },
        );
        assert_eq!(
            u.url(
                b"{}",
                "/model/arn:aws:bedrock:us-east-2:123456789012:application-inference-profile/a1b2/converse",
                None
            ),
            "https://bedrock-runtime.us-east-2.amazonaws.com/model/\
             arn:aws:bedrock:us-east-2:123456789012:application-inference-profile%2Fa1b2/converse"
        );
    }

    fn answer(status: u16, body: &str) -> reqwest::Response {
        http_1x::Response::builder()
            .status(status)
            .body(body.to_string())
            .unwrap()
            .into()
    }

    #[tokio::test]
    async fn a_refused_credential_keeps_the_upstreams_reason_out_of_the_callers_sight() {
        let body =
            r#"{"message":"You don't have access to the model with the specified model ID."}"#;
        let err = check_status(answer(403, body), "bedrock")
            .await
            .unwrap_err();

        let GatewayError::UpstreamAuthError { status, message } = &err else {
            panic!("{err:?}");
        };
        assert_eq!(*status, 403);
        assert!(
            message.contains("You don't have access to the model"),
            "{message}"
        );
        // The caller is told the gateway's own words: the reason names
        // the AWS account
        assert_eq!(err.to_string(), "Authentication failed with upstream");
        assert_eq!(err.status_code(), 401);
    }

    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";

    fn bedrock(headers: &[(&str, &str)], keys: Option<(&str, &str)>) -> Upstream {
        Upstream::new(
            "us-east-1",
            headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            Shape::Bedrock {
                signer: Arc::new(Signer::new(
                    "us-east-1",
                    keys.map_or(Credential::InstanceRole, |(ak, sk)| {
                        Credential::from_keys(ak.into(), sk.into())
                    }),
                )),
            },
            "test",
        )
    }

    /// The `authorization` headers the request goes out with, and
    /// whether SigV4 touched it.
    async fn auth_headers(u: &Upstream) -> (Vec<String>, bool) {
        let req = u
            .request(
                b"{}".to_vec(),
                "/model/m/converse",
                None,
                tw_dialect::ir::Dialect::Bedrock,
                &[],
                &CallCtx::default(),
            )
            .await
            .unwrap()
            .build()
            .unwrap();
        let auth = req
            .headers()
            .get_all("authorization")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        (auth, req.headers().contains_key("x-amz-date"))
    }

    #[tokio::test]
    async fn a_bedrock_api_key_goes_out_as_it_is_and_nothing_is_signed() {
        // No access keys: signing would have gone to IMDS for credentials
        let u = bedrock(&[("Authorization", "Bearer ABSK-test")], None);
        let (auth, signed) = auth_headers(&u).await;
        assert_eq!(auth, ["Bearer ABSK-test"]);
        assert!(!signed);
    }

    #[tokio::test]
    async fn an_api_key_wins_over_access_keys() {
        // Signing on top would add a second `authorization`, which AWS
        // rejects. The header name matches in any case.
        let u = bedrock(&[("authorization", "Bearer ABSK-test")], Some((AK, SK)));
        let (auth, signed) = auth_headers(&u).await;
        assert_eq!(auth, ["Bearer ABSK-test"]);
        assert!(!signed);
    }

    #[tokio::test]
    async fn bedrock_without_an_api_key_is_signed() {
        let u = bedrock(&[("x-custom", "1")], Some((AK, SK)));
        let (auth, signed) = auth_headers(&u).await;
        assert_eq!(auth.len(), 1, "{auth:?}");
        assert!(auth[0].starts_with("AWS4-HMAC-SHA256 "), "{auth:?}");
        assert!(signed);
    }
}

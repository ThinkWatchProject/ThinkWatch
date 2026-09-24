//! AWS SigV4 signing.
//!
//! Bedrock is the one upstream that does not take a bearer token: every
//! request is signed over its method, URL, time and the hash of its body.
//! So **signing has to happen after the body is final** — change one byte
//! and the signature no longer matches.

use std::time::SystemTime;

use aws_credential_types::Credentials;

/// What went wrong while signing.
#[derive(Debug)]
pub enum SignError {
    /// No credentials: none configured, and IMDSv2 did not answer
    Credentials(String),
    /// Signing itself failed
    Signing(String),
}

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignError::Credentials(m) => write!(f, "AWS credentials are unavailable: {m}"),
            SignError::Signing(m) => write!(f, "SigV4 signing failed: {m}"),
        }
    }
}

impl std::error::Error for SignError {}

/// The signing identity of one Bedrock upstream.
///
/// Without keys the credentials come from EC2 instance metadata (IMDSv2) at
/// call time — the way a deployment inside AWS should work: the instance role
/// hands them out, they rotate on their own, and they never sit in config.
pub struct Signer {
    pub region: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
}

/// The IMDS address. **Link-local** — only answers from inside EC2.
const IMDS: &str = "http://169.254.169.254";

impl Signer {
    /// Sign a request and return the headers to add (`authorization` and
    /// `x-amz-*`).
    ///
    /// `body` must be **exactly what will be sent**: the signature covers its hash.
    pub async fn sign(
        &self,
        client: &reqwest::Client,
        url: &str,
        body: &[u8],
    ) -> Result<Vec<(String, String)>, SignError> {
        use aws_sigv4::http_request::{
            PayloadChecksumKind, SignableBody, SignableRequest, SignatureLocation, SigningSettings,
            sign,
        };
        use aws_sigv4::sign::v4;

        let credentials = match (&self.access_key_id, &self.secret_access_key) {
            (Some(ak), Some(sk)) => Credentials::new(ak, sk, None, None, "think-watch"),
            _ => self.imdsv2_credentials(client).await?,
        };

        let identity = credentials.into();
        let mut settings = SigningSettings::default();
        settings.payload_checksum_kind = PayloadChecksumKind::XAmzSha256;
        settings.signature_location = SignatureLocation::Headers;

        let params = v4::SigningParams::builder()
            .identity(&identity)
            .region(&self.region)
            .name("bedrock")
            .time(SystemTime::now())
            .settings(settings)
            .build()
            .map_err(|e| SignError::Signing(e.to_string()))?;

        let signable = SignableRequest::new(
            "POST",
            url,
            std::iter::once(("content-type", "application/json")),
            SignableBody::Bytes(body),
        )
        .map_err(|e| SignError::Signing(e.to_string()))?;

        let (instructions, _signature) = sign(signable, &params.into())
            .map_err(|e| SignError::Signing(e.to_string()))?
            .into_parts();

        // The signing library only writes onto an http request, so build an empty one to catch it
        let mut req = http_1x::Request::builder()
            .method("POST")
            .uri(url)
            .header("content-type", "application/json")
            .body(())
            .map_err(|e| SignError::Signing(e.to_string()))?;
        instructions.apply_to_request_http1x(&mut req);

        // Only the signed ones. **The other headers belong to the caller** —
        // returning all of them would overwrite what it set itself
        Ok(req
            .headers()
            .iter()
            .filter(|(n, _)| {
                let n = n.as_str();
                n == "authorization" || n.starts_with("x-amz-")
            })
            .map(|(n, v)| (n.to_string(), v.to_str().unwrap_or_default().to_string()))
            .collect())
    }

    /// Fetch temporary credentials from EC2 instance metadata.
    ///
    /// IMDSv2 takes three steps: a short-lived token, then the role name, then
    /// the credentials for that role. v1 answers in one step, which is exactly
    /// why an app with an SSRF hole leaks them — the v2 token needs a PUT, and
    /// an SSRF usually only gets to send GETs.
    async fn imdsv2_credentials(&self, client: &reqwest::Client) -> Result<Credentials, SignError> {
        let fail = |what: &str, e: reqwest::Error| SignError::Credentials(format!("{what}: {e}"));

        let token = client
            .put(format!("{IMDS}/latest/api/token"))
            .header("X-aws-ec2-metadata-token-ttl-seconds", "300")
            .send()
            .await
            .map_err(|e| fail("IMDSv2 token request", e))?
            .text()
            .await
            .map_err(|e| fail("IMDSv2 token read", e))?;

        let role = client
            .get(format!("{IMDS}/latest/meta-data/iam/security-credentials/"))
            .header("X-aws-ec2-metadata-token", &token)
            .send()
            .await
            .map_err(|e| fail("IMDSv2 role lookup", e))?
            .text()
            .await
            .map_err(|e| fail("IMDSv2 role read", e))?;
        let role = role.trim();

        let creds: serde_json::Value = client
            .get(format!(
                "{IMDS}/latest/meta-data/iam/security-credentials/{role}"
            ))
            .header("X-aws-ec2-metadata-token", &token)
            .send()
            .await
            .map_err(|e| fail("IMDSv2 credentials fetch", e))?
            .json()
            .await
            .map_err(|e| fail("IMDSv2 credentials parse", e))?;

        let field = |k: &str| {
            creds[k]
                .as_str()
                .ok_or_else(|| SignError::Credentials(format!("IMDSv2 response has no {k}")))
        };
        Ok(Credentials::new(
            field("AccessKeyId")?,
            field("SecretAccessKey")?,
            creds["Token"].as_str().map(str::to_string),
            None,
            "imdsv2",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> Signer {
        Signer {
            region: "us-east-1".into(),
            access_key_id: Some("AKIAIOSFODNN7EXAMPLE".into()),
            secret_access_key: Some("wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into()),
        }
    }

    #[tokio::test]
    async fn signing_produces_an_authorization_header_and_the_payload_hash() {
        let headers = signer()
            .sign(
                &reqwest::Client::new(),
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
                b"{}",
            )
            .await
            .expect("the keys are configured, so IMDS must not be asked");

        let names: Vec<&str> = headers.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"authorization"), "{names:?}");
        assert!(
            names.contains(&"x-amz-content-sha256"),
            "the signature covers the body hash, so this header must be there: {names:?}"
        );
        assert!(names.contains(&"x-amz-date"), "{names:?}");
    }

    #[tokio::test]
    async fn nothing_but_the_signed_headers_comes_back() {
        // Returning every header would overwrite what the caller set itself
        let headers = signer()
            .sign(
                &reqwest::Client::new(),
                "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse",
                b"{}",
            )
            .await
            .unwrap();
        for (n, _) in &headers {
            assert!(
                n == "authorization" || n.starts_with("x-amz-"),
                "{n} was not produced by signing"
            );
        }
    }

    #[tokio::test]
    async fn a_different_body_signs_differently() {
        // The signature covers the body — one changed byte must sign
        // differently, or a replayed request with an edited body would pass
        let c = reqwest::Client::new();
        let url = "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse";
        let a = signer().sign(&c, url, b"{}").await.unwrap();
        let b = signer().sign(&c, url, b"{\"x\":1}").await.unwrap();

        let hash = |h: &[(String, String)]| {
            h.iter()
                .find(|(n, _)| n == "x-amz-content-sha256")
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_ne!(hash(&a), hash(&b));
    }
}

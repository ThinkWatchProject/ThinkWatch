//! AWS SigV4 signing.
//!
//! A Bedrock provider without an API key has no bearer token to send:
//! every request is signed over its method, URL, time and the hash of its
//! body. So **signing has to happen after the body is final** — change one
//! byte and the signature no longer matches.

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
    /// `body` is the request's JSON body, which must be **exactly what will
    /// be sent**: the signature covers its hash, and its `content-type`.
    /// `None` is a request without one, such as a GET — then the signature
    /// covers the hash of nothing, and there is no content type to sign.
    /// The signature covers the method and the whole URL, query included.
    pub async fn sign(
        &self,
        client: &reqwest::Client,
        method: &reqwest::Method,
        url: &str,
        body: Option<&[u8]>,
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

        let content_type = body.map(|_| ("content-type", "application/json"));
        let signable = SignableRequest::new(
            method.as_str(),
            url,
            content_type.into_iter(),
            SignableBody::Bytes(body.unwrap_or_default()),
        )
        .map_err(|e| SignError::Signing(e.to_string()))?;

        let (instructions, _signature) = sign(signable, &params.into())
            .map_err(|e| SignError::Signing(e.to_string()))?
            .into_parts();

        // The signing library only writes onto an http request, so build an empty one to catch it
        let mut req = http_1x::Request::builder().method(method.as_str()).uri(url);
        if let Some((name, value)) = content_type {
            req = req.header(name, value);
        }
        let mut req = req
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

    const CONVERSE: &str = "https://bedrock-runtime.us-east-1.amazonaws.com/model/m/converse";

    fn header<'a>(headers: &'a [(String, String)], name: &str) -> &'a str {
        headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("no {name} in {headers:?}"))
    }

    /// The `SignedHeaders=` list of an `authorization` header.
    fn signed_headers(headers: &[(String, String)]) -> &str {
        let auth = header(headers, "authorization");
        auth.split("SignedHeaders=")
            .nth(1)
            .and_then(|rest| rest.split(',').next())
            .unwrap_or_else(|| panic!("no SignedHeaders in {auth}"))
    }

    #[tokio::test]
    async fn signing_produces_an_authorization_header_and_the_payload_hash() {
        let headers = signer()
            .sign(
                &reqwest::Client::new(),
                &reqwest::Method::POST,
                CONVERSE,
                Some(b"{}"),
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
                &reqwest::Method::POST,
                CONVERSE,
                Some(b"{}"),
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
        let post = reqwest::Method::POST;
        let a = signer()
            .sign(&c, &post, CONVERSE, Some(b"{}"))
            .await
            .unwrap();
        let b = signer()
            .sign(&c, &post, CONVERSE, Some(b"{\"x\":1}"))
            .await
            .unwrap();

        assert_ne!(
            header(&a, "x-amz-content-sha256"),
            header(&b, "x-amz-content-sha256")
        );
    }

    #[tokio::test]
    async fn a_json_body_signs_its_content_type() {
        let headers = signer()
            .sign(
                &reqwest::Client::new(),
                &reqwest::Method::POST,
                CONVERSE,
                Some(b"{}"),
            )
            .await
            .unwrap();
        assert_eq!(
            signed_headers(&headers),
            "content-type;host;x-amz-content-sha256;x-amz-date"
        );
    }

    #[tokio::test]
    async fn a_get_signs_an_empty_payload_and_no_content_type() {
        // A GET sends neither a body nor a content type, so signing one
        // would describe a request that is never sent
        let headers = signer()
            .sign(
                &reqwest::Client::new(),
                &reqwest::Method::GET,
                "https://bedrock.us-east-1.amazonaws.com/inference-profiles?type=SYSTEM_DEFINED",
                None,
            )
            .await
            .expect("the keys are configured, so IMDS must not be asked");

        assert_eq!(
            header(&headers, "x-amz-content-sha256"),
            // SHA-256 of zero bytes
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            signed_headers(&headers),
            "host;x-amz-content-sha256;x-amz-date"
        );
    }
}

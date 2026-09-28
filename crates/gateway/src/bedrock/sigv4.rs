//! AWS SigV4 signing.
//!
//! A Bedrock provider without an API key has no bearer token to send:
//! every request is signed over its method, URL, time and the hash of its
//! body. So **signing has to happen after the body is final** — change one
//! byte and the signature no longer matches.

use std::time::{Duration, SystemTime};

use aws_credential_types::Credentials;

/// What went wrong while signing.
#[derive(Debug)]
pub enum SignError {
    /// No usable credentials: the stored ones cannot be used, or IMDSv2
    /// did not answer
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

/// Where the signing credentials of one Bedrock upstream come from.
#[derive(Clone, PartialEq)]
pub enum Credential {
    /// Access keys stored on the provider row
    Keys {
        access_key_id: String,
        secret_access_key: String,
    },
    /// The EC2 instance role, through IMDSv2 — the way a deployment inside
    /// AWS should work: the role hands the credentials out, they rotate on
    /// their own, and they never sit in config.
    InstanceRole,
    /// Keys are stored but cannot be used, for the reason given. **Requests
    /// are refused**: signing with an empty secret only earns a
    /// `SignatureDoesNotMatch` from AWS, and falling back to the instance
    /// role would send them under a different identity than the one the
    /// provider was configured with.
    Unusable(String),
}

impl Credential {
    /// The credential a pair of stored keys stands for. No access key ID
    /// means no keys: the instance role signs.
    pub fn from_keys(access_key_id: String, secret_access_key: String) -> Self {
        if access_key_id.is_empty() {
            Credential::InstanceRole
        } else if secret_access_key.is_empty() {
            Credential::Unusable("the access key ID has no secret access key".into())
        } else {
            Credential::Keys {
                access_key_id,
                secret_access_key,
            }
        }
    }
}

/// **Never prints the secret.** Providers and signers end up in logs.
impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Credential::Keys { access_key_id, .. } => f
                .debug_struct("Keys")
                .field("access_key_id", access_key_id)
                .finish_non_exhaustive(),
            Credential::InstanceRole => f.write_str("InstanceRole"),
            Credential::Unusable(why) => f.debug_tuple("Unusable").field(why).finish(),
        }
    }
}

/// The signing identity of one Bedrock upstream.
pub struct Signer {
    pub region: String,
    pub credential: Credential,
    /// The instance role's credentials, kept until shortly before they
    /// expire. Behind an async lock so that a burst of requests at expiry
    /// makes one trip to IMDS, not one each.
    lease: tokio::sync::Mutex<Option<Lease>>,
    /// Where IMDS answers. Only tests point it anywhere else
    imds: String,
}

/// Instance-role credentials and when they stop working.
struct Lease {
    credentials: Credentials,
    expires: SystemTime,
}

/// The IMDS address. **Link-local** — only answers from inside EC2.
const IMDS: &str = "http://169.254.169.254";

/// Fetch new instance-role credentials this long before the old ones
/// expire. IMDS itself hands out new ones five minutes ahead.
const RENEW_BEFORE: Duration = Duration::from_secs(300);

impl Signer {
    pub fn new(region: impl Into<String>, credential: Credential) -> Self {
        Self {
            region: region.into(),
            credential,
            lease: Default::default(),
            imds: IMDS.to_string(),
        }
    }

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

        let credentials = self.credentials(client).await?;

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

    /// The credentials this upstream signs with.
    pub async fn credentials(&self, client: &reqwest::Client) -> Result<Credentials, SignError> {
        match &self.credential {
            Credential::Keys {
                access_key_id,
                secret_access_key,
            } => Ok(Credentials::new(
                access_key_id,
                secret_access_key,
                None,
                None,
                "think-watch",
            )),
            Credential::Unusable(why) => Err(SignError::Credentials(why.clone())),
            Credential::InstanceRole => {
                let mut lease = self.lease.lock().await;
                if let Some(l) = lease
                    .as_ref()
                    .filter(|l| SystemTime::now() + RENEW_BEFORE < l.expires)
                {
                    return Ok(l.credentials.clone());
                }
                let (credentials, expires) = self.imdsv2_credentials(client).await?;
                // Without an expiry there is nothing to keep them by: ask again next time
                *lease = expires.map(|expires| Lease {
                    credentials: credentials.clone(),
                    expires,
                });
                Ok(credentials)
            }
        }
    }

    /// Fetch temporary credentials from EC2 instance metadata, and when
    /// they expire.
    ///
    /// IMDSv2 takes three steps: a short-lived token, then the role name, then
    /// the credentials for that role. v1 answers in one step, which is exactly
    /// why an app with an SSRF hole leaks them — the v2 token needs a PUT, and
    /// an SSRF usually only gets to send GETs.
    ///
    /// **Every step checks its status.** An error page read as a token or a
    /// role name turns into a confusing failure two steps later.
    async fn imdsv2_credentials(
        &self,
        client: &reqwest::Client,
    ) -> Result<(Credentials, Option<SystemTime>), SignError> {
        let fail = |what: &str, e: reqwest::Error| SignError::Credentials(format!("{what}: {e}"));
        let imds = &self.imds;

        let token = client
            .put(format!("{imds}/latest/api/token"))
            .header("X-aws-ec2-metadata-token-ttl-seconds", "300")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| fail("IMDSv2 token request", e))?
            .text()
            .await
            .map_err(|e| fail("IMDSv2 token read", e))?;

        let role = client
            .get(format!("{imds}/latest/meta-data/iam/security-credentials/"))
            .header("X-aws-ec2-metadata-token", &token)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| fail("IMDSv2 role lookup", e))?
            .text()
            .await
            .map_err(|e| fail("IMDSv2 role read", e))?;
        // One role per instance profile; the listing is one name per line
        let role = role.lines().next().unwrap_or_default().trim();
        if role.is_empty() {
            return Err(SignError::Credentials(
                "IMDSv2 names no role: the instance has no instance profile".into(),
            ));
        }

        let creds: serde_json::Value = client
            .get(format!(
                "{imds}/latest/meta-data/iam/security-credentials/{role}"
            ))
            .header("X-aws-ec2-metadata-token", &token)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|e| fail("IMDSv2 credentials fetch", e))?
            .json()
            .await
            .map_err(|e| fail("IMDSv2 credentials parse", e))?;

        let field = |k: &str| {
            creds[k]
                .as_str()
                .ok_or_else(|| SignError::Credentials(format!("IMDSv2 response has no {k}")))
        };
        let expires = creds["Expiration"]
            .as_str()
            .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
            .map(SystemTime::from);
        Ok((
            Credentials::new(
                field("AccessKeyId")?,
                field("SecretAccessKey")?,
                creds["Token"].as_str().map(str::to_string),
                None,
                "imdsv2",
            ),
            expires,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signer() -> Signer {
        Signer::new(
            "us-east-1",
            Credential::Keys {
                access_key_id: "AKIAIOSFODNN7EXAMPLE".into(),
                secret_access_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
            },
        )
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

    /// A fake IMDS: a role whose credentials expire `lasts` from now. Counts
    /// the token requests, one per trip to IMDS.
    async fn imds(token_status: u16, lasts: Duration) -> (String, Arc<AtomicUsize>) {
        use axum::routing::{get, put};
        let trips = Arc::new(AtomicUsize::new(0));
        let counted = trips.clone();
        let app = axum::Router::new()
            .route(
                "/latest/api/token",
                put(move || {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        (
                            axum::http::StatusCode::from_u16(token_status).unwrap(),
                            "tok",
                        )
                    }
                }),
            )
            .route(
                "/latest/meta-data/iam/security-credentials/",
                get(|| async { "dev-role\n" }),
            )
            .route(
                "/latest/meta-data/iam/security-credentials/dev-role",
                get(move || async move {
                    let expires = chrono::Utc::now() + lasts;
                    axum::Json(serde_json::json!({
                        "AccessKeyId": "ASIAROLE",
                        "SecretAccessKey": "role-secret",
                        "Token": "role-token",
                        "Expiration": expires.to_rfc3339(),
                    }))
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        (format!("http://{addr}"), trips)
    }

    fn role_signer(imds: String) -> Signer {
        let mut s = Signer::new("us-east-1", Credential::InstanceRole);
        s.imds = imds;
        s
    }

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn the_instance_roles_credentials_are_kept_until_shortly_before_they_expire() {
        let (url, trips) = imds(200, Duration::from_secs(3600)).await;
        let s = role_signer(url);
        let c = reqwest::Client::new();
        for _ in 0..3 {
            let got = s.credentials(&c).await.unwrap();
            assert_eq!(got.access_key_id(), "ASIAROLE");
            assert_eq!(got.session_token(), Some("role-token"));
        }
        assert_eq!(
            trips.load(Ordering::SeqCst),
            1,
            "one trip to IMDS, not one per request"
        );
    }

    #[tokio::test]
    async fn credentials_about_to_expire_are_fetched_again() {
        let (url, trips) = imds(200, Duration::from_secs(120)).await;
        let s = role_signer(url);
        let c = reqwest::Client::new();
        s.credentials(&c).await.unwrap();
        s.credentials(&c).await.unwrap();
        assert_eq!(trips.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_imds_refusal_says_which_step_failed() {
        let (url, _) = imds(401, Duration::from_secs(3600)).await;
        let err = role_signer(url)
            .credentials(&reqwest::Client::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("IMDSv2 token request"), "{err}");
        assert!(err.contains("401"), "{err}");
    }

    #[tokio::test]
    async fn unusable_keys_are_refused_without_asking_anyone() {
        // IMDS points nowhere: asking it would fail differently
        let s = Signer {
            imds: "http://127.0.0.1:9".into(),
            ..Signer::new(
                "us-east-1",
                Credential::Unusable("the stored secret access key could not be decrypted".into()),
            )
        };
        let err = s
            .sign(
                &reqwest::Client::new(),
                &reqwest::Method::POST,
                CONVERSE,
                Some(b"{}"),
            )
            .await
            .unwrap_err()
            .to_string();
        assert_eq!(
            err,
            "AWS credentials are unavailable: the stored secret access key could not be decrypted"
        );
    }

    #[test]
    fn stored_keys_become_a_credential() {
        assert_eq!(
            Credential::from_keys(String::new(), "anything".into()),
            Credential::InstanceRole
        );
        assert!(matches!(
            Credential::from_keys("AKIA".into(), String::new()),
            Credential::Unusable(_)
        ));
        assert!(matches!(
            Credential::from_keys("AKIA".into(), "s".into()),
            Credential::Keys { .. }
        ));
    }

    #[test]
    fn debug_output_leaves_the_secret_out() {
        let shown = format!("{:?}", signer().credential);
        assert!(shown.contains("AKIAIOSFODNN7EXAMPLE"), "{shown}");
        assert!(!shown.contains("wJalr"), "{shown}");
    }
}

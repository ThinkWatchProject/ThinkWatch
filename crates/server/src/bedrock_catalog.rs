//! The models a Bedrock provider can be routed to.
//!
//! Bedrock's list of foundation models is not that list on its own. It
//! names base model ids, and most current models — Claude 3.7 and later,
//! among others — are not served under their base id at all: they are
//! invoked through an inference profile such as
//! `us.anthropic.claude-sonnet-4-5-20250929-v1:0`, which spreads the calls
//! over several regions. So the catalog is the union of two listings on the
//! region's control plane, `bedrock.{region}.amazonaws.com`:
//!
//! - the foundation models that can be invoked on demand and answer in
//!   text (`ListFoundationModels`), and
//! - the inference profiles AWS defines (`ListInferenceProfiles`).
//!
//! Neither says whether *this* account may call a model, or over which API.
//! The protocol probe settles that, one model at a time, when it is
//! imported.

use std::collections::BTreeSet;

use serde_json::Value;
use think_watch_common::errors::AppError;
use think_watch_gateway::proxy::transport::Upstream;

/// The most pages of inference profiles one listing reads. A page holds up
/// to 1000 and a region has a few hundred profiles at most, so only a
/// listing that never ends gets here.
const MAX_PROFILE_PAGES: usize = 10;

/// The region's control plane, where Bedrock lists its models.
///
/// The host is built from the region, so nothing but a region is accepted.
pub(crate) fn endpoint(region: &str) -> Result<String, AppError> {
    think_watch_common::validation::validate_aws_region(region)?;
    Ok(format!("https://bedrock.{region}.amazonaws.com"))
}

/// Why the catalog could not be listed.
#[derive(Debug, PartialEq)]
pub(crate) enum Failure {
    /// AWS answered, and refused.
    Status { status: u16, message: String },
    /// Nothing to read: the request could not be signed or sent, or what
    /// came back was not a listing.
    Request(String),
}

/// Every model id `upstream` can be routed to, sorted.
///
/// `endpoint` is [`endpoint`] for the provider's region. Requests are
/// authenticated as the gateway authenticates the provider's traffic: its
/// own headers, signed unless one of them is a Bedrock API key.
///
/// Both listings have to succeed. Base ids alone would offer exactly the
/// ids that most current models are not served under.
pub(crate) async fn list_models(
    client: &reqwest::Client,
    endpoint: &str,
    upstream: &Upstream,
) -> Result<Vec<String>, Failure> {
    let (models, profiles) = tokio::try_join!(
        foundation_models(client, endpoint, upstream),
        inference_profiles(client, endpoint, upstream),
    )?;
    let ids: BTreeSet<String> = models.into_iter().chain(profiles).collect();
    Ok(ids.into_iter().collect())
}

/// Does the control plane accept `upstream`'s credential? One small
/// listing: a refusal of the credential shows up here too, a refusal of
/// one model doesn't. See `protocol_probe::is_credential_good`.
pub(crate) async fn accepts_credential(
    client: &reqwest::Client,
    endpoint: &str,
    upstream: &Upstream,
) -> bool {
    let Ok(url) = listing_url(
        endpoint,
        "inference-profiles",
        &[("type", "SYSTEM_DEFINED"), ("maxResults", "1")],
    ) else {
        return false;
    };
    get(client, upstream, url).await.is_ok()
}

async fn foundation_models(
    client: &reqwest::Client,
    endpoint: &str,
    upstream: &Upstream,
) -> Result<Vec<String>, Failure> {
    let url = listing_url(
        endpoint,
        "foundation-models",
        &[
            ("byInferenceType", "ON_DEMAND"),
            ("byOutputModality", "TEXT"),
        ],
    )?;
    let listing = get(client, upstream, url).await?;
    ids(&listing, "modelSummaries", "modelId")
}

async fn inference_profiles(
    client: &reqwest::Client,
    endpoint: &str,
    upstream: &Upstream,
) -> Result<Vec<String>, Failure> {
    let mut profiles = Vec::new();
    let mut next_token: Option<String> = None;
    for _ in 0..MAX_PROFILE_PAGES {
        // `type` is the API's `typeEquals` filter
        let mut query = vec![("type", "SYSTEM_DEFINED"), ("maxResults", "1000")];
        if let Some(token) = &next_token {
            query.push(("nextToken", token));
        }
        let url = listing_url(endpoint, "inference-profiles", &query)?;
        let page = get(client, upstream, url).await?;
        profiles.extend(ids(
            &page,
            "inferenceProfileSummaries",
            "inferenceProfileId",
        )?);
        next_token = page
            .get("nextToken")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
            .map(str::to_string);
        if next_token.is_none() {
            return Ok(profiles);
        }
    }
    Err(Failure::Request(format!(
        "the inference profile listing did not end after {MAX_PROFILE_PAGES} pages"
    )))
}

/// `{endpoint}/{path}?{query}`, the query percent-encoded: a `nextToken`
/// can hold `+`, `/` and `=`, and the signature covers the query exactly as
/// AWS decodes it.
fn listing_url(
    endpoint: &str,
    path: &str,
    query: &[(&str, &str)],
) -> Result<reqwest::Url, Failure> {
    reqwest::Url::parse_with_params(&format!("{endpoint}/{path}"), query)
        .map_err(|e| Failure::Request(format!("{endpoint}/{path}: {e}")))
}

/// GET one listing and read it as JSON.
async fn get(
    client: &reqwest::Client,
    upstream: &Upstream,
    url: reqwest::Url,
) -> Result<Value, Failure> {
    let mut req = client.get(url.clone());
    for (k, v) in &upstream.headers {
        req = req.header(k, v);
    }
    if let Some(signer) = upstream.signer() {
        let signed = signer
            .sign(client, &reqwest::Method::GET, url.as_str(), None)
            .await
            .map_err(|e| Failure::Request(e.to_string()))?;
        for (k, v) in signed {
            req = req.header(k, v);
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| Failure::Request(e.to_string()))?;
    let status = resp.status();
    let body: Value = resp.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(body);
    }
    // AWS names the problem in `message`, or `Message` for some errors
    let message = body
        .get("message")
        .or_else(|| body.get("Message"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| status.canonical_reason().unwrap_or("error"));
    Err(Failure::Status {
        status: status.as_u16(),
        message: message.to_string(),
    })
}

/// The `id` of every entry in a listing's `list`.
///
/// A listing without its array is not one to trust, so it fails rather
/// than reading as empty.
fn ids(listing: &Value, list: &str, id: &str) -> Result<Vec<String>, Failure> {
    let entries = listing
        .get(list)
        .and_then(Value::as_array)
        .ok_or_else(|| Failure::Request(format!("the listing has no {list}")))?;
    Ok(entries
        .iter()
        .filter_map(|entry| entry.get(id).and_then(Value::as_str))
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hmac::{Hmac, Mac, digest::KeyInit};
    use serde_json::json;
    use sha2::{Digest, Sha256};
    use think_watch_gateway::proxy::transport::{Credential, Shape, Signer};
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    const AK: &str = "AKIAIOSFODNN7EXAMPLE";
    const SK: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
    /// Carries every character a query value must have encoded.
    const TOKEN: &str = "page+2/of=2==";

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

    fn api_key() -> Upstream {
        bedrock(&[("Authorization", "Bearer ABSK-test")], None)
    }

    async fn mount_foundation_models(server: &MockServer, ids: &[&str]) {
        let summaries: Vec<Value> = ids.iter().map(|id| json!({"modelId": id})).collect();
        Mock::given(method("GET"))
            .and(path("/foundation-models"))
            .and(query_param("byInferenceType", "ON_DEMAND"))
            .and(query_param("byOutputModality", "TEXT"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "modelSummaries": summaries,
            })))
            .mount(server)
            .await;
    }

    /// Two pages of system-defined profiles: `first`, then `second` behind
    /// [`TOKEN`].
    async fn mount_inference_profiles(server: &MockServer, first: &[&str], second: &[&str]) {
        let page = |ids: &[&str]| -> Vec<Value> {
            ids.iter()
                .map(|id| json!({"inferenceProfileId": id, "type": "SYSTEM_DEFINED"}))
                .collect()
        };
        Mock::given(method("GET"))
            .and(path("/inference-profiles"))
            .and(query_param("type", "SYSTEM_DEFINED"))
            .and(query_param_is_missing("nextToken"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "inferenceProfileSummaries": page(first),
                "nextToken": TOKEN,
            })))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/inference-profiles"))
            .and(query_param("type", "SYSTEM_DEFINED"))
            .and(query_param("nextToken", TOKEN))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "inferenceProfileSummaries": page(second),
            })))
            .mount(server)
            .await;
    }

    #[test]
    fn the_control_plane_is_built_from_the_region_and_nothing_else() {
        assert_eq!(
            endpoint("eu-west-1").unwrap(),
            "https://bedrock.eu-west-1.amazonaws.com"
        );
        for bad in [
            "us-east-1.evil.example",
            "evil.example#",
            "https://x.example",
        ] {
            assert!(endpoint(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_page_token_is_percent_encoded_into_the_query() {
        let url = listing_url(
            "https://bedrock.us-east-1.amazonaws.com",
            "inference-profiles",
            &[("type", "SYSTEM_DEFINED"), ("nextToken", TOKEN)],
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://bedrock.us-east-1.amazonaws.com/inference-profiles\
             ?type=SYSTEM_DEFINED&nextToken=page%2B2%2Fof%3D2%3D%3D"
        );
    }

    #[test]
    fn a_listing_without_its_array_fails_rather_than_reading_as_empty() {
        assert_eq!(
            ids(
                &json!({"modelSummaries": [{"modelId": "a"}, {}]}),
                "modelSummaries",
                "modelId"
            ),
            Ok(vec!["a".to_string()])
        );
        assert!(ids(&json!({"message": "?"}), "modelSummaries", "modelId").is_err());
        assert!(ids(&Value::Null, "modelSummaries", "modelId").is_err());
    }

    #[tokio::test]
    async fn the_catalog_is_foundation_models_and_every_page_of_inference_profiles() {
        let server = MockServer::start().await;
        mount_foundation_models(
            &server,
            &["meta.llama3-8b-instruct-v1:0", "amazon.nova-lite-v1:0"],
        )
        .await;
        mount_inference_profiles(
            &server,
            &["us.anthropic.claude-sonnet-4-5-20250929-v1:0"],
            &[
                "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
                "us.amazon.nova-lite-v1:0",
            ],
        )
        .await;

        let models = list_models(&reqwest::Client::new(), &server.uri(), &api_key())
            .await
            .unwrap();

        assert_eq!(
            models,
            [
                "amazon.nova-lite-v1:0",
                "global.anthropic.claude-sonnet-4-5-20250929-v1:0",
                "meta.llama3-8b-instruct-v1:0",
                "us.amazon.nova-lite-v1:0",
                "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            ]
        );
    }

    #[tokio::test]
    async fn an_api_key_goes_out_as_it_is_and_nothing_is_signed() {
        // No access keys: signing would have gone to IMDS for credentials
        let server = MockServer::start().await;
        mount_foundation_models(&server, &["amazon.nova-lite-v1:0"]).await;
        mount_inference_profiles(&server, &[], &[]).await;

        list_models(&reqwest::Client::new(), &server.uri(), &api_key())
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3, "one listing and two pages");
        for req in &requests {
            let auth: Vec<_> = req.headers.get_all("authorization").iter().collect();
            assert_eq!(auth, ["Bearer ABSK-test"], "{}", req.url);
            assert!(!req.headers.contains_key("x-amz-date"), "{}", req.url);
        }
    }

    #[tokio::test]
    async fn access_keys_sign_every_listing_request_as_aws_checks_it() {
        let server = MockServer::start().await;
        mount_foundation_models(&server, &["amazon.nova-lite-v1:0"]).await;
        mount_inference_profiles(&server, &["us.amazon.nova-lite-v1:0"], &[]).await;
        let upstream = bedrock(&[("x-custom", "1")], Some((AK, SK)));

        list_models(&reqwest::Client::new(), &server.uri(), &upstream)
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 3, "one listing and two pages");
        assert!(
            requests
                .iter()
                .any(|r| r.url.query().is_some_and(|q| q.contains("nextToken="))),
            "the second page must have been asked for"
        );
        for req in &requests {
            let (claimed, expected) = signature_of(req);
            assert_eq!(claimed, expected, "{}", req.url);
        }
    }

    #[tokio::test]
    async fn a_refused_listing_fails_the_whole_catalog() {
        // Base ids alone are worse than no list: most current models are
        // not served under them
        let server = MockServer::start().await;
        mount_foundation_models(&server, &["anthropic.claude-sonnet-4-5-20250929-v1:0"]).await;
        Mock::given(method("GET"))
            .and(path("/inference-profiles"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "message": "User is not authorized to perform: bedrock:ListInferenceProfiles",
            })))
            .mount(&server)
            .await;

        let failure = list_models(&reqwest::Client::new(), &server.uri(), &api_key())
            .await
            .unwrap_err();

        assert_eq!(
            failure,
            Failure::Status {
                status: 403,
                message: "User is not authorized to perform: bedrock:ListInferenceProfiles".into(),
            }
        );
    }

    #[tokio::test]
    async fn a_credential_is_good_when_the_control_plane_serves_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/inference-profiles"))
            .and(query_param("maxResults", "1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "inferenceProfileSummaries": [],
            })))
            .mount(&server)
            .await;

        assert!(accepts_credential(&reqwest::Client::new(), &server.uri(), &api_key()).await);
    }

    #[tokio::test]
    async fn a_credential_the_control_plane_refuses_is_not_good() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "message": "Authentication failed: Please make sure your API Key is valid.",
            })))
            .mount(&server)
            .await;

        assert!(!accepts_credential(&reqwest::Client::new(), &server.uri(), &api_key()).await);
    }

    #[tokio::test]
    async fn a_listing_that_never_ends_is_given_up_on() {
        let server = MockServer::start().await;
        mount_foundation_models(&server, &["amazon.nova-lite-v1:0"]).await;
        Mock::given(method("GET"))
            .and(path("/inference-profiles"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "inferenceProfileSummaries": [],
                "nextToken": "again",
            })))
            .expect(MAX_PROFILE_PAGES as u64)
            .mount(&server)
            .await;

        let failure = list_models(&reqwest::Client::new(), &server.uri(), &api_key())
            .await
            .unwrap_err();

        assert!(matches!(failure, Failure::Request(_)), "{failure:?}");
    }

    type HmacSha256 = Hmac<Sha256>;

    fn hmac(key: &[u8], data: &str) -> Vec<u8> {
        let mut mac = HmacSha256::new_from_slice(key).expect("HMAC-SHA256 accepts any key length");
        mac.update(data.as_bytes());
        mac.finalize().into_bytes().to_vec()
    }

    /// RFC 3986 encoding, as SigV4 canonicalises a query value.
    fn aws_encode(s: &str) -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    }

    /// The signature `req` claims, and the one AWS would compute for it
    /// from what arrived — written out from the SigV4 spec rather than by
    /// the signing library, so the two cannot agree by sharing a mistake.
    fn signature_of(req: &wiremock::Request) -> (String, String) {
        let header = |name: &str| {
            req.headers
                .get(name)
                .unwrap_or_else(|| panic!("no {name} on {}", req.url))
                .to_str()
                .unwrap()
                .to_string()
        };
        let auth = header("authorization");
        let field = |name: &str| {
            auth.split(&format!("{name}="))
                .nth(1)
                .and_then(|rest| rest.split(',').next())
                .unwrap_or_else(|| panic!("no {name} in {auth}"))
                .to_string()
        };
        let signed_headers = field("SignedHeaders");
        assert_eq!(signed_headers, "host;x-amz-content-sha256;x-amz-date");
        let amz_date = header("x-amz-date");
        let payload_hash = header("x-amz-content-sha256");
        assert_eq!(
            payload_hash,
            hex::encode(Sha256::digest(b"")),
            "a GET has no body"
        );

        let mut query: Vec<(String, String)> = req
            .url
            .query_pairs()
            .map(|(k, v)| (aws_encode(&k), aws_encode(&v)))
            .collect();
        query.sort();
        let query = query
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");
        let canonical_headers = signed_headers
            .split(';')
            .map(|name| format!("{name}:{}\n", header(name).trim()))
            .collect::<String>();
        let canonical_request = format!(
            "GET\n{}\n{query}\n{canonical_headers}\n{signed_headers}\n{payload_hash}",
            req.url.path(),
        );

        let date = &amz_date[..8];
        let scope = format!("{date}/us-east-1/bedrock/aws4_request");
        assert_eq!(field("Credential"), format!("{AK}/{scope}"));
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let key = hmac(format!("AWS4{SK}").as_bytes(), date);
        let key = hmac(&key, "us-east-1");
        let key = hmac(&key, "bedrock");
        let key = hmac(&key, "aws4_request");
        (field("Signature"), hex::encode(hmac(&key, &string_to_sign)))
    }
}

//! OAuth metadata discovery for MCP server setup. Implements:
//!
//! * RFC 8414 — authorization-server metadata fetch (the
//!   `/.well-known/oauth-authorization-server` and OIDC
//!   `/.well-known/openid-configuration` endpoints).
//! * RFC 9728 — protected-resource metadata pointing at the
//!   authz server (the MCP-spec auto-discovery chain that the
//!   server-create wizard's "Detect" button drives).
//! * RFC 7591 — dynamic client registration for upstreams that
//!   support it (so the admin doesn't have to manually provision
//!   a client_id/secret).
//!
//! Lifted out of `mcp_oauth.rs` because none of these touch the
//! per-user OAuth flow state — they only need AppState + reqwest +
//! the test-rate-limiter. Keeping them in their own file lets a
//! future "OIDC discovery for SSO setup" sibling reuse the same
//! shape.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

use super::{callback_base_url, callback_redirect_uri};

// ---------------------------------------------------------------------------
// POST /api/admin/mcp/oauth-discover — RFC 8414 metadata fetch
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct DiscoverRequest {
    pub issuer: String,
}

#[derive(Debug, Serialize)]
pub struct DiscoverResponse {
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub userinfo_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
}

/// Fetch OAuth metadata from `{issuer}/.well-known/oauth-authorization-server`
/// (RFC 8414) so the admin form can autofill the endpoint inputs without
/// the operator hand-copying URLs from the upstream's docs.
///
/// Falls back to `/.well-known/openid-configuration` for OIDC-style
/// providers that publish their metadata under that path instead.
/// Returns whatever fields the upstream advertised — the admin UI
/// merges them into the form, so partial responses are fine.
pub async fn oauth_discover(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<DiscoverRequest>,
) -> Result<Json<DiscoverResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "mcp_servers:create")
        .await?;
    if req.issuer.is_empty() {
        return Err(AppError::BadRequest("issuer is required".into()));
    }
    (state.url_validator)(&req.issuer)?;

    let issuer = req.issuer.trim_end_matches('/');
    let candidates = [
        format!("{issuer}/.well-known/oauth-authorization-server"),
        format!("{issuer}/.well-known/openid-configuration"),
    ];

    let http = state.http_client.load();
    let mut last_err: Option<String> = None;
    for url in &candidates {
        match http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let body: serde_json::Value = match resp.json().await {
                    Ok(v) => v,
                    Err(e) => {
                        last_err = Some(format!("{url}: {e}"));
                        continue;
                    }
                };
                return Ok(Json(parse_oauth_metadata(&body)));
            }
            Ok(resp) => {
                last_err = Some(format!("{url}: HTTP {}", resp.status()));
            }
            Err(e) => {
                last_err = Some(format!("{url}: {e}"));
            }
        }
    }
    Err(AppError::BadRequest(format!(
        "Discovery failed at all known well-known paths. Last error: {}",
        last_err.unwrap_or_else(|| "unknown".into())
    )))
}

fn parse_oauth_metadata(body: &serde_json::Value) -> DiscoverResponse {
    let parsed = parse_authz_server_metadata(body);
    DiscoverResponse {
        authorization_endpoint: parsed.authorization_endpoint,
        token_endpoint: parsed.token_endpoint,
        revocation_endpoint: parsed.revocation_endpoint,
        userinfo_endpoint: parsed.userinfo_endpoint,
        scopes_supported: parsed.scopes_supported,
    }
}

/// Full RFC 8414 / OIDC discovery doc shape — superset of
/// [`DiscoverResponse`] that also captures the `issuer` claim and the
/// `registration_endpoint` (RFC 7591 dynamic client registration).
/// The probe handler needs the extras; the legacy `oauth_discover`
/// keeps its narrower response shape for back-compat.
#[derive(Debug)]
struct AuthzServerMetadata {
    issuer: Option<String>,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    userinfo_endpoint: Option<String>,
    registration_endpoint: Option<String>,
    scopes_supported: Vec<String>,
    /// Raw `token_endpoint_auth_methods_supported` list. Used by DCR
    /// to pick a method the AS will accept; presence of `"none"`
    /// also flips `is_public_client` on the probe response.
    token_endpoint_auth_methods: Vec<String>,
}

impl AuthzServerMetadata {
    fn public_client_supported(&self) -> bool {
        self.token_endpoint_auth_methods.iter().any(|m| m == "none")
    }
}

fn parse_authz_server_metadata(body: &serde_json::Value) -> AuthzServerMetadata {
    fn s(v: &serde_json::Value, k: &str) -> Option<String> {
        v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string())
    }
    fn arr(v: &serde_json::Value, k: &str) -> Vec<String> {
        v.get(k)
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }
    AuthzServerMetadata {
        issuer: s(body, "issuer"),
        authorization_endpoint: s(body, "authorization_endpoint"),
        token_endpoint: s(body, "token_endpoint"),
        revocation_endpoint: s(body, "revocation_endpoint"),
        userinfo_endpoint: s(body, "userinfo_endpoint"),
        registration_endpoint: s(body, "registration_endpoint"),
        scopes_supported: arr(body, "scopes_supported"),
        token_endpoint_auth_methods: arr(body, "token_endpoint_auth_methods_supported"),
    }
}

// ---------------------------------------------------------------------------
// POST /api/admin/mcp/oauth-probe — full MCP-spec auto-discovery chain
// ---------------------------------------------------------------------------
//
// Maps the GitHub Copilot / VS Code MCP UX of "paste URL, done" onto our
// admin form. Three RFCs glued together:
//
//   1. RFC 9728 (OAuth Protected Resource Metadata) — given an MCP wire
//      endpoint, find the authorization server. Tries the
//      `WWW-Authenticate: Bearer resource_metadata="..."` hint first
//      (per MCP spec 2025-06-18 §authorization), falls back to
//      `<endpoint>/.well-known/oauth-protected-resource`.
//   2. RFC 8414 (Authorization Server Metadata) — given the issuer,
//      fetch authorize/token/revocation/userinfo + (crucially)
//      `registration_endpoint`. Reuses `parse_authz_server_metadata`.
//   3. RFC 7591 (Dynamic Client Registration) — POST to
//      `registration_endpoint` with the console's redirect_uri to
//      mint a fresh client_id/secret. This is the magic that lets the
//      admin skip "go register an OAuth app at github.com/developers"
//      entirely. Skipped when the AS doesn't advertise registration.
//
// Each step is best-effort: we return whatever we got, plus a
// human-readable `diagnostic` string so the UI can either auto-fill
// the form silently or surface "step N failed because X — please fill
// the rest manually."

#[derive(Debug, Deserialize)]
pub struct ProbeRequest {
    pub endpoint_url: String,
}

#[derive(Debug, Serialize)]
pub struct ProbeResponse {
    pub issuer: Option<String>,
    pub authorization_endpoint: Option<String>,
    pub token_endpoint: Option<String>,
    pub revocation_endpoint: Option<String>,
    pub userinfo_endpoint: Option<String>,
    pub registration_endpoint: Option<String>,
    pub scopes_supported: Vec<String>,
    pub client_id: Option<String>,
    /// Plaintext — the frontend echoes this straight into the form for
    /// the admin to review. Persisted encrypted on save like any
    /// hand-entered secret.
    pub client_secret: Option<String>,
    /// True when the upstream AS advertises `"none"` in
    /// `token_endpoint_auth_methods_supported` — admin only needs to
    /// paste a Client ID, no Client Secret. Many MCP-spec-aligned
    /// providers (Feishu, Cloudflare, recent Anthropic-spec compliant
    /// servers) advertise this so they're compatible with native-app
    /// public-client flows.
    pub is_public_client: bool,
    /// The callback URL admins should register at the upstream's
    /// developer console (e.g. open.feishu.cn) when DCR is gated /
    /// unsupported. Surfacing this from the probe means the admin can
    /// copy-paste it directly instead of guessing the gateway's host.
    pub redirect_uri: String,
    /// Step-by-step trace: one entry per discovery step, in order.
    /// Frontend renders them as a numbered list inside an expandable
    /// "details" section — one event per line beats a `→`-joined wall
    /// of text once the chain has 4+ steps.
    pub diagnostic: Vec<String>,
}

pub async fn oauth_probe(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<ProbeRequest>,
) -> Result<Json<ProbeResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "mcp_servers:create")
        .await?;

    // Each probe makes 3-4 outbound HTTP requests against admin-supplied
    // URLs. Same per-user 5/min cap as the other probe endpoints to
    // keep this from being abused as a port scanner.
    crate::handlers::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp_oauth_probe",
    )
    .await?;

    if req.endpoint_url.is_empty() {
        return Err(AppError::BadRequest("endpoint_url is required".into()));
    }
    (state.url_validator)(&req.endpoint_url)?;

    let http = state.http_client.load();
    let validator = state.url_validator.clone();
    let mut diag: Vec<String> = Vec::new();
    let redirect_uri = callback_redirect_uri(&state)?;

    // Step 1: find issuer via Protected Resource Metadata.
    let issuer = match discover_issuer(&http, &validator, &req.endpoint_url, &mut diag).await {
        Some(iss) => iss,
        None => {
            return Ok(Json(ProbeResponse {
                issuer: None,
                authorization_endpoint: None,
                token_endpoint: None,
                revocation_endpoint: None,
                userinfo_endpoint: None,
                registration_endpoint: None,
                scopes_supported: vec![],
                client_id: None,
                client_secret: None,
                is_public_client: false,
                redirect_uri,
                diagnostic: diag,
            }));
        }
    };

    // Step 2: fetch authorization server metadata from the issuer.
    let meta = fetch_authz_server_metadata(&http, &validator, &issuer, &mut diag).await;

    // Step 3: dynamic client registration if the AS advertises it.
    let (client_id, client_secret) = match meta
        .registration_endpoint
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(reg_endpoint) => {
            match register_dynamic_client(&http, reg_endpoint, &meta, &state, &mut diag).await {
                Some((id, secret)) => (Some(id), secret),
                None => (None, None),
            }
        }
        None => {
            diag.push("no registration_endpoint — fill client_id/client_secret manually".into());
            (None, None)
        }
    };

    let is_public_client = meta.public_client_supported();
    Ok(Json(ProbeResponse {
        issuer: meta.issuer.or(Some(issuer)),
        authorization_endpoint: meta.authorization_endpoint,
        token_endpoint: meta.token_endpoint,
        revocation_endpoint: meta.revocation_endpoint,
        userinfo_endpoint: meta.userinfo_endpoint,
        registration_endpoint: meta.registration_endpoint,
        scopes_supported: meta.scopes_supported,
        client_id,
        client_secret,
        is_public_client,
        redirect_uri,
        diagnostic: diag,
    }))
}

/// Step 1 — RFC 9728. Returns the issuer URL the MCP endpoint points to.
async fn discover_issuer(
    http: &reqwest::Client,
    validator: &crate::app::UrlValidator,
    endpoint_url: &str,
    diag: &mut Vec<String>,
) -> Option<String> {
    // 1a. POST a minimal MCP `initialize` to trigger the auth challenge.
    // GET against an MCP endpoint typically returns 405 because the
    // protocol is JSON-RPC over POST — only real protocol requests get
    // the 401 + WWW-Authenticate response mandated by MCP spec
    // 2025-06-18 §authorization. We never read the body, only the
    // header; the `id` is arbitrary.
    let init = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "ThinkWatch probe", "version": "0" }
        }
    });
    if let Ok(resp) = http
        .post(endpoint_url)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .json(&init)
        .send()
        .await
        && let Some(hint) = resp
            .headers()
            .get(reqwest::header::WWW_AUTHENTICATE)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_resource_metadata_hint)
    {
        diag.push(format!("got resource_metadata hint from {endpoint_url}"));
        if let Some(iss) = fetch_protected_resource(http, validator, &hint, diag).await {
            return Some(iss);
        }
    }

    // 1b. Fall back to RFC 9728 §3.1 well-known transformation: for a
    // resource at `https://host/foo/bar`, metadata lives at
    // `https://host/.well-known/oauth-protected-resource/foo/bar`. We
    // try the path-aware form first, then the bare form (some
    // implementations publish at the bare path even when the resource
    // has a path).
    if let Some(parsed) = parse_origin_and_path(endpoint_url) {
        let candidates = if parsed.path.is_empty() {
            vec![format!(
                "{}/.well-known/oauth-protected-resource",
                parsed.origin
            )]
        } else {
            vec![
                format!(
                    "{}/.well-known/oauth-protected-resource{}",
                    parsed.origin, parsed.path
                ),
                format!("{}/.well-known/oauth-protected-resource", parsed.origin),
            ]
        };
        for url in &candidates {
            if let Some(iss) = fetch_protected_resource(http, validator, url, diag).await {
                return Some(iss);
            }
        }
        // 1c. Last resort — endpoint origin might already be the issuer.
        diag.push(format!(
            "no protected-resource metadata; trying {} as issuer directly",
            parsed.origin
        ));
        return Some(parsed.origin);
    }

    diag.push("could not derive issuer from endpoint_url".into());
    None
}

/// Parsed `(origin, path)` of a URL. Path is normalized to drop a
/// trailing slash and treat `/` as empty. Used to build RFC 8414 /
/// RFC 9728 well-known URLs, which require the `.well-known` segment
/// between the origin and the resource/issuer path.
struct OriginAndPath {
    origin: String,
    path: String,
}

fn parse_origin_and_path(url_str: &str) -> Option<OriginAndPath> {
    let url = url::Url::parse(url_str).ok()?;
    let host = url.host_str()?;
    let scheme = url.scheme();
    let port = url.port().map(|p| format!(":{p}")).unwrap_or_default();
    let origin = format!("{scheme}://{host}{port}");
    let raw = url.path().trim_end_matches('/');
    let path = if raw == "/" || raw.is_empty() {
        String::new()
    } else {
        raw.to_string()
    };
    Some(OriginAndPath { origin, path })
}

/// Parse `WWW-Authenticate: Bearer resource_metadata="https://..."`.
/// Returns the URL, or None if the header isn't shaped that way.
fn parse_resource_metadata_hint(header: &str) -> Option<String> {
    // The header is a comma-separated parameter list; we just want the
    // resource_metadata one. Sloppy parser, but the value is always
    // double-quoted per RFC 9728 §5.1.
    let key = "resource_metadata=";
    let idx = header.find(key)?;
    let rest = &header[idx + key.len()..];
    let trimmed = rest.trim_start_matches('"');
    let end = trimmed.find('"')?;
    Some(trimmed[..end].to_string())
}

async fn fetch_protected_resource(
    http: &reqwest::Client,
    validator: &crate::app::UrlValidator,
    url: &str,
    diag: &mut Vec<String>,
) -> Option<String> {
    if validator(url).is_err() {
        diag.push(format!("rejected {url} (SSRF guard)"));
        return None;
    }
    let resp = match http.get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            diag.push(format!("{url}: HTTP {}", r.status()));
            return None;
        }
        Err(e) => {
            diag.push(format!("{url}: {e}"));
            return None;
        }
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            diag.push(format!("{url}: parse: {e}"));
            return None;
        }
    };
    let issuer = body
        .get("authorization_servers")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|x| x.as_str())
        .map(String::from);
    if issuer.is_some() {
        diag.push(format!("found issuer via {url}"));
    } else {
        diag.push(format!(
            "{url}: no authorization_servers[] in protected-resource metadata"
        ));
    }
    issuer
}

/// Step 2 — RFC 8414 / OIDC. Returns the richer struct with optional
/// fields populated from whichever well-known URL responded first.
///
/// RFC 8414 §3 requires the `.well-known` segment between the issuer's
/// origin and its path: for `issuer = https://host/foo`, the metadata
/// lives at `https://host/.well-known/oauth-authorization-server/foo`,
/// NOT `https://host/foo/.well-known/...`. We try both shapes — the
/// spec-compliant one first, then the off-spec "under the path" form
/// some implementations still use.
async fn fetch_authz_server_metadata(
    http: &reqwest::Client,
    validator: &crate::app::UrlValidator,
    issuer: &str,
    diag: &mut Vec<String>,
) -> AuthzServerMetadata {
    let candidates = build_authz_metadata_candidates(issuer);
    for url in &candidates {
        if validator(url).is_err() {
            diag.push(format!("rejected {url} (SSRF guard)"));
            continue;
        }
        match http.get(url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<serde_json::Value>().await {
                    Ok(body) => {
                        diag.push(format!("found authz-server metadata at {url}"));
                        return parse_authz_server_metadata(&body);
                    }
                    Err(e) => diag.push(format!("{url}: parse: {e}")),
                }
            }
            Ok(r) => diag.push(format!("{url}: HTTP {}", r.status())),
            Err(e) => diag.push(format!("{url}: {e}")),
        }
    }
    AuthzServerMetadata {
        issuer: None,
        authorization_endpoint: None,
        token_endpoint: None,
        revocation_endpoint: None,
        userinfo_endpoint: None,
        registration_endpoint: None,
        scopes_supported: vec![],
        token_endpoint_auth_methods: vec![],
    }
}

/// Build the well-known URL candidate list for an issuer, ordered
/// most-likely-correct first. Pure function so it's easily unit tested.
fn build_authz_metadata_candidates(issuer: &str) -> Vec<String> {
    let Some(parsed) = parse_origin_and_path(issuer) else {
        return vec![];
    };
    if parsed.path.is_empty() {
        return vec![
            format!("{}/.well-known/oauth-authorization-server", parsed.origin),
            format!("{}/.well-known/openid-configuration", parsed.origin),
        ];
    }
    vec![
        // RFC 8414 §3 spec-correct form (".well-known" before path).
        format!(
            "{}/.well-known/oauth-authorization-server{}",
            parsed.origin, parsed.path
        ),
        format!(
            "{}/.well-known/openid-configuration{}",
            parsed.origin, parsed.path
        ),
        // Legacy "under the path" form (still seen in older OIDC
        // deployments that ignored RFC 8414 and concatenated naively).
        format!(
            "{}{}/.well-known/oauth-authorization-server",
            parsed.origin, parsed.path
        ),
        format!(
            "{}{}/.well-known/openid-configuration",
            parsed.origin, parsed.path
        ),
    ]
}

/// Stable RFC 7591 `software_id` for ThinkWatch's MCP gateway.
/// Same UUID across every deployment so an AS that audit-logs by
/// `software_id` can group all ThinkWatch installs as one product.
/// Don't change this — RFC 7591 §2 says it SHOULD remain the same
/// for all instances of the client software.
const TW_DCR_SOFTWARE_ID: &str = "0d1f3a2b-4c5e-4f6a-8b9c-0d1e2f3a4b5c";

/// Pick the `token_endpoint_auth_method` we'll request at registration
/// time. Prefer `none` (public client + PKCE — simplest and supported
/// by our token-endpoint code), then `client_secret_post` (also
/// supported), then fall through to whatever the AS lists. Defaults to
/// `client_secret_post` when the AS doesn't advertise the array, which
/// is the most widely accepted method.
fn pick_dcr_auth_method(supported: &[String]) -> &'static str {
    let has = |m: &str| supported.iter().any(|s| s == m);
    if has("none") {
        "none"
    } else if has("client_secret_post") {
        "client_secret_post"
    } else if has("client_secret_basic") {
        // We don't natively send the Basic header at the token
        // endpoint today, but registering for `_basic` lets the admin
        // fall back to a hand-edit later if needed. AS that strictly
        // require basic will reject `_post`.
        "client_secret_basic"
    } else {
        "client_secret_post"
    }
}

/// Step 3 — RFC 7591 dynamic client registration. POSTs an enriched
/// metadata body (RFC 7591 §2: `software_id`, `software_version`,
/// `application_type`, `scope`, picked `token_endpoint_auth_method`)
/// and returns the assigned `(client_id, client_secret?)`.
/// Best-effort: any failure leaves both fields blank for manual entry.
async fn register_dynamic_client(
    http: &reqwest::Client,
    registration_endpoint: &str,
    meta: &AuthzServerMetadata,
    state: &AppState,
    diag: &mut Vec<String>,
) -> Option<(String, Option<String>)> {
    if (state.url_validator)(registration_endpoint).is_err() {
        diag.push(format!(
            "rejected registration_endpoint {registration_endpoint} (SSRF guard)"
        ));
        return None;
    }
    let redirect_uri = match callback_redirect_uri(state) {
        Ok(u) => u,
        Err(e) => {
            diag.push(format!("redirect_uri build failed: {e}"));
            return None;
        }
    };
    let auth_method = pick_dcr_auth_method(&meta.token_endpoint_auth_methods);
    let scope_join = meta.scopes_supported.join(" ");
    let mut body = serde_json::json!({
        "client_name": "ThinkWatch MCP gateway",
        "client_uri": callback_base_url(state).ok(),
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": auth_method,
        // Always `web` — we run server-side, the redirect_uri is HTTPS,
        // PKCE is mandatory regardless of `application_type`. RFC 7591
        // §2 / OAuth 2.0 §2.1.
        "application_type": "web",
        // Stable across deployments per RFC 7591 §2 — see constant above.
        "software_id": TW_DCR_SOFTWARE_ID,
        "software_version": env!("CARGO_PKG_VERSION"),
    });
    // Only include `scope` when the AS published `scopes_supported` —
    // claiming arbitrary scopes against an AS that didn't tell us
    // what's available is more likely to be rejected than help.
    if !scope_join.is_empty()
        && let serde_json::Value::Object(ref mut obj) = body
    {
        obj.insert("scope".into(), serde_json::Value::String(scope_join));
    }
    let resp = match http
        .post(registration_endpoint)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            diag.push(format!("dynamic-registration request failed: {e}"));
            return None;
        }
    };
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        diag.push(format!(
            "dynamic-registration HTTP {status}: {}",
            text.chars().take(120).collect::<String>()
        ));
        return None;
    }
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            diag.push(format!("dynamic-registration parse: {e}"));
            return None;
        }
    };
    let client_id = parsed
        .get("client_id")
        .and_then(|v| v.as_str())
        .map(String::from)?;
    let client_secret = parsed
        .get("client_secret")
        .and_then(|v| v.as_str())
        .map(String::from);
    diag.push(format!(
        "dynamic-registration ok (client_id={}, secret={})",
        client_id,
        client_secret.as_ref().map(|_| "yes").unwrap_or("no")
    ));
    Some((client_id, client_secret))
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------- RFC 9728 / 8414 / 7591 probe helpers ----------------------

    #[test]
    fn parse_resource_metadata_hint_extracts_url() {
        let header = r#"Bearer realm="mcp", resource_metadata="https://api.example.com/.well-known/oauth-protected-resource", error="invalid_token""#;
        assert_eq!(
            parse_resource_metadata_hint(header).as_deref(),
            Some("https://api.example.com/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn parse_resource_metadata_hint_returns_none_when_missing() {
        assert_eq!(parse_resource_metadata_hint("Bearer realm=\"mcp\""), None);
        assert_eq!(parse_resource_metadata_hint(""), None);
    }

    #[test]
    fn pick_dcr_auth_method_prefers_none_when_supported() {
        let supported: Vec<String> = ["client_secret_post", "none"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(pick_dcr_auth_method(&supported), "none");
    }

    #[test]
    fn pick_dcr_auth_method_falls_back_to_post_when_no_explicit_pref() {
        // AS lists only basic — we request `_basic` so DCR proceeds,
        // even though we don't natively send Basic at the token
        // endpoint. Admin can fall back to a hand-edit if needed.
        let basic_only: Vec<String> = vec!["client_secret_basic".to_string()];
        assert_eq!(pick_dcr_auth_method(&basic_only), "client_secret_basic");
        // AS lists nothing — default to `_post`, the most widely
        // accepted method.
        assert_eq!(pick_dcr_auth_method(&[]), "client_secret_post");
        // AS lists post — pick post.
        let post_only: Vec<String> = vec!["client_secret_post".to_string()];
        assert_eq!(pick_dcr_auth_method(&post_only), "client_secret_post");
    }

    #[test]
    fn parse_authz_server_metadata_extracts_registration_endpoint() {
        // Real-world shape — Keycloak / Auth0 / GitHub-style.
        let body = serde_json::json!({
            "issuer": "https://auth.example.com",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/oauth/token",
            "registration_endpoint": "https://auth.example.com/oauth/register",
            "scopes_supported": ["read", "write"]
        });
        let parsed = parse_authz_server_metadata(&body);
        assert_eq!(parsed.issuer.as_deref(), Some("https://auth.example.com"));
        assert_eq!(
            parsed.registration_endpoint.as_deref(),
            Some("https://auth.example.com/oauth/register")
        );
        assert_eq!(parsed.scopes_supported, vec!["read", "write"]);
    }

    #[test]
    fn build_authz_metadata_candidates_root_issuer_uses_bare_well_known() {
        // Issuer = origin (no path) — single well-known shape, no
        // ambiguity. Covers GitHub, Google, Auth0 default tenants.
        let urls = build_authz_metadata_candidates("https://github.com");
        assert_eq!(
            urls,
            vec![
                "https://github.com/.well-known/oauth-authorization-server",
                "https://github.com/.well-known/openid-configuration",
            ]
        );
    }

    #[test]
    fn build_authz_metadata_candidates_path_issuer_puts_well_known_before_path() {
        // RFC 8414 §3: for issuer with a path (Feishu = .../mcp), the
        // metadata MUST be at host/.well-known/oauth-authorization-server/path,
        // not host/path/.well-known/.... We try the spec-correct form
        // first and fall through to the legacy "under-path" form for
        // off-spec implementations.
        let urls = build_authz_metadata_candidates("https://accounts.feishu.cn/mcp");
        assert_eq!(urls.len(), 4);
        assert_eq!(
            urls[0],
            "https://accounts.feishu.cn/.well-known/oauth-authorization-server/mcp"
        );
        assert_eq!(
            urls[1],
            "https://accounts.feishu.cn/.well-known/openid-configuration/mcp"
        );
        assert_eq!(
            urls[2],
            "https://accounts.feishu.cn/mcp/.well-known/oauth-authorization-server"
        );
        assert_eq!(
            urls[3],
            "https://accounts.feishu.cn/mcp/.well-known/openid-configuration"
        );
    }

    #[test]
    fn build_authz_metadata_candidates_strips_trailing_slash() {
        // `https://issuer/` is the same as `https://issuer`. No
        // accidental empty-path variant.
        let urls = build_authz_metadata_candidates("https://issuer.test/");
        assert_eq!(urls.len(), 2);
        assert!(urls[0].ends_with("/.well-known/oauth-authorization-server"));
    }

    #[test]
    fn parse_origin_and_path_separates_origin_from_resource_path() {
        let p = parse_origin_and_path("https://api.example.com:8443/foo/bar/").unwrap();
        assert_eq!(p.origin, "https://api.example.com:8443");
        assert_eq!(p.path, "/foo/bar");
    }

    #[test]
    fn parse_origin_and_path_treats_root_as_empty_path() {
        let p = parse_origin_and_path("https://api.example.com/").unwrap();
        assert_eq!(p.path, "");
        let p = parse_origin_and_path("https://api.example.com").unwrap();
        assert_eq!(p.path, "");
    }

    #[test]
    fn parse_authz_server_metadata_handles_missing_optional_fields() {
        // Bare-minimum AS that doesn't advertise dynamic registration —
        // the probe should still extract what's there and fall through
        // to "fill client_id manually".
        let body = serde_json::json!({
            "issuer": "https://issuer.test",
            "authorization_endpoint": "https://issuer.test/authorize",
            "token_endpoint": "https://issuer.test/token",
        });
        let parsed = parse_authz_server_metadata(&body);
        assert!(parsed.registration_endpoint.is_none());
        assert!(parsed.revocation_endpoint.is_none());
        assert!(parsed.userinfo_endpoint.is_none());
        assert!(parsed.scopes_supported.is_empty());
    }
}

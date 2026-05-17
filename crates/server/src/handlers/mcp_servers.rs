use axum::Json;
use axum::extract::{Path, State};
use uuid::Uuid;

use think_watch_common::crypto;
use think_watch_common::dto::CreateMcpServerRequest;
use think_watch_common::errors::AppError;
use think_watch_common::models::McpServer;

use super::serde_util::deserialize_some;
use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// `probe_mcp_endpoint`, `McpProbeOutcome`, `McpToolSummary`, and
// `normalize_namespace_prefix` live in `super::mcp_shared` so
// `mcp_store` (and any future caller) can reach them without
// reaching across handlers.
pub use super::mcp_shared::{McpToolSummary, normalize_namespace_prefix, probe_mcp_endpoint};

/// Process-wide advisory-lock key for serializing template installs.
/// The literal spells "mcpStore" in ASCII so a DBA glancing at
/// `pg_locks` can tell what's holding it. Any new advisory lock
/// added elsewhere in the codebase MUST use a distinct constant —
/// collisions silently serialize unrelated work and can deadlock
/// under concurrent load.
///
/// Reserved advisory lock keys (keep this list current):
///   * `MCP_STORE_INSTALL_LOCK_KEY` (here): template-install
///     serialization in `create_server` when `template_slug` is set.
const MCP_STORE_INSTALL_LOCK_KEY: i64 = 0x6D637053746F7265;

/// Find an available `(name, namespace_prefix)` pair by appending
/// `_2`, `_3`, … when the base values are already taken. Runs inside
/// the caller's tx so two concurrent installs of the same template
/// can't pick the same suffix. Used only on the template-install
/// path; non-template `create_server` calls just rely on UNIQUE to
/// reject collisions and surface a 409 to the admin.
async fn resolve_server_collisions(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    base_name: &str,
    base_prefix: &str,
) -> Result<(String, String), AppError> {
    for i in 1..100 {
        let (n, p) = if i == 1 {
            (base_name.to_owned(), base_prefix.to_owned())
        } else {
            (format!("{base_name} #{i}"), format!("{base_prefix}_{i}"))
        };
        // `SELECT 1` is INT4 on the wire; binding into `Option<i64>`
        // panics with a column-decode mismatch the moment a row
        // comes back. We don't actually care about the value — only
        // whether the row exists — so use Option<i32>.
        let conflict: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM mcp_servers WHERE name = $1 OR namespace_prefix = $2 LIMIT 1",
        )
        .bind(&n)
        .bind(&p)
        .fetch_optional(&mut **tx)
        .await?;
        if conflict.is_none() {
            return Ok((n, p));
        }
    }
    Err(AppError::BadRequest(
        "Too many installations of this template (>99) — remove some before installing again"
            .into(),
    ))
}

// ---------------------------------------------------------------------------
// Test MCP server connection — anonymous probe via JSON-RPC tools/list
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct TestMcpServerRequest {
    pub endpoint_url: String,
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct TestMcpServerResponse {
    pub success: bool,
    /// True when the server responded with 401/403 — reachable, but the
    /// anonymous probe wasn't permitted to enumerate tools. The admin
    /// "test" button treats this as a soft success since per-user auth
    /// happens later when an end user connects via /connections.
    pub requires_auth: bool,
    pub message: String,
    pub latency_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolSummary>>,
}

#[utoipa::path(
    post,
    path = "/api/mcp/servers/test",
    tag = "MCP Servers",
    security(("bearer_token" = [])),
    request_body = TestMcpServerRequest,
    responses(
        (status = 200, description = "Connection test result", body = TestMcpServerResponse),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
    )
)]
pub async fn test_mcp_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<TestMcpServerRequest>,
) -> Result<Json<TestMcpServerResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "mcp_servers:create")
        .await?;
    // The test endpoint makes arbitrary outbound HTTP requests, so
    // cap per-user calls at 5/min to prevent abuse as a port scanner.
    super::test_rate_limit::check_test_rate_limit(
        &state.redis,
        auth_user.claims.sub,
        auth_user.claims.iat,
        "mcp",
    )
    .await?;

    if req.endpoint_url.is_empty() {
        return Err(AppError::BadRequest("endpoint_url is required".into()));
    }
    think_watch_common::validation::validate_url(&req.endpoint_url)?;
    if let Some(ref headers) = req.custom_headers {
        think_watch_common::validation::validate_custom_headers(headers)?;
    }

    let http = state.http_client.load();
    let outcome = probe_mcp_endpoint(&http, &req.endpoint_url, req.custom_headers.as_ref()).await;

    let tools_count = if outcome.success {
        Some(outcome.tools.len())
    } else {
        None
    };
    let tools = if outcome.success {
        Some(outcome.tools)
    } else {
        None
    };
    // Anonymous probe: 401/403 is the *expected* response for OAuth /
    // static-token MCPs — surface as success so the wizard's Save isn't
    // blocked. The frontend uses `requires_auth` to render an explanatory
    // banner ("auth will be validated on first connection").
    let success = outcome.success || outcome.requires_auth;
    Ok(Json(TestMcpServerResponse {
        success,
        requires_auth: outcome.requires_auth,
        message: outcome.message,
        latency_ms: outcome.latency_ms,
        tools_count,
        tools,
    }))
}

#[utoipa::path(
    get,
    path = "/api/mcp/servers",
    tag = "MCP Servers",
    responses(
        (status = 200, description = "List of all MCP servers"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn list_servers(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<McpServer>>, AppError> {
    auth_user
        .require_global_permission(&state.db, "mcp_servers:read")
        .await?;
    let mut servers = sqlx::query_as::<_, McpServer>(
        r#"SELECT s.*, COALESCE(t.cnt, 0) AS tools_count
           FROM mcp_servers s
           LEFT JOIN (SELECT server_id, COUNT(*) AS cnt FROM mcp_tools WHERE is_active = true GROUP BY server_id) t
             ON t.server_id = s.id
           ORDER BY s.created_at DESC"#,
    )
    .fetch_all(&state.db)
    .await?;

    // Attach lifetime call counts from ClickHouse (mcp_logs) — best-effort:
    // if CH is unavailable we simply leave the counter at 0.
    if super::clickhouse_util::ch_available(&state)
        && let Ok(ch) = super::clickhouse_util::ch_client(&state)
    {
        #[derive(clickhouse::Row, serde::Deserialize)]
        struct CallRow {
            server_id: String,
            calls: u64,
        }
        // Read from the pre-aggregated mcp_server_call_counts table —
        // SummingMergeTree, fed by the mcp_server_call_counts_mv MV on
        // mcp_logs. This is O(number_of_servers) merged rows instead of
        // scanning the full mcp_logs retention window per request.
        let rows = ch
            .query(
                "SELECT server_id, toUInt64(sum(calls)) AS calls
                 FROM mcp_server_call_counts
                 GROUP BY server_id",
            )
            .fetch_all::<CallRow>()
            .await
            .unwrap_or_default();
        let mut lookup = std::collections::HashMap::<String, i64>::new();
        for r in rows {
            lookup.insert(r.server_id, r.calls as i64);
        }
        for s in &mut servers {
            s.call_count = lookup.get(&s.id.to_string()).copied().unwrap_or(0);
        }
    }

    Ok(Json(servers))
}

/// SSRF guard for the OAuth `*_endpoint` URLs. The MCP wire endpoint
/// (`endpoint_url`) is checked separately by the caller — this helper
/// only covers the four OAuth-flow URLs that the server fetches
/// later: token, authorization (used by browser redirect, but also
/// returned to clients in error envelopes), revocation, userinfo.
///
/// Empty strings are treated as "not set" and skipped — admins clear
/// optional URLs by sending `""`, and `validate_url` would otherwise
/// reject empty input with a confusing message.
pub(super) fn validate_oauth_endpoint_urls(
    authorization: Option<&str>,
    token: Option<&str>,
    revocation: Option<&str>,
    userinfo: Option<&str>,
) -> Result<(), AppError> {
    for (field, url) in [
        ("oauth_authorization_endpoint", authorization),
        ("oauth_token_endpoint", token),
        ("oauth_revocation_endpoint", revocation),
        ("oauth_userinfo_endpoint", userinfo),
    ] {
        if let Some(u) = url.filter(|s| !s.is_empty()) {
            think_watch_common::validation::validate_url(u).map_err(|e| match e {
                AppError::BadRequest(m) => AppError::BadRequest(format!("{field}: {m}")),
                other => other,
            })?;
        }
    }
    Ok(())
}

/// Encrypt the OAuth client_secret with the configured AES-GCM key.
/// Returns Ok(None) if no secret was provided.
pub fn encrypt_client_secret(
    plain: Option<&str>,
    encryption_key: &str,
) -> Result<Option<Vec<u8>>, AppError> {
    let Some(secret) = plain.filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let key = crypto::parse_encryption_key(encryption_key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Invalid encryption key: {e}")))?;
    let bytes = crypto::encrypt(secret.as_bytes(), &key)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("Encryption failed: {e}")))?;
    Ok(Some(bytes))
}

#[utoipa::path(
    post,
    path = "/api/mcp/servers",
    tag = "MCP Servers",
    request_body(content = inline(serde_json::Value), description = "CreateMcpServerRequest"),
    responses(
        (status = 200, description = "Newly created MCP server"),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn create_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateMcpServerRequest>,
) -> Result<Json<McpServer>, AppError> {
    auth_user
        .require_global_permission(&state.db, "mcp_servers:create")
        .await?;
    if req.name.is_empty() || req.endpoint_url.is_empty() {
        return Err(AppError::BadRequest(
            "name and endpoint_url are required".into(),
        ));
    }

    // Resolve + validate namespace prefix (explicit, or derived from name).
    let namespace_prefix = normalize_namespace_prefix(
        req.namespace_prefix.as_deref().filter(|s| !s.is_empty()),
        &req.name,
    )?;

    // SSRF prevention: validate every URL we'll later fetch server-side.
    // `endpoint_url` is the MCP wire endpoint; the OAuth `*_endpoint` URLs
    // are POST'd to from `oauth_callback` (token + userinfo) and
    // `revoke_connection` (revocation). Without these, an admin could
    // plant `http://169.254.169.254/...` as `oauth_token_endpoint` and
    // turn the server into an SSRF gadget that carries the AES-decrypted
    // client_secret in the body.
    validate_oauth_endpoint_urls(
        req.oauth_authorization_endpoint.as_deref(),
        req.oauth_token_endpoint.as_deref(),
        req.oauth_revocation_endpoint.as_deref(),
        req.oauth_userinfo_endpoint.as_deref(),
    )?;
    think_watch_common::validation::validate_url(&req.endpoint_url)?;

    // Encrypt the OAuth client_secret if one was supplied.
    let oauth_client_secret_encrypted = encrypt_client_secret(
        req.oauth_client_secret.as_deref(),
        &state.config.encryption_key,
    )?;

    // Auto-detect transport type if not explicitly specified. The
    // detector probes anonymously — upstreams that gate detection
    // behind auth will fall back to streamable_http, which is the
    // default for MCP servers.
    let transport_type = if let Some(ref tt) = req.transport_type {
        tt.clone()
    } else {
        let http_detect = state.http_client.load();
        match think_watch_mcp_gateway::detect::detect_transport(
            &http_detect,
            &req.endpoint_url,
            None,
        )
        .await
        {
            Ok(detected) => detected.as_str().to_owned(),
            Err(_) => "streamable_http".to_owned(),
        }
    };

    let auth_shape = req
        .auth_shape
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("anonymous")
        .to_string();
    if !matches!(auth_shape.as_str(), "anonymous" | "oauth" | "static") {
        return Err(AppError::BadRequest(
            "auth_shape must be 'anonymous', 'oauth', or 'static'".into(),
        ));
    }
    let oauth_scopes = req.oauth_scopes.unwrap_or_default();

    // Auth-header injection. Defaults to the Bearer pattern; admins
    // override per upstream (X-API-Key, etc.). Validate the template
    // up front so a bad value is rejected with a 400 instead of
    // silently smuggling a literal `{{user_id}}` into the upstream.
    let auth_header_name = req
        .auth_header_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("Authorization")
        .to_string();
    let auth_value_template = req
        .auth_value_template
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("Bearer {{token}}")
        .to_string();
    think_watch_mcp_gateway::user_token::validate_auth_value_template(&auth_value_template)
        .map_err(AppError::BadRequest)?;

    // Credential ownership. Default per_user; admin_shared optionally
    // pairs with `wizard_session_id` (OAuth flow already completed) or
    // `shared_static_token` (admin pasted at create time) so the
    // credential lands atomically with the row insert.
    let credential_owner = req
        .credential_owner
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("per_user")
        .to_string();
    if credential_owner != "per_user" && credential_owner != "admin_shared" {
        return Err(AppError::BadRequest(
            "credential_owner must be 'per_user' or 'admin_shared'".into(),
        ));
    }
    // anonymous shape ⇒ no credential is ever needed, so admin_shared
    // is meaningless here — there's nothing to share. Rejecting the
    // combo at write time stops orphan mcp_server_shared_credentials
    // rows from accumulating from misconfigured admins or buggy clients.
    if auth_shape == "anonymous" && credential_owner == "admin_shared" {
        return Err(AppError::BadRequest(
            "credential_owner='admin_shared' is incompatible with auth_shape='anonymous' — \
             anonymous servers don't carry credentials. Pick OAuth or static, or set \
             credential_owner='per_user'."
                .into(),
        ));
    }
    if req.wizard_session_id.is_some() && req.shared_static_token.is_some() {
        return Err(AppError::BadRequest(
            "wizard_session_id and shared_static_token are mutually exclusive".into(),
        ));
    }
    if (req.wizard_session_id.is_some() || req.shared_static_token.is_some())
        && credential_owner != "admin_shared"
    {
        return Err(AppError::BadRequest(
            "wizard_session_id / shared_static_token are only valid when credential_owner = 'admin_shared'"
                .into(),
        ));
    }

    // Wizard credential claim is deferred until INSIDE the TX (after
    // the wizard_session_id advisory lock is held). This makes blob
    // consumption + server insert atomic, preventing concurrent POSTs
    // with the same wizard_session_id from each peek-and-insert.
    // Tradeoff: TX rollback loses the blob; pre-TX validation below
    // narrows that path enough to accept.

    // Encrypt the static token (if provided) outside the TX too —
    // crypto failures shouldn't roll back a row insert.
    let shared_static_token_encrypted = match req.shared_static_token.as_deref() {
        Some(token) if !token.is_empty() => {
            let key =
                think_watch_common::crypto::parse_encryption_key(&state.config.encryption_key)
                    .map_err(|e| {
                        AppError::Internal(anyhow::anyhow!("encryption key error: {e}"))
                    })?;
            Some(
                think_watch_common::crypto::encrypt(token.as_bytes(), &key)
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("encrypt token: {e}")))?,
            )
        }
        _ => None,
    };

    // Build config_json once before the TX so any validation error
    // surfaces before we hold a lock.
    let config_json = {
        let mut config = serde_json::json!({});
        if let Some(ref headers) = req.custom_headers {
            think_watch_common::validation::validate_custom_headers(headers)?;
            config["custom_headers"] = serde_json::to_value(headers).unwrap_or_default();
        }
        if let Some(ttl) = req.cache_ttl_secs {
            config["cache_ttl_secs"] = serde_json::json!(ttl);
        }
        config
    };

    // Claim the wizard credential blob FIRST, before opening the
    // Postgres transaction. The previous shape did `tx.begin()` →
    // `pg_advisory_xact_lock` → Redis GETDEL → PG inserts, which
    // held a pooled PG connection across the Redis round-trip;
    // under any Redis latency blip every concurrent install would
    // tie up the connection pool and serialise on the advisory
    // lock. The advisory lock was justified as guarding a "read-
    // modify-write window" but Redis GETDEL is itself atomic and
    // single-use — concurrent submits already lose the second
    // claim. Hoisting the claim out of the tx keeps the PG path
    // pure SQL, and the original rollback property holds: on PG
    // failure the Redis blob is already gone (admin re-runs OAuth)
    // exactly as it was before, because Redis GETDEL has never
    // been transactional with PG.
    let wizard_cred = match req.wizard_session_id.as_deref() {
        Some(id) if !id.is_empty() => {
            let claimed =
                super::mcp_oauth::claim_wizard_credential(&state, auth_user.claims.sub, id).await?;
            Some(claimed.ok_or_else(|| {
                AppError::BadRequest(
                    "Wizard credential blob not found — the OAuth dance may have timed out, \
                     or another submit already consumed it. Re-run authorize from the wizard."
                        .into(),
                )
            })?)
        }
        _ => None,
    };

    // One transaction: optional template lock + server INSERT +
    // optional shared-credential INSERT + optional store-install
    // audit row. Failures of any step roll back the others so we
    // never end up with an orphan server, a dangling credential,
    // or a count drift on `mcp_store_templates.install_count`.
    let mut tx = state.db.begin().await?;

    // Template-install path: when `template_slug` is present, take
    // a process-wide advisory lock so two concurrent installs of
    // templates with the same default name can't both grab it,
    // then auto-resolve `(name, prefix)` collisions by appending
    // `_2` / `_3` / … . Frontend usually pre-deconflicts via the
    // existing-servers snapshot but the lock guards the race
    // window between snapshot fetch and INSERT.
    let (final_name, final_prefix, template_id) = match req.template_slug.as_deref() {
        Some(slug) if !slug.is_empty() => {
            sqlx::query("SELECT pg_advisory_xact_lock($1)")
                .bind(MCP_STORE_INSTALL_LOCK_KEY)
                .execute(&mut *tx)
                .await?;
            let template_id: Uuid =
                sqlx::query_scalar("SELECT id FROM mcp_store_templates WHERE slug = $1 FOR UPDATE")
                    .bind(slug)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or_else(|| AppError::NotFound(format!("Template '{slug}' not found")))?;
            let (resolved_name, resolved_prefix) =
                resolve_server_collisions(&mut tx, &req.name, &namespace_prefix).await?;
            (resolved_name, resolved_prefix, Some(template_id))
        }
        _ => (req.name.clone(), namespace_prefix.clone(), None),
    };

    // Empty-string display_label collapses to NULL so the server-list
    // and connections-page fallback to `name` works uniformly.
    let display_label = req
        .display_label
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let server = sqlx::query_as::<_, McpServer>(
        r#"INSERT INTO mcp_servers (
               name, namespace_prefix, display_label, description, endpoint_url, transport_type,
               oauth_issuer, oauth_authorization_endpoint, oauth_token_endpoint,
               oauth_revocation_endpoint, oauth_userinfo_endpoint,
               oauth_client_id, oauth_client_secret_encrypted,
               oauth_scopes, auth_shape, static_token_help_url,
               auth_header_name, auth_value_template, credential_owner,
               config_json
           )
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                   $16, $17, $18, $19, $20)
           RETURNING *"#,
    )
    .bind(&final_name)
    .bind(&final_prefix)
    .bind(&display_label)
    .bind(&req.description)
    .bind(&req.endpoint_url)
    .bind(&transport_type)
    .bind(&req.oauth_issuer)
    .bind(&req.oauth_authorization_endpoint)
    .bind(&req.oauth_token_endpoint)
    .bind(&req.oauth_revocation_endpoint)
    .bind(&req.oauth_userinfo_endpoint)
    .bind(&req.oauth_client_id)
    .bind(&oauth_client_secret_encrypted)
    .bind(&oauth_scopes)
    .bind(&auth_shape)
    .bind(&req.static_token_help_url)
    .bind(&auth_header_name)
    .bind(&auth_value_template)
    .bind(&credential_owner)
    .bind(&config_json)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_mcp_server_unique_violation)?;

    // Template install audit row + install_count bump. Same TX as
    // the server INSERT so the count never drifts even if
    // mcp_store_installs FK violations rollback the whole thing.
    if let Some(tid) = template_id {
        sqlx::query(
            "INSERT INTO mcp_store_installs (template_id, server_id, installed_by) VALUES ($1, $2, $3)",
        )
        .bind(tid)
        .bind(server.id)
        .bind(auth_user.claims.sub)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE mcp_store_templates SET install_count = install_count + 1 WHERE id = $1",
        )
        .bind(tid)
        .execute(&mut *tx)
        .await?;
    }

    // Atomic credential install for admin_shared mode.
    if let Some(cred) = &wizard_cred {
        super::mcp_oauth::insert_shared_credential_from_wizard(&mut tx, server.id, cred).await?;
    } else if let Some(encrypted) = &shared_static_token_encrypted {
        sqlx::query(
            r#"INSERT INTO mcp_server_shared_credentials (
                   mcp_server_id, credential_type, access_token_encrypted, configured_by
               )
               VALUES ($1, 'static_token', $2, $3)"#,
        )
        .bind(server.id)
        .bind(encrypted)
        .bind(auth_user.claims.sub)
        .execute(&mut *tx)
        .await?;
    }

    tx.commit().await?;

    // Wizard credential blob was already consumed via GETDEL inside
    // the TX (see claim_wizard_credential above) — no post-commit
    // cleanup needed.

    // Sync the in-memory MCP registry so the gateway can route to the new
    // server immediately, without a restart. The CB is also pre-registered
    // so the dashboard upstream-health panel reflects it on next snapshot.
    if let Ok(registered) = crate::mcp_runtime::build_registered_server(
        &state.db,
        &server,
        &state.config.encryption_key,
    )
    .await
    {
        state.mcp_registry.register(registered).await;
        state.mcp_circuit_breakers.register(&server.name).await;
    }

    // Kick off tool discovery in the background — adding a server in
    // the UI should not block on a slow upstream tools/list, but the
    // metadata should arrive shortly after so the admin sees its
    // tools. For admin_shared servers that already have a bearer
    // (either pasted as `shared_static_token` or transferred from a
    // completed wizard OAuth dance), use the shared bearer so the
    // upstream actually returns tools instead of 401-ing on the
    // anonymous probe.
    let shared_discovery_bearer: Option<String> = if credential_owner == "admin_shared" {
        if let Some(plain) = req.shared_static_token.as_deref().filter(|s| !s.is_empty()) {
            Some(plain.to_string())
        } else if let Some(cred) = &wizard_cred {
            // Decrypt the access token we just stored — the
            // encryption key handle is already parsed above.
            let key =
                think_watch_common::crypto::parse_encryption_key(&state.config.encryption_key)
                    .map_err(|e| {
                        AppError::Internal(anyhow::anyhow!("encryption key error: {e}"))
                    })?;
            think_watch_common::crypto::decrypt(&cred.access_token_encrypted, &key)
                .ok()
                .and_then(|b| String::from_utf8(b).ok())
        } else {
            None
        }
    } else {
        None
    };
    {
        let db = state.db.clone();
        let key = state.config.encryption_key.clone();
        let http = (**state.http_client.load()).clone();
        let registry = state.mcp_registry.clone();
        let server = server.clone();
        let server_id = server.id;
        let db_for_err = state.db.clone();
        let bearer = shared_discovery_bearer;
        tokio::spawn(async move {
            use crate::mcp_runtime::SystemDiscoveryOutcome;
            // Auth-aware discovery when we have an admin_shared bearer;
            // anonymous probe otherwise (the historical default).
            let outcome = match bearer.as_deref() {
                Some(token) => {
                    let header_value = server.auth_value_template.replace("{{token}}", token);
                    let auth = (server.auth_header_name.as_str(), header_value.as_str());
                    crate::mcp_runtime::discover_and_persist_tools_with_auth(
                        &db,
                        &http,
                        &server,
                        Some(auth),
                    )
                    .await
                }
                None => crate::mcp_runtime::discover_and_persist_tools(&db, &http, &server).await,
            };
            match outcome {
                SystemDiscoveryOutcome::Tools(n) => {
                    tracing::info!(
                        mcp_server = %server.name,
                        tools = n,
                        "MCP tool discovery completed for new server"
                    );
                    let _ = sqlx::query("UPDATE mcp_servers SET last_error = NULL WHERE id = $1")
                        .bind(server_id)
                        .execute(&db_for_err)
                        .await;
                    if let Ok(updated) =
                        crate::mcp_runtime::build_registered_server(&db, &server, &key).await
                    {
                        registry.register(updated).await;
                    }
                }
                SystemDiscoveryOutcome::AuthRequired => {
                    // Server requires per-user auth — `mcp_tools` stays
                    // empty by design. Clear last_error so the admin UI
                    // doesn't show stale failure text.
                    let _ = sqlx::query("UPDATE mcp_servers SET last_error = NULL WHERE id = $1")
                        .bind(server_id)
                        .execute(&db_for_err)
                        .await;
                }
                SystemDiscoveryOutcome::Failed(e) => {
                    tracing::warn!(
                        mcp_server = %server.name,
                        error = %e,
                        "Initial MCP tool discovery failed"
                    );
                    let _ = sqlx::query("UPDATE mcp_servers SET last_error = $1 WHERE id = $2")
                        .bind(format!("{e}"))
                        .bind(server_id)
                        .execute(&db_for_err)
                        .await;
                }
            }
        });
    }

    state.audit.log(
        auth_user
            .audit("mcp_server.created")
            .resource("mcp_server")
            .resource_id(server.id.to_string())
            .detail(serde_json::json!({ "name": &req.name })),
    );

    Ok(Json(server))
}

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct UpdateMcpServerRequest {
    pub name: Option<String>,
    pub namespace_prefix: Option<String>,
    /// Human-friendly label shown to end users on /connections and in
    /// the tool catalog. Falls back to `name` when NULL. PATCH
    /// semantics: absent = unchanged, JSON `null` = clear, JSON
    /// string = replace.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub display_label: Option<Option<String>>,
    /// PATCH semantics: absent = unchanged, JSON `null` = clear,
    /// JSON string = replace.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub description: Option<Option<String>>,
    pub endpoint_url: Option<String>,
    /// PATCH semantics: absent = unchanged, JSON `null` = clear,
    /// JSON string = replace.
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_issuer: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_authorization_endpoint: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_token_endpoint: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_revocation_endpoint: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_userinfo_endpoint: Option<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub oauth_client_id: Option<Option<String>>,
    /// Plaintext client secret; sending an empty string clears it.
    /// Encrypted at rest before persisting.
    pub oauth_client_secret: Option<String>,
    pub oauth_scopes: Option<Vec<String>>,
    /// Switch the server's auth shape. Changing this purges any
    /// existing credentials of the previous shape (per-user rows or
    /// shared row) so callers can't end up with stale OAuth tokens
    /// on a server that's now static, etc.
    pub auth_shape: Option<String>,
    #[serde(default, deserialize_with = "deserialize_some")]
    #[schema(value_type = Option<String>)]
    pub static_token_help_url: Option<Option<String>>,
    /// Custom HTTP headers forwarded when connecting to this MCP server.
    /// Values may contain `{{user_id}}` / `{{user_email}}` template variables.
    pub custom_headers: Option<std::collections::HashMap<String, String>>,
    /// Per-server response cache TTL in seconds. `None` = use global default.
    /// `0` = disable caching for this server.
    pub cache_ttl_secs: Option<u64>,
    /// Override the HTTP header name under which the upstream credential
    /// is sent. Defaults to `Authorization` when absent on create.
    pub auth_header_name: Option<String>,
    /// Override the value template (must contain `{{token}}`).
    pub auth_value_template: Option<String>,
    /// Switch credential ownership between `per_user` and
    /// `admin_shared`. When switched to `admin_shared`, any existing
    /// `mcp_user_credentials` rows are cleaned up by the handler so
    /// the per-user UI doesn't show stale connections.
    pub credential_owner: Option<String>,
}

#[utoipa::path(
    patch,
    path = "/api/mcp/servers/{id}",
    tag = "MCP Servers",
    params(
        ("id" = uuid::Uuid, Path, description = "MCP server ID"),
    ),
    request_body(content = UpdateMcpServerRequest),
    responses(
        (status = 200, description = "Updated MCP server"),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn update_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Json(req): Json<UpdateMcpServerRequest>,
) -> Result<Json<McpServer>, AppError> {
    auth_user
        .require_global_permission(&state.db, "mcp_servers:update")
        .await?;
    let existing = sqlx::query_as::<_, McpServer>("SELECT * FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("MCP Server not found".into()))?;

    let name = req.name.as_deref().unwrap_or(&existing.name);
    // PATCH semantics for nullable strings: None = absent (preserve),
    // Some(None) = JSON null (clear), Some(Some(s)) = replace.
    let display_label: Option<&str> = match &req.display_label {
        None => existing.display_label.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let description: Option<&str> = match &req.description {
        None => existing.description.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let endpoint_url = req
        .endpoint_url
        .as_deref()
        .unwrap_or(&existing.endpoint_url);
    let oauth_issuer: Option<&str> = match &req.oauth_issuer {
        None => existing.oauth_issuer.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let oauth_authorization_endpoint: Option<&str> = match &req.oauth_authorization_endpoint {
        None => existing.oauth_authorization_endpoint.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let oauth_token_endpoint: Option<&str> = match &req.oauth_token_endpoint {
        None => existing.oauth_token_endpoint.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let oauth_revocation_endpoint: Option<&str> = match &req.oauth_revocation_endpoint {
        None => existing.oauth_revocation_endpoint.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let oauth_userinfo_endpoint: Option<&str> = match &req.oauth_userinfo_endpoint {
        None => existing.oauth_userinfo_endpoint.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let oauth_client_id: Option<&str> = match &req.oauth_client_id {
        None => existing.oauth_client_id.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let static_token_help_url: Option<&str> = match &req.static_token_help_url {
        None => existing.static_token_help_url.as_deref(),
        Some(inner) => inner.as_deref(),
    };
    let auth_shape = req
        .auth_shape
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&existing.auth_shape)
        .to_string();
    if !matches!(auth_shape.as_str(), "anonymous" | "oauth" | "static") {
        return Err(AppError::BadRequest(
            "auth_shape must be 'anonymous', 'oauth', or 'static'".into(),
        ));
    }
    let auth_shape_changed = auth_shape != existing.auth_shape;
    let oauth_scopes = req
        .oauth_scopes
        .clone()
        .unwrap_or_else(|| existing.oauth_scopes.clone());

    // Auth-header injection — preserve when absent, validate the
    // template when supplied so we can never store a placeholder
    // other than `{{token}}`.
    let auth_header_name = req
        .auth_header_name
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&existing.auth_header_name)
        .to_string();
    let auth_value_template = match req.auth_value_template.as_deref() {
        Some(s) if !s.is_empty() => {
            think_watch_mcp_gateway::user_token::validate_auth_value_template(s)
                .map_err(AppError::BadRequest)?;
            s.to_string()
        }
        _ => existing.auth_value_template.clone(),
    };

    // Credential ownership transitions need a side effect: switching
    // from per_user → admin_shared invalidates per-user credentials
    // (they're irrelevant when a shared bearer exists), and the
    // reverse leaves the now-orphaned shared row in place so the
    // admin can revoke it explicitly. Both decisions are conservative
    // — we never wipe data the user might still need on a hot toggle.
    let credential_owner = req
        .credential_owner
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or(&existing.credential_owner)
        .to_string();
    if credential_owner != "per_user" && credential_owner != "admin_shared" {
        return Err(AppError::BadRequest(
            "credential_owner must be 'per_user' or 'admin_shared'".into(),
        ));
    }
    // See create_server for the rationale.
    if auth_shape == "anonymous" && credential_owner == "admin_shared" {
        return Err(AppError::BadRequest(
            "credential_owner='admin_shared' is incompatible with auth_shape='anonymous' — \
             anonymous servers don't carry credentials. Pick OAuth or static, or set \
             credential_owner='per_user'."
                .into(),
        ));
    }
    let switching_to_admin_shared =
        credential_owner == "admin_shared" && existing.credential_owner != "admin_shared";
    // Reverse direction: admin_shared → per_user. The shared row
    // becomes orphaned once we leave admin_shared mode (the resolver
    // ignores it), so we revoke it upstream best-effort and DELETE
    // the row inside the same TX as the row UPDATE.
    let switching_off_admin_shared =
        existing.credential_owner == "admin_shared" && credential_owner != "admin_shared";

    // Resolve new namespace_prefix: explicit override > existing value.
    let namespace_prefix = match req.namespace_prefix.as_deref() {
        Some(p) if !p.is_empty() => normalize_namespace_prefix(Some(p), name)?,
        _ => existing.namespace_prefix.clone(),
    };

    if req.endpoint_url.is_some() {
        think_watch_common::validation::validate_url(endpoint_url)?;
    }
    // SSRF: validate any newly-supplied OAuth endpoint URLs. Absent
    // fields preserve the existing value (already validated when first
    // set), so we only re-check what the caller is changing.
    validate_oauth_endpoint_urls(
        req.oauth_authorization_endpoint
            .as_ref()
            .and_then(|o| o.as_deref()),
        req.oauth_token_endpoint.as_ref().and_then(|o| o.as_deref()),
        req.oauth_revocation_endpoint
            .as_ref()
            .and_then(|o| o.as_deref()),
        req.oauth_userinfo_endpoint
            .as_ref()
            .and_then(|o| o.as_deref()),
    )?;

    // Auto-detect transport type when endpoint changes; otherwise the
    // value the server is already storing stays.
    let transport_type = if req.endpoint_url.is_some() {
        let http_detect = state.http_client.load();
        match think_watch_mcp_gateway::detect::detect_transport(&http_detect, endpoint_url, None)
            .await
        {
            Ok(detected) => detected.as_str().to_owned(),
            Err(_) => existing.transport_type.clone(),
        }
    } else {
        existing.transport_type.clone()
    };

    // Encrypt the OAuth client_secret iff the caller supplied one;
    // empty string ⇒ clear, absent ⇒ keep existing.
    let oauth_client_secret_encrypted = match req.oauth_client_secret.as_deref() {
        Some("") => None,
        Some(s) => encrypt_client_secret(Some(s), &state.config.encryption_key)?,
        None => existing.oauth_client_secret_encrypted.clone(),
    };

    // Merge custom_headers + cache_ttl into existing config_json
    let mut config_json = existing.config_json.clone();
    if let Some(ref headers) = req.custom_headers {
        think_watch_common::validation::validate_custom_headers(headers)?;
        config_json["custom_headers"] = serde_json::to_value(headers)
            .map_err(|e| AppError::Internal(anyhow::anyhow!("Failed to serialize headers: {e}")))?;
    }
    if let Some(ttl) = req.cache_ttl_secs {
        config_json["cache_ttl_secs"] = serde_json::json!(ttl);
    }

    // Best-effort upstream OAuth revocation runs OUTSIDE the TX —
    // network calls inside a long-held DB lock are a recipe for
    // deadlocks, and this call is fire-and-forget anyway (the row
    // gets DELETEd inside the TX regardless of upstream success).
    if switching_off_admin_shared {
        super::mcp_oauth::best_effort_revoke_shared_upstream(&state, id).await?;
    }

    // One transaction for: row UPDATE + (when relevant) credential
    // cleanup. Without the TX, a DELETE failure after the UPDATE
    // commits would leave the server in the new auth_shape /
    // credential_owner with old-shape credentials still attached —
    // the resolver would then mismatch. Wrapping both in a TX makes
    // the transition atomic.
    let mut tx = state.db.begin().await?;
    let updated = sqlx::query_as::<_, McpServer>(
        r#"UPDATE mcp_servers SET
              name = $2, namespace_prefix = $3, display_label = $4,
              description = $5, endpoint_url = $6,
              transport_type = $7,
              oauth_issuer = $8, oauth_authorization_endpoint = $9,
              oauth_token_endpoint = $10, oauth_revocation_endpoint = $11,
              oauth_userinfo_endpoint = $12,
              oauth_client_id = $13, oauth_client_secret_encrypted = $14,
              oauth_scopes = $15, auth_shape = $16, static_token_help_url = $17,
              auth_header_name = $18, auth_value_template = $19, credential_owner = $20,
              config_json = $21
           WHERE id = $1 RETURNING *"#,
    )
    .bind(id)
    .bind(name)
    .bind(&namespace_prefix)
    .bind(display_label)
    .bind(description)
    .bind(endpoint_url)
    .bind(transport_type)
    .bind(oauth_issuer)
    .bind(oauth_authorization_endpoint)
    .bind(oauth_token_endpoint)
    .bind(oauth_revocation_endpoint)
    .bind(oauth_userinfo_endpoint)
    .bind(oauth_client_id)
    .bind(&oauth_client_secret_encrypted)
    .bind(&oauth_scopes)
    .bind(&auth_shape)
    .bind(static_token_help_url)
    .bind(&auth_header_name)
    .bind(&auth_value_template)
    .bind(&credential_owner)
    .bind(&config_json)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_mcp_server_unique_violation)?;

    // Credential cleanup on relevant transitions. Switching to
    // admin_shared makes per-user creds dead weight; flipping the
    // auth_shape (oauth ↔ static, or either ↔ anonymous) makes the
    // *previous shape's* tokens incompatible with the new resolver
    // path. Both cases purge per-user + shared rows for the server
    // so callers don't end up holding mismatched credentials.
    if switching_to_admin_shared || auth_shape_changed {
        sqlx::query("DELETE FROM mcp_user_credentials WHERE mcp_server_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM mcp_user_tools WHERE mcp_server_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    // Drop the shared-credential row when *either* the auth_shape
    // changed (old token is wrong shape) OR we left admin_shared
    // entirely. Same DELETE either way; collapsing the two
    // conditions avoids running it twice on a combined transition
    // (e.g. admin_shared/oauth → per_user/static).
    if auth_shape_changed || switching_off_admin_shared {
        sqlx::query("DELETE FROM mcp_server_shared_credentials WHERE mcp_server_id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;

    // Evict any cached connection first — the pool keys by id, so a
    // changed endpoint URL needs a fresh connection.
    state.mcp_pool.load().remove(id).await;

    // Re-register so the in-memory registry picks up the new endpoint /
    // name. `register` is an upsert keyed by id.
    if let Ok(registered) = crate::mcp_runtime::build_registered_server(
        &state.db,
        &updated,
        &state.config.encryption_key,
    )
    .await
    {
        state.mcp_registry.register(registered).await;
        state.mcp_circuit_breakers.register(&updated.name).await;
    }

    // Wipe response cache for this server across every user. Admin
    // edits are rare and may flip upstream identity (endpoint, OAuth
    // client, transport, headers) — leaving entries minted under the
    // previous config in place would tunnel pre-update responses into
    // the new epoch until TTL elapses. Always invalidating on update
    // is simpler than tracking which fields actually changed and is
    // cheap given how rarely admins touch server config.
    think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
        .invalidate_server_lane(&id)
        .await;

    // Re-run system-level tool discovery in the background. This
    // serves three purposes:
    //   1. Refresh `mcp_tools` against the (possibly new) endpoint.
    //   2. If the admin flipped this server from "direct" to
    //      auth-required (auth_shape flipped from anonymous to oauth/static),
    //      anonymous tools/list now returns 401 — `discover_and_persist_tools`
    //      catches that and wipes `mcp_tools` + `cached_tools_jsonb` to
    //      `AuthRequired`, so old system-level tool rows can't leak
    //      to users post-flip.
    //   3. If the admin flipped from auth-required to direct, the
    //      anonymous probe will succeed and refill the catalog.
    //
    // Per-user tool caches in `mcp_user_tools` are intentionally
    // *not* wiped — those are scoped to (user, server) and refreshed
    // by the gateway proxy on the user's next tools/list call.
    {
        let db = state.db.clone();
        let http = (**state.http_client.load()).clone();
        let server = updated.clone();
        let key = state.config.encryption_key.clone();
        let registry = state.mcp_registry.clone();
        tokio::spawn(async move {
            use crate::mcp_runtime::SystemDiscoveryOutcome;
            match crate::mcp_runtime::discover_and_persist_tools(&db, &http, &server).await {
                SystemDiscoveryOutcome::Tools(_) | SystemDiscoveryOutcome::AuthRequired => {
                    if let Ok(reg) =
                        crate::mcp_runtime::build_registered_server(&db, &server, &key).await
                    {
                        registry.register(reg).await;
                    }
                }
                SystemDiscoveryOutcome::Failed(e) => {
                    tracing::warn!(
                        mcp_server = %server.name,
                        error = %e,
                        "post-update tool discovery failed"
                    );
                }
            }
        });
    }

    state.audit.log(
        auth_user
            .audit("mcp_server.updated")
            .resource("mcp_server")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": existing.name })),
    );

    Ok(Json(updated))
}

#[utoipa::path(
    get,
    path = "/api/mcp/servers/{id}",
    tag = "MCP Servers",
    params(
        ("id" = uuid::Uuid, Path, description = "MCP server ID"),
    ),
    responses(
        (status = 200, description = "MCP server details"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn get_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<McpServer>, AppError> {
    auth_user
        .require_global_permission(&state.db, "mcp_servers:read")
        .await?;
    let server = sqlx::query_as::<_, McpServer>("SELECT * FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(AppError::NotFound("MCP Server not found".into()))?;

    Ok(Json(server))
}

#[utoipa::path(
    delete,
    path = "/api/mcp/servers/{id}",
    tag = "MCP Servers",
    params(
        ("id" = uuid::Uuid, Path, description = "MCP server ID"),
    ),
    responses(
        (status = 200, description = "Server deleted"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "Not found"),
    ),
    security(("bearer_token" = []))
)]
pub async fn delete_server(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user
        .require_global_permission(&state.db, "mcp_servers:delete")
        .await?;

    let mut tx = state.db.begin().await?;
    let name = delete_server_inner(&mut tx, id)
        .await?
        .ok_or_else(|| AppError::NotFound("MCP Server not found".into()))?;
    tx.commit().await?;

    // Drop from the in-memory registry and connection pool — otherwise the
    // gateway would keep a stale entry for a server that no longer exists
    // in the database.
    state.mcp_registry.unregister(id).await;
    state.mcp_pool.load().remove(id).await;

    // Wipe response cache for this server across every user. The
    // request path resolves through the registry first (which we just
    // unregistered) so a stale cache entry could never actually be
    // served — but leaving the Redis keys to expire on their own TTL
    // is a slow memory leak proportional to traffic on the deleted
    // server. Same `invalidate_server_lane` call we already make on
    // update_server (mcp_servers.rs:~1108) for the same reason.
    think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
        .invalidate_server_lane(&id)
        .await;

    state.audit.log(
        auth_user
            .audit("mcp_server.deleted")
            .resource("mcp_server")
            .resource_id(id.to_string())
            .detail(serde_json::json!({ "name": name })),
    );

    Ok(Json(serde_json::json!({"status": "deleted"})))
}

/// Tear down a single MCP server inside the caller's transaction.
/// Performs the same DB-side work as [`delete_server`]:
///   * SELECT the server name (returned to the caller for audit detail)
///   * decrement the originating store template's `install_count`
///   * DELETE the server row (children CASCADE: `mcp_tools`,
///     `mcp_user_credentials`, `mcp_server_shared_credentials`,
///     `mcp_user_tools`, `mcp_store_installs`)
///
/// Returns `Ok(Some(name))` on success, `Ok(None)` if the row doesn't
/// exist (caller maps that to a "not_found" skip). In-memory registry
/// / connection-pool eviction happens at the call site, *after* the
/// TX commits, so a rolled-back batch never desyncs the registry.
pub(super) async fn delete_server_inner(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
) -> Result<Option<String>, AppError> {
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM mcp_servers WHERE id = $1")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?;
    if name.is_none() {
        return Ok(None);
    }

    // Decrement install_count if this server was installed from the store.
    sqlx::query(
        r#"UPDATE mcp_store_templates SET install_count = GREATEST(install_count - 1, 0)
           WHERE id = (SELECT template_id FROM mcp_store_installs WHERE server_id = $1)"#,
    )
    .bind(id)
    .execute(&mut **tx)
    .await?;

    sqlx::query("DELETE FROM mcp_servers WHERE id = $1")
        .bind(id)
        .execute(&mut **tx)
        .await?;

    Ok(name)
}

/// Hard cap on `POST /api/mcp/servers/bulk-delete` batch size. Picked
/// to keep the worst-case transaction short — every id triggers a
/// SELECT + UPDATE + DELETE plus CASCADE work on
/// `mcp_user_credentials` / `mcp_user_tools`, so 50 is the upper
/// bound at which the TX still completes well under any reasonable
/// statement timeout.
pub(super) const BULK_DELETE_MAX: usize = 50;

#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
pub struct BulkDeleteMcpServersRequest {
    /// IDs of MCP servers to delete. Duplicates are collapsed
    /// server-side; ordering of the result is not guaranteed.
    pub server_ids: Vec<Uuid>,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct BulkDeleteSkip {
    pub id: Uuid,
    /// Stable machine-readable reason: `"not_found"` (no row with
    /// that id) for now. Reserved values: `"unauthorized"`,
    /// `"active_sessions"` — added when those checks come online.
    pub reason: String,
}

#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct BulkDeleteMcpServersResponse {
    pub deleted: Vec<Uuid>,
    pub skipped: Vec<BulkDeleteSkip>,
}

#[utoipa::path(
    post,
    path = "/api/mcp/servers/bulk-delete",
    tag = "MCP Servers",
    request_body = BulkDeleteMcpServersRequest,
    responses(
        (status = 200, description = "Bulk delete result", body = BulkDeleteMcpServersResponse),
        (status = 400, description = "Bad request"),
        (status = 401, description = "Unauthorized"),
        (status = 403, description = "Forbidden"),
    ),
    security(("bearer_token" = []))
)]
pub async fn bulk_delete_servers(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<BulkDeleteMcpServersRequest>,
) -> Result<Json<BulkDeleteMcpServersResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "mcp_servers:delete")
        .await?;

    if req.server_ids.is_empty() {
        return Err(AppError::BadRequest("server_ids must not be empty".into()));
    }
    if req.server_ids.len() > BULK_DELETE_MAX {
        return Err(AppError::BadRequest(format!(
            "server_ids exceeds bulk-delete cap of {BULK_DELETE_MAX}"
        )));
    }

    // Collapse duplicates so a caller that sent the same id twice
    // doesn't get one "deleted" + one "not_found" entry for the same
    // row (the second pass would land on the now-missing row inside
    // the same TX).
    let mut unique_ids: Vec<Uuid> = Vec::with_capacity(req.server_ids.len());
    {
        let mut seen = std::collections::HashSet::with_capacity(req.server_ids.len());
        for id in &req.server_ids {
            if seen.insert(*id) {
                unique_ids.push(*id);
            }
        }
    }

    // Single transaction so the batch is atomic — either every id we
    // report as deleted is gone (and the corresponding install_count
    // decrements applied), or nothing changed. A per-id loop with
    // separate TXs would let a mid-batch failure leave the DB in a
    // half-deleted state, which is exactly the footgun bulk-delete
    // is meant to avoid.
    let mut tx = state.db.begin().await?;
    let mut deleted_pairs: Vec<(Uuid, String)> = Vec::new();
    let mut skipped: Vec<BulkDeleteSkip> = Vec::new();
    for id in unique_ids {
        match delete_server_inner(&mut tx, id).await? {
            Some(name) => deleted_pairs.push((id, name)),
            None => skipped.push(BulkDeleteSkip {
                id,
                reason: "not_found".to_string(),
            }),
        }
    }
    tx.commit().await?;

    // Post-commit cleanup + audit. Done outside the TX so an audit
    // emit that briefly blocks on the forwarder pool can't roll back
    // the delete. One audit entry per server preserves
    // resource-level granularity in `/api/admin/audit`.
    let cache = think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone());
    for (id, name) in &deleted_pairs {
        state.mcp_registry.unregister(*id).await;
        state.mcp_pool.load().remove(*id).await;
        // Same cache wipe the single-delete path performs — without
        // this, bulk-deleting N servers leaves N×K Redis keys lying
        // around to expire on their own TTL.
        cache.invalidate_server_lane(id).await;
        state.audit.log(
            auth_user
                .audit("mcp_server.deleted")
                .resource("mcp_server")
                .resource_id(id.to_string())
                .detail(serde_json::json!({ "name": name, "bulk": true })),
        );
    }

    Ok(Json(BulkDeleteMcpServersResponse {
        deleted: deleted_pairs.into_iter().map(|(id, _)| id).collect(),
        skipped,
    }))
}

/// Translate PostgreSQL unique-constraint violations on `mcp_servers` into
/// user-facing conflict errors, so the UI shows "already in use" instead of
/// a generic 500. Other sqlx errors fall through unchanged.
fn map_mcp_server_unique_violation(e: sqlx::Error) -> AppError {
    if let sqlx::Error::Database(db_err) = &e
        && db_err.code().as_deref() == Some("23505")
    {
        let constraint = db_err.constraint().unwrap_or("");
        if constraint.contains("namespace_prefix") {
            return AppError::Conflict("namespace_prefix already in use".into());
        }
        if constraint.contains("name") {
            return AppError::Conflict("server name already in use".into());
        }
        return AppError::Conflict("duplicate server".into());
    }
    AppError::from(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generate a valid 64-char hex key (32 bytes) for crypto tests.
    fn test_hex_key() -> String {
        "0".repeat(64)
    }

    #[test]
    fn encrypt_client_secret_returns_none_for_no_input() {
        let out = encrypt_client_secret(None, &test_hex_key()).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn encrypt_client_secret_returns_none_for_empty_string() {
        // Empty string treated the same as None — admins toggling the
        // input field shouldn't accidentally persist an empty ciphertext
        // that decrypts to an empty bearer token.
        let out = encrypt_client_secret(Some(""), &test_hex_key()).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn encrypt_client_secret_produces_ciphertext_for_real_value() {
        let out = encrypt_client_secret(Some("my-secret"), &test_hex_key()).unwrap();
        let bytes = out.expect("Some ciphertext when secret is non-empty");
        // AES-GCM ciphertext = nonce (12) + ciphertext + tag (16) ≥ 28 bytes
        // even for a single byte of input. "my-secret" (9 bytes) → ≥ 37.
        assert!(
            bytes.len() >= 28,
            "ciphertext smaller than nonce+tag overhead: {}",
            bytes.len()
        );
        // Ciphertext must NOT contain the plaintext as a substring.
        assert!(
            !bytes.windows(9).any(|w| w == b"my-secret"),
            "ciphertext contains plaintext leakage"
        );
    }

    #[test]
    fn encrypt_client_secret_uses_nonce_so_ciphertexts_differ() {
        // AES-GCM with a fresh nonce per call MUST produce distinct
        // ciphertexts for the same plaintext+key. Catches a refactor
        // that accidentally fixes the nonce (catastrophic for GCM).
        let key = test_hex_key();
        let a = encrypt_client_secret(Some("same-secret"), &key)
            .unwrap()
            .unwrap();
        let b = encrypt_client_secret(Some("same-secret"), &key)
            .unwrap()
            .unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn encrypt_client_secret_rejects_invalid_hex_key() {
        // Non-hex chars in the key surface as Internal — admins seeing
        // this in logs know to check their `ENCRYPTION_KEY` env var.
        let err = encrypt_client_secret(Some("x"), "not-hex").unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
    }

    #[test]
    fn encrypt_client_secret_rejects_short_key() {
        // 30 hex chars = 15 bytes ≠ 32. Must fail rather than silently
        // pad or truncate (which would weaken the cipher).
        let short = "ab".repeat(15);
        let err = encrypt_client_secret(Some("x"), &short).unwrap_err();
        assert!(matches!(err, AppError::Internal(_)));
    }
}

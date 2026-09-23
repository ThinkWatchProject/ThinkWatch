//! Content-filter and PII-redactor test sandbox endpoints. The
//! admin UI uses these to preview how a proposed rule set would
//! flag a given piece of text BEFORE saving the rules into
//! `dynamic_config` — so a typo in a regex doesn't immediately
//! start dropping live traffic.

use axum::Json;
use axum::extract::State;
use serde::{Deserialize, Serialize};

use think_watch_common::errors::AppError;

use crate::app::AppState;
use crate::middleware::auth_guard::AuthUser;

// ---------------------------------------------------------------------------
// Content filter — test sandbox & presets
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ContentFilterTestRequest {
    /// User text to test against the supplied rules.
    pub text: String,
    /// Rules to test (the unsaved rules currently in the UI).
    pub rules: Vec<think_watch_gateway::content_filter::DenyRuleConfig>,
}

#[derive(Debug, Serialize)]
pub struct ContentFilterTestMatch {
    pub name: String,
    pub pattern: String,
    pub match_type: String,
    pub action: String,
    pub matched_snippet: String,
}

#[derive(Debug, Serialize)]
pub struct ContentFilterTestResponse {
    pub matches: Vec<ContentFilterTestMatch>,
}

/// POST /api/admin/settings/content-filter/test — try the supplied rules
/// against a sample of user text and return every rule that fires.
#[utoipa::path(
    post,
    path = "/api/admin/settings/content-filter/test",
    tag = "Settings",
    request_body(
        content = inline(serde_json::Value),
        description = "text: string, rules: DenyRuleConfig[]",
    ),
    responses(
        (status = 200, description = "Rules that matched the input text"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn test_content_filter(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<ContentFilterTestRequest>,
) -> Result<Json<ContentFilterTestResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "content_filter:read")
        .await?;
    use think_watch_gateway::content_filter::ContentFilter;
    let filter = ContentFilter::from_config(&req.rules);
    let matches = filter
        .check_text_all(&req.text)
        .into_iter()
        .map(|m| ContentFilterTestMatch {
            name: m.name,
            pattern: m.pattern,
            match_type: m.match_type.to_string(),
            action: m.action.to_string(),
            matched_snippet: m.matched_snippet,
        })
        .collect();
    Ok(Json(ContentFilterTestResponse { matches }))
}

#[derive(Debug, Serialize)]
pub struct ContentFilterPreset {
    pub id: String,
    pub rules: Vec<think_watch_gateway::content_filter::DenyRuleConfig>,
}

/// GET /api/admin/settings/content-filter/presets — return built-in rule groups
/// (basic / strict / chinese). UI labels are localized on the frontend.
#[utoipa::path(
    get,
    path = "/api/admin/settings/content-filter/presets",
    tag = "Settings",
    responses(
        (status = 200, description = "Built-in content filter preset groups"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn list_content_filter_presets(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<ContentFilterPreset>>, AppError> {
    auth_user
        .require_global_permission(&state.db, "content_filter:read")
        .await?;
    let groups = think_watch_gateway::content_filter::presets()
        .into_iter()
        .map(|g| ContentFilterPreset {
            id: g.id.to_string(),
            rules: g.rules,
        })
        .collect();
    Ok(Json(groups))
}

// ---------------------------------------------------------------------------
// PII redactor — test sandbox
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct PiiRedactorTestRequest {
    pub text: String,
    pub patterns: Vec<think_watch_common::pii::PiiPatternConfig>,
}

#[derive(Debug, Serialize)]
pub struct PiiRedactorTestMatch {
    pub name: String,
    pub original: String,
    pub placeholder: String,
}

#[derive(Debug, Serialize)]
pub struct PiiRedactorTestResponse {
    pub redacted_text: String,
    pub matches: Vec<PiiRedactorTestMatch>,
}

/// POST /api/admin/settings/pii-redactor/test — apply the supplied PII patterns
/// to a text sample and return the redacted version with the substitution map.
#[utoipa::path(
    post,
    path = "/api/admin/settings/pii-redactor/test",
    tag = "Settings",
    request_body(
        content = inline(serde_json::Value),
        description = "text: string, patterns: PiiPatternConfig[]",
    ),
    responses(
        (status = 200, description = "Redacted text and substitution map"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn test_pii_redactor(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<PiiRedactorTestRequest>,
) -> Result<Json<PiiRedactorTestResponse>, AppError> {
    auth_user
        .require_global_permission(&state.db, "pii_redactor:read")
        .await?;
    use think_watch_gateway::pii_redactor::PiiRedactor;

    let redactor = PiiRedactor::from_config(&req.patterns);
    let (redacted_text, ctx) = redactor.redact_str(&req.text);

    let matches = ctx
        .replacements()
        .map(|(original, placeholder)| {
            // `{{CUSTOM_EMAIL_2}}` → `CUSTOM_EMAIL`: the prefix may itself
            // contain underscores, so cut at the last one
            let name = placeholder
                .trim_start_matches("{{")
                .trim_end_matches("}}")
                .rsplit_once('_')
                .map_or("", |(label, _)| label)
                .to_string();
            PiiRedactorTestMatch {
                name,
                original: original.to_string(),
                placeholder: placeholder.to_string(),
            }
        })
        .collect();

    Ok(Json(PiiRedactorTestResponse {
        redacted_text,
        matches,
    }))
}

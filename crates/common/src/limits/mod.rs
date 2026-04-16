// ============================================================================
// Generic limits engine
//
// One module for everything in the "rate limit + budget cap" surface.
// The five tables that touch the gateway hot path are:
//
//   rate_limit_rules     — sliding-window rules (1m / 5m / 1h / 5h / 1d / 1w)
//   budget_caps          — natural-period token budgets (daily / weekly / monthly)
//   models               — input_multiplier / output_multiplier for weighted tokens
//
// And the corresponding submodules in this folder:
//
//   sliding              — bucketed Lua check_and_record over Redis
//   budget               — natural-period add_weighted_tokens / check_cap
//   weight               — model_id → weighted token converter (LRU cached)
//
// Subject identification: every rule / cap is keyed by a (subject_kind,
// subject_id) tuple. The kinds are pinned to a small closed enum:
//
//   rate_limit_rules.subject_kind ∈ { user, api_key, provider, mcp_server, team }
//   budget_caps.subject_kind      ∈ { user, api_key, team, provider }
//
// At request time the proxy resolves which subjects apply (e.g. an AI
// gateway call resolves to api_key + user + provider) and runs every
// matching enabled rule through `sliding::check_and_record`. Any single
// failure rejects the request — Lua handles the all-or-nothing INCR.
//
// See `plan.md` (limits chapter) for the full design.
// ============================================================================

use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Row};
use uuid::Uuid;

pub mod budget;
pub mod sliding;
pub mod weight;

// ----------------------------------------------------------------------------
// Subject + surface enums (string-typed in the DB, typed in Rust)
// ----------------------------------------------------------------------------

/// What kind of resource a rule or cap is attached to.
///
/// Role-level constraints are stored inline on `rbac_roles.surface_constraints`
/// (see `SurfaceConstraints` below), so `'role'` is intentionally absent
/// here — the side tables are reserved for per-user / per-key overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitSubject {
    User,
    ApiKey,
}

impl RateLimitSubject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ApiKey => "api_key",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user" => Self::User,
            "api_key" => Self::ApiKey,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetSubject {
    User,
    ApiKey,
}

impl BudgetSubject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ApiKey => "api_key",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user" => Self::User,
            "api_key" => Self::ApiKey,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    AiGateway,
    McpGateway,
}

impl Surface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AiGateway => "ai_gateway",
            Self::McpGateway => "mcp_gateway",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ai_gateway" => Self::AiGateway,
            "mcp_gateway" => Self::McpGateway,
            _ => return None,
        })
    }
}

// ----------------------------------------------------------------------------
// Role-inline surface constraints
//
// Persisted on `rbac_roles.surface_constraints` as JSONB. Shape:
//
//   {
//     "ai_gateway":  { "rules": [...], "budgets": [...] },
//     "mcp_gateway": { "rules": [...], "budgets": [...] }
//   }
//
// Absent surface key == empty block. The aggregator (see
// `rbac::compute_user_surface_constraints`) merges across every role
// the user holds using "most restrictive wins" semantics.
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceConstraints {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_gateway: Option<SurfaceBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_gateway: Option<SurfaceBlock>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceBlock {
    #[serde(default)]
    pub rules: Vec<SurfaceRule>,
    #[serde(default)]
    pub budgets: Vec<SurfaceBudget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceRule {
    pub metric: RateMetric,
    pub window_secs: i32,
    pub max_count: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceBudget {
    pub period: BudgetPeriod,
    pub limit_tokens: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

impl SurfaceConstraints {
    pub fn block(&self, surface: Surface) -> Option<&SurfaceBlock> {
        match surface {
            Surface::AiGateway => self.ai_gateway.as_ref(),
            Surface::McpGateway => self.mcp_gateway.as_ref(),
        }
    }

    pub fn block_mut(&mut self, surface: Surface) -> &mut Option<SurfaceBlock> {
        match surface {
            Surface::AiGateway => &mut self.ai_gateway,
            Surface::McpGateway => &mut self.mcp_gateway,
        }
    }
}

/// Merge every role's constraints into a single effective set using
/// "most restrictive wins":
///   - per (surface, metric, window_secs): take the MIN enabled max_count
///   - per (surface, period):              take the MIN enabled limit_tokens
///
/// Disabled entries are ignored. An entry absent from a role is treated
/// as "no constraint from that role" — it does NOT tighten the min.
pub fn merge_most_restrictive(inputs: &[SurfaceConstraints]) -> SurfaceConstraints {
    use std::collections::HashMap;

    let mut ai = SurfaceBlock::default();
    let mut mcp = SurfaceBlock::default();

    // (surface_key, metric, window_secs) -> min_max_count
    let mut rule_min: HashMap<(&str, RateMetric, i32), i64> = HashMap::new();
    let mut budget_min: HashMap<(&str, BudgetPeriod), i64> = HashMap::new();

    for cons in inputs {
        for (surface_key, block_opt) in [
            (Surface::AiGateway.as_str(), cons.ai_gateway.as_ref()),
            (Surface::McpGateway.as_str(), cons.mcp_gateway.as_ref()),
        ] {
            let Some(block) = block_opt else { continue };
            for r in &block.rules {
                if !r.enabled || r.max_count <= 0 {
                    continue;
                }
                rule_min
                    .entry((surface_key, r.metric, r.window_secs))
                    .and_modify(|v| {
                        if r.max_count < *v {
                            *v = r.max_count;
                        }
                    })
                    .or_insert(r.max_count);
            }
            for b in &block.budgets {
                if !b.enabled || b.limit_tokens <= 0 {
                    continue;
                }
                budget_min
                    .entry((surface_key, b.period))
                    .and_modify(|v| {
                        if b.limit_tokens < *v {
                            *v = b.limit_tokens;
                        }
                    })
                    .or_insert(b.limit_tokens);
            }
        }
    }

    for ((surface_key, metric, window_secs), max_count) in rule_min {
        let rule = SurfaceRule {
            metric,
            window_secs,
            max_count,
            enabled: true,
        };
        if surface_key == Surface::AiGateway.as_str() {
            ai.rules.push(rule);
        } else {
            mcp.rules.push(rule);
        }
    }
    for ((surface_key, period), limit_tokens) in budget_min {
        let b = SurfaceBudget {
            period,
            limit_tokens,
            enabled: true,
        };
        if surface_key == Surface::AiGateway.as_str() {
            ai.budgets.push(b);
        } else {
            mcp.budgets.push(b);
        }
    }

    SurfaceConstraints {
        ai_gateway: (!ai.rules.is_empty() || !ai.budgets.is_empty()).then_some(ai),
        mcp_gateway: (!mcp.rules.is_empty() || !mcp.budgets.is_empty()).then_some(mcp),
    }
}

/// Schema validation for an incoming `surface_constraints` JSON payload
/// on role create/update. Returns a user-friendly error message on
/// failure. Accepts the same shape `SurfaceConstraints` deserializes
/// from; additionally checks window/period/value ranges.
pub fn validate_surface_constraints(
    value: &serde_json::Value,
) -> Result<SurfaceConstraints, String> {
    if !value.is_object() {
        return Err("surface_constraints must be an object".into());
    }
    let cons: SurfaceConstraints = serde_json::from_value(value.clone())
        .map_err(|e| format!("Invalid surface_constraints JSON: {e}"))?;
    for (surface_name, block) in [
        ("ai_gateway", cons.ai_gateway.as_ref()),
        ("mcp_gateway", cons.mcp_gateway.as_ref()),
    ] {
        let Some(block) = block else { continue };
        for (i, r) in block.rules.iter().enumerate() {
            if !is_allowed_window(r.window_secs) {
                return Err(format!(
                    "{surface_name}.rules[{i}]: window_secs {} not in allowed set",
                    r.window_secs
                ));
            }
            if r.max_count <= 0 {
                return Err(format!("{surface_name}.rules[{i}]: max_count must be > 0"));
            }
        }
        for (i, b) in block.budgets.iter().enumerate() {
            if b.limit_tokens <= 0 {
                return Err(format!(
                    "{surface_name}.budgets[{i}]: limit_tokens must be > 0"
                ));
            }
        }
    }
    Ok(cons)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateMetric {
    /// Each call counts as 1 unit. Used for "max N requests per window".
    Requests,
    /// Each call counts as its weighted-token cost. Used for "max N
    /// tokens per window", where N is in weighted (multiplier-adjusted)
    /// tokens.
    Tokens,
}

impl RateMetric {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requests => "requests",
            Self::Tokens => "tokens",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "requests" => Self::Requests,
            "tokens" => Self::Tokens,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    Daily,
    Weekly,
    Monthly,
}

impl BudgetPeriod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Daily => "daily",
            Self::Weekly => "weekly",
            Self::Monthly => "monthly",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "daily" => Self::Daily,
            "weekly" => Self::Weekly,
            "monthly" => Self::Monthly,
            _ => return None,
        })
    }
}

// ----------------------------------------------------------------------------
// Row structs (DB shapes)
// ----------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct RateLimitRule {
    pub id: Uuid,
    pub subject_kind: RateLimitSubject,
    pub subject_id: Uuid,
    pub surface: Surface,
    pub metric: RateMetric,
    pub window_secs: i32,
    pub max_count: i64,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct BudgetCap {
    pub id: Uuid,
    pub subject_kind: BudgetSubject,
    pub subject_id: Uuid,
    pub period: BudgetPeriod,
    pub limit_tokens: i64,
    pub enabled: bool,
}

// ----------------------------------------------------------------------------
// CRUD — straight DB reads / writes. The hot read path goes through
// `LimitsCache` (below) instead of these helpers.
// ----------------------------------------------------------------------------

/// Permitted sliding-window lengths (seconds). Anything outside this
/// list is rejected at insert time. Limits are kept short enough to
/// stay in the rate-limit zone — week-long is the cutoff; longer
/// windows belong in `budget_caps`.
pub const ALLOWED_WINDOW_SECS: &[i32] = &[
    60,      // 1m
    300,     // 5m
    3_600,   // 1h
    18_000,  // 5h
    86_400,  // 1d
    604_800, // 1w
];

pub fn is_allowed_window(secs: i32) -> bool {
    ALLOWED_WINDOW_SECS.contains(&secs)
}

pub async fn list_rules(
    pool: &PgPool,
    subject_kind: RateLimitSubject,
    subject_id: Uuid,
) -> Result<Vec<RateLimitRule>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, subject_kind, subject_id, surface, metric, window_secs, max_count, enabled \
           FROM rate_limit_rules \
          WHERE subject_kind = $1 AND subject_id = $2 \
          ORDER BY surface, metric, window_secs",
    )
    .bind(subject_kind.as_str())
    .bind(subject_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter_map(row_to_rule).collect())
}

/// Load every enabled rule for a subject. Used by the gateway hot path
/// after the cache misses.
pub async fn list_enabled_rules_for_subjects(
    pool: &PgPool,
    subjects: &[(RateLimitSubject, Uuid)],
) -> Result<Vec<RateLimitRule>, sqlx::Error> {
    if subjects.is_empty() {
        return Ok(Vec::new());
    }
    // Build $1, $2 ... pairs. We can't UNNEST tuples cleanly across
    // both columns in pgvec form, so a loop is fine — subjects is
    // bounded by the resolver to ~3 entries.
    let mut out: Vec<RateLimitRule> = Vec::new();
    for (kind, id) in subjects {
        let rows = sqlx::query(
            "SELECT id, subject_kind, subject_id, surface, metric, window_secs, max_count, enabled \
               FROM rate_limit_rules \
              WHERE subject_kind = $1 AND subject_id = $2 AND enabled = TRUE",
        )
        .bind(kind.as_str())
        .bind(id)
        .fetch_all(pool)
        .await?;
        out.extend(rows.into_iter().filter_map(row_to_rule));
    }
    Ok(out)
}

/// Insert-or-update payload for `upsert_rule`. Bundled into a struct
/// because the column count tripped clippy::too_many_arguments and
/// because callers tend to construct this once from a request DTO
/// anyway.
#[derive(Debug, Clone)]
pub struct UpsertRule {
    pub subject_kind: RateLimitSubject,
    pub subject_id: Uuid,
    pub surface: Surface,
    pub metric: RateMetric,
    pub window_secs: i32,
    pub max_count: i64,
    pub enabled: bool,
}

pub async fn upsert_rule(pool: &PgPool, req: UpsertRule) -> Result<RateLimitRule, sqlx::Error> {
    if !is_allowed_window(req.window_secs) {
        // Surface validation as a sqlx error so the caller propagates it
        // through the existing error machinery without a new variant.
        return Err(sqlx::Error::Protocol(format!(
            "window_secs {} not in allowed set",
            req.window_secs
        )));
    }
    if req.max_count <= 0 {
        return Err(sqlx::Error::Protocol("max_count must be > 0".into()));
    }
    let row = sqlx::query(
        "INSERT INTO rate_limit_rules \
            (subject_kind, subject_id, surface, metric, window_secs, max_count, enabled) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (subject_kind, subject_id, surface, metric, window_secs) \
         DO UPDATE SET max_count = EXCLUDED.max_count, \
                       enabled   = EXCLUDED.enabled, \
                       updated_at = now() \
         RETURNING id, subject_kind, subject_id, surface, metric, window_secs, max_count, enabled",
    )
    .bind(req.subject_kind.as_str())
    .bind(req.subject_id)
    .bind(req.surface.as_str())
    .bind(req.metric.as_str())
    .bind(req.window_secs)
    .bind(req.max_count)
    .bind(req.enabled)
    .fetch_one(pool)
    .await?;
    row_to_rule(row).ok_or_else(|| sqlx::Error::Protocol("rule row decode failed".into()))
}

pub async fn delete_rule(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let n = sqlx::query("DELETE FROM rate_limit_rules WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

pub async fn list_caps(
    pool: &PgPool,
    subject_kind: BudgetSubject,
    subject_id: Uuid,
) -> Result<Vec<BudgetCap>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT id, subject_kind, subject_id, period, limit_tokens, enabled \
           FROM budget_caps \
          WHERE subject_kind = $1 AND subject_id = $2 \
          ORDER BY period",
    )
    .bind(subject_kind.as_str())
    .bind(subject_id)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().filter_map(row_to_cap).collect())
}

pub async fn list_enabled_caps_for_subjects(
    pool: &PgPool,
    subjects: &[(BudgetSubject, Uuid)],
) -> Result<Vec<BudgetCap>, sqlx::Error> {
    if subjects.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<BudgetCap> = Vec::new();
    for (kind, id) in subjects {
        let rows = sqlx::query(
            "SELECT id, subject_kind, subject_id, period, limit_tokens, enabled \
               FROM budget_caps \
              WHERE subject_kind = $1 AND subject_id = $2 AND enabled = TRUE",
        )
        .bind(kind.as_str())
        .bind(id)
        .fetch_all(pool)
        .await?;
        out.extend(rows.into_iter().filter_map(row_to_cap));
    }
    Ok(out)
}

pub async fn upsert_cap(
    pool: &PgPool,
    subject_kind: BudgetSubject,
    subject_id: Uuid,
    period: BudgetPeriod,
    limit_tokens: i64,
    enabled: bool,
) -> Result<BudgetCap, sqlx::Error> {
    if limit_tokens <= 0 {
        return Err(sqlx::Error::Protocol("limit_tokens must be > 0".into()));
    }
    let row = sqlx::query(
        "INSERT INTO budget_caps (subject_kind, subject_id, period, limit_tokens, enabled) \
         VALUES ($1, $2, $3, $4, $5) \
         ON CONFLICT (subject_kind, subject_id, period) \
         DO UPDATE SET limit_tokens = EXCLUDED.limit_tokens, \
                       enabled      = EXCLUDED.enabled, \
                       updated_at   = now() \
         RETURNING id, subject_kind, subject_id, period, limit_tokens, enabled",
    )
    .bind(subject_kind.as_str())
    .bind(subject_id)
    .bind(period.as_str())
    .bind(limit_tokens)
    .bind(enabled)
    .fetch_one(pool)
    .await?;
    row_to_cap(row).ok_or_else(|| sqlx::Error::Protocol("cap row decode failed".into()))
}

pub async fn delete_cap(pool: &PgPool, id: Uuid) -> Result<bool, sqlx::Error> {
    let n = sqlx::query("DELETE FROM budget_caps WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?
        .rows_affected();
    Ok(n > 0)
}

// ----------------------------------------------------------------------------
// Row decoders. sqlx::FromRow can't reach the enum types directly, so
// we decode manually and skip rows whose enum strings don't parse —
// this can only happen if the DB has been manually edited to a value
// the CHECK constraint forbids, in which case dropping the row at the
// boundary is safer than blowing up the request.
// ----------------------------------------------------------------------------

fn row_to_rule(row: sqlx::postgres::PgRow) -> Option<RateLimitRule> {
    let kind = RateLimitSubject::parse(row.try_get::<String, _>("subject_kind").ok()?.as_str())?;
    let surface = Surface::parse(row.try_get::<String, _>("surface").ok()?.as_str())?;
    let metric = RateMetric::parse(row.try_get::<String, _>("metric").ok()?.as_str())?;
    Some(RateLimitRule {
        id: row.try_get("id").ok()?,
        subject_kind: kind,
        subject_id: row.try_get("subject_id").ok()?,
        surface,
        metric,
        window_secs: row.try_get("window_secs").ok()?,
        max_count: row.try_get("max_count").ok()?,
        enabled: row.try_get("enabled").ok()?,
    })
}

pub fn row_to_cap(row: sqlx::postgres::PgRow) -> Option<BudgetCap> {
    let kind = BudgetSubject::parse(row.try_get::<String, _>("subject_kind").ok()?.as_str())?;
    let period = BudgetPeriod::parse(row.try_get::<String, _>("period").ok()?.as_str())?;
    Some(BudgetCap {
        id: row.try_get("id").ok()?,
        subject_kind: kind,
        subject_id: row.try_get("subject_id").ok()?,
        period,
        limit_tokens: row.try_get("limit_tokens").ok()?,
        enabled: row.try_get("enabled").ok()?,
    })
}

// ----------------------------------------------------------------------------
// Cache-invalidation pubsub
//
// Same shape as `dynamic_config::notify_config_changed`, but on its
// own channel so a settings change doesn't force every gateway to
// drop its limits cache.
// ----------------------------------------------------------------------------

const LIMITS_CHANGED_CHANNEL: &str = "limits:changed";

pub async fn notify_limits_changed(redis: &fred::clients::Client) {
    use fred::interfaces::PubsubInterface;
    let _: Result<(), _> = redis.publish(LIMITS_CHANGED_CHANNEL, "reload").await;
}

pub fn limits_changed_channel() -> &'static str {
    LIMITS_CHANGED_CHANNEL
}

// ----------------------------------------------------------------------------
// Startup validation — A.6 lives here so all the static checks are in
// one place. Called once from `main.rs` after migrations.
// ----------------------------------------------------------------------------

/// Walk every persisted rule + every model multiplier and refuse to
/// start if anything is out of range. The migration's CHECK constraints
/// catch most of this, but a manual UPDATE can still slip a value
/// through, and we'd rather fail-fast than silently misbehave.
pub async fn validate_persisted(pool: &PgPool) -> anyhow::Result<()> {
    let bad_windows: Vec<(i32,)> = sqlx::query_as(
        "SELECT DISTINCT window_secs FROM rate_limit_rules WHERE NOT (window_secs = ANY($1))",
    )
    .bind(ALLOWED_WINDOW_SECS)
    .fetch_all(pool)
    .await?;
    if !bad_windows.is_empty() {
        let list: Vec<String> = bad_windows.iter().map(|(s,)| s.to_string()).collect();
        anyhow::bail!(
            "Found rate_limit_rules.window_secs values outside the allowed set: {}",
            list.join(", ")
        );
    }

    let bad_models: Vec<(String,)> = sqlx::query_as(
        "SELECT model_id FROM models \
          WHERE input_multiplier <= 0 OR output_multiplier <= 0",
    )
    .fetch_all(pool)
    .await?;
    if !bad_models.is_empty() {
        let list: Vec<String> = bad_models.iter().map(|(m,)| m.clone()).collect();
        anyhow::bail!(
            "Found models with non-positive input/output multiplier: {}",
            list.join(", ")
        );
    }

    tracing::info!("limits validation passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_constraints_roundtrip_json() {
        let json = serde_json::json!({
            "ai_gateway": {
                "rules": [
                    { "metric": "requests", "window_secs": 60, "max_count": 100, "enabled": true }
                ],
                "budgets": [
                    { "period": "daily", "limit_tokens": 1_000_000, "enabled": true }
                ]
            }
        });
        let cons: SurfaceConstraints = serde_json::from_value(json.clone()).unwrap();
        let ai = cons.ai_gateway.as_ref().unwrap();
        assert_eq!(ai.rules.len(), 1);
        assert_eq!(ai.rules[0].metric, RateMetric::Requests);
        assert_eq!(ai.rules[0].window_secs, 60);
        assert_eq!(ai.rules[0].max_count, 100);
        assert_eq!(ai.budgets[0].period, BudgetPeriod::Daily);
        assert!(cons.mcp_gateway.is_none());
        // Round-trip back to JSON preserves shape.
        let back = serde_json::to_value(&cons).unwrap();
        assert_eq!(back, json);
    }

    #[test]
    fn surface_constraints_absent_and_empty_blocks() {
        let empty: SurfaceConstraints = serde_json::from_str("{}").unwrap();
        assert!(empty.ai_gateway.is_none());
        assert!(empty.mcp_gateway.is_none());

        let empty_blocks: SurfaceConstraints =
            serde_json::from_str(r#"{"ai_gateway":{"rules":[],"budgets":[]}}"#).unwrap();
        let ai = empty_blocks.ai_gateway.as_ref().unwrap();
        assert!(ai.rules.is_empty());
        assert!(ai.budgets.is_empty());
    }

    #[test]
    fn validate_surface_constraints_rejects_bad_window() {
        let json = serde_json::json!({
            "ai_gateway": {
                "rules": [
                    { "metric": "requests", "window_secs": 42, "max_count": 10, "enabled": true }
                ]
            }
        });
        assert!(validate_surface_constraints(&json).is_err());
    }

    #[test]
    fn validate_surface_constraints_rejects_bad_max_count() {
        let json = serde_json::json!({
            "ai_gateway": {
                "rules": [
                    { "metric": "tokens", "window_secs": 60, "max_count": 0 }
                ]
            }
        });
        assert!(validate_surface_constraints(&json).is_err());
    }

    #[test]
    fn merge_most_restrictive_multi_role() {
        let a = serde_json::from_value::<SurfaceConstraints>(serde_json::json!({
            "ai_gateway": {
                "rules": [
                    { "metric": "requests", "window_secs": 60, "max_count": 100 },
                    { "metric": "tokens",   "window_secs": 60, "max_count": 10_000 }
                ],
                "budgets": [
                    { "period": "daily", "limit_tokens": 5_000_000 }
                ]
            }
        }))
        .unwrap();
        let b = serde_json::from_value::<SurfaceConstraints>(serde_json::json!({
            "ai_gateway": {
                "rules": [
                    { "metric": "requests", "window_secs": 60, "max_count": 50 }
                ],
                "budgets": [
                    { "period": "daily",   "limit_tokens": 10_000_000 },
                    { "period": "monthly", "limit_tokens": 50_000_000 }
                ]
            },
            "mcp_gateway": {
                "rules": [
                    { "metric": "requests", "window_secs": 300, "max_count": 20 }
                ]
            }
        }))
        .unwrap();

        let merged = merge_most_restrictive(&[a, b]);
        let ai = merged.ai_gateway.as_ref().unwrap();

        // requests/60s: min(100, 50) = 50
        let reqs = ai
            .rules
            .iter()
            .find(|r| r.metric == RateMetric::Requests && r.window_secs == 60)
            .unwrap();
        assert_eq!(reqs.max_count, 50);
        // tokens/60s: only in A
        let toks = ai
            .rules
            .iter()
            .find(|r| r.metric == RateMetric::Tokens && r.window_secs == 60)
            .unwrap();
        assert_eq!(toks.max_count, 10_000);
        // daily budget: min(5M, 10M) = 5M
        let daily = ai
            .budgets
            .iter()
            .find(|b| b.period == BudgetPeriod::Daily)
            .unwrap();
        assert_eq!(daily.limit_tokens, 5_000_000);
        // monthly: only in B
        let monthly = ai
            .budgets
            .iter()
            .find(|b| b.period == BudgetPeriod::Monthly)
            .unwrap();
        assert_eq!(monthly.limit_tokens, 50_000_000);

        let mcp = merged.mcp_gateway.as_ref().unwrap();
        assert_eq!(mcp.rules.len(), 1);
        assert_eq!(mcp.rules[0].window_secs, 300);
    }

    #[test]
    fn merge_ignores_disabled_and_nonpositive() {
        let a = SurfaceConstraints {
            ai_gateway: Some(SurfaceBlock {
                rules: vec![SurfaceRule {
                    metric: RateMetric::Requests,
                    window_secs: 60,
                    max_count: 10,
                    enabled: false,
                }],
                budgets: vec![SurfaceBudget {
                    period: BudgetPeriod::Daily,
                    limit_tokens: 0,
                    enabled: true,
                }],
            }),
            mcp_gateway: None,
        };
        let b = SurfaceConstraints {
            ai_gateway: Some(SurfaceBlock {
                rules: vec![SurfaceRule {
                    metric: RateMetric::Requests,
                    window_secs: 60,
                    max_count: 100,
                    enabled: true,
                }],
                budgets: vec![],
            }),
            mcp_gateway: None,
        };
        let merged = merge_most_restrictive(&[a, b]);
        let ai = merged.ai_gateway.as_ref().unwrap();
        assert_eq!(ai.rules[0].max_count, 100);
        assert!(ai.budgets.is_empty());
    }
}

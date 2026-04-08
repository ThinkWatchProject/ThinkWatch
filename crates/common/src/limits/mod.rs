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
//   rate_limit_rules.subject_kind ∈ { user, api_key, provider, mcp_server }
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
/// Mapping to DB tables is implicit — `RateLimitSubject` covers the
/// kinds allowed by `rate_limit_rules.subject_kind`, `BudgetSubject`
/// covers `budget_caps.subject_kind`. They overlap on user / api_key /
/// provider; only `mcp_server` is rate-only and only `team` is budget-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitSubject {
    User,
    ApiKey,
    Provider,
    McpServer,
}

impl RateLimitSubject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ApiKey => "api_key",
            Self::Provider => "provider",
            Self::McpServer => "mcp_server",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user" => Self::User,
            "api_key" => Self::ApiKey,
            "provider" => Self::Provider,
            "mcp_server" => Self::McpServer,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetSubject {
    User,
    ApiKey,
    Team,
    Provider,
}

impl BudgetSubject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ApiKey => "api_key",
            Self::Team => "team",
            Self::Provider => "provider",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user" => Self::User,
            "api_key" => Self::ApiKey,
            "team" => Self::Team,
            "provider" => Self::Provider,
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

fn row_to_cap(row: sqlx::postgres::PgRow) -> Option<BudgetCap> {
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

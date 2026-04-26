// ============================================================================
// Generic limits engine
//
// One module for everything in the "rate limit + budget cap" surface.
// The five tables that touch the gateway hot path are:
//
//   rate_limit_rules     — sliding-window rules (1m / 5m / 1h / 5h / 1d / 1w)
//   budget_caps          — natural-period token budgets (daily / weekly / monthly)
//   models               — input_weight / output_weight for weighted tokens
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
//   rate_limit_rules.subject_kind ∈ { user, api_key_lineage }
//   budget_caps.subject_kind      ∈ { user, api_key_lineage }
//
// `api_key_lineage` (NOT `api_key`) is deliberate: a rule attached to a
// key's `lineage_id` automatically applies to every generation of the
// rotation chain. Rotating an api_key mints a fresh row with a new `id`
// but the same `lineage_id`, so the rule keeps biting without anyone
// having to copy it forward. The handler layer keeps the URL surface as
// `…/limits/api_key/{api_key_id}` (frontend stays unaware of lineage)
// and resolves the path id to its lineage_id before the SELECT/INSERT.
//
// Role- and team-level constraints are NOT their own subjects — they live
// in `rbac_roles.policy_document` (and, if ever added, the analogous field
// on teams) and fold into each member's merged policy at request time,
// materializing as `subject = User` rules. Redis counters therefore stay
// user-scoped and grouping membership never becomes a shared pool.
//
// At request time the proxy resolves which subjects apply (user +
// api_key, plus the merged role/team constraint set attributed to that
// same user) and runs every matching enabled rule through
// `sliding::check_and_record`. Any single failure rejects the request —
// Lua handles the all-or-nothing INCR.
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
/// Role-level constraints are derived from `rbac_roles.policy_document`
/// Constraints blocks, so `'role'` is intentionally absent here — the
/// side tables are reserved for per-user / per-key overrides.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitSubject {
    User,
    /// Rule attached to an api_key's `lineage_id`. Survives rotation:
    /// every generation in the chain shares the same lineage_id, so
    /// the rule keeps applying without copy-forward on rotation.
    ApiKeyLineage,
}

impl RateLimitSubject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ApiKeyLineage => "api_key_lineage",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user" => Self::User,
            "api_key_lineage" => Self::ApiKeyLineage,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetSubject {
    User,
    /// Cap attached to an api_key's `lineage_id`. See
    /// `RateLimitSubject::ApiKeyLineage` for the rotation rationale.
    ApiKeyLineage,
}

impl BudgetSubject {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::ApiKeyLineage => "api_key_lineage",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "user" => Self::User,
            "api_key_lineage" => Self::ApiKeyLineage,
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
// Derived from `rbac_roles.policy_document` Constraints on statements
// that target `ai_gateway:use` or `mcp_gateway:use`.
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

// ----------------------------------------------------------------------------
// PolicyDocument → derived data extraction
//
// Pure functions that parse a policy_document JSONB value (already
// deserialized into `rbac::PolicyDocument`) into the runtime types the
// gateway and RBAC layers consume. No DB calls.
// ----------------------------------------------------------------------------

/// Human-readable window string ↔ seconds conversion.
/// Supported values: "1m", "5m", "1h", "5h", "1d", "1w".
pub fn window_to_secs(s: &str) -> Option<i32> {
    Some(match s {
        "1m" => 60,
        "5m" => 300,
        "1h" => 3_600,
        "5h" => 18_000,
        "1d" => 86_400,
        "1w" => 604_800,
        _ => return None,
    })
}

pub fn secs_to_window(secs: i32) -> Option<&'static str> {
    Some(match secs {
        60 => "1m",
        300 => "5m",
        3_600 => "1h",
        18_000 => "5h",
        86_400 => "1d",
        604_800 => "1w",
        _ => return None,
    })
}

/// PascalCase constraint types matching the policy_document JSON shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PolicyConstraints {
    #[serde(default)]
    pub rate_limits: Vec<PolicyRateLimit>,
    #[serde(default)]
    pub budgets: Vec<PolicyBudget>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PolicyRateLimit {
    pub metric: String,
    pub window: String,
    pub max_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct PolicyBudget {
    pub period: String,
    pub max_tokens: i64,
}

/// Validate a policy_document's Constraints blocks. Called during role
/// create/update to reject invalid window strings or non-positive counts.
pub fn validate_policy_constraints(constraints: &PolicyConstraints) -> Result<(), String> {
    for (i, rl) in constraints.rate_limits.iter().enumerate() {
        let secs = window_to_secs(&rl.window).ok_or_else(|| {
            format!(
                "RateLimits[{i}]: Window '{}' not in allowed set (1m/5m/1h/5h/1d/1w)",
                rl.window
            )
        })?;
        if !is_allowed_window(secs) {
            return Err(format!(
                "RateLimits[{i}]: Window '{}' not allowed",
                rl.window
            ));
        }
        if rl.max_count <= 0 {
            return Err(format!("RateLimits[{i}]: MaxCount must be > 0"));
        }
        if RateMetric::parse(&rl.metric).is_none() {
            return Err(format!(
                "RateLimits[{i}]: Metric '{}' unknown (expected requests/tokens)",
                rl.metric
            ));
        }
    }
    for (i, b) in constraints.budgets.iter().enumerate() {
        if b.max_tokens <= 0 {
            return Err(format!("Budgets[{i}]: MaxTokens must be > 0"));
        }
        if BudgetPeriod::parse(&b.period).is_none() {
            return Err(format!(
                "Budgets[{i}]: Period '{}' unknown (expected daily/weekly/monthly)",
                b.period
            ));
        }
    }
    Ok(())
}

/// Extract SurfaceConstraints from a parsed PolicyDocument by examining
/// statements whose Action includes `ai_gateway:use` or `mcp_gateway:use`
/// and pulling their Constraints blocks. Uses `think_watch_auth::rbac`
/// types via the `serde_json::Value` representation so this crate
/// does not depend on the auth crate.
pub fn extract_surface_constraints(doc: &serde_json::Value) -> SurfaceConstraints {
    let statements = match doc.get("Statement").and_then(|s| s.as_array()) {
        Some(arr) => arr,
        None => return SurfaceConstraints::default(),
    };

    let mut ai_block = SurfaceBlock::default();
    let mut mcp_block = SurfaceBlock::default();

    for stmt in statements {
        let effect = stmt.get("Effect").and_then(|e| e.as_str()).unwrap_or("");
        if effect != "Allow" {
            continue;
        }

        let constraints_val = match stmt.get("Constraints") {
            Some(c) if !c.is_null() => c,
            _ => continue,
        };
        let constraints: PolicyConstraints = match serde_json::from_value(constraints_val.clone()) {
            Ok(c) => c,
            Err(_) => continue,
        };

        let actions = stmt_actions(stmt);
        let targets_ai = action_matches_any(&actions, "ai_gateway:use");
        let targets_mcp = action_matches_any(&actions, "mcp_gateway:use");

        if targets_ai {
            append_constraints(&mut ai_block, &constraints);
        }
        if targets_mcp {
            append_constraints(&mut mcp_block, &constraints);
        }
    }

    SurfaceConstraints {
        ai_gateway: if ai_block.rules.is_empty() && ai_block.budgets.is_empty() {
            None
        } else {
            Some(ai_block)
        },
        mcp_gateway: if mcp_block.rules.is_empty() && mcp_block.budgets.is_empty() {
            None
        } else {
            Some(mcp_block)
        },
    }
}

/// Extract the effective model scope from a parsed PolicyDocument.
/// Looks at Allow statements whose Action matches `ai_gateway:use` and
/// collects Resource entries that start with `model:`. Returns `None`
/// when any matching statement has Resource `"*"` (unrestricted).
pub fn extract_allowed_models(doc: &serde_json::Value) -> Option<Vec<String>> {
    let statements = doc.get("Statement").and_then(|s| s.as_array())?;
    let mut models: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut found_any = false;

    for stmt in statements {
        let effect = stmt.get("Effect").and_then(|e| e.as_str()).unwrap_or("");
        if effect != "Allow" {
            continue;
        }
        let actions = stmt_actions(stmt);
        if !action_matches_any(&actions, "ai_gateway:use") {
            continue;
        }
        found_any = true;
        let resources = stmt_resources(stmt);
        for r in &resources {
            if r == "*" {
                return None;
            }
            if let Some(model) = r.strip_prefix("model:") {
                models.insert(model.to_string());
            }
        }
    }
    if !found_any {
        return None;
    }
    Some(models.into_iter().collect())
}

/// Extract the effective MCP tool scope from a parsed PolicyDocument.
/// Same logic as `extract_allowed_models` but for `mcp_gateway:use`
/// statements and `mcp_tool:` resource prefixes.
pub fn extract_allowed_mcp_tools(doc: &serde_json::Value) -> Option<Vec<String>> {
    let statements = doc.get("Statement").and_then(|s| s.as_array())?;
    let mut tools: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut found_any = false;

    for stmt in statements {
        let effect = stmt.get("Effect").and_then(|e| e.as_str()).unwrap_or("");
        if effect != "Allow" {
            continue;
        }
        let actions = stmt_actions(stmt);
        if !action_matches_any(&actions, "mcp_gateway:use") {
            continue;
        }
        found_any = true;
        let resources = stmt_resources(stmt);
        for r in &resources {
            if r == "*" {
                return None;
            }
            if let Some(tool) = r.strip_prefix("mcp_tool:") {
                tools.insert(tool.to_string());
            }
        }
    }
    if !found_any {
        return None;
    }
    Some(tools.into_iter().collect())
}

/// Extract the flat set of permission strings from a parsed
/// PolicyDocument. Collects Action strings from all Allow statements,
/// expanding `"*"` into the full permission catalog (caller supplies
/// the known keys via the `all_perms` parameter).
pub fn extract_permissions(doc: &serde_json::Value, all_perms: &[&str]) -> Vec<String> {
    let statements = match doc.get("Statement").and_then(|s| s.as_array()) {
        Some(arr) => arr,
        None => return Vec::new(),
    };
    let mut perms: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for stmt in statements {
        let effect = stmt.get("Effect").and_then(|e| e.as_str()).unwrap_or("");
        if effect != "Allow" {
            continue;
        }
        let actions = stmt_actions(stmt);
        for action in &actions {
            if action == "*" {
                perms.extend(all_perms.iter().map(|s| s.to_string()));
            } else if action.ends_with(":*") {
                let prefix = &action[..action.len() - 1];
                for p in all_perms {
                    if p.starts_with(prefix) {
                        perms.insert(p.to_string());
                    }
                }
            } else {
                perms.insert(action.to_string());
            }
        }
    }
    perms.into_iter().collect()
}

// Internal helpers for policy_document field extraction

fn stmt_actions(stmt: &serde_json::Value) -> Vec<String> {
    match stmt.get("Action") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn stmt_resources(stmt: &serde_json::Value) -> Vec<String> {
    match stmt.get("Resource") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn action_matches_any(actions: &[String], target: &str) -> bool {
    actions.iter().any(|a| a == "*" || a == target)
}

fn append_constraints(block: &mut SurfaceBlock, constraints: &PolicyConstraints) {
    for rl in &constraints.rate_limits {
        if let (Some(metric), Some(secs)) =
            (RateMetric::parse(&rl.metric), window_to_secs(&rl.window))
        {
            block.rules.push(SurfaceRule {
                metric,
                window_secs: secs,
                max_count: rl.max_count,
                enabled: true,
            });
        }
    }
    for b in &constraints.budgets {
        if let Some(period) = BudgetPeriod::parse(&b.period) {
            block.budgets.push(SurfaceBudget {
                period,
                limit_tokens: b.max_tokens,
                enabled: true,
            });
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

/// Schema validation for an incoming `surface_constraints` JSON payload.
/// Returns a user-friendly error message on failure.
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
    /// tokens per window", where N is in weighted (weight-adjusted)
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
//
// Three "override metadata" fields — `expires_at`, `reason`, `created_by` —
// are all optional. They're only meaningful on persisted rows; rules the
// gateway synthesizes in-memory from role-merged constraints leave them
// None. The admin API populates them from the request + authenticated
// caller; the hot path ignores them past the "is this row still active?"
// filter done at SELECT time.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize)]
pub struct BudgetCap {
    pub id: Uuid,
    pub subject_kind: BudgetSubject,
    pub subject_id: Uuid,
    pub period: BudgetPeriod,
    pub limit_tokens: i64,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Uuid>,
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
        "SELECT id, subject_kind, subject_id, surface, metric, window_secs, max_count, enabled, \
                expires_at, reason, created_by \
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

/// Load every enabled, non-expired rule for a subject. Used by the
/// override merge path in `auth::rbac::compute_user_surface_constraints`.
/// `expires_at IS NULL` rows (permanent) and `expires_at > now()` rows
/// (still active) are both returned.
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
            "SELECT id, subject_kind, subject_id, surface, metric, window_secs, max_count, enabled, \
                    expires_at, reason, created_by \
               FROM rate_limit_rules \
              WHERE subject_kind = $1 AND subject_id = $2 AND enabled = TRUE \
                AND (expires_at IS NULL OR expires_at > now())",
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
    /// Optional UTC expiry. None = permanent. When set, the request-time
    /// override merge ignores rows whose `expires_at` is in the past.
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Operator-supplied justification, surfaced in the audit log and
    /// the admin UI. None allowed for role-default-equivalent rules;
    /// the handler layer enforces "required when an override differs
    /// from the role default".
    pub reason: Option<String>,
    /// Actor who wrote this row. Populated by the handler from
    /// `auth_user.id`; None when no auth context is available (e.g.
    /// system-seeded rules).
    pub created_by: Option<Uuid>,
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
            (subject_kind, subject_id, surface, metric, window_secs, max_count, enabled, \
             expires_at, reason, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
         ON CONFLICT (subject_kind, subject_id, surface, metric, window_secs) \
         DO UPDATE SET max_count  = EXCLUDED.max_count, \
                       enabled    = EXCLUDED.enabled, \
                       expires_at = EXCLUDED.expires_at, \
                       reason     = EXCLUDED.reason, \
                       created_by = EXCLUDED.created_by, \
                       updated_at = now() \
         RETURNING id, subject_kind, subject_id, surface, metric, window_secs, max_count, enabled, \
                   expires_at, reason, created_by",
    )
    .bind(req.subject_kind.as_str())
    .bind(req.subject_id)
    .bind(req.surface.as_str())
    .bind(req.metric.as_str())
    .bind(req.window_secs)
    .bind(req.max_count)
    .bind(req.enabled)
    .bind(req.expires_at)
    .bind(req.reason)
    .bind(req.created_by)
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
        "SELECT id, subject_kind, subject_id, period, limit_tokens, enabled, \
                expires_at, reason, created_by \
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
            "SELECT id, subject_kind, subject_id, period, limit_tokens, enabled, \
                    expires_at, reason, created_by \
               FROM budget_caps \
              WHERE subject_kind = $1 AND subject_id = $2 AND enabled = TRUE \
                AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(kind.as_str())
        .bind(id)
        .fetch_all(pool)
        .await?;
        out.extend(rows.into_iter().filter_map(row_to_cap));
    }
    Ok(out)
}

/// Insert-or-update payload for `upsert_cap`. Mirrors `UpsertRule`'s
/// shape so handlers can share validation scaffolding; same override
/// metadata semantics.
#[derive(Debug, Clone)]
pub struct UpsertCap {
    pub subject_kind: BudgetSubject,
    pub subject_id: Uuid,
    pub period: BudgetPeriod,
    pub limit_tokens: i64,
    pub enabled: bool,
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    pub reason: Option<String>,
    pub created_by: Option<Uuid>,
}

pub async fn upsert_cap(pool: &PgPool, req: UpsertCap) -> Result<BudgetCap, sqlx::Error> {
    if req.limit_tokens <= 0 {
        return Err(sqlx::Error::Protocol("limit_tokens must be > 0".into()));
    }
    let row = sqlx::query(
        "INSERT INTO budget_caps \
            (subject_kind, subject_id, period, limit_tokens, enabled, \
             expires_at, reason, created_by) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         ON CONFLICT (subject_kind, subject_id, period) \
         DO UPDATE SET limit_tokens = EXCLUDED.limit_tokens, \
                       enabled      = EXCLUDED.enabled, \
                       expires_at   = EXCLUDED.expires_at, \
                       reason       = EXCLUDED.reason, \
                       created_by   = EXCLUDED.created_by, \
                       updated_at   = now() \
         RETURNING id, subject_kind, subject_id, period, limit_tokens, enabled, \
                   expires_at, reason, created_by",
    )
    .bind(req.subject_kind.as_str())
    .bind(req.subject_id)
    .bind(req.period.as_str())
    .bind(req.limit_tokens)
    .bind(req.enabled)
    .bind(req.expires_at)
    .bind(req.reason)
    .bind(req.created_by)
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
        // Override metadata. Queries that don't SELECT these columns
        // (or run against the pre-002 schema) get None — safe default.
        expires_at: row.try_get("expires_at").ok(),
        reason: row.try_get("reason").ok(),
        created_by: row.try_get("created_by").ok(),
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
        expires_at: row.try_get("expires_at").ok(),
        reason: row.try_get("reason").ok(),
        created_by: row.try_get("created_by").ok(),
    })
}

// ----------------------------------------------------------------------------
// Per-subject override merge
//
// The engine's role-derived constraints (synthesized by
// `extract_surface_constraints` on each role's policy_document) define
// the baseline for a user. Administrators can then override specific
// (surface, metric, window) rules or (surface, period) budgets via the
// side tables — rows that live in `rate_limit_rules` / `budget_caps`
// keyed by the user's id rather than a role's.
//
// Semantics: each override row REPLACES any matching role-derived
// entry. That is, if a role would give the user 100 rps and a
// non-expired user override row says 500 rps, the effective limit is
// 500 rps — overrides can both tighten AND relax. Budgets are
// per-period only (no surface column on budget_caps today), so a
// budget override replaces the same-period budget on BOTH surfaces.
// Non-matching role entries are preserved untouched.
// ----------------------------------------------------------------------------

/// Turn a list of rate-limit rules and budget caps (typically the
/// enabled+active subset from the side tables for one subject) into
/// the same `SurfaceConstraints` shape role extraction produces, so
/// the two can be merged without special-casing in callers.
pub fn side_table_as_constraints(
    rules: &[RateLimitRule],
    caps: &[BudgetCap],
) -> SurfaceConstraints {
    let mut out = SurfaceConstraints::default();
    for r in rules {
        if !r.enabled {
            continue;
        }
        let block = out
            .block_mut(r.surface)
            .get_or_insert_with(SurfaceBlock::default);
        block.rules.push(SurfaceRule {
            metric: r.metric,
            window_secs: r.window_secs,
            max_count: r.max_count,
            enabled: true,
        });
    }
    for c in caps {
        if !c.enabled {
            continue;
        }
        // Budget caps have no surface column — a cap override applies
        // to both gateway surfaces at once. Push into each.
        for surface in [Surface::AiGateway, Surface::McpGateway] {
            let block = out
                .block_mut(surface)
                .get_or_insert_with(SurfaceBlock::default);
            block.budgets.push(SurfaceBudget {
                period: c.period,
                limit_tokens: c.limit_tokens,
                enabled: true,
            });
        }
    }
    out
}

/// Apply `overrides` on top of a baseline `base` constraint set.
/// Within each surface block, override entries replace base entries
/// that share the same key — `(metric, window_secs)` for rules,
/// `period` for budgets. Entries in `base` that don't match anything
/// in `overrides` pass through untouched.
pub fn apply_user_overrides(
    mut base: SurfaceConstraints,
    overrides: SurfaceConstraints,
) -> SurfaceConstraints {
    apply_block_overrides(&mut base.ai_gateway, overrides.ai_gateway);
    apply_block_overrides(&mut base.mcp_gateway, overrides.mcp_gateway);
    base
}

fn apply_block_overrides(target: &mut Option<SurfaceBlock>, overrides: Option<SurfaceBlock>) {
    let Some(ov) = overrides else { return };
    let block = target.get_or_insert_with(SurfaceBlock::default);
    for ov_rule in ov.rules {
        block
            .rules
            .retain(|r| !(r.metric == ov_rule.metric && r.window_secs == ov_rule.window_secs));
        block.rules.push(ov_rule);
    }
    for ov_budget in ov.budgets {
        block.budgets.retain(|b| b.period != ov_budget.period);
        block.budgets.push(ov_budget);
    }
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

/// Walk every persisted rule + every model weight and refuse to start
/// if anything is out of range. The migration's CHECK constraints
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
          WHERE input_weight <= 0 OR output_weight <= 0",
    )
    .fetch_all(pool)
    .await?;
    if !bad_models.is_empty() {
        let list: Vec<String> = bad_models.iter().map(|(m,)| m.clone()).collect();
        anyhow::bail!(
            "Found models with non-positive input/output weight: {}",
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

    #[test]
    fn override_replaces_matching_role_rule() {
        // Role gives 100 req/min; override bumps to 500.
        let role = SurfaceConstraints {
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
        let overrides = SurfaceConstraints {
            ai_gateway: Some(SurfaceBlock {
                rules: vec![SurfaceRule {
                    metric: RateMetric::Requests,
                    window_secs: 60,
                    max_count: 500,
                    enabled: true,
                }],
                budgets: vec![],
            }),
            mcp_gateway: None,
        };
        let merged = apply_user_overrides(role, overrides);
        let rules = &merged.ai_gateway.as_ref().unwrap().rules;
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].max_count, 500, "override should replace role");
    }

    #[test]
    fn override_tightens_role_rule_when_lower() {
        // Override strictly below role — same replace semantic, just
        // the tightening direction. This is the "compromised key"
        // use case: admin clamps to 1 req/min temporarily.
        let role = SurfaceConstraints {
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
        let overrides = SurfaceConstraints {
            ai_gateway: Some(SurfaceBlock {
                rules: vec![SurfaceRule {
                    metric: RateMetric::Requests,
                    window_secs: 60,
                    max_count: 1,
                    enabled: true,
                }],
                budgets: vec![],
            }),
            mcp_gateway: None,
        };
        let merged = apply_user_overrides(role, overrides);
        assert_eq!(merged.ai_gateway.unwrap().rules[0].max_count, 1);
    }

    #[test]
    fn override_preserves_non_matching_role_rules() {
        // Role has two rules (requests/min, tokens/day). Override
        // only touches tokens/day. The requests/min rule must remain.
        let role = SurfaceConstraints {
            ai_gateway: Some(SurfaceBlock {
                rules: vec![
                    SurfaceRule {
                        metric: RateMetric::Requests,
                        window_secs: 60,
                        max_count: 100,
                        enabled: true,
                    },
                    SurfaceRule {
                        metric: RateMetric::Tokens,
                        window_secs: 86_400,
                        max_count: 1_000_000,
                        enabled: true,
                    },
                ],
                budgets: vec![],
            }),
            mcp_gateway: None,
        };
        let overrides = SurfaceConstraints {
            ai_gateway: Some(SurfaceBlock {
                rules: vec![SurfaceRule {
                    metric: RateMetric::Tokens,
                    window_secs: 86_400,
                    max_count: 5_000_000,
                    enabled: true,
                }],
                budgets: vec![],
            }),
            mcp_gateway: None,
        };
        let merged = apply_user_overrides(role, overrides);
        let rules = &merged.ai_gateway.as_ref().unwrap().rules;
        assert_eq!(rules.len(), 2);
        let tokens_rule = rules
            .iter()
            .find(|r| r.metric == RateMetric::Tokens)
            .unwrap();
        assert_eq!(tokens_rule.max_count, 5_000_000);
        let req_rule = rules
            .iter()
            .find(|r| r.metric == RateMetric::Requests)
            .unwrap();
        assert_eq!(req_rule.max_count, 100, "untouched rule must survive");
    }

    #[test]
    fn override_adds_rule_to_surface_without_role_coverage() {
        // Role doesn't set an mcp_gateway block at all. An override
        // for mcp_gateway should still get applied.
        let role = SurfaceConstraints::default();
        let overrides = SurfaceConstraints {
            ai_gateway: None,
            mcp_gateway: Some(SurfaceBlock {
                rules: vec![SurfaceRule {
                    metric: RateMetric::Requests,
                    window_secs: 60,
                    max_count: 10,
                    enabled: true,
                }],
                budgets: vec![],
            }),
        };
        let merged = apply_user_overrides(role, overrides);
        assert_eq!(
            merged.mcp_gateway.unwrap().rules[0].max_count,
            10,
            "override should install onto empty surface"
        );
    }

    #[test]
    fn override_budget_replaces_matching_period() {
        let role = SurfaceConstraints {
            ai_gateway: Some(SurfaceBlock {
                rules: vec![],
                budgets: vec![SurfaceBudget {
                    period: BudgetPeriod::Monthly,
                    limit_tokens: 1_000_000,
                    enabled: true,
                }],
            }),
            mcp_gateway: None,
        };
        let overrides = SurfaceConstraints {
            ai_gateway: Some(SurfaceBlock {
                rules: vec![],
                budgets: vec![SurfaceBudget {
                    period: BudgetPeriod::Monthly,
                    limit_tokens: 5_000_000,
                    enabled: true,
                }],
            }),
            mcp_gateway: None,
        };
        let merged = apply_user_overrides(role, overrides);
        let budgets = &merged.ai_gateway.as_ref().unwrap().budgets;
        assert_eq!(budgets.len(), 1);
        assert_eq!(budgets[0].limit_tokens, 5_000_000);
    }

    #[test]
    fn side_table_as_constraints_skips_disabled_rows() {
        let rule = RateLimitRule {
            id: Uuid::nil(),
            subject_kind: RateLimitSubject::User,
            subject_id: Uuid::nil(),
            surface: Surface::AiGateway,
            metric: RateMetric::Requests,
            window_secs: 60,
            max_count: 10,
            enabled: false, // disabled → should not appear in constraints
            expires_at: None,
            reason: None,
            created_by: None,
        };
        let out = side_table_as_constraints(&[rule], &[]);
        assert!(out.ai_gateway.is_none());
    }

    #[test]
    fn side_table_budget_applies_to_both_surfaces() {
        // budget_caps has no surface column so an override must
        // install itself onto BOTH gateway surfaces.
        let cap = BudgetCap {
            id: Uuid::nil(),
            subject_kind: BudgetSubject::User,
            subject_id: Uuid::nil(),
            period: BudgetPeriod::Monthly,
            limit_tokens: 42,
            enabled: true,
            expires_at: None,
            reason: None,
            created_by: None,
        };
        let out = side_table_as_constraints(&[], &[cap]);
        assert_eq!(out.ai_gateway.as_ref().unwrap().budgets[0].limit_tokens, 42);
        assert_eq!(
            out.mcp_gateway.as_ref().unwrap().budgets[0].limit_tokens,
            42
        );
    }

    #[test]
    fn window_to_secs_roundtrip() {
        for (s, expected) in [
            ("1m", 60),
            ("5m", 300),
            ("1h", 3_600),
            ("5h", 18_000),
            ("1d", 86_400),
            ("1w", 604_800),
        ] {
            assert_eq!(window_to_secs(s), Some(expected), "parse failed for {s}");
            assert_eq!(
                secs_to_window(expected),
                Some(s),
                "format failed for {expected}"
            );
        }
        assert_eq!(window_to_secs("bogus"), None);
        assert_eq!(secs_to_window(42), None);
    }

    #[test]
    fn extract_permissions_wildcard() {
        let doc = serde_json::json!({
            "Version": "2024-01-01",
            "Statement": [{"Effect":"Allow","Action":"*","Resource":"*"}]
        });
        let all = &["ai_gateway:use", "providers:read", "roles:delete"];
        let perms = extract_permissions(&doc, all);
        assert_eq!(perms.len(), 3);
        assert!(perms.contains(&"ai_gateway:use".to_string()));
    }

    #[test]
    fn extract_permissions_prefix_wildcard() {
        let doc = serde_json::json!({
            "Version": "2024-01-01",
            "Statement": [{"Effect":"Allow","Action":"providers:*","Resource":"*"}]
        });
        let all = &["providers:read", "providers:write", "models:read"];
        let perms = extract_permissions(&doc, all);
        assert_eq!(perms, vec!["providers:read", "providers:write"]);
    }

    #[test]
    fn extract_allowed_models_unrestricted() {
        let doc = serde_json::json!({
            "Version": "2024-01-01",
            "Statement": [{"Effect":"Allow","Action":"ai_gateway:use","Resource":"*"}]
        });
        assert_eq!(extract_allowed_models(&doc), None);
    }

    #[test]
    fn extract_allowed_models_scoped() {
        let doc = serde_json::json!({
            "Version": "2024-01-01",
            "Statement": [{
                "Effect":"Allow",
                "Action":"ai_gateway:use",
                "Resource":["model:gpt-4o","model:claude-sonnet-4-20250514"]
            }]
        });
        let models = extract_allowed_models(&doc).unwrap();
        assert_eq!(models, vec!["claude-sonnet-4-20250514", "gpt-4o"]);
    }

    #[test]
    fn extract_surface_constraints_from_policy() {
        let doc = serde_json::json!({
            "Version": "2024-01-01",
            "Statement": [{
                "Effect":"Allow",
                "Action":"ai_gateway:use",
                "Resource":"*",
                "Constraints": {
                    "RateLimits": [{"Metric":"requests","Window":"1h","MaxCount":100}],
                    "Budgets": [{"Period":"daily","MaxTokens":1000000}]
                }
            }]
        });
        let sc = extract_surface_constraints(&doc);
        let ai = sc.ai_gateway.unwrap();
        assert_eq!(ai.rules.len(), 1);
        assert_eq!(ai.rules[0].window_secs, 3600);
        assert_eq!(ai.rules[0].max_count, 100);
        assert_eq!(ai.budgets[0].limit_tokens, 1_000_000);
        assert!(sc.mcp_gateway.is_none());
    }

    #[test]
    fn validate_policy_constraints_rejects_bad_window() {
        let c = PolicyConstraints {
            rate_limits: vec![PolicyRateLimit {
                metric: "requests".into(),
                window: "2h".into(),
                max_count: 10,
            }],
            budgets: vec![],
        };
        assert!(validate_policy_constraints(&c).is_err());
    }
}

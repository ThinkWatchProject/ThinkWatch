//! Model catalog repository — the `models` table (what clients see on
//! `/v1/models`) and `model_routes` (which provider serves each one).
//!
//! Thin wrappers over sqlx, one statement (or one transaction) per
//! function; validation, audit and the router / weight-cache refresh stay
//! in `handlers::models`.

use rust_decimal::Decimal;
use serde::Serialize;
use sqlx::PgPool;
use think_watch_common::errors::AppError;
use think_watch_common::models::Model;
use uuid::Uuid;

/// Row shape returned by `GET /api/admin/models`. Route counts are
/// joined in so the UI can show "active / draft / unrouted" status
/// without a second round-trip.
#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ModelRow {
    pub id: Uuid,
    pub model_id: String,
    pub display_name: String,
    #[schema(value_type = f64)]
    pub input_weight: Decimal,
    #[schema(value_type = f64)]
    pub output_weight: Decimal,
    /// Cache weights as stored. `None` ⇒ derived from `input_weight`.
    #[schema(value_type = Option<f64>)]
    pub cache_read_weight: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub cache_write_weight: Option<Decimal>,
    #[schema(value_type = Option<f64>)]
    pub cache_write_1h_weight: Option<Decimal>,
    pub route_count: i64,
    pub enabled_route_count: i64,
    /// Model-level kill switch. FALSE ⇒ all routes are skipped at
    /// router-bootstrap (gateway behaves as if the model has no routes).
    /// Independent of per-route `enabled` so flipping back restores the
    /// previous traffic split exactly.
    pub enabled: bool,
    /// Provider display names (or `name` if display_name is null) for
    /// every route attached to the model, ordered by weight DESC. Lets
    /// the list table show "who serves this?" without an extra fetch.
    pub providers: Vec<String>,
    /// Per-model routing override. `None` ⇒ inherit
    /// `gateway.default_routing_strategy`. The detail drawer reads this
    /// to label the strategy picker — without it, refetch-after-PATCH
    /// can't reflect the new value.
    pub routing_strategy: Option<String>,
    pub affinity_mode: Option<String>,
    pub affinity_ttl_secs: Option<i32>,
    /// Output guardrails as stored in JSONB. The list endpoint returns
    /// the raw `Value` (rather than `Vec<OutputGuardrail>`) so the UI
    /// can render unrecognised future variants without breaking. The
    /// shape is `[{ "type": "max_length", "max_chars": N }, ...]`.
    #[schema(value_type = serde_json::Value)]
    pub output_guardrails: serde_json::Value,
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ModelIdRow {
    pub model_id: String,
    pub display_name: String,
}

#[derive(Debug, Serialize, sqlx::FromRow, utoipa::ToSchema)]
pub struct ModelRouteRow {
    pub id: Uuid,
    pub model_id: String,
    pub provider_id: Uuid,
    pub provider_name: String,
    pub upstream_model: String,
    pub weight: i32,
    pub enabled: bool,
    /// Optional human-readable identifier (e.g. "EU-primary"). Pure
    /// metadata for the admin UI; ignored by the routing layer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Free-form note. Surfaced in the edit dialog only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Per-route RPM cap. NULL = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_cap: Option<i32>,
    /// Per-route TPM cap. NULL = unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm_cap: Option<i32>,
}

// ---------------------------------------------------------------------------
// models
// ---------------------------------------------------------------------------

/// The columns a `Model` is read with.
const MODEL_COLUMNS: &str = "id, model_id, display_name, input_weight, output_weight, \
     cache_read_weight, cache_write_weight, cache_write_1h_weight, \
     routing_strategy, affinity_mode, affinity_ttl_secs, tags, enabled, \
     output_guardrails";

/// One page of the catalog, and the total matching `search` / `status`.
///
/// `status`:
///   'active'    — m.enabled = true AND enabled_route_count > 0
///   'disabled'  — m.enabled = false, OR
///                 (m.enabled = true AND route_count > 0 AND enabled_route_count = 0)
///   'unrouted'  — route_count = 0
///   otherwise   — no filter
///
/// `route_count` / `enabled_route_count` come from a `LATERAL` subquery so
/// the filter happens on the joined shape; PG rewrites this to a
/// HashAggregate over `model_routes`.
pub async fn list(
    pool: &PgPool,
    search: &str,
    status: &str,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<ModelRow>), AppError> {
    let search_pattern = format!("%{search}%");
    let status_filter_sql = match status {
        "active" => "AND m.enabled = true AND rc.enabled_route_count > 0",
        "disabled" => {
            "AND (m.enabled = false OR (rc.route_count > 0 AND rc.enabled_route_count = 0))"
        }
        "unrouted" => "AND rc.route_count = 0",
        _ => "",
    };

    let total_sql = format!(
        r#"SELECT COUNT(*) FROM models m
           LEFT JOIN LATERAL (
             SELECT COUNT(*)                                 AS route_count,
                    COUNT(*) FILTER (WHERE mr.enabled = true) AS enabled_route_count
             FROM model_routes mr
             JOIN providers p ON p.id = mr.provider_id AND p.deleted_at IS NULL
             WHERE mr.model_id = m.model_id
           ) rc ON true
           WHERE ($1 = '' OR m.model_id ILIKE $2 OR m.display_name ILIKE $2)
             {status_filter_sql}"#,
    );
    let list_sql = format!(
        r#"SELECT m.id, m.model_id, m.display_name,
                  m.input_weight, m.output_weight,
                  m.cache_read_weight, m.cache_write_weight, m.cache_write_1h_weight,
                  COALESCE(rc.route_count, 0)         AS route_count,
                  COALESCE(rc.enabled_route_count, 0) AS enabled_route_count,
                  m.enabled,
                  COALESCE(rc.providers, '{{}}'::text[]) AS providers,
                  m.routing_strategy, m.affinity_mode, m.affinity_ttl_secs,
                  m.output_guardrails
           FROM models m
           LEFT JOIN LATERAL (
             SELECT COUNT(*)                                 AS route_count,
                    COUNT(*) FILTER (WHERE mr.enabled = true) AS enabled_route_count,
                    array_agg(COALESCE(p.display_name, p.name)
                              ORDER BY mr.weight DESC, p.name) AS providers
             FROM model_routes mr
             JOIN providers p ON p.id = mr.provider_id AND p.deleted_at IS NULL
             WHERE mr.model_id = m.model_id
           ) rc ON true
           WHERE ($1 = '' OR m.model_id ILIKE $2 OR m.display_name ILIKE $2)
             {status_filter_sql}
           ORDER BY m.model_id
           LIMIT $3 OFFSET $4"#,
    );

    let total: Option<i64> = sqlx::query_scalar(&total_sql)
        .bind(search)
        .bind(&search_pattern)
        .fetch_one(pool)
        .await?;
    let rows = sqlx::query_as::<_, ModelRow>(&list_sql)
        .bind(search)
        .bind(&search_pattern)
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
    Ok((total.unwrap_or(0), rows))
}

/// Every column a new catalog entry is written with.
pub struct ModelFields<'a> {
    pub display_name: &'a str,
    pub input_weight: Decimal,
    pub output_weight: Decimal,
    pub routing_strategy: Option<&'a str>,
    pub affinity_mode: Option<&'a str>,
    pub affinity_ttl_secs: Option<i32>,
    pub tags: Option<&'a [String]>,
    pub output_guardrails: &'a serde_json::Value,
    /// Read, 5-minute write, 1-hour write.
    pub cache_weights: [Option<Decimal>; 3],
}

pub async fn insert(pool: &PgPool, model_id: &str, f: &ModelFields<'_>) -> Result<Model, AppError> {
    let sql = format!(
        r#"INSERT INTO models
              (model_id, display_name, input_weight, output_weight,
               routing_strategy, affinity_mode, affinity_ttl_secs, tags,
               output_guardrails,
               cache_read_weight, cache_write_weight, cache_write_1h_weight)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
           RETURNING {MODEL_COLUMNS}"#
    );
    Ok(sqlx::query_as::<_, Model>(&sql)
        .bind(model_id)
        .bind(f.display_name)
        .bind(f.input_weight)
        .bind(f.output_weight)
        .bind(f.routing_strategy)
        .bind(f.affinity_mode)
        .bind(f.affinity_ttl_secs)
        .bind(f.tags)
        .bind(f.output_guardrails)
        .bind(f.cache_weights[0])
        .bind(f.cache_weights[1])
        .bind(f.cache_weights[2])
        .fetch_one(pool)
        .await?)
}

pub async fn find(pool: &PgPool, id: Uuid) -> Result<Option<Model>, AppError> {
    let sql = format!("SELECT {MODEL_COLUMNS} FROM models WHERE id = $1");
    Ok(sqlx::query_as::<_, Model>(&sql)
        .bind(id)
        .fetch_optional(pool)
        .await?)
}

/// Overwrite every editable column of one catalog entry.
pub async fn update(
    pool: &PgPool,
    id: Uuid,
    f: &ModelFields<'_>,
    enabled: bool,
) -> Result<Model, AppError> {
    let sql = format!(
        r#"UPDATE models SET
              display_name      = $2,
              input_weight      = $3,
              output_weight     = $4,
              routing_strategy  = $5,
              affinity_mode     = $6,
              affinity_ttl_secs = $7,
              tags              = $8,
              enabled           = $9,
              output_guardrails = $10,
              cache_read_weight     = $11,
              cache_write_weight    = $12,
              cache_write_1h_weight = $13
           WHERE id = $1
           RETURNING {MODEL_COLUMNS}"#
    );
    Ok(sqlx::query_as::<_, Model>(&sql)
        .bind(id)
        .bind(f.display_name)
        .bind(f.input_weight)
        .bind(f.output_weight)
        .bind(f.routing_strategy)
        .bind(f.affinity_mode)
        .bind(f.affinity_ttl_secs)
        .bind(f.tags)
        .bind(enabled)
        .bind(f.output_guardrails)
        .bind(f.cache_weights[0])
        .bind(f.cache_weights[1])
        .bind(f.cache_weights[2])
        .fetch_one(pool)
        .await?)
}

/// The exposed `model_id` of a catalog row, by its primary key.
pub async fn model_id_of(pool: &PgPool, id: Uuid) -> Result<Option<String>, AppError> {
    Ok(
        sqlx::query_scalar("SELECT model_id FROM models WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?,
    )
}

pub async fn exists(pool: &PgPool, model_id: &str) -> Result<bool, AppError> {
    let found: Option<String> =
        sqlx::query_scalar("SELECT model_id FROM models WHERE model_id = $1")
            .bind(model_id)
            .fetch_optional(pool)
            .await?;
    Ok(found.is_some())
}

pub async fn delete(pool: &PgPool, id: Uuid) -> Result<(), AppError> {
    sqlx::query("DELETE FROM models WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// Every exposed id with its display name, unpaginated.
pub async fn list_ids(pool: &PgPool) -> Result<Vec<ModelIdRow>, AppError> {
    Ok(sqlx::query_as::<_, ModelIdRow>(
        "SELECT model_id, display_name FROM models ORDER BY model_id",
    )
    .fetch_all(pool)
    .await?)
}

/// Delete catalog entries with no route to a live provider. Returns how
/// many went.
///
/// `model_routes.provider_id` has `ON DELETE CASCADE`, so soft-deleted
/// providers still count as "having a route" unless filtered by the
/// provider's `deleted_at IS NULL`.
pub async fn delete_unrouted(pool: &PgPool) -> Result<u64, AppError> {
    let result = sqlx::query(
        r#"DELETE FROM models
           WHERE model_id NOT IN (
             SELECT DISTINCT mr.model_id
             FROM model_routes mr
             JOIN providers p ON p.id = mr.provider_id AND p.deleted_at IS NULL
           )"#,
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

/// Delete catalog entries by id (routes cascade). Returns how many went.
pub async fn delete_many(pool: &PgPool, ids: &[Uuid]) -> Result<u64, AppError> {
    let result = sqlx::query("DELETE FROM models WHERE id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Flip the model-level kill switch. Returns how many rows changed.
pub async fn set_enabled_many(pool: &PgPool, ids: &[Uuid], enabled: bool) -> Result<u64, AppError> {
    let result = sqlx::query(
        r#"UPDATE models
              SET enabled = $2
            WHERE id = ANY($1)
              AND enabled IS DISTINCT FROM $2"#,
    )
    .bind(ids)
    .bind(enabled)
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

// ---------------------------------------------------------------------------
// model_routes
// ---------------------------------------------------------------------------

/// A model's routes to live providers, in creation order — so the routes
/// table and the traffic-share sliders stay in place while admins drag
/// weights.
pub async fn routes_of(pool: &PgPool, model_id: &str) -> Result<Vec<ModelRouteRow>, AppError> {
    Ok(sqlx::query_as::<_, ModelRouteRow>(
        r#"SELECT mr.id, mr.model_id, mr.provider_id, p.name AS provider_name,
                  mr.upstream_model, mr.weight, mr.enabled,
                  mr.label, mr.notes, mr.rpm_cap, mr.tpm_cap
           FROM model_routes mr
           JOIN providers p ON p.id = mr.provider_id
           WHERE mr.model_id = $1 AND p.deleted_at IS NULL
           ORDER BY mr.created_at, mr.id"#,
    )
    .bind(model_id)
    .fetch_all(pool)
    .await?)
}

/// Is there already a route for this (model, provider, upstream model)?
/// That triple is the uniqueness key: the same provider with a different
/// upstream is a legal second route.
pub async fn route_exists(
    pool: &PgPool,
    model_id: &str,
    provider_id: Uuid,
    upstream_model: &str,
) -> Result<bool, AppError> {
    let existing: Option<Uuid> = sqlx::query_scalar(
        r#"SELECT id FROM model_routes
           WHERE model_id = $1
             AND provider_id = $2
             AND upstream_model = $3"#,
    )
    .bind(model_id)
    .bind(provider_id)
    .bind(upstream_model)
    .fetch_optional(pool)
    .await?;
    Ok(existing.is_some())
}

pub struct NewRoute<'a> {
    pub model_id: &'a str,
    pub provider_id: Uuid,
    pub upstream_model: &'a str,
    pub weight: i32,
    pub enabled: bool,
    pub label: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub rpm_cap: Option<i32>,
    pub tpm_cap: Option<i32>,
    pub upstream_protocol: Option<&'a str>,
}

pub async fn insert_route(pool: &PgPool, r: &NewRoute<'_>) -> Result<ModelRouteRow, AppError> {
    Ok(sqlx::query_as::<_, ModelRouteRow>(
        r#"INSERT INTO model_routes
              (model_id, provider_id, upstream_model, weight, enabled,
               label, notes, rpm_cap, tpm_cap, upstream_protocol)
           VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
           RETURNING id, model_id, provider_id,
                     (SELECT name FROM providers WHERE id = provider_id) AS provider_name,
                     upstream_model, weight, enabled,
                     label, notes, rpm_cap, tpm_cap"#,
    )
    .bind(r.model_id)
    .bind(r.provider_id)
    .bind(r.upstream_model)
    .bind(r.weight)
    .bind(r.enabled)
    .bind(r.label)
    .bind(r.notes)
    .bind(r.rpm_cap)
    .bind(r.tpm_cap)
    .bind(r.upstream_protocol)
    .fetch_one(pool)
    .await?)
}

/// A PATCH to one route. `None` leaves a column alone; for the clearable
/// ones, `Some(None)` clears it.
pub struct RouteUpdate<'a> {
    pub upstream_model: Option<&'a str>,
    pub weight: Option<i32>,
    pub enabled: Option<bool>,
    pub label: Option<Option<&'a str>>,
    pub notes: Option<Option<&'a str>>,
    pub rpm_cap: Option<Option<i32>>,
    pub tpm_cap: Option<Option<i32>>,
}

/// `None` when there is no such route.
pub async fn update_route(
    pool: &PgPool,
    route_id: Uuid,
    u: &RouteUpdate<'_>,
) -> Result<Option<ModelRouteRow>, AppError> {
    Ok(sqlx::query_as::<_, ModelRouteRow>(
        r#"UPDATE model_routes SET
              upstream_model = COALESCE($2, upstream_model),
              weight   = COALESCE($3, weight),
              enabled  = COALESCE($4, enabled),
              label    = CASE WHEN $6  THEN $5  ELSE label    END,
              notes    = CASE WHEN $8  THEN $7  ELSE notes    END,
              rpm_cap  = CASE WHEN $10 THEN $9  ELSE rpm_cap  END,
              tpm_cap  = CASE WHEN $12 THEN $11 ELSE tpm_cap  END
           WHERE id = $1
           RETURNING id, model_id, provider_id,
                     (SELECT name FROM providers WHERE id = provider_id) AS provider_name,
                     upstream_model, weight, enabled,
                     label, notes, rpm_cap, tpm_cap"#,
    )
    .bind(route_id)
    .bind(u.upstream_model)
    .bind(u.weight)
    .bind(u.enabled)
    .bind(u.label.flatten())
    .bind(u.label.is_some())
    .bind(u.notes.flatten())
    .bind(u.notes.is_some())
    .bind(u.rpm_cap.flatten())
    .bind(u.rpm_cap.is_some())
    .bind(u.tpm_cap.flatten())
    .bind(u.tpm_cap.is_some())
    .fetch_optional(pool)
    .await?)
}

/// Returns whether the route existed.
pub async fn delete_route(pool: &PgPool, route_id: Uuid) -> Result<bool, AppError> {
    let result = sqlx::query("DELETE FROM model_routes WHERE id = $1")
        .bind(route_id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// One page of every route to a live provider, filtered by a search over
/// model id and provider name and by provider, and the total matching.
pub async fn list_routes(
    pool: &PgPool,
    search: &str,
    provider_id: Option<Uuid>,
    limit: i64,
    offset: i64,
) -> Result<(i64, Vec<ModelRouteRow>), AppError> {
    if search.is_empty() && provider_id.is_none() {
        let total: Option<i64> = sqlx::query_scalar(
            "SELECT COUNT(*) FROM model_routes mr JOIN providers p ON p.id = mr.provider_id WHERE p.deleted_at IS NULL",
        )
        .fetch_one(pool)
        .await?;
        let rows = sqlx::query_as::<_, ModelRouteRow>(
            r#"SELECT mr.id, mr.model_id, mr.provider_id, p.name AS provider_name,
                      mr.upstream_model, mr.weight, mr.enabled,
                      mr.label, mr.notes, mr.rpm_cap, mr.tpm_cap
               FROM model_routes mr
               JOIN providers p ON p.id = mr.provider_id
               WHERE p.deleted_at IS NULL
               ORDER BY mr.model_id, mr.weight DESC
               LIMIT $1 OFFSET $2"#,
        )
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await?;
        return Ok((total.unwrap_or(0), rows));
    }
    let search_pattern = format!("%{search}%");
    let total: Option<i64> = sqlx::query_scalar(
        r#"SELECT COUNT(*) FROM model_routes mr
           JOIN providers p ON p.id = mr.provider_id
           WHERE p.deleted_at IS NULL
             AND ($1 = '' OR mr.model_id ILIKE $2 OR p.name ILIKE $2)
             AND ($3::UUID IS NULL OR mr.provider_id = $3)"#,
    )
    .bind(search)
    .bind(&search_pattern)
    .bind(provider_id)
    .fetch_one(pool)
    .await?;
    let rows = sqlx::query_as::<_, ModelRouteRow>(
        r#"SELECT mr.id, mr.model_id, mr.provider_id, p.name AS provider_name,
                  mr.upstream_model, mr.weight, mr.enabled,
                  mr.label, mr.notes, mr.rpm_cap, mr.tpm_cap
           FROM model_routes mr
           JOIN providers p ON p.id = mr.provider_id
           WHERE p.deleted_at IS NULL
             AND ($1 = '' OR mr.model_id ILIKE $2 OR p.name ILIKE $2)
             AND ($3::UUID IS NULL OR mr.provider_id = $3)
           ORDER BY mr.model_id, mr.weight DESC
           LIMIT $4 OFFSET $5"#,
    )
    .bind(search)
    .bind(&search_pattern)
    .bind(provider_id)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await?;
    Ok((total.unwrap_or(0), rows))
}

/// Routes to create for one provider in a batch import. The vectors of
/// each half are parallel.
#[derive(Default)]
pub struct RouteImport {
    /// New catalog entries: the exposed id, the provider's name for it,
    /// and the protocol the probe found (if any).
    pub new_exposed: Vec<String>,
    pub new_upstreams: Vec<String>,
    pub new_protocols: Vec<Option<String>>,
    /// Routes onto existing catalog entries.
    pub attach_targets: Vec<String>,
    pub attach_upstreams: Vec<String>,
    pub attach_protocols: Vec<Option<String>>,
}

/// Create a batch import's catalog entries and routes in one transaction,
/// one bulk statement per half. Returns how many routes landed (existing
/// ones are skipped, as are attach targets that do not exist).
pub async fn import_routes(
    pool: &PgPool,
    provider_id: Uuid,
    import: &RouteImport,
) -> Result<i64, AppError> {
    let mut tx = pool.begin().await?;

    // Catalog insert is idempotent. Route insert counts rows via the
    // RETURNING/CTE pattern so the count reflects only rows that actually
    // landed (skipping ON CONFLICT dupes).
    let new_inserted: i64 = if import.new_exposed.is_empty() {
        0
    } else {
        sqlx::query(
            r#"INSERT INTO models (model_id, display_name)
               SELECT exposed, exposed
               FROM UNNEST($1::TEXT[]) AS t(exposed)
               ON CONFLICT (model_id) DO NOTHING"#,
        )
        .bind(&import.new_exposed)
        .execute(&mut *tx)
        .await?;

        sqlx::query_scalar::<_, i64>(
            r#"WITH ins AS (
                 INSERT INTO model_routes
                     (model_id, provider_id, upstream_model, weight, upstream_protocol)
                 SELECT exposed, $3, upstream, 100, protocol
                 FROM UNNEST($1::TEXT[], $2::TEXT[], $4::TEXT[])
                   AS t(exposed, upstream, protocol)
                 ON CONFLICT (model_id, provider_id, upstream_model) DO NOTHING
                 RETURNING 1
               )
               SELECT COUNT(*) FROM ins"#,
        )
        .bind(&import.new_exposed)
        .bind(&import.new_upstreams)
        .bind(provider_id)
        .bind(&import.new_protocols)
        .fetch_one(&mut *tx)
        .await?
    };

    // Targets that don't exist in `models` are silently skipped (EXISTS
    // guard) to avoid a FK failure on a typo.
    let attach_inserted: i64 = if import.attach_targets.is_empty() {
        0
    } else {
        sqlx::query_scalar::<_, i64>(
            r#"WITH ins AS (
                 INSERT INTO model_routes
                     (model_id, provider_id, upstream_model, weight, upstream_protocol)
                 SELECT t.target, $3, t.upstream, 100, t.protocol
                 FROM UNNEST($1::TEXT[], $2::TEXT[], $4::TEXT[])
                   AS t(target, upstream, protocol)
                 WHERE EXISTS (SELECT 1 FROM models m WHERE m.model_id = t.target)
                 ON CONFLICT (model_id, provider_id, upstream_model) DO NOTHING
                 RETURNING 1
               )
               SELECT COUNT(*) FROM ins"#,
        )
        .bind(&import.attach_targets)
        .bind(&import.attach_upstreams)
        .bind(provider_id)
        .bind(&import.attach_protocols)
        .fetch_one(&mut *tx)
        .await?
    };

    tx.commit().await?;
    Ok(new_inserted + attach_inserted)
}

/// Returns how many routes went.
pub async fn delete_routes(pool: &PgPool, ids: &[Uuid]) -> Result<u64, AppError> {
    let result = sqlx::query("DELETE FROM model_routes WHERE id = ANY($1)")
        .bind(ids)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// Set many routes' weights in one transaction, so a partial failure
/// rolls back. Returns how many rows changed.
pub async fn set_route_weights(pool: &PgPool, weights: &[(Uuid, i32)]) -> Result<u64, AppError> {
    let mut tx = pool.begin().await?;
    let mut updated = 0;
    for (id, weight) in weights {
        let result = sqlx::query("UPDATE model_routes SET weight = $1 WHERE id = $2")
            .bind(weight)
            .bind(id)
            .execute(&mut *tx)
            .await?;
        updated += result.rows_affected();
    }
    tx.commit().await?;
    Ok(updated)
}

/// Returns how many routes changed.
pub async fn set_routes_enabled(
    pool: &PgPool,
    ids: &[Uuid],
    enabled: bool,
) -> Result<u64, AppError> {
    let result = sqlx::query("UPDATE model_routes SET enabled = $1 WHERE id = ANY($2)")
        .bind(enabled)
        .bind(ids)
        .execute(pool)
        .await?;
    Ok(result.rows_affected())
}

/// What `gateway_logs` records for a route — (model id, provider name,
/// upstream model) — since the log has no route id. `None` when the route
/// is gone or its provider deleted.
pub async fn route_log_identity(
    pool: &PgPool,
    route_id: Uuid,
) -> Result<Option<(String, String, String)>, sqlx::Error> {
    sqlx::query_as::<_, (String, String, String)>(
        "SELECT mr.model_id, p.name, mr.upstream_model \
         FROM model_routes mr \
         JOIN providers p ON p.id = mr.provider_id AND p.deleted_at IS NULL \
         WHERE mr.id = $1",
    )
    .bind(route_id)
    .fetch_optional(pool)
    .await
}

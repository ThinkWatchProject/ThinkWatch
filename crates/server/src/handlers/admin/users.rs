//! User admin handlers — list / create / update / delete / force-logout
//! / reset-password / role assignments. Lifted out of the parent
//! `admin.rs` so each admin resource lives in its own focused module.

use axum::Json;
use axum::extract::{Path, Query, State};
use serde::{Deserialize, Serialize};

use think_watch_auth::password;
use think_watch_common::dto::{
    PaginatedResponse, PaginationParams, RoleAssignment, RoleAssignmentRequest, UserResponse,
};
use think_watch_common::errors::AppError;
use think_watch_common::models::User;
use think_watch_common::validation::{normalize_email, validate_email, validate_password};

use crate::app::AppState;
use crate::middleware::auth_guard::{AuthUser, invalidate_user_perms};
use crate::services::{role_repository, user_repository};

/// Parse a scope string into the `(scope_kind, scope_id)` tuple that
/// `rbac_role_assignments` stores. Accepted shapes:
///
///   "global"           → ("global", None)
///   "team:<uuid>"      → ("team",   Some(uuid))
///
/// Anything else is rejected with a 400. The schema only knows two
/// scope kinds: `global` and `team`.
pub(crate) fn parse_scope(input: &str) -> Result<(String, Option<uuid::Uuid>), AppError> {
    let trimmed = input.trim();
    if trimmed == "global" || trimmed.is_empty() {
        return Ok(("global".into(), None));
    }
    if let Some((kind, rest)) = trimmed.split_once(':') {
        let kind = kind.trim();
        if kind != "team" {
            return Err(AppError::BadRequest(format!(
                "Unknown scope kind '{kind}' (expected 'global' or 'team:<uuid>')"
            )));
        }
        let id = uuid::Uuid::parse_str(rest.trim())
            .map_err(|_| AppError::BadRequest(format!("Invalid UUID in scope '{input}'")))?;
        return Ok((kind.into(), Some(id)));
    }
    Err(AppError::BadRequest(format!(
        "Invalid scope '{input}' (expected 'global' or 'team:<uuid>')"
    )))
}

// --- User management ---

#[derive(Debug, Deserialize)]
pub struct ListUsersQuery {
    pub page: Option<u32>,
    pub per_page: Option<u32>,
    /// Case-insensitive substring match against email + display_name.
    pub search: Option<String>,
}

impl ListUsersQuery {
    fn pagination(&self) -> PaginationParams {
        PaginationParams {
            page: self.page,
            per_page: self.per_page,
        }
    }
}

/// Wrap a user-supplied search term in `%…%` for `ILIKE` substring
/// matching, escaping `\`, `%`, and `_` so they are treated as literals.
///
/// This is NOT about SQL injection — the returned string is bound as a
/// `$N` parameter, so Postgres never parses it as SQL. The escaping is
/// purely about `LIKE` semantics: without it, a user searching for "50%"
/// or "_" would match way more than they expected because those
/// characters are wildcards inside a `LIKE` pattern.
fn build_ilike_pattern(raw: &str) -> String {
    let escaped = raw
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[utoipa::path(
    get,
    path = "/api/admin/users",
    tag = "Users",
    params(
        ("page" = Option<i64>, Query, description = "Page number (1-based)"),
        ("per_page" = Option<i64>, Query, description = "Items per page"),
        ("search" = Option<String>, Query, description = "Search email / display_name (substring, case-insensitive)"),
    ),
    responses(
        (status = 200, description = "Paginated user list"),
        (status = 403, description = "Forbidden"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn list_users(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Query(query): Query<ListUsersQuery>,
) -> Result<Json<PaginatedResponse<UserResponse>>, AppError> {
    auth_user.require_permission("users:read")?;

    let pagination = query.pagination();
    let per_page = pagination.per_page();
    let offset = pagination.offset();
    // An empty search string means "no filter"; a non-empty term is
    // passed to Postgres as an `ILIKE` pattern with `%` escaped.
    let search_pattern = query
        .search
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(build_ilike_pattern);

    // Determine the team filter. None = global scope (see all),
    // Some = scoped to those team_ids (see only their members).
    let owned_teams = auth_user
        .owned_team_scope_for_perm(&state.db, "users:read")
        .await?;

    let (total, users): (i64, Vec<User>) = match owned_teams {
        None => {
            user_repository::list(
                &state.db,
                search_pattern.as_deref(),
                per_page as i64,
                offset as i64,
            )
            .await?
        }
        Some(team_ids) => {
            let team_ids_vec: Vec<uuid::Uuid> = team_ids.into_iter().collect();
            // Caller sees themselves + every team member of any team
            // they hold `users:read` for. The self inclusion makes
            // sure a team manager doesn't disappear from their own
            // user list.
            user_repository::list_in_teams(
                &state.db,
                auth_user.claims.sub,
                &team_ids_vec,
                search_pattern.as_deref(),
                per_page as i64,
                offset as i64,
            )
            .await?
        }
    };

    let user_ids: Vec<uuid::Uuid> = users.iter().map(|u| u.id).collect();

    // Skip the per-user joins entirely when the page is empty —
    // happens for any search-with-no-match and for over-paginated
    // requests, and previously fired both ANY($1) queries against an
    // empty array round-tripping for nothing.
    if user_ids.is_empty() {
        return Ok(Json(PaginatedResponse {
            data: Vec::new(),
            total,
            page: pagination.page.unwrap_or(1).max(1),
            per_page,
        }));
    }

    // Single query: every assignment for every user, joined against
    // `rbac_roles` so we can report system + custom uniformly.
    let rows = user_repository::role_assignments_of(&state.db, &user_ids)
        .await
        .unwrap_or_default();

    // Pre-size to the page so a 100-row page doesn't bounce through
    // multiple HashMap rehashes while we drain the join rows.
    let mut assignments_map: std::collections::HashMap<uuid::Uuid, Vec<RoleAssignment>> =
        std::collections::HashMap::with_capacity(user_ids.len());
    for (uid, role_id, name, is_system, scope_kind, scope_id) in rows {
        let scope = match (scope_kind.as_str(), scope_id) {
            ("global", _) => "global".to_string(),
            (kind, Some(id)) => format!("{kind}:{id}"),
            (kind, None) => kind.to_string(),
        };
        assignments_map
            .entry(uid)
            .or_default()
            .push(RoleAssignment {
                role_id,
                name,
                is_system,
                scope,
            });
    }

    // Team memberships in one shot. The frontend uses this to
    // render which team each row belongs to (so a team_manager
    // looking at their merged-team list can tell engineering rows
    // from marketing rows). Joined with `teams` so we can return
    // the human name, not just the UUID.
    let team_rows = user_repository::teams_of(&state.db, &user_ids)
        .await
        .unwrap_or_default();

    let mut teams_map: std::collections::HashMap<
        uuid::Uuid,
        Vec<think_watch_common::dto::UserTeamSummary>,
    > = std::collections::HashMap::with_capacity(user_ids.len());
    for (user_id, team_id, team_name) in team_rows {
        teams_map
            .entry(user_id)
            .or_default()
            .push(think_watch_common::dto::UserTeamSummary {
                id: team_id,
                name: team_name,
            });
    }

    let responses: Vec<UserResponse> = users
        .into_iter()
        .map(|u| {
            let role_assignments = assignments_map.remove(&u.id).unwrap_or_default();
            let teams = teams_map.remove(&u.id).unwrap_or_default();
            UserResponse {
                id: u.id,
                email: u.email,
                display_name: u.display_name,
                avatar_url: u.avatar_url,
                is_active: u.is_active,
                oidc_subject: u.oidc_subject,
                role_assignments,
                // Admin user-list intentionally omits permissions —
                // the per-row payload would balloon and the admin
                // page never reads them. /api/auth/me is the
                // canonical place for the live permission set.
                permissions: Vec::new(),
                denied_permissions: Vec::new(),
                teams,
                created_at: u.created_at,
            }
        })
        .collect();

    Ok(Json(PaginatedResponse {
        data: responses,
        total,
        page: pagination.page.unwrap_or(1).max(1),
        per_page,
    }))
}

#[derive(Debug, Deserialize)]
pub struct CreateUserByAdminRequest {
    pub email: String,
    pub display_name: String,
    /// If omitted, a random password is generated and the user must change it on first login.
    pub password: Option<String>,
    /// All roles (system + custom) to assign to the new user. If empty
    /// the user has no permissions; callers typically send at least
    /// one entry (e.g. `developer`).
    #[serde(default)]
    pub role_assignments: Vec<RoleAssignmentRequest>,
}

#[derive(Debug, Serialize)]
pub struct CreateUserByAdminResponse {
    #[serde(flatten)]
    pub user: UserResponse,
    /// Only present when password was auto-generated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_password: Option<String>,
}

// ---------------------------------------------------------------------------
// Super-admin quorum guard
//
// Three operations can lock the platform out of admin recovery:
//   · deleting the last active super_admin
//   · disabling (is_active=false) the last active super_admin
//   · editing the role set so no user holds super_admin anymore
//
// Each path calls `assert_super_admin_quorum` inside the same
// transaction as the mutation, AFTER the write has happened. If the
// invariant would be violated the tx is dropped without commit.
//
// `acquire_super_admin_guard_lock` takes a Postgres advisory lock
// (released at tx end) so two concurrent super-admin-affecting txs
// serialize — otherwise both could check "1 remaining" and both
// commit, leaving zero.
//
// Arbitrary 64-bit key picked once; must stay stable across releases.
// ---------------------------------------------------------------------------

// Super-admin quorum helpers moved to `crate::services::rbac_service`.
// Re-exported at their original names so the rest of this handler and
// any external callers keep compiling.
use crate::services::rbac_service::{acquire_super_admin_guard_lock, assert_super_admin_quorum};

/// Small read-only companion to the quorum guard — surfaces the live
/// list of active super-admin user ids so the admin UI can disable
/// destructive actions on whichever user is currently the sole holder,
/// without waiting for the backend to reject the request. Returning
/// just the ids (not the user rows) keeps the payload tiny and the
/// endpoint cheap enough to refetch on every users-list reload.
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct SuperAdminIds {
    pub ids: Vec<uuid::Uuid>,
}

#[utoipa::path(
    get,
    path = "/api/admin/users/super-admin-ids",
    tag = "Users",
    responses((status = 200, description = "Active super-admin user ids", body = SuperAdminIds)),
    security(("BearerAuth" = []))
)]
pub async fn list_super_admin_ids(
    auth_user: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<SuperAdminIds>, AppError> {
    // `users:read` is the same gate the list endpoint uses — anyone
    // who can see the user table can see the super-admin subset.
    auth_user.require_permission("users:read")?;
    let ids = crate::services::rbac_service::super_admin_ids(&state.db).await?;
    Ok(Json(SuperAdminIds { ids }))
}

/// Apply a set of role assignments to a user atomically inside `tx`.
/// Every existing row for `user_id` is deleted first, then the new
/// rows are inserted. Returns the fully-hydrated assignment list for
/// inclusion in the response. The caller is responsible for any
/// escalation checks (super_admin promotion, etc).
async fn write_user_role_assignments(
    tx: &mut sqlx::PgConnection,
    user_id: uuid::Uuid,
    assignments: &[RoleAssignmentRequest],
    assigned_by: uuid::Uuid,
) -> Result<Vec<RoleAssignment>, AppError> {
    user_repository::delete_role_assignments(tx, user_id).await?;

    let mut out: Vec<RoleAssignment> = Vec::with_capacity(assignments.len());
    for a in assignments {
        let raw_scope = a.scope.clone().unwrap_or_else(|| "global".into());
        let (scope_kind, scope_id) = parse_scope(&raw_scope)?;
        // Insert + return role metadata in one round trip so we can
        // build the UserResponse without a second query.
        let row = user_repository::insert_role_assignment(
            tx,
            user_id,
            a.role_id,
            &scope_kind,
            scope_id,
            assigned_by,
        )
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db)
                if db.constraint() == Some("rbac_role_assignments_role_id_fkey") =>
            {
                AppError::BadRequest(format!("Unknown role id: {}", a.role_id))
            }
            _ => AppError::from(e),
        })?;
        let (name, is_system) =
            row.ok_or_else(|| AppError::BadRequest(format!("Unknown role id: {}", a.role_id)))?;
        out.push(RoleAssignment {
            role_id: a.role_id,
            name,
            is_system,
            scope: raw_scope,
        });
    }
    Ok(out)
}

#[utoipa::path(
    post,
    path = "/api/admin/users",
    tag = "Users",
    request_body(
        content = inline(serde_json::Value),
        description = "email, display_name, optional password, role_assignments[]",
    ),
    responses(
        (status = 200, description = "Created user with optional generated password"),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
        (status = 409, description = "Email already registered"),
    ),
    security(("BearerAuth" = []))
)]
#[tracing::instrument(skip_all, fields(handler = "admin.create_user"))]
pub async fn create_user(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateUserByAdminRequest>,
) -> Result<Json<CreateUserByAdminResponse>, AppError> {
    // Creating a brand-new user is a global operation: the user
    // doesn't yet belong to any team, so a team-scoped admin has
    // nowhere to put them. Team managers add existing users to
    // their team via the team_members API instead. SSO JIT
    // provisioning is the other way new users enter the system.
    auth_user
        .require_global_permission(&state.db, "users:create")
        .await?;

    // Use the canonical validator — `register` and `setup_initialize`
    // both call `validate_email`; the admin create path was laxer
    // (`contains('@') && contains('.')` admitted shapes like `..@x.`).
    // Threat-model-wise the admin path being the laxest is backwards:
    // it's the only one that doesn't go through public POW + lockout.
    //
    // Normalize so the row lands in the same canonical form the
    // login handler will look up. Without this, an admin creating
    // a user as "Alice@x.com" would lock them out — login normalizes
    // to "alice@x.com" and the SQL `WHERE email = $1` would miss.
    let email = normalize_email(&req.email);
    validate_email(&email)?;

    let (raw_password, force_change) = match &req.password {
        Some(p) => {
            validate_password(p)?;
            (p.clone(), false)
        }
        None => (password::generate_random_password(), true),
    };

    // Privilege escalation check: only a user who ALREADY has
    // `roles:create` can assign any role here, but granting
    // super_admin/admin additionally requires the caller to hold that
    // role themselves. Without this gate a user with `users:create`
    // could bootstrap themselves a super_admin account.
    // Load caller's role names from DB for privilege escalation checks.
    let caller_roles =
        think_watch_auth::rbac::load_user_role_names(&state.db, auth_user.claims.sub)
            .await
            .unwrap_or_default();
    let caller_has_super = caller_roles.iter().any(|r| r == "super_admin");
    let caller_has_admin = caller_has_super || caller_roles.iter().any(|r| r == "admin");
    // Look up requested role names in one query to check privilege.
    let role_ids: Vec<uuid::Uuid> = req.role_assignments.iter().map(|a| a.role_id).collect();
    let requested = role_repository::names_of(&state.db, &role_ids).await?;
    for (name,) in &requested {
        if name == "super_admin" && !caller_has_super {
            return Err(AppError::Forbidden(
                "Only super_admin can assign the super_admin role".into(),
            ));
        }
        if name == "admin" && !caller_has_admin {
            return Err(AppError::Forbidden(
                "Only admin/super_admin can assign the admin role".into(),
            ));
        }
    }

    let exists = user_repository::email_taken(&state.db, &email).await?;

    if exists {
        return Err(AppError::Conflict("Email already registered".into()));
    }

    let password_hash = password::hash_password(&raw_password)?;

    let mut tx = state.db.begin().await?;

    let user = user_repository::insert(
        &mut tx,
        &email,
        &req.display_name,
        &password_hash,
        force_change,
    )
    .await?;

    let role_assignments = write_user_role_assignments(
        &mut tx,
        user.id,
        &req.role_assignments,
        auth_user.claims.sub,
    )
    .await?;

    tx.commit().await?;

    // Temp-password expiry: set the Redis marker so the login path
    // can enforce a TTL on admin-issued credentials. Only when we
    // generated the password (force_change=true) — admin-supplied
    // passwords are the user's responsibility.
    if force_change {
        crate::handlers::auth::mark_temporary_password(&state.redis, user.id).await;
    }

    // Sensitive operation — audit the creation with role details so
    // tenant admins can trace who provisioned whom. Password itself is
    // not logged (only whether a reset was forced).
    let temp_expires_at = if force_change {
        Some(
            (chrono::Utc::now()
                + chrono::Duration::seconds(crate::handlers::auth::TEMP_PASSWORD_TTL_SECS))
            .to_rfc3339(),
        )
    } else {
        None
    };
    state.audit.log(
        auth_user
            .audit("admin.create_user")
            .resource("user")
            .resource_id(user.id.to_string())
            .detail(serde_json::json!({
                "email": &user.email,
                "role_ids": &role_ids,
                "force_password_change": force_change,
                "temp_password_expires_at": temp_expires_at,
            })),
    );

    Ok(Json(CreateUserByAdminResponse {
        user: UserResponse {
            id: user.id,
            email: user.email,
            display_name: user.display_name,
            avatar_url: user.avatar_url,
            is_active: user.is_active,
            oidc_subject: user.oidc_subject,
            role_assignments,
            permissions: Vec::new(),
            denied_permissions: Vec::new(),
            teams: Vec::new(),
            created_at: user.created_at,
        },
        generated_password: if force_change {
            Some(raw_password)
        } else {
            None
        },
    }))
}

/// POST /api/admin/users/{id}/force-logout — admin force-logout a user.
#[utoipa::path(
    post,
    path = "/api/admin/users/{id}/force-logout",
    tag = "Users",
    params(
        ("id" = uuid::Uuid, Path, description = "User ID"),
    ),
    responses(
        (status = 200, description = "User logged out"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "User not found"),
    ),
    security(("BearerAuth" = []))
)]
#[tracing::instrument(skip_all, fields(handler = "admin.force_logout_user"))]
pub async fn force_logout_user(
    auth_user: AuthUser,
    State(state): State<AppState>,
    axum::extract::Path(user_id): axum::extract::Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("sessions:revoke")?;
    auth_user
        .assert_scope_for_user(&state.db, "sessions:revoke", user_id)
        .await?;
    // Delete signing public key (invalidates ECDSA-signed requests)
    let _: () =
        fred::interfaces::KeysInterface::del(&state.redis, &format!("signing_pubkey:{user_id}"))
            .await
            .unwrap_or(());

    // Set a password-change epoch so outstanding refresh tokens are
    // rejected — without this the user can mint new access tokens
    // for up to 7 days using a previously-issued refresh token.
    let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;
    crate::handlers::auth::invalidate_refresh_tokens(&state.redis, user_id, refresh_ttl_days).await;

    // Force-close live dashboard WebSockets for this user
    let revoke_key = crate::handlers::dashboard::user_revoked_key(user_id);
    let _: Result<(), _> = fred::interfaces::KeysInterface::set(
        &state.redis,
        &revoke_key,
        "1",
        Some(fred::types::Expiration::EX(300)),
        None,
        false,
    )
    .await;

    state.audit.log(
        auth_user
            .audit("admin.force_logout")
            .resource(format!("user:{user_id}")),
    );

    Ok(Json(
        serde_json::json!({"status": "user_logged_out", "user_id": user_id}),
    ))
}

// --- Update user ---

#[derive(Debug, Deserialize)]
pub struct UpdateUserRequest {
    pub display_name: Option<String>,
    pub is_active: Option<bool>,
    /// When present, replaces **all** role assignments for this user
    /// atomically. Pass an empty array to strip every role. Omit the
    /// field to leave assignments untouched.
    #[serde(default)]
    pub role_assignments: Option<Vec<RoleAssignmentRequest>>,
}

/// PATCH /api/admin/users/{id} — update user display_name, role assignments, or active status.
#[utoipa::path(
    patch,
    path = "/api/admin/users/{id}",
    tag = "Users",
    params(
        ("id" = uuid::Uuid, Path, description = "User ID"),
    ),
    request_body(
        content = inline(serde_json::Value),
        description = "display_name, is_active, role_assignments[]",
    ),
    responses(
        (status = 200, description = "Update applied"),
        (status = 400, description = "Bad request"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "User not found"),
    ),
    security(("BearerAuth" = []))
)]
pub async fn update_user(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<uuid::Uuid>,
    Json(req): Json<UpdateUserRequest>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("users:update")?;
    auth_user
        .assert_scope_for_user(&state.db, "users:update", user_id)
        .await?;

    // Prevent self-deactivation
    if req.is_active == Some(false) && user_id == auth_user.claims.sub {
        return Err(AppError::BadRequest(
            "Cannot deactivate your own account".into(),
        ));
    }

    let exists = user_repository::exists(&state.db, user_id).await?;
    if !exists {
        return Err(AppError::NotFound("User not found".into()));
    }

    // Pre-check role-assignment authorization BEFORE starting the tx —
    // this avoids holding row locks while doing additional DB reads for
    // caller-role lookups.
    let authorized_role_assignments = if let Some(ref assignments) = req.role_assignments {
        auth_user
            .require_global_permission(&state.db, "roles:update")
            .await?;
        let caller_roles =
            think_watch_auth::rbac::load_user_role_names(&state.db, auth_user.claims.sub)
                .await
                .unwrap_or_default();
        let caller_has_super = caller_roles.iter().any(|r| r == "super_admin");
        let caller_has_admin = caller_has_super || caller_roles.iter().any(|r| r == "admin");

        let role_ids: Vec<uuid::Uuid> = assignments.iter().map(|a| a.role_id).collect();
        let requested = role_repository::names_of(&state.db, &role_ids).await?;
        let requested_names: std::collections::HashSet<&String> =
            requested.iter().map(|(n,)| n).collect();

        for name in &requested_names {
            if name.as_str() == "super_admin" && !caller_has_super {
                return Err(AppError::Forbidden(
                    "Only super_admin can assign the super_admin role".into(),
                ));
            }
            if name.as_str() == "admin" && !caller_has_admin {
                return Err(AppError::Forbidden(
                    "Only admin/super_admin can assign the admin role".into(),
                ));
            }
        }

        if user_id == auth_user.claims.sub
            && caller_has_super
            && !requested_names.iter().any(|n| n.as_str() == "super_admin")
        {
            return Err(AppError::BadRequest(
                "Cannot remove your own super_admin role".into(),
            ));
        }
        Some(assignments)
    } else {
        None
    };

    // Apply all DB mutations atomically. Prior code fired three separate
    // UPDATEs without a transaction — a mid-flight error could leave the
    // user with display_name changed but is_active and roles unchanged,
    // which then required manual DB cleanup to fix.
    let mut tx = state.db.begin().await?;
    // Only two kinds of update can erode super-admin quorum: disabling
    // the user, or swapping in a role set that no longer includes
    // super_admin. Take the advisory lock on those paths so concurrent
    // admin edits serialize against each other.
    let touches_quorum = req.is_active == Some(false) || req.role_assignments.is_some();
    if touches_quorum {
        acquire_super_admin_guard_lock(&mut tx).await?;
    }

    if let Some(ref name) = req.display_name {
        if name.trim().is_empty() {
            return Err(AppError::BadRequest("Display name cannot be empty".into()));
        }
        user_repository::set_display_name(&mut tx, user_id, name.trim()).await?;
    }

    if let Some(active) = req.is_active {
        user_repository::set_active(&mut tx, user_id, active).await?;
    }

    if let Some(assignments) = authorized_role_assignments {
        write_user_role_assignments(&mut tx, user_id, assignments, auth_user.claims.sub).await?;
    }

    // Post-mutation quorum check. Phrased as "≥1 active super admin
    // after all pending changes" so disabling + role-removal both
    // funnel into the same guarantee without special-casing either.
    if touches_quorum {
        assert_super_admin_quorum(&mut tx).await?;
    }

    tx.commit().await?;

    // Post-commit side effects: Redis session/permission invalidation.
    // These can race with concurrent requests but are idempotent.
    if req.is_active == Some(false) {
        // Same credential-invalidation chain `delete_user` runs —
        // without these, disabling a user leaves their existing
        // gateway API keys + access JWTs working until natural TTL.
        // Specifically: api_keys had no `users.is_active` join in
        // the gateway auth path, so a deactivated user kept spending
        // org quota on /v1/* indefinitely; JWTs survived ~15 min.
        let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;
        crate::handlers::auth::invalidate_refresh_tokens(&state.redis, user_id, refresh_ttl_days)
            .await;
        invalidate_user_perms(&state.redis, user_id).await;
        let _: () = fred::interfaces::KeysInterface::del(
            &state.redis,
            &format!("signing_pubkey:{user_id}"),
        )
        .await
        .unwrap_or(());
        // Cascade-disable every API key the user owns. Mirrors the
        // `delete_user` cascade but uses `disabled_reason='user_disabled'`
        // so the audit trail distinguishes "admin disabled" from
        // "user deleted." Failure is logged but doesn't abort —
        // the gateway-side users-join (api_key_auth.rs) is the
        // ultimate guarantee.
        if let Err(e) = user_repository::disable_api_keys_of_disabled_user(&state.db, user_id).await
        {
            tracing::warn!(%user_id, "failed to cascade api_keys disable on user deactivation: {e}");
        }
        // Same MCP cache lane wipe as `delete_user` — a deactivated
        // user's cached tool responses would otherwise survive for
        // ~15min, defeating the rest of the invalidation chain above.
        think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
            .invalidate_user_lane_all_servers(&user_id)
            .await;
    }
    if req.role_assignments.is_some() {
        invalidate_user_perms(&state.redis, user_id).await;
    }

    state.audit.log(
        auth_user
            .audit("admin.update_user")
            .resource(format!("user:{user_id}")),
    );

    Ok(Json(
        serde_json::json!({"status": "updated", "user_id": user_id}),
    ))
}

/// DELETE /api/admin/users/{id} — soft-delete a user.
#[utoipa::path(
    delete,
    path = "/api/admin/users/{id}",
    tag = "Users",
    params(
        ("id" = uuid::Uuid, Path, description = "User ID"),
    ),
    responses(
        (status = 200, description = "User deleted"),
        (status = 400, description = "Bad request (e.g. self-deletion)"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "User not found"),
    ),
    security(("BearerAuth" = []))
)]
#[tracing::instrument(skip_all, fields(handler = "admin.delete_user"))]
pub async fn delete_user(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("users:delete")?;
    // Prevent self-deletion. Done BEFORE the scope check because
    // assert_scope_for_user has a self-shortcut that would
    // otherwise let a team manager nuke their own account.
    if user_id == auth_user.claims.sub {
        return Err(AppError::BadRequest(
            "Cannot delete your own account from admin panel".into(),
        ));
    }
    auth_user
        .assert_scope_for_user(&state.db, "users:delete", user_id)
        .await?;

    // Soft-delete + super-admin-quorum check inside one transaction,
    // serialized against concurrent super-admin-touching operations
    // via a Postgres advisory lock. The lock is released when the tx
    // ends, so two racing deletes can't both see "one other super
    // admin remaining" and both fire.
    let mut tx = state.db.begin().await?;
    acquire_super_admin_guard_lock(&mut tx).await?;

    let rows = user_repository::soft_delete(&mut tx, user_id).await?;

    if rows == 0 {
        return Err(AppError::NotFound("User not found".into()));
    }
    // Soft-delete the user's API keys inside the SAME transaction.
    // Previously this UPDATE ran AFTER tx.commit() with its error
    // swallowed by `let _ = …`: if the keys UPDATE failed (PG blip,
    // connection drop), the user row was marked deleted but their
    // API keys stayed active for the full 30-day retention window,
    // letting them keep authenticating against the gateway. Pull it
    // into the TX so a failure rolls back the user delete too — both
    // succeed or neither does.
    user_repository::disable_api_keys_of_deleted_user(&mut tx, user_id).await?;
    // Validate the post-mutation invariant. If this delete took out the
    // last active super admin, we haven't committed yet — the Err short-
    // circuits and the tx rolls back on drop.
    assert_super_admin_quorum(&mut tx).await?;
    tx.commit().await?;

    // Invalidate every active credential the deleted user holds:
    //   - pw_epoch bumps so existing access/refresh JWTs are rejected
    //     by `require_auth` and the refresh handler (both compare
    //     `claims.iat` against the epoch).
    //   - perm cache drop forces the next request to hit the DB.
    //   - signing pubkey delete closes the HMAC signing channel.
    // Without these, a deleted user keeps a working session until the
    // refresh TTL (7 days) expires naturally. These run post-commit so
    // a TX rollback above doesn't strand the cache wipe against a
    // still-active user.
    let refresh_ttl_days = state.dynamic_config.jwt_refresh_ttl_days().await;
    crate::handlers::auth::invalidate_refresh_tokens(&state.redis, user_id, refresh_ttl_days).await;
    crate::middleware::auth_guard::invalidate_user_perms(&state.redis, user_id).await;
    let _: () =
        fred::interfaces::KeysInterface::del(&state.redis, &format!("signing_pubkey:{user_id}"))
            .await
            .unwrap_or(());

    // Wipe MCP per-user response cache lanes across every server. Without
    // this, the deleted user's cached upstream responses linger for the
    // full cache TTL (15min default) — any in-flight request that still
    // holds a valid identity token would see a pre-deletion response,
    // and the rare case of UUID reuse (e.g. a manual restore) would
    // hand the new account the old account's tool outputs. Same
    // post-commit placement as the JWT/perm invalidation above so a
    // rolled-back delete doesn't strand the wipe.
    think_watch_mcp_gateway::cache::McpResponseCache::new(state.redis.clone())
        .invalidate_user_lane_all_servers(&user_id)
        .await;

    state.audit.log(
        auth_user
            .audit("admin.delete_user")
            .resource(format!("user:{user_id}")),
    );

    Ok(Json(
        serde_json::json!({"status": "deleted", "user_id": user_id}),
    ))
}

/// POST /api/admin/users/{id}/reset-password — admin reset user password.
#[utoipa::path(
    post,
    path = "/api/admin/users/{id}/reset-password",
    tag = "Users",
    params(
        ("id" = uuid::Uuid, Path, description = "User ID"),
    ),
    responses(
        (status = 200, description = "Password reset, returns temporary password"),
        (status = 403, description = "Forbidden"),
        (status = 404, description = "User not found"),
    ),
    security(("BearerAuth" = []))
)]
#[tracing::instrument(skip_all, fields(handler = "admin.reset_user_password"))]
pub async fn reset_user_password(
    auth_user: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, AppError> {
    auth_user.require_permission("users:update")?;
    auth_user
        .assert_scope_for_user(&state.db, "users:update", user_id)
        .await?;
    if !user_repository::exists(&state.db, user_id).await? {
        return Err(AppError::NotFound("User not found".into()));
    }

    let new_password = password::generate_random_password();
    let hash = password::hash_password(&new_password)?;

    user_repository::update_password_hash(&state.db, user_id, &hash, true).await?;

    // Invalidate signing public key to force re-login
    let _: () =
        fred::interfaces::KeysInterface::del(&state.redis, &format!("signing_pubkey:{user_id}"))
            .await
            .unwrap_or(());

    // Mark the new temp password with a TTL; login path enforces it.
    crate::handlers::auth::mark_temporary_password(&state.redis, user_id).await;
    let temp_expires_at = (chrono::Utc::now()
        + chrono::Duration::seconds(crate::handlers::auth::TEMP_PASSWORD_TTL_SECS))
    .to_rfc3339();

    state.audit.log(
        auth_user
            .audit("admin.reset_password")
            .resource(format!("user:{user_id}"))
            .detail(serde_json::json!({
                "temp_password_expires_at": temp_expires_at,
            })),
    );

    // NOTE: The temporary password is returned here so the admin can securely
    // communicate it to the user. The audit log does NOT record this value
    // (sanitize_detail redacts any field containing "password").
    // The user is forced to change it on first login (password_change_required=true).
    Ok(Json(serde_json::json!({
        "status": "password_reset",
        "temporary_password": new_password,
        "user_id": user_id,
        "password_change_required": true,
    })))
}

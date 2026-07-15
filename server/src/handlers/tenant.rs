use crate::{
    handlers::helpers::extract_handler_context,
    repositories::{
        tenant::{DEFAULT_TENANT_ID, TenantResponse},
        user::Entity as UserEntity,
    },
    services::tenant::{TenantError, TenantService},
};
use ains_runtime::{HttpError, RequestContext, Response};
use sea_orm::{ConnectionTrait, DatabaseBackend, EntityTrait, Statement};
use serde::Deserialize;

fn error(e: TenantError) -> HttpError {
    match e {
        TenantError::NotFound => HttpError::not_found("Tenant not found"),
        TenantError::NotEmpty => HttpError::conflict("Tenant still contains users or channels"),
        TenantError::InvalidStatus => HttpError::bad_request("status must be active or disabled"),
        TenantError::CannotDisableDefault => {
            HttpError::bad_request("Cannot disable the default tenant")
        }
        TenantError::CannotDeleteDefault => {
            HttpError::bad_request("Cannot delete the default tenant")
        }
        TenantError::InvalidInput(s) => HttpError::bad_request(s),
        TenantError::Internal(e) => {
            tracing::error!(error = ?e, "tenant operation failed");
            HttpError::internal("Tenant operation failed")
        }
    }
}
#[derive(Deserialize)]
pub struct CreateTenantRequest {
    pub name: String,
}
#[derive(Deserialize)]
pub struct UpdateTenantRequest {
    pub name: Option<String>,
    pub status: Option<String>,
}
#[derive(serde::Serialize)]
struct TenantList {
    items: Vec<TenantResponse>,
}

pub async fn list_tenants(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, auth_user) = extract_handler_context(&req)?;
    let items = if auth_user.role == "system" {
        TenantService::new(state.db).list().await.map_err(error)?
    } else if auth_user.role == "admin" {
        // Admin can only see their own tenant
        vec![
            TenantService::new(state.db.clone())
                .get(&auth_user.tenant_id)
                .await
                .map_err(error)?,
        ]
    } else {
        return Err(HttpError::forbidden(
            "Only system and admin can list tenants",
        ));
    };
    Response::json(&TenantList { items })
}
pub async fn create_tenant(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, auth_user) = extract_handler_context(&req)?;
    if auth_user.role != "system" {
        return Err(HttpError::forbidden("Only system can create tenants"));
    }
    let body: CreateTenantRequest = req.parse_json().await.map_err(HttpError::bad_request)?;
    if body.name.trim().is_empty() {
        return Err(HttpError::bad_request("name is required"));
    }
    Response::json(
        &TenantService::new(state.db)
            .create(body.name)
            .await
            .map_err(error)?,
    )
}
pub async fn update_tenant(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, auth_user) = extract_handler_context(&req)?;
    let id: String = req.parse_param("id").map_err(HttpError::bad_request)?;
    if auth_user.role != "system" && auth_user.tenant_id != id {
        return Err(HttpError::not_found("Tenant not found"));
    }
    let body: UpdateTenantRequest = req.parse_json().await.map_err(HttpError::bad_request)?;

    // Design invariant: the default tenant can never be disabled.
    // This handler-layer check is defense-in-depth — the service layer
    // (TenantService::update) also enforces this, and direct DB writes
    // are blocked by application convention.
    if id == DEFAULT_TENANT_ID && body.status.as_deref() == Some("disabled") {
        return Err(HttpError::bad_request("Cannot disable the default tenant"));
    }

    Response::json(
        &TenantService::with_cache(state.db, state.cache.clone())
            .update(&id, body.name, body.status)
            .await
            .map_err(error)?,
    )
}
pub async fn delete_tenant(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, auth_user) = extract_handler_context(&req)?;
    let id: String = req.parse_param("id").map_err(HttpError::bad_request)?;
    if auth_user.role != "system" && auth_user.tenant_id != id {
        return Err(HttpError::not_found("Tenant not found"));
    }
    // Use with_cache so that delete() can invalidate the tenant:active:{id}
    // cache key, preventing stale "active=true" results after deletion.
    TenantService::with_cache(state.db, state.cache.clone())
        .delete(&id)
        .await
        .map_err(error)?;
    Ok(Response::with_status(http::StatusCode::NO_CONTENT))
}
#[derive(Deserialize)]
pub struct MoveUserRequest {
    pub tenant_id: String,
}
pub async fn move_user_tenant(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, auth_user) = extract_handler_context(&req)?;
    let user_id: i64 = req.parse_param("id").map_err(HttpError::bad_request)?;
    let body: MoveUserRequest = req.parse_json().await.map_err(HttpError::bad_request)?;
    // System can move any user; admin can only move regular user-role users
    // within their own tenant.
    let found = UserEntity::find_by_id(user_id)
        .one(state.db.write_conn())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, user_id = %user_id, "Failed to load user");
            HttpError::internal("Failed to load user")
        })?
        .ok_or_else(|| HttpError::not_found("User not found"))?;
    if auth_user.role != "system" {
        if found.role != "user" || found.tenant_id != auth_user.tenant_id {
            return Err(HttpError::not_found("User not found"));
        }
        // Admin can only move users to their own tenant
        if body.tenant_id != auth_user.tenant_id {
            return Err(HttpError::forbidden(
                "Admin can only move users to their own tenant",
            ));
        }
    }
    if !TenantService::with_cache(state.db.clone(), state.cache.clone())
        .is_active(&body.tenant_id)
        .await
        .map_err(error)?
    {
        return Err(HttpError::bad_request(
            "Tenant does not exist or is disabled",
        ));
    }

    // No-op guard: if the user is already in the target tenant, skip the
    // UPDATE entirely so we don't unnecessarily increment token_version
    // (which would invalidate all existing JWTs for no reason).
    if found.tenant_id == body.tenant_id {
        return Response::json(&crate::repositories::user::UserResponse::from(found));
    }

    let old_tenant_id = found.tenant_id.clone();
    let new_tenant_id = body.tenant_id.clone();

    // Single atomic SQL UPDATE: set tenant_id, increment token_version,
    // and update updated_at in one operation. This avoids the lost-update
    // risk of ActiveModel's full-row update (which could overwrite concurrent
    // modifications to other fields like name/email/role).
    state.db.write_conn().execute(Statement::from_sql_and_values(
        DatabaseBackend::Postgres,
        "UPDATE users SET tenant_id = $1, token_version = token_version + 1, updated_at = NOW() WHERE id = $2",
        [new_tenant_id.clone().into(), user_id.into()],
    ))
    .await
    .map_err(|e| {
        tracing::error!(error = %e, user_id = %user_id, "Failed to move user tenant");
        HttpError::internal("Failed to move user")
    })?;

    // Re-query to get the atomically-incremented token_version
    let updated = UserEntity::find_by_id(user_id)
        .one(state.db.write_conn())
        .await
        .map_err(|e| {
            tracing::error!(error = %e, user_id = %user_id, "Failed to load updated user");
            HttpError::internal("Failed to load updated user")
        })?
        .ok_or_else(|| HttpError::internal("User not found after update"))?;

    // Invalidate caches so subsequent reads return fresh data
    let cache_key = format!("user:{}", user_id);
    let _ = state.cache.invalidate(&cache_key).await;
    let token_cache_key = format!("user:token_version:{}", user_id);
    let _ = state.cache.invalidate(&token_cache_key).await;

    // Invalidate per-tenant count caches for both source and destination tenants,
    // because the admin-scoped count changes when a user moves between tenants.
    // Also invalidate the user-scoped count for consistency with create_user
    // and delete_user (services/user.rs).
    // The system-level count (user:count:system) is NOT invalidated because the
    // total user count across all tenants is unchanged by a tenant move.
    for tenant in [&old_tenant_id, &new_tenant_id] {
        let _ = state
            .cache
            .invalidate(&format!("user:count:admin:{}", tenant))
            .await;
        let _ = state
            .cache
            .invalidate(&format!("user:count:user:{}", tenant))
            .await;
    }

    Response::json(&crate::repositories::user::UserResponse::from(updated))
}

use crate::{
    handlers::helpers,
    repositories::channel::ChannelResponse,
    services::gateway::{CreateChannelInput, GatewayError, UpdateChannelInput},
};
use ains_runtime::{HttpError, RequestContext, Response};
use sea_orm::EntityTrait;
use serde::Deserialize;
use uuid::Uuid;

fn error(e: GatewayError) -> HttpError {
    helpers::handle_gateway_error(e)
}
fn service(state: &crate::AppState) -> &crate::services::gateway::GatewayService {
    helpers::gateway_service(state)
}

async fn require_active_tenant(state: &crate::AppState, tenant_id: &str) -> Result<(), HttpError> {
    helpers::require_active_tenant(state, tenant_id).await
}

async fn assert_channel_scope(
    state: &crate::AppState,
    actor: &ains_runtime::AuthUser,
    id: Uuid,
) -> Result<(), HttpError> {
    if actor.role == "system" {
        return Ok(());
    }
    require_active_tenant(state, &actor.tenant_id).await?;
    let channel = crate::repositories::channel::Entity::find_by_id(id)
        .one(&*state.db)
        .await
        .map_err(|_| HttpError::internal("Failed to load channel"))?
        .ok_or_else(|| HttpError::not_found("Channel not found"))?;
    if channel.tenant_id != actor.tenant_id {
        return Err(HttpError::not_found("Channel not found"));
    }
    Ok(())
}
fn default_page() -> u64 {
    1
}
fn default_per_page() -> u64 {
    10
}

#[derive(Deserialize)]
struct ListChannelsQuery {
    #[serde(default = "default_page")]
    page: u64,
    #[serde(default = "default_per_page")]
    per_page: u64,
}

#[derive(serde::Serialize)]
struct PaginatedChannelResponse {
    items: Vec<ChannelResponse>,
    total: u64,
    page: u64,
    per_page: u64,
    total_pages: u64,
}

pub async fn list_channels(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = helpers::extract_handler_context(&req)?;
    let query: ListChannelsQuery = req.parse_query().map_err(HttpError::bad_request)?;

    let tenant = if actor.role == "system" {
        None
    } else {
        require_active_tenant(&state, &actor.tenant_id).await?;
        Some(actor.tenant_id.clone())
    };

    // Clamp 与 Service 层保持一致，避免响应回传未经校正的原始参数：
    // 否则 per_page=200 时实际只返回 100 条却声称 200（total_pages 偏小，
    // 前端翻不到全部数据），per_page=0 时字段自相矛盾。
    let page = query.page.clamp(1, 1_000_000);
    let per_page = query.per_page.clamp(1, 100);
    let (items, total) = service(&state)
        .list_paginated(tenant.as_deref(), page, per_page)
        .await
        .map_err(error)?;

    let mut items = items;
    // Best-effort enrich each row with its tenant display name (see list_users).
    let tenant_ids: Vec<String> = items.iter().map(|c| c.tenant_id.clone()).collect();
    if let Ok(names) = crate::services::tenant::TenantService::new(state.db.clone())
        .names_for(&tenant_ids)
        .await
    {
        for item in items.iter_mut() {
            item.tenant_name = names.get(&item.tenant_id).cloned();
        }
    }

    let total_pages = total.div_ceil(per_page);

    Response::json(&PaginatedChannelResponse {
        items,
        total,
        page,
        per_page,
        total_pages,
    })
}
pub async fn create_channel(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = helpers::extract_handler_context(&req)?;
    let body: serde_json::Value = req.parse_json().await.map_err(HttpError::bad_request)?;
    let tenant_id = match actor.role.as_str() {
        "system" => body
            .get("tenant_id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .ok_or_else(|| HttpError::bad_request("system must supply tenant_id"))?,
        // Non-system (admin/user) always scoped to their own tenant.
        // The body-level tenant_id, if present, is ignored to prevent
        // privilege escalation where an actor could create channels
        // belonging to other tenants.
        _ => actor.tenant_id.clone(),
    };
    require_active_tenant(&state, &tenant_id).await?;
    let input: CreateChannelInput =
        serde_json::from_value(body).map_err(|e| HttpError::bad_request(e.to_string()))?;
    Response::json(
        &service(&state)
            .create_channel(input, &tenant_id)
            .await
            .map_err(error)?,
    )
}
pub async fn update_channel(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = helpers::extract_handler_context(&req)?;
    let id: Uuid = req.parse_param("id").map_err(HttpError::bad_request)?;
    assert_channel_scope(&state, &actor, id).await?;
    let input: UpdateChannelInput = req.parse_json().await.map_err(HttpError::bad_request)?;

    // Only system can change a channel's tenant; admin is scoped to their own.
    if input.tenant_id.is_some() && actor.role != "system" {
        return Err(HttpError::forbidden(
            "Only system can change channel tenant",
        ));
    }
    // If tenant_id is being changed, verify the target tenant is active.
    if let Some(ref new_tenant_id) = input.tenant_id {
        helpers::require_active_tenant(&state, new_tenant_id).await?;
    }

    Response::json(
        &service(&state)
            .update_channel(id, input)
            .await
            .map_err(error)?,
    )
}
pub async fn delete_channel(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = helpers::extract_handler_context(&req)?;
    let id: Uuid = req.parse_param("id").map_err(HttpError::bad_request)?;
    assert_channel_scope(&state, &actor, id).await?;
    service(&state).delete_channel(id).await.map_err(error)?;
    Response::json(&serde_json::json!({"message": "Channel deleted successfully"}))
}

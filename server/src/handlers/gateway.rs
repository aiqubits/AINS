use crate::{
    handlers::helpers,
    repositories::channel::ChannelResponse,
    services::gateway::{CreateChannelInput, GatewayError, UpdateChannelInput},
};
use ains_runtime::{HttpError, RequestContext, Response};
use sea_orm::EntityTrait;
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
#[derive(serde::Serialize)]
struct ChannelList {
    items: Vec<ChannelResponse>,
}
pub async fn list_channels(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = helpers::extract_handler_context(&req)?;
    let tenant = if actor.role == "system" {
        None
    } else {
        require_active_tenant(&state, &actor.tenant_id).await?;
        Some(&actor.tenant_id)
    };
    Response::json(&ChannelList {
        items: service(&state)
            .list_channels(tenant.map(|x| x.as_str()))
            .await
            .map_err(error)?,
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
    Response::json(
        &service(&state)
            .update_channel(id, input)
            .await
            .map_err(error)?,
    )
}
pub async fn disable_channel(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = helpers::extract_handler_context(&req)?;
    let id: Uuid = req.parse_param("id").map_err(HttpError::bad_request)?;
    assert_channel_scope(&state, &actor, id).await?;
    service(&state).disable_channel(id).await.map_err(error)?;
    Ok(Response::with_status(http::StatusCode::NO_CONTENT))
}

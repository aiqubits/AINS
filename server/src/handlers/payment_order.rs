//! Payment order handlers — tenant-scoped order records.
//!
//! Admin endpoints (behind the admin guard): order CRUD across the actor's
//! scope (system: all tenants, admin: own tenant only). Deletion is further
//! restricted to the system role — orders are the audit trail of real money
//! movement and tenant admins must not be able to erase them. Self-service:
//! listing the caller's own orders.

use crate::{
    handlers::helpers::{self, extract_handler_context},
    services::payment_order::{
        CreateOrderInput, ListOrdersParams, PaymentOrderError, PaymentOrderService,
        UpdateOrderInput,
    },
    snowflake::SnowflakeId,
};
use ains_runtime::{HttpError, RequestContext, Response};
use serde::Deserialize;

fn error(e: PaymentOrderError) -> HttpError {
    match e {
        PaymentOrderError::NotFound => HttpError::not_found("Order not found"),
        PaymentOrderError::UserNotFound => HttpError::not_found("User not found"),
        PaymentOrderError::PlanNotFound => HttpError::not_found("Plan not found"),
        PaymentOrderError::InvalidInput(s) => HttpError::bad_request(s),
        PaymentOrderError::Internal(e) => {
            tracing::error!(error = ?e, "payment order operation failed");
            HttpError::internal("Payment order operation failed")
        }
    }
}

fn default_page() -> u64 {
    1
}
fn default_per_page() -> u64 {
    10
}

#[derive(Deserialize)]
struct ListOrdersQuery {
    #[serde(default = "default_page")]
    page: u64,
    #[serde(default = "default_per_page")]
    per_page: u64,
    /// system-only tenant filter; ignored for admin.
    tenant_id: Option<String>,
    /// Optional user filter (snowflake ID as string).
    user_id: Option<String>,
    /// Optional status filter.
    status: Option<String>,
}

#[derive(Deserialize)]
struct MyOrdersQuery {
    #[serde(default = "default_page")]
    page: u64,
    #[serde(default = "default_per_page")]
    per_page: u64,
}

#[derive(Deserialize)]
pub struct CreateOrderRequest {
    pub user_id: SnowflakeId,
    pub plan_id: Option<SnowflakeId>,
    pub amount: i64,
    pub status: Option<String>,
    pub payment_method: Option<String>,
    pub external_txn_id: Option<String>,
}

#[derive(Deserialize)]
pub struct UpdateOrderRequest {
    pub status: Option<String>,
    pub payment_method: Option<String>,
    pub external_txn_id: Option<String>,
}

/// `GET /api/orders` — admin/system order listing.
pub async fn list_orders(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    helpers::require_actor_tenant_active(&state, &actor).await?;
    let query: ListOrdersQuery = req.parse_query().map_err(HttpError::bad_request)?;

    let user_id = match query.user_id.as_deref().filter(|s| !s.is_empty()) {
        Some(raw) => Some(
            raw.parse::<i64>()
                .map_err(|_| HttpError::bad_request("Invalid user_id filter"))?,
        ),
        None => None,
    };

    let mut result = PaymentOrderService::new(state.db.clone())
        .list_paginated(
            ListOrdersParams {
                page: query.page,
                per_page: query.per_page,
                tenant_id: query.tenant_id,
                user_id,
                status: query.status,
            },
            &actor.role,
            &actor.tenant_id,
        )
        .await
        .map_err(error)?;

    helpers::enrich_tenant_names(&state, &mut result.items).await;

    Response::json(&result)
}

/// `GET /api/orders/{id}` — fetch a single order (tenant-scoped).
pub async fn get_order(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    helpers::require_actor_tenant_active(&state, &actor).await?;
    let id: i64 = req.parse_param("id").map_err(HttpError::bad_request)?;
    Response::json(
        &PaymentOrderService::new(state.db)
            .get(id, &actor.role, &actor.tenant_id)
            .await
            .map_err(error)?,
    )
}

/// `POST /api/orders` — manually create an order record.
pub async fn create_order(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    helpers::require_actor_tenant_active(&state, &actor).await?;
    let body: CreateOrderRequest = req.parse_json().await.map_err(HttpError::bad_request)?;

    Response::json(
        &PaymentOrderService::new(state.db)
            .create(
                CreateOrderInput {
                    user_id: body.user_id.as_i64(),
                    plan_id: body.plan_id.map(|id| id.as_i64()),
                    amount: body.amount,
                    status: body.status,
                    payment_method: body.payment_method,
                    external_txn_id: body.external_txn_id,
                },
                &actor.role,
                &actor.tenant_id,
            )
            .await
            .map_err(error)?,
    )
}

/// `PUT /api/orders/{id}` — update an order record (record-keeping only).
pub async fn update_order(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    helpers::require_actor_tenant_active(&state, &actor).await?;
    let id: i64 = req.parse_param("id").map_err(HttpError::bad_request)?;
    let body: UpdateOrderRequest = req.parse_json().await.map_err(HttpError::bad_request)?;

    Response::json(
        &PaymentOrderService::new(state.db)
            .update(
                id,
                UpdateOrderInput {
                    status: body.status,
                    payment_method: body.payment_method,
                    external_txn_id: body.external_txn_id,
                },
                &actor.role,
                &actor.tenant_id,
            )
            .await
            .map_err(error)?,
    )
}

/// `DELETE /api/orders/{id}` — delete an order record (system only).
pub async fn delete_order(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    helpers::require_actor_tenant_active(&state, &actor).await?;
    // Orders record real money movement (audit trail). Tenant admins may
    // flip statuses but must not be able to erase the record itself.
    if actor.role != "system" {
        return Err(HttpError::forbidden(
            "Only the system role can delete order records",
        ));
    }
    let id: i64 = req.parse_param("id").map_err(HttpError::bad_request)?;
    PaymentOrderService::new(state.db)
        .delete(id, &actor.role, &actor.tenant_id)
        .await
        .map_err(error)?;
    Response::json(&serde_json::json!({"message": "Order deleted successfully"}))
}

/// `GET /api/users/me/orders` — the calling user's own orders.
pub async fn list_my_orders(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    let query: MyOrdersQuery = req.parse_query().map_err(HttpError::bad_request)?;
    let user_id: i64 = actor
        .user_id
        .parse()
        .map_err(|_| HttpError::unauthorized("Invalid user ID in token"))?;

    Response::json(
        &PaymentOrderService::new(state.db)
            .list_of_user(user_id, query.page, query.per_page)
            .await
            .map_err(error)?,
    )
}

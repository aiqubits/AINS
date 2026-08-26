//! Plan management handlers — tenant-scoped subscription plans.
//!
//! Admin endpoints (behind the admin guard): plan CRUD, per-user plan
//! listing/assignment. Self-service endpoints: available plan listing,
//! balance purchase, own plan listing.

use crate::{
    handlers::helpers::{self, extract_handler_context},
    repositories::{plan::PlanResponse, user_plan::UserPlanResponse},
    services::plan::{AvailablePlan, CreatePlanInput, PlanError, PlanService, UpdatePlanInput},
    services::user::BALANCE_SCALE,
    snowflake::SnowflakeId,
};
use ains_runtime::{HttpError, RequestContext, Response};
use serde::{Deserialize, Serialize};

fn error(e: PlanError) -> HttpError {
    match e {
        PlanError::NotFound => HttpError::not_found("Plan not found"),
        PlanError::UserNotFound => HttpError::not_found("User not found"),
        PlanError::InvalidInput(s) => HttpError::bad_request(s),
        // Distinct error codes so the web client can translate them.
        PlanError::InsufficientBalance => HttpError::with_status(
            400,
            "insufficient_balance",
            "Insufficient balance to purchase this plan",
        ),
        PlanError::PurchaseLimitReached => HttpError::with_status(
            409,
            "purchase_limit_reached",
            "Purchase limit reached for this plan",
        ),
        // NoActivePlan is only ever produced by `consume_call`, which is
        // consumed by the AI response gate (responses.rs) — the arm here
        // exists for match exhaustiveness and consistent error mapping.
        PlanError::NoActivePlan => {
            HttpError::with_status(403, "no_active_plan", "No active plan with remaining calls")
        }
        PlanError::Internal(e) => {
            tracing::error!(error = ?e, "plan operation failed");
            HttpError::internal("Plan operation failed")
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
struct ListPlansQuery {
    #[serde(default = "default_page")]
    page: u64,
    #[serde(default = "default_per_page")]
    per_page: u64,
    /// system-only tenant filter; ignored for admin.
    tenant_id: Option<String>,
}

#[derive(Deserialize)]
pub struct CreatePlanRequest {
    pub tenant_id: Option<String>,
    pub name: String,
    pub description: Option<String>,
    pub price: i64,
    pub total_calls: i64,
    pub validity_days: i32,
    pub purchase_limit: Option<i32>,
    pub status: Option<String>,
}

fn deserialize_present_nullable<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

#[derive(Deserialize)]
pub struct UpdatePlanRequest {
    pub name: Option<String>,
    pub description: Option<String>,
    pub price: Option<i64>,
    pub total_calls: Option<i64>,
    pub validity_days: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_present_nullable")]
    pub purchase_limit: Option<Option<i32>>,
    pub status: Option<String>,
}

#[derive(Deserialize)]
pub struct AssignPlanRequest {
    pub plan_id: SnowflakeId,
}

/// `GET /api/plans` — admin/system plan listing.
pub async fn list_plans(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    helpers::require_actor_tenant_active(&state, &actor).await?;
    let query: ListPlansQuery = req.parse_query().map_err(HttpError::bad_request)?;

    let mut result = PlanService::new(state.db.clone())
        .list_paginated(
            &actor.role,
            &actor.tenant_id,
            query.tenant_id.as_deref(),
            query.page,
            query.per_page,
        )
        .await
        .map_err(error)?;

    helpers::enrich_tenant_names(&state, &mut result.items).await;

    Response::json(&result)
}

/// `POST /api/plans` — create a plan (system: any tenant, admin: own tenant).
pub async fn create_plan(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    let body: CreatePlanRequest = req.parse_json().await.map_err(HttpError::bad_request)?;

    let tenant_id = match actor.role.as_str() {
        "system" => body
            .tenant_id
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| HttpError::bad_request("system must supply tenant_id"))?,
        // Non-system always scoped to their own tenant — a body-level
        // tenant_id is ignored to prevent cross-tenant privilege escalation.
        _ => actor.tenant_id.clone(),
    };
    helpers::require_active_tenant(&state, &tenant_id).await?;

    Response::json(
        &PlanService::new(state.db)
            .create(CreatePlanInput {
                tenant_id,
                name: body.name,
                description: body.description,
                price: body.price,
                total_calls: body.total_calls,
                validity_days: body.validity_days,
                purchase_limit: body.purchase_limit,
                status: body.status,
            })
            .await
            .map_err(error)?,
    )
}

/// `PUT /api/plans/{id}` — update a plan (tenant-scoped).
pub async fn update_plan(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    helpers::require_actor_tenant_active(&state, &actor).await?;
    let id: i64 = req.parse_param("id").map_err(HttpError::bad_request)?;
    let body: UpdatePlanRequest = req.parse_json().await.map_err(HttpError::bad_request)?;

    Response::json(
        &PlanService::new(state.db)
            .update(
                id,
                UpdatePlanInput {
                    name: body.name,
                    description: body.description,
                    price: body.price,
                    total_calls: body.total_calls,
                    validity_days: body.validity_days,
                    purchase_limit: body.purchase_limit,
                    status: body.status,
                },
                &actor.role,
                &actor.tenant_id,
            )
            .await
            .map_err(error)?,
    )
}

/// `DELETE /api/plans/{id}` — delete a plan template (tenant-scoped).
pub async fn delete_plan(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    helpers::require_actor_tenant_active(&state, &actor).await?;
    let id: i64 = req.parse_param("id").map_err(HttpError::bad_request)?;
    PlanService::new(state.db)
        .delete(id, &actor.role, &actor.tenant_id)
        .await
        .map_err(error)?;
    Response::json(&serde_json::json!({"message": "Plan deleted successfully"}))
}

#[derive(Serialize)]
struct UserPlanListResponse {
    items: Vec<UserPlanResponse>,
}

/// `GET /api/users/{id}/plans` — list a user's plan instances (admin view).
pub async fn list_user_plans(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    helpers::require_actor_tenant_active(&state, &actor).await?;
    let user_id: i64 = req.parse_param("id").map_err(HttpError::bad_request)?;
    let items = PlanService::new(state.db)
        .list_user_plans(user_id, &actor.role, &actor.tenant_id)
        .await
        .map_err(error)?;
    Response::json(&UserPlanListResponse { items })
}

/// `POST /api/users/{id}/plans` — grant a plan to a user (admin action).
pub async fn assign_user_plan(mut req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    helpers::require_actor_tenant_active(&state, &actor).await?;
    let user_id: i64 = req.parse_param("id").map_err(HttpError::bad_request)?;
    let body: AssignPlanRequest = req.parse_json().await.map_err(HttpError::bad_request)?;

    Response::json(
        &PlanService::new(state.db)
            .assign_to_user(
                user_id,
                body.plan_id.as_i64(),
                &actor.role,
                &actor.tenant_id,
            )
            .await
            .map_err(error)?,
    )
}

/// `GET /api/users/me/plans` — list the calling user's plan instances.
pub async fn list_my_plans(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    let user_id: i64 = actor
        .user_id
        .parse()
        .map_err(|_| HttpError::unauthorized("Invalid user ID in token"))?;
    let items = PlanService::new(state.db)
        .list_plans_of(user_id)
        .await
        .map_err(error)?;
    Response::json(&UserPlanListResponse { items })
}

#[derive(Serialize)]
struct AvailablePlansResponse {
    items: Vec<AvailablePlanResponse>,
}

#[derive(Serialize)]
struct AvailablePlanResponse {
    #[serde(flatten)]
    plan: PlanResponse,
    purchases_used: u64,
}

impl From<AvailablePlan> for AvailablePlanResponse {
    fn from(value: AvailablePlan) -> Self {
        Self {
            plan: value.plan,
            purchases_used: value.purchases_used,
        }
    }
}

/// `GET /api/plans/available` — active plans of the caller's tenant.
pub async fn list_available_plans(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    let user_id: i64 = actor
        .user_id
        .parse()
        .map_err(|_| HttpError::unauthorized("Invalid user ID in token"))?;
    // Consistent with the purchase gate: users of a disabled tenant cannot
    // browse purchasable plans either.
    helpers::require_actor_tenant_active(&state, &actor).await?;
    let items = PlanService::new(state.db)
        .list_available(&actor.tenant_id, user_id)
        .await
        .map_err(error)?
        .into_iter()
        .map(Into::into)
        .collect();
    Response::json(&AvailablePlansResponse { items })
}

#[derive(Serialize)]
struct PurchaseResponse {
    order: crate::repositories::payment_order::PaymentOrderResponse,
    user_plan: UserPlanResponse,
    balance: i64,
    display_balance: f64,
    message: String,
}

/// `POST /api/plans/{id}/purchase` — buy a plan with account balance.
pub async fn purchase_plan(req: crate::ServerRequest) -> Result<Response, HttpError> {
    let (state, actor) = extract_handler_context(&req)?;
    let plan_id: i64 = req.parse_param("id").map_err(HttpError::bad_request)?;
    let user_id: i64 = actor
        .user_id
        .parse()
        .map_err(|_| HttpError::unauthorized("Invalid user ID in token"))?;

    // Disabled tenants cannot purchase (same gate as the AI proxy;
    // system is exempt, consistent with the other tenant-scoped modules).
    helpers::require_actor_tenant_active(&state, &actor).await?;

    // Per-user purchase lock: suppresses accidental double submits (double
    // click / client retry) that would otherwise deduct twice and stack two
    // instances. Fail-open by convention — balance correctness is already
    // guaranteed by the row lock inside `purchase`; this lock only guards
    // against unintended duplicate purchases.
    let purchase_guard = match crate::services::LockGuard::acquire_with_client(
        state.cache.redis_client(),
        &format!("plan:purchase:{user_id}"),
        10, /* generous upper bound; released explicitly right after purchase */
        1,  /* single attempt, no retry — a held lock means a duplicate submit */
        std::time::Duration::ZERO,
    )
    .await
    {
        Ok(Some(crate::services::AcquireResult::Acquired(guard))) => Some(guard),
        Ok(Some(crate::services::AcquireResult::Contended)) => {
            return Err(HttpError::with_status(
                409,
                "purchase_in_progress",
                "Another purchase is already in progress",
            ));
        }
        // Redis unavailable / lock error: proceed unprotected (fail-open).
        Ok(None) => None,
        Err(e) => {
            tracing::warn!(error = ?e, "purchase lock unavailable, proceeding without it");
            None
        }
    };

    let result = PlanService::with_cache(state.db.clone(), state.cache.clone())
        .purchase(user_id, plan_id)
        .await;

    // Release deterministically before responding — Drop releases via a
    // spawned task, which could spuriously reject an immediate follow-up
    // purchase as purchase_in_progress. Failures fall back to Drop/TTL.
    if let Some(guard) = purchase_guard
        && let Err(e) = guard.release().await
    {
        tracing::warn!(error = ?e, "failed to release purchase lock");
    }

    let outcome = result.map_err(error)?;

    Response::json(&PurchaseResponse {
        order: outcome.order,
        user_plan: outcome.user_plan,
        balance: outcome.balance,
        display_balance: outcome.balance as f64 / BALANCE_SCALE as f64,
        message: "Plan purchased successfully".to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::UpdatePlanRequest;

    #[test]
    fn update_purchase_limit_distinguishes_missing_null_and_value() {
        let missing: UpdatePlanRequest = serde_json::from_str("{}").unwrap();
        assert_eq!(missing.purchase_limit, None);

        let unlimited: UpdatePlanRequest =
            serde_json::from_str(r#"{"purchase_limit":null}"#).unwrap();
        assert_eq!(unlimited.purchase_limit, Some(None));

        let limited: UpdatePlanRequest = serde_json::from_str(r#"{"purchase_limit":3}"#).unwrap();
        assert_eq!(limited.purchase_limit, Some(Some(3)));
    }
}

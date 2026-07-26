//! Payment order service — tenant-scoped order management.
//!
//! Orders are created automatically by `PlanService::purchase` and manually
//! by admins (e.g. to back-fill an external payment). Status updates here
//! are record-keeping only: setting `refunded`/`cancelled` does NOT refund
//! balance or revoke granted plan instances — refund automation is deferred
//! until external payment providers are integrated.
//!
//! # Tenant isolation
//!
//! `system` sees all orders (optionally filtered by tenant); `admin` is
//! confined to orders whose `tenant_id` snapshot matches its own tenant.
//! Out-of-scope orders surface as `NotFound`.

use anyhow::Context;
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, Set,
};
use std::sync::Arc;

use crate::{
    AutoRouter,
    repositories::{
        payment_order::{
            ActiveModel, Column, Entity, Model, ORDER_STATUSES, PAYMENT_METHODS,
            PaymentOrderResponse,
        },
        plan::Entity as PlanEntity,
        user::Entity as UserEntity,
    },
    services::user::PaginatedResponse,
};

#[derive(Debug, thiserror::Error)]
pub enum PaymentOrderError {
    #[error("Order not found")]
    NotFound,
    #[error("User not found")]
    UserNotFound,
    #[error("Plan not found")]
    PlanNotFound,
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Filters for the admin order listing.
#[derive(Debug, Default)]
pub struct ListOrdersParams {
    pub page: u64,
    pub per_page: u64,
    /// system-only tenant filter; ignored for admin (forced to own tenant).
    pub tenant_id: Option<String>,
    pub user_id: Option<i64>,
    pub status: Option<String>,
}

/// Input for manual order creation by an admin.
pub struct CreateOrderInput {
    pub user_id: i64,
    pub plan_id: Option<i64>,
    pub amount: i64,
    pub status: Option<String>,
    pub payment_method: Option<String>,
    pub external_txn_id: Option<String>,
}

/// Partial update — `None` fields are left unchanged.
#[derive(Default)]
pub struct UpdateOrderInput {
    pub status: Option<String>,
    pub payment_method: Option<String>,
    pub external_txn_id: Option<String>,
}

pub struct PaymentOrderService {
    db: Arc<AutoRouter>,
}

impl PaymentOrderService {
    pub fn new(db: Arc<AutoRouter>) -> Self {
        Self { db }
    }

    fn in_scope(actor_role: &str, actor_tenant_id: &str, order_tenant_id: &str) -> bool {
        actor_role == "system" || actor_tenant_id == order_tenant_id
    }

    /// Load an order enforcing tenant scope (based on the tenant_id snapshot).
    async fn load_scoped(
        &self,
        id: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<Model, PaymentOrderError> {
        let order = Entity::find_by_id(id)
            .one(&*self.db)
            .await
            .context("load order")?
            .ok_or(PaymentOrderError::NotFound)?;
        if !Self::in_scope(actor_role, actor_tenant_id, &order.tenant_id) {
            return Err(PaymentOrderError::NotFound);
        }
        Ok(order)
    }

    /// Paginated order listing with tenant isolation.
    pub async fn list_paginated(
        &self,
        params: ListOrdersParams,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<PaginatedResponse<PaymentOrderResponse>, PaymentOrderError> {
        let page = params.page.clamp(1, 1_000_000);
        let per_page = params.per_page.clamp(1, 100);

        let tenant = if actor_role == "system" {
            params.tenant_id.filter(|t| !t.is_empty())
        } else {
            Some(actor_tenant_id.to_string())
        };

        let mut select = Entity::find().order_by_desc(Column::CreatedAt);
        if let Some(ref t) = tenant {
            select = select.filter(Column::TenantId.eq(t));
        }
        if let Some(user_id) = params.user_id {
            select = select.filter(Column::UserId.eq(user_id));
        }
        if let Some(ref status) = params.status
            && !status.is_empty()
        {
            select = select.filter(Column::Status.eq(status));
        }

        let paginator = select.paginate(&*self.db, per_page);
        let total = paginator.num_items().await.context("count orders")?;
        let total_pages = total.div_ceil(per_page);
        let items: Vec<PaymentOrderResponse> = paginator
            .fetch_page(page - 1)
            .await
            .context("fetch orders page")?
            .into_iter()
            .map(Into::into)
            .collect();

        Ok(PaginatedResponse {
            items,
            total,
            page,
            per_page,
            total_pages,
        })
    }

    /// List the calling user's own orders (self view, no tenant scoping).
    pub async fn list_of_user(
        &self,
        user_id: i64,
        page: u64,
        per_page: u64,
    ) -> Result<PaginatedResponse<PaymentOrderResponse>, PaymentOrderError> {
        let page = page.clamp(1, 1_000_000);
        let per_page = per_page.clamp(1, 100);

        let paginator = Entity::find()
            .filter(Column::UserId.eq(user_id))
            .order_by_desc(Column::CreatedAt)
            .paginate(&*self.db, per_page);
        let total = paginator.num_items().await.context("count own orders")?;
        let total_pages = total.div_ceil(per_page);
        let items: Vec<PaymentOrderResponse> = paginator
            .fetch_page(page - 1)
            .await
            .context("fetch own orders page")?
            .into_iter()
            .map(Into::into)
            .collect();

        Ok(PaginatedResponse {
            items,
            total,
            page,
            per_page,
            total_pages,
        })
    }

    pub async fn get(
        &self,
        id: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<PaymentOrderResponse, PaymentOrderError> {
        Ok(self
            .load_scoped(id, actor_role, actor_tenant_id)
            .await?
            .into())
    }

    /// Manually create an order record.
    ///
    /// The tenant_id / user_email snapshots are taken from the target user's
    /// current state. Admin can only create orders for own-tenant users.
    pub async fn create(
        &self,
        input: CreateOrderInput,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<PaymentOrderResponse, PaymentOrderError> {
        if input.amount < 0 {
            return Err(PaymentOrderError::InvalidInput(
                "amount must not be negative".into(),
            ));
        }
        let status = input.status.unwrap_or_else(|| "paid".to_string());
        validate_order_status(&status)?;
        let payment_method = input
            .payment_method
            .unwrap_or_else(|| "balance".to_string());
        validate_payment_method(&payment_method)?;

        let user = UserEntity::find_by_id(input.user_id)
            .one(&*self.db)
            .await
            .context("load target user")?
            .ok_or(PaymentOrderError::UserNotFound)?;
        if !Self::in_scope(actor_role, actor_tenant_id, &user.tenant_id) {
            return Err(PaymentOrderError::UserNotFound);
        }

        // Optional plan reference — snapshot the plan name when provided.
        let mut plan_name = String::new();
        if let Some(plan_id) = input.plan_id {
            let plan = PlanEntity::find_by_id(plan_id)
                .one(&*self.db)
                .await
                .context("load referenced plan")?
                .ok_or(PaymentOrderError::PlanNotFound)?;
            if !Self::in_scope(actor_role, actor_tenant_id, &plan.tenant_id) {
                return Err(PaymentOrderError::PlanNotFound);
            }
            plan_name = plan.name;
        }

        let now = Utc::now();
        let paid_at = (status == "paid").then_some(now);
        let result: PaymentOrderResponse = ActiveModel {
            id: Set(crate::snowflake::generate_id()),
            user_id: Set(user.id),
            tenant_id: Set(user.tenant_id),
            user_email: Set(user.email),
            plan_id: Set(input.plan_id),
            plan_name: Set(plan_name),
            amount: Set(input.amount),
            status: Set(status),
            payment_method: Set(payment_method),
            external_txn_id: Set(input.external_txn_id),
            created_at: Set(now),
            paid_at: Set(paid_at),
        }
        .insert(self.db.write_conn())
        .await
        .context("create order")?
        .into();

        tracing::info!(order_id = %result.id.as_i64(), user_id = %input.user_id, "Order created by {actor_role}");
        Ok(result)
    }

    /// Update an order record.
    ///
    /// Record-keeping only: status transitions carry no side effects
    /// (no balance refund, no plan revocation). `paid_at` is stamped on the
    /// FIRST transition into "paid" and deliberately preserved afterwards —
    /// even across paid → refunded/cancelled → paid flips — as an audit
    /// record of when the payment originally settled.
    pub async fn update(
        &self,
        id: i64,
        input: UpdateOrderInput,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<PaymentOrderResponse, PaymentOrderError> {
        let order = self.load_scoped(id, actor_role, actor_tenant_id).await?;

        if let Some(ref status) = input.status {
            validate_order_status(status)?;
        }
        if let Some(ref method) = input.payment_method {
            validate_payment_method(method)?;
        }

        let had_paid_at = order.paid_at.is_some();
        let mut active: ActiveModel = order.into();
        if let Some(status) = input.status {
            // First transition into "paid" stamps the payment time.
            if status == "paid" && !had_paid_at {
                active.paid_at = Set(Some(Utc::now()));
            }
            active.status = Set(status);
        }
        if let Some(method) = input.payment_method {
            active.payment_method = Set(method);
        }
        if let Some(txn_id) = input.external_txn_id {
            active.external_txn_id = Set((!txn_id.is_empty()).then_some(txn_id));
        }

        let result: PaymentOrderResponse = active
            .update(self.db.write_conn())
            .await
            .context("update order")?
            .into();
        tracing::info!(order_id = %id, "Order updated");
        Ok(result)
    }

    /// Delete an order record.
    ///
    /// Orders form the audit trail of money movement, so the handler layer
    /// additionally restricts this operation to the `system` role — the
    /// tenant-scope check here is a second line of defense only.
    pub async fn delete(
        &self,
        id: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<(), PaymentOrderError> {
        self.load_scoped(id, actor_role, actor_tenant_id).await?;
        Entity::delete_by_id(id)
            .exec(self.db.write_conn())
            .await
            .context("delete order")?;
        tracing::info!(order_id = %id, "Order deleted");
        Ok(())
    }
}

fn validate_order_status(status: &str) -> Result<(), PaymentOrderError> {
    if ORDER_STATUSES.contains(&status) {
        Ok(())
    } else {
        Err(PaymentOrderError::InvalidInput(format!(
            "status must be one of: {}",
            ORDER_STATUSES.join(", ")
        )))
    }
}

fn validate_payment_method(method: &str) -> Result<(), PaymentOrderError> {
    if PAYMENT_METHODS.contains(&method) {
        Ok(())
    } else {
        Err(PaymentOrderError::InvalidInput(format!(
            "payment_method must be one of: {}",
            PAYMENT_METHODS.join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_status_validation() {
        for s in ORDER_STATUSES {
            assert!(validate_order_status(s).is_ok());
        }
        assert!(matches!(
            validate_order_status("shipped"),
            Err(PaymentOrderError::InvalidInput(_))
        ));
        assert!(matches!(
            validate_order_status(""),
            Err(PaymentOrderError::InvalidInput(_))
        ));
    }

    #[test]
    fn payment_method_validation() {
        for m in PAYMENT_METHODS {
            assert!(validate_payment_method(m).is_ok());
        }
        assert!(matches!(
            validate_payment_method("cash"),
            Err(PaymentOrderError::InvalidInput(_))
        ));
    }

    #[test]
    fn scope_rules() {
        assert!(PaymentOrderService::in_scope("system", "default", "other"));
        assert!(PaymentOrderService::in_scope("admin", "t1", "t1"));
        assert!(!PaymentOrderService::in_scope("admin", "t1", "t2"));
    }
}

//! Plan service — tenant-scoped subscription plan management.
//!
//! Covers plan CRUD (system: all tenants, admin: own tenant only), plan
//! instance assignment/purchase, and the per-call quota consumption used
//! by the AI response endpoint.
//!
//! # Tenant isolation
//!
//! All methods take `actor_role` / `actor_tenant_id` and enforce the same
//! RBAC convention as the users/channels modules: `system` operates across
//! tenants, `admin` is confined to its own tenant, and out-of-scope
//! resources are reported as `NotFound` (no existence leak).

use anyhow::Context;
use chrono::{Duration as ChronoDuration, Utc};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};
use std::sync::Arc;

use crate::{
    AutoRouter,
    repositories::{
        payment_order::{ActiveModel as OrderActiveModel, PaymentOrderResponse},
        plan::{
            ActiveModel as PlanActiveModel, Column as PlanColumn, Entity as PlanEntity,
            Model as PlanModel, PlanResponse,
        },
        user::{Entity as UserEntity, Model as UserModel},
        user_plan::{
            ActiveModel as UserPlanActiveModel, Column as UserPlanColumn, Entity as UserPlanEntity,
            SOURCE_ADMIN_GRANT, SOURCE_PURCHASE, UserPlanResponse,
        },
    },
    services::{CacheService, user::PaginatedResponse},
};

#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("Plan not found")]
    NotFound,
    #[error("User not found")]
    UserNotFound,
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    #[error("Insufficient balance")]
    InsufficientBalance,
    #[error("No active plan with remaining calls")]
    NoActivePlan,
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Input for plan creation. `tenant_id` is resolved at the handler layer
/// (system supplies it explicitly, admin is forced to its own tenant).
pub struct CreatePlanInput {
    pub tenant_id: String,
    pub name: String,
    pub description: Option<String>,
    pub price: i64,
    pub total_calls: i64,
    pub validity_days: i32,
    pub status: Option<String>,
}

/// Partial update for a plan — `None` fields are left unchanged.
#[derive(Default)]
pub struct UpdatePlanInput {
    pub name: Option<String>,
    pub description: Option<String>,
    pub price: Option<i64>,
    pub total_calls: Option<i64>,
    pub validity_days: Option<i32>,
    pub status: Option<String>,
}

/// Result of a successful balance purchase.
pub struct PurchaseOutcome {
    pub order: PaymentOrderResponse,
    pub user_plan: UserPlanResponse,
    /// The buyer's balance after deduction (stored units).
    pub balance: i64,
}

pub struct PlanService {
    db: Arc<AutoRouter>,
    cache: Option<CacheService>,
}

impl PlanService {
    pub fn new(db: Arc<AutoRouter>) -> Self {
        Self { db, cache: None }
    }

    /// With cache support — purchase invalidates the buyer's cached profile
    /// so the next get_me returns the deducted balance.
    pub fn with_cache(db: Arc<AutoRouter>, cache: CacheService) -> Self {
        Self {
            db,
            cache: Some(cache),
        }
    }

    /// Whether `actor` may see/touch a plan belonging to `plan_tenant_id`.
    fn in_scope(actor_role: &str, actor_tenant_id: &str, plan_tenant_id: &str) -> bool {
        actor_role == "system" || actor_tenant_id == plan_tenant_id
    }

    /// Load a plan enforcing tenant scope. Out-of-scope plans surface as
    /// `NotFound` to avoid leaking cross-tenant existence.
    async fn load_scoped_plan(
        &self,
        id: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<PlanModel, PlanError> {
        let plan = PlanEntity::find_by_id(id)
            .one(&*self.db)
            .await
            .context("load plan")?
            .ok_or(PlanError::NotFound)?;
        if !Self::in_scope(actor_role, actor_tenant_id, &plan.tenant_id) {
            return Err(PlanError::NotFound);
        }
        Ok(plan)
    }

    /// Load a user enforcing tenant scope (admin sees own tenant only).
    async fn load_scoped_user(
        &self,
        id: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<UserModel, PlanError> {
        let user = UserEntity::find_by_id(id)
            .one(&*self.db)
            .await
            .context("load user")?
            .ok_or(PlanError::UserNotFound)?;
        if !Self::in_scope(actor_role, actor_tenant_id, &user.tenant_id) {
            return Err(PlanError::UserNotFound);
        }
        Ok(user)
    }

    /// List plans with pagination.
    ///
    /// - system: all tenants, optionally narrowed by `tenant_filter`.
    /// - admin: always scoped to its own tenant (`tenant_filter` ignored).
    pub async fn list_paginated(
        &self,
        actor_role: &str,
        actor_tenant_id: &str,
        tenant_filter: Option<&str>,
        page: u64,
        per_page: u64,
    ) -> Result<PaginatedResponse<PlanResponse>, PlanError> {
        let page = page.clamp(1, 1_000_000);
        let per_page = per_page.clamp(1, 100);

        let tenant = if actor_role == "system" {
            tenant_filter.filter(|t| !t.is_empty()).map(str::to_string)
        } else {
            Some(actor_tenant_id.to_string())
        };

        let mut select = PlanEntity::find().order_by_desc(PlanColumn::CreatedAt);
        if let Some(ref t) = tenant {
            select = select.filter(PlanColumn::TenantId.eq(t));
        }

        let paginator = select.paginate(&*self.db, per_page);
        let total = paginator.num_items().await.context("count plans")?;
        let total_pages = total.div_ceil(per_page);
        let items: Vec<PlanResponse> = paginator
            .fetch_page(page - 1)
            .await
            .context("fetch plans page")?
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

    /// List the active (purchasable) plans of a tenant — user-facing.
    pub async fn list_available(&self, tenant_id: &str) -> Result<Vec<PlanResponse>, PlanError> {
        let items = PlanEntity::find()
            .filter(PlanColumn::TenantId.eq(tenant_id))
            .filter(PlanColumn::Status.eq("active"))
            .order_by_asc(PlanColumn::Price)
            .all(&*self.db)
            .await
            .context("list available plans")?;
        Ok(items.into_iter().map(Into::into).collect())
    }

    pub async fn create(&self, input: CreatePlanInput) -> Result<PlanResponse, PlanError> {
        if input.name.trim().is_empty() {
            return Err(PlanError::InvalidInput("name is required".into()));
        }
        validate_plan_numbers(input.price, input.total_calls, input.validity_days as i64)?;
        let status = input.status.unwrap_or_else(|| "active".to_string());
        validate_plan_status(&status)?;

        let now = Utc::now();
        let model = PlanActiveModel {
            id: Set(crate::snowflake::generate_id()),
            tenant_id: Set(input.tenant_id),
            name: Set(input.name.trim().to_string()),
            description: Set(input.description.unwrap_or_default()),
            price: Set(input.price),
            total_calls: Set(input.total_calls),
            validity_days: Set(input.validity_days),
            status: Set(status),
            created_at: Set(now),
            updated_at: Set(now),
        };
        let result: PlanResponse = model
            .insert(self.db.write_conn())
            .await
            .context("create plan")?
            .into();
        tracing::info!(plan_id = %result.id.as_i64(), tenant_id = %result.tenant_id, "Plan created");
        Ok(result)
    }

    pub async fn update(
        &self,
        id: i64,
        input: UpdatePlanInput,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<PlanResponse, PlanError> {
        let plan = self
            .load_scoped_plan(id, actor_role, actor_tenant_id)
            .await?;

        validate_plan_numbers(
            input.price.unwrap_or(plan.price),
            input.total_calls.unwrap_or(plan.total_calls),
            input.validity_days.unwrap_or(plan.validity_days) as i64,
        )?;
        if let Some(ref status) = input.status {
            validate_plan_status(status)?;
        }

        let mut active: PlanActiveModel = plan.into();
        if let Some(name) = input.name {
            if name.trim().is_empty() {
                return Err(PlanError::InvalidInput("name must not be empty".into()));
            }
            active.name = Set(name.trim().to_string());
        }
        if let Some(description) = input.description {
            active.description = Set(description);
        }
        if let Some(price) = input.price {
            active.price = Set(price);
        }
        if let Some(total_calls) = input.total_calls {
            active.total_calls = Set(total_calls);
        }
        if let Some(validity_days) = input.validity_days {
            active.validity_days = Set(validity_days);
        }
        if let Some(status) = input.status {
            active.status = Set(status);
        }
        active.updated_at = Set(Utc::now());

        let result: PlanResponse = active
            .update(self.db.write_conn())
            .await
            .context("update plan")?
            .into();
        tracing::info!(plan_id = %id, "Plan updated");
        Ok(result)
    }

    /// Delete a plan template. Existing user_plans instances are snapshots
    /// and remain valid — no cascading cleanup is needed.
    pub async fn delete(
        &self,
        id: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<(), PlanError> {
        self.load_scoped_plan(id, actor_role, actor_tenant_id)
            .await?;
        PlanEntity::delete_by_id(id)
            .exec(self.db.write_conn())
            .await
            .context("delete plan")?;
        tracing::info!(plan_id = %id, "Plan deleted");
        Ok(())
    }

    /// Grant a plan instance to a user (admin action — no payment order).
    ///
    /// Intentionally allows granting `disabled` plan templates: disabling a
    /// plan only removes it from the user-facing purchase list, while admins
    /// may still back-fill retired plans (e.g. honoring an offline deal).
    /// Only `purchase` is restricted to active plans.
    pub async fn assign_to_user(
        &self,
        target_user_id: i64,
        plan_id: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<UserPlanResponse, PlanError> {
        let user = self
            .load_scoped_user(target_user_id, actor_role, actor_tenant_id)
            .await?;
        let plan = self
            .load_scoped_plan(plan_id, actor_role, actor_tenant_id)
            .await?;
        // A plan grants calls against the tenant's channels, so the instance
        // must stay within the user's tenant even for system actors.
        if plan.tenant_id != user.tenant_id {
            return Err(PlanError::InvalidInput(
                "Plan does not belong to the user's tenant".into(),
            ));
        }

        let result = insert_user_plan(self.db.write_conn(), &user, &plan, SOURCE_ADMIN_GRANT)
            .await?
            .into();
        tracing::info!(
            user_id = %target_user_id,
            plan_id = %plan_id,
            "Plan assigned to user by {actor_role}"
        );
        Ok(result)
    }

    /// List all plan instances of a user (admin view, tenant-scoped).
    pub async fn list_user_plans(
        &self,
        target_user_id: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<Vec<UserPlanResponse>, PlanError> {
        self.load_scoped_user(target_user_id, actor_role, actor_tenant_id)
            .await?;
        self.list_plans_of(target_user_id).await
    }

    /// List all plan instances of the calling user (self view).
    pub async fn list_plans_of(&self, user_id: i64) -> Result<Vec<UserPlanResponse>, PlanError> {
        let items = UserPlanEntity::find()
            .filter(UserPlanColumn::UserId.eq(user_id))
            .order_by_desc(UserPlanColumn::CreatedAt)
            .all(&*self.db)
            .await
            .context("list user plans")?;
        Ok(items.into_iter().map(Into::into).collect())
    }

    /// Purchase a plan with account balance.
    ///
    /// Runs in a single transaction with `SELECT ... FOR UPDATE` on the buyer
    /// row (same TOCTOU protection as `UserService::adjust_balance`):
    /// deduct balance → insert a paid order → insert the plan instance.
    pub async fn purchase(&self, user_id: i64, plan_id: i64) -> Result<PurchaseOutcome, PlanError> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to start transaction")?;

        // Lock the buyer row to serialize concurrent balance mutations.
        let user = UserEntity::find_by_id(user_id)
            .lock_exclusive()
            .one(&txn)
            .await
            .context("load buyer")?
            .ok_or(PlanError::UserNotFound)?;

        let plan = PlanEntity::find_by_id(plan_id)
            .one(&txn)
            .await
            .context("load plan")?
            .ok_or(PlanError::NotFound)?;
        // Only active plans of the buyer's own tenant are purchasable;
        // both violations read as NotFound (no cross-tenant leak).
        if plan.tenant_id != user.tenant_id || plan.status != "active" {
            return Err(PlanError::NotFound);
        }
        if user.balance < plan.price {
            return Err(PlanError::InsufficientBalance);
        }

        let now = Utc::now();
        let new_balance = user.balance - plan.price;
        let tenant_id = user.tenant_id.clone();
        let user_email = user.email.clone();

        let mut buyer: crate::repositories::user::ActiveModel = user.clone().into();
        buyer.balance = Set(new_balance);
        buyer.updated_at = Set(now);
        buyer.update(&txn).await.context("deduct balance")?;

        let order = OrderActiveModel {
            id: Set(crate::snowflake::generate_id()),
            user_id: Set(user_id),
            tenant_id: Set(tenant_id),
            user_email: Set(user_email),
            plan_id: Set(Some(plan.id)),
            plan_name: Set(plan.name.clone()),
            amount: Set(plan.price),
            status: Set("paid".to_string()),
            payment_method: Set("balance".to_string()),
            external_txn_id: Set(None),
            created_at: Set(now),
            paid_at: Set(Some(now)),
        }
        .insert(&txn)
        .await
        .context("insert payment order")?;

        let user_plan = insert_user_plan(&txn, &user, &plan, SOURCE_PURCHASE).await?;

        txn.commit().await.context("commit purchase")?;

        tracing::info!(
            user_id = %user_id,
            plan_id = %plan_id,
            amount = %plan.price,
            "Plan purchased with balance"
        );

        // Invalidate the cached profile so get_me sees the deducted balance.
        if let Some(ref cache) = self.cache
            && let Err(e) = cache
                .invalidate(&crate::services::user::UserService::user_cache_key(user_id))
                .await
        {
            tracing::warn!(
                "Failed to invalidate cache for user {} after purchase: {:?}",
                user_id,
                e
            );
        }

        Ok(PurchaseOutcome {
            order: order.into(),
            user_plan: user_plan.into(),
            balance: new_balance,
        })
    }

    /// Consume one call from the user's earliest-expiring active plan and
    /// return the consumed instance ID (the "ticket" for a potential refund).
    ///
    /// A single atomic UPDATE picks the instance with the closest expiry
    /// that still has remaining calls. Plain `FOR UPDATE` is used on purpose:
    /// the lock is held only for this single statement (microseconds), and
    /// `SKIP LOCKED` would mis-report `NoActivePlan` when concurrent requests
    /// race on a user's only active instance.
    ///
    /// Zero affected rows is ambiguous under READ COMMITTED: when a
    /// concurrent request drains the last call of the instance the `LIMIT 1`
    /// subquery had picked, EvalPlanQual drops the re-checked row without
    /// restarting the scan — the statement affects nothing even though a
    /// later-expiring instance may still have calls. A cheap existence probe
    /// distinguishes "genuinely exhausted" from that dropout, and a bounded
    /// retry re-plans from a fresh snapshot to pick the next instance.
    pub async fn consume_call(&self, user_id: i64) -> Result<i64, PlanError> {
        const MAX_ATTEMPTS: u32 = 3;
        for _ in 0..MAX_ATTEMPTS {
            let row = self
                .db
                .write_conn()
                .query_one(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "UPDATE user_plans SET remaining_calls = remaining_calls - 1 \
                     WHERE id = ( \
                         SELECT id FROM user_plans \
                         WHERE user_id = $1 AND remaining_calls > 0 AND expires_at > NOW() \
                         ORDER BY expires_at ASC LIMIT 1 \
                         FOR UPDATE \
                     ) \
                     RETURNING id",
                    [user_id.into()],
                ))
                .await
                .context("consume plan call")?;

            if let Some(row) = row {
                return Ok(row
                    .try_get::<i64>("", "id")
                    .context("read consumed instance id")?);
            }

            // Probe on the write connection (read-your-writes): only retry
            // when a usable instance still exists, so genuinely exhausted
            // users pay one UPDATE + one SELECT, not MAX_ATTEMPTS updates.
            let eligible = self
                .db
                .write_conn()
                .query_one(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    "SELECT 1 AS one FROM user_plans \
                     WHERE user_id = $1 AND remaining_calls > 0 AND expires_at > NOW() \
                     LIMIT 1",
                    [user_id.into()],
                ))
                .await
                .context("recheck plan availability")?;
            if eligible.is_none() {
                break;
            }
        }
        Err(PlanError::NoActivePlan)
    }

    /// Refund one previously consumed call on `instance_id`.
    ///
    /// Compensation for channel-selection failures (NoChannel): the request
    /// never reached any upstream, and a missing channel is an operator-side
    /// configuration issue that must not burn user quota. Upstream failures
    /// after a channel was selected are intentionally NOT refunded.
    ///
    /// The `remaining_calls < total_calls` guard makes the refund idempotent
    /// against anomalous double-compensation. Refunding an instance that
    /// expired in the meantime is harmless — expired instances are never
    /// consumable regardless of their counter.
    pub async fn refund_call(&self, instance_id: i64) -> Result<(), PlanError> {
        self.db
            .write_conn()
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE user_plans SET remaining_calls = remaining_calls + 1 \
                 WHERE id = $1 AND remaining_calls < total_calls",
                [instance_id.into()],
            ))
            .await
            .context("refund plan call")?;
        Ok(())
    }
}

/// Insert a plan instance snapshot for `user` from the `plan` template.
async fn insert_user_plan<C: ConnectionTrait>(
    conn: &C,
    user: &UserModel,
    plan: &PlanModel,
    source: &str,
) -> Result<crate::repositories::user_plan::Model, PlanError> {
    let now = Utc::now();
    UserPlanActiveModel {
        id: Set(crate::snowflake::generate_id()),
        user_id: Set(user.id),
        plan_id: Set(Some(plan.id)),
        plan_name: Set(plan.name.clone()),
        total_calls: Set(plan.total_calls),
        remaining_calls: Set(plan.total_calls),
        expires_at: Set(now + ChronoDuration::days(plan.validity_days as i64)),
        source: Set(source.to_string()),
        created_at: Set(now),
    }
    .insert(conn)
    .await
    .context("insert user plan")
    .map_err(PlanError::Internal)
}

/// Upper bound for validity_days (~100 years). Prevents pathological values
/// from overflowing chrono's date range (DateTime + TimeDelta panics past
/// year 262142) when computing expires_at at grant/purchase time.
const MAX_VALIDITY_DAYS: i64 = 36_500;

/// Validate that plan numeric fields are strictly positive (and bounded).
fn validate_plan_numbers(
    price: i64,
    total_calls: i64,
    validity_days: i64,
) -> Result<(), PlanError> {
    if price <= 0 {
        return Err(PlanError::InvalidInput("price must be positive".into()));
    }
    if total_calls <= 0 {
        return Err(PlanError::InvalidInput(
            "total_calls must be positive".into(),
        ));
    }
    if validity_days <= 0 || validity_days > MAX_VALIDITY_DAYS {
        return Err(PlanError::InvalidInput(format!(
            "validity_days must be within 1..={MAX_VALIDITY_DAYS}"
        )));
    }
    Ok(())
}

/// Validate that a plan status string is one of the allowed values.
fn validate_plan_status(status: &str) -> Result<(), PlanError> {
    if status == "active" || status == "disabled" {
        Ok(())
    } else {
        Err(PlanError::InvalidInput(
            "status must be active or disabled".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_numbers_must_be_positive() {
        assert!(validate_plan_numbers(1, 1, 1).is_ok());
        assert!(validate_plan_numbers(1, 1, MAX_VALIDITY_DAYS).is_ok());
        assert!(matches!(
            validate_plan_numbers(0, 1, 1),
            Err(PlanError::InvalidInput(_))
        ));
        assert!(matches!(
            validate_plan_numbers(1, 0, 1),
            Err(PlanError::InvalidInput(_))
        ));
        assert!(matches!(
            validate_plan_numbers(1, 1, 0),
            Err(PlanError::InvalidInput(_))
        ));
        assert!(matches!(
            validate_plan_numbers(-5, 1, 1),
            Err(PlanError::InvalidInput(_))
        ));
        // Beyond the ~100-year cap: rejected to keep chrono date math safe.
        assert!(matches!(
            validate_plan_numbers(1, 1, MAX_VALIDITY_DAYS + 1),
            Err(PlanError::InvalidInput(_))
        ));
        assert!(matches!(
            validate_plan_numbers(1, 1, i32::MAX as i64),
            Err(PlanError::InvalidInput(_))
        ));
    }

    #[test]
    fn plan_status_validation() {
        assert!(validate_plan_status("active").is_ok());
        assert!(validate_plan_status("disabled").is_ok());
        assert!(matches!(
            validate_plan_status("pending"),
            Err(PlanError::InvalidInput(_))
        ));
        assert!(matches!(
            validate_plan_status(""),
            Err(PlanError::InvalidInput(_))
        ));
    }

    #[test]
    fn scope_rules() {
        assert!(PlanService::in_scope("system", "default", "other"));
        assert!(PlanService::in_scope("admin", "t1", "t1"));
        assert!(!PlanService::in_scope("admin", "t1", "t2"));
    }

    #[test]
    fn plan_error_formatting() {
        assert_eq!(PlanError::NotFound.to_string(), "Plan not found");
        assert_eq!(PlanError::UserNotFound.to_string(), "User not found");
        assert_eq!(
            PlanError::InsufficientBalance.to_string(),
            "Insufficient balance"
        );
        assert_eq!(
            PlanError::NoActivePlan.to_string(),
            "No active plan with remaining calls"
        );
    }
}

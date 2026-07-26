use anyhow::Context;
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseBackend, EntityTrait, FromQueryResult, PaginatorTrait,
    QueryFilter, QueryOrder, Set, Statement,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::{
    AutoRouter,
    repositories::{
        channel::Entity as ChannelEntity,
        plan::Entity as PlanEntity,
        tenant::{ActiveModel, DEFAULT_TENANT_ID, Entity, TenantResponse},
        user::Entity as UserEntity,
    },
    services::CacheService,
};

#[derive(Debug, thiserror::Error)]
pub enum TenantError {
    #[error("Tenant not found")]
    NotFound,
    #[error("Tenant {id} has {users} user(s), {channels} channel(s) and {plans} plan(s)")]
    NotEmpty {
        id: String,
        users: u64,
        channels: u64,
        plans: u64,
    },
    #[error("Invalid tenant status")]
    InvalidStatus,
    #[error("Cannot disable the default tenant")]
    CannotDisableDefault,
    #[error("Cannot delete the default tenant")]
    CannotDeleteDefault,
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub struct TenantService {
    db: Arc<AutoRouter>,
    cache: Option<CacheService>,
}
impl TenantService {
    pub fn new(db: Arc<AutoRouter>) -> Self {
        Self { db, cache: None }
    }
    /// Create a TenantService with Redis caching support for is_active checks.
    ///
    /// The cache stores only "active" results (30s TTL) to reduce DB queries
    /// on hot paths (every AI proxy request verifies tenant status).
    /// Non-active/disabled tenants are never cached — the next request always
    /// re-checks the DB and will cache as soon as the tenant becomes active.
    pub fn with_cache(db: Arc<AutoRouter>, cache: CacheService) -> Self {
        Self {
            db,
            cache: Some(cache),
        }
    }

    /// List tenants with pagination.
    ///
    /// Returns `(items, total_count)` — the handler layer wraps this into
    /// a paginated JSON response.
    pub async fn list_paginated(
        &self,
        page: u64,
        per_page: u64,
    ) -> Result<(Vec<TenantResponse>, u64), TenantError> {
        let db = &*self.db;
        let page = page.clamp(1, 1_000_000);
        let per_page = per_page.clamp(1, 100);

        let paginator = Entity::find()
            .order_by_asc(crate::repositories::tenant::Column::CreatedAt)
            .paginate(db, per_page);

        let total = paginator.num_items().await.context("count tenants")?;

        let tenant_models: Vec<crate::repositories::tenant::Model> = paginator
            .fetch_page(page - 1)
            .await
            .context("fetch tenants page")?;

        if tenant_models.is_empty() {
            return Ok((vec![], total));
        }

        // Full GROUP BY for user counts (same as the legacy list() —
        // the aggregation queries are cheap enough that a WHERE IN
        // filter per page is unnecessary complexity).
        #[derive(FromQueryResult)]
        struct UserCountRow {
            tenant_id: Option<String>,
            cnt: Option<i64>,
        }
        let all_user_counts: HashMap<String, u64> =
            UserCountRow::find_by_statement(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT tenant_id, COUNT(*)::BIGINT as cnt FROM users GROUP BY tenant_id",
                [],
            ))
            .all(db)
            .await
            .context("count users per tenant")?
            .into_iter()
            .filter_map(|r| r.tenant_id.zip(r.cnt.and_then(|c| u64::try_from(c).ok())))
            .collect();

        // Full GROUP BY for channel counts.
        #[derive(FromQueryResult)]
        struct ChannelCountRow {
            tenant_id: Option<String>,
            cnt: Option<i64>,
        }
        let all_channel_counts: HashMap<String, u64> =
            ChannelCountRow::find_by_statement(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT tenant_id, COUNT(*)::BIGINT as cnt FROM ai_gateway_channels GROUP BY tenant_id",
                [],
            ))
            .all(db)
            .await
            .context("count channels per tenant")?
            .into_iter()
            .filter_map(|r| {
                r.tenant_id
                    .zip(r.cnt.and_then(|c| u64::try_from(c).ok()))
            })
            .collect();

        // Merge counts into tenant responses.
        let items: Vec<TenantResponse> = tenant_models
            .into_iter()
            .map(|m| {
                let mut resp = TenantResponse::from(m);
                resp.user_count = all_user_counts.get(&resp.id).copied().unwrap_or(0);
                resp.channel_count = all_channel_counts.get(&resp.id).copied().unwrap_or(0);
                resp
            })
            .collect();

        Ok((items, total))
    }

    /// Batch-resolve tenant IDs to display names.
    ///
    /// Used to enrich admin-facing list responses (users/channels) with a
    /// `tenant_name` field so the UI need not pre-fetch the full tenant set.
    /// Unknown IDs are simply absent from the returned map — callers fall
    /// back to displaying the raw ID.
    pub async fn names_for(&self, ids: &[String]) -> Result<HashMap<String, String>, TenantError> {
        use crate::repositories::tenant::Column;
        use std::collections::HashSet;

        let unique: Vec<String> = ids
            .iter()
            .cloned()
            .collect::<HashSet<String>>()
            .into_iter()
            .collect();
        if unique.is_empty() {
            return Ok(HashMap::new());
        }

        let rows = Entity::find()
            .filter(Column::Id.is_in(unique))
            .all(&*self.db)
            .await
            .context("batch fetch tenant names")?;

        Ok(rows.into_iter().map(|m| (m.id, m.name)).collect())
    }
    pub async fn create(&self, name: String) -> Result<TenantResponse, TenantError> {
        let model = ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            name: Set(name),
            status: Set("active".into()),
            created_at: Set(Utc::now()),
        };
        let result: TenantResponse = model
            .insert(self.db.write_conn())
            .await
            .context("create tenant")?
            .into();
        tracing::info!(tenant_id = %result.id, name = %result.name, "Tenant created");
        Ok(result)
    }

    pub async fn update(
        &self,
        id: &str,
        name: Option<String>,
        status: Option<String>,
    ) -> Result<TenantResponse, TenantError> {
        // Only prevent changing the default tenant to disabled;
        // changing it back to active from disabled is allowed.
        if id == DEFAULT_TENANT_ID && status.as_deref() == Some("disabled") {
            return Err(TenantError::CannotDisableDefault);
        }
        if let Some(ref s) = status {
            validate_tenant_status(s)?;
        }
        let found = Entity::find_by_id(id)
            .one(&*self.db)
            .await
            .context("load tenant")?
            .ok_or(TenantError::NotFound)?;
        let mut active: ActiveModel = found.into();
        if let Some(name) = name {
            if name.trim().is_empty() {
                return Err(TenantError::InvalidInput("name must not be empty".into()));
            }
            active.name = Set(name);
        }
        let status_changed = status.is_some();
        if let Some(status) = status {
            active.status = Set(status);
        }
        let result = active
            .update(self.db.write_conn())
            .await
            .context("update tenant")?
            .into();

        tracing::info!(tenant_id = %id, "Tenant updated");

        // Invalidate is_active cache when status changes.
        if status_changed && let Some(ref cache) = self.cache {
            let cache_key = format!("tenant:active:{}", id);
            let _ = cache.invalidate(&cache_key).await;
        }

        Ok(result)
    }
    pub async fn delete(&self, id: &str) -> Result<(), TenantError> {
        if id == DEFAULT_TENANT_ID {
            return Err(TenantError::CannotDeleteDefault);
        }
        let db = self.db.write_conn();
        if Entity::find_by_id(id)
            .one(db)
            .await
            .context("load tenant")?
            .is_none()
        {
            return Err(TenantError::NotFound);
        }
        let users = UserEntity::find()
            .filter(crate::repositories::user::Column::TenantId.eq(id))
            .count(db)
            .await
            .context("count users")?;
        let channels = ChannelEntity::find()
            .filter(crate::repositories::channel::Column::TenantId.eq(id))
            .count(db)
            .await
            .context("count channels")?;
        // Plans reference tenants with ON DELETE RESTRICT — count them here
        // so the caller gets a structured NotEmpty error instead of a raw
        // FK violation from the database.
        let plans = PlanEntity::find()
            .filter(crate::repositories::plan::Column::TenantId.eq(id))
            .count(db)
            .await
            .context("count plans")?;
        if users != 0 || channels != 0 || plans != 0 {
            return Err(TenantError::NotEmpty {
                id: id.to_string(),
                users,
                channels,
                plans,
            });
        }
        if let Err(e) = Entity::delete_by_id(id).exec(db).await {
            // A dependent row (user / channel / plan) may be created between
            // the pre-check above and the delete; the RESTRICT FK then
            // rejects it. Surface the same structured NotEmpty error instead
            // of a raw 500. All three counts are re-read so the message
            // names the actual blocker regardless of which table raced.
            if matches!(
                e.sql_err(),
                Some(sea_orm::SqlErr::ForeignKeyConstraintViolation(_))
            ) {
                // The FK violation already proves the tenant is non-empty;
                // a recount failure must not downgrade that to a 500. Fall
                // back to zero counts, which the handler renders as the
                // generic "dependent resource(s)" message.
                let recount = async {
                    let users = UserEntity::find()
                        .filter(crate::repositories::user::Column::TenantId.eq(id))
                        .count(db)
                        .await?;
                    let channels = ChannelEntity::find()
                        .filter(crate::repositories::channel::Column::TenantId.eq(id))
                        .count(db)
                        .await?;
                    let plans = PlanEntity::find()
                        .filter(crate::repositories::plan::Column::TenantId.eq(id))
                        .count(db)
                        .await?;
                    Ok::<_, sea_orm::DbErr>((users, channels, plans))
                };
                let (users, channels, plans) = recount.await.unwrap_or_else(|e| {
                    tracing::warn!(
                        tenant_id = %id,
                        error = ?e,
                        "recount after FK violation failed; reporting generic NotEmpty"
                    );
                    (0, 0, 0)
                });
                return Err(TenantError::NotEmpty {
                    id: id.to_string(),
                    users,
                    channels,
                    plans,
                });
            }
            return Err(anyhow::Error::from(e).context("delete tenant").into());
        }

        tracing::info!(tenant_id = %id, "Tenant deleted");

        // Invalidate is_active cache so stale "active=true" results don't
        // linger after the tenant has been deleted.
        if let Some(ref cache) = self.cache {
            let cache_key = format!("tenant:active:{}", id);
            let _ = cache.invalidate(&cache_key).await;
        }

        Ok(())
    }
    pub async fn get(&self, id: &str) -> Result<TenantResponse, TenantError> {
        Entity::find_by_id(id)
            .one(&*self.db)
            .await
            .context("load tenant")?
            .map(Into::into)
            .ok_or(TenantError::NotFound)
    }
    pub async fn is_active(&self, id: &str) -> Result<bool, TenantError> {
        // Try cache first — only "active" results are cached (30s TTL).
        // Disabled/non-existent tenants are never cached, so a newly-enabled
        // tenant is immediately visible without waiting for TTL expiry.
        if let Some(ref cache) = self.cache {
            let cache_key = format!("tenant:active:{}", id);
            if let Ok(Some(true)) = cache.get::<bool>(&cache_key).await {
                return Ok(true);
            }
        }

        // DB fallback
        let active = Entity::find_by_id(id)
            .one(&*self.db)
            .await
            .context("load tenant")?
            .map(|t| t.status == "active")
            .unwrap_or(false);

        // Populate cache only for active tenants (negative caching disabled)
        if active && let Some(ref cache) = self.cache {
            let cache_key = format!("tenant:active:{}", id);
            let _ = cache.set(&cache_key, &true, Duration::from_secs(30)).await;
        }

        Ok(active)
    }
}

/// Validate that a tenant status string is one of the allowed values.
pub fn validate_tenant_status(status: &str) -> Result<(), TenantError> {
    if status == "active" || status == "disabled" {
        Ok(())
    } else {
        Err(TenantError::InvalidStatus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::DatabaseConnection;

    #[test]
    fn default_tenant_cannot_be_disabled() {
        // The default tenant can never be disabled — this is a design invariant.
        let service = TenantService::new(AutoRouter::single(DatabaseConnection::Disconnected));
        let result =
            futures::executor::block_on(service.update("default", None, Some("disabled".into())));
        assert!(matches!(result, Err(TenantError::CannotDisableDefault)));
    }

    #[test]
    fn default_tenant_cannot_be_deleted() {
        let service = TenantService::new(AutoRouter::single(DatabaseConnection::Disconnected));
        let result = futures::executor::block_on(service.delete("default"));
        assert!(matches!(result, Err(TenantError::CannotDeleteDefault)));
    }

    #[test]
    fn tenant_error_formatting() {
        assert_eq!(TenantError::NotFound.to_string(), "Tenant not found");
        assert_eq!(
            TenantError::NotEmpty {
                id: "t1".into(),
                users: 3,
                channels: 2,
                plans: 0,
            }
            .to_string(),
            "Tenant t1 has 3 user(s), 2 channel(s) and 0 plan(s)"
        );
        assert_eq!(
            TenantError::NotEmpty {
                id: "t2".into(),
                users: 0,
                channels: 1,
                plans: 4,
            }
            .to_string(),
            "Tenant t2 has 0 user(s), 1 channel(s) and 4 plan(s)"
        );
        assert_eq!(
            TenantError::InvalidStatus.to_string(),
            "Invalid tenant status"
        );
        assert_eq!(
            TenantError::CannotDisableDefault.to_string(),
            "Cannot disable the default tenant"
        );
        assert_eq!(
            TenantError::CannotDeleteDefault.to_string(),
            "Cannot delete the default tenant"
        );
    }

    #[test]
    fn validate_tenant_status_rejects_invalid() {
        assert!(validate_tenant_status("active").is_ok());
        assert!(validate_tenant_status("disabled").is_ok());
        assert!(matches!(
            validate_tenant_status(""),
            Err(TenantError::InvalidStatus)
        ));
        assert!(matches!(
            validate_tenant_status("pending"),
            Err(TenantError::InvalidStatus)
        ));
        assert!(matches!(
            validate_tenant_status("ACTIVE"),
            Err(TenantError::InvalidStatus)
        ));
        assert!(matches!(
            validate_tenant_status("suspended"),
            Err(TenantError::InvalidStatus)
        ));
    }
}

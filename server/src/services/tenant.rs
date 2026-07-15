use anyhow::Context;
use chrono::Utc;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, Set};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::{
    AutoRouter,
    repositories::{
        channel::Entity as ChannelEntity,
        tenant::{ActiveModel, DEFAULT_TENANT_ID, Entity, TenantResponse},
        user::Entity as UserEntity,
    },
    services::CacheService,
};

#[derive(Debug, thiserror::Error)]
pub enum TenantError {
    #[error("Tenant not found")]
    NotFound,
    #[error("Tenant contains users or channels")]
    NotEmpty,
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
    pub async fn list(&self) -> Result<Vec<TenantResponse>, TenantError> {
        Ok(Entity::find()
            .all(&*self.db)
            .await
            .context("list tenants")?
            .into_iter()
            .map(Into::into)
            .collect())
    }
    pub async fn create(&self, name: String) -> Result<TenantResponse, TenantError> {
        let model = ActiveModel {
            id: Set(Uuid::new_v4().to_string()),
            name: Set(name),
            status: Set("active".into()),
            created_at: Set(Utc::now()),
        };
        Ok(model
            .insert(self.db.write_conn())
            .await
            .context("create tenant")?
            .into())
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
        if users != 0 || channels != 0 {
            return Err(TenantError::NotEmpty);
        }
        Entity::delete_by_id(id)
            .exec(db)
            .await
            .context("delete tenant")?;

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
            TenantError::NotEmpty.to_string(),
            "Tenant contains users or channels"
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

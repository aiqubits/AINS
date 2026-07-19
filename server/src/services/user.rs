use std::sync::Arc;

use crate::repositories::tenant::DEFAULT_TENANT_ID;
use crate::repositories::user::{
    ActiveModel, Column, CreateUserInput, Entity as UserEntity, Model as UserModel,
    UpdateUserInput, UserResponse,
};
use crate::services::cache::CacheService;
use crate::utils::db_router::AutoRouter;
use crate::utils::password::{hash_password, verify_password};
use crate::utils::validator::require_password;
use anyhow::Context;
use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, ConnectionTrait, DatabaseBackend, EntityTrait, PaginatorTrait,
    QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};
use serde::Serialize;

/// Balance scale factor: 1 display unit = 10^10 stored units (1 × 10^10).
pub const BALANCE_SCALE: i64 = 10_000_000_000;

/// Typed errors for user service operations
#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error("User not found")]
    NotFound,
    #[error("Email already registered")]
    EmailConflict,
    #[error("Invalid credentials")]
    InvalidCredentials,
    #[error("Operation forbidden: {0}")]
    Forbidden(String),
    #[error("Weak password: {0}")]
    WeakPassword(String),
    #[error("Password unchanged: {0}")]
    SamePassword(String),
    #[error("Operation not allowed: {0}")]
    NotAllowed(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Check RBAC rules for balance modification operations on a loaded user.
fn check_balance_rbac(
    target: &UserModel,
    actor_role: &str,
    actor_tenant_id: &str,
) -> Result<(), UserError> {
    // Protect system accounts — no one can modify a system user's balance
    if target.role == "system" {
        tracing::warn!(
            target_user_id = %target.id,
            "Attempt to modify system account balance — returning NotFound"
        );
        return Err(UserError::NotFound);
    }

    // System super-admin can modify any non-system user's balance
    // (including cross-tenant and across all roles).
    if actor_role == "system" {
        return Ok(());
    }

    // Admin scope: can only modify users within their own tenant
    if actor_role == "admin" && target.tenant_id != actor_tenant_id {
        tracing::warn!(
            target_user_id = %target.id,
            actor_role = %actor_role,
            target_tenant = %target.tenant_id,
            actor_tenant = %actor_tenant_id,
            "Admin attempted to modify cross-tenant user balance — returning NotFound"
        );
        return Err(UserError::NotFound);
    }

    // Admin scope: can only modify user accounts
    if actor_role == "admin" && target.role != "user" {
        tracing::warn!(
            target_user_id = %target.id,
            actor_role = %actor_role,
            target_role = %target.role,
            "Admin attempted to modify non-user account balance — returning NotFound"
        );
        return Err(UserError::NotFound);
    }

    // Regular users cannot modify any balance
    if actor_role == "user" {
        return Err(UserError::NotAllowed(
            "Users cannot modify balance".to_string(),
        ));
    }

    // Defensive catch-all: unrecognized roles are not permitted.
    // Only "system" or "admin" should reach this point.
    if actor_role != "system" && actor_role != "admin" {
        return Err(UserError::NotAllowed(format!(
            "Role '{actor_role}' is not allowed to modify balance"
        )));
    }
    Ok(())
}

/// User service for CRUD operations
pub struct UserService {
    db: Arc<AutoRouter>,
    cache: CacheService,
}
/// Pagination parameters
#[derive(Debug)]
pub struct PaginationParams {
    pub page: u64,
    pub per_page: u64,
}

impl Default for PaginationParams {
    fn default() -> Self {
        Self {
            page: 1,
            per_page: 10,
        }
    }
}

/// Paginated response
///
/// NOTE: `T` must implement `Serialize` because this struct derives `Serialize`
/// (required for JSON responses via `ains_runtime::Response::json`). Adding
/// a new paginated endpoint whose item type does not impl `Serialize` will
/// cause a compile error here — make the item type serializable first.
#[derive(Debug, Serialize)]
pub struct PaginatedResponse<T: Serialize> {
    pub items: Vec<T>,
    pub total: u64,
    pub page: u64,
    pub per_page: u64,
    pub total_pages: u64,
}

impl UserService {
    /// Create a new user service
    pub fn new(db: Arc<AutoRouter>, cache: CacheService) -> Self {
        Self { db, cache }
    }

    /// Create a new user
    pub async fn create_user(
        &self,
        input: CreateUserInput,
        actor_role: &str,
    ) -> Result<UserResponse, UserError> {
        self.create_user_for_tenant(input, actor_role, DEFAULT_TENANT_ID)
            .await
    }

    /// Create a user in an explicitly selected tenant.  The public legacy
    /// method above deliberately keeps default-tenant behaviour for callers
    /// that predate multi-tenancy.
    pub async fn create_user_for_tenant(
        &self,
        input: CreateUserInput,
        actor_role: &str,
        tenant_id: &str,
    ) -> Result<UserResponse, UserError> {
        tracing::trace!("Creating user with email: {}", input.email);

        require_password(&input.password).map_err(UserError::WeakPassword)?;

        let password_hash = hash_password(&input.password).context("Failed to hash password")?;

        // Determine role based on actor's authority
        let role = match actor_role {
            "system" => {
                let r = input.role.as_deref().unwrap_or("user");
                if r != "user" && r != "admin" {
                    return Err(UserError::NotAllowed(
                        "Role must be 'user' or 'admin'".to_string(),
                    ));
                }
                r.to_string()
            }
            "admin" => {
                if input.role.as_deref() == Some("admin") {
                    return Err(UserError::NotAllowed(
                        "Admin can only create user accounts".to_string(),
                    ));
                }
                "user".to_string()
            }
            _ => "user".to_string(),
        };

        let now = Utc::now();
        let user = ActiveModel {
            id: Set(crate::snowflake::generate_id()),
            // Email is already normalized to lowercase by the handler (the
            // caller's responsibility). The .to_lowercase() here is idempotent
            // and serves as defense-in-depth.
            email: Set(input.email.to_lowercase()),
            password_hash: Set(password_hash),
            name: Set(input.name),
            role: Set(role),
            created_at: Set(now),
            updated_at: Set(now),
            token_version: Set(1),
            email_verified: Set(false),
            verification_code_hash: Set(None),
            verification_code_expires_at: Set(None),
            verification_code_sent_at: Set(None),
            verification_failed_attempts: Set(0),
            password_reset_token_hash: Set(None),
            password_reset_expires_at: Set(None),
            password_reset_sent_at: Set(None),
            password_reset_failed_attempts: Set(0),
            balance: Set(0),
            wx_openid: Set(None),
            tenant_id: Set(tenant_id.to_string()),
        };

        tracing::debug!("Inserting user into database");
        let result = user.insert(&*self.db).await.map_err(|e| {
            if matches!(
                e.sql_err(),
                Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
            ) {
                UserError::EmailConflict
            } else {
                UserError::Internal(anyhow::Error::from(e).context("Failed to create user"))
            }
        })?;
        tracing::debug!("User inserted successfully");

        tracing::info!("User created: {}", result.email);

        // Invalidate count cache so new users appear in pagination immediately.
        // Best-effort: failures are non-fatal.
        // NOTE: When the system role (super-admin) lists users, the count is all users
        // (no role filter), so the "system" key must also be invalidated.
        let _ = self
            .cache
            .invalidate(&Self::user_count_cache_key("admin", Some(tenant_id)))
            .await;
        let _ = self
            .cache
            .invalidate(&Self::user_count_cache_key("user", Some(tenant_id)))
            .await;
        let _ = self
            .cache
            .invalidate(&Self::user_count_cache_key("system", None))
            .await;

        Ok(UserResponse::from(result))
    }

    /// Cache TTLs for user profile data.
    const USER_CACHE_TTL_SECS: u64 = 300; // 5 minutes for user data
    const USER_NULL_TTL_SECS: u64 = 30; // 30 seconds for negative cache

    fn user_cache_key(id: i64) -> String {
        format!("user:{}", id)
    }

    /// Get user by ID (unscoped — caller is responsible for authorization).
    ///
    /// This is used internally (e.g., `get_me`) where the actor is fetching their
    /// own data. No role-based filtering is applied.
    ///
    /// Implements caching with the `get_or_insert` pattern:
    /// 1. Try Redis cache first
    /// 2. On miss, query the write database (read-your-writes consistency)
    /// 3. Populate cache on hit, set null-marker on miss (cache penetration protection)
    ///
    /// Cache is invalidated on every write operation (update, delete, password change).
    pub async fn get_user(&self, id: i64) -> Result<Option<UserResponse>, UserError> {
        let cache_key = Self::user_cache_key(id);
        let ttl = std::time::Duration::from_secs(Self::USER_CACHE_TTL_SECS);
        let null_ttl = std::time::Duration::from_secs(Self::USER_NULL_TTL_SECS);

        // 1. Try cache
        if let Ok(Some(user)) = self.cache.get::<UserResponse>(&cache_key).await {
            return Ok(Some(user));
        }

        // 1b. Check negative-cache marker to avoid cache-penetration DB queries.
        //     When a previous lookup confirmed this user does not exist, a short-lived
        //     ":null" marker is stored.  Skipping this check would defeat the purpose
        //     of storing the marker in the first place.
        if self
            .cache
            .exists(&format!("{}:null", cache_key))
            .await
            .unwrap_or(false)
        {
            return Ok(None);
        }

        // 2. DB fallback
        let user = UserEntity::find_by_id(id)
            .one(self.db.write_conn())
            .await
            .context("Failed to query user")?;

        match user {
            Some(u) => {
                let resp = UserResponse::from(u);
                // Best-effort cache populate
                if let Err(e) = self.cache.set(&cache_key, &resp, ttl).await {
                    tracing::warn!("Failed to cache user {}: {:?}", id, e);
                }
                Ok(Some(resp))
            }
            None => {
                // Negative cache to prevent cache penetration
                if let Err(e) = self.cache.set_null(&cache_key, null_ttl).await {
                    tracing::warn!("Failed to set null cache for user {}: {:?}", id, e);
                }
                Ok(None)
            }
        }
    }

    /// Get user by ID with RBAC scope enforcement.
    ///
    /// When `actor_role` is `"admin"`, the query filters to only return users with
    /// `role = "user"` within the same tenant. This ensures that non-existent users
    /// and scoped-out users both return `None`, eliminating the timing side-channel
    /// that would otherwise allow an admin to distinguish "user does not exist" from
    /// "user exists but is scoped out".
    pub async fn get_user_scoped(
        &self,
        id: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<Option<UserResponse>, UserError> {
        // Delegate to get_user() which uses the cache layer.
        // RBAC filtering is applied in-memory rather than in SQL so that
        // the cache (keyed by user:{id}) can be shared across roles.
        let user = match self.get_user(id).await? {
            Some(u) => u,
            None => return Ok(None),
        };

        // Admin scope: admin can only view user accounts within their own tenant
        if actor_role == "admin" && (user.role != "user" || user.tenant_id != actor_tenant_id) {
            return Ok(None);
        }

        Ok(Some(user))
    }

    /// Update user
    pub async fn update_user(
        &self,
        id: i64,
        input: UpdateUserInput,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<UserResponse, UserError> {
        // READ FROM WRITE: guarantees read-your-writes consistency — a user
        // that exists on the write database must not appear as NotFound due to
        // replication lag, even if this operation was triggered immediately
        // after creation by another node.
        let user = UserEntity::find_by_id(id)
            .one(self.db.write_conn())
            .await
            .context("Failed to query user")?
            .ok_or(UserError::NotFound)?;

        // Prevent modification of the system admin account (security boundary)
        // NOTE: returns NotFound (not Forbidden) to prevent user enumeration —
        // non-existent users and protected accounts are indistinguishable.
        if user.role == "system" {
            tracing::warn!(
                target_user_id = %id,
                "Attempt to modify system account — returning NotFound"
            );
            return Err(UserError::NotFound);
        }

        // Admin scope: can only modify users within their own tenant
        // NOTE: returns NotFound (not NotAllowed) to prevent user enumeration.
        if actor_role == "admin" && user.tenant_id != actor_tenant_id {
            tracing::warn!(
                target_user_id = %id,
                actor_role = %actor_role,
                target_tenant = %user.tenant_id,
                actor_tenant = %actor_tenant_id,
                "Admin attempted to modify cross-tenant user — returning NotFound"
            );
            return Err(UserError::NotFound);
        }

        // Admin scope: can only modify user accounts
        // NOTE: returns NotFound (not NotAllowed) to prevent user enumeration.
        if actor_role == "admin" && user.role != "user" {
            tracing::warn!(
                target_user_id = %id,
                actor_role = %actor_role,
                target_role = %user.role,
                "Admin attempted to modify non-user account — returning NotFound"
            );
            return Err(UserError::NotFound);
        }

        // Admin cannot promote users to admin
        if actor_role == "admin" && input.role.as_deref() == Some("admin") {
            return Err(UserError::NotAllowed(
                "Admin cannot promote users to admin".to_string(),
            ));
        }

        let old_role = user.role.clone();
        let old_tenant_id = user.tenant_id.clone();
        let mut active_model: ActiveModel = user.into();

        if let Some(email) = input.email {
            active_model.email = Set(email.to_lowercase());
        }
        if let Some(name) = input.name {
            active_model.name = Set(name);
        }

        // Tenant reassignment: only system may move a user across tenants.
        // The handler forces `tenant_id` to None for non-system actors, so this
        // check is defense-in-depth. Target tenant existence/active state is
        // validated at the handler layer (mirrors create_user).
        let mut new_tenant_id: Option<String> = None;
        if actor_role == "system"
            && let Some(ref tid) = input.tenant_id
            && *tid != old_tenant_id
        {
            active_model.tenant_id = Set(tid.clone());
            new_tenant_id = Some(tid.clone());
        }

        // Always exclude token_version from the ActiveModel update to avoid
        // overwriting a concurrent atomic increment (password change or role change).
        // When role actually changes, it will be atomically incremented inside
        // the transaction below via raw SQL.
        active_model.token_version = sea_orm::ActiveValue::NotSet;

        let mut token_version_stmt: Option<Statement> = None;
        let mut role_changed = false;
        let tenant_changed = new_tenant_id.is_some();

        if let Some(ref new_role) = input.role {
            // Defense-in-depth: validate role value is one of the allowed values.
            // Handler-level validation should catch this first, but the service
            // must not blindly persist arbitrary role values.
            if new_role != "user" && new_role != "admin" {
                return Err(UserError::NotAllowed(
                    "Role must be 'user' or 'admin'".to_string(),
                ));
            }

            tracing::info!(
                target_user_id = %id,
                old_role = %old_role,
                new_role = %new_role,
                "Role change requested"
            );
            active_model.role = Set(new_role.clone());

            if *new_role != old_role {
                role_changed = true;
            }
        }

        // A role change OR a cross-tenant move must invalidate all existing JWTs:
        // the token embeds both `role` and `tenant_id` as claims, and the auth
        // layer trusts those claims (it only re-verifies `token_version` against
        // the DB). Without this atomic increment, a moved user would retain valid
        // access to their OLD tenant's resources via a stale JWT until expiry.
        // The increment runs in the SAME transaction as the field update below.
        if role_changed || tenant_changed {
            token_version_stmt = Some(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "UPDATE users SET token_version = token_version + 1 WHERE id = $1",
                [id.into()],
            ));
        }

        active_model.updated_at = Set(Utc::now());

        // When the role or tenant changed, wrap the token_version increment and
        // the field update in a single transaction so that a partial failure
        // (e.g. email conflict) does not leave token_version incremented while
        // the requested changes are rolled back.
        let result = if let Some(stmt) = token_version_stmt {
            let txn = self
                .db
                .begin()
                .await
                .context("Failed to begin transaction for token_version change")?;
            txn.execute(stmt)
                .await
                .context("Failed to atomically increment token_version")?;
            let _updated = active_model.update(&txn).await.map_err(|e| {
                if matches!(
                    e.sql_err(),
                    Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
                ) {
                    UserError::EmailConflict
                } else {
                    UserError::Internal(
                        anyhow::Error::from(e).context("Failed to update user in transaction"),
                    )
                }
            })?;
            txn.commit()
                .await
                .context("Failed to commit token_version-change transaction")?;
            // Re-query to obtain the atomically-incremented token_version
            // (ActiveModel::update returns the model with the old token_version
            //  because it was set to NotSet in the SET clause).
            UserEntity::find_by_id(id)
                .one(self.db.write_conn())
                .await
                .context("Failed to re-query user after role change")?
                .ok_or(UserError::NotFound)?
        } else {
            active_model.update(&*self.db).await.map_err(|e| {
                if matches!(
                    e.sql_err(),
                    Some(sea_orm::SqlErr::UniqueConstraintViolation(_))
                ) {
                    UserError::EmailConflict
                } else {
                    UserError::Internal(anyhow::Error::from(e).context("Failed to update user"))
                }
            })?
        };

        tracing::info!("User updated: {}", result.email);

        // Invalidate cache so next read fetches fresh data.
        if let Err(e) = self.cache.invalidate(&Self::user_cache_key(id)).await {
            tracing::warn!("Failed to invalidate cache for user {}: {:?}", id, e);
        }
        // Invalidate token_version cache when role or tenant changed (both
        // atomically increment token_version above).
        if role_changed || tenant_changed {
            let token_cache_key = format!("user:token_version:{}", id);
            if let Err(e) = self.cache.invalidate(&token_cache_key).await {
                tracing::warn!(
                    "Failed to invalidate token_version cache for user {}: {:?}",
                    id,
                    e
                );
            }
        }

        // When the user was moved to a different tenant, invalidate the user-count
        // caches for BOTH the source and destination tenants (admin + user views)
        // plus the system-wide count, so paginated listings stay accurate.
        if let Some(ref new_tid) = new_tenant_id {
            for tid in [old_tenant_id.as_str(), new_tid.as_str()] {
                let _ = self
                    .cache
                    .invalidate(&Self::user_count_cache_key("admin", Some(tid)))
                    .await;
                let _ = self
                    .cache
                    .invalidate(&Self::user_count_cache_key("user", Some(tid)))
                    .await;
            }
            let _ = self
                .cache
                .invalidate(&Self::user_count_cache_key("system", None))
                .await;
        }

        Ok(UserResponse::from(result))
    }

    /// Change user password.
    ///
    /// Verifies the current password, validates new password strength,
    /// hashes the new password, updates the database, and increments
    /// `token_version` to invalidate all existing JWTs.
    ///
    /// Returns the updated `UserResponse` together with the new `token_version`
    /// so the caller can issue a fresh JWT.
    pub async fn change_password(
        &self,
        id: i64,
        current_password: &str,
        new_password: &str,
    ) -> Result<(UserResponse, i32), UserError> {
        // READ FROM WRITE: must see the latest user state to avoid falsely
        // rejecting a password change when the user record is not yet visible
        // on a lagging read replica.
        let user = UserEntity::find_by_id(id)
            .one(self.db.write_conn())
            .await
            .context("Failed to query user")?
            .ok_or(UserError::NotFound)?;

        // Verify current password
        let is_valid = verify_password(current_password, &user.password_hash)
            .context("Failed to verify password")?;
        if !is_valid {
            return Err(UserError::InvalidCredentials);
        }

        // Reject unchanged password — prevents wasted crypto work and
        // unnecessary token_version increment (defense-in-depth; the handler
        // also checks this, but the service owns the semantic boundary).
        if current_password == new_password {
            return Err(UserError::SamePassword(
                "New password must be different from current password".to_string(),
            ));
        }

        // Validate new password strength
        require_password(new_password).map_err(UserError::WeakPassword)?;

        // Hash new password
        let new_hash = hash_password(new_password).context("Failed to hash password")?;

        // Atomically increment token_version at the database level.
        // The raw SQL "SET token_version = token_version + 1" evaluates the
        // increment using the current DB value, avoiding the read-modify-write
        // race condition that would occur with the application-level pattern
        // "read → compute → write".
        let stmt = Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE users SET token_version = token_version + 1 WHERE id = $1",
            [id.into()],
        );

        // Wrap the token_version increment, password update, and refresh token
        // revocation in a single transaction so that a partial failure does not
        // leave token_version incremented while the password or refresh tokens
        // remain in an inconsistent state.
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to begin transaction for password change")?;
        txn.execute(stmt)
            .await
            .context("Failed to atomically increment token_version")?;

        // Update password_hash and updated_at via ActiveModel.
        // token_version is explicitly set to NotSet so the ActiveModel update
        // does not overwrite the atomically-incremented value.
        let mut active_model: ActiveModel = user.into();
        active_model.token_version = sea_orm::ActiveValue::NotSet;
        active_model.password_hash = Set(new_hash);
        active_model.updated_at = Set(Utc::now());

        active_model.update(&txn).await.map_err(|e| {
            UserError::Internal(anyhow::Error::from(e).context("Failed to update password"))
        })?;

        // Revoke all refresh tokens so a stolen refresh cookie cannot mint a
        // fresh JWT at the new token_version. Without this, the token_version
        // bump alone is insufficient — the refresh endpoint reads the current
        // user.token_version and would happily sign a new JWT for an attacker.
        txn.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "DELETE FROM refresh_tokens WHERE user_id = $1",
            [id.into()],
        ))
        .await
        .context("Failed to revoke refresh tokens during password change")?;

        txn.commit()
            .await
            .context("Failed to commit password-change transaction")?;

        tracing::info!("User {} changed password", id);

        // Re-query to obtain the atomically-incremented token_version.
        let updated = UserEntity::find_by_id(id)
            .one(self.db.write_conn())
            .await
            .context("Failed to re-query user after password change")?
            .ok_or(UserError::NotFound)?;
        let new_version = updated.token_version;

        // Invalidate cache so next get_user fetches fresh data.
        if let Err(e) = self.cache.invalidate(&Self::user_cache_key(id)).await {
            tracing::warn!("Failed to invalidate cache for user {}: {:?}", id, e);
        }
        // Invalidate token_version cache so auth middleware reads fresh value.
        let token_cache_key = format!("user:token_version:{}", id);
        if let Err(e) = self.cache.invalidate(&token_cache_key).await {
            tracing::warn!(
                "Failed to invalidate token_version cache for user {}: {:?}",
                id,
                e
            );
        }

        Ok((UserResponse::from(updated), new_version))
    }

    /// Delete user
    pub async fn delete_user(
        &self,
        id: i64,
        actor_role: &str,
        actor_id: i64,
        actor_tenant_id: &str,
    ) -> Result<(), UserError> {
        // Fetch target from the write database so that a recently-created
        // user is not falsely reported as NotFound due to read replica lag.
        let target = UserEntity::find_by_id(id)
            .one(self.db.write_conn())
            .await
            .context("Failed to query user")?
            .ok_or(UserError::NotFound)?;

        // Prevent self-deletion (check after DB fetch so that a non-existent
        // user ID that happens to match actor_id still returns NotFound).
        if id == actor_id {
            return Err(UserError::NotAllowed(
                "Cannot delete your own account".to_string(),
            ));
        }

        // Prevent deletion of the system admin account
        if target.role == "system" {
            tracing::warn!(
                target_user_id = %id,
                "Attempt to delete system account — returning NotFound"
            );
            return Err(UserError::NotFound);
        }

        // Admin scope: can only delete users within their own tenant
        // NOTE: returns NotFound (not NotAllowed) to prevent user enumeration.
        if actor_role == "admin" && target.tenant_id != actor_tenant_id {
            tracing::warn!(
                target_user_id = %id,
                actor_role = %actor_role,
                target_tenant = %target.tenant_id,
                actor_tenant = %actor_tenant_id,
                "Admin attempted to delete cross-tenant user — returning NotFound"
            );
            return Err(UserError::NotFound);
        }

        // Admin scope: can only delete user accounts
        // NOTE: returns NotFound (not NotAllowed) to prevent user enumeration.
        if actor_role == "admin" && target.role != "user" {
            tracing::warn!(
                target_user_id = %id,
                actor_role = %actor_role,
                target_role = %target.role,
                "Admin attempted to delete non-user account — returning NotFound"
            );
            return Err(UserError::NotFound);
        }

        // Use delete_many with role filter for admin as TOCTOU defense:
        // the target's role could have changed between the fetch above and
        // this DELETE statement. Adding AND role = 'user' makes the delete
        // safely fail (0 rows) instead of deleting a now-protected account.
        let mut delete_stmt = UserEntity::delete_many().filter(Column::Id.eq(id));
        if actor_role == "admin" {
            delete_stmt = delete_stmt.filter(Column::Role.eq("user"));
        }

        // Cascade delete refresh tokens to prevent orphaned records.
        // Best-effort: failure to clean up refresh tokens is non-critical
        // (orphaned tokens are functionally inert) and must not prevent
        // the user deletion from succeeding.
        if let Err(e) = self
            .db
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "DELETE FROM refresh_tokens WHERE user_id = $1",
                [id.into()],
            ))
            .await
        {
            tracing::warn!(
                "Failed to cascade-delete refresh tokens for user {}: {:?}",
                id,
                e
            );
        }

        let result = delete_stmt
            .exec(&*self.db)
            .await
            .context("Failed to delete user")?;

        if result.rows_affected == 0 {
            return Err(UserError::NotFound);
        }

        tracing::info!("User deleted: {}", id);

        // Invalidate user profile cache (also removes null marker).
        if let Err(e) = self.cache.invalidate(&Self::user_cache_key(id)).await {
            tracing::warn!(
                "Failed to invalidate cache for deleted user {}: {:?}",
                id,
                e
            );
        }
        // Invalidate token_version cache so auth middleware rejects stale tokens.
        let token_cache_key = format!("user:token_version:{}", id);
        if let Err(e) = self.cache.invalidate(&token_cache_key).await {
            tracing::warn!(
                "Failed to invalidate token_version cache for user {}: {:?}",
                id,
                e
            );
        }
        // Invalidate count cache so pagination reflects deletion immediately.
        // NOTE: When the system role lists users, the count is all users (no role filter),
        // so the "system" key must also be invalidated.
        // NOTE: admin cache keys are tenant-scoped to prevent cross-tenant pollution.
        let _ = self
            .cache
            .invalidate(&Self::user_count_cache_key("admin", Some(actor_tenant_id)))
            .await;
        let _ = self
            .cache
            .invalidate(&Self::user_count_cache_key("user", Some(actor_tenant_id)))
            .await;
        let _ = self
            .cache
            .invalidate(&Self::user_count_cache_key("system", None))
            .await;

        Ok(())
    }

    const USER_COUNT_TTL_SECS: u64 = 30; // 30 seconds for count cache

    /// Build a Redis cache key for the paginated user-count.
    ///
    /// - For the `"system"` role: the key is `user:count:system` (no tenant suffix)
    ///   because system sees ALL users across all tenants.
    /// - For all other roles (`"admin"`, `"user"`): the key is
    ///   `user:count:{role}:{tenant_id}` so count caches are tenant-scoped.
    ///
    /// The caller MUST pass `None` for `actor_tenant_id` when `actor_role` is
    /// `"system"` (the tenant is ignored anyway, but `None` communicates intent).
    fn user_count_cache_key(actor_role: &str, actor_tenant_id: Option<&str>) -> String {
        if actor_role == "system" {
            // system sees ALL users (no tenant filter)
            format!("user:count:{}", actor_role)
        } else {
            // admin only sees users within their own tenant
            debug_assert!(
                actor_tenant_id.is_some(),
                "non-system roles must provide a tenant_id for count cache key"
            );
            format!(
                "user:count:{}:{}",
                actor_role,
                actor_tenant_id.unwrap_or("")
            )
        }
    }

    /// List users with pagination
    pub async fn list_users(
        &self,
        params: PaginationParams,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<PaginatedResponse<UserResponse>, UserError> {
        // Sanitize pagination inputs: clamp zero to 1 and cap large values
        // to reasonable bounds to prevent overflow or excessive offsets.
        let page = params.page.clamp(1, 1_000_000);
        let per_page = params.per_page.clamp(1, 100);

        let mut query = UserEntity::find().order_by_desc(Column::CreatedAt);

        // Admin scope: only see user role users within the same tenant
        if actor_role == "admin" {
            query = query
                .filter(Column::Role.eq("user"))
                .filter(Column::TenantId.eq(actor_tenant_id));
        }

        let paginator = query.paginate(&*self.db, per_page);

        // Cache the total count (30s TTL) — the list data itself still reads
        // from the database (which benefits from read-replica routing).
        // COUNT(*) queries are the most expensive part of pagination, especially
        // under active role-scoped filtering, so caching just the count provides
        // ~80% of the pagination caching benefit with zero cache-fragmentation risk.
        // System role sees ALL users (no tenant filter), so pass None.
        // Admin/pass-through roles are tenant-scoped, pass Some(tenant_id).
        let count_cache_key = if actor_role == "system" {
            Self::user_count_cache_key(actor_role, None)
        } else {
            Self::user_count_cache_key(actor_role, Some(actor_tenant_id))
        };
        let count_ttl = std::time::Duration::from_secs(Self::USER_COUNT_TTL_SECS);

        let total = match self.cache.get::<u64>(&count_cache_key).await {
            Ok(Some(count)) => count,
            _ => {
                let count = paginator
                    .num_items()
                    .await
                    .context("Failed to count users")?;
                if let Err(e) = self.cache.set(&count_cache_key, &count, count_ttl).await {
                    tracing::warn!("Failed to cache user count: {:?}", e);
                }
                count
            }
        };
        let total_pages = total.div_ceil(per_page);

        let users = paginator
            .fetch_page(page - 1)
            .await
            .context("Failed to fetch users")?;

        Ok(PaginatedResponse {
            items: users.into_iter().map(UserResponse::from).collect(),
            total,
            page,
            per_page,
            total_pages,
        })
    }

    /// Set user balance (direct set, follows RBAC rules).
    ///
    /// Uses a transaction with `SELECT ... FOR UPDATE` to prevent concurrent
    /// modifications (TOCTOU protection), same as `adjust_balance`.
    ///
    /// - `system` role: can modify any user's balance
    /// - `admin` role: can only modify `user` role accounts' balance
    /// - `user` role: cannot modify any balance
    pub async fn set_balance(
        &self,
        target_id: i64,
        balance: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<UserResponse, UserError> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to start transaction")?;

        // Lock the row exclusively to prevent concurrent modifications
        let target = UserEntity::find_by_id(target_id)
            .lock_exclusive()
            .one(&txn)
            .await
            .context("Failed to query user")?
            .ok_or(UserError::NotFound)?;

        // RBAC checks
        check_balance_rbac(&target, actor_role, actor_tenant_id)?;

        // Reject negative balance
        if balance < 0 {
            return Err(UserError::NotAllowed(
                "Balance cannot be negative".to_string(),
            ));
        }

        let mut active_model: ActiveModel = target.into();
        active_model.balance = Set(balance);
        active_model.updated_at = Set(Utc::now());

        let result = active_model.update(&txn).await.map_err(|e| {
            UserError::Internal(anyhow::Error::from(e).context("Failed to set balance"))
        })?;

        txn.commit().await.context("Failed to commit transaction")?;

        tracing::info!(
            "Balance set for user {}: {} (by {})",
            target_id,
            balance,
            actor_role
        );

        // Invalidate cache so next get_user/get_me returns latest balance.
        if let Err(e) = self
            .cache
            .invalidate(&Self::user_cache_key(target_id))
            .await
        {
            tracing::warn!(
                "Failed to invalidate cache for balance change on user {}: {:?}",
                target_id,
                e
            );
        }

        Ok(UserResponse::from(result))
    }

    /// Adjust user balance by a delta (increase or decrease).
    ///
    /// Uses a transaction with `SELECT ... FOR UPDATE` to prevent concurrent
    /// modifications (TOCTOU protection). The balance column is atomically
    /// updated within the locked row to guarantee consistency.
    ///
    /// - Positive `amount` = increase, negative `amount` = decrease.
    /// - Final balance must be >= 0.
    /// - RBAC rules follow the same pattern as `set_balance`.
    pub async fn adjust_balance(
        &self,
        target_id: i64,
        amount: i64,
        actor_role: &str,
        actor_tenant_id: &str,
    ) -> Result<UserResponse, UserError> {
        let txn = self
            .db
            .begin()
            .await
            .context("Failed to start transaction")?;

        // Lock the row exclusively to prevent concurrent balance modifications
        let target = UserEntity::find_by_id(target_id)
            .lock_exclusive()
            .one(&txn)
            .await
            .context("Failed to query user")?
            .ok_or(UserError::NotFound)?;

        // RBAC checks
        check_balance_rbac(&target, actor_role, actor_tenant_id)?;

        // Atomic balance adjustment with overflow protection
        let new_balance = target
            .balance
            .checked_add(amount)
            .ok_or_else(|| UserError::NotAllowed("Balance overflow".to_string()))?;

        // Reject negative balance
        if new_balance < 0 {
            return Err(UserError::NotAllowed("Insufficient balance".to_string()));
        }

        let mut active_model: ActiveModel = target.into();
        active_model.balance = Set(new_balance);
        active_model.updated_at = Set(Utc::now());

        let result = active_model.update(&txn).await.map_err(|e| {
            UserError::Internal(anyhow::Error::from(e).context("Failed to adjust balance"))
        })?;

        txn.commit().await.context("Failed to commit transaction")?;

        tracing::info!(
            "Balance adjusted for user {}: {} (amount: {}, by {})",
            target_id,
            new_balance,
            amount,
            actor_role
        );

        // Invalidate cache so next get_user/get_me returns latest balance.
        if let Err(e) = self
            .cache
            .invalidate(&Self::user_cache_key(target_id))
            .await
        {
            tracing::warn!(
                "Failed to invalidate cache for balance change on user {}: {:?}",
                target_id,
                e
            );
        }

        Ok(UserResponse::from(result))
    }
}

#[cfg(test)]
mod tests {
    use crate::snowflake::SnowflakeId;

    use super::*;

    #[test]
    fn test_pagination_params_default() {
        let params = PaginationParams::default();
        assert_eq!(params.page, 1);
        assert_eq!(params.per_page, 10);
    }

    #[test]
    fn test_pagination_params_custom() {
        let params = PaginationParams {
            page: 2,
            per_page: 20,
        };
        assert_eq!(params.page, 2);
        assert_eq!(params.per_page, 20);
    }

    #[test]
    fn test_paginated_response_structure() {
        let response: PaginatedResponse<UserResponse> = PaginatedResponse {
            items: vec![],
            total: 100,
            page: 1,
            per_page: 10,
            total_pages: 10,
        };

        assert_eq!(response.total, 100);
        assert_eq!(response.page, 1);
        assert_eq!(response.per_page, 10);
        assert_eq!(response.total_pages, 10);
        assert_eq!(response.items.len(), 0);
    }

    #[test]
    fn test_pagination_params_debug() {
        let params = PaginationParams {
            page: 3,
            per_page: 15,
        };
        let debug_str = format!("{:?}", params);
        assert!(debug_str.contains("PaginationParams"));
        assert!(debug_str.contains("3"));
        assert!(debug_str.contains("15"));
    }

    #[test]
    fn test_paginated_response_debug() {
        let response: PaginatedResponse<String> = PaginatedResponse {
            items: vec!["item1".to_string()],
            total: 50,
            page: 2,
            per_page: 25,
            total_pages: 2,
        };
        let debug_str = format!("{:?}", response);
        assert!(debug_str.contains("PaginatedResponse"));
        assert!(debug_str.contains("50"));
    }

    #[test]
    fn test_paginated_response_with_items() {
        use crate::repositories::user::UserResponse;
        use chrono::Utc;

        let user = UserResponse {
            id: SnowflakeId::new(1001),
            email: "test@example.com".to_string(),
            name: "Test User".to_string(),
            role: "user".to_string(),
            email_verified: false,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_version: 1,
            balance: 0,
            wx_openid: None,
            tenant_id: "default".to_string(),
            tenant_name: None,
        };

        let response = PaginatedResponse {
            items: vec![user],
            total: 1,
            page: 1,
            per_page: 10,
            total_pages: 1,
        };

        assert_eq!(response.items.len(), 1);
        assert_eq!(response.items[0].email, "test@example.com");
    }

    #[test]
    fn test_paginated_response_generic_string() {
        let response: PaginatedResponse<String> = PaginatedResponse {
            items: vec!["a".to_string(), "b".to_string()],
            total: 2,
            page: 1,
            per_page: 10,
            total_pages: 1,
        };

        assert_eq!(response.items.len(), 2);
        assert_eq!(response.items[0], "a");
        assert_eq!(response.items[1], "b");
    }

    #[test]
    fn test_paginated_response_generic_integer() {
        let response: PaginatedResponse<i32> = PaginatedResponse {
            items: vec![1, 2, 3],
            total: 3,
            page: 1,
            per_page: 10,
            total_pages: 1,
        };

        assert_eq!(response.items.len(), 3);
        assert_eq!(response.total, 3);
    }

    #[test]
    fn test_pagination_boundary_values() {
        let params = PaginationParams {
            page: u64::MAX,
            per_page: u64::MAX,
        };
        assert_eq!(params.page, u64::MAX);
        assert_eq!(params.per_page, u64::MAX);
    }

    #[test]
    fn test_paginated_response_empty() {
        let response: PaginatedResponse<UserResponse> = PaginatedResponse {
            items: vec![],
            total: 0,
            page: 1,
            per_page: 10,
            total_pages: 0,
        };

        assert!(response.items.is_empty());
        assert_eq!(response.total, 0);
        assert_eq!(response.total_pages, 0);
    }

    #[test]
    fn test_paginated_response_calculation() {
        let response: PaginatedResponse<i32> = PaginatedResponse {
            items: vec![],
            total: 95,
            page: 10,
            per_page: 10,
            total_pages: 10,
        };

        // Verify pagination math: 95 items / 10 per_page = 10 pages
        assert_eq!(
            response.total_pages,
            response.total.div_ceil(response.per_page)
        );
    }

    #[test]
    fn test_check_balance_rbac_system_account() {
        let target = UserModel {
            id: 1,
            email: "admin@test.com".to_string(),
            password_hash: String::new(),
            name: "System Admin".to_string(),
            role: "system".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_version: 1,
            email_verified: true,
            verification_code_hash: None,
            verification_code_expires_at: None,
            verification_code_sent_at: None,
            verification_failed_attempts: 0,
            password_reset_token_hash: None,
            password_reset_expires_at: None,
            password_reset_sent_at: None,
            password_reset_failed_attempts: 0,
            balance: 0,
            wx_openid: None,
            tenant_id: "default".to_string(),
        };
        let result = check_balance_rbac(&target, "admin", "default");
        assert!(matches!(result, Err(UserError::NotFound)));
    }

    #[test]
    fn test_check_balance_rbac_admin_on_admin_account() {
        let target = UserModel {
            id: 2,
            email: "admin@test.com".to_string(),
            password_hash: String::new(),
            name: "Admin".to_string(),
            role: "admin".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_version: 1,
            email_verified: true,
            verification_code_hash: None,
            verification_code_expires_at: None,
            verification_code_sent_at: None,
            verification_failed_attempts: 0,
            password_reset_token_hash: None,
            password_reset_expires_at: None,
            password_reset_sent_at: None,
            password_reset_failed_attempts: 0,
            balance: 0,
            wx_openid: None,
            tenant_id: "default".to_string(),
        };
        let result = check_balance_rbac(&target, "admin", "default");
        assert!(matches!(result, Err(UserError::NotFound)));
    }

    #[test]
    fn test_check_balance_rbac_user_actor() {
        let target = UserModel {
            id: 3,
            email: "user@test.com".to_string(),
            password_hash: String::new(),
            name: "User".to_string(),
            role: "user".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_version: 1,
            email_verified: false,
            verification_code_hash: None,
            verification_code_expires_at: None,
            verification_code_sent_at: None,
            verification_failed_attempts: 0,
            password_reset_token_hash: None,
            password_reset_expires_at: None,
            password_reset_sent_at: None,
            password_reset_failed_attempts: 0,
            balance: 0,
            wx_openid: None,
            tenant_id: "default".to_string(),
        };
        let result = check_balance_rbac(&target, "user", "default");
        assert!(matches!(result, Err(UserError::NotAllowed(_))));
    }

    #[test]
    fn test_check_balance_rbac_system_actor_allowed() {
        let target = UserModel {
            id: 4,
            email: "some@test.com".to_string(),
            password_hash: String::new(),
            name: "Some User".to_string(),
            role: "user".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_version: 1,
            email_verified: false,
            verification_code_hash: None,
            verification_code_expires_at: None,
            verification_code_sent_at: None,
            verification_failed_attempts: 0,
            password_reset_token_hash: None,
            password_reset_expires_at: None,
            password_reset_sent_at: None,
            password_reset_failed_attempts: 0,
            balance: 0,
            wx_openid: None,
            tenant_id: "default".to_string(),
        };
        let result = check_balance_rbac(&target, "system", "default");
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_balance_rbac_admin_on_user_allowed() {
        let target = UserModel {
            id: 5,
            email: "regular@test.com".to_string(),
            password_hash: String::new(),
            name: "Regular User".to_string(),
            role: "user".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_version: 1,
            email_verified: false,
            verification_code_hash: None,
            verification_code_expires_at: None,
            verification_code_sent_at: None,
            verification_failed_attempts: 0,
            password_reset_token_hash: None,
            password_reset_expires_at: None,
            password_reset_sent_at: None,
            password_reset_failed_attempts: 0,
            balance: 0,
            wx_openid: None,
            tenant_id: "default".to_string(),
        };
        let result = check_balance_rbac(&target, "admin", "default");
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_balance_rbac_admin_cross_tenant_rejected() {
        // Admin from "tenant_a" attempts to modify a user in "tenant_b"
        // — must be rejected with NotFound (tenant isolation).
        let target = UserModel {
            id: 100,
            email: "cross_tenant_user@test.com".to_string(),
            password_hash: String::new(),
            name: "Cross Tenant User".to_string(),
            role: "user".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_version: 1,
            email_verified: false,
            verification_code_hash: None,
            verification_code_expires_at: None,
            verification_code_sent_at: None,
            verification_failed_attempts: 0,
            password_reset_token_hash: None,
            password_reset_expires_at: None,
            password_reset_sent_at: None,
            password_reset_failed_attempts: 0,
            balance: 0,
            wx_openid: None,
            tenant_id: "tenant_b".to_string(),
        };
        // Actor is admin in tenant_a, target is a user in tenant_b → NotFound
        let result = check_balance_rbac(&target, "admin", "tenant_a");
        assert!(matches!(result, Err(UserError::NotFound)));
    }

    #[test]
    fn test_check_balance_rbac_system_cross_tenant_allowed() {
        // System role can modify any user regardless of tenant.
        let target = UserModel {
            id: 101,
            email: "other_tenant_user@test.com".to_string(),
            password_hash: String::new(),
            name: "Other Tenant User".to_string(),
            role: "user".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_version: 1,
            email_verified: false,
            verification_code_hash: None,
            verification_code_expires_at: None,
            verification_code_sent_at: None,
            verification_failed_attempts: 0,
            password_reset_token_hash: None,
            password_reset_expires_at: None,
            password_reset_sent_at: None,
            password_reset_failed_attempts: 0,
            balance: 0,
            wx_openid: None,
            tenant_id: "remote_tenant".to_string(),
        };
        let result = check_balance_rbac(&target, "system", "some_other_tenant");
        assert!(result.is_ok());
    }

    #[test]
    fn test_check_balance_rbac_unknown_role_catch_all() {
        // Unrecognized roles (e.g., "editor", "viewer") must be rejected
        // by the defensive catch-all. This tests the final branch in
        // check_balance_rbac that no unrecognized role slips through.
        let target = UserModel {
            id: 200,
            email: "editor_user@test.com".to_string(),
            password_hash: String::new(),
            name: "Editor User".to_string(),
            role: "user".to_string(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
            token_version: 1,
            email_verified: false,
            verification_code_hash: None,
            verification_code_expires_at: None,
            verification_code_sent_at: None,
            verification_failed_attempts: 0,
            password_reset_token_hash: None,
            password_reset_expires_at: None,
            password_reset_sent_at: None,
            password_reset_failed_attempts: 0,
            balance: 0,
            wx_openid: None,
            tenant_id: "default".to_string(),
        };
        let result = check_balance_rbac(&target, "editor", "default");
        assert!(matches!(
            result,
            Err(UserError::NotAllowed(ref m)) if m.contains("editor")
        ));

        let result = check_balance_rbac(&target, "manager", "default");
        assert!(matches!(
            result,
            Err(UserError::NotAllowed(ref m)) if m.contains("manager")
        ));
    }
}

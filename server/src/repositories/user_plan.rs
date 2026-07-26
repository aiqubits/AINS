use crate::snowflake::SnowflakeId;
use sea_orm::entity::prelude::*;
use serde::Serialize;

/// Plan instance held by a user (snapshot-style).
///
/// `plan_id` has no foreign key on purpose: deleting a plan template must
/// not invalidate instances users already purchased or were granted.
/// An instance is "active" while `expires_at > NOW() AND remaining_calls > 0`;
/// expiry is evaluated at query time, no background job required.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "user_plans")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    pub user_id: i64,
    pub plan_id: Option<i64>,
    pub plan_name: String,
    pub total_calls: i64,
    pub remaining_calls: i64,
    pub expires_at: DateTimeUtc,
    pub source: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}

/// Instance source: purchased by the user or granted by an admin.
pub const SOURCE_PURCHASE: &str = "purchase";
pub const SOURCE_ADMIN_GRANT: &str = "admin_grant";

#[derive(Debug, Serialize)]
pub struct UserPlanResponse {
    pub id: SnowflakeId,
    pub user_id: SnowflakeId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<SnowflakeId>,
    pub plan_name: String,
    pub total_calls: i64,
    pub remaining_calls: i64,
    pub expires_at: DateTimeUtc,
    pub source: String,
    pub created_at: DateTimeUtc,
    /// Derived state: "active" | "expired" | "exhausted".
    pub status: String,
}

impl From<Model> for UserPlanResponse {
    fn from(v: Model) -> Self {
        // Derived at conversion time — "expired" takes precedence over
        // "exhausted" so a used-up plan past its expiry reads as expired.
        let status = if v.expires_at <= chrono::Utc::now() {
            "expired"
        } else if v.remaining_calls <= 0 {
            "exhausted"
        } else {
            "active"
        };
        Self {
            id: SnowflakeId::new(v.id),
            user_id: SnowflakeId::new(v.user_id),
            plan_id: v.plan_id.map(SnowflakeId::new),
            plan_name: v.plan_name,
            total_calls: v.total_calls,
            remaining_calls: v.remaining_calls,
            expires_at: v.expires_at,
            source: v.source,
            created_at: v.created_at,
            status: status.to_string(),
        }
    }
}

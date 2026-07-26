use crate::snowflake::SnowflakeId;
use sea_orm::entity::prelude::*;
use serde::Serialize;

/// Subscription plan entity (tenant-scoped, mirrors ai_gateway_channels
/// isolation). `price` shares the balance scale: 1 display unit = 10^10
/// stored units (see `services::user::BALANCE_SCALE`).
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "plans")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    pub tenant_id: String,
    pub name: String,
    pub description: String,
    pub price: i64,
    pub total_calls: i64,
    pub validity_days: i32,
    pub status: String,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}

#[derive(Debug, Serialize)]
pub struct PlanResponse {
    pub id: SnowflakeId,
    pub tenant_id: String,
    /// Tenant display name, resolved best-effort at the handler layer for
    /// admin-facing list endpoints. `None` when not enriched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_name: Option<String>,
    pub name: String,
    pub description: String,
    pub price: i64,
    pub total_calls: i64,
    pub validity_days: i32,
    pub status: String,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

impl From<Model> for PlanResponse {
    fn from(v: Model) -> Self {
        Self {
            id: SnowflakeId::new(v.id),
            tenant_id: v.tenant_id,
            tenant_name: None,
            name: v.name,
            description: v.description,
            price: v.price,
            total_calls: v.total_calls,
            validity_days: v.validity_days,
            status: v.status,
            created_at: v.created_at,
            updated_at: v.updated_at,
        }
    }
}

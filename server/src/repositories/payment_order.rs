use crate::snowflake::SnowflakeId;
use sea_orm::entity::prelude::*;
use serde::Serialize;

/// Payment order entity (audit trail of plan purchases and manual entries).
///
/// `tenant_id` and `user_email` are snapshots taken at order creation so
/// that admin tenant-isolation filtering and UI display remain stable even
/// if the user later moves tenant or changes email. `payment_method`
/// reserves 'wechat' / 'alipay' for future external payment integration.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "payment_orders")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    pub user_id: i64,
    pub tenant_id: String,
    pub user_email: String,
    pub plan_id: Option<i64>,
    pub plan_name: String,
    pub amount: i64,
    pub status: String,
    pub payment_method: String,
    pub external_txn_id: Option<String>,
    pub created_at: DateTimeUtc,
    pub paid_at: Option<DateTimeUtc>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}

/// Allowed order status values (must match the CHECK constraint in 001_init.sql).
pub const ORDER_STATUSES: &[&str] = &["paid", "pending", "refunded", "cancelled"];

/// Allowed payment method values (must match the CHECK constraint in 001_init.sql).
pub const PAYMENT_METHODS: &[&str] = &["balance", "wechat", "alipay"];

#[derive(Debug, Serialize)]
pub struct PaymentOrderResponse {
    pub id: SnowflakeId,
    pub user_id: SnowflakeId,
    pub tenant_id: String,
    /// Tenant display name, resolved best-effort at the handler layer for
    /// admin-facing list endpoints. `None` when not enriched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_name: Option<String>,
    pub user_email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan_id: Option<SnowflakeId>,
    pub plan_name: String,
    pub amount: i64,
    pub status: String,
    pub payment_method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_txn_id: Option<String>,
    pub created_at: DateTimeUtc,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paid_at: Option<DateTimeUtc>,
}

impl From<Model> for PaymentOrderResponse {
    fn from(v: Model) -> Self {
        Self {
            id: SnowflakeId::new(v.id),
            user_id: SnowflakeId::new(v.user_id),
            tenant_id: v.tenant_id,
            tenant_name: None,
            user_email: v.user_email,
            plan_id: v.plan_id.map(SnowflakeId::new),
            plan_name: v.plan_name,
            amount: v.amount,
            status: v.status,
            payment_method: v.payment_method,
            external_txn_id: v.external_txn_id,
            created_at: v.created_at,
            paid_at: v.paid_at,
        }
    }
}

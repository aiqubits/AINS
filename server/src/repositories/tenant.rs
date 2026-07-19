/// The ID of the default tenant created during bootstrap.
///
/// All users and channels created during initial setup (seed_system_admin,
/// legacy registration) belong to this tenant.  This value must match the
/// `INSERT INTO tenants (id, ...) VALUES ('default', ...)` in the migration
/// script (001_init.sql).
pub const DEFAULT_TENANT_ID: &str = "default";

use sea_orm::entity::prelude::*;
use serde::Serialize;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "tenants")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub name: String,
    pub status: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}

#[derive(Debug, Serialize)]
pub struct TenantResponse {
    pub id: String,
    pub name: String,
    pub status: String,
    pub created_at: DateTimeUtc,
    /// Number of users in this tenant.
    #[serde(default)]
    pub user_count: u64,
    /// Number of AI gateway channels in this tenant.
    #[serde(default)]
    pub channel_count: u64,
}
impl From<Model> for TenantResponse {
    fn from(value: Model) -> Self {
        Self {
            id: value.id,
            name: value.name,
            status: value.status,
            created_at: value.created_at,
            user_count: 0,
            channel_count: 0,
        }
    }
}

use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "token_usage")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: i64,
    pub user_id: i64,
    pub tenant_id: String,
    pub channel_id: Uuid,
    pub model: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub request_type: String,
    pub created_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}

/// Response model for token usage queries.
#[derive(Debug, Serialize, Deserialize)]
pub struct TokenUsageResponse {
    pub id: i64,
    pub user_id: i64,
    pub tenant_id: String,
    pub channel_id: Uuid,
    pub model: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub request_type: String,
    pub created_at: DateTimeUtc,
}

impl From<Model> for TokenUsageResponse {
    fn from(v: Model) -> Self {
        Self {
            id: v.id,
            user_id: v.user_id,
            tenant_id: v.tenant_id,
            channel_id: v.channel_id,
            model: v.model,
            prompt_tokens: v.prompt_tokens,
            completion_tokens: v.completion_tokens,
            total_tokens: v.total_tokens,
            request_type: v.request_type,
            created_at: v.created_at,
        }
    }
}

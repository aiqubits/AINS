use sea_orm::entity::prelude::*;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "ai_gateway_channels")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub tenant_id: String,
    pub name: String,
    pub protocol_type: String,
    pub models: Json,
    pub capabilities: Json,
    pub api_key_encrypted: String,
    pub base_url: String,
    pub is_active: bool,
    pub weight: i32,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}
impl ActiveModelBehavior for ActiveModel {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProtocolType {
    Openai,
    Anthropic,
}
impl ProtocolType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum ModelCapability {
    Chat,
    Vision,
    Stt,
    Tts,
    WebSearch,
    Embedding,
}
impl ModelCapability {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Vision => "vision",
            Self::Stt => "stt",
            Self::Tts => "tts",
            Self::WebSearch => "websearch",
            Self::Embedding => "embedding",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ChannelResponse {
    pub id: Uuid,
    pub tenant_id: String,
    /// Tenant display name, resolved best-effort at the handler layer for
    /// admin-facing list endpoints. `None` when not enriched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_name: Option<String>,
    pub name: String,
    pub protocol_type: String,
    pub models: Json,
    pub capabilities: Json,
    pub base_url: String,
    pub is_active: bool,
    pub weight: i32,
    pub created_at: DateTimeUtc,
    pub updated_at: DateTimeUtc,
}
impl From<Model> for ChannelResponse {
    fn from(v: Model) -> Self {
        Self {
            id: v.id,
            tenant_id: v.tenant_id,
            tenant_name: None,
            name: v.name,
            protocol_type: v.protocol_type,
            models: v.models,
            capabilities: v.capabilities,
            base_url: v.base_url,
            is_active: v.is_active,
            weight: v.weight,
            created_at: v.created_at,
            updated_at: v.updated_at,
        }
    }
}

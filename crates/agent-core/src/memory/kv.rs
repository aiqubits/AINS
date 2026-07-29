//! KV 存储统一抽象（短期记忆）。
//!
//! Native 端由 redb 实现（`kv_native.rs`），Web 端由浏览器 IndexedDB 实现
//! （`kv_web.rs`）；上层仅通过本 trait 使用，不感知后端。
//!
//! 两侧维护职责相同的表 / ObjectStore（见 AINS_PLAN 4.1 存储结构），
//! 值统一以 [`Envelope`]（TTL 元数据 + JSON 载荷）经 bincode 序列化落盘，
//! 便于两端复用同一套（反）序列化逻辑。

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::MemoryError;
use crate::marker::MaybeSendSync;

/// 短期记忆表：key → value（含 TTL）。
pub const TABLE_KV: &str = "kv";
/// 事实记忆表：id → MemoryEntry。
pub const TABLE_MEMORIES: &str = "memories";
/// 向量数据表：id → vector blob（Source Of Truth）。
pub const TABLE_EMBEDDINGS: &str = "embeddings";
/// 文档元数据表。
pub const TABLE_DOCUMENTS: &str = "documents";
/// HNSW 派生缓存表（仅 Native 使用；Web 端保留同名 ObjectStore 但恒为空）。
pub const TABLE_HNSW_CACHE: &str = "hnsw_cache";

/// 双端统一的全部逻辑表。
pub const ALL_TABLES: [&str; 5] = [
    TABLE_KV,
    TABLE_MEMORIES,
    TABLE_EMBEDDINGS,
    TABLE_DOCUMENTS,
    TABLE_HNSW_CACHE,
];

/// 落盘信封：TTL 元数据 + JSON 载荷（bincode 序列化，双端一致）。
///
/// `Value` 的 `Deserialize` 依赖 `deserialize_any`，bincode 不支持，
/// 因此载荷以 JSON 字符串形式内嵌。
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Envelope {
    /// 过期时刻（Unix 毫秒）；`None` 表示永不过期。
    pub expires_at_ms: Option<i64>,
    pub json: String,
}

impl Envelope {
    pub(crate) fn new(value: &Value, ttl: Option<Duration>, now_ms: i64) -> Self {
        Self {
            expires_at_ms: ttl.map(|d| {
                // 超大 TTL 饱和为 i64::MAX（等效永不过期），避免 as 截断回绕
                let millis = i64::try_from(d.as_millis()).unwrap_or(i64::MAX);
                now_ms.saturating_add(millis)
            }),
            json: value.to_string(),
        }
    }

    pub(crate) fn is_expired(&self, now_ms: i64) -> bool {
        self.expires_at_ms.is_some_and(|at| at <= now_ms)
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, MemoryError> {
        bincode::serialize(self).map_err(|e| MemoryError::Serialization(e.to_string()))
    }

    pub(crate) fn decode(bytes: &[u8]) -> Result<Self, MemoryError> {
        bincode::deserialize(bytes).map_err(|e| MemoryError::Serialization(e.to_string()))
    }

    pub(crate) fn value(&self) -> Result<Value, MemoryError> {
        serde_json::from_str(&self.json).map_err(|e| MemoryError::Serialization(e.to_string()))
    }
}

/// 当前 Unix 时间（毫秒），双平台实现。
pub fn now_ms() -> i64 {
    #[cfg(target_arch = "wasm32")]
    {
        js_sys::Date::now() as i64
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }
}

/// 计算前缀在 UTF-16 code unit 序（IndexedDB 键序）下的排他上界。
///
/// 返回最小的合法字符串 `s`，使得所有以 `prefix` 开头的键都 `< s`。
/// 上界可能覆盖少量非前缀键（如代理区跳跃），调用方需再做 `starts_with`
/// 过滤。全 `\u{FFFF}` 前缀无上界，返回 `None`（调用方退化为 lower bound）。
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn prefix_successor(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(&last) = chars.last() {
        chars.pop();
        let cp = last as u32;
        if cp == 0xFFFF {
            // 该位已是最大 code unit，进位到前一位
            continue;
        }
        let successor = if cp >= 0x10000 {
            // 增补平面字符：低代理 +1；低代理已满则整对替换为 U+E000
            if (cp - 0x10000) & 0x3FF < 0x3FF {
                char::from_u32(cp + 1)
            } else {
                Some('\u{E000}')
            }
        } else if cp == 0xD7FF {
            // +1 落入代理区，跳到代理区之后
            Some('\u{E000}')
        } else {
            char::from_u32(cp + 1)
        };
        let mut out: String = chars.into_iter().collect();
        out.push(successor?);
        return Some(out);
    }
    None
}

#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait KvStore: MaybeSendSync {
    async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError>;

    /// 写入键值；`ttl` 为过期时长，`None` 表示永不过期。
    async fn set(&self, key: &str, value: &Value, ttl: Option<Duration>)
    -> Result<(), MemoryError>;

    async fn delete(&self, key: &str) -> Result<(), MemoryError>;

    /// 列出指定前缀的全部 key（不含已过期条目）。
    async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError>;

    /// 清理全部已过期条目，返回清理条数（后台定期清理路径，见 2.3）。
    /// 默认实现为 no-op，供无 TTL 语义的实现（如测试桩）沿用。
    async fn sweep_expired(&self) -> Result<u64, MemoryError> {
        Ok(0)
    }

    /// 删除指定前缀的全部条目，返回删除条数（namespace 清空等批量回收路径）。
    ///
    /// 默认实现基于 `list_prefix` 逐键删除，覆盖不到已过期未清理的行
    /// （交由 sweep 回收）；持久化后端应覆写为单事务批量删除，并连同
    /// 过期与损坏行一并清除（clear 语义需彻底）。
    async fn delete_prefix(&self, prefix: &str) -> Result<u64, MemoryError> {
        let keys = self.list_prefix(prefix).await?;
        let mut removed = 0u64;
        for key in &keys {
            self.delete(key).await?;
            removed += 1;
        }
        Ok(removed)
    }
}

#[cfg(test)]
mod tests {
    use super::prefix_successor;

    /// UTF-16 code unit 序比较（IndexedDB 键序）。
    fn utf16_lt(a: &str, b: &str) -> bool {
        let a: Vec<u16> = a.encode_utf16().collect();
        let b: Vec<u16> = b.encode_utf16().collect();
        a < b
    }

    #[test]
    fn successor_ascii() {
        assert_eq!(prefix_successor("a/").as_deref(), Some("a0"));
        assert_eq!(prefix_successor("memdir/").as_deref(), Some("memdir0"));
    }

    #[test]
    fn successor_carries_over_max_unit() {
        assert_eq!(prefix_successor("a\u{FFFF}").as_deref(), Some("b"));
        assert_eq!(prefix_successor("\u{FFFF}\u{FFFF}"), None);
        assert_eq!(prefix_successor(""), None);
    }

    #[test]
    fn successor_skips_surrogate_gap() {
        assert_eq!(prefix_successor("a\u{D7FF}").as_deref(), Some("a\u{E000}"));
    }

    #[test]
    fn successor_handles_astral_chars() {
        assert_eq!(
            prefix_successor("a\u{10000}").as_deref(),
            Some("a\u{10001}")
        );
        // 低代理已满（U+10FFFF = DBFF DFFF）→ 整对替换为 U+E000
        assert_eq!(
            prefix_successor("a\u{10FFFF}").as_deref(),
            Some("a\u{E000}")
        );
    }

    #[test]
    fn successor_bounds_prefixed_keys_in_utf16_order() {
        // 旧实现的反例：`{prefix}\u{10FFFF}` 在 UTF-16 序下小于含
        // [U+E000, U+FFFF] 后继字符的键，导致漏键。
        for prefix in ["a/", "a\u{D7FF}", "a\u{10FFFF}", "k\u{FFFF}"] {
            let bound = prefix_successor(prefix).unwrap();
            for suffix in ["", "x", "\u{E000}", "\u{FFFF}", "\u{10FFFF}"] {
                let key = format!("{prefix}{suffix}");
                assert!(
                    utf16_lt(&key, &bound),
                    "key {key:?} must be < bound {bound:?} for prefix {prefix:?}"
                );
            }
            assert!(utf16_lt(prefix, &bound));
        }
    }
}

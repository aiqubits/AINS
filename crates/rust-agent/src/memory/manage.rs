//! 记忆管理策略（AINS_PLAN 4.2 记忆管理策略 / Phase 2.7）。
//!
//! - **重要性评分**：`metadata.importance`（f64，默认 1.0），低分优先淘汰；
//! - **时间衰减**：半衰期指数衰减，旧记忆降低检索权重与保留权重；
//! - **去重合并**：内容归一化签名（sha256），相同签名的写入合并为刷新；
//! - 签名归一化口径对齐 OpenHarness `memory/manager.py`：小写、空白折叠、
//!   去 ASCII 标点。

use serde_json::Value;
use sha2::{Digest, Sha256};

/// 时间衰减默认半衰期（天）。
pub const DEFAULT_HALF_LIFE_DAYS: f64 = 30.0;
/// 缺省重要性评分。
pub const DEFAULT_IMPORTANCE: f64 = 1.0;

/// 签名归一化：小写、空白折叠为单空格、去 ASCII 标点。
pub fn normalize_for_signature(text: &str) -> String {
    let lowered = text.to_lowercase();
    let cleaned: String = lowered
        .chars()
        .filter(|c| !c.is_ascii_punctuation())
        .collect();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// 内容签名：`sha256("{normalized}|{namespace}")` 的 hex。
pub fn content_signature(content: &str, namespace: &str) -> String {
    let normalized = normalize_for_signature(content);
    let mut hasher = Sha256::new();
    hasher.update(normalized.as_bytes());
    hasher.update(b"|");
    hasher.update(namespace.as_bytes());
    let digest = hasher.finalize();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// 从 metadata 读取重要性评分（缺失/非法回落到默认值，最小 0）。
pub fn importance_of(metadata: &Value) -> f64 {
    metadata
        .get("importance")
        .and_then(Value::as_f64)
        .unwrap_or(DEFAULT_IMPORTANCE)
        .max(0.0)
}

/// 指数时间衰减因子：`0.5 ^ (age_days / half_life_days)`，取值 (0, 1]。
pub fn time_decay(created_at_ms: i64, now_ms: i64, half_life_days: f64) -> f64 {
    if half_life_days <= 0.0 {
        return 1.0;
    }
    let age_ms = (now_ms - created_at_ms).max(0) as f64;
    let age_days = age_ms / (24.0 * 3600.0 * 1000.0);
    0.5f64.powf(age_days / half_life_days)
}

/// 有效新鲜度时间戳：去重刷新会写入 `metadata.refreshed_at`，
/// 存在时以其为准，否则回落 `created_at`。
pub fn effective_recency_ms(metadata: &Value, created_at_ms: i64) -> i64 {
    metadata
        .get("refreshed_at")
        .and_then(Value::as_i64)
        .unwrap_or(created_at_ms)
}

/// 保留权重 = 重要性 × 时间衰减；容量淘汰时按此权重从低到高淘汰。
/// 时间衰减基于有效新鲜度（刷新过的记忆不按创建时间衰减）。
pub fn retention_score(metadata: &Value, created_at_ms: i64, now_ms: i64) -> f64 {
    let recency = effective_recency_ms(metadata, created_at_ms);
    importance_of(metadata) * time_decay(recency, now_ms, DEFAULT_HALF_LIFE_DAYS)
}

/// 检索重排：相似度分数按时间衰减降低排名（越旧排名越低）。
/// 分数口径“越大越相近”且可为负（Euclidean 恒为 -distance，cosine 有
/// 反向区间）：负分乘衰减会抬向 0 反超新记忆，必须除以衰减使其更负。
pub fn decayed_search_score(
    similarity: f32,
    created_at_ms: i64,
    now_ms: i64,
    half_life_days: f64,
) -> f32 {
    let decay = time_decay(created_at_ms, now_ms, half_life_days) as f32;
    if similarity >= 0.0 {
        similarity * decay
    } else if decay > 0.0 {
        similarity / decay
    } else {
        // 极端年龄下 decay 下溢为 0：负分直接压到最低，避免除零
        f32::MIN
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_HALF_LIFE_DAYS, content_signature, decayed_search_score, importance_of,
        normalize_for_signature, time_decay,
    };
    use serde_json::json;

    const DAY_MS: i64 = 24 * 3600 * 1000;

    #[test]
    fn normalization_and_signature() {
        assert_eq!(
            normalize_for_signature("  Hello,\n\tWORLD!!  "),
            "hello world"
        );
        // 同归一化同 namespace 同签名；不同 namespace 不同签名
        assert_eq!(
            content_signature("Hello, WORLD", "personal"),
            content_signature("hello world!", "personal")
        );
        assert_ne!(
            content_signature("hello world", "personal"),
            content_signature("hello world", "document")
        );
    }

    #[test]
    fn importance_clamps_and_defaults() {
        assert_eq!(importance_of(&json!({})), 1.0);
        assert_eq!(importance_of(&json!({"importance": -3.0})), 0.0);
        assert_eq!(importance_of(&json!({"importance": "bad"})), 1.0);
    }

    #[test]
    fn time_decay_known_values() {
        let now = 1_753_600_000_000i64;
        // 一个半衰期（30 天）→ 0.5
        let one = time_decay(now - 30 * DAY_MS, now, DEFAULT_HALF_LIFE_DAYS);
        assert!((one - 0.5).abs() < 1e-9);
        // 未来时间戳 clamp 到 1.0；非法半衰期回落 1.0
        assert_eq!(time_decay(now + DAY_MS, now, DEFAULT_HALF_LIFE_DAYS), 1.0);
        assert_eq!(time_decay(now - 30 * DAY_MS, now, 0.0), 1.0);
    }

    /// 衰减必须单调降低排名：同分数下旧记忆永远不高于新记忆。
    /// 旧行为：负分（Euclidean 恒负）乘衰减被抬向 0，排名反超新记忆。
    #[test]
    fn decay_lowers_rank_for_both_score_signs() {
        let now = 1_753_600_000_000i64;
        let old = now - 60 * DAY_MS;

        // 正分（cosine 同向）：旧 < 新
        let fresh_pos = decayed_search_score(0.9, now, now, DEFAULT_HALF_LIFE_DAYS);
        let old_pos = decayed_search_score(0.9, old, now, DEFAULT_HALF_LIFE_DAYS);
        assert!(old_pos < fresh_pos);

        // 负分（Euclidean -distance）：旧必须更负
        let fresh_neg = decayed_search_score(-2.0, now, now, DEFAULT_HALF_LIFE_DAYS);
        let old_neg = decayed_search_score(-2.0, old, now, DEFAULT_HALF_LIFE_DAYS);
        assert!(
            old_neg < fresh_neg,
            "old {old_neg} must rank below fresh {fresh_neg}"
        );
        // 60 天 = 两个半衰期：-2.0 / 0.25 = -8.0
        assert!((old_neg + 8.0).abs() < 1e-4);

        // 极端年龄（decay 下溢为 0）：不 panic、不 NaN，负分压到最低
        let ancient = decayed_search_score(-0.5, 0, now, 1e-6);
        assert!(ancient.is_finite() || ancient == f32::MIN);
        assert!(ancient <= fresh_neg);
        assert!(!decayed_search_score(0.0, 0, now, 1e-6).is_nan());
    }
}

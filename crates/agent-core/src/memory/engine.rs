//! Memory Engine：KV / Vector 两层之上的统一读写编排（AINS_PLAN 4.2 数据流）。
//!
//! 写入：dedupe 签名 →（容量满则按保留权重淘汰）→ `memories` + `embeddings`
//! （Source Of Truth，先落盘）→ 对应 namespace 索引 `add`（Web write-through
//! 语义天然成立）。
//! 查询：`vector.search(namespace, ...)`（首次命中懒加载重建）→ `memories.get(id)` 回填内容。

use std::sync::Arc;

use serde_json::Value;

use crate::error::MemoryError;
use crate::memory::kv::{KvStore, now_ms};
use crate::memory::manage::{
    DEFAULT_HALF_LIFE_DAYS, content_signature, decayed_search_score, effective_recency_ms,
    importance_of, retention_score,
};
use crate::memory::vector::{
    MemoryEntry, MemoryNamespace, VectorIndexManager, vector_from_value, vector_max_entries,
    vector_to_value,
};

fn sig_key(namespace: MemoryNamespace, signature: &str) -> String {
    format!("sig/{}/{signature}", namespace.as_str())
}

/// `search_ranked` 相似度过采样倍数（与 DocumentStore doc_ids 过滤检索同口径）。
const RANKED_OVERFETCH_FACTOR: usize = 4;

fn sig_prefix(namespace: MemoryNamespace) -> String {
    format!("sig/{}/", namespace.as_str())
}

/// 写入前校验：向量分量必须全部有限（NaN/Inf 会破坏距离计算与序列化）。
fn ensure_finite(vector: &[f32]) -> Result<(), MemoryError> {
    if vector.iter().any(|x| !x.is_finite()) {
        return Err(MemoryError::Storage(
            "vector contains non-finite component (NaN or Inf)".into(),
        ));
    }
    Ok(())
}

/// 被淘汰条目的落盘快照（新条目写入失败时用于 best-effort 恢复）。
struct EvictedSnapshot {
    namespace: MemoryNamespace,
    id: String,
    entry_raw: Option<Value>,
    embedding_raw: Option<Value>,
}

/// 统一记忆引擎；`vector` 类型即 `Box<dyn VectorIndexManager>`（见第三章）。
pub struct MemoryEngine {
    memories: Arc<dyn KvStore>,
    embeddings: Arc<dyn KvStore>,
    pub vector: Box<dyn VectorIndexManager>,
    max_entries: usize,
}

impl MemoryEngine {
    pub fn new(
        memories: Arc<dyn KvStore>,
        embeddings: Arc<dyn KvStore>,
        vector: Box<dyn VectorIndexManager>,
    ) -> Self {
        Self {
            memories,
            embeddings,
            vector,
            max_entries: vector_max_entries(),
        }
    }

    /// 自定义容量上限（测试与嵌入式小容量场景）。
    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries.max(1);
        self
    }

    /// 写入一条长期记忆。相同归一化签名的写入合并为刷新（重要性取 max），
    /// 容量满时按保留权重（重要性 × 时间衰减）淘汰最低分条目。
    pub async fn remember(
        &mut self,
        namespace: MemoryNamespace,
        content: &str,
        vector: &[f32],
        metadata: Value,
    ) -> Result<MemoryEntry, MemoryError> {
        ensure_finite(vector)?;
        let signature = content_signature(content, namespace.as_str());
        let skey = sig_key(namespace, &signature);

        // 去重合并：相同签名 → 刷新既有条目。签名行或目标行损坏时
        // 视为未命中，回落新建路径（损坏行交由容量淘汰优先回收），
        // 避免单行损坏永久阻断同内容写入。
        let dedupe_target = match self.memories.get(&skey).await {
            Ok(Some(Value::String(existing_id))) => {
                let entry_key = namespace.storage_key(&existing_id);
                match self.memories.get(&entry_key).await {
                    Ok(Some(raw)) => match serde_json::from_value::<MemoryEntry>(raw.clone()) {
                        Ok(entry) => Some((existing_id, entry_key, raw, entry)),
                        Err(e) => {
                            tracing::warn!(key = entry_key, error = %e, "dedupe target undecodable; creating new entry");
                            None
                        }
                    },
                    Ok(None) => None,
                    Err(MemoryError::Serialization(e)) => {
                        tracing::warn!(key = entry_key, error = %e, "dedupe target corrupt; creating new entry");
                        None
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(_) => None,
            Err(MemoryError::Serialization(e)) => {
                tracing::warn!(key = skey, error = %e, "signature row corrupt; creating new entry");
                None
            }
            Err(e) => return Err(e),
        };
        if let Some((existing_id, entry_key, raw, mut entry)) = dedupe_target {
            let merged = importance_of(&entry.metadata).max(importance_of(&metadata));
            // 基线刷新语义：正文以新写入为准。签名归一化仅折叠大小写/标点/
            // 空白，保留旧文本会导致内容与新向量分叉。
            entry.content = content.to_string();
            // metadata 同样以新写入为准（对齐 memdir 刷新：标签/来源等随新
            // 写入替换），importance 取 max；refreshed_at 由引擎维护，时间
            // 衰减/淘汰按此计算新鲜度（M1）。非对象 metadata 与新建路径
            // 同口径：原样保留（无处挂字段，衰减锚点回落 created_at）。
            entry.metadata = match metadata {
                Value::Object(mut map) => {
                    map.insert("importance".into(), merged.into());
                    map.insert("refreshed_at".into(), now_ms().into());
                    Value::Object(map)
                }
                Value::Null => serde_json::json!({
                    "importance": merged,
                    "refreshed_at": now_ms(),
                }),
                other => other,
            };
            // 旧向量留作回滚快照：索引 add 失败时恢复 SoT，避免两层分叉。
            // 快照行损坏时视为无旧向量（刷新会覆写修复，回滚则删除该行），
            // 不阻断同签名内容的刷新写入。
            let previous_vector = match self.embeddings.get(&entry_key).await {
                Ok(v) => v,
                Err(MemoryError::Serialization(e)) => {
                    tracing::warn!(key = entry_key, error = %e, "previous embedding corrupt; refresh overwrites");
                    None
                }
                Err(e) => return Err(e),
            };
            self.memories
                .set(
                    &entry_key,
                    &serde_json::to_value(&entry)
                        .map_err(|e| MemoryError::Serialization(e.to_string()))?,
                    None,
                )
                .await?;
            // 刷新向量：新写入的 embedding 同步替换 SoT 与索引节点，
            // 避免检索继续命中陈旧向量。落盘失败则恢复刷新前的条目，
            // 不留下“元数据已刷新但向量/索引未刷新”的分叉。
            if let Err(e) = self
                .embeddings
                .set(&entry_key, &vector_to_value(vector), None)
                .await
            {
                let _ = self.memories.set(&entry_key, &raw, None).await;
                return Err(e);
            }
            if let Err(e) = self.index_add(namespace, &existing_id, vector).await {
                // Best-effort rollback: restore both SoT *and* the resident
                // index.  A backend may report an error after mutating its
                // in-memory node; restoring only KV would make searches use
                // the failed refresh until the next process restart.
                let _ = self.memories.set(&entry_key, &raw, None).await;
                match &previous_vector {
                    Some(prev) => {
                        let _ = self.embeddings.set(&entry_key, prev, None).await;
                        match vector_from_value(prev) {
                            Ok(previous) => {
                                if let Err(restore_error) =
                                    self.index_add(namespace, &existing_id, &previous).await
                                {
                                    tracing::warn!(key = entry_key, error = %restore_error, "failed to restore refreshed vector index entry");
                                }
                            }
                            Err(restore_error) => {
                                tracing::warn!(key = entry_key, error = %restore_error, "previous embedding is undecodable; refreshed index entry not restored");
                            }
                        }
                    }
                    None => {
                        let _ = self.embeddings.delete(&entry_key).await;
                        // The previous persisted embedding was absent/corrupt.
                        // Do not leave a possible partially-added new node in
                        // the resident index after rolling its SoT row back.
                        let _ = self.vector.remove(namespace, &existing_id).await;
                    }
                }
                return Err(e);
            }
            return Ok(entry);
        }

        let now = now_ms();
        let id = format!("mem-{now}-{}", &signature[..12]);
        let metadata = match metadata {
            Value::Object(mut map) => {
                map.entry("importance").or_insert_with(|| 1.0.into());
                Value::Object(map)
            }
            Value::Null => serde_json::json!({ "importance": 1.0 }),
            other => other,
        };
        let entry = MemoryEntry {
            id: id.clone(),
            content: content.to_string(),
            namespace,
            metadata,
            created_at: now,
        };
        let entry_value =
            serde_json::to_value(&entry).map_err(|e| MemoryError::Serialization(e.to_string()))?;

        // 容量淘汰：满则淘汰保留权重最低的一条。淘汰先于写入（生产默认下
        // 索引容量与 max_entries 相等，先写后淘汰会被索引容量检查拒绝）；
        // 后续任一写入失败时 best-effort 恢复被淘汰条目，避免“淘汰一条、
        // 没写进一条”的净丢失。
        let evicted = if self.count(namespace).await? >= self.max_entries {
            self.evict_lowest(namespace).await?
        } else {
            None
        };

        // Source Of Truth 先落盘，再更新索引；三步写入（memories 条目 →
        // embeddings → 签名）与索引 add 任一失败都回滚已写入的行，避免孤儿行
        // 永久占用容量、或缺签名导致后续同内容写入无法去重而重复建条目。
        let entry_key = namespace.storage_key(&id);
        if let Err(e) = self.memories.set(&entry_key, &entry_value, None).await {
            self.restore_evicted(evicted.as_ref()).await;
            return Err(e);
        }
        // 向量落盘失败：撤销已写入的 memories 条目行，不留孤儿占用容量。
        if let Err(e) = self
            .embeddings
            .set(&entry_key, &vector_to_value(vector), None)
            .await
        {
            let _ = self.memories.delete(&entry_key).await;
            self.restore_evicted(evicted.as_ref()).await;
            return Err(e);
        }
        // 签名落盘失败：撤销 memories + embeddings，避免留下“有内容无签名”的
        // 条目——它会在重建后可检索却无法去重，导致后续同内容 remember 重复建条目。
        if let Err(e) = self
            .memories
            .set(&skey, &Value::String(id.clone()), None)
            .await
        {
            let _ = self.embeddings.delete(&entry_key).await;
            let _ = self.memories.delete(&entry_key).await;
            self.restore_evicted(evicted.as_ref()).await;
            return Err(e);
        }
        if let Err(e) = self.index_add(namespace, &id, vector).await {
            // An index backend can fail after adding its resident node.  The
            // persisted rows below are rolled back, so remove that possible
            // in-memory orphan as well.
            let _ = self.vector.remove(namespace, &id).await;
            let _ = self.memories.delete(&skey).await;
            let _ = self.embeddings.delete(&entry_key).await;
            let _ = self.memories.delete(&entry_key).await;
            self.restore_evicted(evicted.as_ref()).await;
            return Err(e);
        }
        Ok(entry)
    }

    /// 向索引写入节点（获取索引失败与 add 失败同样交给调用方回滚）。
    async fn index_add(
        &mut self,
        namespace: MemoryNamespace,
        id: &str,
        vector: &[f32],
    ) -> Result<(), MemoryError> {
        self.vector.add(namespace, id, vector).await
    }

    /// 以调用方指定的 id 直接写入（供文档 chunk 等确定性 id 场景使用；
    /// 不做签名去重合并）。新增条目在容量已满时直接报错——文档 chunk 属
    /// 成组数据，静默淘汰个人记忆或其他 chunk 会破坏完整性，由调用方决策。
    /// 已有 id 的覆盖不增加容量，因此即使已满也允许更新。
    pub async fn insert_with_id(
        &mut self,
        namespace: MemoryNamespace,
        id: &str,
        content: &str,
        vector: &[f32],
        metadata: Value,
    ) -> Result<MemoryEntry, MemoryError> {
        ensure_finite(vector)?;
        let entry_key = namespace.storage_key(id);
        // 确定性 id 用于文档 chunk 等可重试写入。容量检查只限制“新增”，
        // 否则满容量时无法刷新既有 chunk，调用方只能先删除后写入并暴露
        // 中间不完整状态。
        let previous_entry = self.memories.get(&entry_key).await?;
        if previous_entry.is_none() && self.count(namespace).await? >= self.max_entries {
            return Err(MemoryError::Storage(format!(
                "namespace {} is full ({} entries)",
                namespace.as_str(),
                self.max_entries
            )));
        }
        let entry = MemoryEntry {
            id: id.to_string(),
            content: content.to_string(),
            namespace,
            metadata,
            created_at: now_ms(),
        };
        let entry_value =
            serde_json::to_value(&entry).map_err(|e| MemoryError::Serialization(e.to_string()))?;
        let previous_vector = self.embeddings.get(&entry_key).await?;
        self.memories.set(&entry_key, &entry_value, None).await?;
        // 向量落盘失败：恢复旧条目；新增时撤销新行，不留下孤儿占用容量。
        if let Err(e) = self
            .embeddings
            .set(&entry_key, &vector_to_value(vector), None)
            .await
        {
            match &previous_entry {
                Some(raw) => {
                    let _ = self.memories.set(&entry_key, raw, None).await;
                }
                None => {
                    let _ = self.memories.delete(&entry_key).await;
                }
            }
            return Err(e);
        }
        if let Err(e) = self.index_add(namespace, id, vector).await {
            // 索引 add 失败时恢复 SoT。若是覆盖，再 best-effort 恢复内存索引
            // 的旧向量；恢复失败不会掩盖原始错误，重启后的 SoT 重建仍正确。
            match &previous_entry {
                Some(raw) => {
                    let _ = self.memories.set(&entry_key, raw, None).await;
                }
                None => {
                    let _ = self.memories.delete(&entry_key).await;
                }
            }
            match &previous_vector {
                Some(raw) => {
                    let _ = self.embeddings.set(&entry_key, raw, None).await;
                    match vector_from_value(raw) {
                        Ok(previous) => {
                            if let Err(restore_error) =
                                self.index_add(namespace, id, &previous).await
                            {
                                tracing::warn!(key = entry_key, error = %restore_error, "failed to restore overwritten vector index entry");
                            }
                        }
                        Err(restore_error) => {
                            tracing::warn!(key = entry_key, error = %restore_error, "previous embedding is undecodable; index entry not restored");
                        }
                    }
                }
                None => {
                    let _ = self.embeddings.delete(&entry_key).await;
                    // 新增失败时，已物化索引可能已有节点；尽力移除它。
                    let _ = self.vector.remove(namespace, id).await;
                }
            }
            return Err(e);
        }
        Ok(entry)
    }

    /// 相似度检索：索引返回 `(id, score)`，内容从 `memories` 回填。
    /// 单行损坏（Envelope/JSON 无法解码）跳过不毒化整次检索；
    /// 其余存储错误照常上抛。
    pub async fn search(
        &self,
        namespace: MemoryNamespace,
        query: &[f32],
        top_k: usize,
    ) -> Result<Vec<(MemoryEntry, f32)>, MemoryError> {
        // 与写入路径同口径：查询向量分量必须全部有限（NaN/Inf 会让 i8 量化
        // 静默映射 0 / L2 得 -Inf 分数，检索结果失真；review 修复）。
        ensure_finite(query)?;
        let hits = self.vector.search(namespace, query, top_k).await?;
        let mut results = Vec::with_capacity(hits.len());
        for (id, score) in hits {
            let key = namespace.storage_key(&id);
            let raw = match self.memories.get(&key).await {
                Ok(Some(raw)) => raw,
                Ok(None) => continue,
                Err(MemoryError::Serialization(e)) => {
                    tracing::warn!(key, error = %e, "skipping corrupt memory row in search");
                    continue;
                }
                Err(e) => return Err(e),
            };
            match serde_json::from_value::<MemoryEntry>(raw) {
                Ok(entry) => results.push((entry, score)),
                Err(e) => {
                    tracing::warn!(key, error = %e, "skipping undecodable memory row in search");
                }
            }
        }
        Ok(results)
    }

    /// 带时间衰减重排的检索（旧记忆降低检索权重）。
    pub async fn search_ranked(
        &self,
        namespace: MemoryNamespace,
        query: &[f32],
        top_k: usize,
    ) -> Result<Vec<(MemoryEntry, f32)>, MemoryError> {
        let now = now_ms();
        // 相似度过采样后再衰减重排：只取 top_k 时，落在原始窗口外的
        // 新鲜条目永远无法反超窗口内的陈旧条目（衰减只降不升）。
        let fetch_k = top_k.saturating_mul(RANKED_OVERFETCH_FACTOR);
        let mut results = self.search(namespace, query, fetch_k).await?;
        for (entry, score) in &mut results {
            let recency = effective_recency_ms(&entry.metadata, entry.created_at);
            *score = decayed_search_score(*score, recency, now, DEFAULT_HALF_LIFE_DAYS);
        }
        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(top_k);
        Ok(results)
    }

    pub async fn get(
        &self,
        namespace: MemoryNamespace,
        id: &str,
    ) -> Result<Option<MemoryEntry>, MemoryError> {
        match self.memories.get(&namespace.storage_key(id)).await? {
            Some(raw) => Ok(Some(
                serde_json::from_value(raw)
                    .map_err(|e| MemoryError::Serialization(e.to_string()))?,
            )),
            None => Ok(None),
        }
    }

    /// 删除一条记忆：签名、内容、向量、索引节点全部清理。
    /// 内容行损坏（无法解码）时跳过签名清理，仍删除裸键，
    /// 保证损坏行不会永久占用容量（`count` 按前缀统计）。
    pub async fn forget(
        &mut self,
        namespace: MemoryNamespace,
        id: &str,
    ) -> Result<(), MemoryError> {
        // 损坏行（Envelope/JSON 无法解码）跳过签名清理，仍删除裸键；
        // 其余错误（存储 I/O 等）照常上抛。
        let raw = match self.memories.get(&namespace.storage_key(id)).await {
            Ok(raw) => raw,
            Err(MemoryError::Serialization(_)) => None,
            Err(e) => return Err(e),
        };
        if let Some(raw) = raw
            && let Ok(entry) = serde_json::from_value::<MemoryEntry>(raw)
        {
            let signature = content_signature(&entry.content, namespace.as_str());
            let skey = sig_key(namespace, &signature);
            // 守卫：仅当签名仍指向本条时才删（同签名可能已被新条目接管）。
            if let Some(Value::String(mapped)) = self.memories.get(&skey).await?
                && mapped == id
            {
                self.memories.delete(&skey).await?;
            }
        }
        self.memories.delete(&namespace.storage_key(id)).await?;
        self.embeddings.delete(&namespace.storage_key(id)).await?;
        match self.vector.remove(namespace, id).await {
            Ok(()) | Err(MemoryError::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// 清空整个 namespace：memories 行、签名映射、embeddings 与索引实例
    /// 一并删除（保持 Source Of Truth 与派生索引一致）。幂等，可重复调用；
    /// 之后需 `vector.create_index` 重建索引才能继续写入。
    pub async fn clear_namespace(&mut self, namespace: MemoryNamespace) -> Result<(), MemoryError> {
        // 批量前缀删除（后端单事务）；连同过期与损坏行一并清除
        self.memories
            .delete_prefix(&namespace.storage_prefix())
            .await?;
        self.memories.delete_prefix(&sig_prefix(namespace)).await?;
        // remove_index 负责删除 embeddings 前缀与派生缓存
        self.vector.remove_index(namespace).await
    }

    /// 该 namespace 的记忆条数。
    pub async fn count(&self, namespace: MemoryNamespace) -> Result<usize, MemoryError> {
        Ok(self
            .memories
            .list_prefix(&namespace.storage_prefix())
            .await?
            .len())
    }

    /// 列出 namespace 内 id 以 `id_prefix` 开头的全部记忆 id
    /// （成组数据——如文档 chunk——的批量回收入口）。
    pub async fn list_ids(
        &self,
        namespace: MemoryNamespace,
        id_prefix: &str,
    ) -> Result<Vec<String>, MemoryError> {
        let prefix = namespace.storage_prefix();
        Ok(self
            .memories
            .list_prefix(&format!("{prefix}{id_prefix}"))
            .await?
            .into_iter()
            .filter_map(|key| key.strip_prefix(&prefix).map(|id| id.to_string()))
            .collect())
    }

    /// 淘汰保留权重（重要性 × 时间衰减）最低的一条记忆，返回其落盘快照
    /// 供写入失败时恢复。损坏行（Envelope/JSON 无法解码）无检索价值，
    /// 视为最低分优先淘汰，顺带回收其占用的容量；删除目标以 key 派生 id
    /// 为准，避免行内 id 与 key 不一致时删空。
    async fn evict_lowest(
        &mut self,
        namespace: MemoryNamespace,
    ) -> Result<Option<EvictedSnapshot>, MemoryError> {
        let now = now_ms();
        let prefix = namespace.storage_prefix();
        let mut lowest: Option<(String, f64)> = None;
        for key in self.memories.list_prefix(&prefix).await? {
            let Some(id) = key.strip_prefix(&prefix) else {
                continue;
            };
            let raw = match self.memories.get(&key).await {
                Ok(Some(raw)) => Some(raw),
                // 行已被并发删除：容量已释放，跳过
                Ok(None) => continue,
                Err(MemoryError::Serialization(_)) => None,
                Err(e) => return Err(e),
            };
            let score = match raw.and_then(|r| serde_json::from_value::<MemoryEntry>(r).ok()) {
                Some(entry) => retention_score(&entry.metadata, entry.created_at, now),
                None => f64::NEG_INFINITY,
            };
            if lowest.as_ref().is_none_or(|(_, s)| score < *s) {
                lowest = Some((id.to_string(), score));
            }
        }
        if let Some((id, _)) = lowest {
            // 快照被淘汰条目的落盘状态（损坏行快照为 None，恢复时跳过）
            let entry_key = namespace.storage_key(&id);
            let entry_raw = match self.memories.get(&entry_key).await {
                Ok(v) => v,
                Err(MemoryError::Serialization(_)) => None,
                Err(e) => return Err(e),
            };
            let embedding_raw = match self.embeddings.get(&entry_key).await {
                Ok(v) => v,
                Err(MemoryError::Serialization(_)) => None,
                Err(e) => return Err(e),
            };
            self.forget(namespace, &id).await?;
            return Ok(Some(EvictedSnapshot {
                namespace,
                id,
                entry_raw,
                embedding_raw,
            }));
        }
        Ok(None)
    }

    /// best-effort 恢复被淘汰的条目（新条目落盘失败后调用），避免“淘汰
    /// 一条、没写进一条”的净丢失；恢复失败仅记日志，不改变原始错误。
    async fn restore_evicted(&mut self, snapshot: Option<&EvictedSnapshot>) {
        let Some(snapshot) = snapshot else { return };
        let entry_key = snapshot.namespace.storage_key(&snapshot.id);
        // 条目行快照缺失（损坏行）：整体跳过——只恢复 embedding 会留下
        // 无 memories 行的孤儿向量，重启重建后变成无内容可回填的索引节点
        let Some(raw) = &snapshot.entry_raw else {
            return;
        };
        if let Err(e) = self.memories.set(&entry_key, raw, None).await {
            tracing::warn!(key = entry_key, error = %e, "failed to restore evicted memory row");
            return;
        }
        // 签名映射恢复：仅当行可解码且签名槽位空闲（避免覆盖接管者）
        if let Ok(entry) = serde_json::from_value::<MemoryEntry>(raw.clone()) {
            let skey = sig_key(
                snapshot.namespace,
                &content_signature(&entry.content, snapshot.namespace.as_str()),
            );
            if matches!(self.memories.get(&skey).await, Ok(None)) {
                let _ = self
                    .memories
                    .set(&skey, &Value::String(snapshot.id.clone()), None)
                    .await;
            }
        }
        let Some(vraw) = &snapshot.embedding_raw else {
            return;
        };
        // 向量/索引恢复失败同样需可观测：条目占容量但不可检索，直到重启重建
        if let Err(e) = self.embeddings.set(&entry_key, vraw, None).await {
            tracing::warn!(key = entry_key, error = %e, "failed to restore evicted embedding row");
            return;
        }
        match vector_from_value(vraw) {
            Ok(vector) => {
                if let Err(e) = self
                    .index_add(snapshot.namespace, &snapshot.id, &vector)
                    .await
                {
                    tracing::warn!(key = entry_key, error = %e, "failed to re-add evicted entry to index");
                }
            }
            Err(e) => {
                tracing::warn!(key = entry_key, error = %e, "evicted embedding snapshot undecodable; index node not restored");
            }
        }
    }
}

//! 子代理 / Swarm（AINS_PLAN 7+.3）：进程内 `TeammateExecutor` 后端 +
//! KV 信箱 IPC + 权限上收 lead + `AgentDefinition`。
//!
//! 对齐 OpenHarness `swarm/`（in_process 后端 + mailbox）+ `coordinator/`
//! （`AgentDefinition`）。AINS 首选**进程内**后端：子代理是同进程内以受限
//! `AgentDefinition`（工具白名单 + 模型覆盖 + 系统提示）运行的 agent，经
//! **KV 信箱**（而非文件系统）与 lead 及彼此收发消息，双 target 可用。
//!
//! 权限上收：子代理工具受其 `AgentDefinition` 白名单约束；越权工具不在子代理
//! 就地放行，而是 [`needs_escalation`] 判定后**上收 lead** 决策（子代理不得自行
//! 提权），对齐基线 `permission_sync`。
//!
//! **接线前提（review）**：`needs_escalation` 当前为纯函数、无调用方——
//! 子代理工具门控（`AgentDefinition::allows_tool`）的强制点与 lead 批准后的
//! 权限回填均由宿主 runner 负责，接线时必须：1) 在子代理工具循环内强制
//! `allows_tool`（未授权工具必须走 escalation 而非就地放行）；2) lead 批准
//! 后按原 `AgentDefinition` 重放工具调用（批准一次不等于永久提权）。
//! 另：与 `tasks/`（后台任务）共享进程级单例时，需先补齐任务归属校验
//! （见 `tasks` 模块文档）。

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::MemoryError;
use crate::marker::MaybeSendSync;
use crate::memory::kv::KvStore;
use crate::memory::now_ms;

/// 子代理定义（对齐 coordinator `AgentDefinition`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub name: String,
    /// 何时使用（whenToUse）。
    pub description: String,
    #[serde(default)]
    pub system_prompt: Option<String>,
    /// 工具白名单：含 `"*"` 表示全部允许；缺省值为空白名单。
    ///
    /// 最小权限语义不接受隐式的“全部工具”：缺失字段默认为空，JSON `null`
    /// 会被 serde 拒绝。需要全工具权限时，宿主必须显式写入 `vec!["*"]`。
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// 模型覆盖；`None` 继承默认。
    #[serde(default)]
    pub model: Option<String>,
    /// 首轮用户消息前置的初始提示。
    #[serde(default)]
    pub initial_prompt: Option<String>,
}

impl AgentDefinition {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            system_prompt: None,
            // Least privilege: the host must explicitly grant tools to a child.
            tools: Vec::new(),
            disallowed_tools: Vec::new(),
            model: None,
            initial_prompt: None,
        }
    }

    /// 该子代理是否被授予使用某工具（disallowed 优先；白名单 `*`=全部）。
    /// `disallowed_tools` 中的 `*` 同样按通配处理（=全禁）：若按精确
    /// 匹配对待，`tools:["*"]` + `disallowed_tools:["*"]` 会被解释为
    /// 全部放行（fail-open），与白名单 `*` 语义不对称。
    pub fn allows_tool(&self, tool: &str) -> bool {
        if self.disallowed_tools.iter().any(|t| t == "*" || t == tool) {
            return false;
        }
        self.tools.iter().any(|t| t == "*" || t == tool)
    }
}

/// 子代理定义注册表。
#[derive(Debug, Default)]
pub struct AgentRegistry {
    defs: HashMap<String, AgentDefinition>,
    order: Vec<String>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, def: AgentDefinition) {
        if !self.defs.contains_key(&def.name) {
            self.order.push(def.name.clone());
        }
        self.defs.insert(def.name.clone(), def);
    }

    pub fn get(&self, name: &str) -> Option<&AgentDefinition> {
        self.defs.get(name)
    }

    pub fn list(&self) -> Vec<&AgentDefinition> {
        self.order.iter().filter_map(|n| self.defs.get(n)).collect()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }
}

/// 子代理越权某工具时是否需上收 lead 决策（子代理不得自行放行越权工具）。
pub fn needs_escalation(def: &AgentDefinition, tool: &str) -> bool {
    !def.allows_tool(tool)
}

/// 向 lead 上收的权限请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionEscalation {
    /// 发起的子代理名。
    pub teammate: String,
    /// 请求使用的工具。
    pub tool: String,
    /// 上收理由（供 lead 判断）。
    pub reason: String,
}

// ── KV 信箱 IPC ──────────────────────────────────────────────

/// 信箱 key 前缀：`swarm/mbox/{scope}/{recipient}/{created_at:013}-{nonce}`。
/// `created_at` 零填充 13 位保证按时间字典序，`list_prefix` 天然有序。
const MAILBOX_PREFIX: &str = "swarm/mbox/";
/// 单条信箱消息体上限（文本消息；防无界消息膨胀 KV 与 inbox 读取面）。
const MAILBOX_MAX_BODY_BYTES: usize = 64 * 1024;

fn inbox_prefix(scope: &str, agent: &str) -> String {
    format!("{MAILBOX_PREFIX}{scope}/{agent}/")
}

/// 校验参与信箱 key 分层的 agent 名称。所有读取和写入入口都必须使用它：
/// 空名会退化为共享 `swarm/mbox/` 前缀，`/` 会改变收件箱层级。
fn validate_agent_name(agent: &str, field: &str) -> Result<(), MemoryError> {
    if agent.is_empty() || agent.contains('/') {
        return Err(MemoryError::Storage(format!(
            "mailbox {field} must be non-empty and contain no '/'"
        )));
    }
    Ok(())
}

/// 消息 ID 由 [`KvMailbox::post`] 生成且不含路径分隔符。拒绝调用方注入
/// 分隔符，避免未来 KV 后端对 key 做路径语义解释时扩大操作范围。
fn validate_message_id(message_id: &str) -> Result<(), MemoryError> {
    if message_id.is_empty() || message_id.contains('/') {
        return Err(MemoryError::Storage(
            "mailbox message id must be non-empty and contain no '/'".to_string(),
        ));
    }
    Ok(())
}

/// 以单调序号作为主键的一部分，随机位仅用于降低跨进程重用同一 scope 时的
/// 碰撞概率。不能只依赖随机数：同一毫秒内 32 位 nonce 碰撞会让后一次 `set`
/// 覆盖先前的信箱消息。
fn mailbox_suffix(sequence: u64, random: [u8; 4]) -> String {
    format!(
        "{sequence:016x}-{}",
        random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    )
}

fn random_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // 无论 RNG 是否可用都消耗序号；它是同一进程内唯一性的来源。
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut bytes = [0u8; 4];
    // RNG 失败不影响唯一性，仍保留序号部分；随机位退化为序号低位以保持
    // 输出格式稳定。
    if getrandom::getrandom(&mut bytes).is_err() {
        bytes = (sequence as u32).to_le_bytes();
    }
    mailbox_suffix(sequence, bytes)
}

/// 默认信箱 scope：同一 `KvMailbox` 实例内稳定，不同新实例独立。
/// 四段含单调序号的随机后缀在 RNG 不可用时仍保持进程内唯一。
fn new_mailbox_scope() -> String {
    format!(
        "session-{}-{}{}{}{}",
        now_ms(),
        random_suffix(),
        random_suffix(),
        random_suffix(),
        random_suffix(),
    )
}

/// 单条信箱消息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxMessage {
    pub id: String,
    pub sender: String,
    pub recipient: String,
    pub body: String,
    pub created_at: i64,
    #[serde(default)]
    pub read: bool,
}

/// KvStore 支撑的 swarm 信箱（双 target；per-scope / per-recipient inbox）。
pub struct KvMailbox {
    kv: Arc<dyn KvStore>,
    /// 同一逻辑 swarm（或会话）共享的稳定 scope；防止同一持久化 KV 内
    /// 不同 swarm 使用相同 agent 名称时混读信箱。
    scope: String,
}

impl KvMailbox {
    /// 创建一个拥有随机独立 scope 的信箱。需要由多个组件共享同一信箱时，
    /// 复用本实例，或改用 [`Self::with_scope`] 传入宿主保存的 swarm/session id。
    pub fn new(kv: Arc<dyn KvStore>) -> Self {
        Self {
            kv,
            scope: new_mailbox_scope(),
        }
    }

    /// 创建 / 重连指定逻辑 swarm 的信箱。`scope` 是存储 key 的一个路径段，
    /// 因此不能为空且不能包含 `/`。
    pub fn with_scope(kv: Arc<dyn KvStore>, scope: impl Into<String>) -> Result<Self, MemoryError> {
        let scope = scope.into();
        validate_agent_name(&scope, "scope")?;
        Ok(Self { kv, scope })
    }

    /// 当前信箱的逻辑 swarm/session scope。
    pub fn scope(&self) -> &str {
        &self.scope
    }

    fn message_key(&self, recipient: &str, id: &str) -> String {
        // id 自带零填充时间戳前缀（post 生成），key = 收件箱 + 完整 id，
        // 保证 `list_prefix` 按投递时间字典序。
        format!("{}{id}", inbox_prefix(&self.scope, recipient))
    }

    /// 投递一条消息到 `recipient` 的信箱，返回落盘的消息。
    /// 消息体有大小上限（防子代理/外部源写入无界消息膨胀 KV 与读取面）。
    pub async fn post(
        &self,
        sender: &str,
        recipient: &str,
        body: &str,
    ) -> Result<MailboxMessage, MemoryError> {
        validate_agent_name(sender, "sender")?;
        validate_agent_name(recipient, "recipient")?;
        if body.len() > MAILBOX_MAX_BODY_BYTES {
            return Err(MemoryError::Storage(format!(
                "mailbox message body exceeds {} bytes",
                MAILBOX_MAX_BODY_BYTES
            )));
        }
        let created_at = now_ms();
        // id 内嵌零填充时间戳和单调序号：`{ts:013}-{sequence}-{nonce}`，使
        // `list_prefix` 天然有序且同一毫秒内不会覆盖已投递的消息。
        let id = format!("{created_at:013}-{}", random_suffix());
        let message = MailboxMessage {
            id: id.clone(),
            sender: sender.to_string(),
            recipient: recipient.to_string(),
            body: body.to_string(),
            created_at,
            read: false,
        };
        let value = serde_json::to_value(&message)
            .map_err(|e| MemoryError::Serialization(e.to_string()))?;
        self.kv
            .set(&self.message_key(recipient, &id), &value, None)
            .await?;
        Ok(message)
    }

    /// `recipient` 的全部消息（按投递时间升序，损坏行跳过）。
    pub async fn inbox(&self, recipient: &str) -> Result<Vec<MailboxMessage>, MemoryError> {
        validate_agent_name(recipient, "recipient")?;
        let mut out = Vec::new();
        for key in self
            .kv
            .list_prefix(&inbox_prefix(&self.scope, recipient))
            .await?
        {
            let Some(value) = self.kv.get(&key).await? else {
                continue;
            };
            if let Ok(message) = serde_json::from_value::<MailboxMessage>(value) {
                out.push(message);
            }
        }
        // list_prefix 已按 key（含零填充时间戳）有序；显式再排序以防后端差异。
        out.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        Ok(out)
    }

    /// `recipient` 的未读消息。
    pub async fn unread(&self, recipient: &str) -> Result<Vec<MailboxMessage>, MemoryError> {
        Ok(self
            .inbox(recipient)
            .await?
            .into_iter()
            .filter(|m| !m.read)
            .collect())
    }

    /// 标记一条消息为已读（幂等；不存在返回 NotFound）。
    ///
    /// key 由收件箱前缀 + 完整消息 id 直接构造（id 内嵌时间戳），不再从
    /// id 反向解析时间戳——key 格式变化不再破坏本方法（review 修复：
    /// 历史实现对 id 做 `split_once('-')` 解析，格式脆弱）。
    /// NOTE：本方法使用 get→修改→set 模式，非原子操作。对同一消息的并发
    /// `mark_read` 是安全的（`read=true` 设置幂等），但对同一 key 的并发
    /// 异构修改（如同时 `mark_read` + 外部更新）会导致丢失更新。
    pub async fn mark_read(&self, recipient: &str, message_id: &str) -> Result<(), MemoryError> {
        validate_agent_name(recipient, "recipient")?;
        validate_message_id(message_id)?;
        let key = format!("{}{message_id}", inbox_prefix(&self.scope, recipient));
        let Some(value) = self.kv.get(&key).await? else {
            return Err(MemoryError::NotFound(message_id.to_string()));
        };
        let mut message: MailboxMessage =
            serde_json::from_value(value).map_err(|e| MemoryError::Serialization(e.to_string()))?;
        message.read = true;
        let value = serde_json::to_value(&message)
            .map_err(|e| MemoryError::Serialization(e.to_string()))?;
        self.kv.set(&key, &value, None).await
    }

    /// 清空 `recipient` 信箱（删除收件箱前缀下的全部消息；返回删除条数）。
    ///
    /// 消息持久化在 KV 中永不过期（`post` 无 TTL），长期运行的 swarm 会
    /// 无限累积——清理 API 供宿主在会话收尾 / 定期维护时回收（review 修复：
    /// 历史实现无任何删除路径）。
    pub async fn clear_recipient(&self, recipient: &str) -> Result<u64, MemoryError> {
        validate_agent_name(recipient, "recipient")?;
        self.kv
            .delete_prefix(&inbox_prefix(&self.scope, recipient))
            .await
    }

    /// 删除 `recipient` 信箱中的全部**已读**消息（保留未读；返回删除条数）。
    ///
    /// 读取已读标记需解析载荷：损坏行跳过（不误删）。
    pub async fn prune_read(&self, recipient: &str) -> Result<u64, MemoryError> {
        validate_agent_name(recipient, "recipient")?;
        let mut removed = 0;
        for key in self
            .kv
            .list_prefix(&inbox_prefix(&self.scope, recipient))
            .await?
        {
            if let Some(value) = self.kv.get(&key).await?
                && let Ok(message) = serde_json::from_value::<MailboxMessage>(value)
                && message.read
            {
                self.kv.delete(&key).await?;
                removed += 1;
            }
        }
        Ok(removed)
    }
}

// ── 进程内 TeammateExecutor 后端 ─────────────────────────────

/// 子代理任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateTask {
    /// 目标子代理名（对应 [`AgentDefinition::name`]）。
    pub agent: String,
    /// 派发给子代理的提示 / 任务。
    pub prompt: String,
}

/// 子代理执行结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeammateResult {
    pub agent: String,
    pub output: String,
    pub success: bool,
}

/// 子代理实际执行体（由上层提供：接入 Kernel/Agent Loop 或测试桩）。
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
pub trait TeammateRunner: MaybeSendSync {
    /// 以给定定义运行任务；`Ok(output)` 成功，`Err(reason)` 失败。
    async fn run(&self, def: &AgentDefinition, task: &TeammateTask) -> Result<String, String>;
}

/// 进程内子代理执行后端：按注册的 `AgentDefinition` 派发任务给 `runner`。
pub struct InProcessExecutor {
    registry: AgentRegistry,
    runner: Arc<dyn TeammateRunner>,
}

impl InProcessExecutor {
    pub fn new(registry: AgentRegistry, runner: Arc<dyn TeammateRunner>) -> Self {
        Self { registry, runner }
    }

    pub fn registry(&self) -> &AgentRegistry {
        &self.registry
    }

    /// 派发任务：查定义 → 交 runner 运行 → 归一化为 [`TeammateResult`]。
    /// 未注册的子代理返回 `NotFound`。
    pub async fn dispatch(&self, task: TeammateTask) -> Result<TeammateResult, MemoryError> {
        let def = self
            .registry
            .get(&task.agent)
            .ok_or_else(|| MemoryError::NotFound(task.agent.clone()))?;
        let result = match self.runner.run(def, &task).await {
            Ok(output) => TeammateResult {
                agent: task.agent,
                output,
                success: true,
            },
            Err(reason) => TeammateResult {
                agent: task.agent,
                output: reason,
                success: false,
            },
        };
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // 仅 native 测试使用（MockKvStore 与信箱投递测试依赖 tokio）。
    #[cfg(not(target_arch = "wasm32"))]
    use serde_json::Value;
    #[cfg(not(target_arch = "wasm32"))]
    use std::collections::HashMap;
    #[cfg(not(target_arch = "wasm32"))]
    use std::sync::Mutex;
    #[cfg(not(target_arch = "wasm32"))]
    use std::time::Duration;

    /// 轻量内存 KvStore mock（信箱测试用；真实 redb 集成见 memory_native.rs）。
    /// 仅 native 测试使用（信箱投递测试依赖 tokio）。
    #[cfg(not(target_arch = "wasm32"))]
    struct MockKvStore {
        data: Mutex<HashMap<String, Value>>,
    }

    #[cfg(not(target_arch = "wasm32"))]
    impl MockKvStore {
        fn new() -> Self {
            Self {
                data: Mutex::new(HashMap::new()),
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[async_trait::async_trait]
    impl KvStore for MockKvStore {
        async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
            Ok(self.data.lock().unwrap().get(key).cloned())
        }
        async fn set(
            &self,
            key: &str,
            value: &Value,
            _ttl: Option<Duration>,
        ) -> Result<(), MemoryError> {
            self.data
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
        }
        async fn delete(&self, key: &str) -> Result<(), MemoryError> {
            self.data.lock().unwrap().remove(key);
            Ok(())
        }
        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
            Ok(self
                .data
                .lock()
                .unwrap()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    #[test]
    fn tool_gating_grants_and_denies() {
        // 新建子代理默认无工具，宿主必须显式授予白名单。
        let mut def = AgentDefinition::new("researcher", "reads docs");
        assert!(!def.allows_tool("web_fetch"));
        // 白名单
        def.tools = vec!["read_file".into(), "web_fetch".into()];
        assert!(def.allows_tool("read_file"));
        assert!(!def.allows_tool("shell"));
        // disallowed 覆盖白名单
        def.disallowed_tools = vec!["web_fetch".into()];
        assert!(!def.allows_tool("web_fetch"));
        // "*" = 全部
        def.tools = vec!["*".into()];
        def.disallowed_tools = vec![];
        assert!(def.allows_tool("anything"));
        // disallowed "*" = 全禁（回归：修复前按精确匹配处理，tools=["*"] +
        // disallowed=["*"] 会被解释为全部放行，fail-open）。
        def.disallowed_tools = vec!["*".into()];
        assert!(!def.allows_tool("anything"));
        assert!(!def.allows_tool("read_file"));
        // 精确匹配仍优先于白名单通配。
        def.disallowed_tools = vec!["shell".into()];
        assert!(!def.allows_tool("shell"));
        assert!(def.allows_tool("read_file"));
    }

    #[test]
    fn deserialized_tool_allowlists_require_an_explicit_star() {
        let missing: AgentDefinition =
            serde_json::from_str(r#"{"name":"worker","description":"x"}"#).unwrap();
        assert!(missing.tools.is_empty());
        assert!(!missing.allows_tool("shell"));

        let explicit: AgentDefinition =
            serde_json::from_str(r#"{"name":"worker","description":"x","tools":["*"]}"#).unwrap();
        assert!(explicit.allows_tool("shell"));

        // disallowed "*" 序列化后同样全禁（host 可经 JSON 配置全禁）。
        let locked: AgentDefinition = serde_json::from_str(
            r#"{"name":"worker","description":"x","tools":["*"],"disallowed_tools":["*"]}"#,
        )
        .unwrap();
        assert!(!locked.allows_tool("anything"));

        let null = serde_json::from_str::<AgentDefinition>(
            r#"{"name":"worker","description":"x","tools":null}"#,
        );
        assert!(
            null.is_err(),
            "null must not become an unrestricted allowlist"
        );
    }

    #[test]
    fn mailbox_suffix_embeds_a_monotonic_sequence() {
        // 这是消息 ID 在同一毫秒内的唯一性锚点；随机 nonce 只能降低碰撞
        // 概率，不能作为不覆盖先前 KV value 的唯一保证。
        assert_eq!(
            mailbox_suffix(42, [0xde, 0xad, 0xbe, 0xef]),
            "000000000000002a-deadbeef"
        );
        assert_ne!(
            mailbox_suffix(42, [0, 0, 0, 0]),
            mailbox_suffix(43, [0, 0, 0, 0])
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn post_rejects_names_that_break_inbox_prefixes() {
        // review 修复回归：sender/recipient 拼入 key 前缀（`swarm/mbox/{name}/`），
        // 含 '/' 会破坏前缀结构（跨收件箱可见）、空名产生悬空前缀——必须拒绝。
        let mbox = KvMailbox::new(Arc::new(MockKvStore::new()));
        for (sender, recipient) in [("a/b", "c"), ("a", "c/d"), ("", "c"), ("a", "")] {
            let err = mbox.post(sender, recipient, "x").await.unwrap_err();
            assert!(
                matches!(err, MemoryError::Storage(_)),
                "({sender:?} -> {recipient:?}) must be rejected: {err:?}"
            );
        }
        // 合法名称照常投递。
        assert!(mbox.post("lead", "researcher", "hi").await.is_ok());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn post_rejects_oversized_message_body() {
        // review 修复回归：消息体无上限时，子代理/外部源可写入无界消息
        // 膨胀 KV 与 inbox 读取面；必须按字节上限拒绝。
        let mbox = KvMailbox::new(Arc::new(MockKvStore::new()));
        let oversized = "x".repeat(MAILBOX_MAX_BODY_BYTES + 1);
        let err = mbox
            .post("lead", "researcher", &oversized)
            .await
            .unwrap_err();
        assert!(
            matches!(err, MemoryError::Storage(_)),
            "oversized body must be rejected: {err:?}"
        );
        // 边界值：恰好等于上限的正文照常投递。
        let exact = "x".repeat(MAILBOX_MAX_BODY_BYTES);
        assert!(mbox.post("lead", "researcher", &exact).await.is_ok());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn mailbox_read_paths_preserve_recipient_isolation() {
        let mbox = KvMailbox::new(Arc::new(MockKvStore::new()));
        let lead = mbox.post("researcher", "lead", "private").await.unwrap();
        mbox.post("lead", "researcher", "reply").await.unwrap();

        // 空 recipient 曾退化为 `swarm/mbox/` 前缀并枚举全部信箱；所有
        // 读取/修改入口必须像 post 一样拒绝它以及可改变层级的 '/'.
        for recipient in ["", "lead/other"] {
            assert!(matches!(
                mbox.inbox(recipient).await,
                Err(MemoryError::Storage(_))
            ));
            assert!(matches!(
                mbox.unread(recipient).await,
                Err(MemoryError::Storage(_))
            ));
            assert!(matches!(
                mbox.mark_read(recipient, &lead.id).await,
                Err(MemoryError::Storage(_))
            ));
        }
        assert!(matches!(
            mbox.mark_read("lead", "../researcher/not-a-message").await,
            Err(MemoryError::Storage(_))
        ));

        // 合法收件人仍只能看到自己的消息，且可标记自己的消息。
        assert_eq!(mbox.inbox("lead").await.unwrap(), vec![lead.clone()]);
        mbox.mark_read("lead", &lead.id).await.unwrap();
        assert!(mbox.unread("lead").await.unwrap().is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn mailbox_cleanup_prunes_read_and_clears_recipient() {
        // review 修复回归：消息无 TTL 会无限累积，清理 API 必须真实删除。
        let mbox = KvMailbox::new(Arc::new(MockKvStore::new()));
        let m1 = mbox.post("researcher", "lead", "one").await.unwrap();
        let m2 = mbox.post("researcher", "lead", "two").await.unwrap();
        let m3 = mbox.post("researcher", "other", "other-box").await.unwrap();

        // 全部未读：prune_read 不删任何消息。
        assert_eq!(mbox.prune_read("lead").await.unwrap(), 0);
        assert_eq!(mbox.inbox("lead").await.unwrap().len(), 2);

        // 标记一条已读后 prune：只删已读，未读保留。
        mbox.mark_read("lead", &m1.id).await.unwrap();
        assert_eq!(mbox.prune_read("lead").await.unwrap(), 1);
        assert_eq!(mbox.inbox("lead").await.unwrap(), vec![m2]);
        // 其它收件人不受影响。
        assert_eq!(mbox.inbox("other").await.unwrap(), vec![m3.clone()]);

        // clear_recipient：清空该收件人信箱（其它收件人仍保留）。
        assert_eq!(mbox.clear_recipient("lead").await.unwrap(), 1);
        assert!(mbox.inbox("lead").await.unwrap().is_empty());
        assert_eq!(mbox.inbox("other").await.unwrap(), vec![m3]);

        // 非法收件人同样拒绝。
        assert!(matches!(
            mbox.clear_recipient("").await,
            Err(MemoryError::Storage(_))
        ));
        assert!(matches!(
            mbox.prune_read("lead/other").await,
            Err(MemoryError::Storage(_))
        ));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn mailbox_scope_isolates_independent_swarms() {
        let shared = Arc::new(MockKvStore::new());
        let alpha = KvMailbox::with_scope(Arc::clone(&shared) as Arc<_>, "alpha").unwrap();
        let beta = KvMailbox::with_scope(Arc::clone(&shared) as Arc<_>, "beta").unwrap();
        let alpha_message = alpha
            .post("researcher", "lead", "alpha-only")
            .await
            .unwrap();
        let beta_message = beta.post("researcher", "lead", "beta-only").await.unwrap();

        // 两个 swarm 可使用同名 lead/researcher，但共享持久化 KV 时不得
        // 枚举或修改彼此的收件箱。
        assert_eq!(
            alpha.inbox("lead").await.unwrap(),
            vec![alpha_message.clone()]
        );
        assert_eq!(
            beta.inbox("lead").await.unwrap(),
            vec![beta_message.clone()]
        );
        alpha.mark_read("lead", &alpha_message.id).await.unwrap();
        assert!(alpha.unread("lead").await.unwrap().is_empty());
        assert_eq!(beta.unread("lead").await.unwrap(), vec![beta_message]);

        for invalid in ["", "alpha/child"] {
            assert!(matches!(
                KvMailbox::with_scope(Arc::clone(&shared) as Arc<_>, invalid),
                Err(MemoryError::Storage(_))
            ));
        }
    }

    #[test]
    fn escalation_required_for_ungranted_tool() {
        let mut def = AgentDefinition::new("tester", "runs tests");
        def.tools = vec!["read_file".into()];
        assert!(needs_escalation(&def, "shell")); // 越权 → 上收 lead
        assert!(!needs_escalation(&def, "read_file")); // 已授予 → 无需上收
    }

    #[test]
    fn registry_register_get_list() {
        let mut reg = AgentRegistry::new();
        reg.register(AgentDefinition::new("a", "A"));
        reg.register(AgentDefinition::new("b", "B"));
        assert_eq!(reg.len(), 2);
        assert!(reg.get("a").is_some());
        let names: Vec<&str> = reg.list().iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[cfg(not(target_arch = "wasm32"))]
    struct EchoRunner;
    #[cfg(not(target_arch = "wasm32"))]
    #[async_trait::async_trait]
    impl TeammateRunner for EchoRunner {
        async fn run(&self, def: &AgentDefinition, task: &TeammateTask) -> Result<String, String> {
            if task.prompt.contains("fail") {
                return Err("teammate failed".into());
            }
            Ok(format!("[{}] {}", def.name, task.prompt))
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[tokio::test]
    async fn executor_dispatches_to_registered_agent() {
        let mut reg = AgentRegistry::new();
        reg.register(AgentDefinition::new("researcher", "reads"));
        let exec = InProcessExecutor::new(reg, Arc::new(EchoRunner));

        let ok = exec
            .dispatch(TeammateTask {
                agent: "researcher".into(),
                prompt: "summarize".into(),
            })
            .await
            .unwrap();
        assert!(ok.success);
        assert_eq!(ok.output, "[researcher] summarize");

        let fail = exec
            .dispatch(TeammateTask {
                agent: "researcher".into(),
                prompt: "please fail".into(),
            })
            .await
            .unwrap();
        assert!(!fail.success);

        // 未注册子代理
        let missing = exec
            .dispatch(TeammateTask {
                agent: "ghost".into(),
                prompt: "x".into(),
            })
            .await;
        assert!(matches!(missing, Err(MemoryError::NotFound(_))));
    }
}

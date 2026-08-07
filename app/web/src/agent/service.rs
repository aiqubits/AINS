//! Agent 装配层（Phase 6.1/6.2）：AgentKernel + ToolRuntime + 存储 + 桥接通道。
//!
//! 平台差异集中在本文件的 `cfg` 分支：RuntimeAdapter、KvStore 后端与
//! 工具集；桥接协议（channel 结构）双端一致。desktop 端经 `#[path]`
//! 引用本文件复用同一实现。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use futures::channel::mpsc;

use agent_core::context::session::{SessionSaveInput, SessionStore};
use agent_core::kernel::{
    AgentEvent, AgentKernel, AgentKernelConfig, ConversationMessage, StreamEvent,
};
use agent_core::memory::{KvStore, MemdirStore};
use agent_core::model_client::UsageSnapshot;
use agent_core::model_service::GatewayModelClient;
use agent_core::policy::{PermissionEngine, PermissionMode, PermissionSettings, SandboxPolicy};
use agent_core::skills::KvSkillStore;
use agent_core::tools::compute::{CalculatorTool, DateTool, JsonTool, MarkdownTool, TextTool};
#[cfg(any(target_arch = "wasm32", unix))]
use agent_core::tools::interact::TodoWriteTool;
use agent_core::tools::interact::{AskUserQuestionTool, EnterPlanModeTool, ExitPlanModeTool};
use agent_core::tools::network::WebFetchTool;
use agent_core::tools::{ToolCategory, ToolRuntime};
use client_api::Client;
use ui::{
    PERSIST_IDLE, PERSIST_PENDING, PERSIST_STATE, TOOL_STATE_LOAD_ERROR, persist_task_in_flight,
    should_sync_persist_error, sync_persist_error,
};

use super::permission_bridge::{
    InteractionMsg, PermissionPromptMsg, UiInteraction, UiPermissionPrompt,
};

// ═══════════════════════════════════════════════════════════════════
//  工具活跃状态（Phase 6.7 扩展）
// ═══════════════════════════════════════════════════════════════════

/// KvStore 中工具活跃状态的键：值为禁用工具名数组（空 = 全部活跃）。
const TOOL_STATES_KEY: &str = "tool_states";

/// KvStore 中"上次持久化失败"标记键：值为失败原因字符串。persist 失败
/// 时尽力写入、成功时清除；/tools 视图挂载时读取并提示——组件级 Signal
/// 在视图重挂载后清空，仅靠它无法让"切换未落盘"跨挂载/跨重启可见。
const TOOL_STATES_PERSIST_ERROR_KEY: &str = "tool_states_persist_error";

/// 工具活跃状态的进程级单例（纯内存）。Kernel 的 `ToolRuntime` 与 /tools
/// 面板共享同一 `Arc<RwLock<HashSet>>`：面板切换后 Kernel 下一轮
/// `api_schemas()` 自动生效，无需跨会话通知。持久化由宿主经 KvStore
/// 完成（[`load_tool_states`] / [`persist_tool_states`]，key 见
/// `TOOL_STATES_KEY`）；`Arc<RwLock<HashSet<String>>>` 双端均为 Send+Sync，
/// 故使用 `static OnceLock` 进程级单例（wasm 单线程 / desktop 多线程一致）。
///
/// 约束：wasm 端为单标签页假设——IndexedDB 同一 origin 跨标签页共享，但
/// 各标签页持有独立内存单例与独立 dirty 版本号，后写覆盖先写（与 session
/// 快照、skills 状态等既有单进程假设一致）。若未来支持多标签页，需用
/// BroadcastChannel / storage 事件做跨页同步。
///
/// `dirty` 为修改版本号（0 = 无未落盘修改）：面板切换（异步持久化尚未
/// 完成）后，`load_tool_states` 不得用陈旧存储值覆盖内存，否则用户刚
/// 禁用的工具会被静默回滚为活跃，破坏 fail-closed 语义。用单调递增版本
/// 号而非布尔位：重叠持久化时，较早完成的落盘只有在其期间无新修改
/// （版本号未变）时才清除标记，避免把用户更新的切换误标为已落盘。
pub struct ToolStateService {
    disabled: Arc<RwLock<HashSet<String>>>,
    dirty: Arc<AtomicU64>,
}

impl ToolStateService {
    fn new() -> Self {
        Self {
            disabled: Arc::new(RwLock::new(HashSet::new())),
            dirty: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 共享禁用集合引用（注入 Kernel 的 ToolRuntime）。
    pub fn shared(&self) -> Arc<RwLock<HashSet<String>>> {
        Arc::clone(&self.disabled)
    }

    /// 查询工具活跃状态（缺省 true）。
    pub fn is_enabled(&self, name: &str) -> bool {
        !self
            .disabled
            .read()
            .expect("tool state lock poisoned")
            .contains(name)
    }

    /// 设置工具活跃状态（默认所有工具活跃）。仅在状态实际变化时递增修改
    /// 版本号，且递增发生在写锁内（与加载路径的检查互斥，消除
    /// check-then-act 竞态）：面板切换后的异步持久化完成前，存储加载不得
    /// 覆盖内存。
    ///
    /// 单一事实源契约（review Nit）：本方法（与 [`Self::apply_from_store`]）
    /// 是生产路径唯一的禁用集合读写入口；`ToolRuntime` 侧的
    /// `set_tool_enabled` / `import_disabled` 仅供框架独立使用/测试，其
    /// "仅在变更时记账"语义必须与本方法保持一致——修改其一须同步另一。
    pub fn set_enabled(&self, name: &str, enabled: bool) {
        let mut guard = self.disabled.write().expect("tool state lock poisoned");
        let changed = if enabled {
            guard.remove(name)
        } else {
            guard.insert(name.to_string())
        };
        if changed {
            self.dirty.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// 从持久化状态重建集合。本进程已有未落盘修改（版本号非 0）时跳过，
    /// 避免陈旧存储值覆盖用户刚做的切换。版本号检查在写锁内完成：与
    /// `set_enabled` 的持锁递增互斥，二者之间不存在竞态窗口。
    ///
    /// 与 `ToolRuntime::import_disabled` 的语义差异（review Nit）：后者
    /// 无条件替换且不检查版本号——存储恢复只能走本方法，误用 runtime 侧
    /// 导入会覆盖用户刚做的未落盘切换（破坏 fail-closed 语义）。
    fn apply_from_store(&self, disabled: Vec<String>) {
        let mut guard = self.disabled.write().expect("tool state lock poisoned");
        if self.dirty.load(Ordering::SeqCst) != 0 {
            // 本进程存在未落盘切换：跳过陈旧存储值（语义正确）。debug
            // 留痕即可——这是设计内分支而非错误路径，且桌面端每次会话
            // 装配都会走到这里，warn 会在多窗口/多次进入时重复刷屏。
            tracing::debug!("tool states: local changes pending, skipping stale store apply");
            return;
        }
        guard.clear();
        guard.extend(disabled);
    }

    /// 当前禁用集合快照 + 修改版本号（单次读锁内原子采样，review 中等
    /// 问题 3 修复）：落盘前记录版本号与快照必须一致——分开采样时，两者
    /// 之间发生切换（set_enabled）会让快照已含新变更而版本号未含，
    /// `mark_clean(persisted_at)` 拒绝清零导致脏标记残留到下一轮
    /// （fail-safe 但多一轮无谓落盘，且版本号语义失真）。持读锁期间与
    /// `set_enabled`/`apply_from_store` 的写锁互斥，读取一致；dirty 为
    /// 原子量，读锁内 load 同样安全（与 `has_retained_state` 同模式）。
    pub fn snapshot_with_version(&self) -> (Vec<String>, u64) {
        let guard = self.disabled.read().expect("tool state lock poisoned");
        let mut names: Vec<String> = guard.iter().cloned().collect();
        names.sort();
        (names, self.dirty.load(Ordering::SeqCst))
    }

    /// 当前修改版本号（持久化落盘前记录，用于落盘后条件清除脏标记）。
    pub fn dirty_version(&self) -> u64 {
        self.dirty.load(Ordering::SeqCst)
    }

    /// 本进程内存中是否存在需保留的工具状态：未落盘修改（dirty != 0）或
    /// 已生效的禁用清单（成功落盘后 dirty 清零，但禁用仍生效）。存储加载
    /// 失败时据此区分横幅文案——true 表示禁用仍生效，用"未保存/已有状态
    /// 保留"文案；false 才是全量 fail-open（全部工具活跃）。必须合并判断
    /// 而非只查 dirty：已落盘状态 + 加载失败时，内存禁用清单原样保留，
    /// 仅查 dirty 会误报"全部活跃"（review Medium）。
    pub fn has_retained_state(&self) -> bool {
        // 单次读锁内完成 dirty 与禁用集合的合并判断（review Minor 2 修复）：
        // 两次独立读取（dirty_version 无锁原子读 + 禁用集合读锁）之间
        // 存在中间状态窗口——dirty 刚被 mark_clean 清零而快照尚未清空，
        // 或反之——可能选错横幅文案。持读锁期间与 set_enabled/apply_from_store
        // 的写锁互斥，读取一致；dirty 为原子量，读锁内 load 同样安全。
        let guard = self.disabled.read().expect("tool state lock poisoned");
        self.dirty.load(Ordering::SeqCst) != 0 || !guard.is_empty()
    }

    /// dirty 递增回调（注入 Kernel 的 ToolRuntime）：runtime 侧直写共享
    /// 集合（`set_tool_enabled`/`import_disabled`）时经此回调与面板侧
    /// `set_enabled` 统一记账，存储加载不会用陈旧值覆盖未落盘修改。
    /// 捕获共享计数器引用（review 中等问题 7）：任意实例调用都记账到该
    /// 实例自身的版本号，而非硬编码进程级单例——非单例实例（如测试隔离
    /// 环境）调用时版本号判断不失真。
    pub fn dirty_observer(&self) -> Arc<dyn Fn() + Send + Sync> {
        let dirty = Arc::clone(&self.dirty);
        Arc::new(move || {
            dirty.fetch_add(1, Ordering::SeqCst);
        })
    }

    /// 持久化成功后条件清除脏标记：仅当落盘期间没有新修改（当前版本号仍
    /// 等于落盘时记录值）才清零；否则保留版本号，避免较早完成的持久化把
    /// 用户更新的切换误标为已落盘，导致存储加载重新覆盖内存。
    fn mark_clean(&self, persisted_at: u64) {
        self.dirty
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
                (current == persisted_at).then_some(0)
            })
            .ok();
    }

    /// 测试辅助：重置为默认全活跃（清空禁用集合并清除脏标记）。
    /// cfg 与 tests 模块一致：wasm32 下无 native 测试引用，避免 dead_code。
    #[cfg(all(test, not(target_arch = "wasm32")))]
    fn reset_all(&self) {
        self.disabled
            .write()
            .expect("tool state lock poisoned")
            .clear();
        self.dirty.store(0, Ordering::SeqCst);
    }
}

/// 获取进程级单例（惰性初始化）。
pub fn tool_state_service() -> &'static ToolStateService {
    static SERVICE: OnceLock<ToolStateService> = OnceLock::new();
    SERVICE.get_or_init(ToolStateService::new)
}

/// 从本地存储加载工具活跃状态到进程级单例（会话装配 / 面板打开时调用）。
/// 无记录时保持默认全活跃；存储值格式不识别（数据损坏 / 旧版本格式）时
/// 返回 Err 走 fail-open 横幅路径（显式告知“设置未恢复”，而非静默清空
/// 用户停用列表）。本进程存在未落盘修改时跳过覆盖（见
/// [`ToolStateService::apply_from_store`]）。
#[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
pub async fn load_tool_states() -> Result<(), String> {
    load_tool_states_from(open_kv_store().await?.as_ref()).await
}

/// 将当前禁用集合持久化到本地存储（面板切换后调用）。落盘前与当前已
/// 注册工具清单求交，过滤已不存在的陈旧工具名；成功后清除脏标记。
#[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
pub async fn persist_tool_states() -> Result<(), String> {
    persist_tool_states_to(open_kv_store().await?.as_ref()).await
}

/// [`load_tool_states`] 的核心，接收任意存储后端（测试注入 mock）。
/// 与 persist 串行（review 第二轮 Medium）：`store.get` 读取与
/// `apply_from_store` 的 dirty 检查之间，若在途落盘恰好完成（写新值 +
/// `mark_clean` 清 dirty），陈旧读值会在 dirty==0 时覆盖内存——用户刚
/// 保存的切换在会话内被静默回滚（破坏 fail-closed，且 load 返回 Ok 无
/// 横幅提示）。持锁后 load 与 persist 互斥：load 先跑则 persist 无法清
/// dirty（apply 跳过陈旧值），persist 先跑则 load 读到最终值；锁序与
/// persist 一致（先 PERSIST_LOCK 后 disabled 锁），无死锁。
async fn load_tool_states_from(store: &dyn KvStore) -> Result<(), String> {
    // 与持久化写串行化锁互斥（见 PERSIST_LOCK 注释），消除陈旧读窗口
    let _guard = PERSIST_LOCK.lock().await;
    match store.get(TOOL_STATES_KEY).await {
        Ok(Some(value)) => {
            let Some(items) = value.as_array() else {
                // 格式不识别（数据损坏 / 旧版本写入的其他格式）不得静默清空
                // 禁用清单：`unwrap_or_default` 会在 dirty==0 时把空列表
                // apply 进内存，用户停用被悄悄撤销且 load 返回 Ok 无横幅
                // 提示。升级为 Err 复用 fail-open 横幅路径，与存储读取失败
                // 行为对称（显式告知而非静默）。
                tracing::warn!(
                    "tool states: stored value has unexpected format, ignoring: {value:?}"
                );
                return Err(
                    "tool states: stored value has unexpected format (expected JSON array of tool names)"
                        .to_string(),
                );
            };
            let mut disabled = Vec::with_capacity(items.len());
            for v in items {
                let Some(s) = v.as_str() else {
                    // 与整体非数组格式一致：数组内混入非字符串元素同样视为
                    // 数据损坏，不得静默丢弃部分元素——用户禁用清单会被悄悄
                    // 截断且无提示（review Major #2）。升级为 Err 走 fail-open
                    // 横幅路径，apply_from_store 不执行。
                    tracing::warn!(
                        "tool states: array contains non-string element, ignoring: {v:?}"
                    );
                    return Err(
                        "tool states: array contains non-string element (expected JSON array of tool names)"
                            .to_string(),
                    );
                };
                disabled.push(s.to_string());
            }
            tool_state_service().apply_from_store(disabled);
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(format!("tool states: {e}")),
    }
}

/// 持久化写串行化锁：`dirty` 版本号只防护 load-vs-save（陈旧存储值不得
/// 覆盖内存）；两个重叠 persist 写同一 key 时，较早发起的落盘若晚于较新
/// 落盘完成，会把存储回退为陈旧值且脏标记已清除，下次 load 回灌导致
/// 用户切换被静默回滚。此锁保证同一时刻只有一个落盘在写，后发起的
/// persist 等待前一个完成后覆盖写最新快照（futures::lock::Mutex 跨 await
/// 安全，wasm 单线程 / native 多线程一致）。
static PERSIST_LOCK: futures::lock::Mutex<()> = futures::lock::Mutex::new(());

/// 已注册工具名集合的进程级缓存（`tool_schema_snapshot()` 的名字投影）。
///
/// 工具系统为编译期静态注册（agent-core 硬编码实现 + `register_tools`），
/// 注册表在进程生命周期内不变：每次落盘都重建 ToolRuntime（工具注册 +
/// UiInteraction channel + workspace 解析）是纯浪费。snapshot 的 workspace
/// 解析与 [`initialize`] 对齐（cwd 不可用时回退占位路径，见
/// [`resolve_schema_workspace`]），故注册集在 native 端恒定包含
/// `background_task`。若未来引入动态注册（如 MCP 热插拔），需在注册点
/// 失效此缓存。
static REGISTERED_TOOL_NAMES: OnceLock<HashSet<String>> = OnceLock::new();

/// 进程内已注册工具名集合（惰性初始化）。持久化落盘前与禁用集合求交，
/// 过滤已不存在的陈旧工具名。
fn registered_tool_names() -> &'static HashSet<String> {
    REGISTERED_TOOL_NAMES.get_or_init(|| {
        tool_schema_snapshot()
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
    })
}

/// [`persist_tool_states`] 的核心，接收任意存储后端（测试注入 mock）。
/// 落盘前记录当前修改版本号：落盘期间若用户继续切换（版本号递增），
/// 完成时的条件清除会被跳过，保证后续加载不会回灌陈旧状态。
async fn persist_tool_states_to(store: &dyn KvStore) -> Result<(), String> {
    persist_tool_states_to_with_known(store, registered_tool_names().clone()).await
}

/// 落盘核心（测试可注入自定义“已知工具注册表”，避免耦合真实
/// `tool_schema_snapshot()` 的注册内容）。串行化锁在此获取：
/// [`persist_tool_states_to`] 的并发调用在锁上排队，后发起者覆盖写最新快照。
async fn persist_tool_states_to_with_known(
    store: &dyn KvStore,
    known: HashSet<String>,
) -> Result<(), String> {
    let _persist_guard = PERSIST_LOCK.lock().await;
    // 快照与版本号在同一读锁内采样（review 中等问题 3 修复）：分开采样时
    // 两者之间发生切换会让快照含新变更而版本号旧，mark_clean 拒绝清零致
    // 脏标记残留——版本号须精确反映所落盘快照。
    let (snapshot, persisted_at) = tool_state_service().snapshot_with_version();
    let names: Vec<String> = snapshot
        .into_iter()
        .filter(|name| known.contains(name))
        .collect();
    let value =
        serde_json::Value::Array(names.into_iter().map(serde_json::Value::String).collect());
    if let Err(e) = store.set(TOOL_STATES_KEY, &value, None).await {
        // 尽力写入失败标记：视图重挂载后仍能提示"上次切换未落盘"。标记
        // 写入本身失败（存储不可用）时仅记日志，该次提示缺失可接受。
        if let Err(marker_err) = store
            .set(
                TOOL_STATES_PERSIST_ERROR_KEY,
                &serde_json::json!(format!("tool states: {e}")),
                None,
            )
            .await
        {
            tracing::warn!("tool states persist error marker write failed: {marker_err}");
        }
        return Err(format!("tool states: {e}"));
    }
    // 数据落盘成功后清除失败标记。顺序不可颠倒：先删标记再写数据若失败，
    // 会丢失"未保存"提示且存储仍为旧值（静默回滚无告知）。
    if let Err(marker_err) = store.delete(TOOL_STATES_PERSIST_ERROR_KEY).await {
        // 标记删除失败（review 中等问题 4）：禁用清单已成功写入，返回 Err
        // 会让视图误报"保存失败"，与事实（已持久化）相反。记 warn 后按
        // 成功处理并正常清脏；残留 marker 仅造成"宁多提示"方向的跨挂载
        // 误报，由下一次成功 persist 再次尝试删除。
        tracing::warn!("tool states persist error marker delete failed: {marker_err}");
    }
    tool_state_service().mark_clean(persisted_at);
    Ok(())
}

/// 读取"上次持久化失败"标记：存在表示最近一次切换未落盘，跨挂载/跨重启
/// 可能回滚，需继续提示用户。无标记返回 None。视图挂载同步的完整流程见
/// [`sync_persist_error_on_mount`]。
async fn pending_persist_error_from(store: &dyn KvStore) -> Option<String> {
    match store.get(TOOL_STATES_PERSIST_ERROR_KEY).await {
        Ok(Some(value)) => value.as_str().map(String::from),
        _ => None,
    }
}

/// 视图挂载时同步 PERSIST_ERROR 的完整流程（陈旧 marker 竞态修复，review
/// Minor 1）：首次读取 marker 可能在在途落盘任务成功收敛前完成——任务成功
/// 会删除存储 marker 并清空 PERSIST_ERROR，若随后仍用首次读到的陈旧 marker
/// 置位，会造成"保存失败"横幅在状态已成功落盘后长期残留（任务完成路径
/// 不再写信号，下次切换/重挂载前无自愈手段）。流程：先检查是否有在途落盘
/// 任务（persist_task_in_flight），无在途任务时重读一次 marker 作为权威值
/// 再决定置位/清空（无任务会再修改 marker，此刻读取即最终状态）。与 /tools
/// 与会话视图挂载共用，两视图对"在途任务未收敛"感知一致。
pub async fn sync_persist_error_on_mount(message: &str) {
    if let Ok(store) = open_kv_store().await {
        sync_persist_error_on_mount_from(store.as_ref(), message, persist_task_in_flight()).await;
    }
    // 存储不可用无法读取标记：与"无标记"同处理（加载失败已有横幅），
    // 不额外提示。
}

/// [`sync_persist_error_on_mount`] 的核心，接收任意存储后端（测试注入
/// mock）；`in_flight` 为挂载时刻是否存在在途落盘任务（测试可注入，不依赖
/// 进程级状态机）。
async fn sync_persist_error_on_mount_from(store: &dyn KvStore, message: &str, in_flight: bool) {
    let pending = mount_persist_error_pending(store, in_flight).await;
    if should_sync_persist_error(&pending, in_flight) {
        sync_persist_error(pending, message);
    }
}

/// 挂载同步的 marker 读取阶段：读取失败标记，并在无在途落盘任务时重读
/// 一次作为权威值。返回最终应同步到 PERSIST_ERROR 的 marker 值。
///
/// 重读目的（review 建议 3 澄清）：`in_flight` 是挂载时刻的状态机快照，
/// 第一次 `store.get` 是异步操作——await 期间另一视图可能切换并启动新
/// 落盘任务（写/删 marker），无在途任务时重读一次缩小该窗口；有在途
/// 任务时由任务完成路径（成功清空 / 失败置位信号）收敛最终状态，重读
/// 反而可能读到写入前的 None 而误清任务即将置位的失败信号，故不重读。
async fn mount_persist_error_pending(store: &dyn KvStore, in_flight: bool) -> Option<String> {
    let pending = pending_persist_error_from(store).await;
    if !in_flight {
        pending_persist_error_from(store).await
    } else {
        pending
    }
}

/// 尽力写入"上次持久化失败"标记（panic 恢复与持久化失败路径共用，接收
/// 任意存储后端供测试注入 mock）。写入失败仅记 warn：存储不可用时该次
/// 提示缺失可接受（加载失败已有横幅兜底）。
async fn record_persist_error_marker_from(store: &dyn KvStore, message: &str) {
    if let Err(marker_err) = store
        .set(
            TOOL_STATES_PERSIST_ERROR_KEY,
            &serde_json::json!(message),
            None,
        )
        .await
    {
        tracing::warn!("tool states persist error marker write failed: {marker_err}");
    }
}

/// 落盘任务 panic 后的恢复序列（review 中等问题 1 修复，自 tools.rs 的
/// panic 分支提取为可测函数）：存在未落盘修改（dirty != 0，含被收敛丢弃
/// 的挂起切换）时尽力写入失败 marker——视图重挂载时进程级 PERSIST_ERROR
/// 信号被清空，未落盘切换若只靠信号提示会被静默遗忘（跨重启回滚无提示）；
/// 再收敛状态机到 IDLE，由下次切换经 0→1 转移重新 spawn 落盘任务。
///
/// 顺序不可颠倒：marker 先于收敛——若先收敛，挂载同步会读到
/// in_flight=false + pending=None 而清空刚置位的失败信号，marker 随后才
/// 写入且无人再重读，"切换未落盘"提示丢失（review Minor 2）。marker 写入
/// 是尽力而为的静默操作（内部不 panic），先做无风险。存储不可用时 marker
/// 无法写入（加载失败已有横幅兜底），仍须收敛状态机——否则在途标记残留，
/// 后续切换只置 PENDING 而无人消费，持久化静默失效。
///
/// 返回 true 表示收敛时存在挂起切换（recover 的 marker await 期间用户
/// 切换产生 PENDING）：调用方（落盘任务）应补一轮落盘，否则该切换只在
/// 内存（dirty 未落盘）且无人消费（下次切换 PENDING→PENDING 不 spawn 新
/// 任务），跨重启静默回滚（review 建议 1 加固）。
///
/// #[must_use]：返回值是补轮信号，忽略即静默丢弃挂起切换（仅 marker
/// 提示兜底）——未来新增调用方必须显式消费。
#[must_use]
pub async fn recover_persist_panic(message: &str) -> bool {
    match open_kv_store().await {
        Ok(store) => recover_persist_panic_from(store.as_ref(), message).await,
        Err(_) => converge_persist_state(),
    }
}

/// [`recover_persist_panic`] 的核心，接收任意存储后端（测试注入 mock）。
/// 返回语义同 [`recover_persist_panic`]（#[must_use]：见其文档）。
#[must_use]
async fn recover_persist_panic_from(store: &dyn KvStore, message: &str) -> bool {
    if tool_state_service().dirty_version() != 0 {
        record_persist_error_marker_from(store, message).await;
    }
    converge_persist_state()
}

/// 收敛落盘状态机到 IDLE（panic 恢复专用，见 [`recover_persist_panic`]）。
/// 返回 true 表示收敛前存在挂起切换（PENDING）——调用方应补一轮落盘消费。
///
/// 无条件收敛到 IDLE 而非保留 PENDING：调用方不补轮时，残留的 PENDING
/// 会永远无人消费（下次切换经 persist_on_toggle 得 PENDING→PENDING，prev
/// 非 IDLE 不 spawn 新任务），比丢弃切换更糟；丢弃方向由 dirty 保护 +
/// 失败 marker 提示兜底。
fn converge_persist_state() -> bool {
    let prev =
        PERSIST_STATE.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |_| Some(PERSIST_IDLE));
    prev == Ok(PERSIST_PENDING)
}

/// 平台 RuntimeAdapter。
#[cfg(target_arch = "wasm32")]
pub type Rt = agent_core::WasmRuntimeAdapter;
#[cfg(not(target_arch = "wasm32"))]
pub type Rt = agent_core::TokioRuntimeAdapter;

/// 装配完成的 Agent 会话：Kernel（待 take 后 spawn）+ 各桥接端点。
pub struct AgentBridge {
    kernel: Option<AgentKernel<Rt>>,
    pub event_tx: mpsc::Sender<AgentEvent>,
    pub stream_rx: Option<mpsc::UnboundedReceiver<StreamEvent>>,
    pub permission_rx: Option<mpsc::UnboundedReceiver<PermissionPromptMsg>>,
    pub interaction_rx: Option<mpsc::UnboundedReceiver<InteractionMsg>>,
    pub engine: Arc<PermissionEngine>,
    /// 中断句柄（Phase 7.1）：UI 点击停止时 `store(true)`，Kernel 在模型
    /// turn / 工具批边界中止本次查询并原子消费标志。宿主不应在发送新消息时
    /// 主动清除它，否则 Stop→Send 的竞态可能让旧查询恢复执行。
    pub interrupt: Arc<AtomicBool>,
    pub session_store: Arc<SessionStore>,
    /// 上次会话恢复的历史（已 sanitize；用于首屏渲染与镜像初始化）。
    pub restored_messages: Vec<ConversationMessage>,
    pub session_id: Option<String>,
    pub cwd: String,
}

impl AgentBridge {
    /// Kernel 只能被驱动一次：宿主取出后 `spawn(kernel.run())`。
    pub fn take_kernel(&mut self) -> Option<AgentKernel<Rt>> {
        self.kernel.take()
    }
}

/// 打开平台 KvStore 后端（Web: IndexedDB；Native: redb）。
///
/// 进程内共享单例：redb 为单进程独占锁，/agent 与 /skills 视图必须
/// 复用同一句柄；IndexedDB 虽支持多连接，为行为一致同样缓存。
// 双端统一用 Arc（native 多线程需要；wasm 单线程下非 Send 无害）
#[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
async fn open_kv_store() -> Result<Arc<dyn KvStore>, String> {
    #[cfg(target_arch = "wasm32")]
    {
        use std::cell::RefCell;
        thread_local! {
            static KV_CACHE: RefCell<Option<Arc<dyn KvStore>>> = const { RefCell::new(None) };
        }
        if let Some(kv) = KV_CACHE.with(|cache| cache.borrow().clone()) {
            return Ok(kv);
        }
        use agent_core::memory::IndexedDbKvStore;
        let kv: Arc<dyn KvStore> = Arc::new(
            IndexedDbKvStore::open("ains-agent")
                .await
                .map_err(|e| format!("IndexedDB: {e}"))?,
        );
        KV_CACHE.with(|cache| *cache.borrow_mut() = Some(Arc::clone(&kv)));
        Ok(kv)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        use agent_core::memory::{EncryptedKvStore, RedbKvStore};
        use std::sync::Mutex;
        // 仅缓存成功句柄：瞬时打开失败（如另一进程持锁）不被永久缓存，
        // 重访 /agent 或 /skills 可重试。持锁期间完成“查缓存→打开→
        // 写缓存”（open 为同步调用，无跨 await 持锁）：并发首次调用
        // 不会双开 redb 独占锁文件而误报初始化失败。
        static KV_CACHE: Mutex<Option<Arc<dyn KvStore>>> = Mutex::new(None);
        let mut cache = KV_CACHE.lock().unwrap_or_else(|poison| poison.into_inner());
        if let Some(kv) = cache.as_ref() {
            return Ok(Arc::clone(kv));
        }
        let path = native_data_path()?;
        let raw: Arc<dyn KvStore> = Arc::new(
            RedbKvStore::open(&path).map_err(|e| format!("redb {}: {e}", path.display()))?,
        );
        // 静态加密的密钥必须来自数据库之外的秘密管理面。故不自动生成并写入
        // `AINS_DATA_DIR`（那样的“加密”在磁盘失窃时无法提供保护）。运维可以从系统钥匙串 /
        // secrets manager 注入 AINS_STORAGE_KEY_HEX，以显式启用。
        let store: Arc<dyn KvStore> = match native_storage_encryption_key()? {
            Some(key) => Arc::new(EncryptedKvStore::new(raw, key)),
            None => raw,
        };
        *cache = Some(Arc::clone(&store));
        Ok(store)
    }
}

/// Native storage-encryption key environment variable.  The value is exactly
/// 32 bytes encoded as 64 hexadecimal characters, so deployment systems can
/// inject it without adding a new key-file lifecycle next to the database.
///
/// It is deliberately opt-in: turning it on for an existing plaintext store
/// requires the documented one-time migration through [`EncryptedKvStore`].
#[cfg(not(target_arch = "wasm32"))]
const STORAGE_KEY_ENV: &str = "AINS_STORAGE_KEY_HEX";

/// Load a deployment-managed key for native local storage.  Invalid key
/// material fails startup rather than silently disabling encryption.
#[cfg(not(target_arch = "wasm32"))]
fn native_storage_encryption_key() -> Result<Option<agent_core::memory::EncryptionKey>, String> {
    let Some(raw) = std::env::var_os(STORAGE_KEY_ENV) else {
        return Ok(None);
    };
    let raw = raw
        .into_string()
        .map_err(|_| format!("{STORAGE_KEY_ENV} must contain ASCII hexadecimal key material"))?;
    let bytes = parse_storage_key_hex(&raw)?;
    Ok(Some(agent_core::memory::EncryptionKey::from_bytes(bytes)))
}

/// Decode exactly 256 bits of external key material without including the
/// supplied secret in diagnostic messages.
#[cfg(not(target_arch = "wasm32"))]
fn parse_storage_key_hex(value: &str) -> Result<[u8; 32], String> {
    if value.len() != 64 {
        return Err(format!(
            "{STORAGE_KEY_ENV} must be exactly 64 hexadecimal characters (32 bytes)"
        ));
    }
    let mut bytes = [0u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(pair[0])
            .ok_or_else(|| format!("{STORAGE_KEY_ENV} must contain only hexadecimal characters"))?;
        let low = hex_nibble(pair[1])
            .ok_or_else(|| format!("{STORAGE_KEY_ENV} must contain only hexadecimal characters"))?;
        bytes[index] = (high << 4) | low;
    }
    Ok(bytes)
}

#[cfg(not(target_arch = "wasm32"))]
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Skills 面板的轻量入口：不拉起 Kernel，仅打开存储（Phase 6.4）。
// 双端统一用 Arc（native 多线程需要；wasm 单线程下非 Send 无害）
#[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
pub async fn open_skill_store() -> Result<Arc<KvSkillStore>, String> {
    Ok(Arc::new(KvSkillStore::new(open_kv_store().await?)))
}

/// Memory 浏览器的轻量入口（Phase 6.6）：打开 memdir 长期记忆库。
#[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
pub async fn open_memory_store() -> Result<Arc<MemdirStore>, String> {
    Ok(Arc::new(MemdirStore::new(open_kv_store().await?)))
}

/// snapshot 的 workspace 占位路径（native 端 cwd 不可用时回退）。
///
/// 安全前提（review 建议 2 强化）：**本路径仅用于 schema 投影，snapshot 永不
/// 执行工具**——`background_task` 的 schema 完全静态，bwrap 沙箱与任务管理器
/// 仅构造对象，canonicalize 延迟到 spawn 时。若未来任何代码路径让 snapshot
/// 真正执行工具，此占位值（文件系统根）会变成真实工作区，必须在此之前
/// 移除回退或改用安全占位。用具名常量而非内联字符串：让该前提在 use 处
/// 可见，避免未来重构时误把 `/` 当作普通真实路径使用。
#[cfg(not(target_arch = "wasm32"))]
const SCHEMA_PLACEHOLDER_WORKSPACE: &str = "/";

/// native 端 snapshot 的 workspace 解析：优先真实 cwd（与 [`initialize`]
/// 的 `bridge_cwd` 成功路径一致），失败时回退占位路径
/// [`SCHEMA_PLACEHOLDER_WORKSPACE`]。`bridge_cwd` 的 Err 来源有三种：getcwd
/// 失败（如 Linux 下进程 cwd 被删除、返回 ENOENT）、canonicalize 失败、
/// cwd 恰为文件系统根目录被拒绝——三者均回退（见常量处安全前提）。回退
/// 保证 `background_task` 始终注册——否则 /tools 面板持久化的已知工具集
/// （`REGISTERED_TOOL_NAMES`）缺少该工具，用户停用 background_task 的
/// 设置会在落盘时被静默过滤丢弃（review Major #1）。
#[cfg(not(target_arch = "wasm32"))]
fn resolve_schema_workspace(cwd: Result<String, String>) -> std::path::PathBuf {
    cwd.map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from(SCHEMA_PLACEHOLDER_WORKSPACE))
}

/// Tool 面板的轻量入口（Phase 6.7）：仅构造 ToolRuntime 取（name,
/// description, category）快照，不装配 Kernel/会话恢复/权限通道。
pub fn tool_schema_snapshot() -> Vec<(String, String, ToolCategory)> {
    let engine = PermissionEngine::new(PermissionMode::Default, PermissionSettings::default());
    let (interaction, _interaction_rx) = UiInteraction::channel();
    let mut runtime = ToolRuntime::new();
    // The snapshot never executes tools, but pass a real workspace when it is
    // available so the registered background-task schema has the same safe
    // construction path as a live agent session. 进程 cwd 不可用时回退占位
    // 路径（[`resolve_schema_workspace`]），保证 background_task 始终注册
    //（review Major #1：缺失注册会让持久化过滤掉该工具的禁用设置）。
    #[cfg(target_arch = "wasm32")]
    let schema_workspace: Option<std::path::PathBuf> = None;
    #[cfg(not(target_arch = "wasm32"))]
    let schema_workspace = Some(resolve_schema_workspace(bridge_cwd()));
    register_tools(
        &mut runtime,
        &engine,
        interaction,
        &SandboxPolicy::default(),
        schema_workspace.as_deref(),
    );
    runtime
        .all_schemas()
        .into_iter()
        .map(|def| {
            let category = runtime
                .get(&def.name)
                .map(|tool| tool.category())
                .unwrap_or(ToolCategory::Compute);
            (def.name, def.description, category)
        })
        .collect()
}

/// Native 数据目录：`AINS_DATA_DIR` 优先，其次 `$HOME/.ains`。
#[cfg(not(target_arch = "wasm32"))]
fn native_data_path() -> Result<std::path::PathBuf, String> {
    let dir = std::env::var_os("AINS_DATA_DIR")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::PathBuf::from(home).join(".ains"))
        })
        .ok_or_else(|| "neither AINS_DATA_DIR nor HOME is set".to_string())?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    Ok(dir.join("agent.redb"))
}

/// 会话与权限求值的锚定 cwd（Web 端无文件系统，用固定虚拟根）。
/// Native 端拒绝把文件系统根目录当作工作区，避免默认四象限策略意外
/// 将整台主机授权给文件工具。
fn bridge_cwd() -> Result<String, String> {
    #[cfg(target_arch = "wasm32")]
    {
        Ok("/ains-web".to_string())
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::current_dir()
            .map_err(|error| format!("cannot determine agent workspace: {error}"))
            // Shell sandbox 与权限引擎都按真实路径执行策略；保留符号链接形式
            // 会使工作区 allowlist 与实际 cwd 不同，既可能误拒绝，也会削弱
            // 工具层对工作区边界的一致性。
            .and_then(|path| {
                std::fs::canonicalize(&path).map_err(|error| {
                    format!("cannot resolve agent workspace {}: {error}", path.display())
                })
            })
            .and_then(|path| {
                if path.parent().is_none() {
                    Err("agent workspace cannot be the filesystem root".to_string())
                } else {
                    Ok(path.display().to_string())
                }
            })
    }
}

/// 注册平台工具集（按隔离环境驱动，而非仅 wasm/native 二分）：
/// - 通用工具（compute/interact/web_fetch）：全平台；
/// - 文件工具：Unix 原生平台；Web 与 Windows 均不注册，直到各自具备安全的
///   句柄相对文件系统实现；
/// - Shell + 系统集成：仅 Desktop 原生（Mobile 的 Android/iOS 应用沙箱禁止/
///   无法有用地派生子进程；Web 由浏览器隔离）。
fn register_tools(
    runtime: &mut ToolRuntime,
    engine: &Arc<PermissionEngine>,
    interaction: Arc<UiInteraction>,
    policy: &SandboxPolicy,
    workspace: Option<&std::path::Path>,
) {
    #[cfg(target_arch = "wasm32")]
    let _ = workspace;
    runtime.register(Box::new(CalculatorTool));
    runtime.register(Box::new(JsonTool));
    runtime.register(Box::new(TextTool));
    runtime.register(Box::new(MarkdownTool));
    runtime.register(Box::new(DateTool));
    // todo_write 使用同一安全文件打开路径；Windows 后端尚未实现前不向模型
    // 暴露这个必然失败的工具。
    #[cfg(any(target_arch = "wasm32", unix))]
    runtime.register(Box::new(TodoWriteTool));
    runtime.register(Box::new(AskUserQuestionTool::new(Some(interaction))));
    runtime.register(Box::new(EnterPlanModeTool::new(Arc::clone(engine))));
    runtime.register(Box::new(ExitPlanModeTool::new(Arc::clone(engine))));
    // web_fetch 携带网络域名策略（Layer 1，全平台生效；Web 仍 fail-closed）
    runtime.register(Box::new(WebFetchTool::new(policy.network.clone())));

    #[cfg(not(target_arch = "wasm32"))]
    {
        use agent_core::platform::Platform;
        // `std::fs` cannot provide the required reparse-point-safe,
        // handle-relative semantics on Windows. Keep these tools unregistered
        // there rather than advertising operations that always fail closed.
        #[cfg(unix)]
        {
            use agent_core::tools::filesystem::{
                FileEditTool, FileReadTool, FileWriteTool, GlobTool, GrepTool,
            };
            runtime.register(Box::new(FileReadTool));
            runtime.register(Box::new(FileWriteTool));
            runtime.register(Box::new(FileEditTool));
            runtime.register(Box::new(GlobTool));
            runtime.register(Box::new(GrepTool));
        }

        // Shell 与系统集成：仅 Desktop。default_sandbox 按平台选真实隔离
        //（Linux bwrap）或诚实桩（mac/Win）；隔离不可用时 shell 因
        // capabilities().shell=false 拒绝执行（不降级直跑）。
        if Platform::current() == Platform::Desktop {
            use agent_core::policy::default_sandbox;
            use agent_core::tasks::{BackgroundTaskManager, BackgroundTaskTool};
            use agent_core::tools::system::{
                ClipboardTool, NotificationTool, ScreenshotTool, ShellCommandTool,
            };
            let sandbox = default_sandbox(policy.clone());
            runtime.register(Box::new(ShellCommandTool::new(Arc::clone(&sandbox))));
            // 后台任务与 shell 共用同一沙箱及同一 workspace 边界。缺少
            // workspace 时宁可不注册，绝不把任意宿主目录作为可写 bind。
            if let Some(workspace) = workspace {
                let tasks = Arc::new(BackgroundTaskManager::with_sandbox_in_workspace(
                    sandbox, workspace,
                ));
                runtime.register(Box::new(BackgroundTaskTool::new(tasks)));
            }
            // 平台集成注入随 Phase 6.10/7 落地，当前 None → 工具自报不可用
            runtime.register(Box::new(ClipboardTool::new(None)));
            runtime.register(Box::new(NotificationTool::new(None)));
            runtime.register(Box::new(ScreenshotTool::new(None)));
        }
    }

    // 契约断言（review S5 + Nit 1 修复）：工具系统为编译期静态注册
    // （agent-core 硬编码实现 + 本函数），`REGISTERED_TOOL_NAMES` 进程级缓存
    // （落盘过滤已知工具集用）必须与实际注册集一致。缓存可能已由先前落盘经
    // `registered_tool_names()` 创建——若不一致，说明引入了动态注册（如
    // MCP 热插拔）或注册条件变化，缓存已失效，用户禁用设置会在落盘时被
    // 静默过滤（需在注册点失效缓存）。比较用无过滤的 `registered_names`
    // （review Nit 1）：api_schemas 带禁用过滤，若 runtime 已 share_disabled
    // 且禁用集合非空，过滤后的集合必然 ⊆ 缓存，debug 断言会误报 panic，
    // 掩盖真实的缓存失效；原始注册集语义与缓存（fresh runtime 快照）一致。
    #[cfg(debug_assertions)]
    if let Some(cached) = REGISTERED_TOOL_NAMES.get() {
        let actual = runtime.registered_names();
        debug_assert_eq!(
            cached, &actual,
            "registered tool set diverged from REGISTERED_TOOL_NAMES cache; \
             dynamic registration requires cache invalidation"
        );
    }
}

/// 装配 Agent 会话。`client` 由宿主提供（Web 复用已认证的 AuthState
/// client；Desktop 从环境变量构造，见各端视图）。
// 双端统一用 Arc（native 多线程需要；wasm 单线程下非 Send 无害）
#[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
pub async fn initialize(client: Client) -> Result<AgentBridge, String> {
    let kv = open_kv_store().await?;
    // 恢复工具活跃状态。读取失败不阻断会话（存储不可读时无法恢复禁用
    // 清单），但必须升级为 error 并明确告知：这是 fail-open 回退——
    // 用户此前禁用的工具会在本会话恢复为活跃，防止静默失效。失败信息
    // 写入进程级信号 TOOL_STATE_LOAD_ERROR（/tools 与会话视图共享订阅，
    // 替代原 AgentBridge 装配时快照，review Minor 1 修复）。
    match load_tool_states().await {
        Ok(()) => *TOOL_STATE_LOAD_ERROR.write() = None,
        Err(e) => {
            tracing::error!("tool state restore failed, tools default to active: {e}");
            // 快照恢复失败时刻的本地状态（review 中等问题 3）：本地已有
            // 未落盘切换，或内存禁用清单非空（成功落盘后 dirty 已清零但
            // 禁用仍生效）时加载被跳过、内存清单保留（并非全量 fail-open），
            // 视图文案需据此区分——与错误一起写入信号，渲染期重读会随
            // 会话期间 dirty/禁用集合变化而漂移，故须用失败时刻快照。
            let had_local = tool_state_service().has_retained_state();
            *TOOL_STATE_LOAD_ERROR.write() = Some((e, had_local));
        }
    }
    // 上次切换未落盘的失败标记：由会话视图挂载时经 sync_persist_error
    // 从存储同步到进程级 PERSIST_ERROR 信号（与 /tools 挂载对称），此处
    // 不再桥接——会话存活期间的落盘结果由落盘任务失败/成功直接写信号
    // 实时反映（review Minor 1 修复）。
    let session_store = Arc::new(SessionStore::new(Arc::clone(&kv)));

    // Sandbox 策略（Layer 1 + Layer 2 共用源）：默认把 Agent 工作区作为
    // 唯一可读写根目录。不要把空策略传给权限引擎，否则 read-only 文件工具
    // 会允许读取工作区之外的任意主机路径。
    let cwd = bridge_cwd()?;
    let mut policy = SandboxPolicy::default();
    policy.filesystem.allow_read.push(cwd.clone());
    policy.filesystem.allow_write.push(cwd.clone());
    let engine = PermissionEngine::with_filesystem_policy(
        PermissionMode::Default,
        PermissionSettings::default(),
        policy.filesystem.clone(),
    );
    let (prompt, permission_rx) = UiPermissionPrompt::channel();
    let (interaction, interaction_rx) = UiInteraction::channel();

    // 先强制初始化 REGISTERED_TOOL_NAMES 缓存（注册集为编译期静态，见
    // registered_tool_names 注释），使 register_tools 内的一致性断言在首次
    // 装配即生效（缓存已存在则对比实际注册集）——否则缓存只在首次落盘时
    // 初始化，动态注册回归会在首次装配静默通过（review 建议 8）。get_or_init
    // 幂等：后续装配仅读取。
    let _ = registered_tool_names();
    let mut runtime = ToolRuntime::new();
    register_tools(
        &mut runtime,
        &engine,
        interaction,
        &policy,
        Some(std::path::Path::new(&cwd)),
    );
    // 注入进程级工具活跃状态：/tools 面板与 Kernel 共享同一 Arc 集合，
    // 面板切换在 Kernel 下一轮 api_schemas 自动生效；同时注入 dirty 递增
    // 回调，runtime 侧直写（set_tool_enabled/import_disabled）统一记账。
    runtime.share_disabled(
        tool_state_service().shared(),
        Some(tool_state_service().dirty_observer()),
    );
    let runtime = runtime.with_permissions(Arc::clone(&engine), Some(prompt));

    let config = AgentKernelConfig {
        cwd: cwd.clone().into(),
        ..AgentKernelConfig::default()
    };
    let model = GatewayModelClient::<Rt>::shared(client);
    let (mut kernel, event_tx, stream_rx) = AgentKernel::<Rt>::with_runtime(model, runtime, config);
    let interrupt = kernel.interrupt_handle();

    // 会话恢复：latest 快照种子进 Kernel 上下文（快照落盘前已 sanitize）
    let mut restored_messages = Vec::new();
    let mut session_id = None;
    match session_store.load_latest(&cwd).await {
        Ok(Some(snapshot)) => {
            restored_messages = snapshot.messages.clone();
            session_id = Some(snapshot.session_id);
            kernel.context_mut().conversation = snapshot.messages;
        }
        Ok(None) => {}
        Err(err) => {
            // 恢复失败不阻断新会话（损坏快照会被下一次保存覆盖）
            tracing::warn!("session restore failed: {err}");
        }
    }

    Ok(AgentBridge {
        kernel: Some(kernel),
        event_tx,
        stream_rx: Some(stream_rx),
        permission_rx: Some(permission_rx),
        interaction_rx: Some(interaction_rx),
        engine,
        interrupt,
        session_store,
        restored_messages,
        session_id,
        cwd,
    })
}

/// 持久化会话镜像（每个 assistant turn 完成后调用）。返回稳定 session_id。
pub async fn save_snapshot(
    session_store: &SessionStore,
    cwd: &str,
    session_id: Option<String>,
    messages: Vec<ConversationMessage>,
    usage: UsageSnapshot,
) -> Option<String> {
    let input = SessionSaveInput {
        session_id,
        cwd: cwd.to_string(),
        model: None,
        system_prompt: None,
        messages,
        usage,
        tool_metadata: Default::default(),
    };
    match session_store.save(input).await {
        Ok(id) => Some(id),
        Err(err) => {
            tracing::warn!("session snapshot save failed: {err}");
            None
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    use agent_core::error::MemoryError;
    use async_trait::async_trait;
    // rsx! 宏展开需要 dioxus_elements 命名空间（tools.rs 同模式），且
    // GlobalSignal（PERSIST_ERROR）读写需 dioxus runtime（VirtualDom 提供）。
    use dioxus::prelude::*;
    use dioxus_core::VirtualDom;
    use serde_json::Value;
    use ui::{
        PERSIST_ERROR, PERSIST_IDLE, PERSIST_PENDING, PERSIST_RUNNING, persist_on_round_done,
    };

    /// 工具状态测试共享单例，必须串行（进程级 `OnceLock` 单例在测试间
    /// 共享状态；`reset_all` 隔离）。用 async-aware 锁：测试函数跨 await
    /// 持有 guard，`std::sync::Mutex` 会触发 clippy
    /// `async_await_holding_lock`（CI `-D warnings` 下失败）。
    static STATE_TEST_LOCK: futures::lock::Mutex<()> = futures::lock::Mutex::new(());

    /// 内存 KvStore 桩（仅实现 trait 必选方法，TTL/前缀清理用默认实现）。
    struct MemoryKvStore(Mutex<HashMap<String, Value>>);

    impl MemoryKvStore {
        fn new() -> Self {
            Self(Mutex::new(HashMap::new()))
        }
    }

    #[async_trait]
    impl KvStore for MemoryKvStore {
        async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        async fn set(
            &self,
            key: &str,
            value: &Value,
            _ttl: Option<Duration>,
        ) -> Result<(), MemoryError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), MemoryError> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    /// marker 读取桩：对 `TOOL_STATES_PERSIST_ERROR_KEY` 的首次 `get` 返回
    /// 陈旧失败标记、后续读取 map——模拟挂载读取 marker 时在途落盘任务恰
    /// 成功收敛（删除 marker、清空 PERSIST_ERROR 并收敛状态机）的竞态窗口
    /// （review Minor 1 回归测试用）。其余 key 行为与 [`MemoryKvStore`] 一致。
    struct MarkerStaleReadKvStore {
        map: Mutex<HashMap<String, Value>>,
        marker_gets: AtomicUsize,
    }

    impl MarkerStaleReadKvStore {
        fn new() -> Self {
            Self {
                map: Mutex::new(HashMap::new()),
                marker_gets: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl KvStore for MarkerStaleReadKvStore {
        async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
            if key == TOOL_STATES_PERSIST_ERROR_KEY {
                let call = self.marker_gets.fetch_add(1, Ordering::SeqCst);
                if call == 0 {
                    // 首次读取：任务尚未收敛，返回陈旧失败标记
                    return Ok(Some(serde_json::json!("stale failure")));
                }
            }
            Ok(self.map.lock().unwrap().get(key).cloned())
        }

        async fn set(
            &self,
            key: &str,
            value: &Value,
            _ttl: Option<Duration>,
        ) -> Result<(), MemoryError> {
            self.map
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), MemoryError> {
            self.map.lock().unwrap().remove(key);
            Ok(())
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .keys()
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    /// 可控写延迟的 KvStore 桩：第 1 次 `set` 慢（50ms）、后续快（10ms），
    /// 模拟"较早发起的落盘最后完成"的写序颠倒（M1 回归测试用）。修复前
    /// 两个重叠 persist 并发写同一 key，慢写晚完成会覆盖新落盘结果。
    struct SlowKvStore {
        map: Mutex<HashMap<String, Value>>,
        sets: AtomicUsize,
    }

    impl SlowKvStore {
        fn new() -> Self {
            Self {
                map: Mutex::new(HashMap::new()),
                sets: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl KvStore for SlowKvStore {
        async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
            Ok(self.map.lock().unwrap().get(key).cloned())
        }

        async fn set(
            &self,
            key: &str,
            value: &Value,
            _ttl: Option<Duration>,
        ) -> Result<(), MemoryError> {
            let call = self.sets.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
            } else {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            self.map
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), MemoryError> {
            self.map.lock().unwrap().remove(key);
            Ok(())
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .keys()
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    /// 首次 `set` 失败、后续成功的 KvStore 桩：复现"持久化失败 → 写入
    /// 失败标记 → 再次落盘成功清除标记"的提示闭环（review 修复）。
    struct FailOnceKvStore {
        map: Mutex<HashMap<String, Value>>,
        sets: AtomicUsize,
    }

    impl FailOnceKvStore {
        fn new() -> Self {
            Self {
                map: Mutex::new(HashMap::new()),
                sets: AtomicUsize::new(0),
            }
        }
    }

    #[async_trait]
    impl KvStore for FailOnceKvStore {
        async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
            Ok(self.map.lock().unwrap().get(key).cloned())
        }

        async fn set(
            &self,
            key: &str,
            value: &Value,
            _ttl: Option<Duration>,
        ) -> Result<(), MemoryError> {
            let call = self.sets.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                return Err(MemoryError::Storage("injected persist failure".to_string()));
            }
            self.map
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), MemoryError> {
            self.map.lock().unwrap().remove(key);
            Ok(())
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
            Ok(self
                .map
                .lock()
                .unwrap()
                .keys()
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    /// `set` 成功、`delete` 失败的 KvStore 桩：复现"数据已落盘但失败标记
    /// 删除失败"路径——持久化应返回 Ok（禁用清单已持久化，review 中等
    /// 问题 4），残留 marker 由下一次成功 persist 再删。
    struct DeleteFailKvStore(Mutex<HashMap<String, Value>>);

    impl DeleteFailKvStore {
        fn new() -> Self {
            Self(Mutex::new(HashMap::new()))
        }
    }

    #[async_trait]
    impl KvStore for DeleteFailKvStore {
        async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
            Ok(self.0.lock().unwrap().get(key).cloned())
        }

        async fn set(
            &self,
            key: &str,
            value: &Value,
            _ttl: Option<Duration>,
        ) -> Result<(), MemoryError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        async fn delete(&self, _key: &str) -> Result<(), MemoryError> {
            Err(MemoryError::Storage("injected delete failure".to_string()))
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    /// `get` 失败的 KvStore 桩：模拟存储读取失败（fail-open 分支）——
    /// 恢复失败时内存禁用清单原样保留，`has_retained_state` 决定横幅文案。
    struct GetFailKvStore;

    #[async_trait]
    impl KvStore for GetFailKvStore {
        async fn get(&self, _key: &str) -> Result<Option<Value>, MemoryError> {
            Err(MemoryError::Storage("injected get failure".to_string()))
        }

        async fn set(
            &self,
            _key: &str,
            _value: &Value,
            _ttl: Option<Duration>,
        ) -> Result<(), MemoryError> {
            Ok(())
        }

        async fn delete(&self, _key: &str) -> Result<(), MemoryError> {
            Ok(())
        }

        async fn list_prefix(&self, _prefix: &str) -> Result<Vec<String>, MemoryError> {
            Ok(Vec::new())
        }
    }

    /// 读旧值后延迟返回的 KvStore 桩：`get` 先读取当前值快照再 sleep
    /// 60ms 返回——复现 load 读到在途落盘前的存储旧值、期间 persist 完成
    /// 写新值并清 dirty、apply 用陈旧值覆盖内存的 TOCTOU（review 第二轮
    /// Medium）。
    struct StaleReadKvStore(Mutex<HashMap<String, Value>>);

    impl StaleReadKvStore {
        fn new() -> Self {
            Self(Mutex::new(HashMap::new()))
        }
    }

    #[async_trait]
    impl KvStore for StaleReadKvStore {
        async fn get(&self, key: &str) -> Result<Option<Value>, MemoryError> {
            // 先读快照（在途落盘前的旧值），再延迟返回——apply 的 dirty
            // 检查将发生在 persist 完成清 dirty 之后
            let value = self.0.lock().unwrap().get(key).cloned();
            tokio::time::sleep(Duration::from_millis(60)).await;
            Ok(value)
        }

        async fn set(
            &self,
            key: &str,
            value: &Value,
            _ttl: Option<Duration>,
        ) -> Result<(), MemoryError> {
            self.0
                .lock()
                .unwrap()
                .insert(key.to_string(), value.clone());
            Ok(())
        }

        async fn delete(&self, key: &str) -> Result<(), MemoryError> {
            self.0.lock().unwrap().remove(key);
            Ok(())
        }

        async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>, MemoryError> {
            Ok(self
                .0
                .lock()
                .unwrap()
                .keys()
                .filter(|key| key.starts_with(prefix))
                .cloned()
                .collect())
        }
    }

    fn stored_disabled(map: &Mutex<HashMap<String, Value>>) -> Vec<String> {
        map.lock()
            .unwrap()
            .get(TOOL_STATES_KEY)
            .and_then(|v| v.as_array().cloned())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn tool_state_persist_roundtrip_restores_disabled_names() {
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        // known 集合显式注入（与 filters_unknown_names 测试同风格）：不
        // 依赖真实注册表当前恰好注册 calculator/date，未来移除/改名真实
        // 工具不破坏本测试。
        let known: HashSet<String> = ["calculator".to_string(), "date".to_string()]
            .into_iter()
            .collect();

        tool_state_service().set_enabled("calculator", false);
        tool_state_service().set_enabled("date", false);
        persist_tool_states_to_with_known(&store, known)
            .await
            .unwrap();
        assert_eq!(
            stored_disabled(&store.0),
            vec!["calculator".to_string(), "date".to_string()]
        );

        // 新“进程”：重置内存后从存储恢复
        tool_state_service().reset_all();
        load_tool_states_from(&store).await.unwrap();
        assert!(!tool_state_service().is_enabled("calculator"));
        assert!(!tool_state_service().is_enabled("date"));
        assert!(tool_state_service().is_enabled("web_fetch"));
    }

    #[tokio::test]
    async fn tool_state_load_rejects_non_array_value_with_error() {
        // 回归（review Minor 1）：存储值非数组（数据损坏 / 旧格式）时不得
        // 静默清空禁用清单——`unwrap_or_default` 会把空列表 apply 进内存，
        // dirty==0 时用户停用被悄悄撤销且无任何提示。修复后返回 Err 走
        // fail-open 横幅路径（与存储读取失败对称），apply_from_store 不
        // 执行，内存保持默认全活跃。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        store.0.lock().unwrap().insert(
            TOOL_STATES_KEY.to_string(),
            Value::Object(Default::default()),
        );

        assert!(load_tool_states_from(&store).await.is_err());
        // apply_from_store 未执行：内存保持默认全活跃（无禁用清单）
        assert!(tool_state_service().is_enabled("calculator"));
        assert_eq!(
            tool_state_service().snapshot_with_version().0,
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn tool_state_load_rejects_non_string_array_element_with_error() {
        // 回归（review Major #2）：数组内混入非字符串元素（数据损坏）时不得
        // 静默丢弃部分元素——修复前 filter_map 会把禁用清单悄悄截断且无提示
        // （与整体非数组格式不一致）。修复后显式校验返回 Err 走 fail-open
        // 横幅路径，apply_from_store 不执行，内存保持默认全活跃。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        store.0.lock().unwrap().insert(
            TOOL_STATES_KEY.to_string(),
            serde_json::json!(["date", 42, "calculator"]),
        );

        assert!(load_tool_states_from(&store).await.is_err());
        // apply_from_store 未执行：内存保持默认全活跃（无禁用清单），
        // 部分有效元素（date/calculator）也不得被单独应用
        assert!(tool_state_service().is_enabled("date"));
        assert!(tool_state_service().is_enabled("calculator"));
        assert_eq!(
            tool_state_service().snapshot_with_version().0,
            Vec::<String>::new()
        );
    }

    #[tokio::test]
    async fn tool_state_load_non_string_element_keeps_retained_state() {
        // 回归（review Major #2 对照）：内存已有已落盘的禁用状态（dirty==0
        // 但禁用仍生效）时，数组内非字符串元素同样不得清空——与整体非数组
        // 格式路径（tool_state_load_non_array_value_keeps_retained_state）
        // 对称，横幅文案依据 has_retained_state（"已有状态保留"）。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        store
            .0
            .lock()
            .unwrap()
            .insert(TOOL_STATES_KEY.to_string(), serde_json::json!(["date", 42]));
        // 已落盘的禁用状态：直接注入共享集合模拟（不经 set_enabled，dirty
        // 保持 0——即成功落盘后 mark_clean 清零的常态）
        tool_state_service()
            .shared()
            .write()
            .expect("tool state lock poisoned")
            .insert("date".to_string());
        assert_eq!(tool_state_service().dirty_version(), 0);

        assert!(load_tool_states_from(&store).await.is_err());
        // 内存禁用清单原样保留：禁用仍生效，绝非"全部活跃"
        assert!(!tool_state_service().is_enabled("date"));
        assert!(
            tool_state_service().has_retained_state(),
            "non-string element must not clear retained disabled state"
        );
    }

    #[tokio::test]
    async fn tool_state_load_non_array_value_keeps_retained_state() {
        // 回归（review Minor 1 对照）：内存已有已落盘的禁用状态（dirty==0
        // 但禁用仍生效）时，存储值格式不识别同样不得清空——横幅文案依据
        // has_retained_state（"已有状态保留"），与存储读取失败路径一致。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        store.0.lock().unwrap().insert(
            TOOL_STATES_KEY.to_string(),
            Value::Object(Default::default()),
        );
        // 已落盘的禁用状态：直接注入共享集合模拟（不经 set_enabled，dirty
        // 保持 0——即成功落盘后 mark_clean 清零的常态）
        tool_state_service()
            .shared()
            .write()
            .expect("tool state lock poisoned")
            .insert("date".to_string());
        assert_eq!(tool_state_service().dirty_version(), 0);

        assert!(load_tool_states_from(&store).await.is_err());
        // 内存禁用清单原样保留：禁用仍生效，绝非"全部活跃"
        assert!(!tool_state_service().is_enabled("date"));
        assert!(
            tool_state_service().has_retained_state(),
            "format error must not clear retained disabled state"
        );
    }

    #[tokio::test]
    async fn tool_state_load_missing_record_keeps_default_active() {
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();

        load_tool_states_from(&store).await.unwrap();
        assert!(tool_state_service().is_enabled("calculator"));
    }

    #[tokio::test]
    async fn tool_state_load_failure_keeps_retained_state() {
        // 回归（review Medium #1）：成功落盘后 dirty==0 但内存禁用清单非空
        // （已落盘的禁用仍生效），此时存储读取失败、加载被跳过，禁用保持
        // ——横幅文案依据 has_retained_state 必须为 true（"未保存/已有状态
        // 保留"文案），不得误报"全部工具活跃"。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        // 已落盘的禁用状态：直接注入共享集合模拟（不经 set_enabled，dirty
        // 保持 0——即成功落盘后 mark_clean 清零的常态）
        tool_state_service()
            .shared()
            .write()
            .expect("tool state lock poisoned")
            .insert("date".to_string());
        assert_eq!(tool_state_service().dirty_version(), 0);

        // 存储读取失败（fail-open 分支）：apply_from_store 不执行
        let store = GetFailKvStore;
        assert!(load_tool_states_from(&store).await.is_err());
        // 内存禁用清单原样保留：禁用仍生效，绝非"全部活跃"
        assert!(!tool_state_service().is_enabled("date"));
        assert!(
            tool_state_service().has_retained_state(),
            "persisted disabled + load failure must keep retained-state banner"
        );

        // 对照 1：空禁用清单 + 无未落盘修改 → 全量 fail-open（"全部活跃"文案）
        tool_state_service().reset_all();
        assert!(
            !tool_state_service().has_retained_state(),
            "no retained state means full fail-open banner"
        );
        // 对照 2：未落盘修改同样视为保留（既有 dirty 语义不变）
        tool_state_service().set_enabled("date", false);
        assert!(tool_state_service().has_retained_state());
    }

    #[tokio::test]
    async fn tool_state_load_persist_toctou_keeps_latest_switch() {
        // 回归（review 第二轮 Medium）：load 的 store.get 与在途落盘的
        // TOCTOU——get 先读到落盘前旧值快照再延迟返回，期间在途 persist
        // 完成（写新值 + mark_clean 清 dirty），apply_from_store 看到
        // dirty==0 会用陈旧值覆盖内存，用户刚保存的切换在会话内被静默
        // 回滚且 load 返回 Ok 无横幅提示。修复后 load 持 PERSIST_LOCK：
        // load 先拿锁则 apply 时 dirty 仍非 0（跳过陈旧值），persist 先
        // 拿锁则 get 读到最终值——窗口被消除。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = StaleReadKvStore::new();
        let known: HashSet<String> = ["date".to_string(), "a".to_string()].into_iter().collect();
        // 存储中留有上一个进程的旧记录（禁用 a）；本进程切换禁用 date
        store
            .0
            .lock()
            .unwrap()
            .insert(TOOL_STATES_KEY.to_string(), serde_json::json!(["a"]));
        tool_state_service().set_enabled("date", false);

        // 并发：load 的 get 先读到旧值快照 [a]（sleep 期间 persist 完成
        // 写 [date] 并清 dirty）；persist 延迟 10ms 启动确保旧值快照先
        // 落袋，时序确定。
        let persist = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            persist_tool_states_to_with_known(&store, known).await
        };
        let (load, persist) = tokio::join!(load_tool_states_from(&store), persist);
        load.unwrap();
        persist.unwrap();

        // 核心断言：date 禁用必须保留（修复前被陈旧值 [a] 回滚为活跃）
        assert!(!tool_state_service().is_enabled("date"));
        // 陈旧记录不得污染内存：a 保持活跃（最终存储值 [date] 与跳过路径
        // 都不含 a）
        assert!(tool_state_service().is_enabled("a"));
    }

    #[tokio::test]
    async fn concurrent_loads_serialize_and_apply_final_value() {
        // 跨视图挂载竞态（/tools 挂载与 /agent 会话装配可能几乎同时调用
        // load_tool_states）：PERSIST_LOCK 串行化两次加载，后执行的 apply
        // 读到最终存储值——不存在交错 apply 的中间态，两次都返回 Ok 且
        // 内存状态一致（review 建议补测）。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        store
            .0
            .lock()
            .unwrap()
            .insert(TOOL_STATES_KEY.to_string(), serde_json::json!(["date"]));

        let (first, second) =
            tokio::join!(load_tool_states_from(&store), load_tool_states_from(&store),);
        first.unwrap();
        second.unwrap();
        // 两次加载后内存反映最终存储值，且无交错覆盖的中间态
        assert!(!tool_state_service().is_enabled("date"));
        assert_eq!(
            tool_state_service().snapshot_with_version().0,
            vec!["date".to_string()]
        );
    }

    #[tokio::test]
    async fn tool_state_stale_store_apply_skipped_when_local_dirty() {
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        // 存储中有旧记录（禁用 a），本地面板刚切换禁用 b（dirty、未落盘）
        let store = MemoryKvStore::new();
        store
            .0
            .lock()
            .unwrap()
            .insert(TOOL_STATES_KEY.to_string(), serde_json::json!(["a"]));
        tool_state_service().set_enabled("b", false);

        load_tool_states_from(&store).await.unwrap();
        // 陈旧存储值不得覆盖本地未落盘修改：b 保持禁用，a 不得被旧记录重新启用
        assert!(!tool_state_service().is_enabled("b"));
        assert!(tool_state_service().is_enabled("a"));
    }

    #[tokio::test]
    async fn tool_state_persist_filters_unknown_names_and_clears_dirty() {
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();

        // 幽灵工具名（不在已知注册表）与真实工具同时禁用。known 集合显式
        // 注入而非取真实 tool_schema_snapshot()：测试不依赖当前注册表内容，
        // 未来移除/改名真实工具不破坏本测试。
        let known: HashSet<String> = ["date".to_string(), "calculator".to_string()]
            .into_iter()
            .collect();
        tool_state_service().set_enabled("date", false);
        tool_state_service().set_enabled("ghost_tool", false);
        persist_tool_states_to_with_known(&store, known)
            .await
            .unwrap();

        let stored = stored_disabled(&store.0);
        assert!(stored.contains(&"date".to_string()));
        assert!(
            !stored.contains(&"ghost_tool".to_string()),
            "unregistered tool name must be filtered: {stored:?}"
        );

        // persist 成功后脏标记清除：存储值可安全回灌（仅含已注册工具）
        tool_state_service().reset_all();
        load_tool_states_from(&store).await.unwrap();
        assert_eq!(
            tool_state_service().snapshot_with_version().0,
            vec!["date".to_string()]
        );
    }

    #[test]
    fn schema_workspace_falls_back_when_cwd_unavailable() {
        // 回归（review Major #1）：进程 cwd 不可用（如 Linux 下被删除、
        // getcwd 返回 ENOENT）时，snapshot 仍须解析出 workspace——保证
        // background_task 注册进已知工具集，/tools 面板持久化不会过滤掉
        // 用户对该工具的禁用设置（修复前 current_dir().ok() 为 None，
        // background_task 缺失注册，禁用设置在落盘时被静默丢弃）。
        let fallback = resolve_schema_workspace(Err("cwd deleted".into()));
        assert!(
            fallback.is_absolute(),
            "fallback must be an absolute path: {fallback:?}"
        );
        // 成功路径与 initialize 的 bridge_cwd 一致：真实路径原样使用
        let real = resolve_schema_workspace(Ok("/home/u/proj".into()));
        assert_eq!(real, std::path::PathBuf::from("/home/u/proj"));
    }

    #[test]
    fn registered_tool_names_keep_background_task() {
        // 契约（review Major #1）：持久化过滤用的已知工具集（REGISTERED_
        // TOOL_NAMES 缓存）必须包含 background_task——它是 initialize 成功
        // 装配会话时注册的工具。若 snapshot 因 workspace 缺失而未注册它，
        // 用户停用该工具的设置在落盘时被过滤丢失；本测试锁定该工具在已知
        // 集中始终存在（native + desktop 环境，测试进程 cwd 可用或回退占位
        // 路径均成立）。
        assert!(
            registered_tool_names().contains("background_task"),
            "known tool set must keep background_task so its disabled state is not filtered out"
        );
    }

    #[tokio::test]
    async fn tool_schema_snapshot_includes_disabled_tools() {
        // 面板契约（review 建议 4）：tool_schema_snapshot 必须返回全部注册
        // 工具（含已禁用的）——已禁用工具的面板卡片是重新启用的唯一入口，
        // 缺失则用户永远无法重新启用。修复前依赖"snapshot 新建 runtime 禁用
        // 集合为空"的隐式前提（api_schemas 过滤），显式改用无过滤的
        // all_schemas 后语义自明（未来注入共享禁用源也不会消失）。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        // 禁用真实注册工具：snapshot 仍须包含它（与进程级禁用状态无关）
        tool_state_service().set_enabled("calculator", false);
        let names: Vec<String> = tool_schema_snapshot()
            .into_iter()
            .map(|(name, _, _)| name)
            .collect();
        assert!(
            names.contains(&"calculator".to_string()),
            "snapshot must include disabled tools (re-enable entry point): {names:?}"
        );
        assert!(!tool_state_service().is_enabled("calculator"));
        tool_state_service().reset_all();
    }

    #[tokio::test]
    async fn tool_state_persist_succeeds_when_marker_delete_fails() {
        // 回归（review 中等问题 4）：数据已落盘后 marker 删除失败时，持久化
        // 必须按成功处理——返回 Err 会让视图误报"保存失败"，与事实（已持久
        // 化）相反；残留 marker 仅造成"宁多提示"方向的跨挂载误报。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = DeleteFailKvStore::new();
        let known: HashSet<String> = ["date".to_string()].into_iter().collect();
        // 上次失败的 marker 残留（本次 delete 被注入失败，无法清除）
        store.0.lock().unwrap().insert(
            TOOL_STATES_PERSIST_ERROR_KEY.to_string(),
            serde_json::json!("previous failure"),
        );

        tool_state_service().set_enabled("date", false);
        // 数据写入成功、marker 删除失败：仍按成功返回并清脏
        persist_tool_states_to_with_known(&store, known)
            .await
            .unwrap();
        assert_eq!(tool_state_service().dirty_version(), 0);
        assert!(stored_disabled(&store.0).contains(&"date".to_string()));
        // 禁用清单已落盘：新“进程”加载可恢复
        tool_state_service().reset_all();
        load_tool_states_from(&store).await.unwrap();
        assert!(!tool_state_service().is_enabled("date"));
        // marker 删除失败残留 → 跨挂载继续提示，方向安全（宁多提示）
        assert!(pending_persist_error_from(&store).await.is_some());
    }

    #[tokio::test]
    async fn tool_state_persist_failure_marks_then_success_clears_marker() {
        // 回归（review 中等问题 2）：持久化失败仅靠组件级 Signal 提示，
        // 视图重挂载后清空 → 未落盘切换被静默遗忘。失败标记写入存储后，
        // 下次挂载（pending_persist_error）仍能提示；直到落盘成功才清除。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = FailOnceKvStore::new();
        let known: HashSet<String> = ["date".to_string()].into_iter().collect();

        tool_state_service().set_enabled("date", false);
        // 第一次落盘失败：写入失败标记并返回 Err，dirty 保持非 0（未落盘
        // 修改仍受版本号保护，存储加载不得覆盖）
        assert!(
            persist_tool_states_to_with_known(&store, known.clone())
                .await
                .is_err()
        );
        assert!(
            pending_persist_error_from(&store).await.is_some(),
            "persist failure must record a marker for the next mount"
        );
        assert_ne!(tool_state_service().dirty_version(), 0);
        // 存储中无禁用清单（失败未写入），加载旧值不会覆盖内存（dirty 保护）
        load_tool_states_from(&store).await.unwrap();
        assert!(!tool_state_service().is_enabled("date"));

        // 第二次落盘成功：清除失败标记并清脏
        persist_tool_states_to_with_known(&store, known)
            .await
            .unwrap();
        assert!(
            pending_persist_error_from(&store).await.is_none(),
            "successful persist must clear the failure marker"
        );
        assert_eq!(tool_state_service().dirty_version(), 0);
    }

    #[tokio::test]
    async fn mount_sync_rereads_stale_marker_when_no_task_in_flight() {
        // 回归（review Minor 1）：无在途落盘任务时，挂载同步必须重读存储
        // marker 作为权威值。修复前：首次读取 marker 恰在任务成功收敛前
        // 完成（读到陈旧失败标记）→ 随后任务删除 marker、清空 PERSIST_ERROR
        // 并收敛状态机到 IDLE → 挂载代码按"无在途 + 有 marker"用陈旧值
        // 置位，"保存失败"横幅在状态已成功落盘后长期残留（任务完成路径
        // 不再写信号，无自愈手段）。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MarkerStaleReadKvStore::new();
        // 无在途任务（挂载时刻任务已收敛）：重读权威值 → None（任务已删
        // marker），不得沿用首次读到的陈旧失败标记
        let pending = mount_persist_error_pending(&store, false).await;
        assert!(
            pending.is_none(),
            "idle mount must reread authoritative marker, got: {pending:?}"
        );
    }

    #[tokio::test]
    async fn mount_sync_keeps_marker_when_persist_really_failed() {
        // 反向边界：marker 真实存在（上次落盘确实失败）且无在途任务时，
        // 重读仍为 Some——挂载必须继续置位提示，重读逻辑不得误清真实失败
        // 提示（与"陈旧 marker 被任务删除"场景区分）。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        store.0.lock().unwrap().insert(
            TOOL_STATES_PERSIST_ERROR_KEY.to_string(),
            serde_json::json!("previous failure"),
        );
        let pending = mount_persist_error_pending(&store, false).await;
        assert_eq!(pending.as_deref(), Some("previous failure"));
    }

    #[tokio::test]
    async fn mount_sync_does_not_reread_when_task_in_flight() {
        // 在途任务存在时不得重读：任务可能即将写入失败 marker，重读会读到
        // 写入前的 None 而误清任务即将置位的失败信号（方向错误，宁少提示）。
        // 保持首次读取值（宁多提示，任务完成后收敛最终状态）。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MarkerStaleReadKvStore::new();
        let pending = mount_persist_error_pending(&store, true).await;
        assert_eq!(
            pending.as_deref(),
            Some("stale failure"),
            "in-flight mount must keep first read value without rereading"
        );
    }

    #[test]
    fn mount_sync_full_path_clears_stale_banner_via_signal() {
        // PERSIST_ERROR 是进程级 GlobalSignal，跨测试存活且被 views::tools
        // 的横幅测试同时读写（review 建议 2）：并行执行时交叉污染会导致
        // 偶发断言失败，须持 SIGNAL_TEST_LOCK 串行（与 tools.rs 同一把锁）。
        let _signal_guard = crate::test_shared::SIGNAL_TEST_LOCK.lock().unwrap();
        // 端到端（review Minor 1）：挂载流程（读 marker → 无在途重读权威值
        // → 按决策同步信号）在陈旧 marker 场景下最终清空 PERSIST_ERROR，
        // 而非置位假"保存失败"横幅。PERSIST_ERROR 是进程级 GlobalSignal，
        // 读写必须处于 dioxus runtime 内（tools.rs 同模式），故用 VirtualDom
        // 包裹并在闭包内 block_on 执行异步挂载逻辑。
        static CLEARED: AtomicBool = AtomicBool::new(false);
        static RESTORED: AtomicBool = AtomicBool::new(false);

        let mut dom = VirtualDom::new(|| {
            // 首次读取 marker 读到陈旧失败标记；无在途任务触发重读 → 权威
            // None → 同步清空 PERSIST_ERROR（修复前此处会置位假横幅）
            let store = MarkerStaleReadKvStore::new();
            futures::executor::block_on(async {
                sync_persist_error_on_mount_from(&store, "prefix", false).await;
            });
            if PERSIST_ERROR.read().is_none() {
                CLEARED.store(true, Ordering::SeqCst);
            }
            // 收尾（review m3）：PERSIST_ERROR 是进程级 GlobalSignal，跨
            // 测试存活。测试不得残留写入值——恢复初始值 None，避免污染后续
            // 断言信号初始状态的测试（cargo test 并行时顺序依赖会偶发失败）。
            *PERSIST_ERROR.write() = None;
            if PERSIST_ERROR.read().is_none() {
                RESTORED.store(true, Ordering::SeqCst);
            }
            rsx! {
                div {}
            }
        });
        dom.rebuild_in_place();

        assert!(
            CLEARED.load(Ordering::SeqCst),
            "stale banner must be cleared"
        );
        assert!(
            RESTORED.load(Ordering::SeqCst),
            "PERSIST_ERROR must be restored to its initial value"
        );
    }

    #[tokio::test]
    async fn panic_marker_recorded_readable_and_cleared_by_next_persist() {
        // 回归（review 第二轮 Minor 1）：落盘任务 panic 收敛状态机后须尽力
        // 写入失败 marker——视图重挂载时进程级 PERSIST_ERROR 信号被清空，
        // 未落盘切换（dirty != 0）若只靠信号提示会被静默遗忘（跨重启回滚
        // 无提示）。marker 与成功落盘形成闭环：下次 persist 成功即清除。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        let known: HashSet<String> = ["date".to_string()].into_iter().collect();

        record_persist_error_marker_from(&store, "persist task panicked").await;
        assert_eq!(
            pending_persist_error_from(&store).await.as_deref(),
            Some("persist task panicked"),
            "panic marker must survive for the next mount"
        );

        // 下一次成功落盘清除标记（与 Ok(Err) 失败路径共用闭环）
        tool_state_service().set_enabled("date", false);
        persist_tool_states_to_with_known(&store, known)
            .await
            .unwrap();
        assert!(
            pending_persist_error_from(&store).await.is_none(),
            "successful persist must clear the panic marker"
        );
    }

    #[tokio::test]
    async fn recover_persist_panic_writes_marker_and_converges_state() {
        // review 中等问题 1 修复：panic 恢复序列（写 marker + 收敛状态机）
        // 提取为可测函数后的端到端验证——模拟在途任务 panic 时刻（状态机
        // RUNNING + 未落盘修改），恢复后 marker 可被下次挂载读取、状态机
        // 回到 IDLE 使下次切换能重新 spawn 落盘任务。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        // 模拟在途任务 panic 时刻：状态机 RUNNING + 未落盘修改
        PERSIST_STATE.store(PERSIST_RUNNING, Ordering::SeqCst);
        tool_state_service().set_enabled("date", false);

        // 无挂起切换（状态 RUNNING 为本任务在途标记）→ 不请求补轮（#[must_use]
        // 契约，review 建议 6）
        let has_pending = recover_persist_panic_from(&store, "persist task panicked").await;
        assert!(
            !has_pending,
            "running-only converge must not request a retry round"
        );

        assert_eq!(
            PERSIST_STATE.load(Ordering::SeqCst),
            PERSIST_IDLE,
            "panic recovery must converge the state machine to idle"
        );
        assert_eq!(
            pending_persist_error_from(&store).await.as_deref(),
            Some("persist task panicked"),
            "panic marker must survive for the next mount"
        );
    }

    #[tokio::test]
    async fn recover_persist_panic_skips_marker_when_no_pending_changes() {
        // 无未落盘修改（dirty == 0）时 panic 恢复不写 marker：没有"切换未
        // 落盘"事实，写 marker 会造成宁多提示方向的跨挂载误报。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        PERSIST_STATE.store(PERSIST_RUNNING, Ordering::SeqCst);

        // 无未落盘修改且无挂起切换 → 不请求补轮（#[must_use] 契约，review 建议 6）
        let has_pending = recover_persist_panic_from(&store, "persist task panicked").await;
        assert!(
            !has_pending,
            "clean running-only converge must not request a retry round"
        );

        assert_eq!(PERSIST_STATE.load(Ordering::SeqCst), PERSIST_IDLE);
        assert!(
            pending_persist_error_from(&store).await.is_none(),
            "clean state must not record a failure marker"
        );
    }

    #[tokio::test]
    async fn recover_persist_panic_converge_reports_pending_for_retry() {
        // 回归（review 建议 1）：panic 恢复不得无条件丢弃收敛期间新产生的
        // 挂起切换——converge 报告 PENDING 供调用方补一轮落盘，否则该切换
        // 只在内存（dirty 未落盘）且无人消费（下次切换经 persist_on_toggle
        // 得 PENDING→PENDING，prev 非 IDLE 不 spawn 新任务），跨重启静默
        // 回滚。状态机仍无条件收敛到 IDLE：调用方不补轮也不残留无人消费的
        // PENDING（比丢弃切换更糟）；丢弃方向由 marker 提示兜底。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        let known: HashSet<String> = ["date".to_string()].into_iter().collect();
        // 模拟：任务 panic 的 marker await 期间用户切换（PENDING + 未落盘修改）
        PERSIST_STATE.store(PERSIST_PENDING, Ordering::SeqCst);
        tool_state_service().set_enabled("date", false);

        let has_pending = recover_persist_panic_from(&store, "persist task panicked").await;
        assert!(
            has_pending,
            "converge must report the pending switch so the caller can retry"
        );
        // 状态机已收敛到 IDLE：调用方不补轮也不残留无人消费的 PENDING
        assert_eq!(PERSIST_STATE.load(Ordering::SeqCst), PERSIST_IDLE);
        // marker 已写入（dirty != 0）：提示兜底存在
        assert!(pending_persist_error_from(&store).await.is_some());

        // 调用方补轮等价路径（tools.rs continue 后直接落盘，状态机 IDLE）：
        // 落盘最新内存快照（含 date 禁用）→ 清除 marker 与 dirty，闭环。
        persist_tool_states_to_with_known(&store, known)
            .await
            .unwrap();
        let prev = PERSIST_STATE.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |s| {
            Some(persist_on_round_done(s))
        });
        assert_eq!(prev, Ok(PERSIST_IDLE), "retry round must converge to idle");
        assert!(stored_disabled(&store.0).contains(&"date".to_string()));
        assert_eq!(tool_state_service().dirty_version(), 0);
        assert!(
            pending_persist_error_from(&store).await.is_none(),
            "successful retry must clear the failure marker"
        );
    }

    #[tokio::test]
    async fn recover_persist_panic_no_pending_returns_false() {
        // 对照：收敛时无挂起切换（状态 RUNNING——本任务在途标记）→ 返回
        // false，调用方不补轮；状态机 IDLE，后续切换正常 spawn。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();
        PERSIST_STATE.store(PERSIST_RUNNING, Ordering::SeqCst);

        let has_pending = recover_persist_panic_from(&store, "persist task panicked").await;
        assert!(
            !has_pending,
            "running-only converge must not request a retry"
        );
        assert_eq!(PERSIST_STATE.load(Ordering::SeqCst), PERSIST_IDLE);
    }

    #[tokio::test]
    async fn persist_snapshot_and_version_are_atomic_under_concurrent_toggle() {
        // review 中等问题 3：快照与版本号必须在同一读锁内采样。并发 toggle
        // 下二者要么都含变更、要么都不含——若分开采样（版本号先读、快照
        // 后读），toggle 插入两者之间会出现"快照含 date 而版本号仍为 0"的
        // 组合，mark_clean 拒绝清零致脏标记残留。多线程竞争下循环探测该
        // 非法组合：原子实现下不存在（读锁与写锁互斥，toggle 要么整体在
        // 采样前、要么整体在采样后）。
        let _guard = STATE_TEST_LOCK.lock().await;
        for _ in 0..100 {
            tool_state_service().reset_all();
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let (b1, b2) = (Arc::clone(&barrier), Arc::clone(&barrier));
            let reader = std::thread::spawn(move || {
                b1.wait();
                tool_state_service().snapshot_with_version()
            });
            let writer = std::thread::spawn(move || {
                b2.wait();
                tool_state_service().set_enabled("date", false);
            });
            let (snapshot, version) = reader.join().expect("reader thread panicked");
            writer.join().expect("writer thread panicked");
            assert!(
                !snapshot.contains(&"date".to_string()) || version != 0,
                "snapshot contains the toggle but version predates it: \
                 snapshot and version must be sampled under one read lock"
            );
        }
    }

    #[tokio::test]
    async fn tool_state_set_enabled_unchanged_keeps_dirty_version() {
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        // 状态实际变化才递增版本号（避免重复切换触发无意义持久化）
        tool_state_service().set_enabled("date", false);
        let version = tool_state_service().dirty_version();
        assert_eq!(version, 1);
        tool_state_service().set_enabled("date", false);
        assert_eq!(tool_state_service().dirty_version(), version);
        tool_state_service().set_enabled("date", true);
        assert_eq!(tool_state_service().dirty_version(), version + 1);
        // 已是启用状态再次置启用：不得递增
        tool_state_service().set_enabled("date", true);
        assert_eq!(tool_state_service().dirty_version(), version + 1);
    }

    #[tokio::test]
    async fn runtime_direct_mutation_counts_toward_dirty_via_observer() {
        // 修复回归（review P3-1）：Kernel 侧 ToolRuntime 直写共享集合时，
        // 经 share_disabled 注入的 dirty observer 统一递增版本号——否则
        // 存储加载会误以为无未落盘修改，用陈旧值覆盖直写结果。
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let mut runtime = ToolRuntime::new();
        runtime.share_disabled(
            tool_state_service().shared(),
            Some(tool_state_service().dirty_observer()),
        );

        runtime.set_tool_enabled("calculator", false);
        assert_eq!(tool_state_service().dirty_version(), 1);
        // 无实际变化不递增（与 ToolStateService::set_enabled 语义一致）
        runtime.set_tool_enabled("calculator", false);
        assert_eq!(tool_state_service().dirty_version(), 1);

        // 直写后存储加载被跳过：陈旧存储（禁用 date）不得覆盖 calculator
        let store = MemoryKvStore::new();
        store
            .0
            .lock()
            .unwrap()
            .insert(TOOL_STATES_KEY.to_string(), serde_json::json!(["date"]));
        load_tool_states_from(&store).await.unwrap();
        assert!(!tool_state_service().is_enabled("calculator"));
        assert!(tool_state_service().is_enabled("date"));
    }

    #[tokio::test]
    async fn tool_state_overlapped_persist_keeps_dirty_until_latest_flush() {
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        let store = MemoryKvStore::new();

        // 首次切换 → 版本 1；模拟落盘开始（记录落盘时版本号）
        tool_state_service().set_enabled("date", false);
        let first_flush = tool_state_service().dirty_version();
        assert_eq!(first_flush, 1);

        // 落盘期间用户又切换 → 版本 2；较早完成的落盘不得清除脏标记
        tool_state_service().set_enabled("calculator", false);
        tool_state_service().mark_clean(first_flush);
        assert_ne!(
            tool_state_service().dirty_version(),
            0,
            "stale flush must not clear dirty while newer edits are pending"
        );
        // 陈旧落盘后的存储加载仍不得覆盖内存
        load_tool_states_from(&store).await.unwrap();
        assert!(!tool_state_service().is_enabled("date"));
        assert!(!tool_state_service().is_enabled("calculator"));

        // 最新落盘（版本 2）完成后清除；此后存储值可安全回灌
        tool_state_service().mark_clean(2);
        assert_eq!(tool_state_service().dirty_version(), 0);
    }

    #[tokio::test]
    async fn tool_state_overlapped_persist_serialized_keeps_latest() {
        let _guard = STATE_TEST_LOCK.lock().await;
        tool_state_service().reset_all();
        // 慢速存储：第 1 次 set 慢（50ms）、后续快（10ms），复现写序颠倒——
        // 修复前较早发起的落盘晚完成会覆盖较新落盘，存储回退为陈旧值且脏
        // 标记已清除，下次 load 回灌用户切换被静默回滚（M1）。
        let store = SlowKvStore::new();
        let known: HashSet<String> = ["date".to_string(), "calculator".to_string()]
            .into_iter()
            .collect();

        tool_state_service().set_enabled("date", false); // 版本 → 1
        let (first, second) = tokio::join!(
            persist_tool_states_to_with_known(&store, known.clone()),
            async {
                // 等待第一个落盘进入写阶段（版本号已记录）后再切换
                tokio::time::sleep(Duration::from_millis(20)).await;
                tool_state_service().set_enabled("calculator", false); // 版本 → 2
                persist_tool_states_to_with_known(&store, known).await
            },
        );
        first.unwrap();
        second.unwrap();

        // 串行化后第二个落盘覆盖写最新快照：存储不得回退为陈旧值
        let stored: Vec<String> = store
            .map
            .lock()
            .unwrap()
            .get(TOOL_STATES_KEY)
            .and_then(|v| v.as_array().cloned())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            stored.contains(&"date".to_string()),
            "stale value rolled back: {stored:?}"
        );
        assert!(
            stored.contains(&"calculator".to_string()),
            "latest edit lost to stale overwrite: {stored:?}"
        );
        // 最新落盘已清除脏标记：存储可安全回灌
        assert_eq!(tool_state_service().dirty_version(), 0);
        tool_state_service().reset_all();
        load_tool_states_from(&store).await.unwrap();
        assert!(!tool_state_service().is_enabled("date"));
        assert!(!tool_state_service().is_enabled("calculator"));
    }

    #[test]
    fn storage_encryption_key_parser_accepts_exact_256_bit_hex() {
        let key = parse_storage_key_hex(
            "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f",
        )
        .unwrap();
        assert_eq!(key[0], 0x00);
        assert_eq!(key[15], 0x0f);
        assert_eq!(key[31], 0x1f);

        let uppercase = parse_storage_key_hex(&"AB".repeat(32)).unwrap();
        assert!(uppercase.iter().all(|byte| *byte == 0xab));
    }

    #[test]
    fn storage_encryption_key_parser_rejects_malformed_or_wrong_size_values() {
        for value in ["", "aa", &"a".repeat(63), &"g0".repeat(32)] {
            let error = parse_storage_key_hex(value).unwrap_err();
            assert!(error.contains(STORAGE_KEY_ENV));
            // A validation error must never echo the supplied secret.
            if !value.is_empty() {
                assert!(!error.contains(value));
            }
        }
    }
}

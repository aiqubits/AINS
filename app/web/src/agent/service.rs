//! Agent 装配层（Phase 6.1/6.2）：AgentKernel + ToolRuntime + 存储 + 桥接通道。
//!
//! 平台差异集中在本文件的 `cfg` 分支：RuntimeAdapter、KvStore 后端与
//! 工具集；桥接协议（channel 结构）双端一致。desktop 端经 `#[path]`
//! 引用本文件复用同一实现。

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

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

use super::permission_bridge::{
    InteractionMsg, PermissionPromptMsg, UiInteraction, UiPermissionPrompt,
};

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

/// Tool 面板的轻量入口（Phase 6.7）：仅构造 ToolRuntime 取（name,
/// description, category）快照，不装配 Kernel/会话恢复/权限通道。
pub fn tool_schema_snapshot() -> Vec<(String, String, ToolCategory)> {
    let engine = PermissionEngine::new(PermissionMode::Default, PermissionSettings::default());
    let (interaction, _interaction_rx) = UiInteraction::channel();
    let mut runtime = ToolRuntime::new();
    // The snapshot never executes tools, but pass a real workspace when it is
    // available so the registered background-task schema has the same safe
    // construction path as a live agent session.
    let schema_workspace = std::env::current_dir().ok();
    register_tools(
        &mut runtime,
        &engine,
        interaction,
        &SandboxPolicy::default(),
        schema_workspace.as_deref(),
    );
    runtime
        .api_schemas()
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
}

/// 装配 Agent 会话。`client` 由宿主提供（Web 复用已认证的 AuthState
/// client；Desktop 从环境变量构造，见各端视图）。
// 双端统一用 Arc（native 多线程需要；wasm 单线程下非 Send 无害）
#[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
pub async fn initialize(client: Client) -> Result<AgentBridge, String> {
    let kv = open_kv_store().await?;
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

    let mut runtime = ToolRuntime::new();
    register_tools(
        &mut runtime,
        &engine,
        interaction,
        &policy,
        Some(std::path::Path::new(&cwd)),
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

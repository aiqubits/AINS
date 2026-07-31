//! Agent 装配层（Phase 6.1/6.2）：AgentKernel + ToolRuntime + 存储 + 桥接通道。
//!
//! 平台差异集中在本文件的 `cfg` 分支：RuntimeAdapter、KvStore 后端与
//! 工具集；桥接协议（channel 结构）双端一致。desktop 端经 `#[path]`
//! 引用本文件复用同一实现。

use std::sync::Arc;

use futures::channel::mpsc;

use agent_core::context::session::{SessionSaveInput, SessionStore};
use agent_core::kernel::{
    AgentEvent, AgentKernel, AgentKernelConfig, ConversationMessage, StreamEvent,
};
use agent_core::memory::{KvStore, MemdirStore};
use agent_core::model_client::UsageSnapshot;
use agent_core::model_service::GatewayModelClient;
use agent_core::policy::{PermissionEngine, PermissionMode, PermissionSettings};
use agent_core::skills::KvSkillStore;
use agent_core::tools::compute::{CalculatorTool, DateTool, JsonTool, MarkdownTool, TextTool};
use agent_core::tools::interact::{
    AskUserQuestionTool, EnterPlanModeTool, ExitPlanModeTool, TodoWriteTool,
};
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
        use agent_core::memory::RedbKvStore;
        let path = native_data_path()?;
        let store: Arc<dyn KvStore> = Arc::new(
            RedbKvStore::open(&path).map_err(|e| format!("redb {}: {e}", path.display()))?,
        );
        *cache = Some(Arc::clone(&store));
        Ok(store)
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
    register_tools(&mut runtime, &engine, interaction);
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
fn bridge_cwd() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        "/ains-web".to_string()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| ".".to_string())
    }
}

/// 注册平台工具集（Web 通用集；Native 追加文件/系统工具）。
fn register_tools(
    runtime: &mut ToolRuntime,
    engine: &Arc<PermissionEngine>,
    interaction: Arc<UiInteraction>,
) {
    runtime.register(Box::new(CalculatorTool));
    runtime.register(Box::new(JsonTool));
    runtime.register(Box::new(TextTool));
    runtime.register(Box::new(MarkdownTool));
    runtime.register(Box::new(DateTool));
    runtime.register(Box::new(TodoWriteTool));
    runtime.register(Box::new(AskUserQuestionTool::new(Some(interaction))));
    runtime.register(Box::new(EnterPlanModeTool::new(Arc::clone(engine))));
    runtime.register(Box::new(ExitPlanModeTool::new(Arc::clone(engine))));
    runtime.register(Box::new(WebFetchTool));

    #[cfg(not(target_arch = "wasm32"))]
    {
        use agent_core::policy::NoopSandbox;
        use agent_core::tools::filesystem::{
            FileEditTool, FileReadTool, FileWriteTool, GlobTool, GrepTool,
        };
        use agent_core::tools::system::{
            ClipboardTool, NotificationTool, ScreenshotTool, ShellCommandTool,
        };
        runtime.register(Box::new(FileReadTool));
        runtime.register(Box::new(FileWriteTool));
        runtime.register(Box::new(FileEditTool));
        runtime.register(Box::new(GlobTool));
        runtime.register(Box::new(GrepTool));
        // Shell 必经 Sandbox；NoopSandbox 占位下拒绝执行（Phase 7.1 替换）
        runtime.register(Box::new(ShellCommandTool::new(Arc::new(NoopSandbox))));
        // 平台集成注入随 Phase 6.10/7 落地，当前 None → 工具自报不可用
        runtime.register(Box::new(ClipboardTool::new(None)));
        runtime.register(Box::new(NotificationTool::new(None)));
        runtime.register(Box::new(ScreenshotTool::new(None)));
    }
}

/// 装配 Agent 会话。`client` 由宿主提供（Web 复用已认证的 AuthState
/// client；Desktop 从环境变量构造，见各端视图）。
// 双端统一用 Arc（native 多线程需要；wasm 单线程下非 Send 无害）
#[cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
pub async fn initialize(client: Client) -> Result<AgentBridge, String> {
    let kv = open_kv_store().await?;
    let session_store = Arc::new(SessionStore::new(Arc::clone(&kv)));

    let engine = PermissionEngine::new(PermissionMode::Default, PermissionSettings::default());
    let (prompt, permission_rx) = UiPermissionPrompt::channel();
    let (interaction, interaction_rx) = UiInteraction::channel();

    let mut runtime = ToolRuntime::new();
    register_tools(&mut runtime, &engine, interaction);
    let runtime = runtime.with_permissions(Arc::clone(&engine), Some(prompt));

    let cwd = bridge_cwd();
    let config = AgentKernelConfig {
        cwd: cwd.clone().into(),
        ..AgentKernelConfig::default()
    };
    let model = GatewayModelClient::<Rt>::shared(client);
    let (mut kernel, event_tx, stream_rx) = AgentKernel::<Rt>::with_runtime(model, runtime, config);

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

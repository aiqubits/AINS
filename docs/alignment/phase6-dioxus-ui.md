# Phase 6 对齐清单：Dioxus UI 集成（全部任务 6.1–6.12）

对齐基线：`OpenHarness/frontend/terminal/`（React 终端 UI 的交互语义：流式
消息、工具调用卡片、权限确认、模式切换）与 `OpenHarness/src/openharness/`
（`engine/stream_events.py`、`permissions/`、`skills/`）。范围：Phase 6 的
P0 任务——agent-core 嵌入 Web/Desktop（6.1/6.2）、Chat 对话视图（6.3）、
Skills 管理面板（6.4）、权限交互 UI（6.11）。P1/P2 任务（6.5–6.10、6.12）
不在本批次。

本地验收：`agent-core`/`ui`/`web`/`desktop`/`i18n` 双 target（native +
wasm32，desktop/mobile 仅 native）build/clippy(-D warnings) 全通过；
`cargo build -p agent-core -p web --target wasm32-unknown-unknown` 通过；
mobile native build 通过（NavKey 扩展无回归）。Native 测试全过：agent-core
430（lib 281 + 集成 149，含新增 `tests/skills_store.rs` 15）；web 桥接/映射
单测（`agent::view_model` 18 含 ConversationMirror/mask + `agent::permission_bridge`
异步契约）；desktop 经 `#[path]` 复用同一套；i18n/ui/client-api 既有套件零
回归。Web 端 `tests/web_skills.rs`（IndexedDB 契约）随既有 wasm 套件走 CI
wasm-pack 浏览器测试。用例数随评审回归演进，以 `cargo test` 为准。

> **Code Review 修正（一轮）**：(1) 会话镜像补齐 `tool_result` 消息
> （`ConversationMirror`），避免 `sanitize_conversation_messages` 丢弃未配对
> `tool_use` 的中间 assistant 轮；(2) 聊天工具卡片输入预览与权限弹窗共用
> `mask_sensitive`（消除敏感值经工具卡片旁路外泄）；(3) `SkillLoader::load`
> 补完整性门控（无 meta/checksum 不匹配拒绝，堵经名称旁路加载篡改内容）；
> (4) Native KV `OnceLock` 仅缓存成功句柄（瞬时打开失败可重试）；(5) 技能
> 列表存储读错误上报而非误标损坏。

## 架构总览

| 决策 | 说明 |
|---|---|
| `app/ui` 保持纯展示层 | 新组件（chat_view/permission_dialog/permission_controls/skills_panel）仅依赖 dioxus + i18n，全部 props 驱动；视图模型（`ChatItem`/`PermissionRequestView`/`SkillCard` 等）在 ui 内定义，**不引入 agent-core 依赖**。宿主负责 `StreamEvent`/`PermissionRequest` → 视图模型映射 |
| 宿主 `agent/` 桥接模块 | `app/web/src/agent/{service,view_model,permission_bridge}.rs` 装配 `AgentKernel::with_runtime`（完整 ToolRuntime + PermissionEngine + 确认回调），持 `event_tx`/`stream_rx`，泵入 Dioxus Signal |
| Desktop 复用 | `app/desktop/src/agent.rs` 与 `views/{agent_chat,skills}.rs` 经 `#[path]` 直接引用 web 端文件，双端同构；平台差异集中在 `service.rs` 的 `cfg` 分支 |
| 视图解耦 | `agent_chat`/`skills` 视图只依赖 `crate::agent::*` 与 `use_context::<Client>()`（不依赖 web 专有 `AuthState`），故可被 desktop `#[path]` 复用；Client 上下文由各端 App 层提供（web 复用 AuthState client，desktop 从 `AINS_API_URL` 构造） |

## 1. agent-core 嵌入 Web（6.1，`app/web/src/agent/`）

| 能力点 | 设计 | AINS | 结论 |
|---|---|---|---|
| 依赖装配 | agent-core 入 web crate | `app/web/Cargo.toml` 新增 `agent-core`/`futures`/`async-trait`/`tracing`/`serde_yaml`；双 target 编译通过 | 对齐 |
| ModelClient | 复用 5.1 传输层 | `GatewayModelClient::<WasmRuntimeAdapter>::shared(client)`，client 取自 App 层 `use_context::<Client>()`（AuthState client，token 跨 clone 共享） | 对齐 |
| 工具集（Web） | compute 全套 + interact + web_fetch | Calculator/Json/Text/Markdown/Date + TodoWrite/AskUserQuestion/Enter・ExitPlanMode + WebFetch | 对齐 |
| 存储 | IndexedDB 供会话持久化 | `IndexedDbKvStore::open("ains-agent")`；进程内 thread_local 缓存单例（/agent 与 /skills 视图共用一句柄） | 对齐 |
| 生命周期 | spawn kernel.run + 泵 stream | `use_future` 内 `spawn(kernel.run())` + 三条泵协程（stream/权限/ask）；组件卸载随 scope 取消，`event_tx` 释放后 Kernel 事件循环优雅退出 | 对齐 |
| 输入 | `AgentEvent::UserMessage` | `ChatInput on_send` → `event_tx.send(UserMessage)` | 对齐 |
| 会话恢复 | 刷新后从 IndexedDB 续 | 启动 `load_latest(cwd)` 种子进 `kernel.context_mut().conversation` + `seed_history` 首屏渲染；恢复失败仅告警不阻断新会话 | 对齐 |
| 路由 | `/agent` | main.rs 受保护路由新增 `AgentChat`；Sidebar 新增「AI Agent」组（所有角色可见），`NavKey::AgentChat` | 对齐 |

## 2. agent-core 嵌入 Desktop（6.2，`app/desktop/`）

| 能力点 | 设计 | AINS | 结论 |
|---|---|---|---|
| 依赖/复用 | Native 桥接 | Cargo.toml 新增 agent-core/client-api/tokio(rt-multi-thread) 等；`agent.rs` + views 经 `#[path]` 复用 web 实现 | 对齐 |
| RuntimeAdapter | Tokio | `service::Rt = TokioRuntimeAdapter`（cfg 选择） | 对齐 |
| 存储 | redb 落数据目录 | `RedbKvStore::open`，路径 `AINS_DATA_DIR` 优先，回退 `$HOME/.ains/agent.redb`；`OnceLock` 进程单例（redb 单进程独占锁） | 对齐 |
| 工具集（Native 追加） | file/glob/grep/系统/Shell | FileRead/Write/Edit + Glob/Grep + Clipboard/Notification/Screenshot（集成注入暂 None → 自报不可用）+ ShellCommand（经 `NoopSandbox`） | 对齐 |
| Shell 占位 | Sandbox 未就位拒绝 | ShellCommandTool 经 NoopSandbox，占位下返回"无操作权限"，不直接 `std::process::Command`（**偏差：Phase 7.1 平台沙箱替换**） | 偏差（占位） |
| Client | 环境变量构造 | `AINS_API_URL` 回退 `http://127.0.0.1:8080`，`with_max_retries(0)`（避免与 GatewayModelClient 流式重试叠加） | 对齐 |
| 路由 | /agent、/skills | main.rs Route 新增 + Navbar 链接；App 层 `use_context_provider(make_client)` | 对齐 |

## 3. Chat 对话视图（6.3，`app/ui/src/chat_view.rs` + `app/web/src/agent/view_model.rs`）

| 能力点 | 基线 `stream_events.py` / 终端 UI | AINS | 结论 |
|---|---|---|---|
| 视图模型 | 消息 + 工具卡片 + 状态行 | `ChatItem::{Text,ToolCall,StatusNote,CompactNote,ErrorNote}` + `ChatViewState{items,streaming_text,busy}` | 对齐 |
| 组件 | 列表 + 流式尾部 + 输入 | `ChatView`（自动滚到底，与 CodeConsole 同 `document::eval` 模式）+ `ChatInput`（多行 + Enter 发送/Shift+Enter 换行 + 停止）+ `ToolCallCard`（折叠展开输入/输出预览） | 对齐 |
| delta 累积 | `AssistantTextDelta` 追加 | `apply_stream_event` 累积 `streaming_text`；`AssistantTurnComplete` 以完整消息文本落定（抗 delta 丢失）并清空尾部 | 对齐 |
| 工具卡片配对 | Started→Completed | Started 创建 Running 卡片；Completed 与**最早的同名 Running** 卡片配对，乱序/无配对完成容错忽略；`is_error` → Failed 态 | 对齐 |
| 错误分级 | recoverable / fatal | `Error{recoverable}` → `ErrorNote`；不可恢复清 busy/streaming；可恢复保留会话（文案区分「可恢复错误」/「会话已失败」） | 对齐 |
| 压缩进度 | CompactProgressEvent | `CompactProgress{phase}` → `CompactNote`（phase 取基线阶段字面量，tf 注入文案） | 对齐 |
| 历史恢复渲染 | — | `seed_history`：Text 落条目、ToolUse 与 ToolResult 按 id 配对成已完成卡片、Image/裸 ToolResult 不单独渲染 | AINS 扩展 |
| 会话镜像 | 快照须含 tool_result | `ConversationMirror` 据流事件补齐 `tool_result` 用户消息（按 tool_use 出现序、同名 FIFO 配对），使快照经 `sanitize_conversation_messages` 不丢弃中间工具轮 assistant 消息（Code Review 修正） | 对齐（修正） |
| 参数脱敏一致性 | UI 展示前掩码 | 工具卡片 input_preview（Started/seed_history）与权限弹窗共用 `view_model::mask_sensitive`（Code Review 修正，消除卡片旁路外泄） | 对齐（修正） |
| 忙碌复位 | 查询结束 | `settle_idle`：无 Running 工具且无 streaming 时复位 busy（final turn = 无 tool_use 的 turn） | 对齐 |
| i18n | — | 中英文案入 `crates/i18n/src/translate_fields.rs`（chat_/perm_/skills_/ask_user_ 等 57 字段）；计数断言随后续批次/修正演进（最终 546，以 `translations.rs` 断言为准） | 对齐 |

## 4. Skills 管理面板（6.4，`crates/agent-core/src/skills/store.rs` + `app/ui/src/skills_panel.rs`）

| 能力点 | 基线 `skills/` 6.1 存储节 | AINS | 结论 |
|---|---|---|---|
| 存储后端 | KvStore（Native redb/Web IndexedDB） | `KvSkillStore<KvStore>`：`skills:{name}` 原文 + `skills_meta:{name}` 元数据；双端复用同实现 | 对齐 |
| SkillMeta | category/requires_tools/platforms/trust_level/creator/created_at/permissions/checksum | 同字段 + 冗余 `description`（列表免逐条解析全文）；`SkillTrust{System,Trusted,Generated,Temporary}` | 对齐（+description） |
| 元数据序列化 | 计划写 `bincode(SkillMeta)` | **偏差**：meta 以 JSON `Value` 写入（KvStore 信封本身已 bincode，Value 反序列化需 deserialize_any，双重 bincode 不可行；与 kv.rs 载荷策略一致） | 偏差（信封已 bincode） |
| checksum 完整性 | 内容校验 | `skill_checksum`=sha256(SKILL.md hex)；`list_entries` 校验，不匹配/meta 缺失/原文缺失 → `corrupted=true`**标记而非静默跳过** | 对齐 |
| 渐进式 Level 0/1 | list/load | `SkillLoader::list`（门控过滤 + 排除损坏）/`load`（frontmatter YAML 解析 + body）；`split_frontmatter` 处理未闭合/行中 `---`/末尾无换行边界 | 对齐 |
| load 完整性门控 | 损坏不可注入 | `load` 与 list 同口径校验：无 meta 或 checksum 不匹配返回 InvalidFormat，堵经名称旁路加载已篡改内容（Code Review 修正） | 对齐（修正） |
| 列表读错误处理 | — | `list_entries` 对存储读错误 `?` 上报（仅内容缺失/非字符串载荷才标损坏），避免瞬时读错误把健康 skill 误标为可删除损坏条目（Code Review 修正） | 对齐（修正） |
| Level 2 引用文件 | load_reference | **偏差**：返回 NotFound，随 Phase 6.8 落地 | 偏差（后置 6.8） |
| 门控 | platform + requires_tools | `SkillContext{platform,available_tools}`：平台匹配（空=全平台）∧ 依赖工具全可用 | 对齐 |
| SkillManage | create/update/rollback/delete | 仅 `delete_skill` 实现（双 key 原子清除，不存在报 NotFound，损坏条目/孤儿 meta 可删）；create/update **偏差后置 6.8**、rollback **后置 6.9**（调用返回显式错误，不静默成功） | 对齐（delete）/ 偏差（后置） |
| 面板 | 浏览/删除，无导入 | `SkillsPanel`：卡片列表（trust 徽标 + 元信息）+ 详情抽屉（frontmatter + body 只读）+ 删除二次确认；**无导入/上传入口**（仅 Agent 自主创建）；损坏条目仅可删除不可查看 | 对齐 |
| 名称校验 | — | `validate_name`：非空、无首尾空白、拒 `/ \ :` 与控制字符（key 注入防护） | AINS 扩展 |
| 内容上限 | — | `MAX_SKILL_CONTENT_BYTES=256KiB`（防超大条目撑爆面板/上下文） | AINS 扩展 |

## 5. 权限交互 UI（6.11，`app/ui/src/permission_{dialog,controls}.rs` + `app/web/src/agent/permission_bridge.rs`）

| 能力点 | 基线 `permissions/` | AINS | 结论 |
|---|---|---|---|
| 三态确认弹窗 | 允许/总是允许/拒绝 | `PermissionDialog` 展示 tool_name/reason/resolved_file_path/command/参数摘要 + 三按钮；关闭/背板点击 = 拒绝（fail-closed，`disable_backdrop` 强制显式选择） | 对齐 |
| 桥接回调 | `PermissionPrompt::confirm` | `UiPermissionPrompt`（实现 `PermissionPrompt`）：请求 + oneshot 回执推入 unbounded channel，UI 点击回填 `PermissionReply::{Allow,AlwaysAllow,Deny}` | 对齐 |
| fail-closed | 拒绝优先 | channel 关闭（UI 协程退出）/ oneshot 被 drop（弹窗销毁）/ 显式 Deny 均归一为 `Deny` | 对齐 |
| FIFO 串行化 | 并发工具逐个确认 | `confirm` 内 async `Mutex` 串行化：上一弹窗未答复前不投递下一个（引擎侧已顺序化独占资源，桥接仅保证一次一窗）；单测验证第二请求等待第一回执 | 对齐 |
| 敏感字段脱敏 | UI 展示前掩码 | `mask_sensitive`：key 含 token/password/passwd/secret/authorization/api_key/apikey/credential（大小写不敏感、子串、递归对象/数组）→ 值替换 `***`；单测覆盖嵌套与大小写 | 对齐 |
| 模式切换器 | default/plan/full_auto | `PermissionModeSwitcher` 三段选择器；full_auto（放宽安全边界方向）内联二次确认条后才切换 | 对齐 |
| 模式回读 | enter/exit_plan_mode 改写模式 | 桥接层每次 stream 事件后读 `engine.mode()` 刷新 Signal（不新增事件面）；`PlanModeIndicator` Plan 态常驻徽标 | 对齐（选定回读方案） |
| ask_user_question | 交互回调回 UI | `UiInteraction`（实现 `UserInteraction`）：同 channel+oneshot+Mutex 模式；UI 弹 Modal 输入答案，关闭返回空串 | 对齐 |

## 6. 有意偏差与后置项汇总

| 项 | 处置 |
|---|---|
| Shell 经 NoopSandbox 拒绝执行 | Phase 7.1 平台沙箱替换（Linux namespace/seccomp、macOS sandbox-exec、Windows Job Object） |
| SkillManage.create/update_skill | 随 Phase 6.8（渐进式加载 + 门控整体交付） |
| SkillManage.rollback_skill | 随 Phase 6.9（版本链 + 回滚机制） |
| SkillLoader.load_reference（Level 2） | 随 Phase 6.8（引用文件面） |
| SkillMeta 用 JSON 而非 bincode | KvStore 信封已 bincode，Value 双重 bincode 不可行（与 kv.rs 载荷策略一致） |
| 查询中断（stop 按钮） | Kernel 暂无查询中断 API，`on_interrupt` 仅复位 UI 忙碌位；后续流事件仍正常渲染。真正的中断随 Kernel 中断接口落地 |
| 系统集成（Clipboard/Notification/Screenshot） | 注入 None，工具自报不可用；平台集成随 Phase 6.10/7 |
| Web 端非 Send Arc | `service.rs` wasm 分支 `allow(clippy::arc_with_non_send_sync)`（单线程 wasm 无害，双端统一 Arc 便于代码复用） |
| Native KV 单例缓存 | `OnceLock` 仅缓存成功句柄（redb 单进程独占锁）；瞬时打开失败不永久缓存，重访路由可重试（Code Review 修正） |
| 会话镜像 tool_result 补齐 | 视图层据流事件重建，非直接读 Kernel 对话（Kernel 已 move 入 spawn）；同名并发工具按 FIFO 配对，result_metadata 置 Null（不影响 sanitize 配对） |

## 7. P1/P2 批次（6.5–6.10、6.12，2026-07-30 追加）

| 任务 | 实现 | 验证 |
|---|---|---|
| 6.5 Agent 状态指示器 | `ui/agent_status.rs`：Idle/Thinking/RunningTools/Compacting/Error 五态圆点+文案（进行态脉冲动画）；宿主从流事件派生状态 | 浏览器实测：/agent 顶部右侧「空闲」常驻 |
| 6.6 Memory 浏览器 | `ui/memory_viewer.rs` + `web/views/memory.rs`：memdir `scan(500)` 卡片列表 + 详情抽屉（重要度徽标/标签/正文）；/memory 路由 + 侧边栏「记忆库」 | 浏览器实测：标题/副标题/空态渲染 |
| 6.7 Tool 执行面板 | `ui/tool_panel.rs` + `web/views/tools.rs`：`service::tool_schema_snapshot()`（仅构造 ToolRuntime 取 `api_schemas()` 快照）渲染真实工具名+描述；/tools 路由 + 侧边栏「工具面板」 | 浏览器实测：10 个运行时工具全部展示 |
| 6.8 Skills 渐进式加载+门控 | `create_skill`（v1.0 Active，frontmatter 提取元数据，trust=Generated）/`update_skill`（minor bump，旧版 Deprecated）；Level 2 `load_reference`/`put_reference`（`skills_ref:` key）；门控已在 `SkillLoader::list`（platform ∧ requires_tools） | native 18 + wasm 契约测试 |
| 6.9 清理与回滚 | 版本化存储（`skills_ver:`/`skills_head:`，`SkillVersion` v{major}.{minor}，链只增不删）；`rollback_skill`（目标提升为新大版本，限保留范围）；`SkillPruner`（最近 3+Golden+活跃版，评分升序淘汰）；`record_outcome` 连续失败≥5 且存在更优版自动回滚；面板详情展示 Active 版本+回滚按钮 | 专项测试：版本链/自动回滚/保留集/引用文件 |
| 6.10 Mobile 适配 | 既有 sidebar 抽屉+overlay；新增 chat/tools/memory CSS `@media(max-width:640px)`（单列网格、消息 90% 宽、发送钮纯图标） | CSS 媒体查询（随 release 构建交付） |
| 6.12 Slash Command + todo | `ChatInput` 增 `slash_commands`（`/` 前缀过滤建议下拉）；`/skill <name>` 加载 SKILL.md 全文作为指令发送（失败 warning toast）、`/help` 提示；`ui/todo_list.rs` + `parse_todo_markdown`（`- [ ]`/`- [x]`）从 todo_write 输出同步，有条目时展示于输入区上方 | 浏览器实测：`/` 弹出命令下拉；解析器单测 |

新增 i18n 24 字段；后续权限模式提示段与评审修正追加后最终计数断言 546。desktop 经 `#[path]` 同步获得 Memory/Tools 视图与全部新组件。
截图：`.devtmp/p6_tools_panel.png`、`.devtmp/p6_slash_commands.png`（含状态指示器/四项侧边栏/命令下拉）。

> **Code Review 修正（二轮，全量 Phase 6 评审，无阻断项）**：
> (1) 自动回滚防抖动——候选内容与当前活跃版 checksum 相同时跳过（否则
> 新版评分归零后每 5 次失败会无限重升同一内容），含回归测试；
> (2) `promote_version`/`create_skill` 写入顺序按“失败危害最小”重排
> （新版→镜像→头→旧版降级），`list_entries` 补 `skills_head:` 孤儿暴露，
> 部分写入中断后条目始终可见可删；(3) `/skill` 严格 token 匹配
> （`/skills x`、`/skillet` 不再误识）；(4) Tool 面板改用轻量
> `tool_schema_snapshot()`（不再为取 schema 装配整个 Kernel/会话恢复）；
> (5) `/help` 列出全部命令，移除未实现的 `chat_slash_clear` 文案；
> (6) desktop `make_client` 对非法 `AINS_API_URL` 告警回退默认值而非 panic。
>
> **有意后置（评审确认）**：`record_outcome` 与 `SkillPruner` 当前仅由
> 测试调用——技能执行评分/会话后清理的运行时接线依赖 skill-runtime
> 把技能调用事件化（Agent 经 /skill 或自主加载后的成败判定），随
> Phase 7+ 技能执行回路落地；存储层 API 已就绪并经契约测试锁定。
>
> **后置项回收：权限模式提示段已接线（2026-07-30）**：Kernel 每轮构建
> ModelRequest 时动态拼接 `permission_mode_section(engine.mode())`（宿主基础
> 提示可选在前）——模式会经 UI 开关或 enter/exit_plan_mode 中途改变，
> 因此不能固化在 config；Plan 下模型事先收到“勿调写工具”指引，减少
> “试错→被拒→再退出”轮次（权限引擎仍为硬边界）。Kernel 集成测试
> `system_prompt_carries_live_permission_mode_section` 验证首轮 Default 段、
> enter_plan_mode 后次轮 Plan 段。完整 prompt pipeline（技能索引/记忆段/
> 项目指令）仍为宿主后置装配项。
>
> **Code Review 修正（三轮，main 全量复评，无阻断项）**：
> (1) 初始化未完成时发送消息不再静默丢失（warning toast 提示初始化中/
> 失败原因）；(2) `delete_skill` 前缀删除先于 NotFound 判定无条件执行，
> create 中断仅残 `skills_ver:` 孤儿时可回收不泄漏（含回归测试）；
> (3) 面板活跃版本改读头指针权威源 `active_version()`（promote 容忍的
> 瞬态双 Active 记录不再误显旧版）；(4) 泵协程退出 toast 推送时取当前
> 语言而非挂载时快照；(5) desktop `make_client` 回退告警改 stderr
> （无 tracing subscriber 也可诊断）。已评估不修：`/skill` 恢复历史渲染
> 完整插值 prompt（镜像保真优先，外观差异，待后续 chip 化）。
>
> **Code Review 修正（四轮，全量 Phase 6 复评，无阻断项）**：
> (1) 中断按钮同步复位状态指示器（此前仅复位 busy，头部持续显示
> 进行态脉冲）；(2) `mask_sensitive` 补字符串值内嵌秘钥的模式掩码
> （Bearer token / sk- 长密钥 / URL userinfo，仅掩秘钥本体保留命令
> 结构可审阅，含单测）；(3) 镜像 user 消息改为 `tx.send` 成功后写入，
> 发送失败 toast 提示（`agent_send_failed`）而非快照残留 Kernel 未见
> 消息；(4) desktop `make_client` 支持 `AINS_API_TOKEN` 注入认证，
> 未设置时 stderr 告警（桌面登录流前的过渡方案）；(5) `record_outcome`
> 补并发语义文档（读-改-写非原子，Phase 7+ 接线时需按技能名串行）。
> 新增回归测试：prune 后过期候选回滚报错路径、含悬空 tool_use 快照
> 的存→载→续问端到端、值级掩码模式用例。
>
> **Code Review 修正（五轮，全量 Phase 6 复评，无阻断项）**：
> (1) 权限弹窗 `command` 字段同经 `mask_embedded_secrets` 值级掩码
> （此前 Arguments 块已掩的秘钥在 Command 行明文旁路）；(2) 发送
> 失败时 `retract_last_user` 从尾部回收可见转写中未送达的消息（与
> 镜像/持久历史一致；尾部匹配防流事件插入误删）；(3) Bearer 掩码
> 改为仅掩词内 token 连续段（字符集含 JWT/base64 的 `.+/=`），首尾
> 引号/括号保留。**评估不修**：工具输出预览与镜像 `tool_result` 不掩
> 码——镜像内容须忠实于内核实际对话（掩码会污染恢复后回喂模型的
> 历史），且只掩输出预览而不掩 assistant 正文属半套措施；完整输出侧
> 脱敏策略归 Phase 7.5 隐私审计。新增测试：command 掩码断言、多字节
> 紧邻掩码边界、retract 尾部匹配用例。

## 8. 里程碑验收

- [x] `cargo clippy -p agent-core -p ui -p web -p desktop -p i18n --all-targets -- -D warnings` 通过
- [x] `cargo clippy -p agent-core -p web --target wasm32-unknown-unknown -- -D warnings` 通过
- [x] `cargo build -p agent-core -p web --target wasm32-unknown-unknown` 通过；mobile native build 通过
- [x] `cargo test`：agent-core 430 + client-api 147 + web/desktop/ui/i18n 全过，零回归（含 CodeReview 一轮修正回归）
- [ ] wasm-pack CI 浏览器测试（`tests/web_skills.rs` 等）：CI-only，推送后确认
- [x] Web 浏览器 E2E（真实运行）：release 构建经 Docker（`ains-web` 镜像 + 我的 dist）发布于公网 8099，`browser-use` 实测——登录（WeChat 关闭后仅邮箱+密码）→ `/agent` 渲染 ChatView + 权限模式切换器；发送消息经内嵌 Kernel→`GatewayModelClient`→实时 AI 网关（真实返回 `HTTP 403 no_active_plan`，前端归类为「可恢复错误」）；全自动模式二次确认弹出；`/skills` 渲染面板（"不提供导入入口" + 空态）。截图见 `.devtmp/p6_*.png`（login/agent_view/agent_conversation/fullauto_confirm/skills_view）。
- [x] Desktop：`cargo build -p desktop` + clippy 通过；视图/桥接经 `#[path]` 与 Web 完全同构（Web 已浏览器实测），desktop 为原生应用不适用浏览器测试。
- 备注：为使登录免验证码，将 `ains-server` 以 `AINS_WECHAT__ENABLED=false`（compose 默认值）重建；`.env` 未改动，`docker compose up -d --no-deps --no-build ains-server` 可恢复原状态。验证用容器 `ains-p6`（`docker rm -f ains-p6` 清理）。

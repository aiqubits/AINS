# Phase 1 对齐清单：Agent Kernel + FSM

对齐基线：`OpenHarness/src/openharness/engine/`（`query.py` / `messages.py` /
`stream_events.py`）与 `api/client.py`（提交时仓库内版本）。
范围：Phase 1.1–1.9（AgentState FSM、消息 sanitize、StreamEvent、AgentKernel
主循环、tool_metadata 状态袋、ContextStore、Mock ModelClient、单元/集成测试）。

## 1. AgentState + FSM（`kernel/state.rs` + `kernel/fsm.rs` ↔ `engine/query.py` 隐式状态）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 状态建模 | 无显式 FSM，`query()` 生成器控制流隐式表达（等待输入→请求模型→执行工具→循环） | 显式 `AgentState` 八态：`Idle/Observing/Querying/ExecutingTools/Compacting/Waiting/Completed/Failed` + `StateKind` + `is_valid_transition` 转换表 | **AINS 扩展**（计划 3.1 要求显式 FSM；语义覆盖基线控制流） |
| 转换守护 | 无 | `debug_assert!(is_valid_transition(...))` 于主循环每次状态切换处 | AINS 扩展（debug 期守护，release 零开销） |
| 事件输入 | `query(messages, ...)` 单次调用 | `AgentEvent::UserMessage{content, attachments}` / `SystemEvent{Startup, Shutdown}` 经 mpsc(32) 注入 | **偏差（有意）**：常驻事件循环替代单次函数调用（计划 3.1 事件驱动内核） |
| Compacting | 基线 harness 层 compaction（不在 query.py） | 占位态：进入即发 `CompactProgress{phase:"compact_failed"}` → Idle | **偏差（暂缺）**：压缩策略随后续 Phase（上下文管理）落地 |

## 2. 消息与 sanitize（`kernel/messages.rs` ↔ `engine/messages.py`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 丢空 assistant | `_is_effectively_empty`（无块或全空文本）跳过 | `is_effectively_empty()` 同语义 | 对齐 |
| 未匹配 tool_use 回溯删除 | pending tool_use ids 非 ⊆ 后续 tool_result ids 时，删除该 assistant 消息 | `pending_tool_use_ids: HashSet` 子集校验 + `pending_tool_use_index` 回溯移除 | 对齐（含超集匹配保留 / 子集不匹配删除两侧测试） |
| 孤儿 tool_result 剔除 | 从 user 消息剔除无对应 tool_use 的 tool_result 块；剔空则整条丢弃 | 同 | 对齐 |
| 尾部悬空 tool_use 修剪 | 会话末尾 assistant 带未回填 tool_use 时移除 | 同（新输入到达 Observing 时先 sanitize） | 对齐 |
| 消息构造 | `user_message(text)` 等辅助 | `from_user_text` / `from_user_content` / `text()` / `tool_uses()` | 对齐（`text()` 与基线一致无分隔符拼接） |

## 3. StreamEvent（`kernel/stream_events.rs` ↔ `engine/stream_events.py`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 文本增量 | `TextDelta{text}` | `AssistantTextDelta{text}` | 对齐 |
| 轮次完成 | `MessageComplete{message, usage}` | `AssistantTurnComplete{message, usage}` | 对齐 |
| 工具开始/完成 | `ToolExecutionStart{tool_name, arguments}` / `ToolExecutionEnd{tool_name, output, is_error}` | `ToolExecutionStarted{tool_name, tool_input}` / `ToolExecutionCompleted{tool_name, output, is_error}` | 对齐（字段名 arguments→tool_input） |
| 错误/状态 | 异常上抛（无事件）；`ApiRetry` 转状态文案 | `Error{message, recoverable}` / `Status{message}` | **偏差（有意）**：常驻内核不 raise，错误封装为事件流（recoverable 标记会话是否存活） |
| 压缩进度 | 无（harness 层） | `CompactProgress{phase, trigger}` | AINS 扩展 |

## 4. AgentKernel 主循环（`kernel/event_loop.rs` ↔ `engine/query.py`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 工具循环骨架 | while True：请求模型 → 无 tool_use 则 return → 执行工具 → 回填 user(tool_result) → 继续 | Querying → 流式收集 → 有 tool_use 则 ExecutingTools → 回填 → Querying{turn+1}；无则 Idle | 对齐 |
| max_turns | `max_turns=8`（QueryEngine 默认），超限 raise `MaxTurnsExceeded` | 默认 8；Querying 入口 `turn >= max_turns` → `Error{recoverable:false}` + `Failed(MaxTurnsExceeded)` | 对齐（守护位置等价：max_turns=1 时工具执行一轮后、第二次模型调用前失败） |
| 工具并发 | `asyncio.gather` 并行执行本轮全部 tool_use | 顺序逐个执行 | **偏差（有意）**：WASM 单线程 + `&mut ToolContext` 独占借用；单轮多工具正确性不受影响，并行化留待 Native 侧按需优化 |
| 工具错误 | 工具异常 → `is_error=True` 的 tool_result 回填，循环继续 | `dispatch_tool` 同语义（`Tool {name} failed: {error}` / `Unknown tool: {name}`） | 对齐 |
| 结果回填 | 单条 user 消息聚合本轮全部 ToolResult 块 | 同 | 对齐 |
| 空 assistant | 收到空消息 → 以固定文案 raise | 发 `Error{recoverable:true}` 事件 + 不入会话 → Idle | **偏差（有意）**：事件化处理，会话存活 |
| 传输错误 | 类型化异常上抛（connect/timeout → 网络类） | `transport_error_message`：网络类 → "Network error: ... Check your internet connection and try again."，其余 "API error: ..." → `Error{recoverable:true}` → Idle | **偏差（有意）**：文案对齐基线，处理方式事件化 |
| Retry 透传 | `ApiRetry` → 文案 "Request failed; retrying in {delay:.1}s (attempt {a} of {max}): {msg}" | `Status{message}` 同格式文案 | 对齐 |
| continue 语义 | `--continue`：加载历史，末条 user 为 tool_result 且前有 tool_use 时直接续跑模型 | `has_pending_continuation()` + `prepare_continuation()`（sanitize 后置 Querying{0}） | 对齐 |
| Hooks / 权限 | `hook_executor` / permission 交互 | 未纳入 | **偏差（暂缺）**：随 Phase 3 Hook System / 权限模型加入 |
| Event Sourcing | 无 | 计划 3.4，未纳入 Phase 1 | 偏差（暂缺），后续 Phase 落地 |

## 5. tool_metadata 状态袋（`tools/mod.rs` ↔ `ToolExecutionContext.metadata`）

- 基线：`dict[str, Any]` 自由状态袋，跨工具调用共享。
- AINS：Phase 0 的 `serde_json::Map` 别名按计划收敛为结构化
  `ToolMetadata{read_files, invoked_skills, user_goal, work_log, extra}`，
  列表字段带 `TOOL_METADATA_LIST_CAP = 50` 去重上限（重复项移动至末尾，
  溢出淘汰最旧）。`extra` 保留基线自由 map 语义。
- 结论：对齐 + **AINS 扩展**（容量治理为嵌入式/长会话场景需要）。
- 影响面：`tests/core_traits.rs` 的 EchoTool 同步改用 `metadata.extra`（Phase 0
  清单已预告该收敛）。

## 6. ContextStore + Mock ModelClient（`kernel/context.rs` / `kernel/mock_model.rs`）

- `ContextStore{conversation, tool_metadata, loaded_skills}`：`build(event)` 将
  UserMessage 文本转 Text 块并记录 `user_goal`；`image/*` 附件 base64（STANDARD）
  编码为 Image 块（wire 形状对齐基线 `ImageBlock`）；空块跳过。
- `ScriptedModelClient`：脚本化 `ModelStreamEvent` 序列 + 请求录制，等价基线测试的
  `FakeClient`；`embed/stt/tts` 未脚本化（返回 `AgentError::Model`），Phase 1 不涉及。

## 7. 双 target 与并发原语

- 通道/选择器统一 `futures::channel::mpsc` + `futures::select!`（非 tokio 专属），
  Idle 超时以 `std::pin::pin!(R::sleep(...).fuse())` 与事件接收竞争。
- `AgentKernel<R: RuntimeAdapter>` 以 `PhantomData<fn() -> R>` 携带平台参数，
  避免 R 影响自动 trait 推导。
- 新增依赖：`base64 = "0.22"`（附件编码，双 target 通用）。

## 8. 验收记录（Phase 1 收尾）

- `cargo test -p agent-core`（Native）：lib 单测 23 项（sanitize 9 / fsm 4 /
  metadata 3 / context 3 及其他）+ `core_traits.rs` 19 项 + `kernel_loop.rs`
  集成 11 项，全部通过。
- `kernel_loop.rs` 覆盖：纯文本回复、完整工具循环（会话 4 条 + 第二次请求 3 条
  消息 + work_log 写入）、失败工具合成 is_error 回填、未知工具、max_turns=2 超限
  Failed、Retry→Status 文案、空 assistant 忽略、传输错误存活、新输入触发悬空
  tool_use 修剪、continue_pending 续跑、Shutdown 事件零查询完成。
- `cargo clippy -p agent-core --all-targets -- -D warnings`：通过（修正
  futures-channel `try_next` 弃用 → `try_recv`）。
- `cargo build/clippy -p agent-core --target wasm32-unknown-unknown -- -D warnings`：
  通过（内核仅依赖 futures/std，双 target 编译干净）。

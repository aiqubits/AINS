# Phase 0 对齐清单：核心 Trait 定义

对齐基线：`OpenHarness/src/openharness/`（提交时仓库内版本）。
范围：Phase 0.2 的七个核心 trait（`Tool` / `KvStore` / `VectorIndex` /
`DocumentStore` / `SkillLoader` / `SkillManage` / `ModelClient`）及其支撑类型。
详细行为对齐（sanitize 语义、工具循环、权限交互等）在对应 Phase 的特性清单中另行归档。

## 1. Tool（`tools/mod.rs` ↔ `tools/base.py`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 工具定义 | 类属性 `name`/`description`/`input_model` + `to_api_schema()` → `{name, description, input_schema}` | `definition() -> ToolDef{name, description, input_schema}` | 对齐（wire 形状一致；schema 来源从 Pydantic 换为手写/生成 JSON Schema） |
| 执行结果 | `ToolResult{output, is_error=False, metadata={}}` | `ToolResult{output, is_error, metadata}` + `ok()`/`err()` | 对齐（基线 metadata 默认空 dict，AINS 用 `Value::Null` 表达缺省） |
| 只读自报 | `is_read_only(arguments) -> bool`，默认 False | `is_read_only(&Value) -> bool`，默认 false | 对齐 |
| 输入校验 | `execute` 收到已校验的 Pydantic 类型化参数（调用侧校验） | `execute(Value, ctx)`，校验位置待 Phase 3 在 Registry 分发层统一做 | **偏差（有意）**：Rust 无 Pydantic 等价物；计划在 dispatch 层按 `input_schema` 校验后再进 execute |
| 执行上下文 | `ToolExecutionContext{cwd, metadata, hook_executor?}` | `ToolContext{cwd, metadata}` | **偏差（暂缺）**：hooks 通道随 Phase 3 Hook System 加入 |
| ToolMetadata | dict[str, Any] | 暂为 `serde_json::Map` 别名 | Phase 1.5 收敛为带条数上限的结构化状态袋 |

## 2. ModelClient（`model_client.rs` ↔ `api/client.py`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 流式方法 | `stream_message(ApiMessageRequest) -> AsyncIterator[ApiStreamEvent]`（Protocol：`SupportsStreamingMessages`） | `stream_response(ModelRequest) -> EventStream<ModelStreamEvent>` | 对齐（方法名按 AINS_PLAN 命名；单方法流式协议语义一致） |
| 事件三联合 | `ApiTextDelta{text}` / `ApiMessageComplete{message, usage, stop_reason}` / `ApiRetry{message, attempt, max_attempts, delay_seconds}` | `TextDelta` / `Complete` / `Retry{..., delay_secs: f32}` | 对齐（字段名 `delay_seconds → delay_secs`） |
| usage | `UsageSnapshot{input_tokens, output_tokens}` | 同 | 对齐 |
| 请求形状 | `{model, messages, system_prompt, max_tokens, tools, effort}` | `{model: Option, messages, system_prompt, max_output_tokens, tools}` | **偏差（有意）**：`effort` 裁剪（AI Gateway 侧路由）；`model` 可空由网关按套餐路由 |
| max_tokens 默认值 | `max_tokens: int = 4096` | `Default` 手工实现，`DEFAULT_MAX_OUTPUT_TOKENS = 4096` | 对齐（Code Review 修正：原 derive Default 会得到 0） |
| 重试语义 | 可重试异常 yield `ApiRetryEvent` 后退避重试；不可重试 raise 类型化错误 | 同语义；实现于 Phase 5 | 注意“不复刻”第 1 条：重试不得重复已发出的 delta |
| embed/stt/tts | **无对齐物**（stt 仅 `voice/stream_stt.py` 占位；无 embed/tts 抽象） | trait 类型化便捷方法 | **AINS 扩展**（服务端 AI Gateway 能力，见附录 D） |
| 多 provider | openai/codex/copilot 多客户端 + registry | 不对齐（仅自家 AI Gateway） | 按对齐矩阵“明确不对齐”执行 |

## 3. 消息与内容块（`kernel/messages.rs` ↔ `engine/messages.py`）

- `ContentBlock` 判别式（`type` tag）：`text` / `image` / `tool_use` / `tool_result` — 对齐。
- **偏差（有意裁剪）**：基线 `ImageBlock.source_path`（默认 ""）与 `ToolResultBlock.result_metadata`
  （序列化到 wire 时被丢弃）未纳入 Phase 0 类型；如 Phase 1 工具循环需要再补，wire 形状不受影响。
- `ToolUseBlock.id` 基线有默认值 `toolu_<uuid>`；AINS 由构造方显式提供（Phase 1 定夺生成策略）。
- `sanitize_conversation_messages` 为 Phase 1.2 任务，其四条语义（丢空 assistant、
  超集校验回溯删除、剔除孤儿 tool_result、修剪尾部悬空 tool_use）已记录于调研，
  Phase 1 清单再展开。

## 4. KvStore / VectorIndex / DocumentStore（`memory/` ↔ 基线 `memory/`）

基线 memory 为**纯文件系统 Memdir + 词法检索**，无 KV/向量/文档存储接口抽象。
三个 trait 均为 **AINS 新抽象**（嵌入式双后端架构需要），无逐项对齐物：

- `KvStore`：get/set(ttl)/delete/list_prefix，redb（Native）/ IndexedDB（Web）双实现（Phase 2）。
- `VectorIndex` + `VectorIndexManager`：多 namespace 独立索引；**与计划骨架的偏差**：
  - 计划中 `VectorIndex::load(kv) -> Self` 静态构造方法未纳入 trait（构造依赖
    namespace/config，且破坏 dyn 可用性），移至 Phase 2 各平台实现的构造函数。
  - 计划中 Manager 仅有 `get_index(&self) -> &dyn`，无法满足 `add/remove` 的
    `&mut self` 写路径，补充 `get_index_mut`。
- `DocumentStore`：index/search/list_docs/delete/is_indexed，按计划 4.3。
- 基线 memdir（MEMORY.md 索引/frontmatter schema/usage index）在 Phase 2.8/2.9 对齐。

## 5. SkillLoader / SkillManage（`skills/mod.rs` ↔ `skills/`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 渐进式加载 | 系统提示注入 `- **name**: description` 索引；`skill` 工具按需取全文（content 扫描期已在内存） | Level 0 `list` / Level 1 `load` / Level 2 `load_reference` | 对齐（AINS 存储走 KvStore 故 load 真惰性；Level 2 引用文件为 AINS 扩展） |
| frontmatter | `name/description/user-invocable/disable-model-invocation/model/argument-hint` | `SkillContent.frontmatter: serde_yaml::Value` 全量保留；`SkillSummary` 摘要含 category/requires_tools（`metadata.ains` 扩展段） | 对齐 + AINS 扩展；Phase 6 实现时逐字段落地基线键名 |
| 门控 | disable_model_invocation 过滤 | `SkillContext{platform, available_tools}` 门控 | AINS 扩展（平台/工具门控），基线双门控语义 Phase 6 补齐 |
| 管理 | 无（文件系统扫描） | `SkillManage` create/update/rollback/delete（Agent 自主创建，Trust Model） | AINS 增强（第六章） |

## 6. 平台适配层（非 0.2 范围，Phase 0.1 附带）

- `RuntimeAdapter` + `TokioRuntimeAdapter` / `WasmRuntimeAdapter`（cfg 门控）。
- **实现取舍**：计划中 trait 定义"无 cfg"，但 Send 约束双端不同（tokio 需要
  `Send`，wasm 单线程不需要），引入 `marker::MaybeSend / MaybeSendSync`
  （cfg 收敛的标记 trait）保持业务逻辑单一定义；async trait 统一用
  `cfg_attr(async_trait / async_trait(?Send))` 处理。
- 流类型 `EventStream<T>`：Native = `BoxStream`，Web = `LocalBoxStream`。

## 7. 依赖偏差（附录 A 对照）

- Phase 0 仅引入实际使用的依赖：`serde/serde_json/serde_yaml/async-trait/thiserror/futures`
  + target 门控的 `tokio` / `wasm-bindgen(-futures)/js-sys/web-sys`。
- `bincode/redb/hnsw_rs/regex/pulldown-cmark` 等推迟到首个使用它们的 Phase（2/3）引入，
  避免无引用依赖拉长双目标编译。

## 8. Code Review 修正记录（Phase 0 收尾）

- `MemoryNamespace` / `Metric` / `ToolCategory` 补 `#[serde(rename_all = "snake_case")]`：
  三者会随 Phase 2 持久化落盘，提前固定 wire 格式避免后续数据迁移。
- `ModelRequest` 弃用 derive `Default`（`max_output_tokens` 会默认为 0），改为手工
  `Default` + `DEFAULT_MAX_OUTPUT_TOKENS = 4096`（对齐基线默认值）。
- `WasmRuntimeAdapter::sleep` 改用 `js_sys::global()` 上的 `setTimeout`：原实现绑定
  `web_sys::window()`，在 Web Worker（无 Window 的全局作用域）中会 panic；对全局作用域
  中立后，Window 与 Worker 双环境均可用。
- 补充测试：`tests/core_traits.rs`（Native，19 项）—— 全部七类 trait 的内存 mock 实现
  + `dyn` 可用性、KvStore 契约、Tool 元数据写入/错误映射、ModelClient 三事件流协议、
  VectorIndex 相似度排序、Manager namespace 隔离、save→KvStore 持久化、DocumentStore
  去重/范围限定/top_k 截断、Skill 门控过滤与生命周期、snake_case 序列化契约、
  未知 content block 拒绝、Image/ToolResult(is_error) wire 形状、ModelRequest（含
  ToolDef）请求形状 roundtrip、AgentError 对 Memory/Skills 错误的 From 映射、
  Native 端 `dyn Trait: Send + Sync` 编译期断言；
  `tests/web_smoke.rs`（wasm32 浏览器，5 项）—— Platform/serde/ModelRequest 默认值/
  `WasmRuntimeAdapter::sleep`/`spawn`；
  `tests/web_worker_smoke.rs`（wasm32 DedicatedWorker，2 项）—— 无 `Window` 作用域下
  `sleep`/`spawn` 可用（守护 Worker 兼容性修正不回退）。

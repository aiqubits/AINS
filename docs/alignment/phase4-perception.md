# Phase 4 对齐清单：Perception System

对齐基线：`OpenHarness/src/openharness/engine/messages.py`（`ImageBlock`）、
`ui/protocol.py`（`FrontendImageAttachment`）、`voice/`、
`tools/image_to_text_tool.py`（提交时仓库内版本）；文档解析复用 AINS
Phase 2 `memory/parser.rs`。
范围：Phase 4.1–4.4（Vision / Voice / File 感知通道 + 写入 ContextStore）。

> **依赖**：4.1/4.2 的 Server API 调用以 Phase 5.1（client-api）+ 5.2
> （`ModelClient`）为前置；4.3 依赖 Phase 2.6 DocumentStore Parser；4.4 依赖
> Phase 1.6 ContextStore。本阶段与 Phase 5.2–5.5 并行推进（见 AINS_PLAN
> 第十一章推进规则例外）。

本地验收：`rust-agent` 双 target build/clippy(-D warnings) 通过；Native
`perception` 单测全过（mod / vision / voice / file，具体用例数随评审回归
测试演进，以 `cargo test -p rust-agent perception` 为准），并经
`tests/context_pipeline.rs` 与 Phase 5.4 汇合验证（感知→ContextStore→会话
快照往返）。

## 架构分工

平台特定采集（摄像头 / 麦克风 / 截屏 / 拖拽）由各前端（app/web、
app/desktop）在其平台 API 层完成；`rust-agent::perception` 只接收**已采集的
原始字节**，保持核心纯粹、双端可测。感知结果统一为 `PerceptionOutcome`
（text + image 附件 + 来源说明），经 `into_agent_event` 转 `AgentEvent::
UserMessage`，由既有 `ContextStore::build` 落入上下文（图像附件→Image block、
文本→Text block + user_goal）。

## 1. Vision（4.1，`perception/vision.rs`）

| 能力点 | 基线 | AINS `VisionChannel` | 结论 |
|---|---|---|---|
| 帧采集 | 前端 `FrontendImageAttachment`（media_type 须 `image/`、data 非空）→ `ImageBlock` | `capture(data, mime)`：校验 `image/*` + 大小 ≤ `MAX_IMAGE_BYTES`(16 MiB)，产出 Image 附件 outcome（来源说明 `[vision: captured frame]`） | 对齐 |
| Server API 调用 | vision 即带 `input_image` 的 chat 消息（`api/client.py`） | `describe(model, data, mime, prompt)`：构造 `[Text(prompt), Image(base64)]` chat 消息经 `ModelClient::stream_response` 收集 Complete 文本（默认提示 `DEFAULT_VISION_PROMPT`） | 对齐 |
| 校验 | media_type 前缀 / 非空 | 非 `image/*`、空、超限均报 `AgentError::Model` | 对齐 + 大小护栏 |

## 2. Voice（4.2，`perception/voice.rs`）

| 能力点 | 基线 | AINS `VoiceChannel` | 结论 |
|---|---|---|---|
| 麦克风采集 | `voice/`：采集后转文本进入普通消息 | 平台采集音频字节 → `transcribe(model, audio)` 经 `ModelClient::stt` 转写 | 对齐 |
| 采集入口 | — | `capture(model, audio)`：转写为文本 outcome（来源说明 `[voice transcript]`）；空白转写返回空 outcome（落上下文时跳过） | AINS 扩展 |
| 校验 | — | 空音频 / 超 `MAX_AUDIO_BYTES`(32 MiB) 报错 | 资源护栏 |

## 3. File（4.3，`perception/file.rs`）

| 能力点 | 基线 | AINS `FileChannel` | 结论 |
|---|---|---|---|
| 拖拽解析 | 图像→ImageBlock；文本/代码/PDF 进入消息 | `ingest(data, filename, mime_hint)`：图像（`image/*` 或图像扩展名）→ Image 附件；PDF → `extract_pdf_text`（Web 无解析报错）；Text/Code/Markdown → UTF-8 有损解码 | 对齐 |
| 类型判定 | mimetypes 猜测 | `DocumentKind::from_name`（复用 Phase 2）+ 图像扩展名集合（png/jpg/jpeg/gif/webp/bmp/svg）+ mime 提示优先 | 对齐（复用 parser） |
| 图像 mime 推断 | — | mime 提示优先，否则按扩展名映射（jpg→jpeg、svg→svg+xml…） | AINS 扩展 |
| 截断/护栏 | — | 文本按 `MAX_FILE_TEXT_CHARS`(200000) 截断 + 标记；非图像文件输入字节按 `MAX_FILE_BYTES`(32 MiB) 上限（解码/PDF 抽取前拒绝）；空文件 / 空白文本 / 超大图像报错；来源说明 `[file: <name>]` | 资源护栏（字节上限见 §6 Code Review 修正 CR-5） |

## 4. 写入 ContextStore（4.4，`perception/mod.rs`）

| 能力点 | 基线 | AINS `PerceptionOutcome` | 结论 |
|---|---|---|---|
| 归一化结果 | 各通道各自入消息 | 统一 `PerceptionOutcome{text, attachments, source_note}`；`compose_content` 按 `[user_prompt]+[source_note]+[text]` 有序拼接（空段跳过） | 对齐（统一抽象） |
| 转事件 | 前端请求组装 user 消息 | `into_agent_event(user_prompt)` → `AgentEvent::UserMessage`（文本 + image 附件）；全空返回 None | 对齐 |
| 落入上下文 | — | `apply_to_context(ctx, user_prompt)`：复用既有 `ContextStore::build`（图像 base64→Image block、文本→Text block + user_goal，非图像附件仍由 build 忽略——文件文本已在 4.3 转为文本注入） | 对齐（复用 Kernel 上下文构建） |
| Kernel 集成 | 事件通道提交 | 感知产出即普通 `AgentEvent`，可直接经 Kernel 事件通道提交，无需改 Kernel（与 5.5 Kernel 接线正交） | 对齐（零耦合） |

## 5. 有意偏差与后置项

- 平台采集（摄像头 / 麦克风 / 截屏）不在 `rust-agent` 内实现，由前端平台层
  提供原始字节（架构分工；保持核心双端可测）。
- Web 端 PDF 解析不可用（`extract_pdf_text` 仅 Native），拖拽 PDF 在 Web 报错
  （对齐 Phase 2 memory 层双端分工）。
- 非图像附件（如原始二进制）经 `ContextStore::build` 时仍被忽略；文档类由
  File 通道在 4.3 阶段先转为文本注入，二进制附件的文档索引随 Document Memory
  异步接入面完善。
- Vision `describe` 为便捷方法；常规多模态对话仍走 Kernel 主循环（带 Image
  block 的 user 消息 → `ModelClient`），不重复模型调用职责。

## 6. Code Review 修正记录（超越基线）

- **CR-5 非图像文件缺输入字节上限**（`perception/file.rs`）：图像（`MAX_IMAGE_BYTES`
  16 MiB）与音频（`MAX_AUDIO_BYTES` 32 MiB）均有输入字节护栏，但 Text/Code/
  Markdown/PDF 分支无字节上限，超大输入先全量 `from_utf8_lossy` 二次分配
  再按字符截断（PDF 抽取也在无界输入上运行），与项目既有护栏模式不一致。
  修复：新增 `MAX_FILE_BYTES`(32 MiB)，非图像分支在解码/PDF 抽取前拒绝超限
  输入。回归测试 `ingest_rejects_oversized_non_image_file_before_decode`。

# Phase 5 对齐清单：客户端 AI 传输层 + 上下文管线

对齐基线：`OpenHarness/src/openharness/api/client.py`（+ `api/usage.py`、
`api/errors.py`）、`prompts/`（`context.py`、`system_prompt.py`、
`environment.py`、`claudemd.py`）、`services/session_storage.py`、
`services/compact/`、`services/token_estimation.py`（提交时仓库内版本）。
服务端契约以 `server/src/handlers/responses.rs` + `services/responses.rs` 为准。
范围：Phase 5.1–5.6（client-api 传输层、ModelClient、提示流水线、会话持久化、
上下文压缩、集成测试）。

本地验收：`client-api` 与 `rust-agent` 双 target build/clippy(-D warnings) 通过；
Native 测试全过——client-api 单元/集成（`tests/ai_test.rs`）；rust-agent lib
（model_service、context::{prompt_pipeline,session,compact}、perception）+
集成（`tests/model_service.rs`、`tests/context_pipeline.rs`，Phase 4∥5 汇合）。
具体用例数随评审回归测试演进，以 `cargo test -p rust-agent -p client-api`
为准。Web 端 `client-api`/`rust-agent` 随既有 wasm 套件走 CI wasm-pack
浏览器测试。

## 1. client-api 统一传输层（5.1，`app/client-api/src/ai.rs`）

| 能力点 | 基线 / 服务端契约 | AINS | 结论 |
|---|---|---|---|
| 统一入口 | `POST /api/ai/response` 单 envelope（JWT），chat/vision SSE 流式 | `Client::response`（非流式）/ `Client::response_stream`（SSE）；`AiRequest` 序列化 capability 路由字段 + `input`(untagged Text/Texts/Messages/Audio) + `#[serde(flatten)] extra` 平铺直连专有字段 | 对齐 |
| 类型化便捷方法 | embedding/stt/tts 直连；vision 即带 `input_image` 的 chat | `chat`/`chat_stream`/`embed`/`stt`/`tts`；vision 无独立方法（消息带 `AiContentPart::InputImage`） | 对齐 |
| 响应提取 | `output` 项形态随 capability 变化 | `AiResponse::{output_text, refusal, transcription, audio(解码 base64), embeddings}` | 对齐 |
| SSE 事件序列 | `response.created`→`output_item/content_part.added`→`output_text.delta`→`.done`→终止事件三选一；`error` 事件后关闭；`: keepalive` 注释 | `AiStreamEvent` 覆盖 Created/OutputTextDelta/RefusalDelta/OutputTextDone/RefusalDone/Completed/Incomplete/Failed/Error/Other；多行 `data:` 合并、冒号后单空格剥离、注释行忽略、`\r\n\r\n`\|`\n\n`\|`\r\r` 分隔、4 MiB 缓冲上限 | 对齐 |
| 失败信封 | `status="failed"` + `error{code,message}`（`ai_response_error_body`）；中间件 401/429 也走该信封（429 例外为 `{"error":...}`） | `ClientError::Api{status,code,message}`；`map_ai_error` 仅当 `object==response` 且 `error` 非空才解析为 Api，否则保留原错误（中间件 429 非信封形态不误判） | 对齐 |
| 重试 | 既有 `send_and_parse` 网络/5xx/429 指数退避（500ms/1s/2s） | 非流式复用 `send_and_parse`；流式仅在**建连阶段**按 `max_retries` 退避重试，流建立后不在传输层重试（重试语义上收 ModelClient，见 5.2） | 对齐（分层） |
| 跨端 | reqwest 双 target；wasm 无 tokio | 新增 `stream` feature（双 target）+ `futures`/`bytes`/`base64`；`AiEventStream` cfg 收敛 Box/LocalBox；`sleep_ms` 复用既有跨端定时器 | 对齐 |

## 2. ModelClient 实现（5.2，`rust-agent/src/model_service.rs`）

| 能力点 | 基线 `api/client.py` | AINS `GatewayModelClient` | 结论 |
|---|---|---|---|
| 重试常量 | MAX_RETRIES=3、BASE_DELAY=1.0s、MAX_DELAY=30s、可重试 {429,500,502,503,529} | 同常量同集合（`MAX_RETRIES`/`BASE_DELAY_SECS`/`MAX_DELAY_SECS`/`RETRYABLE_STATUS_CODES`） | 对齐 |
| 退避 | `min(1.0×2^attempt, 30) + uniform(0, delay×0.25)` | `retry_delay`：同公式；抖动源为毫秒时钟均匀映射（客户端无 RNG 依赖） | 对齐（偏差：抖动源用时钟而非 `random`） |
| 事件语义 | delta/complete/retry 三事件；retry 作为流内事件；认证类不重试 | `ModelStreamEvent::{TextDelta,Complete,Retry}`；建连/流中断可重试则先 yield Retry 再退避重试；不可重试（401/403/请求形状）以终止性 Retry(attempt==max) 上报后终止（trait 无错误事件变体，Kernel 对无 Complete 流归一为 recoverable Error） | 对齐（偏差记录见下） |
| delta 去重 | 重试后不重复已发文本 | 按原始字符计数 `emitted_raw_chars` 去重，仅发超出既发计数的增量 | 对齐 |
| usage 快照 | input/output_tokens | 从终止响应 `usage` 映射 `UsageSnapshot` | 对齐 |
| 工具下发 | 基线走 provider function tools | **偏差（有意）**：服务端 AI 端点拒绝 function tools，故采用**提示词内 `<tool_use>` 标签协议**：工具清单 + 协议说明注入 system prompt，从 assistant 全文解析回 `ToolUse` block（非法块 fail-open 保留为文本）；历史 `ToolUse`/`ToolResult` 回渲染为 `<tool_use>`/`<tool_result>` 文本；`ToolTagFilter` 状态机过滤 UI delta，抑制跨分片协议片段外漏 | 偏差（协议桥接） |
| 直连能力 | — | `embed`/`stt`/`tts` 复用 client-api；`stt` 按魔数嗅探音频格式（RIFF/fLaC/OggS/ID3/ftyp/EBML→wav/flac/ogg/mp3/mp4/webm，回退 wav） | AINS 扩展 |

## 3. 分段系统提示流水线（5.3，`rust-agent/src/context/{prompt_pipeline,environment,project_docs}.rs`）

| 能力点 | 基线 `prompts/` | AINS | 结论 |
|---|---|---|---|
| 段顺序/连接 | base→权限模式→(fast/reasoning)→技能→委派→项目指令→本地规则→项目上下文→记忆；空段过滤，`\n\n` 连接 | base→权限模式→技能→项目指令→Environment→记忆；空段过滤，`\n\n` 连接（客户端裁剪掉 coordinator/委派/reasoning/issue-pr 等服务端/CLI 段） | 对齐（裁剪记录） |
| base prompt | `_BASE_SYSTEM_PROMPT` 五节 | `BASE_SYSTEM_PROMPT` 同五节结构（System/Doing tasks/Executing actions with care/Using your tools/Tone and style），文案 AINS 化 | 对齐 |
| Environment | OS/Arch/Shell/cwd/Date/Python/…；shell 读 SHELL、git 起子进程（5s 超时） | `EnvironmentInfo::detect`：OS/Arch 取编译期常量、Platform、cwd(入参)、Date(毫秒时钟 `format_iso_utc` 前 10 位)；**shell/git 为宿主可选注入项**（不读环境变量、不起子进程，纯函数双端可测，对齐附录 C 配置边界） | 对齐（偏差：探测收敛到平台层注入） |
| 项目指令逐级发现 | `discover_claude_md_files`：cwd 向上到根，每层 `CLAUDE.md`/`.claude/CLAUDE.md`/`.claude/rules/*.md`，近者在前、去重 | `discover_agents_md_files`：**AINS 用 `AGENTS.md`/`.agents/AGENTS.md`**，cwd `ancestors()` 向上到根、近者在前、seen 去重（Web 无文件系统返回空） | 对齐（命名 AGENTS.md） |
| 截断 | 每文件 12000 字符 + `...[truncated]...` | `MAX_CHARS_PER_PROJECT_DOC=12000` + 同标记；`# Project Instructions` + 逐文件 `## <path>` + md 代码块；单文件输入字节受 `MAX_PROJECT_DOC_BYTES`(1 MiB) 护栏（超限跳过，避免全量读入） | 对齐（字节上限见 §8 Code Review 修正 CR-7） |
| 权限模式段 | PLAN/FULL_AUTO/Default 三文案 | `permission_mode_section` 三文案对齐 `PermissionMode` | 对齐 |
| 技能索引段 | `# Available Skills` + `- **cmd**: desc` | `skills_section`：同标题同条目格式（`SkillSummary`），空则 None | 对齐 |
| 记忆段 | `load_memory_prompt` | 由调用方从 `MemdirStore::load_memory_prompt` 取字符串传入（保持流水线为同步纯函数） | 对齐（解耦） |
| 各段开关 | coordinator/system_prompt/memory 等开关 | `PromptSections` 六 bool + `custom_base`（替换内置 base） | 对齐 |

## 4. 会话持久化（5.4，`rust-agent/src/context/session.rs`）

| 能力点 | 基线 `session_storage.py` | AINS `SessionStore` | 结论 |
|---|---|---|---|
| 存储 | 文件系统 latest.json + session-{id}.json，`atomic_write`(tmp+fsync+rename) | **偏差（平台）**：走 `KvStore`（Native redb / Web IndexedDB，双端一致）；单键 set 各自事务原子；先写按 id 完整条目再更新 latest 指针 | 偏差（KV 后端） |
| 快照结构 | session_id/cwd/model/system_prompt/messages/usage/tool_metadata/created_at/summary/message_count | `SessionSnapshot` 同字段（created_at→created_at_ms） | 对齐 |
| 落盘前 sanitize | `sanitize_conversation_messages` | save 前 sanitize，load 后再 sanitize（回载防悬空 tool 结构，对齐 `_sanitize_snapshot_payload`） | 对齐 |
| tool_metadata 白名单 | `_PERSISTED_TOOL_METADATA_KEYS`(10) 键过滤 | 结构化字段（read_files/invoked_skills/work_log 等，已受治理）全量保留；`extra` 仅保留 `PERSISTED_EXTRA_KEYS`(6) 白名单键（permission_mode/task_focus_state/async_agent_*/compact_*） | 对齐（结构化 + extra 白名单） |
| 项目隔离 | `basename-sha1(abspath)[:12]` 目录 | `project_slug` = `basename-sha256(cwd)[:12]`（key 前缀 `session/{slug}/`），不依赖 canonicalize（双端一致） | 对齐（摘要 sha256、路径不 canonicalize） |
| 恢复/列表 | latest / by-id(回退 latest) / list(mtime 降序 limit 20) | `load_latest`/`load_by_id`(命名条目→回退 latest，id 匹配或字面 "latest")/`list`(created_at 降序 `DEFAULT_LIST_LIMIT=20`，损坏条目逐条跳过) | 对齐（损坏条目跳过见 §8 Code Review 修正 CR-3） |
| summary | 首条 user 文本前 80 字符 | `extract_summary` 同语义（`SUMMARY_MAX_CHARS=80`） | 对齐 |

## 5. 上下文压缩四级降级链（5.5，`rust-agent/src/context/compact.rs`）

| 能力点 | 基线 `services/compact/` | AINS | 结论 |
|---|---|---|---|
| 常量 | COMPACTABLE_TOOLS(8)、AUTOCOMPACT_BUFFER=13000、MAX_OUTPUT_FOR_SUMMARY=20000、MAX_CONSECUTIVE_FAILURES=3、SESSION_MEMORY(12/48/4000)、COLLAPSE(2400/900/500)、KEEP_RECENT=5、padding 4/3、image 3072、window 200000 | 全部逐字对齐（同名常量） | 对齐 |
| token 估算 | `max(1,(len+3)//4)`；消息含图像预算 + 4/3 padding | `estimate_tokens`(div_ceil 4)、`estimate_message_tokens`（文本/tool_result/tool_use(name+input)/image 3072，×4/3） | 对齐 |
| 阈值/熔断 | 窗口−min(20000,20000)−13000=167000；连续失败≥3 熔断 | `get_autocompact_threshold`=167000；`should_autocompact` 熔断 | 对齐（熔断重置见 §8 Code Review 修正 CR-1/CR-2） |
| 第1级 microcompact | 清旧可压缩 tool_result（保留最近 5），替换为清除占位；无 LLM | `microcompact_messages` 同语义（`CLEARED_TOOL_RESULT`），返回 tokens_saved | 对齐 |
| 第2级 文本折叠 | 老段 Text/ToolResult >2400 折叠 head900+标记+tail500；不降则 None | `try_context_collapse` 同语义（按字符/Unicode 折叠） | 对齐 |
| 第3级 会话记忆 | 老段压单条摘要（48 行/4000 字符），无 LLM | `try_session_memory_compaction` 同语义 | 对齐 |
| 第4级 LLM 摘要 | microcompact→切分→compact prompt→流式（25s 超时、2 流式重试、3 PTL 头部截断）；`<analysis>`/`<summary>` | `compact_conversation`(pub，manual 直调)：同流程；`format_compact_summary` 抽 `<summary>` 剥 `<analysis>`；PTL `truncate_head_for_ptl_retry`（丢最老 `max(1,组数/5)` 组）；**偏差**：无硬超时（`RuntimeAdapter` 无 timeout；依赖 ModelClient 重试收敛） | 对齐（偏差：无 25s 硬超时） |
| 配对保护切分 | `_split_preserving_tool_pairs` 切点左移不劈 tool_use/result；新段 sanitize | `split_preserving_tool_pairs` 同语义 | 对齐 |
| 触发源 | auto/manual/reactive | `CompactTrigger`（复用 kernel::state）；auto 由 Kernel Querying 起始内联触发，manual/reactive 经 Compacting 状态 | 对齐 |
| CompactProgress | 九阶段 phase | `ProgressFn`(Native `+Send`) 收集 phase，Kernel 转 `StreamEvent::CompactProgress`；phase 取 context_collapse/session_memory/compact_start/end/retry/failed | 对齐 |
| 压缩后历史 | boundary+summary+keep+attachments | `build_compact_summary_message`（续接说明 + 格式化摘要 + suppress_follow_up）+ 保留近段，最终 sanitize | 对齐（简化：不含 8 类 attachment 构建，随会话 metadata 后置） |

## 6. Kernel 接线与集成（5.6，`rust-agent/src/kernel/event_loop.rs`）

- Querying 起始 `should_autocompact` 达阈值则内联 `run_compaction(Auto, force=false)`（PreCompact hook→四级链→CompactProgress→PostCompact hook），压缩后同轮续建请求（turn 不变）。
- `Compacting` 状态桩替换为真实压缩（manual 强制），完成回 `Querying{turn:0}`；FSM `Compacting→Querying` 收敛为主出边。
- `AgentKernel` 新增 `compact_state: AutoCompactState` 跨轮持有熔断计数。
- 集成测试 `context_pipeline.rs`：感知→ContextStore→会话快照往返、超阈值自动压缩后续答（CompactProgress + token 量下降）、系统提示各段开关。

## 7. 有意偏差与后置项汇总

- ModelClient 工具下发用提示词内 `<tool_use>` 标签协议（服务端拒 function tools）。
- 重试抖动源为毫秒时钟均匀映射（无 RNG）；LLM 摘要无 25s 硬超时（RuntimeAdapter 无 timeout）。
- 会话持久化落 KvStore（非文件系统）；project_slug 用 sha256 且不 canonicalize（双端一致）。
- Environment 的 shell/git 探测收敛为平台层可选注入（不读环境变量、不起子进程）。
- 压缩 attachments 8 类构建器后置（随会话 tool_metadata 白名单持久化面完善）。
- Web 端会话/项目指令受平台能力约束（KvStore 后端、无本地文件系统项目指令）。

## 8. Code Review 修正记录（超越基线）

以下为本阶段 code review 发现并修复的问题，均为对基线语义的**改进性偏差**
（基线本身存在同类缺陷），已补回归测试钉定：

- **CR-1 熔断计数在低级压缩成功时未重置**（`context/compact.rs`）：基线仅第 3/4
  级成功重置 `consecutive_failures`，第 1/2 级（microcompact / 文本折叠）成功
  早退只置 `compacted`。这使成功压缩穿插在失败之间时仍按“连续失败”累计，
  可能提前触发熔断禁用自动压缩。修复：第 1/2 级成功早退同样重置
  `consecutive_failures` 并 `turn_counter += 1`。回归测试
  `low_level_success_resets_consecutive_failures`。
- **CR-2 第 4 级 no-op passthrough 虚报压缩成功**（`context/compact.rs`）：当
  ≤ `preserve_recent` 条巨型消息超阈值时，`compact_conversation` passthrough
  原样返回，`auto_compact_if_needed` 却置 `compacted=true`、重置熔断并返回
  `was_compacted=true`，导致 `should_autocompact` 恒真、后续每轮徒劳重跑。
  修复：结果与输入相等时不算压缩（返回 `was_compacted=false`、不动状态），
  上报 `compact_noop` 供 UI 呈现“上下文无法继续压缩”。回归测试
  `few_huge_messages_noop_does_not_report_compacted`。
- **CR-3 `list` 遇损坏条目整表失败**（`context/session.rs`）：单条快照反序列化
  失败经 `?` 传播使整个列表调用失败；基线 `list_sessions` 对 JSONDecodeError
  逐条 `continue`。修复：损坏条目跳过而非中止。回归测试
  `list_skips_corrupted_entries_instead_of_failing`。
- **CR-4 SSE 分隔符每 chunk 从 0 重扫**（`app/client-api/src/ai.rs`）：单个大事件
  分小 chunk 到达时 `find_sse_delimiter` 反复重扫已扫前缀，最坏 O(n²)（4 MiB
  上限内）。修复：`SseState` 记录 `scan_from` 偏移，未命中时回退 `len-3`
  （覆盖跨界 `\r\n\r\n`），切出事件后归零。回归测试
  `take_next_sse_event_byte_split_torture`（逐字节切分与一次性输入等价）、
  `find_sse_delimiter_scan_offset_resumes_without_missing`。
- **CR-6 压缩摘要对终态不可重试错误仍消耗重试预算**（`context/compact.rs`）：
  `collect_summary` 将 ModelClient 的终态失败（`attempt == max_attempts` 的
  Retry，如 no_active_plan/unauthorized）降级为普通 `String` 错误，使
  `compact_conversation` 仍额外重试 `MAX_COMPACT_STREAMING_RETRIES` 次，每次又
  触发一整轮模型重试周期（最坏 ≈ 3×(3+1) 次物理请求与退避延迟）。
  修复：`collect_summary` 返回 `SummaryFailure{message, terminal}`，终态位源自
  终态 Retry；`compact_conversation` 先判 PTL（不受影响，仍做头部截断重试），
  非 PTL 且 terminal 则立即失败。回归测试
  `compact_conversation_terminal_failure_does_not_retry`（断言仅一次模型调用）。
- **CR-7 项目指令文件无输入字节上限**（`context/project_docs.rs`）：
  `load_project_instructions` 以 `read_to_string` 全量读取发现的 AGENTS.md 再按
  12000 字符截断；而 `discover_agents_md_files` 会沿 `ancestors()` 上溯至文件
  系统根，可能命中项目外父目录（如 `~/AGENTS.md` 或符链）的超大同名文件，
  与项目一贯的资源护栏模式（如感知层 CR-5）不一致。修复：新增
  `MAX_PROJECT_DOC_BYTES`(1 MiB)，读取前经 `metadata` 大小守卫跳过超限文件
  （对合理体量文件行为零变化；12000 字符 ≤ 48 KB 远低于 1 MiB）。回归测试
  `load_skips_oversized_doc_but_keeps_normal_sibling`。
- **CR-8 `ToolTagFilter` 相似标签误入 in_tag 吞文本**（`model_service.rs`）：
  UI 过滤器以 `<tool_use`（无尾随分隔符）为开标记，`<tool_used>` 之类同前缀
  普通文本会误入 in_tag 态；若后续无 `</tool_use>` 闭合，`flush` 丢弃扣留
  内容，余下流式文本从 UI 视图永久消失（最终 Complete 消息不受影响，
  `parse_assistant_content` 自带 fail-open）。修复：仅当 `<tool_use` 紧跟空白
  或 `>` 才进入 in_tag；后随其它字符按普通文本放行；分片恰断在标记结尾
  时扣留待下一分片定夺。回归测试 `tool_tag_filter_does_not_swallow_similar_tag_name`、
  `tool_tag_filter_similar_tag_split_across_chunks_is_not_swallowed`、
  `tool_tag_filter_open_tag_split_after_marker_still_suppresses`。
- **CR-9 SSE 终止事件载荷非法被静默丢弃**（`app/client-api/src/ai.rs`）：
  `parse_sse_block` 对反序列化失败一律 `.ok()?` 跳过，终止事件
  （completed/incomplete/failed）损坏时流直至连接关闭才结束，消费方无法
  区分“连接断开”与“终止事件损坏”（无诊断的异常结束）。修复：
  `parse_sse_block` 改返回 `Result<Option<_>>`，终止事件载荷非法时上报
  `ClientError::Deserialization` 并收敛流；非终止事件的非法 JSON 仍为可忽略
  块（内容由终止事件全量文本兑付）。回归测试
  `parse_sse_block_malformed_terminal_event_surfaces_error`（单元）、
  `test_response_stream_malformed_terminal_event_yields_error`（集成）。
- **CR-10 孤立 `\r` 行终止符的框架/解析不对称**（`app/client-api/src/ai.rs`）：
  块切分层（`line_terminator_len`）按 SSE 规范支持孤立 `\r`，但
  `parse_sse_block` 用 `str::lines()`（仅识别 `\n`/`\r\n`）切块内行；若
  中间层规范化行尾为 `\r`，事件名与 data 粘连成单行被整块丢弃，终止
  事件静默消失会绕过 CR-9 建立的错误上报保证。修复：块内行切分改
  `split(['\n','\r'])`（`\r\n` 产生的空片段无前缀匹配自然忽略），与块
  切分层同口径。回归测试 `parse_sse_block_handles_lone_cr_line_terminators`
  （含 `\r` 框架的多行 data 合并与损坏终止事件 Err 路径）。

另有非行为性改进：`response_stream` 建连重试退避上限改引用共享常量
`MAX_BACKOFF_SHIFT`（消除硬编码 `2` 的双源漂移风险）；非 SSE 2xx 回退的
响应体经 `truncate_error_body`(2 KiB) 截断后才入错误串（防任意大 body 经
Display 外泄，失败信封解析不受影响）；`estimate_tokens`
字节/字符计数偏差、`retry_delay` 时钟失败抖动归零退化、防御路径合成
200 状态码与 SVG vision 上游兼容性限制均补文档注释。
另补评审建议回归测试：建连阶段 503→重试→成功
（`test_response_stream_retries_connection_phase_then_succeeds`）、重试耗尽
保留最后错误（`test_response_stream_retry_exhaustion_returns_last_error`）、
4 MiB 缓冲溢出护栏（`test_response_stream_buffer_overflow_yields_single_error`）、
无终止事件干净结束契约（`test_response_stream_connection_close_without_terminal_ends_cleanly`）、
多字节 CJK delta 跨重试去重（`test_midstream_retry_dedups_multibyte_unicode_without_corruption`）、
自动压缩不重置 turn 预算（`kernel_auto_compact_does_not_reset_turn_budget`）、
force 绕阈值但停首个成功层级（`force_bypasses_threshold_but_stops_at_first_successful_level`）、
单 push 多协议块过滤（`tool_tag_filter_multiple_blocks_in_single_push`）、
配对保护切分退化用例（`split_preserving_tool_pairs_pair_shift_to_zero_returns_empty_older`
+ `compact_conversation_empty_older_after_pair_shift_is_passthrough`）。

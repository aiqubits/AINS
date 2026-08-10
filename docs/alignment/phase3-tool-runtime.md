# Phase 3 对齐清单：Tool Runtime (Local + Remote)

对齐基线：`OpenHarness/src/openharness/tools/`（`base.py` 与内置工具）、
`permissions/`、`sandbox/`、`hooks/`、`mcp/`、`utils/network_guard.py`、
`services/tool_outputs.py` + `engine/query.py::_execute_tool_call`
（提交时仓库内版本）。
范围：Phase 3.1–3.10（Tool trait + ToolRuntime + 输出预算、Pure Rust
Tools、交互/元工具、Native Tools、Network Tool、MCP Client、三态权限、
Sandbox 占位、Hook System、集成测试）。

## 1. Tool trait + ToolRuntime + 输出预算（3.1，`tools/mod.rs` / `runtime.rs` / `outputs.rs`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| Tool 协议 | `BaseTool`：name/description/`input_model`(pydantic)/`execute → ToolResult`/`is_read_only(arguments)` | `Tool` trait：`definition() → ToolDef{name, description, input_schema}`（JSON Schema 直书，无 pydantic 对应物）/`execute(Value, &mut ToolContext) → ToolResult`/`is_read_only(&Value)` 按参数自报（默认 false，同基线）；ToolResult metadata 传导到 conversation `result_metadata` 与 `ToolExecutionCompleted.metadata` | 对齐（schema 由手写 JSON 替代 pydantic 反射） |
| 注册表 | `ToolRegistry`：dict 名字映射、`to_api_schema()` | `ToolRuntime`：注册序保持（Vec+index）、同名重注册**原位替换**（对齐 dict 覆盖语义）、`api_schemas()` | 对齐 |
| 分发管线 | `query.py::_execute_tool_call`：pre_tool_use hook → 权限 evaluate（file_path/command 归一化）→ 确认回调 → 执行 → 输出预算外置 → post_tool_use hook；全部失败归一化为 is_error tool_result | `ToolRuntime::dispatch` 同序同语义；`file_path` 取 `file_path`/`path`/`root` 首个非空并以 cwd 锚定词法 resolve；post_tool_use 观察性执行不改写结果；`AgentKernel::new` 自动装配无确认回调的 default 权限引擎，写操作 fail-closed，完整宿主经 `with_runtime` 注入 UI 回调 | 对齐 + 安全默认 |
| tool_use 批次完整性 | `tool_use_id` 是 tool_use/tool_result 唯一配对键，基线未提供整批 ID 前置守卫 | Kernel 在记录 assistant turn、发送 completion 事件或分发任一工具前，整批拒绝空/纯空白或重复 ID；以 recoverable error 丢弃畸形 turn，不执行任何副作用，也不留悬空 tool_use 历史 | 协议 + 安全加固 |
| 输出预算 | inline 16000 / preview 3000 / microcompact 4000 字符（Unicode 码点），`OPENHARNESS_*` 环境变量覆盖（下限 256/128/256）；超限落盘 `tool_artifacts/` + `[Tool output truncated]` 标记 + 预览 | 同常量同下限，环境变量 `AINS_TOOL_OUTPUT_*`（仅 Native；WASM 取默认）；外置为 `ArtifactSink` 端口（Native `FsArtifactSink` 文件、文件名 `{unix_ms}-{safe}-{seq}.txt`），标记文案同结构；工件引用记入 tool_metadata 独立活跃工件列表（对齐 `_remember_active_artifact`）；包括 hook/权限提前拒绝在内的所有结果统一应用预算；截断诊断头、工具 ID、工件引用/失败原因与 preview 合成后再执行 inline hard cap，即使 preview 配置大于 inline，最终回填也不越界 | 对齐 + 资源加固（工件文件名时间戳格式为**偏差（有意）**：epoch ms 替代 `%Y%m%d-%H%M%S`，避免引入 chrono） |
| 无外置存储 | 不存在该分支（桌面恒有文件系统） | Web 未注入 sink 时：全文丢弃、标记注明 "no artifact storage available"，仅留预览 | **偏差（平台限制）**：Web sink 可后续接 KvStore |
| microcompact 判定 | `mcp__` 前缀恒可清理，其余按阈值 | `is_microcompactable_tool_result` 同语义（消费方随 Phase 5 compact 落地） | 对齐 |

## 2. Pure Rust Tools（3.2，`tools/compute.rs`，基线无对应物）

Calculator（Shunting-yard，`+ - * / % ^` 括号一元负号，除零/非法表达式报
error 结果）、JSON（validate/format/minify/get(RFC6901)/keys）、Text
（upper/lower/trim/replace/count）、Markdown（to_html/headings/to_text，
pulldown-cmark）、Date（now/from_epoch_ms/to_epoch_ms/diff_days，复用
memdir civil 历法，无 chrono；from_epoch_ms 在工具边界限制为四位年份
ISO-8601 可表示域，from/to_epoch_ms 保留非零毫秒并可无损往返）。
所有字符串入参在工具边界限制为 1 MiB，可能放大的转换结果限制为
8 MiB；Text replace 用 checked 字节算术预计精确结果大小后才分配，JSON
format/minify/get 通过有界 writer 序列化，避免在共享输出预算介入前构造
无界中间 String。
全部只读、双 target。**AINS 扩展**
（计划 3.2；基线无此五件套）。

## 3. 交互/元工具（3.3，`tools/interact.rs`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| todo_write | markdown 清单文件：未勾选→就地打勾、目标态命中 no-op、新条目追加 | `apply_todo` 纯函数同三分支；Native 经 filesystem 同一路径解析（`~` 展开 + 词法归一，与权限求值同口径，review 十二轮）落 cwd 锚定文件；Web 存 `tool_metadata.extra["todo_markdown:{normalized_path}"]`；路径别名共用独占资源键，两端均限制文档与 item 为 8 MiB，并拒绝空白/多行 item、归一外层空白 | 对齐 + 加固（Web 存储介质为**偏差（平台限制）**） |
| ask_user_question | 回调经 `context.metadata["ask_user_prompt"]` 传入；无回调报 "unavailable in this session"；答复 strip、空回 "(no response)"；只读 | `UserInteraction` trait 构造注入（Rust 无鸭子类型，语义等价）；文案/strip/空答复同基线 | 对齐（注入方式差异） |
| enter/exit_plan_mode | 读写 settings 文件切换 PermissionMode | 切换共享 `PermissionEngine` 模式（进程内即时生效）；模式持久化随 Phase 5 会话快照 | 对齐（持久化时点偏差，Phase 5 回收） |
| enter_plan_mode 只读性 | 非只读（default 模式下需确认，UI 侧放行） | 标记只读：收紧权限是安全方向，免确认 | **偏差（有意）** |

## 4. Native Tools（3.4，`tools/filesystem.rs` / `system.rs`，仅 Native 编译）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| read_file | offset/limit(200/2000)、`{:>6}\t` 行号、NUL 判二进制、UTF-8 lossy、目录/缺失报错 | 同；额外 `record_read_file` 入状态袋（基线在 engine 层做 carryover，AINS 就近记录） | 对齐 |
| write_file | create_directories 默认 true；`edit_approval_prompt` 二次确认 + ANSI diff 统计 | 同写入语义；**无工具内二次确认**——AINS 统一走 ToolRuntime 三态权限确认回调，diff 预览属 Phase 6 权限交互 UI；单次写入上限 8 MiB | **偏差（有意）**：确认职责上收管线 + 资源加固 |
| edit_file | old_str 未命中报错、replace 首个 / replace_all | 同（含确认职责上收，同上）；拒绝空 old_str，并在分配前校验 8 MiB 结果上限 | 对齐 + 资源加固 |
| glob | rg `--files --glob` 子进程 + Python 回退；git 仓库含隐藏路径（上溯 6 级找 `.git`）；排序 + limit(200/5000)；绝对路径模式拆根 | `ignore` crate 进程内遍历（ripgrep 家族实现，尊重 .gitignore）；同启发式/排序/limit/拆根算法；逐文件过滤内置敏感路径；最多访问 100,000 个条目 | 对齐 + 安全/资源加固（实现方式差异：进程内替代子进程） |
| grep | rg 子进程 + Python 回退：`path:line:content`、大小写开关、file_glob、limit(200/2000)、单文件 root、非法正则文案 `(invalid regex pattern …)`、root 缺失文案 | 纯 Rust（regex + ignore walker）对齐 Python 回退路径语义；逐文件过滤内置敏感路径，避免安全根下的凭据后代被读取；最多访问 100,000 个条目 | 对齐 + 安全/资源加固（以条目硬预算替代子进程 timeout） |
| Shell | `bash_tool`：pty 优先子进程、12000 字符截断、超时 partial output、交互式脚手架预检 | `ShellCommandTool` 执行**必经 Sandbox 层**：NoopSandbox 下返回"无操作权限"，不触碰 `std::process::Command`（计划 3.4 硬约束）；输出整形/超时文案对齐（`format_shell_output` + timed_out metadata）；交互式预检随平台沙箱 Phase 7.1 一并落地 | 对齐（执行后端为占位，Phase 7.1 回收） |
| Clipboard/Notification/Screenshot | 基线无对应物 | `SystemIntegration` 宿主端口（Phase 6 Dioxus 注入）；未注入报 "no system integration" | AINS 扩展（计划 3.4） |

## 5. Network Tool（3.5，`tools/network.rs`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| URL 语法校验 | 仅 http/https、必须有 host、禁内嵌凭据 | `validate_http_url` 同三条 | 对齐 |
| 公网校验 | 字面量 IP `is_global`；本地主机名黑名单 + 后缀 + 单标签拒绝；DNS 解析逐地址复核（阻断列表 ≤3 渲染） | Native 同；`is_global_ip` 手写 IANA 特殊段判定（含 CGN/基准测试/文档段/v4-mapped 递归）；已验证地址通过 reqwest `resolve_to_addrs` 固定，消除校验/连接二次解析窗口 | 对齐 + 加固 |
| 每跳重定向复检 | `follow_redirects=False` + 手动 urljoin 循环 + 每跳 `ensure_http_url_allowed`，>5 跳报错 | Native 同（reqwest Policy::none + `Url::join`）；WASM 因浏览器不暴露可靠 DNS/逐跳目标校验而 fail-closed，要求可信同源代理 | **偏差（安全收紧）**：CORS 不能阻止请求发往内网，不能作为 SSRF 边界 |
| 解析模式 | auto/direct/proxy/synthetic_dns 四模式 + 配置 | 仅 DIRECT 语义 | **偏差（暂缺）**：代理配置面随桌面设置需求评估 |
| web_fetch 输出 | UA/banner/HTML→文本（跳 script/style + 实体子集 + 空白折叠）/max_chars 截断 + `...[truncated]`/`URL: Status: Content-Type:` 头 | Native 同结构；UA 品牌换 AINS；status≥400 报错；响应流在 2 MiB 硬上限停止，避免截断前无界缓冲 | 对齐 + 加固 |

## 6. MCP Client（3.6，`tools/mcp.rs`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 配置 | stdio/http/ws 三类（pydantic 判别式）；ws 报 "Unsupported MCP transport in current build" | serde `type` 判别式同三类；ws 同文案报不支持；WASM 上 stdio 亦报不支持 | 对齐 |
| 连接 | mcp SDK：initialize → tools/list（list_resources 容错）；单 server 失败仅记 failed 状态不阻断启动 | 手写 JSON-RPC：initialize（校验协商版本）→ notifications/initialized → tools/list（完整跟随 opaque `nextCursor`，重复 cursor 与 100 页上限 fail-closed；每 server 跨页累计最多 1,024 个工具，序列化工具元数据与保留 cursor 合计最多 8 MiB，并在并入累计状态前校验）；同不阻断语义；resources 面（list/read_mcp_resource 工具）后置 | 对齐 + 资源加固（resources 为**偏差（暂缺）**，随资源工具需求补） |
| stdio 传输 | SDK stdio_client（newline-delimited JSON-RPC） | tokio 子进程 + 行分帧；30s 请求超时 + 256 行洪泛护栏 + 单帧 4 MiB 硬上限 + kill_on_drop | 对齐 + 加固 |
| streamable-http | SDK streamable_http_client | POST JSON-RPC，`Accept: application/json, text/event-stream`；SSE 按事件组帧（多 `data:` 行合并，LF/CRLF/CR，跨 chunk）增量消费并在 id 匹配时立即返回（不等待长连接 EOF）；JSON 响应同样校验 id；`Mcp-Session-Id` 透传（双 target）；30s 平台超时 + 4 MiB 响应上限 | 对齐 + 加固 |
| 工具桥接 | `McpToolAdapter`：`mcp__{server}__{tool}`（段安全化：非字母数字→`_`、空回落 `tool`、非字母开头前缀 `mcp_`）、桥接为普通 Tool | 同名同安全化算法；`register_mcp_tools` 注入同一 ToolRuntime，Kernel 无感知；安全化后的同名冲突原子拒绝，不静默覆盖 | 对齐 + 加固 |
| 结果字符串化 | text 项取文本、其余 JSON dump、空回落 structuredContent、再空 "(no output)" | `stringify_call_result` 同四级回落；空、纯空白或缺失 `text` 字段的 text block 不再被视为有效输出，会继续回落 structuredContent / "(no output)"；MCP `isError` 映射到 `ToolResult.is_error` | 对齐 + 协议修正（基线丢失 `isError`） |

## 7. 三态权限引擎（3.7，`policy/permission_engine.rs`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 决策序 | 敏感路径 → denied_tools → allowed_tools → PathRule → denied_commands → full_auto → 只读 → plan → default 确认 | 敏感路径 → denied_tools → denied_commands → PathRule → allowed_tools/会话级"总是允许" → 模式门控 | **有意修正基线缺陷**：命令/路径 deny 均不可被工具白名单、AlwaysAllow 或路径 allow 覆盖 |
| 敏感路径黑名单 | .ssh/.aws×2/gcloud/.azure/.gnupg/.docker/.kube + openharness 自有凭据×2；fnmatch；`_policy_match_paths` 目录根加尾 `/`；不可被任何模式/规则覆盖 | 同列表；openharness 两项替换为 `*/.ains/credentials.json`；目标/最近存在父目录先解析 symlink；Windows `\\` 统一为 `/` 后匹配；同双形态匹配 | 对齐 + 加固（自有凭据路径改名为**偏差（有意）**） |
| PathRule | allow 分支为死代码（仅 deny 生效） | **完整 allow/deny 语义**：按序首个命中生效，allow 命中跳过模式门控（计划明确要求） | AINS 扩展（计划注记） |
| PermissionMode | default/plan/full_auto | 同三枚举；`RwLock` 共享句柄支持 enter/exit_plan_mode 即时切换；plan 下 `exit_plan_mode` 进入确认分支而非被写操作规则永久拦截；会话级 AlwaysAllow 放行集在 plan 模式下挂起、退出后恢复（配置级 allowed_tools 仍按基线序先于模式门控，review 十二轮） | 对齐 + 修正 |
| 确认回调 | `permission_prompt(tool_name, reason) → bool`；询问前发 notification hook | `PermissionPrompt::confirm → Allow/AlwaysAllow/Deny`（6.11 三答复）；请求携带结构化输入、规范化路径和命令供 UI 知情确认；同 notification hook 时点；AlwaysAllow 写会话放行集 | 对齐 + AINS 扩展 |
| bash 提示 | 安装/脚手架命令追加确认提示（17 个 marker） | `bash_permission_hint` 同列表同文案 | 对齐 |
| 模式持久化 | settings 文件 | 随 Phase 5 会话快照 | **偏差（暂缺）** |

## 8. Sandbox（3.8，`policy/sandbox.rs`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 能力探测 | Docker sandbox `is_docker_sandbox_active` 等 | `Sandbox` trait：`capabilities()`（shell/网络策略/文件系统策略三能力位）+ `is_available()`；`NoopSandbox` 全 false | 对齐（能力模型按计划 3.8 设计） |
| 高风险拒绝 | `SandboxUnavailableError` | `SandboxError::Unavailable`；Shell 工具收到后归一化为 is_error 结果（"无操作权限"文案，计划原文）；`ShellRequest` 携带 timeout + max_output_bytes，真实后端必须在管道读取阶段强制 | 对齐 + 资源加固 |
| validate_sandbox_path | `Path.resolve()`（词法，strict=False）→ cwd relative_to → extra_allowed | 同判定；**词法规范化**（`.`/`..` 消解、`..` 越根钳位）替代 canonicalize（Rust canonicalize 要求路径存在）；不追 symlink——OS 层拦截属 Phase 7.1 | 对齐（resolve 语义差异注记） |
| 平台运行时 | Docker backend | Phase 3 的执行接口仅覆盖 Shell，NoopSandbox 默认拒绝；文件工具由路径权限约束，网络工具由 SSRF/DNS 绑定约束。Linux namespace/seccomp、macOS sandbox-exec、Windows Job Object 及文件/网络 OS 级策略接口随 Phase 7.1 落地 | 计划安排 |

## 9. Hook System（3.9，`hooks/mod.rs`）

| 能力点 | 基线 | AINS | 结论 |
|---|---|---|---|
| 触发点 | 10 个（session_start/end、pre/post_compact、pre/post_tool_use、user_prompt_submit、notification、stop、subagent_stop） | `HookEvent` 同 10 枚举同蛇形命名 | 对齐 |
| 定义类型 | command/prompt/http/agent 四类 | command/prompt 先行（计划 3.9 明确 http/agent 后置）；serde `type` 判别式；默认值同基线（timeout 30、prompt 默认 block_on_failure=true） | 对齐（http/agent 后置） |
| matcher/priority | fnmatch 对 payload `tool_name→prompt→event`；priority 降序、同级注册序 | 同；`sort_by_key(Reverse)` 稳定排序 | 对齐 |
| command hook | shell 执行、`$ARGUMENTS` shlex 转义注入、`OPENHARNESS_HOOK_EVENT/PAYLOAD` 环境变量、超时 kill、stdout+stderr 拼接、退出码 metadata | 同（环境变量前缀 AINS_；`shell_quote` 等价 shlex.quote），Native 强制经注入的 `Sandbox`，默认 Noop fail-closed；WASM 报 "not supported on the web platform"（blocked 依 block_on_failure） | 对齐 + 加固（env 前缀改名；WASM 降级注记） |
| prompt hook | 模型校验，严格 JSON `{ok, reason}` + 宽松回退（ok/true/yes）；固定 system 前缀 | 同（前缀品牌换 AINS；ModelClient 未注入报失败）；hook 级模型覆盖 default_model；请求/响应各 256 KiB 硬上限 | 对齐 + 资源加固 |
| 聚合 | `AggregatedHookResult`：任一 blocked 即阻断；reason 回落 output | 同 | 对齐 |
| Kernel 触发 | 生命周期与工具循环触发对应事件 | 已接线 session_start/end、pre/post_compact、pre/post_tool_use、user_prompt_submit、notification、stop；subagent_stop 随 subagent 生命周期落地 | 对齐（subagent 尚无运行时触发源） |
| loader/hot_reload | 配置文件加载 + 热重载 | `HookRegistry` 程序化注册 + `update_registry`（热重载由宿主驱动）；文件加载面随插件/配置系统（Phase 7+）落地 | **偏差（暂缺）** |

## 10. 测试与验收（3.10）

- Native：`cargo test -p rust-agent --all-targets` 303 项全过 —— lib 184 +
  core_traits 19 + kernel_loop 24 + memory_native 61 +
  `tests/tool_runtime.rs` 集成 15（plan 拦截端到端、
  确认回调 Allow/Deny/AlwaysAllow、敏感路径 full_auto 兜底、pre_tool_use
  hook 阻断端到端 + matcher 不命中直通、输出预算外置落盘、Shell 经
  NoopSandbox 拒绝、MCP stdio 真进程握手 + 桥接调用、同文件别名顺序化、
  混合批次失败隔离、19 内置工具注册冒烟）。
- Web：`tests/web_tools.rs` 9 项契约测试（计算工具、plan/敏感路径、
  todo_write 状态袋、无 sink 预算降级、command hook Web 降级 + 管线阻断、
  network 语法守卫 + Web 直连 fail-closed、prompt hook 平台超时），随 CI
  `wasm-pack test --headless --chrome` 执行。
- 双 target `cargo clippy --all-targets -- -D warnings` / wasm `--tests` 通过。
- Review 一轮修复（均含回归覆盖）：`is_global_ip` 补 `0.0.0.0/8`（BSD 上
  路由回环，SSRF 绕过）与 IPv6 `2001::/23` IANA 保留段（对齐基线
  `ipaddress.is_global` 口径，并补齐 `100::/64` discard-only 特殊用途段）；MCP 工具调用改为短暂持 manager 锁仅取
  会话句柄、RPC 期间只持单 server transport 锁（旧实现把全部 MCP 调用
  串行化）；`HookExecutor::update_registry` 改 `&self` + RwLock 内部可变
  （Arc 封装下原 `&mut` 签名不可调用，快照后执行不跨 await 持锁）；
  command hook 成功时 `reason` 置空（失败语义字段不在成功时携带输出）。
- Review 二轮修复（均含回归覆盖）：权限路径解析目标/最近存在父目录的
  symlink，并统一 Windows 分隔符；plan 模式允许 `exit_plan_mode` 进入用户
  确认；Native web_fetch 将已验证 DNS 地址固定到实际连接且响应流限制
  2 MiB；Web web_fetch 因浏览器无法提供可靠 SSRF 边界改为 fail-closed；
  MCP HTTP 双平台 30s 超时、4 MiB 响应上限并传导远端 `isError`。
- Review 三/四轮修复（均含回归覆盖）：命令 hook 强制经 Sandbox 且默认
  fail-closed；web_fetch 禁用系统代理以保持 DNS 固定边界；Kernel 生命周期
  hook 接线；MCP stdio 单帧上限、SSE 增量响应与 JSON-RPC id 校验；文件编辑、
  TODO、glob 的输入/收集上限；shell cwd 锚定与权限求值；提前拒绝结果同样
  进入输出预算。
- Review 五轮修复（均含回归覆盖）：`AgentKernel::new` 裸工具入口自动装配
  default 权限引擎并在无 UI 回调时对写操作 fail-closed；SSRF IPv6 分类补齐
  RFC 6666 `100::/64` discard-only 前缀；Date `from_epoch_ms` 拒绝四位年份
  ISO-8601 表示域之外的输入。
- Review 六轮修复（均含回归覆盖）：file_write/edit/todo 结果字节上限、
  edit 空匹配拒绝与分配前放大校验；ToolResult metadata 传导至消息与
  UI 事件；SSE 多行/跨 chunk 事件组帧；prompt hook 请求/响应硬上限。
- Review 七轮修复（均含回归覆盖）：PathRule deny 提升到配置/
  会话工具白名单之前；`ToolRuntime` 所有公开构造路径默认装配
  fail-closed 权限引擎；多 tool_use 并发执行、失败隔离且按原始顺序回填，
  跨轮 tool_metadata 按工具顺序合并增量；生命周期 hook payload 补齐
  `cwd` 与 Stop `stop_reason`。
- Review 八/九轮修复（均含回归覆盖）：glob/grep 对遍历后代逐文件执行
  敏感路径过滤、目录遍历硬预算、相同独占资源键顺序执行；denied_commands
  不可被 allow 覆盖、Date 毫秒无损往返、Web todo_write 路径别名归一与
  Sandbox 输出字节上限契约。
- Review 十轮修复（均含回归覆盖）：SSRF 地址分类同步 IANA 新增/遗漏的
  非全球可达 IPv4/IPv6 特殊用途前缀；MCP `tools/list` 完整分页、异常 cursor
  与总页数护栏、initialize 协议版本校验；todo_write 拒绝空白/多行 item 并
  归一外层空白以保持幂等匹配。
- Review 十一轮修复（均含回归覆盖）：Kernel 在任何工具副作用前整批拒绝
  空白/重复 tool_use ID；MCP `tools/list` 补每 server 累计工具数与工具/cursor 状态
  字节上限，并让空/纯空白/缺失 text block 正确回落 structuredContent 或
  `(no output)`；compute 工具补字符串输入与结果字节上限、Text replace 分配前
  放大预算及 JSON 有界序列化；超限工具输出的诊断头与 preview 合成后再
  强制 inline hard cap。
- Review 十二轮修复（均含回归覆盖）：会话级 AlwaysAllow 放行集在 plan
  模式下挂起、退出后恢复（配置级 allowed_tools 保持基线序）；Native
  todo_write 复用 `filesystem::resolve_path`（`~` 展开 + 词法归一）使执行
  路径与权限求值同口径；grep 隐藏文件启发式改用与 glob 同口径的
  `looks_like_git_repo`（上溯 6 级找 `.git`）；MCP HTTP 仅从成功响应采纳
  `mcp-session-id`（失败响应不得覆盖既有会话）；MCP stdio 写入侧补
  30s 超时（子进程停读 stdin 时管道写满不再无限挂起）。
- Review 十三轮修复（均含回归覆盖）：glob/grep 隐藏文件包含启发式统一
  为共享谓词 `search_includes_hidden`（`looks_like_git_repo` ∨ 根下存在
  `.gitignore`，取两基线信号并集），非 git 目录含 `.gitignore` 时两工具
  不再分岔；非法 glob 模式不再静默降级（grep 旧行为忽略过滤器全量遍历、
  glob 旧行为空结果），改为显式 `(invalid glob pattern/file glob ...)` 提示；
  `exclusive_execution_key` 签名增补 `cwd`，Native 端 write_file/edit_file/
  todo_write 共享 `file:{resolve_path 规范路径}` 独占键命名空间，同批次内
  相对/绝对路径别名或跨工具命中同一文件时回退顺序执行，封堵并发
  read-modify-write 丢更新（Web todo_write 保持状态袋键词法归一口径）。
- Review 十四轮修复（均含回归覆盖，外部 review 合入前复审）：
  `is_global_ip` 封堵 `::/96` v4-compatible 形式（`::127.0.0.1` 等，
  `to_ipv4_mapped` 不覆盖且逐段黑名单无命中；RFC 4291 已废弃，
  整段按非公网 fail-closed）；Markdown `to_html` 改经模块级
  `LimitedBuffer` 有界写入器渲染，嵌套 blockquote 放大在 8 MiB 处
  就地拒绝而非先分配后检查；`~` 展开统一经 `runtime::home_dir()`
  （`HOME` 优先、`USERPROFILE` 回退，纯函数 `select_home` 可测），
  权限求值与 `filesystem::resolve_path` 同口径；计算器一元负号
  优先级约定（`-2 ^ 2 = 4`）写入工具 description 避免模型/工具
  静默分歧；MCP `HttpTransport` 标注可信配置信任边界（动态添加
  server 前必须接入公网校验）；新增 `dispatch_many` N=8 混合读写
  批次集成测试（失败隔离 + 全部成功编辑不丢 + metadata 按序合并）。
  已评估不修：权限校验与打开间 TOCTOU（同用户本地威胁模型外，
  随 Phase 7.1 平台沙箱 O_NOFOLLOW 回收）；重定向逐跳端到端 mock
  测试（需测试专用 resolver 注入，单元级已覆盖）；Windows 实机
  路径测试（CI 无 Windows runner，归一化逻辑已有跨平台单测）。

## 不复刻清单

1. pydantic 输入模型反射与 `create_model` 动态建模：Rust 侧 JSON Schema
   直书，输入校验由各工具 `execute` 显式做。
2. Docker sandbox 会话（`sandbox/docker_backend.py`）：AINS 面向嵌入式
   客户端，平台沙箱走 OS 原生机制（Phase 7.1）。
3. bash pty 优先派生与交互式脚手架预检：随 Phase 7.1 真实 shell 执行
   后端一并落地。
4. http/agent 两类 hook、hook 配置文件 loader/hot_reload：计划 3.9 明确
   后置。
5. MCP resources 面（list/read_mcp_resource 工具）与 mcp_auth：随资源
   需求评估。
6. 基线 42 内置工具全集：按计划裁剪为首批（compute 5 + fs 5 + system 4 +
   web_fetch + interact 4 + MCP 桥接）；skill 工具属 Phase 6.8。

## 遗留偏差汇总（后续 Phase 回收）

| 偏差 | 回收点 |
|---|---|
| NoopSandbox 占位（Shell 拒绝执行） | Phase 7.1 平台沙箱（namespace/seccomp 等） |
| write/edit 无 diff 预览确认 UI | Phase 6.11 权限交互 UI（数据已在 PermissionRequest） |
| 权限模式/会话放行不持久化 | Phase 5 会话快照 |
| Web 无 ArtifactSink（超长输出仅留预览） | KvStore sink（随 Web 宿主装配） |
| 网络 proxy/synthetic_dns 解析模式 | 桌面设置面需求落地时 |
| http/agent hook + hook 文件加载 | Phase 7+（插件系统） |
| MCP resources / auth 工具面 | 资源需求评估后 |
| `AcceptEdits` 模式 / `DomainRule` 域规则 | Phase 6.11 权限交互 UI + Phase 7.1 网络 Sandbox 策略；Phase 3.7 收敛范围仅 default/plan/full_auto + PathRule |

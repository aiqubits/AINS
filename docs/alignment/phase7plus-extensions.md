# Phase 7+ 对齐清单：扩展特性（Slash Commands / 插件 / 子代理 / 后台任务 / 个性化）

对齐基线 OpenHarness `commands/` `plugins/` `swarm/`+`coordinator/` `tasks/`
`personalization/`。本轮交付 Phase 7+ 全部 P2 项（7+.1–7+.5）。

> **范围说明**：本页的“交付/验收”仅表示 `rust-agent` 的组件契约已实现并通过测试；
> 它们尚未全部接入产品运行时的端到端链路，应用层编排属于后续集成工作。

> **验收状态**：rust-agent 双 target（native + wasm32）`clippy -D warnings`
> 全绿（含 `--tests`）；native 全量测试通过（lib 353：含 commands 10 / plugins 6 /
> tasks 7 / personalization 6 / swarm 4；memory_native 66：含 mailbox 集成 1）。

---

## 7+.1 Slash Commands（`commands/`）

frontmatter markdown 命令模板，与 Skill **同构互转**。命令是提示词模板：
`/name args` 展开为提交模型的 prompt。

| 能力 | 实现 | 对齐 |
|---|---|---|
| 解析 | `SlashCommand::from_markdown`（复用 `skills::split_frontmatter`）：description / argument-hint / allowed-tools / model + body | `commands/registry.py::SlashCommand` |
| 参数展开 | `$ARGUMENTS`/`${ARGUMENTS}`（原文）+ 位置 `$1`..`$9`/`${N}` + 无占位则追加 `Arguments:` | `_render_*_command_prompt` |
| Skill 互转 | `from_skill_content`（SkillContent→命令）+ `to_markdown`（命令→SKILL.md）；roundtrip 单测 | `_make_skill_slash_command` |
| 注册表 | `CommandRegistry`：register / lookup(`/name args`) / list / help / invoke→`CommandOutcome{prompt,model,allowed_tools}` | `CommandRegistry` |

## 7+.2 插件系统（`plugins/`）

聚合贡献包，**skills / commands / tools / hooks / MCP 五注册面统一注入**。

- `Plugin`（`from_json` 清单）聚合五面：commands 复用 `SlashCommand`，hooks 复用
  `hooks::{HookEvent,HookDefinition}`，skills/tools/mcp 为声明。
- `PluginRegistry::inject(&mut CommandRegistry, &mut HookRegistry) -> InjectionSummary`：
  命令/hooks 直接注入既有注册表，skills/tools/mcp 汇总交上层接入子系统。
- 禁用插件完全惰性（不贡献任何面）。
- **有意偏差**：编译型双 target（wasm 无动态原生加载）下 `tools` 面为**声明式**
  （引用内置 / MCP 工具并白名单），非携带可执行代码——对齐基线 `LoadedPlugin`
  的贡献聚合语义，但工具执行体由编译期注册。

## 7+.3 子代理 Swarm（`swarm/` + `coordinator/`）

进程内 `TeammateExecutor` + KV 信箱 IPC + 权限上收 lead + `AgentDefinition`。

| 组件 | 实现 | 对齐 |
|---|---|---|
| AgentDefinition | name/description/system_prompt/tools/disallowed_tools/model/initial_prompt + `allows_tool`（disallowed 优先，`*`=全部） | `coordinator/agent_definitions.py` |
| 注册表 | `AgentRegistry` register/get/list | — |
| KV 信箱 | `KvMailbox`（双 target，per-swarm/session + per-recipient inbox；key `swarm/mbox/{scope}/{r}/{ts:013}-{nonce}` 时间有序）：post/inbox/unread/mark_read | `swarm/mailbox.py`（文件→KV） |
| 权限上收 | `needs_escalation(def, tool)` = 越权即上收 lead（子代理不得自行提权）+ `PermissionEscalation` | `swarm/permission_sync.py` |
| 进程内后端 | `TeammateExecutor`：`InProcessExecutor` 按定义派发给 `TeammateRunner`（上层接 Agent Loop / 测试桩），归一化 `TeammateResult` | `swarm/in_process.py` |

**偏差**：首选进程内后端；subprocess/tmux pane 后端与 worktree 隔离为后续增强。

## 7+.4 后台任务（`tasks/`，Native 先行）

`BackgroundTaskManager` + `background_task` 工具。依赖子进程/tokio，**仅非 wasm 编译**
（Web 无子进程模型）。

- 状态机：Running →（完成）Completed/Failed；`stop` 抢占→Killed；完成回写守卫
  （已 Killed 不被覆盖，对齐基线）。
- `spawn_shell`（**沙箱内执行**：经 `Sandbox::exec_shell`（`default_sandbox` 或宿主注入），
  `capabilities().shell == false` 时拒绝启动（fail-closed，不降级宿主直跑）；长兑底
  超时 `TASK_RUNTIME_TIMEOUT`；输出 `TASK_OUTPUT_MAX_BYTES` 由后端读管道时强制）/
  list / get / wait / update(progress≤100,note) / stop / output。
- `stop`：**协作式取消**（置 `ShellRequest.cancel` → 后端杀整个进程树：Unix killpg /
  Windows kill-on-close）后置 Killed；刻意不用 `abort`（只杀包装进程，沙箱内命令树残留）。
- `BackgroundTaskTool`（`ToolCategory::System`）多路复用 `run/list/show/stop/update/output`
  子动作（对齐 `/tasks` 子命令面）；`is_read_only` 对 list/show/output 为真。
- 对齐 `tasks/`：`TaskStatus`(running/completed/failed/killed) + `TaskRecord`。
- Review 修复（2026-08-01）：`spawn_shell` 原直接 `sh -c` 绕过沙箱（Blocking），现必经
  `Sandbox` 执行；`stop` 原 `abort()`（孙进程残留），现协作式取消。

## 7+.5 个性化（`personalization/`）

会话后偏好提取 → 规则注入回路（双 target，正则启发式，无 LLM）。

- `extract_facts_from_text` / `extract_preferences`（仅采用**用户**消息）：偏好类
  （称呼 / 回复语言 / prefer|like|want / always|never 规则）+ 环境类（ssh_host /
  ip_address（跳过回环/广播）/ env_var / api_endpoint）。
- `merge_facts` 去重（kind+归一化值）；`facts_to_rules_markdown` 分组渲染；
  `rules_prompt_section` 生成 System Prompt 注入段（空规则不注入）。
- `PreferenceStore`（KvStore）：`update_from_session`（提取→合并→存）闭合回路，
  `prompt_section` 供注入端。对齐 `extractor`+`rules`+`session_hook`。
- 含显式 secret 赋值、Bearer 凭据或 URL/SSH 内嵌凭据的值会在提取、持久化和
  渲染三处拒绝；渲染标签固定为受控类别，避免外部同步数据进入跨会话存储或
  System Prompt。

## Review 修复（2026-08-01）

- 插件清单默认禁用（`enabled_by_default` 仅作元数据，不能自授权），必须由宿主显式信任后才注入 command/hook/MCP 声明；
  Swarm 子代理缺省为空工具白名单。
- 命令 frontmatter 的 `allowed-tools` 与 Skill 的工具门控统一；偏好 singleton
  更新会持久化最新值，事实集合和渲染提示均有上限。
- 后台任务的并发 `wait`/`stop` 共享完成通知；Native 沙箱清理宿主环境变量，
  `background_task/run` 复用 shell 权限边界。

---

## 依赖新增

| crate | 位置 | 用途 |
|---|---|---|
| `regex` 1 | rust-agent（由 Native-only 移至共享） | filesystem grep（Native）+ personalization 偏好抽取（双 target，wasm 兼容） |

`getrandom`（7.5 引入）复用于 swarm mailbox 消息 nonce。其余全部复用既有依赖。

## 模块清单

`commands/`（双）、`plugins/`（双）、`personalization/`（双）、`swarm/`（双）、
`tasks/`（Native）。`error.rs` 新增 `CommandError` 并并入 `AgentError`。

## 遗留 / 后续

- 插件工具面声明式（无动态原生加载）；子代理 subprocess/pane 后端与 worktree 隔离
  为后续；task 目前仅 shell 类型（agent 类型任务待接 Kernel）。
- 五套扩展均为**库级能力**，接入 UI / Kernel 装配（命令面板、插件加载目录扫描、
  子代理编排、后台任务面板、规则注入点）属后续应用层接线。
